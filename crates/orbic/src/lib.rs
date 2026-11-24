//! `orbic` — component-based backend framework of the Hyperish family.
//!
//! Without the `server` feature (shared crates in client builds) this crate
//! exports only the macros, so `wasm32` clients never link the runtime
//! (`docs/01-dx-and-dsl.md` §1, 공유 모델의 `#[table]`).
//!
//! The facade is completed in Phase 1 step 8 (`docs/05-implementation-plan.md`).

pub use orbic_macros::{key, sql, table, tag, ttl};

/// The result type of handlers: `HyveResult<T>` itself, so no conversion
/// happens between layers.
pub type OrbicResult<T> = hyve::HyveResult<T>;

#[cfg(feature = "server")]
pub use orbic_core::{BackendComponent, OrbicApp, component};

/// Everything most code needs: `use orbic::prelude::*;` (next to
/// `use hyve::prelude::*;`).
pub mod prelude {
    pub use crate::OrbicResult;
    #[cfg(feature = "server")]
    pub use crate::{BackendComponent, OrbicApp, component};
    pub use orbic_macros::{key, sql, table, tag, ttl};
}

#[doc(hidden)]
pub mod __private {
    pub use hyve::HyveError;
    #[cfg(feature = "server")]
    pub use orbic_cache as cache;
    #[cfg(feature = "server")]
    pub use orbic_db as db;
}
