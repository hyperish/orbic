//! `orbic::core` — Component composition (`BackendComponent`), extractors, guards,
//! Dual-Rail dispatch (`OrbicApp`), and portless in-memory `TestClient`.

use crate::auth::{AuthGuard, CurrentUser, SessionStore};
use crate::openapi::{OpenApiRouteMeta, build_openapi_document};
use crate::realtime::RealtimeHub;
use hyve::api::{
    DualRailMode, EndpointDescriptor, IntoServerEndpoint, ServerCallTransport,
    ServerInvocationContext,
};
use hyve::prelude::*;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// HTTP methods supported by [`BackendComponent`] routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
    /// HTTP PATCH.
    Patch,
    /// HTTP DELETE.
    Delete,
}

impl HttpMethod {
    /// Returns the uppercase HTTP verb.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    /// Returns `true` if this method mutates server state (requiring CSRF on cookie clients).
    #[must_use]
    pub const fn is_mutating(&self) -> bool {
        matches!(self, Self::Post | Self::Patch | Self::Delete)
    }
}

/// Cursor pagination query parameters parsed from `?after=...&limit=...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorParams {
    /// Opaque cursor pointing after the previous page's last item.
    pub after: Option<String>,
    /// Maximum number of items requested.
    pub limit: Option<usize>,
}

/// Query string extractor wrapper (`Query<CursorParams>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query<T>(pub T);

impl<T> Deref for Query<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Incoming request execution context passed cleanly across middleware and extractors.
pub struct RequestContext {
    /// Resolved HTTP/RPC method.
    pub method: HttpMethod,
    /// Resolved URL path parameters (e.g. `id` in `/projects/{id}`).
    pub path_params: HashMap<String, String>,
    /// Parsed query string key/value pairs.
    pub query: HashMap<String, String>,
    /// Raw incoming request body bytes.
    pub body: Vec<u8>,
    /// Active Dual-Rail wire encoding.
    pub mode: DualRailMode,
    /// Per-request correlation ID.
    pub request_id: RequestId,
    /// Authenticated identity (injected by guard / `SessionStore` / `TestClient::as_user`).
    pub user: Option<CurrentUser>,
    providers: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    app_inv_inserters: Vec<InvProviderFn>,
}

impl RequestContext {
    fn insert_provider<T: Clone + Send + Sync + 'static>(&mut self, value: T) {
        self.providers.insert(TypeId::of::<T>(), Box::new(value));
    }

    fn resolve_provider<T: Clone + 'static>(&self) -> Option<T> {
        self.providers
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
            .cloned()
    }
}

/// Extractor trait implemented by handler parameters (`Ctx<T>`, `Valid<T>`, `Query<T>`).
pub trait FromRequest: Sized {
    /// Extracts `Self` from the incoming [`RequestContext`].
    fn from_request(req: &RequestContext) -> HyveResult<Self>;

    /// Optional request body schema contributed to OpenAPI generation.
    fn request_schema() -> Option<ModelSchema> {
        None
    }
}

impl<T: Send + Sync + 'static> FromRequest for Ctx<T> {
    fn from_request(req: &RequestContext) -> HyveResult<Self> {
        if TypeId::of::<T>() == TypeId::of::<CurrentUser>()
            && let Some(ref user) = req.user
        {
            let boxed: Box<dyn Any> = Box::new(Ctx::new(user.clone()));
            if let Ok(resolved) = boxed.downcast::<Self>() {
                return Ok(*resolved);
            }
        }
        req.resolve_provider::<Self>().ok_or_else(|| {
            if TypeId::of::<T>() == TypeId::of::<CurrentUser>() {
                HyveError::unauthenticated("missing authenticated user context")
            } else {
                HyveError::internal(format!(
                    "missing provided context `Ctx<{}>`",
                    std::any::type_name::<T>()
                ))
            }
        })
    }
}

impl<T> FromRequest for Valid<T>
where
    T: BinaryDecode + FromJson + Validate + HasSchema,
{
    fn from_request(req: &RequestContext) -> HyveResult<Self> {
        let model: T = if req.mode == DualRailMode::FastBinary || req.body.starts_with(b"HYVE") {
            from_bytes(&req.body)?
        } else {
            let src = std::str::from_utf8(&req.body).map_err(|_| {
                HyveError::domain("invalid_request", "request body must be valid UTF-8")
            })?;
            from_json_str(src)?
        };
        Self::new(model)
    }

    fn request_schema() -> Option<ModelSchema> {
        Some(T::schema())
    }
}

impl FromRequest for Query<CursorParams> {
    fn from_request(req: &RequestContext) -> HyveResult<Self> {
        let after = req.query.get("after").cloned().filter(|s| !s.is_empty());
        let limit = req.query.get("limit").and_then(|s| s.parse::<usize>().ok());
        Ok(Self(CursorParams { after, limit }))
    }
}

/// Encoded response payload produced by route handlers.
#[derive(Debug, Clone)]
pub struct HandlerOutput {
    /// Raw payload bytes encoded according to the request's [`DualRailMode`].
    pub bytes: Vec<u8>,
    /// True if `bytes` already holds a `{ "data": ..., "page": ... }` envelope.
    pub already_enveloped_json: bool,
}

/// Converts a handler return value into Dual-Rail wire bytes.
pub trait IntoResponse {
    /// Encodes `self` into [`HandlerOutput`].
    fn into_response(self, mode: DualRailMode) -> HyveResult<HandlerOutput>;

    /// Optional schema contributed to OpenAPI `/docs`.
    fn response_schema() -> Option<ModelSchema> {
        None
    }
}

impl<T: BinaryEncode + ToJson + HasSchema> IntoResponse for T {
    fn into_response(self, mode: DualRailMode) -> HyveResult<HandlerOutput> {
        match mode {
            DualRailMode::FastBinary => Ok(HandlerOutput {
                bytes: to_bytes(&self),
                already_enveloped_json: false,
            }),
            DualRailMode::StandardJson => {
                let json = self.to_json();
                let is_page = matches!(&json, JsonValue::Object(fields) if fields.iter().any(|(k, _)| k == "data") && fields.iter().any(|(k, _)| k == "page"));
                if is_page {
                    Ok(HandlerOutput {
                        bytes: json.to_json_string().into_bytes(),
                        already_enveloped_json: true,
                    })
                } else {
                    let wrapped = JsonValue::Object(vec![("data".to_owned(), json)]);
                    Ok(HandlerOutput {
                        bytes: wrapped.to_json_string().into_bytes(),
                        already_enveloped_json: true,
                    })
                }
            }
        }
    }

    fn response_schema() -> Option<ModelSchema> {
        Some(T::schema())
    }
}

type BoxRouteFuture = Pin<Box<dyn Future<Output = HyveResult<HandlerOutput>> + 'static>>;
type DynRouteFn = Arc<dyn Fn(RequestContext) -> BoxRouteFuture + Send + Sync>;

/// Trait enabling clean standalone `async fn` handlers with extractors (`Ctx<T>`, `Valid<T>`, `Query<T>`).
pub trait Handler<Args>: Send + Sync + 'static {
    /// Calls the typed async handler from a [`RequestContext`].
    fn call(&self, req: RequestContext) -> BoxRouteFuture;

    /// Returns `(request_schema, response_schema)` for automatic OpenAPI collection.
    fn schemas(&self) -> (Option<ModelSchema>, Option<ModelSchema>);
}

macro_rules! impl_handler {
    ($($arg:ident : $ty:ident),*) => {
        impl<F, Fut, Res $(, $ty)*> Handler<($($ty,)*)> for F
        where
            F: Fn($($ty),*) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = HyveResult<Res>> + 'static,
            Res: IntoResponse + 'static,
            $($ty: FromRequest + 'static,)*
        {
            fn call(&self, req: RequestContext) -> BoxRouteFuture {
                let f = self.clone();
                let mode = req.mode;
                $(let $arg = match $ty::from_request(&req) {
                    Ok(v) => v,
                    Err(e) => return Box::pin(async move { Err(e) }),
                };)*
                Box::pin(async move {
                    let res = f($($arg),*).await?;
                    res.into_response(mode)
                })
            }

            fn schemas(&self) -> (Option<ModelSchema>, Option<ModelSchema>) {
                #[allow(unused_mut)]
                let mut req_schema = None;
                $(
                    if req_schema.is_none() {
                        req_schema = $ty::request_schema();
                    }
                )*
                (req_schema, Res::response_schema())
            }
        }
    };
}

impl_handler!();
impl_handler!(a1: T1);
impl_handler!(a1: T1, a2: T2);
impl_handler!(a1: T1, a2: T2, a3: T3);
impl_handler!(a1: T1, a2: T2, a3: T3, a4: T4);
impl_handler!(a1: T1, a2: T2, a3: T3, a4: T4, a5: T5);

/// Per-component sliding-window rate limiter injecting `RateLimit-*` headers and returning `429`.
#[derive(Debug, Clone)]
pub struct RateLimit {
    max_per_window: usize,
    window_secs: u64,
    used: Arc<AtomicUsize>,
}

impl RateLimit {
    /// Limits requests on this component to `max` per minute.
    #[must_use]
    pub fn per_minute(max: usize) -> Self {
        Self {
            max_per_window: max.max(1),
            window_secs: 60,
            used: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn acquire(&self) -> HyveResult<(usize, usize, u64)> {
        let prev = self.used.fetch_add(1, Ordering::SeqCst);
        let next = prev + 1;
        if next > self.max_per_window {
            return Err(HyveError::rate_limited(
                "rate limit exceeded",
                self.window_secs,
            ));
        }
        Ok((
            self.max_per_window,
            self.max_per_window - next,
            self.window_secs,
        ))
    }
}

/// Composable middleware wrapper accepted by `BackendComponent::wrap(...)`.
#[derive(Debug, Clone)]
pub enum ComponentWrapper {
    /// Authentication / RBAC guard.
    Auth(AuthGuard),
    /// Per-minute rate limiter.
    Rate(RateLimit),
}

impl From<AuthGuard> for ComponentWrapper {
    fn from(g: AuthGuard) -> Self {
        Self::Auth(g)
    }
}

impl From<RateLimit> for ComponentWrapper {
    fn from(r: RateLimit) -> Self {
        Self::Rate(r)
    }
}

#[derive(Clone)]
struct RegisteredRoute {
    method: HttpMethod,
    path: String,
    wrappers: Vec<ComponentWrapper>,
    handler: DynRouteFn,
    request_schema: Option<ModelSchema>,
    response_schema: Option<ModelSchema>,
    is_server_fn: bool,
}

/// Summary metadata of a mounted HTTP route (`RouteSpec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    /// HTTP verb.
    pub method: HttpMethod,
    /// Full mounted URL path pattern.
    pub path: String,
}

/// Registered realtime room route with its hub coordinator.
#[derive(Clone)]
pub struct RoomRegistration {
    /// Mounted room path (`/v1/projects/{id}/live`).
    pub path: String,
    /// Room hub instance.
    pub hub: RealtimeHub,
}

/// Registered background cron/periodic job entry (disabled automatically in `TestClient`).
#[derive(Debug, Clone)]
pub struct ScheduledJobSpec {
    /// Interval expression (`"1h"`, etc.).
    pub schedule: String,
    /// Distributed leader-lock key preventing duplicate multi-replica execution.
    pub leader_lock_key: String,
}

type ReqProviderFn = Arc<dyn Fn(&mut RequestContext) + Send + Sync>;
type InvProviderFn = Arc<dyn Fn(&mut ServerInvocationContext) + Send + Sync>;
type IdempotencyStore = Arc<Mutex<HashMap<String, (Vec<u8>, TestResponse)>>>;

#[derive(Clone, Default)]
struct ProviderBag {
    inserters: Vec<ReqProviderFn>,
    inv_inserters: Vec<InvProviderFn>,
}

impl ProviderBag {
    fn insert<T: Send + Sync + 'static>(&mut self, val: T) {
        let ctx = Ctx::new(val);
        self.inserters.push(Arc::new(move |req| {
            req.insert_provider(ctx);
        }));
        self.inv_inserters.push(Arc::new(move |inv| {
            inv.insert(ctx);
        }));
    }

    fn apply(&self, req: &mut RequestContext) {
        for f in &self.inserters {
            f(req);
        }
    }
}

/// Self-contained backend unit encapsulating routes, state, guards, realtime rooms, and scheduled jobs.
#[derive(Clone, Default)]
pub struct BackendComponent {
    providers: ProviderBag,
    wrappers: Vec<ComponentWrapper>,
    routes: Vec<RegisteredRoute>,
    rooms: Vec<RoomRegistration>,
    jobs: Vec<ScheduledJobSpec>,
}

/// Starts assembling a new [`BackendComponent`].
#[must_use]
pub fn component() -> BackendComponent {
    BackendComponent::default()
}

fn join_paths(prefix: &str, path: &str) -> String {
    let base = prefix.trim_end_matches('/');
    let suffix = path.trim_start_matches('/');
    if suffix.is_empty() {
        if base.is_empty() {
            "/".to_owned()
        } else {
            base.to_owned()
        }
    } else if base.is_empty() {
        format!("/{suffix}")
    } else {
        format!("{base}/{suffix}")
    }
}

impl BackendComponent {
    /// Provides a dependency `T` scoped to this component tree (resolved via `Ctx<T>`).
    #[must_use]
    pub fn provide<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.providers.insert(value);
        self
    }

    /// Wraps all routes in this component with an [`AuthGuard`] or [`RateLimit`].
    #[must_use]
    pub fn wrap(mut self, wrapper: impl Into<ComponentWrapper>) -> Self {
        self.wrappers.push(wrapper.into());
        self
    }

    /// Registers a `GET` route handler.
    #[must_use]
    pub fn get<Args>(self, path: &str, handler: impl Handler<Args>) -> Self {
        self.route(HttpMethod::Get, path, handler)
    }

    /// Registers a `POST` route handler.
    #[must_use]
    pub fn post<Args>(self, path: &str, handler: impl Handler<Args>) -> Self {
        self.route(HttpMethod::Post, path, handler)
    }

    /// Registers a `PATCH` route handler.
    #[must_use]
    pub fn patch<Args>(self, path: &str, handler: impl Handler<Args>) -> Self {
        self.route(HttpMethod::Patch, path, handler)
    }

    /// Registers a `DELETE` route handler.
    #[must_use]
    pub fn delete<Args>(self, path: &str, handler: impl Handler<Args>) -> Self {
        self.route(HttpMethod::Delete, path, handler)
    }

    /// Registers a route with an explicit [`HttpMethod`].
    #[must_use]
    pub fn route<Args>(
        mut self,
        method: HttpMethod,
        path: &str,
        handler: impl Handler<Args>,
    ) -> Self {
        let (request_schema, response_schema) = handler.schemas();
        let dyn_handler: DynRouteFn = Arc::new(move |req| handler.call(req));
        self.routes.push(RegisteredRoute {
            method,
            path: path.to_owned(),
            wrappers: Vec::new(),
            handler: dyn_handler,
            request_schema,
            response_schema,
            is_server_fn: false,
        });
        self
    }

    /// Explicitly mounts a `#[server]` inline server function inside this component's guard/mount scope.
    #[must_use]
    pub fn handle<M>(mut self, endpoint: impl IntoServerEndpoint<M>) -> Self {
        let desc: EndpointDescriptor = endpoint.into_endpoint();
        let method = match desc.method {
            "GET" => HttpMethod::Get,
            "POST" => HttpMethod::Post,
            "PATCH" => HttpMethod::Patch,
            "DELETE" => HttpMethod::Delete,
            _ => HttpMethod::Post,
        };
        let handler_fn = Arc::clone(&desc.handler);
        let dyn_handler: DynRouteFn = Arc::new(move |req| {
            let h = Arc::clone(&handler_fn);
            Box::pin(async move {
                let mut inv = ServerInvocationContext::new(req.body.clone(), req.mode);
                for apply in &req.app_inv_inserters {
                    apply(&mut inv);
                }
                if let Some(ref u) = req.user {
                    inv.insert(Ctx::new(u.clone()));
                }
                let out_bytes = h(inv).await?;
                Ok(HandlerOutput {
                    bytes: out_bytes,
                    already_enveloped_json: false,
                })
            })
        });
        self.routes.push(RegisteredRoute {
            method,
            path: desc.path.to_owned(),
            wrappers: Vec::new(),
            handler: dyn_handler,
            request_schema: None,
            response_schema: None,
            is_server_fn: true,
        });
        self
    }

    /// Registers a realtime room endpoint backed by [`RealtimeHub`].
    #[must_use]
    pub fn room<F, Fut>(mut self, path: &str, _handler: F) -> Self
    where
        F: Fn(RealtimeHub) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HyveResult<()>> + 'static,
    {
        self.rooms.push(RoomRegistration {
            path: path.to_owned(),
            hub: RealtimeHub::new(),
        });
        self
    }

    /// Registers a periodic background job backed by distributed leader locking (disabled in `TestClient`).
    #[must_use]
    pub fn every<F, Fut>(mut self, schedule: &str, _job: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HyveResult<()>> + 'static,
    {
        let idx = self.jobs.len() + 1;
        self.jobs.push(ScheduledJobSpec {
            schedule: schedule.to_owned(),
            leader_lock_key: format!("orbic:job:{schedule}:{idx}"),
        });
        self
    }

    /// Nests a child [`BackendComponent`] under `prefix`, inheriting parent guards & providers.
    #[must_use]
    pub fn mount(mut self, prefix: &str, child: BackendComponent) -> Self {
        self.providers.inserters.extend(child.providers.inserters);
        self.providers
            .inv_inserters
            .extend(child.providers.inv_inserters);
        let parent_wrappers = self.wrappers.clone();
        for mut r in child.routes {
            let mut combined = parent_wrappers.clone();
            combined.extend(child.wrappers.clone());
            combined.extend(r.wrappers);
            r.wrappers = combined;
            r.path = join_paths(prefix, &r.path);
            self.routes.push(r);
        }
        for mut rm in child.rooms {
            rm.path = join_paths(prefix, &rm.path);
            self.rooms.push(rm);
        }
        self.jobs.extend(child.jobs);
        self
    }
}

/// Root `orbic` backend application collecting components, OpenAPI docs, health/metrics, and idempotency state.
#[derive(Clone, Default)]
pub struct OrbicApp {
    providers: ProviderBag,
    routes: Vec<RegisteredRoute>,
    rooms: Vec<RoomRegistration>,
    jobs: Vec<ScheduledJobSpec>,
    openapi_path: Option<String>,
    session_store: Option<SessionStore>,
    idempotency: IdempotencyStore,
}

impl OrbicApp {
    /// Creates an empty [`OrbicApp`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a root-level shared dependency (`Db`, `Cache`, `SessionStore`, etc.).
    #[must_use]
    pub fn provide<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        let any_ref: &dyn Any = &value;
        if let Some(store) = any_ref.downcast_ref::<SessionStore>() {
            self.session_store = Some(store.clone());
        } else if let Some(cache) = any_ref.downcast_ref::<crate::cache::Cache>()
            && self.session_store.is_none()
        {
            self.session_store = Some(SessionStore::new(
                cache.clone(),
                std::time::Duration::from_secs(3600),
            ));
        }
        self.providers.insert(value);
        self
    }

    /// Enables auto-generated OpenAPI 3.1 serving at `path` (e.g. `"/docs"`).
    #[must_use]
    pub fn openapi(mut self, path: &str) -> Self {
        self.openapi_path = Some(path.to_owned());
        self
    }

    /// Mounts a [`BackendComponent`] under `prefix` (e.g. `"/v1"`).
    #[must_use]
    pub fn mount(mut self, prefix: &str, comp: BackendComponent) -> Self {
        let base_wrappers = comp.wrappers.clone();
        self.providers.inserters.extend(comp.providers.inserters);
        self.providers
            .inv_inserters
            .extend(comp.providers.inv_inserters);
        for mut r in comp.routes {
            let mut merged = base_wrappers.clone();
            merged.extend(r.wrappers);
            r.wrappers = merged;
            r.path = join_paths(prefix, &r.path);
            self.routes.push(r);
        }
        for mut rm in comp.rooms {
            rm.path = join_paths(prefix, &rm.path);
            self.rooms.push(rm);
        }
        self.jobs.extend(comp.jobs);
        self
    }

    /// Returns mounted route summaries.
    #[must_use]
    pub fn routes(&self) -> Vec<RouteSpec> {
        self.routes
            .iter()
            .map(|r| RouteSpec {
                method: r.method,
                path: r.path.clone(),
            })
            .collect()
    }

    /// Returns registered room descriptors.
    #[must_use]
    pub fn rooms(&self) -> &[RoomRegistration] {
        &self.rooms
    }

    /// Returns registered cron/background job specifications.
    #[must_use]
    pub fn scheduled_jobs(&self) -> &[ScheduledJobSpec] {
        &self.jobs
    }

    /// Returns the OpenAPI spec path if enabled.
    #[must_use]
    pub fn openapi_path(&self) -> Option<&str> {
        self.openapi_path.as_deref()
    }
}

fn match_path(pattern: &str, actual: &str) -> Option<HashMap<String, String>> {
    let p_segs: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let a_segs: Vec<&str> = actual.trim_matches('/').split('/').collect();
    if p_segs.len() != a_segs.len() {
        return None;
    }
    let mut params = HashMap::new();
    for (p, a) in p_segs.iter().zip(a_segs.iter()) {
        if let Some(key) = p.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            params.insert(key.to_owned(), (*a).to_owned());
        } else if p != a {
            return None;
        }
    }
    Some(params)
}

fn split_path_and_query(raw: &str) -> (&str, HashMap<String, String>) {
    let Some((path, q)) = raw.split_once('?') else {
        return (raw, HashMap::new());
    };
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(k.to_owned(), v.to_owned());
        }
    }
    (path, map)
}

/// Response returned by [`TestClient`] mirroring real HTTP status, headers, and Dual-Rail body.
#[derive(Debug, Clone)]
pub struct TestResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl TestResponse {
    /// HTTP status code (`200`, `400`, `401`, `403`, `404`, `409`, `429`, `500`).
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Looks up a response header case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == needle)
            .map(|(_, v)| v.as_str())
    }

    /// Returns raw UTF-8 response body text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Returns raw response bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    /// Parses the standard `{ "data": ... }` (or paginated `{ "data": [...], "page": ... }`) JSON response envelope into `T`.
    pub fn json<T: FromJson>(&self) -> HyveResult<T> {
        let j = JsonValue::parse(&self.text())?;
        if j.get("page").is_some() {
            T::from_json(&j)
        } else if let Some(data) = j.get("data") {
            T::from_json(data)
        } else {
            T::from_json(&j)
        }
    }

    /// Decodes a `hyve-serde` binary response frame into `T`.
    pub fn binary<T: BinaryDecode>(&self) -> HyveResult<T> {
        from_bytes(&self.body)
    }
}

/// Portless in-memory integration test client executing full `OrbicApp` middleware & handlers directly.
#[derive(Clone)]
pub struct TestClient {
    app: OrbicApp,
    rpc_prefix: String,
    default_user: Option<CurrentUser>,
}

struct DispatchInput<'a> {
    method: HttpMethod,
    raw_path: &'a str,
    explicit_user: Option<CurrentUser>,
    bearer: Option<String>,
    cookie: Option<String>,
    csrf: Option<String>,
    idempotency_key: Option<String>,
    body: Vec<u8>,
    mode: DualRailMode,
}

impl TestClient {
    /// Creates a [`TestClient`] wrapping `app` (background `.every` timers are disabled automatically).
    #[must_use]
    pub fn new(app: OrbicApp) -> Self {
        Self {
            app,
            rpc_prefix: String::new(),
            default_user: None,
        }
    }

    /// Sets an optional mount prefix for inline `#[server]` RPC resolution (e.g. `"/v1"`).
    #[must_use]
    pub fn with_rpc_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.rpc_prefix = prefix.into();
        self
    }

    /// Sets a default authenticated user for `#[server]` calls dispatched through this client.
    #[must_use]
    pub fn with_default_user(mut self, user: CurrentUser) -> Self {
        self.default_user = Some(user);
        self
    }

    /// Builds a [`ServerCallTransport`] dispatching `#[server]` client calls through this `TestClient`.
    #[must_use]
    pub fn into_transport(self) -> Arc<dyn ServerCallTransport> {
        Arc::new(self)
    }

    /// Prepares an in-memory `GET` request.
    #[must_use]
    pub fn get(&self, path: &str) -> TestRequest<'_> {
        self.request(HttpMethod::Get, path)
    }

    /// Prepares an in-memory `POST` request.
    #[must_use]
    pub fn post(&self, path: &str) -> TestRequest<'_> {
        self.request(HttpMethod::Post, path)
    }

    /// Prepares an in-memory `PATCH` request.
    #[must_use]
    pub fn patch(&self, path: &str) -> TestRequest<'_> {
        self.request(HttpMethod::Patch, path)
    }

    /// Prepares an in-memory `DELETE` request.
    #[must_use]
    pub fn delete(&self, path: &str) -> TestRequest<'_> {
        self.request(HttpMethod::Delete, path)
    }

    fn request(&self, method: HttpMethod, path: &str) -> TestRequest<'_> {
        TestRequest {
            client: self,
            method,
            path: path.to_owned(),
            user: None,
            bearer: None,
            cookie: None,
            csrf: None,
            idempotency_key: None,
            body: Vec::new(),
            mode: DualRailMode::StandardJson,
        }
    }

    async fn dispatch_call(&self, input: DispatchInput<'_>) -> TestResponse {
        let DispatchInput {
            method,
            raw_path,
            explicit_user,
            bearer,
            cookie,
            csrf,
            idempotency_key,
            body,
            mode,
        } = input;
        let request_id = RequestId::generate();
        let (path, query) = split_path_and_query(raw_path);

        // 1) Built-in operational endpoints
        if method == HttpMethod::Get && path == "/health" {
            return ok_json_resp(
                request_id,
                HashMap::new(),
                br#"{"data":{"status":"ok"}}"#.to_vec(),
            );
        }
        if method == HttpMethod::Get && path == "/metrics" {
            return ok_json_resp(
                request_id,
                HashMap::new(),
                format!(
                    r#"{{"data":{{"routes":{},"rooms":{},"jobs":{}}}}}"#,
                    self.app.routes.len(),
                    self.app.rooms.len(),
                    self.app.jobs.len()
                )
                .into_bytes(),
            );
        }
        if method == HttpMethod::Get
            && let Some(ref openapi_path) = self.app.openapi_path
            && path == openapi_path
        {
            let metas: Vec<OpenApiRouteMeta> = self
                .app
                .routes
                .iter()
                .map(|r| OpenApiRouteMeta {
                    method: r.method.as_str().to_owned(),
                    path: r.path.clone(),
                    request_schema: r.request_schema.clone(),
                    response_schema: r.response_schema.clone(),
                })
                .collect();
            let doc = build_openapi_document(&metas);
            return ok_json_resp(
                request_id,
                HashMap::new(),
                doc.to_json_string().into_bytes(),
            );
        }

        // 2) Route lookup (direct match or unique rpc suffix match for #[server])
        let matched = self
            .app
            .routes
            .iter()
            .find_map(|r| {
                if r.method == method {
                    match_path(&r.path, path).map(|params| (r, params))
                } else {
                    None
                }
            })
            .or_else(|| {
                if path.starts_with('/') {
                    let candidates: Vec<_> = self
                        .app
                        .routes
                        .iter()
                        .filter(|r| r.method == method && r.is_server_fn && r.path.ends_with(path))
                        .collect();
                    if candidates.len() == 1 {
                        Some((candidates[0], HashMap::new()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

        let Some((route, path_params)) = matched else {
            return err_resp(
                request_id,
                HashMap::new(),
                HyveError::not_found(
                    "route",
                    format!("no route matched {} {path}", method.as_str()),
                ),
            );
        };

        // 3) Idempotency-Key check
        let idem_cache_key = idempotency_key
            .as_ref()
            .map(|k| format!("{}:{}:{}", method.as_str(), route.path, k));
        if let Some(ref ikey) = idem_cache_key
            && let Ok(guard) = self.app.idempotency.lock()
            && let Some((prev_body, prev_resp)) = guard.get(ikey)
        {
            if prev_body == &body {
                return prev_resp.clone();
            }
            return err_resp(
                request_id,
                HashMap::new(),
                HyveError::conflict(
                    "idempotency_key_mismatch",
                    "Idempotency-Key reused with different request payload",
                ),
            );
        }

        // 4) Auth resolution: explicit test user -> Bearer -> Cookie+CSRF
        let resolved_user = if let Some(u) = explicit_user {
            Ok(Some(u))
        } else if let Some(ref tok) = bearer {
            match self.app.session_store {
                Some(ref store) => store.verify_bearer(tok).map(Some),
                None => Err(HyveError::internal("no SessionStore provided")),
            }
        } else if let Some(ref sid) = cookie {
            match self.app.session_store {
                Some(ref store) => store
                    .verify_cookie_and_csrf(sid, csrf.as_deref(), method.is_mutating())
                    .map(Some),
                None => Err(HyveError::internal("no SessionStore provided")),
            }
        } else {
            Ok(None)
        };

        let user = match resolved_user {
            Ok(u) => u,
            Err(err) => return err_resp(request_id, HashMap::new(), err),
        };

        // 5) Component wrappers (AuthGuard + RateLimit)
        let mut headers = HashMap::new();
        for w in &route.wrappers {
            match w {
                ComponentWrapper::Auth(guard) => {
                    if let Err(err) = guard.check(user.as_ref()) {
                        return err_resp(request_id, headers, err);
                    }
                }
                ComponentWrapper::Rate(limit) => match limit.acquire() {
                    Ok((lim, rem, reset)) => {
                        headers.insert("RateLimit-Limit".to_owned(), lim.to_string());
                        headers.insert("RateLimit-Remaining".to_owned(), rem.to_string());
                        headers.insert("RateLimit-Reset".to_owned(), reset.to_string());
                    }
                    Err(err) => {
                        if let Some(retry) = err.retry_after_secs() {
                            headers.insert("Retry-After".to_owned(), retry.to_string());
                        }
                        return err_resp(request_id, headers, err);
                    }
                },
            }
        }

        // 6) Invoke handler cleanly
        let mut req_ctx = RequestContext {
            method,
            path_params,
            query,
            body: body.clone(),
            mode,
            request_id: request_id.clone(),
            user,
            providers: HashMap::new(),
            app_inv_inserters: self.app.providers.inv_inserters.clone(),
        };
        self.app.providers.apply(&mut req_ctx);

        let resp = match (route.handler)(req_ctx).await {
            Ok(output) => {
                if mode == DualRailMode::StandardJson && !output.already_enveloped_json {
                    let wrapped = match JsonValue::parse(&String::from_utf8_lossy(&output.bytes)) {
                        Ok(inner) => JsonValue::Object(vec![("data".to_owned(), inner)])
                            .to_json_string()
                            .into_bytes(),
                        Err(_) => output.bytes,
                    };
                    ok_json_resp(request_id, headers, wrapped)
                } else {
                    ok_json_resp(request_id, headers, output.bytes)
                }
            }
            Err(err) => err_resp(request_id, headers, err),
        };

        if let Some(ikey) = idem_cache_key
            && resp.status() < 500
            && let Ok(mut guard) = self.app.idempotency.lock()
        {
            guard.insert(ikey, (body, resp.clone()));
        }
        resp
    }
}

impl ServerCallTransport for TestClient {
    fn mode(&self) -> DualRailMode {
        DualRailMode::FastBinary
    }

    fn call_raw(
        &self,
        method: &str,
        path: &str,
        payload_bytes: &[u8],
    ) -> Pin<Box<dyn Future<Output = HyveResult<Vec<u8>>> + 'static>> {
        let client = self.clone();
        let http_method = match method {
            "GET" => HttpMethod::Get,
            "PATCH" => HttpMethod::Patch,
            "DELETE" => HttpMethod::Delete,
            _ => HttpMethod::Post,
        };
        let full_path = if client.rpc_prefix.is_empty() {
            path.to_owned()
        } else {
            join_paths(&client.rpc_prefix, path)
        };
        let body = payload_bytes.to_vec();
        let user = client.default_user.clone();
        Box::pin(async move {
            let resp = client
                .dispatch_call(DispatchInput {
                    method: http_method,
                    raw_path: &full_path,
                    explicit_user: user,
                    bearer: None,
                    cookie: None,
                    csrf: None,
                    idempotency_key: None,
                    body,
                    mode: DualRailMode::FastBinary,
                })
                .await;
            if resp.status() < 400 {
                Ok(resp.bytes().to_vec())
            } else {
                Err(HyveError::domain("rpc_error", resp.text()))
            }
        })
    }
}

fn ok_json_resp(
    request_id: RequestId,
    mut headers: HashMap<String, String>,
    body: Vec<u8>,
) -> TestResponse {
    headers.insert("X-Request-Id".to_owned(), request_id.as_str().to_owned());
    TestResponse {
        status: 200,
        headers,
        body,
    }
}

fn err_resp(
    request_id: RequestId,
    mut headers: HashMap<String, String>,
    err: HyveError,
) -> TestResponse {
    let status = err.http_status();
    headers.insert("X-Request-Id".to_owned(), request_id.as_str().to_owned());
    if let Some(secs) = err.retry_after_secs() {
        headers.insert("Retry-After".to_owned(), secs.to_string());
    }
    let payload = JsonValue::Object(vec![(
        "error".to_owned(),
        JsonValue::Object(vec![
            ("code".to_owned(), JsonValue::Str(err.code().to_owned())),
            (
                "message".to_owned(),
                JsonValue::Str(err.message().to_owned()),
            ),
            ("details".to_owned(), JsonValue::Object(Vec::new())),
            (
                "requestId".to_owned(),
                JsonValue::Str(request_id.as_str().to_owned()),
            ),
        ]),
    )])
    .to_json_string()
    .into_bytes();
    TestResponse {
        status,
        headers,
        body: payload,
    }
}

/// Fluent builder returned by `TestClient::{get, post, patch, delete}`.
pub struct TestRequest<'a> {
    client: &'a TestClient,
    method: HttpMethod,
    path: String,
    user: Option<CurrentUser>,
    bearer: Option<String>,
    cookie: Option<String>,
    csrf: Option<String>,
    idempotency_key: Option<String>,
    body: Vec<u8>,
    mode: DualRailMode,
}

impl<'a> TestRequest<'a> {
    /// Impersonates `user` directly in unit tests.
    #[must_use]
    pub fn as_user(mut self, user: CurrentUser) -> Self {
        self.user = Some(user);
        self
    }

    /// Sends a Native/CLI `Authorization: Bearer <token>` credential.
    #[must_use]
    pub fn bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    /// Sends a Browser `HttpOnly` session cookie credential.
    #[must_use]
    pub fn cookie(mut self, session_cookie: impl Into<String>) -> Self {
        self.cookie = Some(session_cookie.into());
        self
    }

    /// Attaches the `X-CSRF-Token` header required on cookie mutations.
    #[must_use]
    pub fn csrf(mut self, csrf_token: impl Into<String>) -> Self {
        self.csrf = Some(csrf_token.into());
        self
    }

    /// Attaches an `Idempotency-Key` header per `docs/03-api-protocol.md` §6.
    #[must_use]
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// Attaches a JSON request body on the Standard JSON rail.
    #[must_use]
    pub fn json<T: ToJson>(mut self, value: &T) -> Self {
        self.mode = DualRailMode::StandardJson;
        self.body = to_json_string(value).into_bytes();
        self
    }

    /// Attaches a `hyve-serde` binary frame on the Fast Vertical Binary rail.
    #[must_use]
    pub fn binary<T: BinaryEncode>(mut self, value: &T) -> Self {
        self.mode = DualRailMode::FastBinary;
        self.body = to_bytes(value);
        self
    }

    /// Executes the in-memory request against `OrbicApp` and returns [`TestResponse`].
    pub async fn send(self) -> TestResponse {
        self.client
            .dispatch_call(DispatchInput {
                method: self.method,
                raw_path: &self.path,
                explicit_user: self.user,
                bearer: self.bearer,
                cookie: self.cookie,
                csrf: self.csrf,
                idempotency_key: self.idempotency_key,
                body: self.body,
                mode: self.mode,
            })
            .await
    }
}
