//! Correlation follows a real HTTP request into its handler and exporter.
#![cfg(feature = "server")]

use hyve::prelude::*;
use hyve::trace::{Kind, Level, TraceContext, Tracer, record};
use orbic::{Cache, OrbicApp, TestClient, component};
use std::sync::{Arc, Mutex};

async fn observed() -> HyveResult<String> {
    let context = TraceContext::current().unwrap();
    record("opened", Kind::Log(Level::Info), Value::from("note"));
    record("notes_read", Kind::Count(1), Value::Null);
    Ok(context.request_id.to_string())
}

#[hyve::test]
async fn request_id_reaches_structured_logs_and_metrics() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let records = events.clone();
    let client = TestClient::new(
        OrbicApp::new()
            .provide(Cache::in_memory())
            .provide(Tracer::new(move |r| records.lock().unwrap().push(r)))
            .mount("/v1", component().get("/notes", observed)),
    );
    let response = client
        .get("/v1/notes")
        .header("X-Request-Id", "trace-test-1")
        .send()
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.header("X-Request-Id"), Some("trace-test-1"));
    let records = events.lock().unwrap();
    let kinds: Vec<_> = records.iter().map(|r| r.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            Kind::Open,
            Kind::Log(Level::Info),
            Kind::Count(1),
            Kind::Close
        ]
    );
    assert_eq!(records[1].fields, Value::from("note"));
    assert_eq!(records[0].context.request_id.as_str(), "trace-test-1");
    assert!(records.iter().all(|r| r.context == records[0].context));
    assert!(TraceContext::current().is_none());
}
