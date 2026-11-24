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
mod request;

pub use app::{AssemblyError, Idle, OrbicApp, Pipeline, RouteInfo};
pub use component::{BackendComponent, component};
pub use extract::{Extract, Parts, decode_body, from_params, parse_query};
pub use guard::{Guard, GuardCtx};
pub use handler::{Handler, encode};
pub use request::{
    Body, Credential, CredentialUpdate, Payload, Rail, RateLimitInfo, Request, RequestMeta,
    Response, ResponseMeta,
};
