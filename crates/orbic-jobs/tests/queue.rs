//! Worker and scheduler contracts on both supported database engines.

use std::time::Duration;

use hyve::{Scope, Value};
use orbic_db::{Db, testing::TempPostgres};
use orbic_jobs::{JobQueue, JobState, Outcome, Schedule};

#[tokio::test]
async fn sqlite_workers_and_scheduler() {
    exercise(Db::in_memory().await).await;
}

#[tokio::test]
async fn postgres_workers_and_scheduler() {
    let pg = TempPostgres::new().await;
    exercise(pg.db().clone()).await;
    pg.close().await;
}

async fn exercise(db: Db) {
    let a = JobQueue::new(db.clone()).await.unwrap();
    let b = JobQueue::new(db).await.unwrap();
    let lease = Duration::from_secs(3);
    let id = a
        .enqueue("mail", Value::from("hello"), Duration::from_millis(80), 2)
        .await
        .unwrap();
    assert!(b.claim("mail", lease).await.unwrap().is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (one, two) = tokio::join!(a.claim("mail", lease), b.claim("mail", lease));
    let mut claims: Vec<_> = [one.unwrap(), two.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    let first = claims.pop().unwrap();
    assert_eq!(first.job().payload, Value::from("hello"));
    a.renew(&first).await.unwrap();
    a.finish(&first, Outcome::Retry("temporary".into()))
        .await
        .unwrap();
    assert!(b.claim("mail", lease).await.unwrap().is_none());
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let last = b.claim("mail", lease).await.unwrap().unwrap();
    assert_eq!((last.job().id, last.job().attempt), (id, 2));
    assert_eq!(
        a.finish(&first, Outcome::Done).await.unwrap_err().code(),
        "job_lease_lost"
    );
    b.finish(&last, Outcome::Retry("still unavailable".into()))
        .await
        .unwrap();
    let dead = a.get(id).await.unwrap();
    assert_eq!(
        (dead.state, dead.last_error.as_deref()),
        (JobState::Dead, Some("still unavailable"))
    );

    let interval = Schedule::every("80ms").unwrap();
    assert!(
        a.tick("digest", &interval, "schedule", Value::Null, 1)
            .await
            .unwrap()
            .is_none()
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (one, two) = tokio::join!(
        a.tick("digest", &interval, "schedule", Value::Null, 1),
        b.tick("digest", &interval, "schedule", Value::Null, 1)
    );
    let ids: Vec<_> = [one.unwrap(), two.unwrap()].into_iter().flatten().collect();
    assert_eq!(ids.len(), 1);
    let scope = Scope::root();
    assert!(
        b.run_once("schedule", lease, &scope, |_| async { Outcome::Done })
            .await
            .unwrap()
    );
    assert_eq!(a.get(ids[0]).await.unwrap().state, JobState::Done);
    assert!(Schedule::cron("*/5 * * * *").is_ok());
    assert!(Schedule::cron("invalid").is_err());
    assert!(Schedule::every("0s").is_err());
}
