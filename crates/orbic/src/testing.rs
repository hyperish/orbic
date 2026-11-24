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
    BoxFuture, Call, HEADER_FN, HEADER_SCHEMA, MEDIA_TYPE_JSON, Method, Rail, Reply,
    ServerFnDescriptor, Transport, errors,
};
use hyve::serde::{Json, from_json_data, to_json};
use hyve::{HyveError, HyveResult};
use orbic_auth::{AuthConfig, Client, SessionUser};
use orbic_cache::Cache;
use orbic_core::{OrbicApp, Pipeline};

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

/// Reads an answer as a transport does (`hyve::api::http::decode`).
fn restore(r: &http::Response<Bytes>) -> HyveResult<Reply> {
    hyve::api::http::decode(hyve::api::http::HttpAnswer {
        status: r.status().as_u16(),
        content_type: r.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        retry_after: r.headers().contains_key(RETRY_AFTER),
        body: r.body(),
    })
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
        let pattern = self.client.path_of(call.descriptor());
        if self.rail == Rail::Fast {
            let r = self
                .send(hyve::api::http::encode(
                    &call,
                    Rail::Fast,
                    &pattern,
                    self.fingerprint,
                )?)
                .await;
            if !hyve::api::http::wants_standard_rail(r.status().as_u16()) {
                return restore(&r);
            }
            // The server cannot take this binary request: once on the
            // standard rail, whatever the code (§3.3).
            self.client.fallbacks.fetch_add(1, Ordering::SeqCst);
        }
        let r = self
            .send(hyve::api::http::encode(
                &call,
                Rail::Standard,
                &pattern,
                None,
            )?)
            .await;
        restore(&r)
    }

    async fn send(&self, req: hyve::api::http::HttpRequest) -> http::Response<Bytes> {
        let mut request = http::Request::new(Bytes::from(req.body));
        *request.method_mut() =
            http::Method::from_bytes(req.method.as_str().as_bytes()).expect("a valid method");
        *request.uri_mut() = req.path.parse().expect("encoded paths are valid URIs");
        for (name, value) in req.headers {
            request.headers_mut().insert(
                HeaderName::from_static(lower(name)),
                HeaderValue::from_str(&value).expect("header-safe values"),
            );
        }
        self.authenticate(request.headers_mut());
        self.client.send_http(request).await.into_inner()
    }

    fn authenticate(&self, h: &mut HeaderMap) {
        if let Some(token) = &self.client.bearer {
            h.insert(AUTHORIZATION, bearer(token));
        }
    }
}

/// `http::HeaderName::from_static` takes lowercase names.
fn lower(name: &'static str) -> &'static str {
    match name {
        "Content-Type" => "content-type",
        n if n == HEADER_FN => "hyperish-fn",
        n if n == HEADER_SCHEMA => "hyperish-schema",
        n => n,
    }
}
