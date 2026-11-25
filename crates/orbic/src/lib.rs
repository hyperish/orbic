//! `orbic` — component-based backend framework of the Hyperish family.
//!
//! ```ignore
//! use hyve::prelude::*;
//! use orbic::prelude::*;
//!
//! pub fn app(db: Db, cache: Cache) -> OrbicApp {
//!     OrbicApp::new()
//!         .provide(db)
//!         .provide(cache)
//!         .openapi("/docs")
//!         .mount("/v1", project_component())
//! }
//! ```
//!
//! - Serve it with [`serve`]; test it without a port with [`TestClient`],
//!   which goes through the same HTTP translation as the server.
//! - `#[server]` functions are called through a transport provided in the
//!   scope: [`InProcess`] runs them through the pipeline without
//!   serialization (single binaries, port-less full-stack tests);
//!   `TestClient` also offers both HTTP rails (`hyve::api::TestClient`).
//!
//! Without the `server` feature (shared crates in client builds) this crate
//! exports only the macros, so `wasm32` clients never link the runtime
//! (`docs/01-dx-and-dsl.md` §1, 공유 모델의 `#[table]`).

#[cfg(feature = "server")]
mod in_process;
#[cfg(feature = "server")]
mod testing;

pub use orbic_macros::{key, sql, table, tag, ttl};

/// The result type of handlers: `HyveResult<T>` itself, so no conversion
/// happens between layers.
pub type OrbicResult<T> = hyve::HyveResult<T>;

#[cfg(feature = "server")]
pub use crate::in_process::InProcess;
#[cfg(feature = "server")]
pub use crate::testing::{TestClient, TestRequest, TestResponse};
#[cfg(feature = "server")]
pub use orbic_adapter_http::{HttpApp, HttpConfig, HttpError};
#[cfg(feature = "server")]
pub use orbic_auth::{AuthConfig, AuthGuard, Client, SessionUser, SignedIn};
#[cfg(feature = "server")]
pub use orbic_cache::{Cache, CacheOpts};
#[cfg(feature = "server")]
pub use orbic_core::{
    AssemblyError, BackendComponent, Credential, OrbicApp, Pipeline, Principal, RateLimit,
    component,
};
#[cfg(feature = "server")]
pub use orbic_db::{Db, Migrations, OrbicConfig, Tx};
#[cfg(feature = "server")]
pub use orbic_openapi::{OpenApiExt, OpenApiInfo};

/// Why an app could not be put behind HTTP.
#[cfg(feature = "server")]
#[derive(Debug)]
pub enum BuildError {
    /// The component tree is invalid.
    Assembly(AssemblyError),
    /// The HTTP settings or reserved paths are invalid.
    Http(HttpError),
}

#[cfg(feature = "server")]
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Assembly(e) => e.fmt(f),
            BuildError::Http(e) => e.fmt(f),
        }
    }
}

#[cfg(feature = "server")]
impl std::error::Error for BuildError {}

/// Assembles `app` and puts it behind the HTTP adapter, with its OpenAPI
/// document when the app asked for one.
#[cfg(feature = "server")]
pub fn http_app(app: OrbicApp) -> Result<HttpApp, BuildError> {
    let pipeline = app.build().map_err(BuildError::Assembly)?;
    let http = HttpApp::new(pipeline).map_err(BuildError::Http)?;
    orbic_openapi::attach(http).map_err(BuildError::Http)
}

/// Serves `app` over HTTP/1.1 on `listener` until `shutdown` resolves
/// ([`HttpApp::serve`]: plain HTTP only on loopback, graceful drain).
#[cfg(feature = "server")]
pub async fn serve(
    app: OrbicApp,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    let http = http_app(app).map_err(std::io::Error::other)?;
    http.serve(listener, shutdown).await
}

/// Everything most code needs: `use orbic::prelude::*;` (next to
/// `use hyve::prelude::*;`).
pub mod prelude {
    pub use crate::OrbicResult;
    #[cfg(feature = "server")]
    pub use crate::{
        AuthConfig, AuthGuard, BackendComponent, Cache, CacheOpts, Client, Db, OpenApiExt,
        OrbicApp, RateLimit, SessionUser, SignedIn, TestClient, Tx, component,
    };
    pub use orbic_macros::{key, sql, table, tag, ttl};
}

#[doc(hidden)]
pub mod __private {
    pub use hyve::HyveError;
    #[cfg(feature = "server")]
    pub use orbic_auth as auth;
    #[cfg(feature = "server")]
    pub use orbic_cache as cache;
    #[cfg(feature = "server")]
    pub use orbic_db as db;
}
