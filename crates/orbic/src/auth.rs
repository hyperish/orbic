//! `orbic::auth` — Unified Web Cookie (+ CSRF) and Native Bearer Opaque Session auth
//! backed directly by L2 cache storage for instant revocation.

use crate::cache::Cache;
use hyve::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// User domain marker for `Id<User>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User;

/// Authenticated identity extracted cleanly as `Ctx<CurrentUser>` across web & native clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    /// Opaque user identifier.
    pub id: Id<User>,
    /// Assigned authorization roles.
    pub roles: Vec<String>,
}

/// Deterministic helper constructing a default authenticated test user for `TestClient`.
#[must_use]
pub fn test_user() -> CurrentUser {
    CurrentUser {
        id: Id::from_raw("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        roles: vec!["user".to_owned()],
    }
}

#[derive(Clone)]
struct OpaqueSessionRecord {
    user_id: String,
    roles: Vec<String>,
    csrf_token: String,
}

impl BinaryEncode for OpaqueSessionRecord {
    fn binary_encode(&self, out: &mut Vec<u8>) {
        self.user_id.binary_encode(out);
        self.roles.binary_encode(out);
        self.csrf_token.binary_encode(out);
    }
}

impl BinaryDecode for OpaqueSessionRecord {
    fn binary_decode(reader: &mut hyve::serde::BinaryReader<'_>) -> HyveResult<Self> {
        Ok(Self {
            user_id: String::binary_decode(reader)?,
            roles: Vec::<String>::binary_decode(reader)?,
            csrf_token: String::binary_decode(reader)?,
        })
    }
}

/// Credentials minted when creating an opaque session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokens {
    /// Bearer token / `HttpOnly` session cookie value.
    pub token: String,
    /// Per-session CSRF token paired with cookie clients (`X-CSRF-Token`).
    pub csrf_token: String,
    /// Single-use rotating refresh token.
    pub refresh_token: String,
}

#[derive(Default)]
struct RefreshState {
    active_refresh: HashMap<String, (Id<User>, Vec<String>, String)>,
    used_refresh: HashMap<String, Id<User>>,
    user_tokens: HashMap<String, HashSet<String>>,
}

/// L2-direct opaque session manager guaranteeing instant revocation (`02-core-contracts` §6).
#[derive(Clone)]
pub struct SessionStore {
    cache: Cache,
    ttl: Duration,
    refresh: Arc<Mutex<RefreshState>>,
}

impl SessionStore {
    /// Constructs a [`SessionStore`] backed directly by `cache`'s L2 tier.
    #[must_use]
    pub fn new(cache: Cache, ttl: Duration) -> Self {
        Self {
            cache,
            ttl,
            refresh: Arc::new(Mutex::new(RefreshState::default())),
        }
    }

    fn session_key(token: &str) -> String {
        format!("auth:session:{token}")
    }

    /// Creates a new high-entropy opaque session stored directly in L2 (bypassing L1).
    #[must_use]
    pub fn create_session(&self, user: &CurrentUser) -> SessionTokens {
        let token = format!("ses_{}", Ulid::generate());
        let csrf_token = format!("csrf_{}", Ulid::generate());
        let refresh_token = format!("rfr_{}", Ulid::generate());

        let record = OpaqueSessionRecord {
            user_id: user.id.as_str().to_owned(),
            roles: user.roles.clone(),
            csrf_token: csrf_token.clone(),
        };
        self.cache.set_raw(
            &Self::session_key(&token),
            to_bytes(&record),
            self.ttl,
            &[],
            false,
        );

        if let Ok(mut guard) = self.refresh.lock() {
            guard.active_refresh.insert(
                refresh_token.clone(),
                (user.id, user.roles.clone(), token.clone()),
            );
            guard
                .user_tokens
                .entry(user.id.as_str().to_owned())
                .or_default()
                .insert(token.clone());
        }

        SessionTokens {
            token,
            csrf_token,
            refresh_token,
        }
    }

    /// Immediately revokes a session token in L2 so subsequent requests fail instantly.
    pub fn revoke_session(&self, token: &str) {
        self.cache.delete_raw(&Self::session_key(token));
    }

    /// Rotates a refresh token once; if an already-used token is replayed, revokes ALL user sessions.
    pub fn rotate_refresh_token(&self, refresh_token: &str) -> HyveResult<SessionTokens> {
        let mut guard = self
            .refresh
            .lock()
            .map_err(|_| HyveError::internal("refresh lock poisoned"))?;

        if let Some(compromised_user) = guard.used_refresh.get(refresh_token).copied() {
            if let Some(tokens) = guard.user_tokens.remove(compromised_user.as_str()) {
                for tok in tokens {
                    self.cache.delete_raw(&Self::session_key(&tok));
                }
            }
            guard
                .active_refresh
                .retain(|_, (uid, _, _)| *uid != compromised_user);
            return Err(HyveError::unauthenticated(
                "refresh token replay detected; all user sessions revoked",
            ));
        }

        let Some((user_id, roles, old_session)) = guard.active_refresh.remove(refresh_token) else {
            return Err(HyveError::unauthenticated("invalid refresh token"));
        };
        guard.used_refresh.insert(refresh_token.to_owned(), user_id);
        self.cache.delete_raw(&Self::session_key(&old_session));
        drop(guard);

        Ok(self.create_session(&CurrentUser { id: user_id, roles }))
    }

    /// Verifies a Native/CLI `Bearer` token via L2 direct lookup.
    pub fn verify_bearer(&self, token: &str) -> HyveResult<CurrentUser> {
        let Some(raw) = self.cache.l2_get_raw(&Self::session_key(token)) else {
            return Err(HyveError::unauthenticated("session missing or revoked"));
        };
        let rec: OpaqueSessionRecord = from_bytes(&raw)?;
        Ok(CurrentUser {
            id: Id::from_raw(rec.user_id),
            roles: rec.roles,
        })
    }

    /// Verifies a Browser `HttpOnly` cookie session plus `X-CSRF-Token` for state-changing methods.
    pub fn verify_cookie_and_csrf(
        &self,
        cookie_token: &str,
        csrf_header: Option<&str>,
        require_csrf: bool,
    ) -> HyveResult<CurrentUser> {
        let Some(raw) = self.cache.l2_get_raw(&Self::session_key(cookie_token)) else {
            return Err(HyveError::unauthenticated(
                "cookie session missing or revoked",
            ));
        };
        let rec: OpaqueSessionRecord = from_bytes(&raw)?;
        if require_csrf && csrf_header != Some(rec.csrf_token.as_str()) {
            return Err(HyveError::forbidden("missing or mismatched X-CSRF-Token"));
        }
        Ok(CurrentUser {
            id: Id::from_raw(rec.user_id),
            roles: rec.roles,
        })
    }
}

/// Middleware guard enforcing authentication and optional RBAC roles on a `BackendComponent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthGuard {
    required_role: Option<String>,
}

impl AuthGuard {
    /// Requires an authenticated `CurrentUser`.
    #[must_use]
    pub const fn require_user() -> Self {
        Self {
            required_role: None,
        }
    }

    /// Requires an authenticated `CurrentUser` carrying `role`.
    #[must_use]
    pub fn require_role(role: impl Into<String>) -> Self {
        Self {
            required_role: Some(role.into()),
        }
    }

    /// Validates an optional [`CurrentUser`] against this guard.
    pub fn check(&self, user: Option<&CurrentUser>) -> HyveResult<()> {
        let Some(u) = user else {
            return Err(HyveError::unauthenticated(
                "authentication required for this component",
            ));
        };
        if let Some(ref role) = self.required_role
            && !u.roles.iter().any(|r| r == role)
        {
            return Err(HyveError::forbidden(format!(
                "missing required role `{role}`"
            )));
        }
        Ok(())
    }
}
