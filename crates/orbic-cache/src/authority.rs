//! Operations used by request guards. In a multi-replica deployment these
//! always go to the authoritative store, including on cache hits.

use crate::{Begin, Cache, IdempotencyKey, RateDecision, Reservation};
use hyve_core::{HyveError, HyveResult};
use std::sync::Arc;
use std::time::Duration;

/// One credential hash, its encoded record and its lifetime.
pub struct SessionWrite {
    /// Hash of the opaque credential, including the credential kind.
    pub key: String,
    /// Private encoded session data.
    pub value: Vec<u8>,
    /// How long this record can be used.
    pub ttl: Duration,
}

/// The idempotency gate for an encoded request outcome.
pub enum RequestBegin {
    /// The caller owns the reservation.
    Started(RequestReservation),
    /// A completed request with the same identity and input.
    Replay(Vec<u8>),
    /// Another request owns this key.
    InProgress,
    /// The existing key has different input.
    Mismatch,
}

/// A claimed request whose outcome can be persisted.
pub struct RequestReservation(pub(crate) Claim);

pub(crate) enum Claim {
    Local(Reservation),
    #[cfg(feature = "redis")]
    Remote(crate::redis::RemoteReservation),
}

impl RequestReservation {
    /// Persists the reply or releases the key after a retryable failure.
    pub async fn finish(self, outcome: Option<Vec<u8>>) -> HyveResult<()> {
        match self.0 {
            Claim::Local(r) => {
                if let Some(bytes) = outcome {
                    r.complete(Arc::new(bytes));
                }
                Ok(())
            }
            #[cfg(feature = "redis")]
            Claim::Remote(r) => r.finish(outcome).await,
        }
    }
}

impl Cache {
    /// Stores all credentials for a sign-in as one operation.
    pub async fn create_sessions(&self, family: &str, records: &[SessionWrite]) -> HyveResult<()> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote
                .write_sessions(family, records, None)
                .await
                .map(|_| ());
        }
        self.sessions().write(family, records, None);
        Ok(())
    }

    /// Reads the data and family of a live credential together.
    pub async fn session_record(&self, key: &str) -> HyveResult<Option<(Vec<u8>, String)>> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote.session(key).await;
        }
        Ok(self.sessions().record(key))
    }

    /// Rotates a refresh credential only if it still contains the expected
    /// live record. A concurrent replay revokes the entire family.
    pub async fn rotate_sessions(
        &self,
        family: &str,
        key: &str,
        expected: &[u8],
        records: &[SessionWrite],
    ) -> HyveResult<bool> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote
                .write_sessions(family, records, Some((key, expected)))
                .await;
        }
        Ok(self
            .sessions()
            .write(family, records, Some((key, expected))))
    }

    /// Extends a currently live credential in its family.
    pub async fn renew_session(&self, key: &str, family: &str, ttl: Duration) -> HyveResult<bool> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote.renew(key, family, ttl).await;
        }
        let _ = family;
        Ok(self.sessions().touch(key, ttl))
    }

    /// Revokes all live credentials belonging to the family.
    pub async fn revoke_sessions(&self, family: &str) -> HyveResult<()> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote.revoke(family).await;
        }
        self.sessions().revoke_family(family);
        Ok(())
    }

    /// Counts a request against the shared guard and subject.
    pub async fn count_request(
        &self,
        guard: &str,
        subject: &str,
        limit: u32,
        window: Duration,
    ) -> HyveResult<RateDecision> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote.count(guard, subject, limit, window).await;
        }
        Ok(self.rate_limits().count(guard, subject, limit, window))
    }

    /// Claims a key or obtains the encoded reply from its previous owner.
    pub async fn begin_request(
        &self,
        key: IdempotencyKey,
        digest: u128,
    ) -> HyveResult<RequestBegin> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            return remote.begin(key, digest).await;
        }
        Ok(match self.idempotency().begin(key, digest) {
            Begin::Started(r) => RequestBegin::Started(RequestReservation(Claim::Local(r))),
            Begin::InProgress => RequestBegin::InProgress,
            Begin::Mismatch => RequestBegin::Mismatch,
            Begin::Replay(s) => RequestBegin::Replay(
                s.downcast_ref::<Vec<u8>>()
                    .ok_or_else(|| HyveError::internal("invalid stored request outcome"))?
                    .clone(),
            ),
        })
    }
}
