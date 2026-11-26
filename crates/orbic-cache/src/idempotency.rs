//! The idempotency store behind the pipeline's `Idempotency-Key` gate
//! (`docs/03-adapters-and-pipeline.md` §1).

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::clock::Clock;
use crate::ttl::TtlMap;

/// How long a finished response is kept for reuse: 24 hours.
pub const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// What a key is scoped to: the authenticated principal, the method, the
/// concrete path and the client's key (`hyperish/docs` `03-api-protocol.md` §6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    /// The principal's identifier.
    pub principal: String,
    /// The bound method.
    pub method: &'static str,
    /// The request path.
    pub path: String,
    /// The `Idempotency-Key` value.
    pub key: String,
}

/// A stored outcome; the pipeline decides what it holds.
pub type Stored = Arc<dyn Any + Send + Sync>;

enum State {
    Running,
    Done(Stored),
}

struct Entry {
    owner: hyve_core::Ulid,
    digest: u128,
    state: State,
}

/// Keys and their outcomes, kept for [`IDEMPOTENCY_RETENTION`]. Never evicted
/// before that.
#[derive(Clone)]
pub struct IdempotencyStore(Arc<Inner>);

struct Inner {
    map: TtlMap<IdempotencyKey, Entry>,
    clock: Clock,
}

/// The result of [`IdempotencyStore::begin`].
pub enum Begin {
    /// First use: run the request, then [`Reservation::complete`]. Dropping the
    /// reservation releases the key so the request can be retried.
    Started(Reservation),
    /// The same request finished before: reuse its outcome.
    Replay(Stored),
    /// The same key is being processed.
    InProgress,
    /// The key was used with a different body or rail.
    Mismatch,
}

impl fmt::Debug for Begin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Begin::Started(_) => "Started",
            Begin::Replay(_) => "Replay",
            Begin::InProgress => "InProgress",
            Begin::Mismatch => "Mismatch",
        })
    }
}

impl IdempotencyStore {
    pub(crate) fn new(clock: Clock) -> Self {
        IdempotencyStore(Arc::new(Inner {
            map: TtlMap::new(),
            clock,
        }))
    }

    /// Claims `key` for a request whose rail and body hash to `digest`.
    pub fn begin(&self, key: IdempotencyKey, digest: u128) -> Begin {
        let now = self.0.clock.now();
        let mut g = self.0.map.lock(&key, now);
        if let Some((e, _)) = g.live(&key, now) {
            return if e.digest != digest {
                Begin::Mismatch
            } else {
                match &e.state {
                    State::Running => Begin::InProgress,
                    State::Done(v) => Begin::Replay(v.clone()),
                }
            };
        }
        let owner = hyve_core::Ulid::new();
        g.map.insert(
            key.clone(),
            (
                Entry {
                    owner,
                    digest,
                    state: State::Running,
                },
                now + IDEMPOTENCY_RETENTION,
            ),
        );
        Begin::Started(Reservation {
            owner,
            store: self.clone(),
            key: Some(key),
        })
    }
}

/// A claimed key; see [`Begin::Started`].
pub struct Reservation {
    owner: hyve_core::Ulid,
    store: IdempotencyStore,
    key: Option<IdempotencyKey>,
}

impl Reservation {
    /// Stores the outcome for reuse.
    pub fn complete(mut self, outcome: Stored) {
        let key = self.key.take().expect("completed once");
        let inner = &self.store.0;
        let now = inner.clock.now();
        let mut g = inner.map.lock(&key, now);
        if let Some((e, expires)) = g.live(&key, now)
            && e.owner == self.owner
        {
            e.state = State::Done(outcome);
            *expires = now + IDEMPOTENCY_RETENTION;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let inner = &self.store.0;
            let mut g = inner.map.lock(&key, inner.clock.now());
            if g.map
                .get(&key)
                .is_some_and(|(e, _)| e.owner == self.owner && matches!(e.state, State::Running))
            {
                g.map.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn key(k: &str) -> IdempotencyKey {
        IdempotencyKey {
            principal: "p".into(),
            method: "POST",
            path: "/x".into(),
            key: k.into(),
        }
    }

    #[test]
    fn completing_after_expiry_does_not_restore_an_old_reply() {
        let time = ManualClock::new();
        let store = IdempotencyStore::new(time.clock());
        let Begin::Started(owner) = store.begin(key("late"), 1) else {
            panic!()
        };
        time.advance(IDEMPOTENCY_RETENTION);
        owner.complete(Arc::new(9u32));
        assert!(matches!(store.begin(key("late"), 1), Begin::Started(_)));
    }

    #[test]
    fn claims_replays_and_releases() {
        let time = ManualClock::new();
        let store = IdempotencyStore::new(time.clock());
        let Begin::Started(r) = store.begin(key("a"), 1) else {
            panic!()
        };
        assert!(matches!(store.begin(key("a"), 1), Begin::InProgress));
        assert!(matches!(store.begin(key("a"), 2), Begin::Mismatch));
        r.complete(Arc::new(7u32));
        let Begin::Replay(v) = store.begin(key("a"), 1) else {
            panic!()
        };
        assert_eq!(v.downcast_ref::<u32>(), Some(&7));
        let mut other = key("a");
        other.principal = "q".into();
        assert!(
            matches!(store.begin(other, 1), Begin::Started(_)),
            "scoped by principal"
        );

        // A dropped reservation (failed, cancelled or panicking request) frees the key.
        let Begin::Started(r) = store.begin(key("b"), 1) else {
            panic!()
        };
        drop(r);
        assert!(matches!(store.begin(key("b"), 1), Begin::Started(_)));

        time.advance(IDEMPOTENCY_RETENTION);
        assert!(
            matches!(store.begin(key("a"), 2), Begin::Started(_)),
            "expired after retention"
        );
    }
}
