//! The `Idempotency-Key` gate and `RateLimit` (`docs/03-adapters-and-pipeline.md`
//! §1, `hyperish/docs` `03-api-protocol.md` §6, §8).

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use hyve::api::{BoxFuture, ContextNeed, Method};
use hyve::prelude::*;
use hyve::serde::{from_json_data, to_json};
use orbic_cache::{Cache, ManualClock};
use orbic_core::{
    Credential, Guard, GuardCtx, OrbicApp, Payload, Pipeline, Principal, RateLimit, Request,
    Response, component,
};
use orbic_tests::Note;

fn poll<F: Future + ?Sized>(f: Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

fn ok_u32(r: &Response) -> u32 {
    match &r.outcome {
        Ok(Payload::Json(s)) => from_json_data(s).unwrap(),
        other => panic!("expected JSON, got {other:?}"),
    }
}

fn code(r: &Response) -> &str {
    r.outcome
        .as_ref()
        .err()
        .map(HyveError::code)
        .unwrap_or_else(|| panic!("expected an error, got {:?}", r.outcome))
}

/// The bearer token is the principal (a stand-in for `AuthGuard`); `deny`
/// rejects requests before anything else.
struct Auth;

impl Guard for Auth {
    fn provides(&self) -> Vec<ContextNeed> {
        vec![ContextNeed::of::<Principal>()]
    }

    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>> {
        Box::pin(async move {
            match &cx.meta.credential {
                Some(Credential::Bearer(t)) if t == "deny" => {
                    Err(HyveError::domain("forbidden", "no"))
                }
                Some(Credential::Bearer(t)) => {
                    cx.scope.provide(Principal::new(t.clone()));
                    Ok(())
                }
                _ => Ok(()),
            }
        })
    }
}

#[derive(Default)]
struct Counter(AtomicU32);

impl Counter {
    fn bump(&self) -> u32 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
}

#[derive(Default)]
struct Open(AtomicBool);

async fn create(counter: Ctx<Counter>, _note: Valid<Note>) -> HyveResult<u32> {
    Ok(counter.bump())
}

async fn refuse(counter: Ctx<Counter>) -> HyveResult<u32> {
    counter.bump();
    Err(HyveError::domain("note_taken", "taken"))
}

async fn flaky(counter: Ctx<Counter>) -> HyveResult<u32> {
    match counter.bump() {
        1 => Err(HyveError::infra("db_down", "down").retryable(true)),
        n => Ok(n),
    }
}

async fn slow(counter: Ctx<Counter>, open: Ctx<Open>) -> HyveResult<u32> {
    let n = counter.bump();
    std::future::poll_fn(|_| {
        if open.0.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    Ok(n)
}

async fn peek(counter: Ctx<Counter>) -> HyveResult<u32> {
    Ok(counter.bump())
}

fn app() -> Pipeline {
    OrbicApp::new()
        .provide(Cache::in_memory())
        .provide(Counter::default())
        .provide(Open::default())
        .wrap(Auth)
        .post("/notes", create)
        .post("/refuse", refuse)
        .post("/flaky", flaky)
        .post("/slow", slow)
        .route(Method::Get, "/peek", peek)
        .build()
        .unwrap()
}

trait Post {
    fn post<H: orbic_core::Handler<A>, A>(self, path: &str, h: H) -> Self;
}

impl Post for OrbicApp {
    fn post<H: orbic_core::Handler<A>, A>(self, path: &str, h: H) -> Self {
        self.route(Method::Post, path, h)
    }
}

fn req(
    method: Method,
    path: &str,
    user: Option<&str>,
    key: Option<&str>,
    body: Option<&Note>,
) -> Request {
    let mut r = Request::new(method, path);
    if let Some(n) = body {
        r = r.json(to_json(n));
    }
    r.meta.credential = user.map(|u| Credential::Bearer(u.into()));
    r.meta.idempotency_key = key.map(Into::into);
    r
}

#[hyve::test]
async fn a_repeated_key_reuses_the_first_response() {
    let app = app();
    let note = Note {
        text: "a".into(),
        pinned: false,
    };
    let other = Note {
        text: "b".into(),
        pinned: false,
    };
    let send = |user, key, body| app.dispatch(req(Method::Post, "/notes", user, key, body));

    let first = send(Some("ada"), Some("k1"), Some(&note)).await;
    assert_eq!(ok_u32(&first), 1);
    let again = send(Some("ada"), Some("k1"), Some(&note)).await;
    assert_eq!(ok_u32(&again), 1, "not executed again");
    assert_ne!(again.request_id, first.request_id);
    assert_eq!(
        code(&send(Some("ada"), Some("k1"), Some(&other)).await),
        "idempotency_key_mismatch"
    );

    // Another principal, or none, never sees ada's response.
    assert_eq!(ok_u32(&send(Some("bob"), Some("k1"), Some(&note)).await), 2);
    assert_eq!(ok_u32(&send(None, Some("k1"), Some(&note)).await), 3);
    assert_eq!(
        ok_u32(&send(None, Some("k1"), Some(&note)).await),
        4,
        "no principal: no reuse"
    );
    // Without a key the gate does nothing; on another path the key is another key.
    assert_eq!(ok_u32(&send(Some("ada"), None, Some(&note)).await), 5);
    assert_eq!(
        ok_u32(
            &app.dispatch(req(Method::Get, "/peek", Some("ada"), Some("k1"), None))
                .await
        ),
        6
    );
    assert_eq!(
        ok_u32(
            &app.dispatch(req(Method::Get, "/peek", Some("ada"), Some("k1"), None))
                .await
        ),
        7,
        "GET is not gated"
    );

    let long = "x".repeat(256);
    for bad in ["", long.as_str(), "a\nb"] {
        assert_eq!(
            code(&send(Some("ada"), Some(bad), Some(&note)).await),
            "invalid_request",
            "{bad:?}"
        );
    }
    // Rejected by a guard: the gate is never reached.
    assert_eq!(
        code(&send(Some("deny"), Some("k1"), Some(&note)).await),
        "forbidden"
    );
}

#[hyve::test]
async fn domain_errors_are_reused_and_other_errors_are_not() {
    let app = app();
    let send = |path: &str| app.dispatch(req(Method::Post, path, Some("ada"), Some("k"), None));
    assert_eq!(code(&send("/refuse").await), "note_taken");
    assert_eq!(code(&send("/refuse").await), "note_taken");
    assert_eq!(
        ok_u32(
            &app.dispatch(req(Method::Get, "/peek", None, None, None))
                .await
        ),
        2,
        "refuse ran once"
    );

    let app = self::app();
    let send = || app.dispatch(req(Method::Post, "/flaky", Some("ada"), Some("k"), None));
    let r = send().await;
    assert_eq!(
        code(&r),
        "service_unavailable",
        "Infra codes are not public"
    );
    assert_eq!(
        ok_u32(&send().await),
        2,
        "an Infra error leaves the key free"
    );
    assert_eq!(ok_u32(&send().await), 2, "then the success is reused");
}

#[test]
fn a_running_key_answers_in_progress_and_a_cancelled_one_is_released() {
    let app = app();
    let send = || app.dispatch(req(Method::Post, "/slow", Some("ada"), Some("k"), None));
    let mut first = send();
    assert!(poll(first.as_mut()).is_pending());
    let Poll::Ready(r) = poll(send().as_mut()) else {
        panic!("answers at once")
    };
    assert_eq!(code(&r), "request_in_progress");
    assert_eq!(r.meta.retry_after, Some(1));

    drop(first); // the client went away mid-request
    let mut second = send();
    assert!(
        poll(second.as_mut()).is_pending(),
        "runs again instead of answering 409"
    );
    let Poll::Ready(peeked) = poll(
        app.dispatch(req(Method::Get, "/peek", None, None, None))
            .as_mut(),
    ) else {
        panic!()
    };
    assert_eq!(ok_u32(&peeked), 3, "slow ran twice");
}

#[test]
fn assembly_requires_the_cache() {
    let e = OrbicApp::new()
        .provide(Counter::default())
        .mount(
            "/v1",
            component()
                .wrap(RateLimit::per_minute(5))
                .get("/peek", peek),
        )
        .build()
        .err()
        .expect("no cache");
    assert_eq!(e.problems().len(), 1, "{e}");
    assert!(e.problems()[0].starts_with("guard `orbic_core::rate_limit::RateLimit` wrapped at `/v1` takes `orbic_cache::cache::Cache`"), "{e}");
}

fn limited(time: &ManualClock, auth_first: bool) -> Pipeline {
    let inner = component().get("/peek", peek);
    let inner = if auth_first {
        inner.wrap(Auth).wrap(RateLimit::per_minute(2))
    } else {
        inner.wrap(RateLimit::per_minute(2)).wrap(Auth)
    };
    OrbicApp::new()
        .provide(Cache::in_memory_with(100, time.clock()))
        .provide(Counter::default())
        .mount("/v1", inner)
        .build()
        .unwrap()
}

fn from(addr: u8, user: Option<&str>) -> Request {
    let mut r = req(Method::Get, "/v1/peek", user, None, None);
    r.meta.client_addr = Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, addr)));
    r
}

#[hyve::test]
async fn rate_limits_count_per_principal_or_address_and_reset() {
    let time = ManualClock::new();
    let app = limited(&time, true);
    let limit = |r: &Response| {
        r.meta
            .rate_limit
            .map(|l| (l.limit, l.remaining, l.reset_secs))
    };
    let r = app.dispatch(from(1, None)).await;
    assert_eq!(limit(&r), Some((2, 1, 60)));
    time.advance(Duration::from_millis(10_500));
    assert_eq!(
        limit(&app.dispatch(from(1, None)).await),
        Some((2, 0, 50)),
        "rounded up"
    );
    let r = app.dispatch(from(1, None)).await;
    assert_eq!(code(&r), "rate_limited");
    assert_eq!(
        (limit(&r), r.meta.retry_after),
        (Some((2, 0, 50)), Some(50))
    );

    // Another address, and principals (even from the same address), count apart.
    assert!(app.dispatch(from(2, None)).await.outcome.is_ok());
    assert!(app.dispatch(from(1, Some("ada"))).await.outcome.is_ok());
    assert!(app.dispatch(from(1, Some("bob"))).await.outcome.is_ok());
    // Rejected before the limiter: not counted, no header.
    let r = app.dispatch(from(3, Some("deny"))).await;
    assert_eq!((code(&r), limit(&r)), ("forbidden", None));

    time.advance(Duration::from_secs(50));
    assert_eq!(
        limit(&app.dispatch(from(1, None)).await),
        Some((2, 1, 60)),
        "a new window"
    );

    // Wrapped before authentication, the limiter counts by address only.
    let app = limited(&time, false);
    assert!(app.dispatch(from(1, Some("ada"))).await.outcome.is_ok());
    assert!(app.dispatch(from(1, Some("bob"))).await.outcome.is_ok());
    assert_eq!(
        code(&app.dispatch(from(1, Some("cy"))).await),
        "rate_limited"
    );
}
