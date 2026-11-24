//! `TestClient`: the app without a socket, through the same HTTP
//! translation as the server (`docs/03-adapters-and-pipeline.md` §1,
//! `TestClient`의 위치).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER};
use http::{HeaderMap, StatusCode};
use hyve::api::{
    BoxFuture, Call, ErrorBody, ErrorWire, HEADER_FN, HEADER_SCHEMA, MEDIA_TYPE_BINARY,
    MEDIA_TYPE_JSON, Method, Rail, Reply, ServerFnDescriptor, Transport, errors,
};
use hyve::serde::{AlignedBytes, Binary as _, Json, from_json, from_json_data, to_json};
use hyve::{ErrorKind, HyveError, HyveResult, Value};
use orbic_auth::{AuthConfig, Client, SessionUser};
use orbic_cache::Cache;
use orbic_core::{OrbicApp, Pipeline, arg_json_name};

use crate::{HttpApp, InProcess, http_app};

/// The peer address requests appear to come from.
const PEER: &str = "127.0.0.1:1";

/// An app behind the HTTP adapter without a socket: requests pass status
/// codes, headers, the envelope, CORS, security headers and size limits
/// exactly as on a real server. It is also a `hyve::api::TestClient`, so
/// `#[server]` stubs can be called on either rail through that translation.
#[derive(Clone)]
pub struct TestClient {
    http: Arc<HttpApp>,
    /// A session every request carries (`TestClient::as_user`).
    bearer: Option<String>,
    fallbacks: Arc<AtomicUsize>,
}

impl std::fmt::Debug for TestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestClient").finish_non_exhaustive()
    }
}

impl TestClient {
    /// Assembles `app` (panicking with the assembly problems) and puts it
    /// behind the HTTP adapter, plain HTTP allowed as on loopback.
    pub fn new(app: OrbicApp) -> Self {
        match http_app(app) {
            Ok(http) => Self::from_http(http),
            Err(e) => panic!("{e}"),
        }
    }

    /// A client for an already built HTTP app.
    pub fn from_http(http: HttpApp) -> Self {
        TestClient {
            http: Arc::new(http.allow_plaintext(true)),
            bearer: None,
            fallbacks: Arc::default(),
        }
    }

    /// The pipeline.
    pub fn pipeline(&self) -> &Pipeline {
        self.http.pipeline()
    }

    /// The HTTP app.
    pub fn http(&self) -> &HttpApp {
        &self.http
    }

    /// A client whose requests and `#[server]` calls carry a fresh, real
    /// session of `user` (Bearer), so guards authenticate it like any
    /// client.
    pub fn as_user<U: SessionUser>(&self, user: U) -> TestClient {
        TestClient {
            bearer: Some(self.session(&user)),
            ..self.clone()
        }
    }

    /// The in-process transport into this app, with this client's session.
    pub fn in_process(&self) -> InProcess {
        let t = InProcess::new(self.pipeline().clone());
        match &self.bearer {
            Some(token) => t.with_credential(orbic_core::Credential::Bearer(token.clone())),
            None => t,
        }
    }

    /// How many fast-rail calls fell back to the standard rail (`415`).
    pub fn fallbacks(&self) -> usize {
        self.fallbacks.load(Ordering::SeqCst)
    }

    fn session<U: SessionUser>(&self, user: &U) -> String {
        let pipeline = self.pipeline();
        let cache = pipeline.provided::<Cache>().expect(
            "TestClient::as_user creates a session in the app's `Cache`; provide one at the root",
        );
        let config = pipeline
            .provided::<AuthConfig>()
            .map(|c| *c)
            .unwrap_or_default();
        orbic_auth::issue(&cache, &config, user, Client::Bearer)
            .expect("the session could not be created")
            .session
    }

    /// A request.
    pub fn request(&self, method: Method, path: &str) -> TestRequest<'_> {
        let mut headers = HeaderMap::new();
        if let Some(token) = &self.bearer {
            headers.insert(AUTHORIZATION, bearer(token));
        }
        TestRequest {
            client: self,
            method,
            path: path.to_owned(),
            headers,
            body: Bytes::new(),
        }
    }

    /// A `GET` request.
    pub fn get(&self, path: &str) -> TestRequest<'_> {
        self.request(Method::Get, path)
    }

    /// A `POST` request.
    pub fn post(&self, path: &str) -> TestRequest<'_> {
        self.request(Method::Post, path)
    }

    /// A `PATCH` request.
    pub fn patch(&self, path: &str) -> TestRequest<'_> {
        self.request(Method::Patch, path)
    }

    /// A `DELETE` request.
    pub fn delete(&self, path: &str) -> TestRequest<'_> {
        self.request(Method::Delete, path)
    }

    /// Sends a raw HTTP request.
    pub async fn send_http(&self, request: http::Request<Bytes>) -> TestResponse {
        let peer: SocketAddr = PEER.parse().expect("a valid address");
        TestResponse(self.http.handle(request, peer).await)
    }

    /// The full path of a `#[server]` function: its registered route, or
    /// the mount prefix of the app's other `#[server]` functions (Phase 1
    /// keeps them under one prefix) before its bound path.
    fn path_of(&self, d: &ServerFnDescriptor) -> String {
        let mut prefix = None;
        for r in self.pipeline().routes() {
            if let Some(f) = r.server_fn {
                if f.id == d.id {
                    return r.pattern.to_owned();
                }
                prefix = r.pattern.strip_suffix(f.path).map(str::to_owned);
            }
        }
        format!("{}{}", prefix.unwrap_or_default(), d.path)
    }
}

impl hyve::api::TestClient for TestClient {
    fn transport(&self, rail: Rail) -> Arc<dyn Transport> {
        Arc::new(HttpTransport {
            client: self.clone(),
            rail,
            fingerprint: None,
        })
    }
}

impl TestClient {
    /// A transport whose fast-rail requests claim `fingerprint`, to exercise
    /// the schema-mismatch fallback.
    #[doc(hidden)]
    pub fn transport_with_fingerprint(&self, fingerprint: u64) -> Arc<dyn Transport> {
        Arc::new(HttpTransport {
            client: self.clone(),
            rail: Rail::Fast,
            fingerprint: Some(fingerprint),
        })
    }
}

fn bearer(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).expect("tokens are header-safe")
}

/// A request being built.
#[derive(Debug)]
pub struct TestRequest<'c> {
    client: &'c TestClient,
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl TestRequest<'_> {
    /// Adds a header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("a valid header name");
        let value = HeaderValue::from_str(value).expect("a valid header value");
        self.headers.append(name, value);
        self
    }

    /// A JSON body.
    pub fn json<T: Json>(mut self, value: &T) -> Self {
        self.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE_JSON));
        self.body = Bytes::from(to_json(value));
        self
    }

    /// A raw body with its content type.
    pub fn body(mut self, content_type: &str, body: impl Into<Bytes>) -> Self {
        self.headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(content_type).expect("a valid content type"),
        );
        self.body = body.into();
        self
    }

    /// Authenticates as `user` with a fresh, real session (Bearer): the
    /// guards run exactly as for any client.
    pub fn as_user<U: SessionUser>(mut self, user: U) -> Self {
        let token = self.client.session(&user);
        self.headers.insert(AUTHORIZATION, bearer(&token));
        self
    }

    /// `Idempotency-Key`.
    pub fn idempotency_key(self, key: &str) -> Self {
        self.header("idempotency-key", key)
    }

    /// Sends the request.
    pub async fn send(self) -> TestResponse {
        let mut request = http::Request::new(self.body);
        *request.method_mut() =
            http::Method::from_bytes(self.method.as_str().as_bytes()).expect("a valid method");
        *request.uri_mut() = self.path.parse().expect("a valid path");
        *request.headers_mut() = self.headers;
        self.client.send_http(request).await
    }
}

/// An answer.
#[derive(Debug)]
pub struct TestResponse(http::Response<Bytes>);

impl TestResponse {
    /// The status code (`assert_eq!(res.status(), 400)` works).
    pub fn status(&self) -> StatusCode {
        self.0.status()
    }

    /// The headers.
    pub fn headers(&self) -> &HeaderMap {
        self.0.headers()
    }

    /// One header as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.0.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// The body.
    pub fn bytes(&self) -> &[u8] {
        self.0.body()
    }

    /// The body as text.
    pub fn text(&self) -> &str {
        std::str::from_utf8(self.0.body()).unwrap_or("<binary>")
    }

    /// The success value (`{"data": ...}` read back); panics otherwise.
    pub fn json<T: Json>(&self) -> T {
        from_json_data(self.text()).unwrap_or_else(|e| {
            panic!(
                "not a {} answer: {e}: {} {}",
                std::any::type_name::<T>(),
                self.status(),
                self.text()
            )
        })
    }

    /// The error, restored as a transport would; panics on success.
    pub fn error(&self) -> HyveError {
        restore(&self.0).expect_err("expected an error answer")
    }

    /// The HTTP response.
    pub fn into_inner(self) -> http::Response<Bytes> {
        self.0
    }
}

/// Reads an answer as a transport does (`hyve/docs/03-adapters-and-pipeline.md`
/// §3.3, §3.4).
fn restore(r: &http::Response<Bytes>) -> HyveResult<Reply> {
    let binary = r
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes() == MEDIA_TYPE_BINARY.as_bytes());
    let status = r.status();
    if binary {
        let bytes = AlignedBytes::copy_from(r.body());
        if status.is_success() {
            return Ok(Reply::Binary(bytes));
        }
        let wire = ErrorWire::from_binary(&bytes)
            .map_err(|e| errors::decode_failed(format!("unreadable binary error: {e}")))?;
        return Err(wire.into());
    }
    let text = std::str::from_utf8(r.body())
        .map_err(|_| errors::decode_failed("the answer is not UTF-8"))?;
    if status == StatusCode::NO_CONTENT {
        return Ok(Reply::NoContent);
    }
    if status.is_success() {
        return Ok(Reply::Json(text.to_owned()));
    }
    let kind = if status.is_client_error() {
        ErrorKind::Domain
    } else if status == StatusCode::SERVICE_UNAVAILABLE {
        ErrorKind::Infra
    } else {
        ErrorKind::Internal
    };
    let retryable = r.headers().contains_key(RETRY_AFTER);
    let envelope: Value = from_json(text)
        .map_err(|e| errors::decode_failed(format!("not an error envelope ({status}): {e}")))?;
    let body = envelope
        .get("error")
        .map(|e| from_json::<ErrorBody>(&hyve::serde::to_json(e)))
        .ok_or_else(|| errors::decode_failed(format!("not an error envelope ({status})")))?
        .map_err(|e| errors::decode_failed(format!("not an error envelope ({status}): {e}")))?;
    Err(body.into_error(kind, retryable))
}

/// `#[server]` calls over the HTTP translation, on one rail.
struct HttpTransport {
    client: TestClient,
    rail: Rail,
    fingerprint: Option<u64>,
}

impl Transport for HttpTransport {
    fn call(&self, call: Call) -> BoxFuture<'_, HyveResult<Reply>> {
        let cancellation = call.cancellation().clone();
        Box::pin(async move {
            tokio::select! {
                r = self.run(call) => r,
                () = cancellation.cancelled() => Err(errors::cancelled("the caller cancelled the call")),
            }
        })
    }
}

impl HttpTransport {
    async fn run(&self, call: Call) -> HyveResult<Reply> {
        let d = call.descriptor();
        let pattern = self.client.path_of(d);
        let model: Value = from_json(&call.encode_json())
            .map_err(|e| HyveError::internal(format!("the request model is not JSON: {e}")))?;
        let params = Params::new(d, model);
        if self.rail == Rail::Fast {
            let body = call.encode_binary()?;
            let fingerprint = self.fingerprint.unwrap_or(d.fingerprint);
            let mut request = http::Request::new(Bytes::from(body.into_vec()));
            *request.method_mut() = http::Method::POST;
            *request.uri_mut() = params.path(&pattern, false)?;
            let h = request.headers_mut();
            h.insert(CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE_BINARY));
            h.insert(
                HEADER_FN,
                HeaderValue::from_str(d.id).expect("ids are header-safe"),
            );
            h.insert(
                HEADER_SCHEMA,
                HeaderValue::from_str(&format!("{fingerprint:016x}")).expect("hex"),
            );
            self.authenticate(h);
            let r = self.client.send_http(request).await.into_inner();
            if r.status() != StatusCode::UNSUPPORTED_MEDIA_TYPE {
                return restore(&r);
            }
            // The server cannot take this binary request: once on the
            // standard rail, whatever the code (§3.3).
            self.client.fallbacks.fetch_add(1, Ordering::SeqCst);
        }
        let bodyless = matches!(d.method, Method::Get | Method::Delete);
        let mut request = http::Request::new(if bodyless || d.args.is_empty() {
            Bytes::new()
        } else {
            Bytes::from(call.encode_json())
        });
        *request.method_mut() =
            http::Method::from_bytes(d.method.as_str().as_bytes()).expect("a valid method");
        *request.uri_mut() = params.path(&pattern, bodyless)?;
        if !request.body().is_empty() {
            request
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE_JSON));
        }
        self.authenticate(request.headers_mut());
        restore(&self.client.send_http(request).await.into_inner())
    }

    fn authenticate(&self, h: &mut HeaderMap) {
        if let Some(token) = &self.client.bearer {
            h.insert(AUTHORIZATION, bearer(token));
        }
    }
}

/// The request model as named values, for path placeholders and (for
/// `GET`/`DELETE`) the query string (`docs/01-dx-and-dsl.md` §2).
struct Params(Vec<(String, Value)>);

impl Params {
    fn new(d: &ServerFnDescriptor, model: Value) -> Params {
        match model {
            _ if d.args.is_empty() => Params(Vec::new()),
            Value::Map(fields) => Params(fields),
            single => Params(vec![(arg_json_name(d.args[0]), single)]),
        }
    }

    fn path(&self, pattern: &str, with_query: bool) -> HyveResult<http::Uri> {
        let mut path = String::new();
        let mut used = Vec::new();
        let mut rest = pattern;
        while let Some(start) = rest.find('{') {
            path.push_str(&rest[..start]);
            let end = rest[start..].find('}').map_or(rest.len(), |e| start + e);
            let name = rest[start + 1..end].trim_start_matches('*');
            let value = self.0.iter().find(|(k, _)| k == name).ok_or_else(|| {
                HyveError::invalid_request(format!("the call has no `{name}` for the path"))
            })?;
            path.push_str(&encode(&scalar(&value.1)?));
            used.push(name.to_owned());
            rest = &rest[(end + 1).min(rest.len())..];
        }
        path.push_str(rest);
        if with_query {
            let mut query = Vec::new();
            for (k, v) in self.0.iter().filter(|(k, _)| !used.contains(k)) {
                match v {
                    Value::Null => {}
                    Value::List(items) => {
                        for item in items {
                            query.push(format!("{}={}", encode(k), encode(&scalar(item)?)));
                        }
                    }
                    v => query.push(format!("{}={}", encode(k), encode(&scalar(v)?))),
                }
            }
            if !query.is_empty() {
                path.push('?');
                path.push_str(&query.join("&"));
            }
        }
        path.parse()
            .map_err(|e| HyveError::internal(format!("bad request URI `{path}`: {e}")))
    }
}

fn scalar(v: &Value) -> HyveResult<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(_) | Value::Int(_) | Value::Float(_) => Ok(to_json(v)),
        other => Err(HyveError::invalid_request(format!(
            "{other:?} cannot travel in a path or query string"
        ))),
    }
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
