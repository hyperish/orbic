//! Mount a verified build in the app scope. Public immutable files and guarded
//! page routes share one deployment snapshot; each response owns its bytes.
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::Frame;
use hyve::{HyveError, HyveResult, RequestId};
use orbic_core::{Guard, GuardCtx, Pipeline, RequestMeta, ResponseMeta};
use serge_web_ssr::{SergeRoute, VerifiedBundle, digest};
pub use serge_web_ssr::{SergeSsrHandler, SsrBody, SsrFuture, SsrRequest, SsrResponse};
use std::{
    collections::BTreeMap,
    io,
    path::Path,
    sync::{Arc, RwLock},
};

/// A finite or streamed HTTP body.
pub type SiteBody = UnsyncBoxBody<Bytes, HyveError>;
/// A single owned chunk in the socket body's representation.
pub fn full(bytes: Bytes) -> SiteBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}
const RESERVED: [&str; 5] = ["/v1", "/health", "/metrics", "/openapi.json", "/openapi"];
const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

fn under(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}

#[derive(Default, Clone)]
/// Policies bound to UI path groups. The most specific group wins.
pub struct Policies {
    groups: BTreeMap<String, Vec<Arc<dyn Guard>>>,
    reserved: Vec<String>,
}
impl Policies {
    /// Start with no public routes.
    pub fn new() -> Self {
        Self::default()
    }
    /// Explicitly make a path group public.
    pub fn public(mut self, prefix: &str) -> Self {
        self.groups.entry(prefix.into()).or_default();
        self
    }
    /// Apply an Orbic component guard to the group, in registration order.
    pub fn guard(mut self, prefix: &str, guard: impl Guard) -> Self {
        self.groups
            .entry(prefix.into())
            .or_default()
            .push(Arc::new(guard));
        self
    }
    /// Reserve an additional API or management prefix.
    pub fn reserve(mut self, prefix: &str) -> Self {
        self.reserved.push(prefix.into());
        self
    }
    fn group(&self, path: &str) -> Option<(&String, &Vec<Arc<dyn Guard>>)> {
        self.groups
            .iter()
            .filter(|(p, _)| under(path, p))
            .max_by_key(|(p, _)| p.len())
    }
}

struct Build {
    data: VerifiedBundle,
    ssr: Option<Arc<dyn SergeSsrHandler>>,
    page_tags: BTreeMap<String, String>,
}
struct Mounted {
    current: Arc<Build>,
    previous_assets: BTreeMap<String, Bytes>,
}
/// A reloadable host. Provide it on the root `OrbicApp`.
pub struct SergeHost {
    policy: Policies,
    mounted: RwLock<Mounted>,
}

impl SergeHost {
    /// Load and check every manifest entry before accepting requests.
    pub fn open(
        root: &Path,
        policy: Policies,
        ssr: Option<Arc<dyn SergeSsrHandler>>,
    ) -> io::Result<Self> {
        let current = Self::verify(root, &policy, ssr)?;
        Ok(Self {
            policy,
            mounted: RwLock::new(Mounted {
                current,
                previous_assets: BTreeMap::new(),
            }),
        })
    }
    fn verify(
        root: &Path,
        policy: &Policies,
        ssr: Option<Arc<dyn SergeSsrHandler>>,
    ) -> io::Result<Arc<Build>> {
        let reserved: Vec<_> = RESERVED
            .into_iter()
            .chain(policy.reserved.iter().map(String::as_str))
            .collect();
        if policy
            .groups
            .keys()
            .chain(&policy.reserved)
            .any(|p| !p.starts_with('/') || (p.len() > 1 && p.ends_with('/')))
        {
            return Err(io::Error::other("invalid UI policy prefix"));
        }
        let data = VerifiedBundle::open(root, &reserved)?;
        for r in &data.manifest.routes {
            if policy.group(r.pattern()).is_none() {
                return Err(io::Error::other(format!(
                    "no server policy for {}",
                    r.pattern()
                )));
            }
            if matches!(r, SergeRoute::Ssr { .. }) && ssr.is_none() {
                return Err(io::Error::other("SSR route needs a handler"));
            }
        }
        if ssr
            .as_ref()
            .is_some_and(|h| h.build_id() != data.manifest.build_id)
        {
            return Err(io::Error::other(
                "SSR handler build ID differs from the bundle",
            ));
        }
        let page_tags = data
            .pages
            .iter()
            .map(|(k, v)| (k.clone(), format!("\"{}\"", digest(v))))
            .collect();
        Ok(Arc::new(Build {
            data,
            ssr,
            page_tags,
        }))
    }
    /// Installs the candidate only after full verification. Streams already
    /// in flight and the previous release's immutable URLs stay usable.
    pub fn reload(&self, root: &Path, ssr: Option<Arc<dyn SergeSsrHandler>>) -> io::Result<()> {
        let next = Self::verify(root, &self.policy, ssr)?;
        let mut state = self.mounted.write().unwrap_or_else(|e| e.into_inner());
        if next.data.manifest.build_id == state.current.data.manifest.build_id {
            return Err(io::Error::other(
                "a replacement bundle needs a new build ID",
            ));
        }
        state.previous_assets = state.current.data.assets.clone();
        state.current = next;
        Ok(())
    }
    /// Whether this path belongs to the UI or public assets. The HTTP
    /// adapter checks its built-ins and registered API routes first.
    pub fn handles(&self, path: &str) -> bool {
        !RESERVED
            .iter()
            .copied()
            .chain(self.policy.reserved.iter().map(String::as_str))
            .any(|p| under(path, p))
    }

    /// Answers a request using authorization metadata decoded by the HTTP adapter.
    pub async fn answer(
        &self,
        pipeline: &Pipeline,
        parts: &http::request::Parts,
        meta: &RequestMeta,
        id: &RequestId,
        response_meta: &mut ResponseMeta,
    ) -> HyveResult<Response<SiteBody>> {
        let (method, uri, headers) = (&parts.method, &parts.uri, &parts.headers);
        let path = uri.path();
        let build = {
            let state = self.mounted.read().unwrap_or_else(|e| e.into_inner());
            if under(path, "/assets") {
                let bytes = state
                    .current
                    .data
                    .assets
                    .get(path)
                    .or_else(|| state.previous_assets.get(path))
                    .ok_or_else(missing)?;
                let name = path.rsplit('/').next().unwrap_or("");
                let etag = format!("\"{}\"", path.split('/').nth(2).unwrap_or(""));
                return Ok(static_page(
                    method,
                    headers,
                    bytes,
                    serge_web_ssr::media_type(name),
                    "public, max-age=31536000, immutable",
                    &etag,
                ));
            }
            state.current.clone()
        };
        let i = build
            .data
            .patterns
            .iter()
            .position(|p| p.matches(path).is_some())
            .ok_or_else(missing)?;
        let route = &build.data.manifest.routes[i];
        if method != Method::GET && method != Method::HEAD {
            return Ok(wrong_method());
        }
        let request = pipeline.adapter_request(id.clone(), route.pattern())?;
        let scope = request.scope();
        let declared = self
            .policy
            .group(route.pattern())
            .expect("policy was verified");
        // Also honor a protected concrete namespace underneath a parameter route.
        let concrete = self.policy.group(path);
        let mut policies = vec![declared];
        if let Some(group) = concrete
            && group.0 != declared.0
        {
            policies.push(group);
        }
        let private = policies.iter().any(|(_, guards)| !guards.is_empty());
        for (prefix, guards) in policies {
            for (n, guard) in guards.iter().enumerate() {
                let key = format!("site:{prefix}:{n}");
                let mut cx = GuardCtx {
                    scope,
                    method: hyve::api::Method::Get,
                    path,
                    route: route.pattern(),
                    meta,
                    response: response_meta,
                    guard_key: &key,
                };
                scope.scoped(guard.check(&mut cx)).await?;
            }
        }
        if let Some(file) = route.file() {
            return Ok(static_page(
                method,
                headers,
                &build.data.pages[file],
                "text/html; charset=utf-8",
                if private {
                    "private, no-store"
                } else {
                    "no-cache"
                },
                &build.page_tags[file],
            ));
        }
        let render = build
            .ssr
            .as_ref()
            .expect("verified SSR handler")
            .render(SsrRequest {
                path: path.into(),
                query: uri.query().unwrap_or("").into(),
                request_id: id.clone(),
                scope: scope.clone(),
            });
        let result = tokio::select! {result=scope.scoped(render)=>result?,_=scope.cancellation().cancelled()=>return Err(cancelled())};
        let mut builder = Response::builder()
            .header(header::CACHE_CONTROL, "private, no-store")
            .header(header::CONTENT_SECURITY_POLICY, CSP);
        match result {
            SsrResponse::Redirect {
                location,
                permanent,
            } => {
                if !location.starts_with('/')
                    || location.starts_with("//")
                    || location.contains('\\')
                    || location.chars().any(char::is_control)
                {
                    return Err(HyveError::internal("invalid SSR redirect"));
                }
                let location = HeaderValue::from_str(&location)
                    .map_err(|_| HyveError::internal("invalid SSR redirect header"))?;
                Ok(builder
                    .status(if permanent { 308 } else { 307 })
                    .header(header::LOCATION, location)
                    .body(full(Bytes::new()))
                    .unwrap())
            }
            SsrResponse::Html { status, body } => {
                if !matches!(status, 200 | 404) {
                    return Err(HyveError::internal("invalid SSR status"));
                }
                builder = builder
                    .status(status)
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8");
                if method == Method::HEAD {
                    return Ok(builder.body(full(Bytes::new())).unwrap());
                }
                let request_id = id.clone();
                let chunks = stream::unfold(
                    Some((body, request, request_id)),
                    |state| async move {
                        let (mut body, request, id) = state?;
                        let result = tokio::select! {v=body.next()=>v?,_=request.scope().cancellation().cancelled()=>Err(cancelled())};
                        match result {
                            Ok(data) => Some((Ok(Frame::data(data)), Some((body, request, id)))),
                            Err(e) => {
                                eprintln!(
                                    "orbic [{}] SSR stream failed: {}",
                                    id.as_str(),
                                    e.code()
                                );
                                Some((Err(e), None))
                            }
                        }
                    },
                );
                Ok(builder
                    .body(StreamBody::new(chunks).boxed_unsync())
                    .unwrap())
            }
        }
    }
}
fn missing() -> HyveError {
    HyveError::domain("route_not_found", "no matching UI route or asset")
}
fn cancelled() -> HyveError {
    HyveError::infra("cancelled", "render was cancelled")
}
fn wrong_method() -> Response<SiteBody> {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(header::ALLOW, "GET, HEAD")
        .body(full(Bytes::new()))
        .unwrap()
}
fn static_page(
    method: &Method,
    headers: &HeaderMap,
    bytes: &Bytes,
    ct: &str,
    cache: &str,
    etag: &str,
) -> Response<SiteBody> {
    if method != Method::GET && method != Method::HEAD {
        return wrong_method();
    }
    let matches = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag);
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, ct)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache)
        .header(header::CONTENT_SECURITY_POLICY, CSP);
    if matches {
        response = response.status(304);
    } else {
        response = response.header(header::CONTENT_LENGTH, bytes.len());
    }
    response
        .body(full(if matches || method == Method::HEAD {
            Bytes::new()
        } else {
            bytes.clone()
        }))
        .unwrap()
}
