//! Serving starts component timers, while TestClient stays deterministic.
#![cfg(feature = "jobs")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use hyve::{HyveResult, Scope, Value};
use orbic::{
    Cache, Db, OrbicApp, TestClient, component,
    jobs::{Job, JobQueue},
};

#[tokio::test]
async fn component_timer_runs_only_in_the_server_with_its_dependencies() {
    let queue = JobQueue::new(Db::in_memory().await).await.unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    async fn task() -> HyveResult<()> {
        let scope = Scope::current().unwrap();
        assert_eq!(scope.require::<Job>()?.payload, Value::Null);
        scope
            .require::<Arc<AtomicUsize>>()?
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    let app = || {
        OrbicApp::new()
            .provide(Cache::in_memory())
            .provide(queue.clone())
            .mount(
                "/notes",
                component().provide(count.clone()).every("30ms", task),
            )
    };
    let client = TestClient::new(app());
    assert_eq!(client.pipeline().schedules().len(), 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let shutdown = async {
        while count.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        orbic::serve(app(), socket, shutdown),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert!(
        OrbicApp::new()
            .mount("/notes", component().every("xx", task))
            .build()
            .is_err()
    );
}
