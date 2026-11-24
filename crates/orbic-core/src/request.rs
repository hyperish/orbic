//! Transport-neutral requests and responses (`docs/03-adapters-and-pipeline.md`
//! §1). Adapters translate HTTP (or anything else) to and from these; the
//! pipeline never sees headers, status codes or sockets.

use std::any::Any;
use std::fmt;
use std::net::IpAddr;

use hyve_api::Method;
use hyve_core::{HyveError, RequestId};
use hyve_serde::AlignedBytes;

/// How a request's body and response are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rail {
    /// JSON (`hyperish/docs` `03-api-protocol.md`).
    Standard,
    /// `hyve-serde` binary, for `#[server]` functions only.
    Fast,
    /// Typed values without serialization (in-process `#[server]` calls).
    InProcess,
}

/// A request body.
pub enum Body {
    /// No body.
    Empty,
    /// Encoded bytes on the request's rail (JSON or binary).
    Bytes(Vec<u8>),
    /// The request model itself ([`Rail::InProcess`]).
    Value(Box<dyn Any + Send>),
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Empty => f.write_str("Empty"),
            Body::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Body::Value(_) => f.write_str("Value(..)"),
        }
    }
}

/// A credential the client presented (translated from cookies or the
/// `Authorization` header by the adapter).
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A session token from a cookie, with the CSRF token the request carried.
    Session {
        /// The session token.
        token: String,
        /// The request's CSRF token, if any.
        csrf: Option<String>,
    },
    /// A bearer token.
    Bearer(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Tokens never reach logs.
        match self {
            Credential::Session { csrf, .. } => {
                write!(
                    f,
                    "Session {{ token: .., csrf: {} }}",
                    if csrf.is_some() { ".." } else { "none" }
                )
            }
            Credential::Bearer(_) => f.write_str("Bearer(..)"),
        }
    }
}

/// The authenticated principal of a request. Authentication guards provide
/// it to the request scope; the `Idempotency-Key` gate and [`RateLimit`]
/// scope their state by it (`hyperish/docs` `02-core-contracts.md` §6).
///
/// [`RateLimit`]: crate::RateLimit
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal(String);

impl Principal {
    /// A principal with a stable identifier (a user ID, not a token).
    pub fn new(id: impl Into<String>) -> Self {
        Principal(id.into())
    }

    /// The identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Request metadata that is not part of the body.
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    /// The correlation ID the client sent; a new one is made otherwise.
    pub request_id: Option<RequestId>,
    /// `Idempotency-Key`.
    pub idempotency_key: Option<String>,
    /// The presented credential.
    pub credential: Option<Credential>,
    /// The client address (from a trusted proxy when behind one).
    pub client_addr: Option<IpAddr>,
    /// Fast rail: the `#[server]` function identifier and schema fingerprint.
    pub server_fn: Option<(String, u64)>,
}

/// A request entering the pipeline.
#[derive(Debug)]
pub struct Request {
    /// The method. For the fast rail it is `POST`, whatever the function binds.
    pub method: Method,
    /// The path, without the query string.
    pub path: String,
    /// The raw query string (without `?`).
    pub query: String,
    /// The rail.
    pub rail: Rail,
    /// The body.
    pub body: Body,
    /// Metadata.
    pub meta: RequestMeta,
}

impl Request {
    /// A standard-rail request without body or metadata.
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        let path = path.into();
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p.to_owned(), q.to_owned()),
            None => (path, String::new()),
        };
        Request {
            method,
            path,
            query,
            rail: Rail::Standard,
            body: Body::Empty,
            meta: RequestMeta::default(),
        }
    }

    /// Sets a JSON body.
    pub fn json(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.rail = Rail::Standard;
        self.body = Body::Bytes(body.into());
        self
    }
}

/// A successful response body.
pub enum Payload {
    /// No value (`()`): `204` on HTTP.
    Empty,
    /// The standard-rail success body (`{"data": ...}`).
    Json(String),
    /// The archived response model.
    Binary(AlignedBytes),
    /// The response model itself ([`Rail::InProcess`]).
    Value(Box<dyn Any + Send>),
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Payload::Empty => f.write_str("Empty"),
            Payload::Json(s) => write!(f, "Json({s})"),
            Payload::Binary(b) => write!(f, "Binary({} bytes)", b.len()),
            Payload::Value(_) => f.write_str("Value(..)"),
        }
    }
}

/// Rate-limit state for the response (`RateLimit-*` headers on HTTP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitInfo {
    /// Requests allowed per window.
    pub limit: u32,
    /// Requests left in this window.
    pub remaining: u32,
    /// Seconds until the window resets.
    pub reset_secs: u64,
}

/// Response metadata that is not part of the body.
#[derive(Debug, Clone, Default)]
pub struct ResponseMeta {
    /// Rate-limit state, when a `RateLimit` guard saw the request.
    pub rate_limit: Option<RateLimitInfo>,
    /// Seconds to wait before retrying (`Retry-After`), for retryable errors.
    pub retry_after: Option<u64>,
    /// Credential changes for the adapter to apply (set or clear cookies).
    pub credential_update: Option<CredentialUpdate>,
}

/// A credential change requested by the handler or a guard.
#[derive(Clone, PartialEq, Eq)]
pub enum CredentialUpdate {
    /// Store this session token (and CSRF token) on the client.
    SetSession {
        /// The session token.
        token: String,
        /// The CSRF token for the client to echo.
        csrf: String,
    },
    /// Remove the session from the client.
    ClearSession,
}

impl fmt::Debug for CredentialUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialUpdate::SetSession { .. } => f.write_str("SetSession(..)"),
            CredentialUpdate::ClearSession => f.write_str("ClearSession"),
        }
    }
}

/// The pipeline's answer.
#[derive(Debug)]
pub struct Response {
    /// The rail the response is encoded for (the request's).
    pub rail: Rail,
    /// The request's correlation ID.
    pub request_id: RequestId,
    /// The result. Errors are already in their public form (codes of
    /// non-public infrastructure errors replaced, internal messages hidden)
    /// and carry the request ID.
    pub outcome: Result<Payload, HyveError>,
    /// Metadata.
    pub meta: ResponseMeta,
}
