//! `orbic-core`: assembly contract, routing, guards, extraction, errors and
//! shutdown (`docs/01-dx-and-dsl.md` §1 조립 계약, `docs/03-adapters-and-pipeline.md` §1).

use std::sync::{Arc, Mutex};

use hyve::api::{BoxFuture, ContextNeed, Method, Path, Query};
use hyve::prelude::*;
use hyve::serde::{from_json_data, to_json};
use orbic_core::{
    Guard, GuardCtx, OrbicApp, Payload, Pipeline, RateLimitInfo, Request, Response, component,
};
use orbic_tests::{Note, NoteFilter};

fn json(r: &Response) -> &str {
    match &r.outcome {
        Ok(Payload::Json(s)) => s,
        other => panic!("expected JSON, got {other:?}"),
    }
}

fn err(r: &Response) -> &HyveError {
    r.outcome
        .as_ref()
        .err()
        .unwrap_or_else(|| panic!("expected an error, got {:?}", r.outcome))
}

async fn send(p: &Pipeline, req: Request) -> Response {
    p.dispatch(req).await
}

struct Label(&'static str);

async fn label(l: Ctx<Label>) -> HyveResult<String> {
    Ok(l.0.to_owned())
}

#[hyve::test]
async fn provided_values_are_scoped_to_the_subtree() {
    let app = OrbicApp::new()
        .provide(Label("root"))
        .mount("/a", component().get("/label", label))
        .mount(
            "/b",
            component()
                .provide(Label("b"))
                .get("/label", label)
                .mount("/c", component().get("/label", label)),
        )
        .build()
        .unwrap();
    for (path, expected) in [("/a/label", "root"), ("/b/label", "b"), ("/b/c/label", "b")] {
        let r = send(&app, Request::new(Method::Get, path)).await;
        assert_eq!(
            from_json_data::<String>(json(&r)).unwrap(),
            expected,
            "{path}"
        );
    }

    // A sibling's value is not visible.
    let problems = OrbicApp::new()
        .mount("/a", component().provide(Label("a")))
        .mount("/b", component().get("/label", label))
        .build()
        .err()
        .expect("assembly must fail")
        .problems()
        .to_vec();
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with("GET /b/label: ")
            && problems[0].contains("takes `Ctx<pipeline::Label>`"),
        "{problems:?}"
    );
}

async fn nothing() -> HyveResult<()> {
    Ok(())
}

#[test]
fn assembly_reports_every_problem() {
    let e = OrbicApp::new()
        .mount(
            "/v1",
            component()
                .get("/x", nothing)
                .get("/x", label)
                .post("/x", nothing),
        )
        .mount("v2", component())
        .route(Method::Get, "no-slash", nothing)
        .build()
        .err()
        .expect("assembly must fail");
    let text = e.to_string();
    assert_eq!(e.problems().len(), 5, "{text}");
    assert!(
        text.contains("POST /v1/x: `POST` routes accept `Idempotency-Key`, which needs a `Cache`"),
        "{text}"
    );
    assert!(text.contains("GET /v1/x is registered twice"), "{text}");
    assert!(text.contains("takes `Ctx<pipeline::Label>`"), "{text}");
    assert!(text.contains("mount prefix \"v2\""), "{text}");
    assert!(text.contains("\"no-slash\""), "{text}");
}

/// Records its name each time it runs; optionally rejects or provides.
struct Trace {
    name: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
    reject: bool,
}

struct User(&'static str);

impl Guard for Trace {
    fn provides(&self) -> Vec<ContextNeed> {
        if self.name == "auth" {
            vec![ContextNeed::of::<User>()]
        } else {
            Vec::new()
        }
    }

    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(self.name);
            if self.reject {
                return Err(HyveError::domain("forbidden", "no"));
            }
            if self.name == "auth" {
                cx.scope.provide(User("ada"));
            }
            if self.name == "limit" {
                cx.response.rate_limit = Some(RateLimitInfo {
                    limit: 60,
                    remaining: 59,
                    reset_secs: 60,
                });
            }
            Ok(())
        })
    }
}

async fn whoami(user: Ctx<User>) -> HyveResult<String> {
    Ok(user.0.to_owned())
}

#[hyve::test]
async fn guards_run_once_each_outermost_first() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let t = |name, reject| Trace {
        name,
        log: log.clone(),
        reject,
    };
    let app = OrbicApp::new()
        .wrap(t("app", false))
        .mount(
            "/v1",
            component()
                .wrap(t("auth", false))
                .wrap(t("limit", false))
                .mount(
                    "/inner",
                    component().wrap(t("inner", false)).get("/me", whoami),
                )
                .mount(
                    "/deny",
                    component().wrap(t("deny", true)).get("/me", whoami),
                ),
        )
        .build()
        .unwrap();

    let r = send(&app, Request::new(Method::Get, "/v1/inner/me")).await;
    assert_eq!(
        from_json_data::<String>(json(&r)).unwrap(),
        "ada",
        "a guard provides the request's user"
    );
    assert_eq!(*log.lock().unwrap(), ["app", "auth", "limit", "inner"]);
    assert_eq!(r.meta.rate_limit.map(|l| l.remaining), Some(59));

    log.lock().unwrap().clear();
    let r = send(&app, Request::new(Method::Get, "/v1/deny/me")).await;
    assert_eq!(err(&r).code(), "forbidden");
    assert_eq!(*log.lock().unwrap(), ["app", "auth", "limit", "deny"]);

    // Routing failures happen before any guard.
    log.lock().unwrap().clear();
    assert_eq!(
        err(&send(&app, Request::new(Method::Get, "/v1/none")).await).code(),
        "route_not_found"
    );
    assert_eq!(
        err(&send(&app, Request::new(Method::Post, "/v1/inner/me")).await).code(),
        "method_not_allowed"
    );
    assert!(log.lock().unwrap().is_empty());
}

async fn create(note: Valid<Note>) -> HyveResult<Note> {
    Ok(note.into_inner())
}

async fn find(filter: Query<NoteFilter>) -> HyveResult<Page<Note>> {
    let note = Note {
        text: "q".into(),
        pinned: filter.pinned.unwrap_or(false),
    };
    Ok(Page::new(
        vec![note; filter.limit.unwrap_or(0) as usize],
        None,
    ))
}

async fn by_id(id: Path<Id<Note>>) -> HyveResult<String> {
    Ok(id.to_string())
}

async fn boom() -> HyveResult<u32> {
    Err(HyveError::internal("secret stack detail"))
}

async fn scope_is_current() -> HyveResult<bool> {
    Ok(Scope::current().is_some_and(|s| s.parent().is_some()))
}

#[hyve::test]
async fn extractors_read_body_query_and_path() {
    let app = OrbicApp::new()
        .provide(orbic_cache::Cache::in_memory())
        .route(Method::Post, "/notes", create)
        .route(Method::Get, "/notes", find)
        .route(Method::Get, "/notes/{id}", by_id)
        .route(Method::Get, "/boom", boom)
        .route(Method::Get, "/scope", scope_is_current)
        .build()
        .unwrap();

    let note = Note {
        text: "hi".into(),
        pinned: true,
    };
    let r = send(
        &app,
        Request::new(Method::Post, "/notes").json(to_json(&note)),
    )
    .await;
    assert_eq!(json(&r), r#"{"data":{"text":"hi","pinned":true}}"#);

    let r = send(
        &app,
        Request::new(Method::Post, "/notes").json(r#"{"text":"","pinned":true}"#),
    )
    .await;
    let e = err(&r);
    assert_eq!(e.code(), "invalid_request");
    assert!(e.details().get("fields").is_some(), "{e:?}");
    assert_eq!(
        err(&send(&app, Request::new(Method::Post, "/notes").json("{")).await).code(),
        "invalid_request"
    );
    assert_eq!(
        err(&send(&app, Request::new(Method::Post, "/notes")).await).code(),
        "invalid_request"
    );

    let r = send(
        &app,
        Request::new(Method::Get, "/notes?limit=2&pinned=true"),
    )
    .await;
    assert_eq!(
        json(&r),
        r#"{"data":[{"text":"q","pinned":true},{"text":"q","pinned":true}],"page":{"nextCursor":null,"hasMore":false}}"#
    );
    let e = err(&send(&app, Request::new(Method::Get, "/notes?limit=many")).await).clone();
    assert_eq!(e.code(), "invalid_request");
    assert!(e.message().contains("`limit`"), "{e:?}");

    let id = Id::<Note>::new();
    let r = send(&app, Request::new(Method::Get, format!("/notes/{id}"))).await;
    assert_eq!(from_json_data::<String>(json(&r)).unwrap(), id.to_string());
    assert_eq!(
        err(&send(&app, Request::new(Method::Get, "/notes/not-an-id")).await).code(),
        "invalid_request"
    );

    // Internal errors keep their message server-side and carry the request ID.
    let r = send(&app, Request::new(Method::Get, "/boom")).await;
    let e = err(&r);
    assert_eq!(
        (e.code(), e.message()),
        ("internal_error", "internal error")
    );
    assert_eq!(e.request_id(), Some(&r.request_id));

    let r = send(&app, Request::new(Method::Get, "/scope")).await;
    assert_eq!(
        json(&r),
        r#"{"data":true}"#,
        "handlers run inside their request scope"
    );
}

async fn slow(gate: Ctx<Gate>) -> HyveResult<bool> {
    let scope = Scope::current().expect("request scope");
    gate.0.notified().await;
    Ok(scope.cancellation().is_cancelled())
}

struct Gate(tokio_like::Notify);

#[hyve::test]
async fn shutdown_stops_new_requests_and_waits_for_running_ones() {
    let app = OrbicApp::new()
        .provide(Gate(tokio_like::Notify))
        .route(Method::Get, "/slow", slow)
        .build()
        .unwrap();
    let running = app.dispatch(Request::new(Method::Get, "/slow"));
    let running = std::pin::pin!(running);
    // Start the request: it parks on the gate.
    let mut running = running;
    assert!(futures_poll_once(running.as_mut()).is_none());
    assert_eq!(app.in_flight(), 1);

    app.begin_shutdown();
    let r = send(&app, Request::new(Method::Get, "/slow")).await;
    let e = err(&r);
    assert_eq!(e.code(), "service_unavailable");
    assert!(e.is_retryable());

    let idle = app.idle();
    let mut idle = std::pin::pin!(idle);
    assert!(
        futures_poll_once(idle.as_mut()).is_none(),
        "a request is still running"
    );
    app.cancel_in_flight();
    tokio_like::notify_all();
    let r = running.await;
    assert_eq!(
        json(&r),
        r#"{"data":true}"#,
        "the running request saw the cancellation"
    );
    idle.await;
    assert_eq!(app.in_flight(), 0);
}

/// Provides the bearer token as the principal (a stand-in for `AuthGuard`).
pub struct BearerPrincipal;

impl Guard for BearerPrincipal {
    fn provides(&self) -> Vec<ContextNeed> {
        vec![ContextNeed::of::<orbic_core::Principal>()]
    }

    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>> {
        Box::pin(async move {
            if let Some(orbic_core::Credential::Bearer(t)) = &cx.meta.credential {
                cx.scope.provide(orbic_core::Principal::new(t.clone()));
            }
            Ok(())
        })
    }
}

/// Polls once with a no-op waker.
fn futures_poll_once<F: std::future::Future>(f: std::pin::Pin<&mut F>) -> Option<F::Output> {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match f.poll(&mut cx) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    }
}

/// A minimal one-shot notification shared by all `Notify`s (enough for one test).
mod tokio_like {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Waker;

    static OPEN: AtomicBool = AtomicBool::new(false);
    static WAITERS: Mutex<Vec<Waker>> = Mutex::new(Vec::new());

    pub struct Notify;

    impl Notify {
        pub fn notified(&self) -> impl std::future::Future<Output = ()> + Send {
            std::future::poll_fn(|cx| {
                if OPEN.load(Ordering::SeqCst) {
                    std::task::Poll::Ready(())
                } else {
                    WAITERS.lock().unwrap().push(cx.waker().clone());
                    std::task::Poll::Pending
                }
            })
        }
    }

    pub fn notify_all() {
        OPEN.store(true, Ordering::SeqCst);
        for w in WAITERS.lock().unwrap().drain(..) {
            w.wake();
        }
    }
}

#[cfg(feature = "server")]
mod server_functions {
    use hyve::api::{ServerFn, StubFn};
    use hyve::serde::Binary;
    use orbic_core::{Body, Rail};
    use orbic_tests::{
        __hyve_request_rename_note, Store, add_note, delete_note, find_notes, get_note, rename_note,
    };

    use super::*;

    fn d<F: StubFn<A>, A>(_: F) -> &'static hyve::api::ServerFnDescriptor {
        <F::Marker as ServerFn>::descriptor()
    }

    fn app() -> Pipeline {
        OrbicApp::new()
            .provide(orbic_cache::Cache::in_memory())
            .wrap(BearerPrincipal)
            .provide(Store {
                prefix: "> ".into(),
            })
            .mount(
                "/v1",
                component()
                    .handle(add_note)
                    .handle(get_note)
                    .handle(find_notes)
                    .handle(rename_note)
                    .handle(delete_note)
                    .post("/plain", create),
            )
            .build()
            .unwrap()
    }

    fn fast(path: &str, id: &str, fingerprint: u64, body: Vec<u8>) -> Request {
        let mut r = Request::new(Method::Post, path);
        r.rail = Rail::Fast;
        r.body = Body::Bytes(body);
        r.meta.server_fn = Some((id.to_owned(), fingerprint));
        r
    }

    #[test]
    fn missing_server_context_fails_assembly() {
        let e = OrbicApp::new()
            .mount("/v1", component().handle(add_note))
            .build()
            .err()
            .expect("Store is missing");
        assert!(
            e.to_string().contains(
                "POST /v1/notes: `orbic_tests::add_note` takes `Ctx<orbic_tests::Store>`"
            ),
            "{e}"
        );
    }

    #[hyve::test]
    async fn standard_rail_uses_the_bound_method_and_path() {
        let app = app();
        let note = Note {
            text: "hi".into(),
            pinned: false,
        };
        let r = send(
            &app,
            Request::new(Method::Post, "/v1/notes").json(to_json(&note)),
        )
        .await;
        assert_eq!(json(&r), r#"{"data":{"text":"> hi","pinned":false}}"#);

        let r = send(
            &app,
            Request::new(Method::Get, "/v1/notes?limit=2&pinned=true"),
        )
        .await;
        let page: Page<Note> = from_json_data(json(&r)).unwrap();
        assert_eq!(
            (
                page.items.len(),
                page.items[0].pinned,
                page.next_cursor.as_deref()
            ),
            (2, true, Some("next"))
        );

        let id = Id::<Note>::new();
        assert_eq!(
            err(&send(&app, Request::new(Method::Get, format!("/v1/notes/{id}"))).await).code(),
            "note_not_found"
        );
        let r = send(
            &app,
            Request::new(Method::Patch, format!("/v1/notes/{id}"))
                .json(format!(r#"{{"id":"{id}","text":"new"}}"#)),
        )
        .await;
        assert_eq!(json(&r), r#"{"data":{"text":"new","pinned":false}}"#);
        let r = send(
            &app,
            Request::new(Method::Delete, format!("/v1/notes/{id}")),
        )
        .await;
        assert!(
            matches!(r.outcome, Ok(Payload::Empty)),
            "() is a response without a value"
        );

        // Server-side validation of `Valid<T>`.
        let r = send(
            &app,
            Request::new(Method::Post, "/v1/notes").json(r#"{"text":"","pinned":false}"#),
        )
        .await;
        assert_eq!(err(&r).code(), "invalid_request");
    }

    #[hyve::test]
    async fn fast_rail_checks_function_and_schema_before_guards() {
        let app = app();
        let add = d(add_note);
        let note = Note {
            text: "hi".into(),
            pinned: true,
        };
        let r = send(
            &app,
            fast(
                "/v1/notes",
                add.id,
                add.fingerprint,
                note.to_binary().unwrap().to_vec(),
            ),
        )
        .await;
        let Ok(Payload::Binary(bytes)) = &r.outcome else {
            panic!("{:?}", r.outcome)
        };
        assert_eq!(Note::from_binary(bytes).unwrap().text, "> hi");

        let e =
            err(&send(&app, fast("/v1/notes", add.id, add.fingerprint ^ 1, vec![])).await).clone();
        assert_eq!(e.code(), "schema_mismatch");
        assert_eq!(
            err(&send(&app, fast("/v1/notes", "nope", 0, vec![])).await).code(),
            "unsupported_media_type"
        );
        assert_eq!(
            err(&send(&app, fast("/v1/plain", "x", 0, vec![])).await).code(),
            "unsupported_media_type"
        );

        // Errors on the fast rail are the same public errors.
        let taken = Note {
            text: "taken".into(),
            pinned: false,
        };
        let r = send(
            &app,
            fast(
                "/v1/notes",
                add.id,
                add.fingerprint,
                taken.to_binary().unwrap().to_vec(),
            ),
        )
        .await;
        assert_eq!(err(&r).code(), "note_taken");

        // GET-bound functions travel as POST with a binary body.
        let get = d(get_note);
        let r = send(
            &app,
            fast(
                &format!("/v1/notes/{}", Id::<Note>::new()),
                get.id,
                get.fingerprint,
                Id::<Note>::new().to_binary().unwrap().to_vec(),
            ),
        )
        .await;
        assert_eq!(err(&r).code(), "note_not_found");
    }

    /// Same key on the other rail: the media type is part of "the same body".
    #[hyve::test]
    async fn an_idempotency_key_is_bound_to_its_rail() {
        let app = app();
        let add = d(add_note);
        let note = Note {
            text: "hi".into(),
            pinned: true,
        };
        let keyed = |mut r: Request| {
            r.meta.idempotency_key = Some("k1".into());
            r.meta.credential = Some(orbic_core::Credential::Bearer("ada".into()));
            r
        };
        let r = send(
            &app,
            keyed(fast(
                "/v1/notes",
                add.id,
                add.fingerprint,
                note.to_binary().unwrap().to_vec(),
            )),
        )
        .await;
        assert!(
            matches!(r.outcome, Ok(Payload::Binary(_))),
            "{:?}",
            r.outcome
        );
        let r = send(
            &app,
            keyed(fast(
                "/v1/notes",
                add.id,
                add.fingerprint,
                note.to_binary().unwrap().to_vec(),
            )),
        )
        .await;
        assert!(
            matches!(r.outcome, Ok(Payload::Binary(_))),
            "replayed: {:?}",
            r.outcome
        );
        let r = send(
            &app,
            keyed(Request::new(Method::Post, "/v1/notes").json(to_json(&note))),
        )
        .await;
        assert_eq!(err(&r).code(), "idempotency_key_mismatch");
    }

    #[hyve::test]
    async fn in_process_calls_skip_serialization() {
        let app = app();
        let rename = d(rename_note);
        let mut req = Request::new(Method::Patch, "");
        req.rail = Rail::InProcess;
        req.body = Body::Value(Box::new(__hyve_request_rename_note {
            id: Id::new(),
            text: "typed".into(),
        }));
        req.meta.server_fn = Some((rename.id.to_owned(), rename.fingerprint));
        let r = send(&app, req).await;
        let Ok(Payload::Value(v)) = r.outcome else {
            panic!("{:?}", r.outcome)
        };
        assert_eq!(v.downcast::<Note>().unwrap().text, "typed");

        let mut req = Request::new(Method::Get, "");
        req.rail = Rail::InProcess;
        req.meta.server_fn = Some(("unknown".into(), 0));
        assert_eq!(err(&send(&app, req).await).code(), "route_not_found");
    }

    #[test]
    fn routes_are_listed_with_their_contracts() {
        let app = app();
        let routes: Vec<_> = app
            .routes()
            .map(|r| (r.method, r.pattern.to_owned(), r.server_fn.is_some()))
            .collect();
        assert!(routes.contains(&(Method::Get, "/v1/notes/{id}".into(), true)));
        assert!(routes.contains(&(Method::Post, "/v1/plain".into(), false)));
        let _ = NoteFilter::default();
    }
}
