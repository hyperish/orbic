//! `orbic-adapter-http`: the HTTP contract (`hyperish/docs`
//! `03-api-protocol.md`, `docs/03-adapters-and-pipeline.md` §1, decision 3
//! in `docs/04-open-questions.md`). Every scenario runs twice: through
//! `HttpApp::handle` (what `TestClient` uses) and over a real loopback
//! socket, and the two must answer alike.
#![cfg(feature = "server")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use hyve::api::{ErrorWire, Method};
use hyve::api::{ServerFn, ServerFnDescriptor, StubFn};
use hyve::prelude::*;
use hyve::serde::{Binary, from_json_data, to_json};
use orbic::__private::auth::{self, Client, SignedIn};
use orbic::{Cache, RateLimit};
use orbic_adapter_http::{HttpApp, HttpConfig};
use orbic_core::{OrbicApp, component};
use orbic_tests::{AuthGuard, CurrentUser, Note, Store, add_note, delete_note};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const PEER: &str = "127.0.0.1:40000";

fn d<F: StubFn<A>, A>(_: F) -> &'static ServerFnDescriptor {
    <F::Marker as ServerFn>::descriptor()
}

#[model]
#[derive(Debug, Clone)]
struct Login {
    name: String,
    bearer: bool,
}

async fn login(input: Valid<Login>) -> HyveResult<SignedIn> {
    let input = input.into_inner();
    let user = CurrentUser {
        id: Id::new(),
        name: input.name,
        admin: false,
    };
    let client = if input.bearer {
        Client::Bearer
    } else {
        Client::Web
    };
    auth::sign_in(&user, client).await
}

async fn me(user: Ctx<CurrentUser>) -> HyveResult<String> {
    Ok(user.name.clone())
}

async fn logout() -> HyveResult<()> {
    auth::sign_out().await
}

async fn ping() -> HyveResult<String> {
    Ok("pong".into())
}

/// A side effect per call, so a replay is visible.
static ECHOES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn echo(note: Valid<Note>) -> HyveResult<String> {
    let n = ECHOES.fetch_add(1, Ordering::SeqCst);
    Ok(format!("{} #{n}", note.into_inner().text))
}

async fn broken() -> HyveResult<String> {
    Err(HyveError::internal("db password is hunter2"))
}

/// Set when a `slow` handler's future is dropped before it finished.
static SLOW_DROPPED: AtomicBool = AtomicBool::new(false);

struct DropFlag(bool);

impl Drop for DropFlag {
    fn drop(&mut self) {
        if !self.0 {
            SLOW_DROPPED.store(true, Ordering::SeqCst);
        }
    }
}

#[model]
#[derive(Debug, Clone)]
struct Wait {
    millis: u64,
}

async fn slow(input: Valid<Wait>) -> HyveResult<String> {
    let mut flag = DropFlag(false);
    tokio::time::sleep(Duration::from_millis(input.into_inner().millis)).await;
    flag.0 = true;
    Ok("done".into())
}

fn pipeline(config: HttpConfig) -> orbic_core::Pipeline {
    OrbicApp::new()
        .provide(Cache::in_memory())
        .provide(Store {
            prefix: "> ".into(),
        })
        .provide(config)
        .route(Method::Post, "/login", login)
        .route(Method::Get, "/broken", broken)
        .route(Method::Post, "/slow", slow)
        .mount("/v1", component().handle(add_note).handle(delete_note))
        .mount(
            "/limited",
            component()
                .wrap(RateLimit::per_minute(2))
                .get("/ping", ping),
        )
        .mount(
            "/me",
            component()
                .wrap(AuthGuard::require_user())
                .get("/", me)
                .post("/", me)
                .post("/logout", logout)
                .post("/notes", echo),
        )
        .build()
        .unwrap()
}

fn config() -> HttpConfig {
    HttpConfig::default()
        .cors_origin("https://app.example")
        .max_body(64)
}

/// How a scenario reaches the app.
enum Via {
    Direct(HttpApp),
    Socket {
        addr: SocketAddr,
        stop: Option<oneshot::Sender<()>>,
        server: tokio::task::JoinHandle<std::io::Result<()>>,
    },
}

impl Via {
    fn direct(config: HttpConfig) -> Via {
        Via::Direct(
            HttpApp::new(pipeline(config))
                .unwrap()
                .allow_plaintext(true),
        )
    }

    async fn socket(config: HttpConfig) -> Via {
        let app = HttpApp::new(pipeline(config)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(app.serve(listener, async {
            let _ = stopped.await;
        }));
        Via::Socket {
            addr,
            stop: Some(stop),
            server,
        }
    }

    async fn both(config: HttpConfig) -> [Via; 2] {
        [Via::direct(config.clone()), Via::socket(config).await]
    }

    fn name(&self) -> &'static str {
        match self {
            Via::Direct(_) => "direct",
            Via::Socket { .. } => "socket",
        }
    }

    async fn send(&self, req: http::Request<Bytes>) -> http::Response<Bytes> {
        match self {
            Via::Direct(app) => app.handle(req, PEER.parse().unwrap()).await,
            Via::Socket { addr, .. } => over_socket(*addr, req).await,
        }
    }

    async fn stop(mut self) {
        if let Via::Socket { stop, server, .. } = &mut self {
            let _ = stop.take().unwrap().send(());
            server.await.unwrap().unwrap();
        }
    }
}

async fn over_socket(addr: SocketAddr, req: http::Request<Bytes>) -> http::Response<Bytes> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let (parts, body) = req.into_parts();
    let res = sender
        .send_request(http::Request::from_parts(parts, Full::new(body)))
        .await
        .unwrap();
    let (parts, body) = res.into_parts();
    http::Response::from_parts(parts, body.collect().await.unwrap().to_bytes())
}

fn req(method: &str, path: &str) -> http::request::Builder {
    http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
}

fn get(path: &str) -> http::Request<Bytes> {
    req("GET", path).body(Bytes::new()).unwrap()
}

fn json_req(method: &str, path: &str, body: &str) -> http::Request<Bytes> {
    req(method, path)
        .header("content-type", "application/json")
        .body(Bytes::from(body.to_owned()))
        .unwrap()
}

fn header<'a>(r: &'a http::Response<Bytes>, name: &str) -> Option<&'a str> {
    r.headers().get(name).and_then(|v| v.to_str().ok())
}

fn body(r: &http::Response<Bytes>) -> &str {
    std::str::from_utf8(r.body()).unwrap()
}

/// `(code, requestId)` of a JSON error envelope.
fn envelope(r: &http::Response<Bytes>) -> (String, String) {
    assert_eq!(header(r, "content-type"), Some("application/json"), "{r:?}");
    let v: Value = hyve::serde::from_json(body(r)).unwrap();
    let e = v
        .get("error")
        .unwrap_or_else(|| panic!("no envelope: {}", body(r)));
    let s = |k: &str| match e.get(k) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{k}: {other:?} in {}", body(r)),
    };
    assert!(e.get("details").is_some() && e.get("message").is_some());
    (s("code"), s("requestId"))
}

fn binary(
    path: &str,
    f: &ServerFnDescriptor,
    fingerprint: u64,
    body: Vec<u8>,
) -> http::Request<Bytes> {
    req("POST", path)
        .header("content-type", "application/vnd.hyperish.rkyv")
        .header("hyperish-fn", f.id)
        .header("hyperish-schema", format!("{fingerprint:016x}"))
        .body(Bytes::from(body))
        .unwrap()
}

#[hyve::test]
async fn standard_rail_envelopes_statuses_and_headers() {
    for via in Via::both(config()).await {
        let at = via.name();
        let note = to_json(&Note {
            text: "hi".into(),
            pinned: false,
        });
        let r = via
            .send(
                req("POST", "/v1/notes")
                    .header("content-type", "application/json; charset=utf-8")
                    .header("x-request-id", "trace-1")
                    .body(Bytes::from(note))
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status(), 200, "{at}");
        assert_eq!(
            body(&r),
            r#"{"data":{"text":"> hi","pinned":false}}"#,
            "{at}"
        );
        assert_eq!(header(&r, "x-request-id"), Some("trace-1"), "{at}");
        for (h, v) in [
            (
                "content-security-policy",
                "default-src 'none'; frame-ancestors 'none'",
            ),
            ("strict-transport-security", "max-age=31536000"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ] {
            assert_eq!(header(&r, h), Some(v), "{at} {h}");
        }

        // Domain errors: the envelope carries the request ID of the header.
        let r = via
            .send(json_req(
                "POST",
                "/v1/notes",
                r#"{"text":"taken","pinned":false}"#,
            ))
            .await;
        assert_eq!(r.status(), 422, "{at}");
        let (code, id) = envelope(&r);
        assert_eq!(code, "note_taken");
        assert_eq!(Some(id.as_str()), header(&r, "x-request-id"), "{at}");
        assert_eq!(header(&r, "retry-after"), None, "{at}: not retryable");

        let r = via
            .send(json_req(
                "POST",
                "/v1/notes",
                r#"{"text":"","pinned":false}"#,
            ))
            .await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{at}"
        );
        let r = via.send(get("/nowhere")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::NOT_FOUND, "route_not_found"),
            "{at}"
        );
        let r = via
            .send(
                req("DELETE", &format!("/v1/notes/{}", Id::<Note>::new()))
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status(), 204, "{at}");
        assert!(r.body().is_empty(), "{at}");

        let r = via.send(get("/v1/notes")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
            "{at}"
        );
        assert_eq!(header(&r, "allow"), Some("POST"), "{at}");
        let r = via
            .send(req("PUT", "/v1/notes").body(Bytes::new()).unwrap())
            .await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
            "{at}"
        );

        let r = via
            .send(
                req("POST", "/v1/notes")
                    .header("content-type", "text/plain")
                    .body(Bytes::from_static(b"hi"))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"),
            "{at}"
        );
        let big = format!(r#"{{"text":"{}","pinned":false}}"#, "x".repeat(100));
        let r = via.send(json_req("POST", "/v1/notes", &big)).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            "{at}"
        );

        // Internal errors: 500, no internals.
        let r = via.send(get("/broken")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            "{at}"
        );
        assert!(!body(&r).contains("hunter2"), "{at}");

        // An unusable request ID is replaced, not refused.
        let r = via
            .send(
                req("GET", "/nowhere")
                    .header("x-request-id", "has space")
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status(), 404);
        assert_ne!(header(&r, "x-request-id"), Some("has space"));
        assert_eq!(
            header(&r, "x-request-id"),
            Some(envelope(&r).1.as_str()),
            "{at}"
        );
        via.stop().await;
    }
}

#[hyve::test]
async fn fast_rail_archives_answers_and_falls_back_on_415() {
    let add = d(add_note);
    for via in Via::both(config()).await {
        let at = via.name();
        let note = Note {
            text: "hi".into(),
            pinned: true,
        };
        let r = via
            .send(binary(
                "/v1/notes",
                add,
                add.fingerprint,
                note.to_binary().unwrap().to_vec(),
            ))
            .await;
        assert_eq!(r.status(), 200, "{at}");
        assert_eq!(
            header(&r, "content-type"),
            Some("application/vnd.hyperish.rkyv"),
            "{at}"
        );
        let aligned = hyve::serde::AlignedBytes::copy_from(r.body());
        assert_eq!(Note::from_binary(&aligned).unwrap().text, "> hi", "{at}");

        // Errors on the fast rail are the archived public error, same status.
        let taken = Note {
            text: "taken".into(),
            pinned: false,
        };
        let r = via
            .send(binary(
                "/v1/notes",
                add,
                add.fingerprint,
                taken.to_binary().unwrap().to_vec(),
            ))
            .await;
        assert_eq!(r.status(), 422, "{at}");
        assert_eq!(
            header(&r, "content-type"),
            Some("application/vnd.hyperish.rkyv"),
            "{at}"
        );
        let wire = ErrorWire::from_binary(&hyve::serde::AlignedBytes::copy_from(r.body())).unwrap();
        assert_eq!(wire.code, "note_taken", "{at}");
        assert_eq!(
            wire.request_id.as_deref(),
            header(&r, "x-request-id"),
            "{at}"
        );

        // A different schema or a missing header: 415 in the JSON envelope.
        let r = via
            .send(binary("/v1/notes", add, add.fingerprint ^ 1, vec![]))
            .await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "schema_mismatch"),
            "{at}"
        );
        let r = via
            .send(
                req("POST", "/v1/notes")
                    .header("content-type", "application/vnd.hyperish.rkyv")
                    .body(Bytes::from_static(b"x"))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"),
            "{at}"
        );
        via.stop().await;
    }
}

#[hyve::test]
async fn idempotency_and_rate_limit_travel_as_headers() {
    for via in Via::both(config()).await {
        let at = via.name();
        // The gate needs a principal (`docs/03-adapters-and-pipeline.md` §1).
        let r = via
            .send(json_req(
                "POST",
                "/login",
                r#"{"name":"ada","bearer":true}"#,
            ))
            .await;
        let token = from_json_data::<SignedIn>(body(&r))
            .unwrap()
            .access_token
            .unwrap();
        let bearer = format!("Bearer {token}");
        let keyed = |text: &str| {
            req("POST", "/me/notes")
                .header("authorization", &bearer)
                .header("content-type", "application/json")
                .header("idempotency-key", "k-1")
                .body(Bytes::from(format!(
                    r#"{{"text":"{text}","pinned":false}}"#
                )))
                .unwrap()
        };
        let first = via.send(keyed("a")).await;
        let again = via.send(keyed("a")).await;
        assert_eq!(
            (first.status(), body(&first)),
            (again.status(), body(&again)),
            "{at}"
        );
        let r = via.send(keyed("b")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::CONFLICT, "idempotency_key_mismatch"),
            "{at}"
        );
        let r = via
            .send(
                req("POST", "/me/notes")
                    .header("authorization", &bearer)
                    .header("content-type", "application/json")
                    .header("idempotency-key", "")
                    .body(Bytes::from_static(b"{}"))
                    .unwrap(),
            )
            .await;
        assert_eq!(envelope(&r).0, "invalid_request", "{at}");

        let r = via.send(get("/limited/ping")).await;
        assert_eq!(r.status(), 200, "{at}");
        assert_eq!(header(&r, "ratelimit-limit"), Some("2"), "{at}");
        assert_eq!(header(&r, "ratelimit-remaining"), Some("1"), "{at}");
        assert!(
            header(&r, "ratelimit-reset").is_some_and(|s| s.parse::<u64>().unwrap() <= 60),
            "{at}"
        );
        via.send(get("/limited/ping")).await;
        let r = via.send(get("/limited/ping")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            "{at}"
        );
        assert!(
            header(&r, "retry-after").is_some_and(|s| s.parse::<u64>().unwrap() >= 1),
            "{at}: retryable"
        );
        assert_eq!(header(&r, "ratelimit-remaining"), Some("0"), "{at}");
        via.stop().await;
    }
}

#[hyve::test]
async fn sessions_use_cookies_csrf_and_bearer_tokens() {
    for via in Via::both(config()).await {
        let at = via.name();
        let r = via
            .send(json_req(
                "POST",
                "/login",
                r#"{"name":"ada","bearer":false}"#,
            ))
            .await;
        assert_eq!(r.status(), 200, "{at}");
        let cookie = header(&r, "set-cookie").unwrap().to_owned();
        let (pair, attrs) = cookie.split_once("; ").unwrap();
        assert_eq!(attrs, "Path=/; HttpOnly; Secure; SameSite=Lax", "{at}");
        assert!(
            pair.starts_with("hyperish_session=") && pair.len() == "hyperish_session=".len() + 43
        );
        let signed: SignedIn = from_json_data(body(&r)).unwrap();
        let csrf = signed.csrf_token.unwrap();
        assert!(
            !body(&r).contains(&pair["hyperish_session=".len()..]),
            "the token stays in the cookie"
        );

        let with_cookie = |method: &str, csrf: Option<&str>| {
            let mut b = req(method, "/me").header("cookie", format!("other=1; {pair}"));
            if let Some(c) = csrf {
                b = b.header("x-csrf-token", c);
            }
            b.body(Bytes::new()).unwrap()
        };
        let r = via.send(with_cookie("GET", None)).await;
        assert_eq!(
            (r.status(), body(&r)),
            (StatusCode::OK, r#"{"data":"ada"}"#),
            "{at}"
        );
        let r = via.send(with_cookie("POST", None)).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::FORBIDDEN, "csrf_token_invalid"),
            "{at}"
        );
        let r = via.send(with_cookie("POST", Some(&csrf))).await;
        assert_eq!(r.status(), 200, "{at}");
        let r = via.send(get("/me")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::UNAUTHORIZED, "unauthenticated"),
            "{at}"
        );

        // Bearer clients: no cookie; the header wins over a cookie.
        let r = via
            .send(json_req(
                "POST",
                "/login",
                r#"{"name":"bob","bearer":true}"#,
            ))
            .await;
        assert_eq!(header(&r, "set-cookie"), None, "{at}");
        let token = from_json_data::<SignedIn>(body(&r))
            .unwrap()
            .access_token
            .unwrap();
        let r = via
            .send(
                req("GET", "/me")
                    .header("authorization", format!("Bearer {token}"))
                    .header("cookie", pair)
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(body(&r), r#"{"data":"bob"}"#, "{at}");

        let r = via
            .send(
                req("POST", "/me/logout")
                    .header("cookie", pair)
                    .header("x-csrf-token", &csrf)
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status(), 204, "{at}");
        assert_eq!(
            header(&r, "set-cookie"),
            Some("hyperish_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax"),
            "{at}"
        );
        let r = via.send(with_cookie("GET", None)).await;
        assert_eq!(r.status(), 401, "{at}: revoked");
        via.stop().await;
    }
}

#[hyve::test]
async fn cors_opens_only_listed_origins() {
    for via in Via::both(config()).await {
        let at = via.name();
        let preflight = |origin: &str| {
            req("OPTIONS", "/v1/notes")
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "content-type, x-csrf-token",
                )
                .body(Bytes::new())
                .unwrap()
        };
        let r = via.send(preflight("https://app.example")).await;
        assert_eq!(r.status(), 204, "{at}");
        assert_eq!(
            header(&r, "access-control-allow-origin"),
            Some("https://app.example"),
            "{at}"
        );
        assert_eq!(
            header(&r, "access-control-allow-credentials"),
            Some("true"),
            "{at}"
        );
        assert_eq!(
            header(&r, "access-control-allow-methods"),
            Some("GET, POST, PATCH, DELETE"),
            "{at}"
        );
        assert!(
            header(&r, "access-control-allow-headers")
                .unwrap()
                .contains("x-csrf-token"),
            "{at}"
        );
        let r = via.send(preflight("https://evil.example")).await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{at}"
        );
        assert_eq!(header(&r, "access-control-allow-origin"), None, "{at}");

        let r = via
            .send(
                req("GET", "/limited/ping")
                    .header("origin", "https://app.example")
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(
            header(&r, "access-control-allow-origin"),
            Some("https://app.example"),
            "{at}"
        );
        assert_eq!(header(&r, "vary"), Some("origin"), "{at}");
        let exposed = header(&r, "access-control-expose-headers").unwrap();
        for h in ["retry-after", "ratelimit-remaining", "x-request-id"] {
            assert!(exposed.contains(h), "{at} {h}");
        }
        let r = via
            .send(
                req("GET", "/limited/ping")
                    .header("origin", "https://evil.example")
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(header(&r, "access-control-allow-origin"), None, "{at}");
        via.stop().await;
    }
}

#[hyve::test]
async fn health_metrics_and_shutdown() {
    for via in Via::both(config()).await {
        let at = via.name();
        let r = via.send(get("/health")).await;
        assert_eq!((r.status(), body(&r)), (StatusCode::OK, "ok"), "{at}");
        assert!(header(&r, "content-security-policy").is_some(), "{at}");
        via.send(get("/limited/ping")).await;
        via.send(get("/nowhere")).await;
        let r = via.send(get("/metrics")).await;
        assert_eq!(r.status(), 200, "{at}");
        assert_eq!(
            header(&r, "content-type"),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
        let text = body(&r);
        assert!(
            text.contains("orbic_http_requests_total{class=\"2xx\"} 1\n"),
            "{at}: {text}"
        );
        assert!(
            text.contains("orbic_http_requests_total{class=\"4xx\"} 1\n"),
            "{at}: {text}"
        );
        assert!(
            text.contains("orbic_http_request_duration_seconds_count 2\n"),
            "{at}: {text}"
        );
        assert!(
            text.contains("# TYPE orbic_http_requests_in_flight gauge\n"),
            "{at}"
        );
        let r = via
            .send(
                req("GET", "/metrics")
                    .header("x-forwarded-for", "1.2.3.4")
                    .body(Bytes::new())
                    .unwrap(),
            )
            .await;
        assert_eq!(
            (r.status(), envelope(&r).0.as_str()),
            (StatusCode::NOT_FOUND, "route_not_found"),
            "{at}: forwarded"
        );
        via.stop().await;
    }

    // Not loopback-allowed: /metrics is hidden.
    let app = HttpApp::new(pipeline(config().metrics_from(&["10.0.0.0/8"])))
        .unwrap()
        .allow_plaintext(true);
    assert_eq!(
        app.handle(get("/metrics"), PEER.parse().unwrap())
            .await
            .status(),
        404
    );

    // Draining: /health turns 503 and new requests get a retryable 503.
    let app = HttpApp::new(pipeline(config()))
        .unwrap()
        .allow_plaintext(true);
    app.pipeline().begin_shutdown();
    let r = app.handle(get("/health"), PEER.parse().unwrap()).await;
    assert_eq!(
        (r.status(), body(&r)),
        (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
    );
    let r = app
        .handle(get("/limited/ping"), PEER.parse().unwrap())
        .await;
    assert_eq!(
        (r.status(), envelope(&r).0.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable")
    );
    assert_eq!(header(&r, "retry-after"), Some("1"));
}

#[hyve::test]
async fn graceful_shutdown_finishes_running_requests_and_cancels_after_the_grace() {
    let wait = |millis: u64| json_req("POST", "/slow", &format!(r#"{{"millis":{millis}}}"#));

    // Within the grace period: the running request completes.
    let via = Via::socket(config()).await;
    let Via::Socket { addr, .. } = &via else {
        unreachable!()
    };
    let running = tokio::spawn(over_socket(*addr, wait(200)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    via.stop().await;
    let r = running.await.unwrap();
    assert_eq!(
        (r.status(), body(&r)),
        (StatusCode::OK, r#"{"data":"done"}"#)
    );

    // Past the grace period: the request is cancelled and the server returns.
    SLOW_DROPPED.store(false, Ordering::SeqCst);
    let via = Via::socket(config().shutdown_grace(Duration::from_millis(100))).await;
    let Via::Socket { addr, .. } = &via else {
        unreachable!()
    };
    let addr = *addr;
    let running = tokio::spawn(async move {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(conn);
        let (parts, body) = wait(60_000).into_parts();
        sender
            .send_request(http::Request::from_parts(parts, Full::new(body)))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(5), via.stop())
        .await
        .expect("serve returned");
    assert!(
        SLOW_DROPPED.load(Ordering::SeqCst),
        "the handler was cancelled"
    );
    assert!(
        running.await.unwrap().is_err(),
        "the connection closed without an answer"
    );
}

#[hyve::test]
async fn a_client_that_disconnects_cancels_its_request() {
    SLOW_DROPPED.store(false, Ordering::SeqCst);
    let via = Via::socket(config()).await;
    let Via::Socket { addr, .. } = &via else {
        unreachable!()
    };
    let addr = *addr;
    let client = tokio::spawn(over_socket(
        addr,
        json_req("POST", "/slow", r#"{"millis":60000}"#),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.abort();
    let _ = client.await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !SLOW_DROPPED.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        SLOW_DROPPED.load(Ordering::SeqCst),
        "the handler was dropped with its connection"
    );
    via.stop().await;
}

#[hyve::test]
async fn behind_a_proxy_https_is_required_and_the_client_address_is_forwarded() {
    let cfg = config().trusted_proxy("10.0.0.0/8");
    let app = HttpApp::new(pipeline(cfg)).unwrap();
    let proxy: SocketAddr = "10.0.0.2:5000".parse().unwrap();
    let outsider: SocketAddr = "203.0.113.9:5000".parse().unwrap();

    // Plain HTTP straight to a non-loopback server, or via the proxy over
    // HTTP, is refused; probes are not.
    let r = app.handle(get("/limited/ping"), outsider).await;
    assert_eq!(
        (r.status(), envelope(&r).0.as_str()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    let spoofed = req("GET", "/limited/ping")
        .header("x-forwarded-proto", "https")
        .body(Bytes::new())
        .unwrap();
    assert_eq!(
        app.handle(spoofed, outsider).await.status(),
        400,
        "only trusted proxies are believed"
    );
    let http_via_proxy = req("GET", "/limited/ping")
        .header("x-forwarded-proto", "https, http")
        .body(Bytes::new())
        .unwrap();
    assert_eq!(
        app.handle(http_via_proxy, proxy).await.status(),
        400,
        "the nearest proxy saw http"
    );
    assert_eq!(app.handle(get("/health"), outsider).await.status(), 200);

    // Two clients behind the proxy are limited separately; a spoofed
    // leftmost entry does not help.
    let from = |client: &str| {
        req("GET", "/limited/ping")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-for", format!("6.6.6.6, {client}"))
            .body(Bytes::new())
            .unwrap()
    };
    for _ in 0..2 {
        assert_eq!(app.handle(from("198.51.100.1"), proxy).await.status(), 200);
    }
    assert_eq!(app.handle(from("198.51.100.1"), proxy).await.status(), 429);
    assert_eq!(app.handle(from("198.51.100.2"), proxy).await.status(), 200);
}

#[test]
fn invalid_http_apps_fail_to_build() {
    let e = HttpApp::new(
        OrbicApp::new()
            .provide(
                HttpConfig::default()
                    .cors_origin("*")
                    .trusted_proxy("10.0.0.0/99"),
            )
            .route(Method::Get, "/health", ping)
            .build()
            .unwrap(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("cors_origin: `*`"), "{e}");
    assert!(e.contains("trusted_proxy: `10.0.0.0/99`"), "{e}");
    assert!(
        e.contains("`/health` is answered by the HTTP adapter"),
        "{e}"
    );
    let app = HttpApp::new(
        OrbicApp::new()
            .route(Method::Get, "/docs", ping)
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(app.document("/docs", "application/json", "{}").is_err());
}

#[hyve::test]
async fn direct_and_socket_answers_are_identical() {
    let [direct, socket] = Via::both(config()).await;
    let add = d(add_note);
    let requests = || {
        vec![
            json_req("POST", "/v1/notes", r#"{"text":"hi","pinned":false}"#),
            json_req("POST", "/v1/notes", r#"{"text":"taken","pinned":false}"#),
            binary("/v1/notes", add, add.fingerprint ^ 1, vec![]),
            get("/v1/notes"),
            get("/limited/ping"),
            req("OPTIONS", "/v1/notes")
                .header("origin", "https://app.example")
                .header("access-control-request-method", "POST")
                .body(Bytes::new())
                .unwrap(),
        ]
    };
    for (a, b) in requests().into_iter().zip(requests()) {
        let path = a.uri().to_string();
        let set_id = |mut r: http::Request<Bytes>| {
            r.headers_mut()
                .insert("x-request-id", "fixed".parse().unwrap());
            r
        };
        let (x, y) = (direct.send(set_id(a)).await, socket.send(set_id(b)).await);
        let strip = |h: &HeaderMap| {
            let mut h = h.clone();
            for n in ["date", "content-length", "transfer-encoding"] {
                h.remove(n);
            }
            h
        };
        assert_eq!(x.status(), y.status(), "{path}");
        assert_eq!(strip(x.headers()), strip(y.headers()), "{path}");
        assert_eq!(x.body(), y.body(), "{path}");
    }
    socket.stop().await;
}
