//! `BackendComponent`: routes, provided values and guards grouped by domain,
//! assembled with plain method chaining (`docs/01-dx-and-dsl.md` §1).

use std::sync::Arc;

use hyve_api::{BoxFuture, Call, ContextNeed, Method, ServerFnDescriptor, ServerHandler, StubFn};
use hyve_core::{HyveResult, Scope};
use hyve_serde::from_json;

use hyve_validate::SchemaRegistry;

use crate::extract::{
    Params, ParamsFrom, Parts, RouteShape, decode_body, from_params, parse_query,
};
use crate::guard::Guard;
use crate::handler::{Handler, encode};
use crate::request::{Body, Payload, Rail};

pub(crate) type Endpoint =
    Arc<dyn Fn(Parts) -> BoxFuture<'static, HyveResult<Payload>> + Send + Sync>;

pub(crate) type Describe = Arc<dyn Fn(&mut SchemaRegistry) -> RouteShape + Send + Sync>;

/// Takes a `#[server]` function's typed request out of a [`Call`].
pub(crate) type TakeRequest = fn(Call) -> Result<Box<dyn std::any::Any + Send>, Call>;

fn take_request<M: ServerHandler>(call: Call) -> Result<Box<dyn std::any::Any + Send>, Call> {
    call.into_request::<M::Request>()
        .map(|r| Box::new(r) as Box<dyn std::any::Any + Send>)
}

pub(crate) struct RouteDef {
    pub method: Method,
    pub path: String,
    /// The handler's type name, for messages.
    pub name: &'static str,
    pub needs: Vec<ContextNeed>,
    pub endpoint: Endpoint,
    pub describe: Describe,
    pub server_fn: Option<&'static ServerFnDescriptor>,
    pub take: Option<TakeRequest>,
}

type ProvideFn = Box<dyn FnOnce(&Scope) + Send>;

/// A group of routes with the values they share and the guards around them.
#[must_use = "a component does nothing until it is mounted into an `OrbicApp`"]
#[derive(Default)]
pub struct BackendComponent {
    pub(crate) provides: Vec<(ContextNeed, ProvideFn)>,
    pub(crate) guards: Vec<Arc<dyn Guard>>,
    pub(crate) routes: Vec<RouteDef>,
    pub(crate) children: Vec<(String, BackendComponent)>,
}

/// An empty component.
pub fn component() -> BackendComponent {
    BackendComponent::default()
}

impl BackendComponent {
    /// Provides `value` to this component and the components mounted under it
    /// (`Ctx<T>`). The nearest provider wins; siblings do not see it.
    pub fn provide<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.provides.push((
            ContextNeed::of::<T>(),
            Box::new(move |s: &Scope| {
                s.provide(value);
            }),
        ));
        self
    }

    /// Runs `guard` before every route of this component and the components
    /// mounted under it, after the guards wrapped earlier.
    pub fn wrap(mut self, guard: impl Guard) -> Self {
        self.guards.push(Arc::new(guard));
        self
    }

    /// Adds a route.
    pub fn route<H: Handler<A>, A>(mut self, method: Method, path: &str, handler: H) -> Self {
        let endpoint: Endpoint = Arc::new(move |parts: Parts| {
            let rail = parts.rail;
            let fut = handler.call(parts);
            Box::pin(async move { encode(fut.await?, rail) })
        });
        self.routes.push(RouteDef {
            method,
            path: path.to_owned(),
            name: std::any::type_name::<H>(),
            needs: H::needs(),
            endpoint,
            describe: {
                let describe: fn(&mut SchemaRegistry) -> RouteShape = H::describe;
                Arc::new(describe)
            },
            server_fn: None,
            take: None,
        });
        self
    }

    /// Adds a `GET` route.
    pub fn get<H: Handler<A>, A>(self, path: &str, handler: H) -> Self {
        self.route(Method::Get, path, handler)
    }

    /// Adds a `POST` route.
    pub fn post<H: Handler<A>, A>(self, path: &str, handler: H) -> Self {
        self.route(Method::Post, path, handler)
    }

    /// Adds a `PATCH` route.
    pub fn patch<H: Handler<A>, A>(self, path: &str, handler: H) -> Self {
        self.route(Method::Patch, path, handler)
    }

    /// Adds a `DELETE` route.
    pub fn delete<H: Handler<A>, A>(self, path: &str, handler: H) -> Self {
        self.route(Method::Delete, path, handler)
    }

    /// Registers a `#[server]` function at its bound method and path (under
    /// this component's mount prefix). Only builds with the declaring crate's
    /// `server` feature have the handler body this needs.
    pub fn handle<F, A>(mut self, _function: F) -> Self
    where
        F: StubFn<A>,
        F::Marker: ServerHandler,
    {
        let d = <F::Marker as hyve_api::ServerFn>::descriptor();
        self.routes.push(RouteDef {
            method: d.method,
            path: d.path.to_owned(),
            name: d.id,
            needs: <F::Marker as ServerHandler>::context(),
            endpoint: server_endpoint::<F::Marker>(),
            describe: server_shape(d),
            server_fn: Some(d),
            take: Some(take_request::<F::Marker>),
        });
        self
    }

    /// Mounts `child` under `prefix` (`"/v1"`); it sees this component's
    /// values and runs inside its guards.
    pub fn mount(mut self, prefix: &str, child: BackendComponent) -> Self {
        self.children.push((prefix.to_owned(), child));
        self
    }
}

/// A `#[server]` function's request model is its body, or for `GET` and
/// `DELETE` its parameters (`docs/01-dx-and-dsl.md` §2, 본문 없는 메서드).
fn server_shape(d: &'static ServerFnDescriptor) -> Describe {
    Arc::new(move |registry: &mut SchemaRegistry| {
        let request = (d.request_schema)(registry);
        let mut shape = RouteShape {
            response: (d.response_schema)(registry),
            ..RouteShape::default()
        };
        if d.args.is_empty() {
        } else if matches!(d.method, Method::Get | Method::Delete) {
            shape.params.push(Params {
                from: ParamsFrom::PathOrQuery,
                name: (d.args.len() == 1).then(|| arg_json_name(d.args[0])),
                schema: request,
            });
        } else {
            shape.body = Some(request);
        }
        shape
    })
}

fn server_endpoint<M: ServerHandler>() -> Endpoint {
    Arc::new(|mut parts: Parts| {
        Box::pin(async move {
            let d = M::descriptor();
            let rail = parts.rail;
            let request = match (rail, &parts.body) {
                (Rail::Standard, _) if matches!(d.method, Method::Get | Method::Delete) => {
                    request_from_params::<M>(&parts, d)?
                }
                (Rail::Standard, Body::Empty) if d.args.is_empty() => {
                    from_json::<M::Request>("null")
                        .map_err(hyve_serde::DecodeError::into_request_error)?
                }
                _ => decode_body::<M::Request>(rail, parts.take_body())?,
            };
            let response = M::handle(parts.scope.clone(), request).await?;
            encode(response, rail)
        })
    })
}

/// `GET`/`DELETE` on the standard rail: the request model's fields come from
/// the query string and the path; a single non-model argument uses its own
/// name (`docs/01-dx-and-dsl.md` §2, 본문 없는 메서드).
fn request_from_params<M: ServerHandler>(
    parts: &Parts,
    d: &ServerFnDescriptor,
) -> HyveResult<M::Request> {
    if d.args.is_empty() {
        return from_json::<M::Request>("null")
            .map_err(hyve_serde::DecodeError::into_request_error);
    }
    let mut pairs = parse_query(&parts.query)?;
    pairs.extend(parts.params.iter().cloned());
    let single = (d.args.len() == 1).then(|| arg_json_name(d.args[0]));
    from_params::<M::Request>(&pairs, single.as_deref(), "request")
}

/// Argument names travel as JSON names (`owner_id` → `ownerId`), like the
/// fields of the generated request model.
pub fn arg_json_name(name: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = !out.is_empty();
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}
