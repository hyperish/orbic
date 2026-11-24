//! `orbic-core` — the `BackendComponent` tree and the transport-neutral
//! request pipeline every adapter and test client runs through
//! (`docs/03-adapters-and-pipeline.md` §1).
//!
//! - [`component`] builds a [`BackendComponent`]: routes ([`Handler`]s and
//!   `#[server]` functions), provided values (`Ctx<T>`) and [`Guard`]s.
//! - [`OrbicApp::build`] checks the tree at assembly (missing `Ctx<T>`,
//!   duplicate routes) and returns a [`Pipeline`].
//! - [`Pipeline::dispatch`] takes a [`Request`] and returns a [`Response`]:
//!   route matching (with the fast rail's function and schema checks), guards
//!   in order, extraction and validation, the handler, and the public form of
//!   errors. HTTP lives in `orbic-adapter-http`.
//! - `POST` routes reuse responses for a repeated `Idempotency-Key` through
//!   the app's `orbic_cache::Cache`, after the guards; [`RateLimit`] counts
//!   requests per [`Principal`] or client address.
//!
//! ```
//! use hyve::prelude::*;
//! use orbic_core::{OrbicApp, Request, component};
//!
//! struct Greeting(&'static str);
//!
//! async fn hello(greeting: Ctx<Greeting>) -> HyveResult<String> {
//!     Ok(greeting.0.to_owned())
//! }
//!
//! let app = OrbicApp::new()
//!     .provide(orbic_cache::Cache::in_memory())
//!     .provide(Greeting("hi"))
//!     .mount("/v1", component().get("/hello", hello))
//!     .build()
//!     .unwrap();
//! # let _ = app;
//! ```

mod app;
mod component;
mod extract;
mod guard;
mod handler;
mod idempotency;
mod rate_limit;
mod request;

pub use app::{AssemblyError, Idle, OrbicApp, Pipeline, RouteInfo};
pub use component::{BackendComponent, component};
pub use extract::{
    Extract, Params, ParamsFrom, Parts, RouteShape, decode_body, from_params, parse_query,
};
pub use guard::{Guard, GuardCtx, GuardShape};
pub use handler::{Handler, encode};
pub use rate_limit::RateLimit;
pub use request::{
    Body, Credential, CredentialSlot, CredentialUpdate, Payload, Principal, Rail, RateLimitInfo,
    Request, RequestMeta, Response, ResponseMeta,
};
