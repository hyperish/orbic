//! `orbic-db` on SQLite in memory and on a temporary PostgreSQL database
//! (`HYPERISH_TEST_POSTGRES_URL`; without it the PostgreSQL tests fail).
#![cfg(feature = "server")]

use std::time::Duration;

use hyve::prelude::*;
use orbic::__private::db::{Db, Dialect, testing::TempPostgres};
use orbic::sql;
use orbic_tests::{Event, Project, User};

fn project(owner: Id<User>, title: &str) -> Project {
    Project {
        id: Id::new(),
        owner_id: owner,
        title: title.into(),
        is_public: false,
        version: 1,
    }
}

async fn chaining(db: Db) {
    let (a, b) = (Id::<User>::new(), Id::<User>::new());
    let mut mine: Vec<Project> = ["one", "two", "three"]
        .iter()
        .map(|t| project(a, t))
        .collect();
    for p in &mine {
        db.insert(p.clone()).await.unwrap();
    }
    db.insert(project(b, "other")).await.unwrap();
    mine.sort_by_key(|p| p.id.to_string());

    let q = || db.query::<Project>().where_eq(Project::OWNER_ID, a);
    assert_eq!(q().count().await.unwrap(), 3);
    let first = q().cursor_after(None, 2).fetch_page().await.unwrap();
    assert_eq!(first.items, mine[..2]);
    let next = first.next_cursor.clone().expect("more rows");
    let second = q().cursor_after(Some(next), 2).fetch_page().await.unwrap();
    assert_eq!(
        (second.items.as_slice(), second.next_cursor),
        (&mine[2..], None)
    );

    // Optimistic locking: the update matches only the expected version.
    let p = &mine[0];
    let bump = |expected: i64| {
        db.update::<Project>()
            .set(Project::TITLE, "renamed".to_owned())
            .set(Project::VERSION, expected + 1)
            .where_eq(Project::ID, p.id)
            .where_eq(Project::VERSION, expected)
            .execute()
    };
    assert_eq!(bump(1).await.unwrap(), 1);
    assert_eq!(bump(1).await.unwrap(), 0, "stale expectedVersion");
    let got = db
        .query::<Project>()
        .where_eq(Project::ID, p.id)
        .fetch_one()
        .await
        .unwrap();
    assert_eq!((got.title.as_str(), got.version), ("renamed", 2));

    assert_eq!(
        db.delete::<Project>()
            .where_eq(Project::ID, p.id)
            .execute()
            .await
            .unwrap(),
        1
    );
    let e = db
        .query::<Project>()
        .where_eq(Project::ID, p.id)
        .fetch_one()
        .await
        .unwrap_err();
    assert_eq!(e.code(), "projects_not_found");
    let e = db.insert(mine[1].clone()).await.unwrap_err();
    assert_eq!(e.code(), "already_exists");
}

async fn queries_and_transactions(db: Db) {
    let owner = Id::<User>::new();
    // `docs/01-dx-and-dsl.md` §1 `create_project`.
    let created = db
        .tx(|tx| async move {
            sql!(
                Project,
                "INSERT INTO projects (id, owner_id, title, is_public, version)
             VALUES ($1, $2, $3, $4, 1) RETURNING *",
                Id::<Project>::new(),
                owner,
                String::from("made"),
                true
            )
            .fetch_one(tx)
            .await
        })
        .await
        .unwrap();
    assert_eq!(
        (created.title.as_str(), created.version, created.is_public),
        ("made", 1, true)
    );
    let all = sql!(Project, "SELECT * FROM projects WHERE owner_id = $1", owner)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(all, std::slice::from_ref(&created));
    let n = sql!(
        "UPDATE projects SET title = $1 WHERE id = $2",
        "x",
        created.id
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(n, 1);

    // A failed transaction leaves nothing and does not touch another's commit.
    let ok = project(owner, "kept");
    let bad = project(owner, "dropped");
    let (ok_id, bad_id) = (ok.id, bad.id);
    let (r1, r2) = tokio::join!(
        db.tx(|tx| async move {
            tx.insert(ok.clone()).await?;
            tokio::task::yield_now().await;
            Ok(())
        }),
        db.tx(|tx| async move {
            tx.insert(bad.clone()).await?;
            Err::<(), _>(HyveError::domain("nope", "rolled back"))
        }),
    );
    assert!(r1.is_ok());
    assert_eq!(r2.unwrap_err().code(), "nope");
    let exists = |id: Id<Project>| {
        db.query::<Project>()
            .where_eq(Project::ID, id)
            .fetch_optional()
    };
    assert!(exists(ok_id).await.unwrap().is_some());
    assert!(exists(bad_id).await.unwrap().is_none());

    // Cancelled before finishing: rolled back.
    let cancelled = project(owner, "cancelled");
    let cancelled_id = cancelled.id;
    let run = db.tx(|tx| async move {
        tx.insert(cancelled.clone()).await?;
        std::future::pending::<()>().await;
        Ok(())
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), run)
            .await
            .is_err()
    );
    assert!(exists(cancelled_id).await.unwrap().is_none());

    // The handle does not outlive its transaction.
    let mut leaked = None;
    db.tx(|tx| {
        leaked = Some(tx);
        async { Ok(()) }
    })
    .await
    .unwrap();
    let e = leaked
        .unwrap()
        .insert(project(owner, "late"))
        .await
        .unwrap_err();
    assert!(e.message().contains("already ended"), "{e:?}");
}

async fn types_round_trip(db: Db) {
    let at = Timestamp::from_unix_micros(1_764_000_000_123_456).unwrap();
    let events = [
        Event {
            id: Id::new(),
            at,
            score: Some(0.25),
            payload: vec![0, 1, 255],
            flag: Some(true),
        },
        Event {
            id: Id::new(),
            at: Timestamp::from_unix_micros(1).unwrap(),
            score: None,
            payload: vec![],
            flag: None,
        },
    ];
    for e in &events {
        db.insert(e.clone()).await.unwrap();
    }
    for e in &events {
        assert_eq!(
            &db.query::<Event>()
                .where_eq(Event::ID, e.id)
                .fetch_one()
                .await
                .unwrap(),
            e
        );
    }
    let later = db
        .query::<Event>()
        .where_gt(Event::AT, Timestamp::from_unix_micros(2).unwrap())
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(
        later,
        [events[0].clone()],
        "timestamps compare in time order"
    );
    assert_eq!(
        db.query::<Event>()
            .where_null(Event::SCORE)
            .count()
            .await
            .unwrap(),
        1
    );
}

async fn dialects(db: Db) {
    let q = || {
        sql!(
            Project,
            "SELECT * FROM projects WHERE title ILIKE $1",
            "%x%"
        )
    };
    match db.dialect() {
        Dialect::Postgres => assert!(q().fetch_all(&db).await.is_ok()),
        Dialect::Sqlite => {
            let e = q().fetch_all(&db).await.unwrap_err();
            assert!(
                e.message().contains("verified on postgres only")
                    && e.message().contains("not sent"),
                "{e:?}"
            );
        }
    }
}

macro_rules! on_both {
    ($($name:ident),*) => {
        mod sqlite {
            use super::*;
            $(
                #[hyve::test]
                async fn $name() {
                    super::$name(Db::in_memory().await).await;
                }
            )*
        }
        mod postgres {
            use super::*;
            $(
                #[hyve::test]
                async fn $name() {
                    let pg = TempPostgres::new().await;
                    super::$name(pg.db().clone()).await;
                }
            )*
        }
    };
}

on_both!(
    chaining,
    queries_and_transactions,
    types_round_trip,
    dialects
);

#[hyve::test]
async fn in_memory_databases_are_separate() {
    let (a, b) = (Db::in_memory().await, Db::in_memory().await);
    a.insert(project(Id::new(), "only in a")).await.unwrap();
    assert_eq!(a.query::<Project>().count().await.unwrap(), 1);
    assert_eq!(b.query::<Project>().count().await.unwrap(), 0);
}
