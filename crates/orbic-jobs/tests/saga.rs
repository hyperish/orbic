//! Durable compensation, lease fencing and external outcome reconciliation.
use hyve::{Scope, Ulid, Value};
use orbic_db::{Db, testing::TempPostgres};
use orbic_jobs::{
    JobQueue,
    saga::{Outcome, Resolution, State, Step},
};
use std::time::Duration;

fn step(apply: &str, undo: Option<&str>) -> Step {
    Step {
        apply: apply.into(),
        undo: undo.map(str::to_owned),
    }
}
fn checkout() -> Vec<Step> {
    vec![
        step("reserve", Some("release")),
        step("bill", Some("refund")),
        step("ship", Some("recall")),
    ]
}

#[tokio::test]
async fn sqlite_saga() {
    exercise(Db::in_memory().await).await;
}
#[tokio::test]
async fn postgres_saga() {
    let pg = TempPostgres::new().await;
    exercise(pg.db().clone()).await;
    pg.close().await;
}
async fn exercise(db: Db) {
    let a = JobQueue::new(db.clone()).await.unwrap().sagas();
    let b = JobQueue::new(db.clone()).await.unwrap().sagas();
    let id = Ulid::new();
    let input = Value::from("order-15");
    let lease = Duration::from_secs(2);
    let (one, two) = tokio::join!(
        a.start(id, checkout(), input.clone(), 2),
        b.start(id, checkout(), input.clone(), 2)
    );
    one.unwrap();
    two.unwrap();
    assert_eq!(
        a.start(id, checkout(), Value::Null, 2)
            .await
            .unwrap_err()
            .code(),
        "saga_conflict"
    );
    let (one, two) = tokio::join!(a.claim("reserve", lease), b.claim("reserve", lease));
    let mut claims: Vec<_> = [one.unwrap(), two.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    let claim = claims.pop().unwrap();
    assert_eq!(claim.work().input, input);
    assert_eq!(claim.work().key, format!("{id}/0/apply"));
    a.finish(&claim, Outcome::Applied).await.unwrap();
    assert_eq!(
        b.finish(&claim, Outcome::Applied).await.unwrap_err().code(),
        "job_lease_lost"
    );
    // A worker disappeared after calling the billing service. The new claim
    // must use the original operation key for the service's idempotent write.
    let gone = b
        .claim("bill", Duration::from_millis(40))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(75)).await;
    let resumed = JobQueue::new(db.clone()).await.unwrap().sagas();
    let again = resumed.claim("bill", lease).await.unwrap().unwrap();
    assert_eq!(again.work().key, gone.work().key);
    assert!(b.renew(&gone).await.is_err());
    resumed.finish(&again, Outcome::Applied).await.unwrap();
    let shipping = a.claim("ship", lease).await.unwrap().unwrap();
    a.finish(&shipping, Outcome::Rejected("out of stock".into()))
        .await
        .unwrap();
    assert!(a.claim("release", lease).await.unwrap().is_none());
    for (op, key) in [
        ("refund", format!("{id}/1/undo")),
        ("release", format!("{id}/0/undo")),
    ] {
        let work = b.claim(op, lease).await.unwrap().unwrap();
        assert_eq!(work.work().key, key);
        assert!(work.work().compensating);
        b.finish(&work, Outcome::Applied).await.unwrap();
    }
    assert_eq!(a.status(id).await.unwrap().state, State::Compensated);

    let id = Ulid::new();
    a.start(id, vec![step("issue", Some("void"))], Value::Null, 1)
        .await
        .unwrap();
    let writing = a.claim("issue", lease).await.unwrap().unwrap();
    b.cancel(id).await.unwrap();
    assert!(a.finish(&writing, Outcome::Applied).await.is_err());
    let uncertain = a.status(id).await.unwrap();
    assert_eq!(uncertain.state, State::Reconcile);
    assert!(uncertain.cancellation_requested);
    b.resolve(id, uncertain.revision, Resolution::Applied)
        .await
        .unwrap();
    assert!(
        b.resolve(id, uncertain.revision, Resolution::Applied)
            .await
            .is_err()
    );
    assert!(
        a.run_once("void", lease, &Scope::root(), |work| async move {
            assert_eq!(work.key, format!("{id}/0/undo"));
            Outcome::Applied
        })
        .await
        .unwrap()
    );
    assert_eq!(a.status(id).await.unwrap().state, State::Compensated);

    let id = Ulid::new();
    a.start(
        id,
        vec![step("irreversible", None), step("next", None)],
        Value::Null,
        1,
    )
    .await
    .unwrap();
    let write = a.claim("irreversible", lease).await.unwrap().unwrap();
    a.finish(&write, Outcome::Applied).await.unwrap();
    b.cancel(id).await.unwrap();
    let manual = b.status(id).await.unwrap();
    assert_eq!(manual.state, State::Reconcile);
    assert!(manual.operation.is_none() && manual.compensating);
    b.resolve(id, manual.revision, Resolution::Applied)
        .await
        .unwrap();
    assert_eq!(a.status(id).await.unwrap().state, State::Compensated);

    let id = Ulid::new();
    a.start(id, vec![step("accept", None)], Value::Null, 1)
        .await
        .unwrap();
    let lost = a
        .claim("accept", Duration::from_millis(30))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(55)).await;
    let check = b.status(id).await.unwrap();
    assert_eq!(check.state, State::Reconcile);
    assert!(a.finish(&lost, Outcome::Applied).await.is_err());
    b.resolve(id, check.revision, Resolution::Applied)
        .await
        .unwrap();
    a.cancel(id).await.unwrap();
    assert_eq!(a.status(id).await.unwrap().state, State::Succeeded);
}
