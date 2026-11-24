//! `orbic-cache` — cache keys and tags (`key!`, `tag!`).
//!
//! The in-memory cache, Singleflight, tag invalidation and the idempotency
//! and rate-limit stores arrive in Phase 1 step 3
//! (`docs/05-implementation-plan.md` §2).

mod key;

pub use key::{CacheKey, KeyPart, Tag};
