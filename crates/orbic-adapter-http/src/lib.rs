//! `orbic-adapter-http` — HTTP/1.1 in front of the transport-neutral
//! pipeline (`docs/03-adapters-and-pipeline.md` §1).
//!
//! [`HttpApp::handle`] is the whole translation (HTTP request in, HTTP
//! response out, no socket): rails, credentials, idempotency and rate-limit
//! headers, status codes, the error envelope, CORS, security headers, size
//! limits and the built-in `/health` and `/metrics`. [`HttpApp::serve`] only
//! adds the socket, so `TestClient` (which calls `handle`) and a real server
//! answer the same way.
//!
//! Header contract (`hyperish/docs` `03-api-protocol.md`):
//! - in: `Content-Type` (`application/json`, or the fast rail's
//!   `application/vnd.hyperish.rkyv` with `Hyperish-Fn` and
//!   `Hyperish-Schema`), `Authorization: Bearer`, the session cookie,
//!   `X-CSRF-Token`, `Idempotency-Key`, `X-Request-Id`, and from trusted
//!   proxies `X-Forwarded-For` / `X-Forwarded-Proto`;
//! - out: `X-Request-Id`, `Retry-After` (exactly when the error is
//!   retryable), `RateLimit-*`, `Allow` (405), `Set-Cookie`, security
//!   headers, CORS for allowed origins.

mod config;
mod metrics;
mod server;

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method as HttpMethod, StatusCode};
use hyve::api::{
    ErrorBody, ErrorWire, HEADER_FN, HEADER_SCHEMA, MEDIA_TYPE_BINARY, MEDIA_TYPE_JSON, Method,
    codes,
};
use hyve::serde::{Binary as _, to_json};
use hyve::{ErrorKind, HyveError, RequestId, Value};
use orbic_core::{
    Body, Credential, CredentialUpdate, Payload, Pipeline, Rail, Request, RequestMeta, Response,
};

use crate::config::Cidr;
pub use crate::config::{DEFAULT_MAX_BODY, DEFAULT_SESSION_COOKIE, HttpConfig};

/// The correlation ID header, in both directions. An unusable incoming value
/// is replaced by a fresh ID.
pub const HEADER_REQUEST_ID: &str = "x-request-id";

/// The CSRF token header of cookie sessions.
pub const HEADER_CSRF: &str = "x-csrf-token";

/// Paths the adapter answers itself; app routes there fail [`HttpApp::new`].
pub const RESERVED_PATHS: [&str; 2] = ["/health", "/metrics"];

/// `Retry-After` of retryable errors that did not set their own.
pub const DEFAULT_RETRY_AFTER_SECS: u64 = 1;

const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const RATE_LIMIT_LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
const RATE_LIMIT_REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
const RATE_LIMIT_RESET: HeaderName = HeaderName::from_static("ratelimit-reset");
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

const SECURITY_HEADERS: [(HeaderName, &str); 5] = [
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; frame-ancestors 'none'",
    ),
    (header::STRICT_TRANSPORT_SECURITY, "max-age=31536000"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::CACHE_CONTROL, "no-store"),
];

const CORS_ALLOW_METHODS: &str = "GET, POST, PATCH, DELETE";
const CORS_ALLOW_HEADERS: &str = "authorization, content-type, hyperish-fn, hyperish-schema, idempotency-key, x-csrf-token, x-request-id";
const CORS_EXPOSE_HEADERS: &str =
    "retry-after, ratelimit-limit, ratelimit-remaining, ratelimit-reset, x-request-id";
const CORS_MAX_AGE: &str = "600";

/// Why an [`HttpApp`] could not be made (bad [`HttpConfig`], an app route
/// on a reserved path).
#[derive(Debug)]
pub struct HttpError {
    problems: Vec<String>,
}

impl HttpError {
    /// Every problem found.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the HTTP app is invalid:")?;
        for p in &self.problems {
            write!(f, "\n  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

struct Settings {
    max_body: usize,
    cors_origins: Vec<HeaderValue>,
    trusted: Vec<Cidr>,
    metrics_from: Vec<Cidr>,
    cookie: String,
    shutdown_grace: Duration,
}

/// A pipeline behind HTTP.
pub struct HttpApp {
    pipeline: Pipeline,
    settings: Settings,
    documents: Vec<(String, &'static str, Bytes)>,
    metrics: metrics::Metrics,
    plaintext: bool,
}

impl fmt::Debug for HttpApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpApp")
            .field("plaintext", &self.plaintext)
            .finish_non_exhaustive()
    }
}

impl HttpApp {
    /// Puts `pipeline` behind HTTP with the [`HttpConfig`] provided at its
    /// root (or the default). Plain HTTP is refused until
    /// [`allow_plaintext`](Self::allow_plaintext) (servers bound to loopback
    /// and `TestClient` allow it).
    pub fn new(pipeline: Pipeline) -> Result<Self, HttpError> {
        let config = pipeline
            .provided::<HttpConfig>()
            .map(|c| (*c).clone())
            .unwrap_or_default();
        let mut problems = Vec::new();
        let mut cidrs = |list: &[String], what: &str| -> Vec<Cidr> {
            list.iter()
                .filter_map(|c| {
                    Cidr::parse(c)
                        .map_err(|e| problems.push(format!("{what}: {e}")))
                        .ok()
                })
                .collect()
        };
        let trusted = cidrs(&config.trusted_proxies, "trusted_proxy");
        let metrics_from = cidrs(&config.metrics_from, "metrics_from");
        let mut cors_origins = Vec::new();
        for origin in &config.cors_origins {
            let exact = origin.contains("://")
                && !origin.contains('*')
                && !origin.ends_with('/')
                && origin.is_ascii();
            match HeaderValue::from_str(origin) {
                Ok(v) if exact => cors_origins.push(v),
                _ => problems.push(format!(
                    "cors_origin: `{origin}` is not an exact origin (`https://app.example`); `*` is never allowed"
                )),
            }
        }
        if HeaderName::from_bytes(config.session_cookie.as_bytes()).is_err()
            || config.session_cookie.contains(['=', ';'])
        {
            problems.push(format!(
                "session_cookie: `{}` is not a cookie name",
                config.session_cookie
            ));
        }
        for route in pipeline.routes() {
            if RESERVED_PATHS.contains(&route.pattern) {
                problems.push(format!(
                    "`{}` is answered by the HTTP adapter; move the route `{}`",
                    route.pattern, route.name
                ));
            }
        }
        if !problems.is_empty() {
            return Err(HttpError { problems });
        }
        Ok(HttpApp {
            pipeline,
            settings: Settings {
                max_body: config.max_body,
                cors_origins,
                trusted,
                metrics_from,
                cookie: config.session_cookie,
                shutdown_grace: config.shutdown_grace,
            },
            documents: Vec::new(),
            metrics: metrics::Metrics::default(),
            plaintext: false,
        })
    }

    /// Serves `body` at `GET path`, outside the pipeline (the OpenAPI
    /// document).
    pub fn document(
        mut self,
        path: &str,
        content_type: &'static str,
        body: impl Into<Bytes>,
    ) -> Result<Self, HttpError> {
        let taken = RESERVED_PATHS.contains(&path)
            || self.documents.iter().any(|d| d.0 == path)
            || self.pipeline.routes().any(|r| r.pattern == path);
        if taken || !path.starts_with('/') {
            return Err(HttpError {
                problems: vec![format!("`{path}` is taken or not a path")],
            });
        }
        self.documents
            .push((path.to_owned(), content_type, body.into()));
        Ok(self)
    }

    /// Accepts plain HTTP from any peer. Otherwise only requests a trusted
    /// proxy forwarded with `X-Forwarded-Proto: https` reach the API.
    pub fn allow_plaintext(mut self, allow: bool) -> Self {
        self.plaintext = allow;
        self
    }

    /// The pipeline.
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// The session cookie's name.
    pub fn session_cookie(&self) -> &str {
        &self.settings.cookie
    }

    /// Answers one request from `peer`.
    pub async fn handle(
        &self,
        request: http::Request<Bytes>,
        peer: SocketAddr,
    ) -> http::Response<Bytes> {
        let (parts, body) = request.into_parts();
        self.answer(&parts, Ok(body), peer).await
    }

    pub(crate) async fn answer(
        &self,
        parts: &http::request::Parts,
        body: Result<Bytes, HyveError>,
        peer: SocketAddr,
    ) -> http::Response<Bytes> {
        let headers = &parts.headers;
        let id = header_str(headers, HEADER_REQUEST_ID)
            .and_then(|s| RequestId::parse(s).ok())
            .unwrap_or_default();
        let path = parts.uri.path();
        if parts.method == HttpMethod::GET
            && let Some(r) = self.builtin(path, headers, peer, &id)
        {
            return finish(r, &id, None);
        }
        let timer = self.metrics.start();
        let origin = self.allowed_origin(headers);
        let response = finish(self.api(parts, body, peer, &id).await, &id, origin);
        timer.done(response.status());
        response
    }

    fn builtin(
        &self,
        path: &str,
        headers: &HeaderMap,
        peer: SocketAddr,
        id: &RequestId,
    ) -> Option<http::Response<Bytes>> {
        match path {
            "/health" if self.pipeline.is_accepting() => {
                Some(text(StatusCode::OK, "text/plain; charset=utf-8", "ok"))
            }
            "/health" => Some(text(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain; charset=utf-8",
                "shutting down",
            )),
            "/metrics" => {
                let direct = !headers.contains_key(&X_FORWARDED_FOR)
                    && !headers.contains_key(header::FORWARDED);
                let peer = peer.ip().to_canonical();
                if direct && self.settings.metrics_from.iter().any(|c| c.contains(peer)) {
                    Some(text(
                        StatusCode::OK,
                        metrics::CONTENT_TYPE,
                        self.metrics.render(),
                    ))
                } else {
                    Some(error(
                        framework("route_not_found", "no route for /metrics".into()),
                        id,
                    ))
                }
            }
            _ => self
                .documents
                .iter()
                .find(|d| d.0 == path)
                .map(|(_, ct, body)| text(StatusCode::OK, ct, body.clone())),
        }
    }

    async fn api(
        &self,
        parts: &http::request::Parts,
        body: Result<Bytes, HyveError>,
        peer: SocketAddr,
        id: &RequestId,
    ) -> http::Response<Bytes> {
        let headers = &parts.headers;
        let peer = peer.ip().to_canonical();
        let via_proxy = self.settings.trusted.iter().any(|c| c.contains(peer));
        let https = via_proxy
            && last_value(headers, &X_FORWARDED_PROTO)
                .is_some_and(|p| p.eq_ignore_ascii_case("https"));
        if !self.plaintext && !https {
            return error(
                HyveError::invalid_request(
                    "HTTPS is required: terminate TLS at a trusted proxy that sets X-Forwarded-Proto",
                ),
                id,
            );
        }
        if parts.method == HttpMethod::OPTIONS
            && headers.contains_key(header::ORIGIN)
            && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
        {
            return self.preflight(headers, id);
        }
        let Some(method) = api_method(&parts.method) else {
            return error(
                framework(
                    "method_not_allowed",
                    format!(
                        "{} is not an API method; use GET, POST, PATCH or DELETE",
                        parts.method
                    ),
                ),
                id,
            );
        };
        let body = match body {
            Ok(b) if b.len() <= self.settings.max_body => b,
            Ok(_) => return error(too_large(self.settings.max_body), id),
            Err(e) => return error(e, id),
        };
        let (rail, server_fn) = match rail(headers, body.is_empty()) {
            Ok(r) => r,
            Err(e) => return error(e, id),
        };
        let client_addr = if via_proxy {
            forwarded_for(headers, &self.settings.trusted).unwrap_or(peer)
        } else {
            peer
        };
        let request = Request {
            method,
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().unwrap_or_default().to_owned(),
            rail,
            body: if body.is_empty() {
                Body::Empty
            } else {
                Body::Bytes(Vec::from(body))
            },
            meta: RequestMeta {
                request_id: Some(id.clone()),
                idempotency_key: headers
                    .get(&IDEMPOTENCY_KEY)
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()),
                credential: self.credential(headers),
                client_addr: Some(client_addr),
                server_fn,
            },
        };
        let response = self.pipeline.dispatch(request).await;
        self.respond(response)
    }

    fn preflight(&self, headers: &HeaderMap, id: &RequestId) -> http::Response<Bytes> {
        if self.allowed_origin(headers).is_none() {
            return error(
                framework("forbidden", "this origin may not call the API".into()),
                id,
            );
        }
        let mut r = empty(StatusCode::NO_CONTENT);
        let h = r.headers_mut();
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(CORS_ALLOW_METHODS),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(CORS_ALLOW_HEADERS),
        );
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static(CORS_MAX_AGE),
        );
        r
    }

    fn allowed_origin(&self, headers: &HeaderMap) -> Option<HeaderValue> {
        let origin = headers.get(header::ORIGIN)?;
        self.settings
            .cors_origins
            .iter()
            .find(|o| *o == origin)
            .cloned()
    }

    /// `Authorization: Bearer` wins over the cookie: it is sent on purpose,
    /// the cookie travels on its own.
    fn credential(&self, headers: &HeaderMap) -> Option<Credential> {
        if let Some(auth) = header_str(headers, header::AUTHORIZATION.as_str())
            && let Some((scheme, token)) = auth.split_once(' ')
            && scheme.eq_ignore_ascii_case("bearer")
            && !token.trim().is_empty()
        {
            return Some(Credential::Bearer(token.trim().to_owned()));
        }
        let token = cookie(headers, &self.settings.cookie)?;
        Some(Credential::Session {
            token,
            csrf: header_str(headers, HEADER_CSRF).map(str::to_owned),
        })
    }

    fn respond(&self, response: Response) -> http::Response<Bytes> {
        let fast = response.rail == Rail::Fast;
        let mut out = match response.outcome {
            Ok(Payload::Json(s)) => text(StatusCode::OK, MEDIA_TYPE_JSON, s),
            Ok(Payload::Empty) => empty(StatusCode::NO_CONTENT),
            Ok(Payload::Binary(b)) => text(StatusCode::OK, MEDIA_TYPE_BINARY, b.into_vec()),
            Ok(Payload::Value(_)) => error(
                HyveError::internal("an in-process payload reached HTTP"),
                &response.request_id,
            ),
            Err(e) => error_body(&e, fast, response.meta.retry_after),
        };
        let h = out.headers_mut();
        if let Some(rl) = response.meta.rate_limit {
            h.insert(RATE_LIMIT_LIMIT, rl.limit.into());
            h.insert(RATE_LIMIT_REMAINING, rl.remaining.into());
            h.insert(RATE_LIMIT_RESET, rl.reset_secs.into());
        }
        let name = &self.settings.cookie;
        let cookie = match response.meta.credential_update {
            Some(CredentialUpdate::SetSession { token, .. }) => Some(format!(
                "{name}={token}; Path=/; HttpOnly; Secure; SameSite=Lax"
            )),
            Some(CredentialUpdate::ClearSession) => Some(format!(
                "{name}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax"
            )),
            None => None,
        };
        if let Some(c) = cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
            h.append(header::SET_COOKIE, c);
        }
        out
    }
}

/// The HTTP status of a public error (`docs/03-adapters-and-pipeline.md`
/// §1, `hyperish/docs` `03-api-protocol.md` §4).
pub fn status_of(e: &HyveError) -> StatusCode {
    match e.kind() {
        ErrorKind::Infra => StatusCode::SERVICE_UNAVAILABLE,
        ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Domain => match e.code() {
            "invalid_request" => StatusCode::BAD_REQUEST,
            "unauthenticated" => StatusCode::UNAUTHORIZED,
            "forbidden" | "csrf_token_invalid" => StatusCode::FORBIDDEN,
            "method_not_allowed" => StatusCode::METHOD_NOT_ALLOWED,
            "version_conflict" | "idempotency_key_mismatch" | "request_in_progress" => {
                StatusCode::CONFLICT
            }
            "payload_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            codes::UNSUPPORTED_MEDIA_TYPE | codes::SCHEMA_MISMATCH => {
                StatusCode::UNSUPPORTED_MEDIA_TYPE
            }
            "rate_limited" => StatusCode::TOO_MANY_REQUESTS,
            c if c.ends_with("_not_found") => StatusCode::NOT_FOUND,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        },
    }
}

fn framework(code: &'static str, message: String) -> HyveError {
    HyveError::domain(code, message)
}

fn too_large(max: usize) -> HyveError {
    framework(
        "payload_too_large",
        format!("the request body is larger than {max} bytes"),
    )
}

/// An error the adapter itself answers (before the pipeline): public form,
/// JSON envelope.
fn error(e: HyveError, id: &RequestId) -> http::Response<Bytes> {
    error_body(&e.to_public().with_request_id(id.clone()), false, None)
}

/// A public error on the request's rail. Fast-rail requests get the archived
/// error, except `415`, which tells the client to fall back and so is the
/// JSON envelope (`hyve/docs/03-adapters-and-pipeline.md` §3.3).
fn error_body(e: &HyveError, fast: bool, retry_after: Option<u64>) -> http::Response<Bytes> {
    let status = status_of(e);
    let archived = (fast && status != StatusCode::UNSUPPORTED_MEDIA_TYPE)
        .then(|| ErrorWire::from(e).to_binary().ok())
        .flatten();
    let mut out = match archived {
        Some(b) => text(status, MEDIA_TYPE_BINARY, b.into_vec()),
        None => text(
            status,
            MEDIA_TYPE_JSON,
            format!("{{\"error\":{}}}", to_json(&ErrorBody::from_error(e))),
        ),
    };
    let h = out.headers_mut();
    if e.is_retryable() {
        h.insert(
            header::RETRY_AFTER,
            retry_after.unwrap_or(DEFAULT_RETRY_AFTER_SECS).into(),
        );
    }
    if status == StatusCode::METHOD_NOT_ALLOWED
        && let Some(Value::String(allowed)) = e.details().get("allowed")
        && let Ok(v) = HeaderValue::from_str(allowed)
    {
        h.insert(header::ALLOW, v);
    }
    out
}

/// Headers every answer carries.
fn finish(
    mut r: http::Response<Bytes>,
    id: &RequestId,
    origin: Option<HeaderValue>,
) -> http::Response<Bytes> {
    let h = r.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        h.insert(name, HeaderValue::from_static(value));
    }
    if let Ok(v) = HeaderValue::from_str(id.as_str()) {
        h.insert(HeaderName::from_static(HEADER_REQUEST_ID), v);
    }
    h.append(header::VARY, HeaderValue::from_static("origin"));
    if let Some(origin) = origin {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        h.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(CORS_EXPOSE_HEADERS),
        );
    }
    r
}

fn text(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> http::Response<Bytes> {
    let mut r = http::Response::new(body.into());
    *r.status_mut() = status;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

fn empty(status: StatusCode) -> http::Response<Bytes> {
    let mut r = http::Response::new(Bytes::new());
    *r.status_mut() = status;
    r
}

fn api_method(m: &HttpMethod) -> Option<Method> {
    match *m {
        HttpMethod::GET => Some(Method::Get),
        HttpMethod::POST => Some(Method::Post),
        HttpMethod::PATCH => Some(Method::Patch),
        HttpMethod::DELETE => Some(Method::Delete),
        _ => None,
    }
}

type RailChoice = (Rail, Option<(String, u64)>);

fn rail(headers: &HeaderMap, empty_body: bool) -> Result<RailChoice, HyveError> {
    let media = headers.get(header::CONTENT_TYPE).map(|v| {
        String::from_utf8_lossy(v.as_bytes())
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    });
    match media.as_deref() {
        Some(MEDIA_TYPE_BINARY) => {
            let id = header_str(headers, HEADER_FN).filter(|s| !s.is_empty());
            let fingerprint = header_str(headers, HEADER_SCHEMA)
                .filter(|s| (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|s| u64::from_str_radix(s, 16).ok());
            match (id, fingerprint) {
                (Some(id), Some(fp)) => Ok((Rail::Fast, Some((id.to_owned(), fp)))),
                _ => Err(framework(
                    codes::UNSUPPORTED_MEDIA_TYPE,
                    format!("binary requests need `{HEADER_FN}` and a hex `{HEADER_SCHEMA}`"),
                )),
            }
        }
        Some(MEDIA_TYPE_JSON) => Ok((Rail::Standard, None)),
        _ if empty_body => Ok((Rail::Standard, None)),
        _ => Err(framework(
            codes::UNSUPPORTED_MEDIA_TYPE,
            format!("send `{MEDIA_TYPE_JSON}` bodies"),
        )),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The last comma-separated value over all instances of a header: the one
/// the nearest proxy wrote.
fn last_value<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .next_back()
}

/// The client behind trusted proxies: the rightmost `X-Forwarded-For` entry
/// that is not a trusted proxy (entries to its left are the client's word).
fn forwarded_for(headers: &HeaderMap, trusted: &[Cidr]) -> Option<IpAddr> {
    let entries: Vec<&str> = headers
        .get_all(&X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    let mut leftmost = None;
    for entry in entries.iter().rev() {
        let ip = entry.parse::<IpAddr>().ok()?.to_canonical();
        if !trusted.iter().any(|c| c.contains(ip)) {
            return Some(ip);
        }
        leftmost = Some(ip);
    }
    leftmost
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, v)| *n == name && !v.is_empty())
        .map(|(_, v)| v.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_the_protocol_table() {
        let d = |c: &'static str| status_of(&HyveError::domain(c, "m")).as_u16();
        let table = [
            ("invalid_request", 400),
            ("unauthenticated", 401),
            ("forbidden", 403),
            ("csrf_token_invalid", 403),
            ("route_not_found", 404),
            ("project_not_found", 404),
            ("method_not_allowed", 405),
            ("version_conflict", 409),
            ("idempotency_key_mismatch", 409),
            ("request_in_progress", 409),
            ("payload_too_large", 413),
            ("unsupported_media_type", 415),
            ("schema_mismatch", 415),
            ("title_taken", 422),
            ("rate_limited", 429),
        ];
        for (code, status) in table {
            assert_eq!(d(code), status, "{code}");
        }
        assert_eq!(status_of(&HyveError::internal("x")).as_u16(), 500);
        assert_eq!(status_of(&HyveError::infra("db_down", "x")).as_u16(), 503);
    }

    #[test]
    fn forwarded_for_takes_the_rightmost_untrusted_entry() {
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        let mut h = HeaderMap::new();
        h.append(&X_FORWARDED_FOR, "6.6.6.6, 1.2.3.4".parse().unwrap());
        h.append(&X_FORWARDED_FOR, "10.0.0.2".parse().unwrap());
        assert_eq!(
            forwarded_for(&h, &trusted),
            Some("1.2.3.4".parse().unwrap())
        );
        let mut h = HeaderMap::new();
        h.insert(&X_FORWARDED_FOR, "garbage, 10.0.0.3".parse().unwrap());
        assert_eq!(
            forwarded_for(&h, &trusted),
            None,
            "a malformed chain is not believed"
        );
        assert_eq!(forwarded_for(&HeaderMap::new(), &trusted), None);
    }

    #[test]
    fn cookies_and_rails() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "a=1; hyperish_session=tok; b=2".parse().unwrap(),
        );
        assert_eq!(cookie(&h, "hyperish_session").as_deref(), Some("tok"));
        assert_eq!(cookie(&h, "c"), None);

        let mut h = HeaderMap::new();
        assert_eq!(rail(&h, true).unwrap().0, Rail::Standard);
        assert_eq!(
            rail(&h, false).unwrap_err().code(),
            "unsupported_media_type"
        );
        h.insert(
            header::CONTENT_TYPE,
            "application/json; charset=utf-8".parse().unwrap(),
        );
        assert_eq!(rail(&h, false).unwrap().0, Rail::Standard);
        h.insert(header::CONTENT_TYPE, MEDIA_TYPE_BINARY.parse().unwrap());
        assert_eq!(
            rail(&h, false).unwrap_err().code(),
            "unsupported_media_type"
        );
        h.insert(HEADER_FN, "f".parse().unwrap());
        h.insert(HEADER_SCHEMA, "+1".parse().unwrap());
        assert!(rail(&h, false).is_err(), "only hex digits");
        h.insert(HEADER_SCHEMA, "00000000000000ff".parse().unwrap());
        assert_eq!(
            rail(&h, false).unwrap(),
            (Rail::Fast, Some(("f".into(), 255)))
        );
    }
}
