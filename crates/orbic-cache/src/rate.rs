//! Fixed-window counters behind the `RateLimit` guard.

use std::sync::Arc;
use std::time::Duration;

use crate::clock::Clock;
use crate::ttl::TtlMap;

/// Counters per guard and subject (principal or client address).
#[derive(Clone)]
pub struct RateLimitStore(Arc<Inner>);

struct Inner {
    map: TtlMap<(String, String), u32>,
    clock: Clock,
}

/// The outcome of one counted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request is within the limit.
    pub allowed: bool,
    /// Requests allowed per window.
    pub limit: u32,
    /// Requests left in this window.
    pub remaining: u32,
    /// Time until the window resets.
    pub reset: Duration,
}

impl RateLimitStore {
    pub(crate) fn new(clock: Clock) -> Self {
        RateLimitStore(Arc::new(Inner {
            map: TtlMap::new(),
            clock,
        }))
    }

    /// Counts a request of `subject` against the limiter `scope`. A window
    /// starts at the subject's first request and resets after `window`.
    pub fn hit(&self, scope: u64, subject: &str, limit: u32, window: Duration) -> RateDecision {
        self.count(&scope.to_string(), subject, limit, window)
    }

    pub(crate) fn count(
        &self,
        scope: &str,
        subject: &str,
        limit: u32,
        window: Duration,
    ) -> RateDecision {
        let now = self.0.clock.now();
        let key = (scope.to_owned(), subject.to_owned());
        let mut g = self.0.map.lock(&key, now);
        let (count, expires) = match g.live(&key, now) {
            Some(e) => e,
            None => g.map.entry(key).or_insert((0, now + window)),
        };
        let allowed = *count < limit;
        if allowed {
            *count += 1;
        }
        RateDecision {
            allowed,
            limit,
            remaining: limit.saturating_sub(*count),
            reset: expires.saturating_duration_since(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn lowering_a_limit_preserves_the_existing_deadline_and_count() {
        let time = ManualClock::new();
        let store = RateLimitStore::new(time.clock());
        let window = Duration::from_secs(60);
        for _ in 0..3 {
            assert!(store.hit(1, "u", 3, window).allowed);
        }
        time.advance(Duration::from_secs(10));
        let denied = store.hit(1, "u", 1, Duration::from_secs(5));
        assert_eq!(
            (denied.allowed, denied.remaining, denied.reset),
            (false, 0, Duration::from_secs(50))
        );
    }

    #[test]
    fn windows_count_and_reset() {
        let time = ManualClock::new();
        let store = RateLimitStore::new(time.clock());
        let min = Duration::from_secs(60);
        let a = store.hit(1, "u", 2, min);
        assert_eq!((a.allowed, a.remaining, a.reset), (true, 1, min));
        time.advance(Duration::from_secs(10));
        assert!(store.hit(1, "u", 2, min).allowed);
        let c = store.hit(1, "u", 2, min);
        assert_eq!(
            (c.allowed, c.remaining, c.reset),
            (false, 0, Duration::from_secs(50))
        );
        assert!(store.hit(1, "v", 2, min).allowed, "per subject");
        assert!(store.hit(2, "u", 2, min).allowed, "per limiter");
        time.advance(Duration::from_secs(50));
        assert_eq!(store.hit(1, "u", 2, min).remaining, 1, "a new window");
    }
}
