//! `orbic-cache` — the in-memory cache and stores (Phase 1).
//!
//! - [`Cache`]: L1 cache with Singleflight and tag invalidation, and the
//!   non-evicting stores the pipeline and guards use. Apps provide one.
//! - [`CacheKey`], [`Tag`]: what `key!` and `tag!` build.
//!
//! The L2 (Redis/Valkey) tier is Phase 2 (`docs/05-implementation-plan.md`).

mod cache;
mod clock;
mod idempotency;
mod key;
mod rate;
mod session;
mod ttl;

pub use cache::{Cache, CacheOpts, DEFAULT_CAPACITY};
pub use clock::{Clock, ManualClock};
pub use idempotency::{
    Begin, IDEMPOTENCY_RETENTION, IdempotencyKey, IdempotencyStore, Reservation, Stored,
};
pub use key::{CacheKey, KeyPart, Tag};
pub use rate::{RateDecision, RateLimitStore};
pub use session::SessionStore;
