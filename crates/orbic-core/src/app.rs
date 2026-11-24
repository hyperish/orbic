//! `OrbicApp`: the root component, assembly checks, and the request pipeline
//! (`docs/03-adapters-and-pipeline.md` §1).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use hyve_api::{BoxFuture, ContextNeed, Method, ServerFnDescriptor, codes};
use hyve_core::{HyveError, Scope};
use orbic_cache::Cache;

use crate::component::{BackendComponent, Endpoint};
use crate::extract::Parts;
use crate::extract::RouteShape;
use crate::guard::GuardShape;
use crate::guard::{Guard, GuardCtx};
use crate::handler::Handler;
use crate::idempotency::{self, Gate};
use crate::request::{CredentialSlot, Rail, Request, Response, ResponseMeta};

/// The application: a root component plus assembly into a [`Pipeline`].
#[must_use = "build the app with `.build()`"]
#[derive(Default)]
pub struct OrbicApp {
    root: BackendComponent,
}

impl OrbicApp {
    /// An empty app.
    pub fn new() -> Self {
        Self::default()
    }

    /// Provides `value` to every route ([`BackendComponent::provide`]).
    pub fn provide<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.root = self.root.provide(value);
        self
    }

    /// Runs `guard` before every route ([`BackendComponent::wrap`]).
    pub fn wrap(mut self, guard: impl Guard) -> Self {
        self.root = self.root.wrap(guard);
        self
    }

    /// Mounts a component under `prefix`.
    pub fn mount(mut self, prefix: &str, component: BackendComponent) -> Self {
        self.root = self.root.mount(prefix, component);
        self
    }

    /// Adds a route at the root.
    pub fn route<H: Handler<A>, A>(mut self, method: Method, path: &str, handler: H) -> Self {
        self.root = self.root.route(method, path, handler);
        self
    }

    /// Resolves the component tree: every `Ctx<T>` a handler takes must be
    /// provided above it (or by a guard around it), and no method and path
    /// may be registered twice. All problems are reported together.
    pub fn build(self) -> Result<Pipeline, AssemblyError> {
        let root = Scope::root();
        let mut cx = Assembly::default();
        let top = cx.walk(self.root, "", &root, &[], &[]);
        let mut problems = cx.problems;

        let mut seen: HashMap<(Method, &str), &'static str> = HashMap::new();
        for r in &cx.routes {
            if let Some(first) = seen.insert((r.method, r.pattern.as_str()), r.name) {
                problems.push(format!(
                    "{} {} is registered twice (`{first}` and `{}`)",
                    method_str(r.method),
                    r.pattern,
                    r.name
                ));
            }
        }

        let mut router = matchit::Router::new();
        let mut paths: Vec<Vec<usize>> = Vec::new();
        let mut path_index: HashMap<String, usize> = HashMap::new();
        for (i, r) in cx.routes.iter().enumerate() {
            match path_index.get(&r.pattern) {
                Some(&p) => paths[p].push(i),
                None => {
                    if let Err(e) = router.insert(r.pattern.clone(), paths.len()) {
                        problems.push(format!("{} {}: {e}", method_str(r.method), r.pattern));
                        continue;
                    }
                    path_index.insert(r.pattern.clone(), paths.len());
                    paths.push(vec![i]);
                }
            }
        }
        let mut by_fn_id = HashMap::new();
        for (i, r) in cx.routes.iter().enumerate() {
            if let Some(d) = r.server_fn
                && by_fn_id.insert(d.id, i).is_some()
            {
                problems.push(format!(
                    "`#[server]` function `{}` is registered twice",
                    d.id
                ));
            }
        }
        if !problems.is_empty() {
            return Err(AssemblyError { problems });
        }
        Ok(Pipeline {
            inner: Arc::new(Inner {
                router,
                paths,
                routes: cx.routes,
                by_fn_id,
                root,
                top,
                state: Shutdown::default(),
            }),
        })
    }
}

/// Why an app could not be assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyError {
    problems: Vec<String>,
}

impl AssemblyError {
    /// One line per problem.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the orbic app cannot be assembled")?;
        for p in &self.problems {
            write!(f, "\n  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AssemblyError {}

/// A matched route and its decoded path parameters.
type Matched<'a> = (&'a Route, Vec<(String, String)>);

struct Route {
    method: Method,
    pattern: String,
    name: &'static str,
    endpoint: Endpoint,
    describe: crate::component::Describe,
    scope: Scope,
    guards: Vec<Arc<dyn Guard>>,
    server_fn: Option<&'static ServerFnDescriptor>,
}

#[derive(Default)]
struct Assembly {
    routes: Vec<Route>,
    problems: Vec<String>,
}

impl Assembly {
    fn walk(
        &mut self,
        c: BackendComponent,
        prefix: &str,
        parent: &Scope,
        guards: &[Arc<dyn Guard>],
        provided: &[ContextNeed],
    ) -> Scope {
        let scope = parent.child();
        let mut provided = provided.to_vec();
        for (need, provide) in c.provides {
            provide(&scope);
            provided.push(need);
        }
        let mut guards = guards.to_vec();
        for g in c.guards {
            for need in g.needs() {
                if !provided.iter().any(|p| p.type_id == need.type_id) {
                    self.problems.push(format!(
                        "guard `{}` wrapped at `{}` takes `{}`, but nothing provides it there \
                         (provide it with `.provide(...)` on that component or one it is mounted under)",
                        g.name(),
                        if prefix.is_empty() { "/" } else { prefix },
                        need.type_name
                    ));
                }
            }
            provided.extend(g.provides());
            guards.push(g);
        }
        let has_cache = provided
            .iter()
            .any(|p| p.type_id == ContextNeed::of::<Cache>().type_id);
        for r in c.routes {
            let Some(pattern) = join(prefix, &r.path) else {
                self.problems.push(format!(
                    "{} {:?} (`{}`): paths start with `/`",
                    method_str(r.method),
                    r.path,
                    r.name
                ));
                continue;
            };
            for need in &r.needs {
                if !provided.iter().any(|p| p.type_id == need.type_id) {
                    self.problems.push(format!(
                        "{} {pattern}: `{}` takes `Ctx<{}>`, but nothing provides it here \
                         (provide it with `.provide(...)` on this component or one it is mounted under, \
                         or wrap a guard that provides it)",
                        method_str(r.method),
                        r.name,
                        need.type_name
                    ));
                }
            }
            if r.method == Method::Post && !has_cache {
                self.problems.push(format!(
                    "POST {pattern}: `POST` routes accept `Idempotency-Key`, which needs a `Cache` \
                     provided here or above (`.provide(Cache::in_memory())`)"
                ));
            }
            self.routes.push(Route {
                method: r.method,
                pattern,
                name: r.name,
                endpoint: r.endpoint,
                describe: r.describe,
                scope: scope.clone(),
                guards: guards.clone(),
                server_fn: r.server_fn,
            });
        }
        for (p, child) in c.children {
            match join(prefix, &p) {
                Some(next) if p.starts_with('/') => {
                    let next = if next == "/" { String::new() } else { next };
                    self.walk(child, &next, &scope, &guards, &provided);
                }
                _ => self
                    .problems
                    .push(format!("mount prefix {p:?}: prefixes start with `/`")),
            }
        }
        scope
    }
}

/// `prefix` + `path`, both starting with `/` (the prefix may be empty).
fn join(prefix: &str, path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let prefix = prefix.trim_end_matches('/');
    Some(match (prefix, path) {
        ("", p) => p.to_owned(),
        (pre, "/") => pre.to_owned(),
        (pre, p) => format!("{pre}{p}"),
    })
}

fn method_str(m: Method) -> &'static str {
    match m {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Patch => "PATCH",
        Method::Delete => "DELETE",
    }
}

/// The assembled app. Cheap to clone; every adapter and test client calls
/// [`dispatch`](Pipeline::dispatch).
#[derive(Clone)]
pub struct Pipeline {
    inner: Arc<Inner>,
}

struct Inner {
    router: matchit::Router<usize>,
    /// Route indexes per path pattern.
    paths: Vec<Vec<usize>>,
    routes: Vec<Route>,
    by_fn_id: HashMap<&'static str, usize>,
    root: Scope,
    /// The root component's scope (values `OrbicApp::provide` gave).
    top: Scope,
    state: Shutdown,
}

/// A registered route, for documentation generators.
#[derive(Clone)]
pub struct RouteInfo<'a> {
    /// The method.
    pub method: Method,
    /// The full pattern (`/v1/projects/{id}`).
    pub pattern: &'a str,
    /// The handler's name.
    pub name: &'static str,
    /// The `#[server]` contract, for routes added with `handle`.
    pub server_fn: Option<&'static ServerFnDescriptor>,
    /// What the guards around the route add, outermost first.
    pub guards: Vec<GuardShape>,
    describe: &'a crate::component::Describe,
}

impl RouteInfo<'_> {
    /// What the route reads and answers; named types are registered in
    /// `registry`.
    pub fn shape(&self, registry: &mut hyve_validate::SchemaRegistry) -> RouteShape {
        (self.describe)(registry)
    }
}

impl std::fmt::Debug for RouteInfo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteInfo")
            .field("method", &self.method)
            .field("pattern", &self.pattern)
            .field("name", &self.name)
            .field("guards", &self.guards)
            .finish_non_exhaustive()
    }
}

impl Pipeline {
    /// The registered routes in registration order.
    pub fn routes(&self) -> impl Iterator<Item = RouteInfo<'_>> {
        self.inner.routes.iter().map(|r| RouteInfo {
            method: r.method,
            pattern: &r.pattern,
            name: r.name,
            server_fn: r.server_fn,
            guards: r.guards.iter().map(|g| g.describe()).collect(),
            describe: &r.describe,
        })
    }

    /// Runs a request through routing, guards, extraction and the handler.
    pub fn dispatch(&self, request: Request) -> BoxFuture<'static, Response> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.dispatch(request).await })
    }

    /// Stops accepting requests: new ones fail with a retryable
    /// `service_unavailable`. Requests already running continue.
    pub fn begin_shutdown(&self) {
        self.inner.state.accepting.store(false, Ordering::SeqCst);
        self.inner.state.wake();
    }

    /// Whether the pipeline still accepts requests (`false` after
    /// [`begin_shutdown`](Self::begin_shutdown)).
    pub fn is_accepting(&self) -> bool {
        self.inner.state.accepting.load(Ordering::SeqCst)
    }

    /// A value provided at the app's root (`OrbicApp::provide`), for test
    /// clients and adapters.
    pub fn provided<T: Send + Sync + 'static>(&self) -> Option<hyve_core::Ctx<T>> {
        self.inner.top.get::<T>()
    }

    /// Requests currently running.
    pub fn in_flight(&self) -> usize {
        self.inner.state.in_flight.load(Ordering::SeqCst)
    }

    /// Completes when no request is running. Combine with a timer and
    /// [`cancel_in_flight`](Self::cancel_in_flight) for a shutdown deadline.
    pub fn idle(&self) -> Idle {
        Idle {
            inner: self.inner.clone(),
        }
    }

    /// Cancels the scope of every running request (and of the app); handlers
    /// observe it through their scope's cancellation token.
    pub fn cancel_in_flight(&self) {
        self.inner.root.cancellation().cancel();
    }
}

#[derive(Default)]
struct Shutdown {
    accepting: AtomicBoolTrue,
    in_flight: AtomicUsize,
    waiters: Mutex<Vec<Waker>>,
}

/// An `AtomicBool` that starts `true`.
struct AtomicBoolTrue(AtomicBool);

impl Default for AtomicBoolTrue {
    fn default() -> Self {
        AtomicBoolTrue(AtomicBool::new(true))
    }
}

impl std::ops::Deref for AtomicBoolTrue {
    type Target = AtomicBool;
    fn deref(&self) -> &AtomicBool {
        &self.0
    }
}

impl Shutdown {
    fn wake(&self) {
        for w in self
            .waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            w.wake();
        }
    }
}

/// Counts a running request until dropped.
struct InFlight<'a>(&'a Shutdown);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.wake();
        }
    }
}

/// Future returned by [`Pipeline::idle`].
pub struct Idle {
    inner: Arc<Inner>,
}

impl Future for Idle {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let state = &self.inner.state;
        if state.in_flight.load(Ordering::SeqCst) == 0 {
            return Poll::Ready(());
        }
        state
            .waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cx.waker().clone());
        if state.in_flight.load(Ordering::SeqCst) == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

fn framework_error(code: &'static str, message: String) -> HyveError {
    HyveError::domain(code, message)
}

impl Inner {
    async fn dispatch(&self, req: Request) -> Response {
        let request_id = req.meta.request_id.clone().unwrap_or_default();
        let rail = req.rail;
        let mut meta = ResponseMeta::default();
        let outcome = if self.state.accepting.load(Ordering::SeqCst) {
            self.state.in_flight.fetch_add(1, Ordering::SeqCst);
            let _counted = InFlight(&self.state);
            self.run(req, &mut meta).await
        } else {
            Err(
                HyveError::infra("service_unavailable", "the server is shutting down")
                    .retryable(true),
            )
        };
        Response {
            rail,
            outcome: outcome.map_err(|e| {
                let public = e.to_public();
                // The original is about to be hidden; it survives only here
                // (Phase 1 log, `docs/04-open-questions.md` decision 3).
                if public.code() != e.code() || public.message() != e.message() {
                    eprintln!(
                        "orbic [{}] {:?} {}: {}",
                        request_id.as_str(),
                        e.kind(),
                        e.code(),
                        e.message()
                    );
                }
                public.with_request_id(request_id.clone())
            }),
            request_id,
            meta,
        }
    }

    async fn run(
        &self,
        req: Request,
        meta: &mut ResponseMeta,
    ) -> Result<crate::request::Payload, HyveError> {
        let (route, params) = self.find(&req)?;
        let scope = route.scope.child();
        let slot = std::sync::Arc::new(CredentialSlot::default());
        scope.provide_arc(slot.clone());
        let result = scope
            .scoped(async {
                let method = route.server_fn.map_or(route.method, |d| d.method);
                {
                    let mut cx = GuardCtx {
                        scope: &scope,
                        method,
                        path: &req.path,
                        route: &route.pattern,
                        meta: &req.meta,
                        response: meta,
                    };
                    for g in &route.guards {
                        g.check(&mut cx).await?;
                    }
                }
                let reservation = if method == Method::Post {
                    match idempotency::begin(&scope, &req, meta)? {
                        Ok(Gate::Pass) => None,
                        Ok(Gate::Run(r)) => Some(r),
                        Err(replayed) => return Ok(replayed),
                    }
                } else {
                    None
                };
                let parts = Parts {
                    scope: scope.clone(),
                    rail: req.rail,
                    params,
                    query: req.query,
                    body: req.body,
                    meta: req.meta,
                };
                let outcome = (route.endpoint)(parts).await;
                if let Some(r) = reservation {
                    idempotency::finish(r, &outcome);
                }
                if outcome.is_ok()
                    && let Some(update) = slot.take()
                {
                    meta.credential_update = Some(update);
                }
                outcome
            })
            .await;
        scope.dispose();
        result
    }

    /// Route matching, including the fast rail's function and schema checks,
    /// before any guard runs.
    fn find(&self, req: &Request) -> Result<Matched<'_>, HyveError> {
        if req.rail == Rail::InProcess {
            let id = req
                .meta
                .server_fn
                .as_ref()
                .map(|(id, _)| id.as_str())
                .unwrap_or_default();
            let route = self.by_fn_id.get(id).map(|&i| &self.routes[i]).ok_or_else(|| {
                framework_error(
                    "route_not_found",
                    format!("no route for the `#[server]` function `{id}`; register it with `.handle(...)`"),
                )
            })?;
            return Ok((route, Vec::new()));
        }
        let matched = self.router.at(&req.path).map_err(|_| {
            framework_error("route_not_found", format!("no route for {}", req.path))
        })?;
        let params = matched
            .params
            .iter()
            .map(|(k, v)| Ok((k.to_owned(), percent_decode_path(v)?)))
            .collect::<Result<Vec<_>, HyveError>>()?;
        let candidates = &self.paths[*matched.value];
        if req.rail == Rail::Fast {
            let Some((id, fingerprint)) = &req.meta.server_fn else {
                return Err(framework_error(
                    codes::UNSUPPORTED_MEDIA_TYPE,
                    "binary requests must name a `#[server]` function".into(),
                ));
            };
            let route = candidates
                .iter()
                .map(|&i| &self.routes[i])
                .find(|r| r.server_fn.is_some_and(|d| d.id == id))
                .ok_or_else(|| {
                    framework_error(
                        codes::UNSUPPORTED_MEDIA_TYPE,
                        format!(
                            "{} has no `#[server]` function `{id}` taking binary requests",
                            req.path
                        ),
                    )
                })?;
            let d = route.server_fn.expect("found by its descriptor");
            if d.fingerprint != *fingerprint {
                return Err(framework_error(
                    codes::SCHEMA_MISMATCH,
                    format!(
                        "`{id}` has schema {}, the request was built for {fingerprint:016x}",
                        d.fingerprint_hex()
                    ),
                ));
            }
            return Ok((route, params));
        }
        match candidates
            .iter()
            .map(|&i| &self.routes[i])
            .find(|r| r.method == req.method)
        {
            Some(r) => Ok((r, params)),
            None => {
                let allowed: Vec<&str> = candidates
                    .iter()
                    .map(|&i| method_str(self.routes[i].method))
                    .collect();
                Err(framework_error(
                    "method_not_allowed",
                    format!(
                        "{} {} is not allowed; use {}",
                        method_str(req.method),
                        req.path,
                        allowed.join(", ")
                    ),
                )
                .with_detail("allowed", allowed.join(", ")))
            }
        }
    }
}

fn percent_decode_path(s: &str) -> Result<String, HyveError> {
    if !s.contains('%') {
        return Ok(s.to_owned());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let byte = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(byte) = byte else {
                return Err(HyveError::invalid_request(format!(
                    "invalid percent-encoding in `{s}`"
                )));
            };
            out.push(byte);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| HyveError::invalid_request(format!("`{s}` is not valid UTF-8 once decoded")))
}
