//! `orbic-auth` — opaque sessions (`docs/03-adapters-and-pipeline.md` §2,
//! 인증·세션).
//!
//! - The app registers its principal type once: `impl SessionUser for
//!   CurrentUser` and `type AuthGuard = orbic::AuthGuard<CurrentUser>;`, so
//!   `.wrap(AuthGuard::require_user())` provides `Ctx<CurrentUser>`.
//! - Web clients get the session in an `HttpOnly` cookie and a CSRF token in
//!   the body, echoed as `X-CSRF-Token` on `POST`/`PATCH`/`DELETE`; native
//!   and CLI clients get a Bearer token and a refresh token.
//! - Credentials are 256-bit CSPRNG values; the store (`Cache::sessions`,
//!   the authoritative in-process store) keeps only their SHA-256. Every
//!   request looks the session up, so revocation applies to the next request.
//! - Refresh tokens are single use; presenting a used one revokes the whole
//!   family. Revoking a session revokes its refresh tokens.
//!
//! Login, logout and session routes are the app's; they call [`sign_in`],
//! [`sign_out`], [`refresh`] and [`current`].

mod token;

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use hyve::api::{BoxFuture, ContextNeed, Method, Wire};
use hyve::prelude::*;
use hyve::serde::Binary as _;
use orbic_cache::Cache;
use orbic_core::{
    Credential, CredentialSlot, CredentialUpdate, Guard, GuardCtx, GuardShape, Principal,
};

/// A type the app authenticates as (`CurrentUser`): stored in the session
/// and provided to handlers.
pub trait SessionUser: Wire + Clone + Sync {
    /// The stable identifier of the principal (a user ID, never a token);
    /// the idempotency gate and `RateLimit` scope their state by it.
    fn principal(&self) -> String;
}

/// Session lifetimes. Provide one to change them (`.provide(AuthConfig {
/// .. })`); otherwise [`AuthConfig::default`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthConfig {
    /// A session unused this long expires (sliding): 30 minutes.
    pub idle: Duration,
    /// A session expires this long after sign-in whatever its use: 7 days.
    pub absolute: Duration,
    /// A refresh token expires this long after it was issued: 30 days.
    pub refresh: Duration,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            idle: Duration::from_secs(30 * 60),
            absolute: Duration::from_secs(7 * 24 * 60 * 60),
            refresh: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

/// How a client keeps its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    /// A browser: session cookie plus CSRF token.
    Web,
    /// A native app or CLI: Bearer token plus refresh token.
    Bearer,
}

/// What a sign-in or refresh returns for the response body.
#[model]
#[derive(Debug, Clone, PartialEq)]
pub struct SignedIn {
    /// Web: the token to send as `X-CSRF-Token`.
    pub csrf_token: Option<String>,
    /// Bearer: the access token.
    pub access_token: Option<String>,
    /// Bearer: the refresh token.
    pub refresh_token: Option<String>,
    /// Seconds the session stays valid without use.
    pub expires_in: u64,
}

/// The tokens of a new session ([`issue`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Issued {
    /// The session token (cookie value or Bearer token).
    pub session: String,
    /// Web: the CSRF token.
    pub csrf: Option<String>,
    /// Bearer: the refresh token.
    pub refresh: Option<String>,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Issued(..)")
    }
}

#[model]
struct SessionRecord {
    user: Vec<u8>,
    principal: String,
    csrf: Option<String>,
    /// Unix microseconds after which the session is dead regardless of use.
    until: i64,
}

#[model]
struct RefreshRecord {
    user: Vec<u8>,
    principal: String,
    used: bool,
}

/// The session the current request authenticated with (provided by
/// [`AuthGuard`]).
#[derive(Debug, Clone)]
pub struct CurrentSession {
    key: String,
    family: String,
    csrf: Option<String>,
}

fn unauthenticated() -> HyveError {
    HyveError::domain("unauthenticated", "sign in to continue")
}

fn config(scope: &Scope) -> AuthConfig {
    scope.get::<AuthConfig>().map(|c| *c).unwrap_or_default()
}

fn decode<T: Wire>(bytes: &[u8]) -> HyveResult<T> {
    let aligned = hyve::serde::AlignedBytes::copy_from(bytes);
    T::from_binary(&aligned)
        .map_err(|e| HyveError::internal(format!("unreadable session record: {e}")))
}

fn now_micros() -> i64 {
    Timestamp::now().as_unix_micros()
}

/// Creates a session for `user` in `cache` (`TestClient::as_user` uses this
/// directly).
pub fn issue<U: SessionUser>(
    cache: &Cache,
    cfg: &AuthConfig,
    user: &U,
    client: Client,
) -> HyveResult<Issued> {
    let session = token::new()?;
    let family = token::new()?;
    let csrf = match client {
        Client::Web => Some(token::new()?),
        Client::Bearer => None,
    };
    let user_bytes = user.to_binary()?.to_vec();
    let until =
        now_micros().saturating_add(i64::try_from(cfg.absolute.as_micros()).unwrap_or(i64::MAX));
    let record = SessionRecord {
        user: user_bytes.clone(),
        principal: user.principal(),
        csrf: csrf.clone(),
        until,
    };
    let store = cache.sessions();
    store.put(
        token::key("s", &session),
        family.clone(),
        record.to_binary()?.to_vec(),
        cfg.idle,
    );
    let refresh = match client {
        Client::Bearer => {
            let r = token::new()?;
            let rec = RefreshRecord {
                user: user_bytes,
                principal: user.principal(),
                used: false,
            };
            store.put(
                token::key("r", &r),
                family,
                rec.to_binary()?.to_vec(),
                cfg.refresh,
            );
            Some(r)
        }
        Client::Web => None,
    };
    Ok(Issued {
        session,
        csrf,
        refresh,
    })
}

fn signed_in(
    issued: Issued,
    cfg: &AuthConfig,
    client: Client,
    slot: Option<&CredentialSlot>,
) -> SignedIn {
    let expires_in = cfg.idle.as_secs();
    match client {
        Client::Web => {
            let csrf = issued.csrf.clone().unwrap_or_default();
            if let Some(slot) = slot {
                slot.set(CredentialUpdate::SetSession {
                    token: issued.session,
                    csrf: csrf.clone(),
                });
            }
            SignedIn {
                csrf_token: Some(csrf),
                access_token: None,
                refresh_token: None,
                expires_in,
            }
        }
        Client::Bearer => SignedIn {
            csrf_token: None,
            access_token: Some(issued.session),
            refresh_token: issued.refresh,
            expires_in,
        },
    }
}

fn request_scope() -> HyveResult<(Scope, Arc<Cache>)> {
    let scope = Scope::current()
        .ok_or_else(|| HyveError::internal("orbic-auth: call this inside a request handler"))?;
    let cache = scope.require::<Cache>()?;
    Ok((scope, Ctx::arc(&cache).clone()))
}

/// Signs `user` in from a handler (the app's login route, after it checked
/// the credentials): web clients get the session cookie (set by the
/// adapter) and a CSRF token, Bearer clients a token pair.
pub async fn sign_in<U: SessionUser>(user: &U, client: Client) -> HyveResult<SignedIn> {
    let (scope, cache) = request_scope()?;
    let cfg = config(&scope);
    let issued = issue(&cache, &cfg, user, client)?;
    Ok(signed_in(
        issued,
        &cfg,
        client,
        scope.get::<CredentialSlot>().as_deref(),
    ))
}

/// The current request's session (behind an [`AuthGuard`]), for a session
/// lookup route: web clients get their CSRF token again (a reloaded page has
/// lost it), Bearer clients only the idle lifetime; tokens are never echoed.
pub async fn current() -> HyveResult<SignedIn> {
    let (scope, _) = request_scope()?;
    let session = scope
        .require::<CurrentSession>()
        .map_err(|_| unauthenticated())?;
    Ok(SignedIn {
        csrf_token: session.csrf.clone(),
        access_token: None,
        refresh_token: None,
        expires_in: config(&scope).idle.as_secs(),
    })
}

/// Signs the current request's session out (behind an [`AuthGuard`]): the
/// session and every refresh token derived from it are revoked, and web
/// clients' cookie is cleared.
pub async fn sign_out() -> HyveResult<()> {
    let (scope, cache) = request_scope()?;
    let current = scope
        .require::<CurrentSession>()
        .map_err(|_| unauthenticated())?;
    cache.sessions().revoke_family(&current.family);
    if let Some(slot) = scope.get::<CredentialSlot>() {
        slot.set(CredentialUpdate::ClearSession);
    }
    Ok(())
}

/// Exchanges a refresh token for a new token pair (Bearer clients). Each
/// refresh token works once; a reused one revokes its whole family (a
/// stolen copy is no longer worth anything).
pub async fn refresh<U: SessionUser>(refresh_token: &str) -> HyveResult<SignedIn> {
    let (scope, cache) = request_scope()?;
    let cfg = config(&scope);
    let store = cache.sessions();
    let key = token::key("r", refresh_token);
    let Some((bytes, family)) = store.take(&key) else {
        return Err(unauthenticated());
    };
    let mut record: RefreshRecord = decode(&bytes)?;
    if record.used {
        store.revoke_family(&family);
        return Err(unauthenticated());
    }
    // Keep a used marker so a replay is recognised, until it would expire.
    record.used = true;
    store.put(
        key,
        family.clone(),
        record.to_binary()?.to_vec(),
        cfg.refresh,
    );

    let user: U = decode(&record.user)?;
    let session = token::new()?;
    let next = token::new()?;
    let until =
        now_micros().saturating_add(i64::try_from(cfg.absolute.as_micros()).unwrap_or(i64::MAX));
    let rec = SessionRecord {
        user: record.user.clone(),
        principal: user.principal(),
        csrf: None,
        until,
    };
    store.put(
        token::key("s", &session),
        family.clone(),
        rec.to_binary()?.to_vec(),
        cfg.idle,
    );
    let fresh = RefreshRecord {
        user: record.user,
        principal: record.principal,
        used: false,
    };
    store.put(
        token::key("r", &next),
        family,
        fresh.to_binary()?.to_vec(),
        cfg.refresh,
    );
    Ok(SignedIn {
        csrf_token: None,
        access_token: Some(session),
        refresh_token: Some(next),
        expires_in: cfg.idle.as_secs(),
    })
}

type Rule<U> = Arc<dyn Fn(&U) -> bool + Send + Sync>;

/// Authenticates every request against the session store and provides the
/// user (`Ctx<U>`), its [`Principal`] and the [`CurrentSession`]. Missing,
/// expired or revoked credentials are `401 unauthenticated`; a web session's
/// `POST`/`PATCH`/`DELETE` without the matching `X-CSRF-Token` is
/// `403 csrf_token_invalid`; a failed [`require`](Self::require) rule is
/// `403 forbidden`.
pub struct AuthGuard<U> {
    rule: Option<Rule<U>>,
    _user: PhantomData<fn() -> U>,
}

impl<U: SessionUser> AuthGuard<U> {
    /// Any signed-in user.
    pub fn require_user() -> Self {
        AuthGuard {
            rule: None,
            _user: PhantomData,
        }
    }

    /// A signed-in user for whom `rule` holds (roles, ownership flags).
    pub fn require(rule: impl Fn(&U) -> bool + Send + Sync + 'static) -> Self {
        AuthGuard {
            rule: Some(Arc::new(rule)),
            _user: PhantomData,
        }
    }
}

impl<U: SessionUser> Guard for AuthGuard<U> {
    fn name(&self) -> &'static str {
        "AuthGuard"
    }

    fn needs(&self) -> Vec<ContextNeed> {
        vec![ContextNeed::of::<Cache>()]
    }

    fn provides(&self) -> Vec<ContextNeed> {
        vec![
            ContextNeed::of::<U>(),
            ContextNeed::of::<Principal>(),
            ContextNeed::of::<CurrentSession>(),
        ]
    }

    fn describe(&self) -> GuardShape {
        let mut errors = vec!["unauthenticated", "csrf_token_invalid"];
        if self.rule.is_some() {
            errors.push("forbidden");
        }
        GuardShape {
            authenticates: true,
            errors,
        }
    }

    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>> {
        Box::pin(async move {
            let cache = cx.scope.require::<Cache>()?;
            let (token, csrf) = match &cx.meta.credential {
                Some(Credential::Session { token, csrf }) => {
                    (token.as_str(), Some(csrf.as_deref()))
                }
                Some(Credential::Bearer(token)) => (token.as_str(), None),
                None => return Err(unauthenticated()),
            };
            let store = cache.sessions();
            let key = token::key("s", token);
            let (Some(bytes), Some(family)) = (store.get(&key), store.family(&key)) else {
                return Err(unauthenticated());
            };
            let record: SessionRecord = decode(&bytes)?;
            if record.until <= now_micros() {
                store.remove(&key);
                return Err(unauthenticated());
            }
            // Cookies travel automatically; state changes must prove the page
            // read the CSRF token.
            if let Some(sent) = csrf
                && matches!(cx.method, Method::Post | Method::Patch | Method::Delete)
            {
                let ok = match (sent, record.csrf.as_deref()) {
                    (Some(sent), Some(expected)) => token::same(sent, expected),
                    _ => false,
                };
                if !ok {
                    return Err(HyveError::domain(
                        "csrf_token_invalid",
                        "missing or wrong X-CSRF-Token",
                    ));
                }
            }
            let user: U = decode(&record.user)?;
            if let Some(rule) = &self.rule
                && !rule(&user)
            {
                return Err(HyveError::domain("forbidden", "not allowed"));
            }
            store.touch(&key, config(cx.scope).idle);
            cx.scope.provide(Principal::new(record.principal));
            cx.scope.provide(CurrentSession {
                key,
                family,
                csrf: record.csrf,
            });
            cx.scope.provide(user);
            Ok(())
        })
    }
}

impl CurrentSession {
    /// The session family (shared with its refresh tokens).
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Whether this is the session stored under `key` (tests).
    #[doc(hidden)]
    pub fn key(&self) -> &str {
        &self.key
    }
}
