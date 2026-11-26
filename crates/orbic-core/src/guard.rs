//! Guards: checks a component `wrap`s around its routes.

use hyve_api::{BoxFuture, ContextNeed, Method};
use hyve_core::{HyveResult, Scope};

use crate::request::{RequestMeta, ResponseMeta};

/// What a guard sees and may change.
pub struct GuardCtx<'r> {
    /// The request scope; a guard provides request values (`CurrentUser`)
    /// here for the handler.
    pub scope: &'r Scope,
    /// Stable identity of this guard at its component mount, shared across replicas.
    pub guard_key: &'r str,
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

/// What a guard adds to a route's description (`orbic-openapi`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardShape {
    /// The route needs a credential.
    pub authenticates: bool,
    /// Error codes the guard may answer with.
    pub errors: Vec<&'static str>,
}

/// A check around a component's routes, added with `wrap`. Guards run in the
/// order they were wrapped, outermost component first, each exactly once per
/// request; the first error ends the request.
pub trait Guard: Send + Sync + 'static {
    /// The guard's name, for assembly messages.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Values the guard takes from the scope it is wrapped in (checked when
    /// the app is assembled, like a handler's `Ctx<T>`).
    fn needs(&self) -> Vec<ContextNeed> {
        Vec::new()
    }

    /// Values this guard provides to the request scope, so handlers below it
    /// may take them as `Ctx<T>` (checked when the app is assembled).
    fn provides(&self) -> Vec<ContextNeed> {
        Vec::new()
    }

    /// What the guard adds to the description of the routes it wraps.
    fn describe(&self) -> GuardShape {
        GuardShape::default()
    }

    /// Checks the request.
    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>>;
}
