use super::*;
use orbic_cache::SessionWrite;
use orbic_db::testing::TempPostgres;
use std::time::Duration;

async fn concurrent(db: Db) {
    let cache = Cache::in_memory();
    let a = Rooms::new(db.clone(), cache.clone(), 2).await.unwrap();
    let b = Rooms::new(db, cache.clone(), 2).await.unwrap();
    for hub in [&a, &b] {
        hub.define("notes", |_, _, cmd| {
            Ok(Change {
                state: cmd.payload.clone(),
                kind: "note.created".into(),
                payload: cmd.payload.clone(),
            })
        })
        .unwrap();
    }
    cache
        .create_sessions(
            "family",
            &[SessionWrite {
                key: "test-session".into(),
                value: vec![1],
                ttl: Duration::from_secs(30),
            }],
        )
        .await
        .unwrap();
    let issue = || a.ticket("notes", "/1", "alice", "test-session", "family");
    let ticket = issue().await.unwrap();
    let (one, two) = tokio::join!(a.consume(&ticket), b.consume(&ticket));
    assert_ne!(one.is_ok(), two.is_ok());
    let grant = one.or(two).unwrap();
    let cmd = Command::new("note.create", Value::Int(19));
    let (left, right) = tokio::join!(a.apply(&grant, cmd.clone()), b.apply(&grant, cmd.clone()));
    let first = left.unwrap();
    assert_eq!(first, right.unwrap());
    let mut changed = cmd.clone();
    changed.payload = Value::Int(77);
    assert_eq!(
        a.apply(&grant, changed).await.unwrap_err().code(),
        "idempotency_mismatch"
    );
    let (left, right) = tokio::join!(
        a.apply(&grant, Command::new("note.create", Value::Int(42))),
        b.apply(&grant, Command::new("note.create", Value::Int(43)))
    );
    let mut numbers = [left.unwrap().seq, right.unwrap().seq];
    numbers.sort();
    assert_eq!(numbers, [2, 3]);
    assert_eq!(
        a.apply(&grant, cmd).await.unwrap(),
        first,
        "dedup survives pruning"
    );
    let (snapshot, head) = a.replay(&grant, 0).await.unwrap();
    assert_eq!(head, 3);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].kind, "room.snapshot");
    assert_eq!(snapshot[0].seq, 3);
    let expired = issue().await.unwrap();
    query::<()>(
        "UPDATE orbic_room_tickets SET expires=0 WHERE token_hash=$1",
        vec![bind(&credential_hash("room", &expired))],
    )
    .execute(&a.db)
    .await
    .unwrap();
    assert!(b.consume(&expired).await.is_err());
    cache.revoke_sessions("family").await.unwrap();
    assert!(b.authorize(&grant).await.is_err());
}

#[tokio::test]
async fn sqlite_room_commits_once_across_replicas() {
    concurrent(Db::in_memory().await).await;
}

#[tokio::test]
async fn postgres_room_commits_once_across_replicas() {
    let pg = TempPostgres::new().await;
    concurrent(pg.db().clone()).await;
    pg.close().await;
}
