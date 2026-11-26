//! Real service-to-service calls and transactional event delivery.
#![cfg(feature = "msa")]
use hyve::{
    prelude::*,
    trace::{Kind, TraceContext, Tracer},
};
use orbic::{
    AuthConfig, AuthGuard, Cache, Client, Db, OrbicApp, SessionUser, component,
    msa::{EventBus, Service},
};
use orbic_db::{Sql, Tx, bind, projection, testing::TempPostgres};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

#[model]
#[derive(Clone)]
struct Account {
    name: String,
}
impl SessionUser for Account {
    fn principal(&self) -> String {
        self.name.clone()
    }
}
struct Replica(&'static str);
#[server(POST "/who")]
async fn who() -> HyveResult<String> {
    identity().await
}
async fn identity() -> HyveResult<String> {
    let scope = Scope::current().unwrap();
    Ok(format!(
        "{}:{}",
        scope.require::<Replica>()?.0,
        scope.require::<orbic::Principal>()?.as_str()
    ))
}
async fn relay() -> HyveResult<String> {
    let scope = Scope::current().unwrap().child();
    let service = scope.require::<Service>()?;
    hyve::api::provide_transport(&scope, Ctx::arc(&service).clone());
    scope.scoped(async { who().await }).await
}
async fn listen(app: OrbicApp) -> (String, oneshot::Sender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, done) = oneshot::channel();
    let task = tokio::spawn(async {
        orbic::serve(app, listener, async {
            let _ = done.await;
        })
        .await
        .unwrap();
    });
    (url, stop, task)
}

#[tokio::test]
async fn guarded_rpc_preserves_trace_across_replicas_fallback_and_health_changes() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let records = events.clone();
    let sink = Tracer::new(move |record| {
        if record.kind == Kind::Open {
            records.lock().unwrap().push(record);
        }
    });
    let cache = Cache::in_memory();
    let token = orbic_auth::issue(
        &cache,
        &AuthConfig::default(),
        &Account {
            name: "gateway".into(),
        },
        Client::Bearer,
    )
    .unwrap()
    .session;
    let app = |name, route| {
        OrbicApp::new()
            .provide(sink.clone())
            .provide(cache.clone())
            .provide(Replica(name))
            .mount("/v1", route)
    };
    let guarded = || component().wrap(AuthGuard::<Account>::require_user());
    let (first, stop_a, task_a) = listen(app("A", guarded().handle(who))).await;
    let (second, stop_b, task_b) = listen(app("B", guarded().post("/who", identity))).await;
    let (a, b) = (format!("{first}/v1"), format!("{second}/v1"));
    let directory = Service::discover(&[&a, &b]).await.unwrap();
    let scope = Scope::root();
    hyve::api::provide_transport(&scope, Arc::new(directory.clone()));
    assert_eq!(
        scope
            .scoped(async { who().await })
            .await
            .unwrap_err()
            .code(),
        "unauthenticated"
    );
    let authenticated = directory.bearer(&token).unwrap();
    let (front, stop_front, task_front) = listen(
        OrbicApp::new()
            .provide(sink)
            .provide(cache)
            .provide(authenticated.clone())
            .mount("/v1", component().get("/relay", relay)),
    )
    .await;
    let caller = reqwest::Client::new();
    let request = || {
        caller
            .get(format!("{front}/v1/relay"))
            .header("x-request-id", "rpc-integration")
            .header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
    };
    let mut answers = Vec::new();
    for _ in 0..2 {
        let response = request().send().await.unwrap();
        assert_eq!(response.status(), 200);
        answers.push(response.text().await.unwrap());
    }
    assert!(answers.iter().any(|a| a.contains("A:gateway")));
    assert!(answers.iter().any(|a| a.contains("B:gateway")));
    {
        let records = events.lock().unwrap();
        let calls: Vec<_> = records
            .iter()
            .filter(|r| r.context.request_id.as_str() == "rpc-integration")
            .collect();
        assert_eq!(calls.len(), 4);
        for pair in calls.as_chunks::<2>().0 {
            assert_eq!(
                pair[0].context.trace_id.to_string(),
                "4bf92f3577b34da6a3ce929d0e0e4736"
            );
            assert_eq!(
                pair[0].context.parent.unwrap().to_u128(),
                0x00f067aa0ba902b7
            );
            assert_eq!(pair[1].context.trace_id, pair[0].context.trace_id);
            assert_eq!(pair[1].context.parent, Some(pair[0].context.span_id));
        }
    }
    stop_a.send(()).unwrap();
    task_a.await.unwrap();
    assert_eq!(authenticated.probe().await, 1);
    assert!(
        request()
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("B:gateway")
    );
    assert!(authenticated.replace(&["http://example.invalid"]).is_err());
    authenticated.replace(&[]).unwrap();
    assert_eq!(request().send().await.unwrap().status(), 503);
    authenticated.replace(&[&b]).unwrap();
    let shutdown = hyve::CancellationToken::new();
    let monitor = tokio::spawn({
        let s = authenticated.clone();
        let token = shutdown.clone();
        async move { s.monitor(Duration::from_millis(15), token).await }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if request().send().await.unwrap().status() == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    shutdown.cancel();
    monitor.await.unwrap().unwrap();
    stop_front.send(()).unwrap();
    stop_b.send(()).unwrap();
    task_front.await.unwrap();
    task_b.await.unwrap();
}

projection!(Total, "msa_projection", total: i64);
async fn increment(tx: &Tx, seq: i64) -> HyveResult<()> {
    Sql::<()>::new(
        "UPDATE msa_projection SET total=total+$1",
        &["sqlite", "postgres"],
        &[],
        vec![bind(&seq)],
    )
    .execute(tx)
    .await?;
    Ok(())
}

async fn event_groups(db: Db) {
    db.execute_script(
        "CREATE TABLE msa_projection(total BIGINT NOT NULL); INSERT INTO msa_projection VALUES(0)",
    )
    .await
    .unwrap();
    let a = EventBus::new(db.clone()).await.unwrap();
    let b = EventBus::new(db.clone()).await.unwrap();
    let value = Value::Int(17);
    let aborted: HyveResult<()> = db
        .tx(|tx| async move {
            increment(&tx, 99).await?;
            EventBus::publish_in(&tx, "notes", "api", "aborted", &Value::Null).await?;
            Err(HyveError::domain("aborted", "roll back"))
        })
        .await;
    assert!(aborted.is_err());
    let scope = Scope::root();
    let parent = hyve::trace::Span::request(&scope, hyve::RequestId::new(), "create note");
    let trace = parent.scope().get::<TraceContext>().unwrap();
    let (x, y) = parent
        .scope()
        .scoped(async {
            tokio::join!(
                a.publish("notes", "api", "x", &value),
                b.publish("notes", "api", "x", &value)
            )
        })
        .await;
    assert_eq!((x.unwrap(), y.unwrap()), (1, 1));
    assert_eq!(
        a.publish("notes", "api", "x", &Value::Null)
            .await
            .unwrap_err()
            .code(),
        "idempotency_key_mismatch"
    );
    assert_eq!(b.publish("notes", "api", "y", &value).await.unwrap(), 2);
    let rejected = a
        .consume("notes", "search", |tx, e| async move {
            increment(&tx, e.seq).await?;
            Err(HyveError::domain("temporary", "abort consumer"))
        })
        .await;
    assert!(rejected.is_err());
    let handler = |tx, e: orbic::msa::ServiceEvent| {
        let trace = trace.clone();
        async move {
            if e.seq == 1 {
                assert_eq!(TraceContext::current().unwrap().parent, Some(trace.span_id));
            }
            assert_eq!(e.payload, Value::Int(17));
            increment(&tx, e.seq).await
        }
    };
    let (x, y) = tokio::join!(
        a.consume("notes", "search", handler),
        b.consume("notes", "search", handler)
    );
    let mut seqs = [x.unwrap().unwrap(), y.unwrap().unwrap()];
    seqs.sort();
    assert_eq!(seqs, [1, 2]);
    assert!(
        a.consume("notes", "search", |_, _| async {
            panic!("duplicate delivery")
        })
        .await
        .unwrap()
        .is_none()
    );
    for seq in 1..=2 {
        assert_eq!(
            b.consume("notes", "mail", |_, _| async { Ok(()) })
                .await
                .unwrap(),
            Some(seq)
        );
    }
    let total = Sql::<Total>::new(
        "SELECT total FROM msa_projection",
        &["sqlite", "postgres"],
        &[],
        vec![],
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(total.total, 3);
}
#[tokio::test]
async fn sqlite_outbox_and_offsets_share_app_transactions() {
    event_groups(Db::empty_in_memory().await).await;
}
#[tokio::test]
async fn postgres_outbox_and_offsets_share_app_transactions() {
    let pg = TempPostgres::new().await;
    event_groups(pg.db().clone()).await;
    pg.close().await;
}
