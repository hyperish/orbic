//! Fixed-window counters behind the `RateLimit` guard.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::Clock;
use crate::ttl::TtlMap;

/// Counters per guard and subject (principal or client address).
#[derive(Clone)]
pub struct RateLimitStore(Arc<Inner>);

struct Inner {
    map: TtlMap<(u64, String), (Instant, u32)>,
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
        let now = self.0.clock.now();
        let mut g = self.0.map.lock(now);
        let key = (scope, subject.to_owned());
        let entry = match g.live(&key, now) {
            Some(e) => e,
            None => g.map.entry(key).or_insert(((now, 0), now + window)),
        };
        let (start, count) = &mut entry.0;
        let allowed = *count < limit;
        if allowed {
            *count += 1;
        }
        RateDecision {
            allowed,
            limit,
            remaining: limit - *count,
            reset: (*start + window).saturating_duration_since(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

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
