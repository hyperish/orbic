//! Guards: checks a component `wrap`s around its routes.

use hyve_api::{BoxFuture, ContextNeed, Method};
use hyve_core::{HyveResult, Scope};

use crate::request::{RequestMeta, ResponseMeta};

/// What a guard sees and may change.
pub struct GuardCtx<'r> {
    /// The request scope; a guard provides request values (`CurrentUser`)
    /// here for the handler.
    pub scope: &'r Scope,
    /// The method the route is bound to (for `#[server]` functions on the
    /// fast rail, the bound method, not `POST`).
    pub method: Method,
    /// The request path.
    pub path: &'r str,
    /// The route pattern (`/v1/projects/{id}`).
    pub route: &'r str,
    /// Request metadata.
    pub meta: &'r RequestMeta,
    /// Response metadata the guard may add to (rate-limit state).
    pub response: &'r mut ResponseMeta,
}

/// A check around a component's routes, added with `wrap`. Guards run in the
/// order they were wrapped, outermost component first, each exactly once per
/// request; the first error ends the request.
pub trait Guard: Send + Sync + 'static {
    /// Values this guard provides to the request scope, so handlers below it
    /// may take them as `Ctx<T>` (checked when the app is assembled).
    fn provides(&self) -> Vec<ContextNeed> {
        Vec::new()
    }

    /// Checks the request.
    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>>;
}
