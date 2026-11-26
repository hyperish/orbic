//! Two independent clients using a private shared cache.
#![cfg(feature = "redis")]
use hyve::{HyveError, Ulid};
use orbic_cache::{Cache, CacheKey, CacheOpts, IdempotencyKey, RequestBegin, Tag};
use std::time::Duration;

#[tokio::test]
#[ignore = "needs a dedicated Redis/Valkey service; scripts/test-cache.sh"]
async fn a_disconnected_replica_cannot_serve_revoked_state_and_recovers() {
    use orbic_cache::SessionWrite;
    use tokio::{
        io::copy_bidirectional,
        net::{TcpListener, TcpStream},
        sync::watch,
    };
    let target = std::env::var("HYPERISH_TEST_REDIS_ADDR").unwrap();
    let url = std::env::var("HYPERISH_TEST_REDIS_URL").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let via_proxy = url.replace(&target, &listener.local_addr().unwrap().to_string());
    let (switch, state) = watch::channel(true);
    let proxy = tokio::spawn(async move {
        while let Ok((mut incoming, _)) = listener.accept().await {
            let mut state = state.clone();
            if !*state.borrow_and_update() {
                continue;
            }
            let target = target.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = state.changed() => {}
                    _ = async {
                        let mut upstream = TcpStream::connect(target).await.unwrap();
                        let _ = copy_bidirectional(&mut incoming, &mut upstream).await;
                    } => {}
                }
            });
        }
    });
    let namespace = Ulid::new().to_string();
    let direct = Cache::tiered(&url, &namespace).await.unwrap();
    let replica = Cache::tiered(&via_proxy, &namespace).await.unwrap();
    let key = CacheKey::new("catalog");
    let ttl = Duration::from_secs(90);
    let opts = || CacheOpts::ttl(ttl);
    assert_eq!(
        replica
            .get_or_try_init(key.clone(), opts(), || async { Ok(1u32) })
            .await
            .unwrap(),
        1
    );
    direct
        .create_sessions(
            "owner",
            &[SessionWrite {
                key: "session-1".into(),
                value: vec![4],
                ttl,
            }],
        )
        .await
        .unwrap();
    assert!(replica.session_record("session-1").await.unwrap().is_some());
    switch.send(false).unwrap();
    direct.revoke_sessions("owner").await.unwrap();
    direct.invalidate(&key).await.unwrap();
    assert_eq!(
        direct
            .get_or_try_init(key.clone(), opts(), || async { Ok(2u32) })
            .await
            .unwrap(),
        2
    );
    let (session, cached) = tokio::join!(
        replica.session_record("session-1"),
        replica.get_or_try_init(key.clone(), opts(), || async { Ok(3u32) })
    );
    assert_eq!(session.unwrap_err().code(), "cache_unavailable");
    assert_eq!(cached.unwrap_err().code(), "cache_unavailable");
    switch.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(session) = replica.session_record("session-1").await {
                assert!(session.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("client reconnects to Valkey");
    assert_eq!(
        replica
            .get_or_try_init(key, opts(), || async { Ok(3u32) })
            .await
            .unwrap(),
        2
    );
    proxy.abort();
}

#[tokio::test]
#[ignore = "needs a dedicated Redis/Valkey service; scripts/test-cache.sh"]
async fn zero_ttl_hands_one_result_to_registered_peers_then_removes_it() {
    use fred::prelude::*;
    let url = std::env::var("HYPERISH_TEST_REDIS_URL").unwrap();
    let namespace = Ulid::new().to_string();
    let a = Cache::tiered(&url, &namespace).await.unwrap();
    let b = Cache::tiered(&url, &namespace).await.unwrap();
    let inspector = Builder::from_config(Config::from_url(&url).unwrap())
        .build()
        .unwrap();
    let driver = inspector.connect();
    inspector.wait_for_connect().await.unwrap();
    let data_key = format!("orbic:{{{namespace}}}:data:once");
    let (begin, begun) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let opts = || CacheOpts::ttl(Duration::ZERO);
    let key = CacheKey::new("once");
    let (leader, follower, revision) = tokio::join!(
        a.get_or_try_init(key.clone(), opts(), || async {
            begin.send(()).unwrap();
            released.await.unwrap();
            Ok(41u32)
        }),
        async {
            begun.await.unwrap();
            b.get_or_try_init(key.clone(), opts(), || async { Ok(78u32) })
                .await
        },
        async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while inspector
                    .hget::<Option<i64>, _, _>(&data_key, "readers")
                    .await
                    .unwrap()
                    != Some(1)
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("peer registered on the running load");
            let revision: String = inspector.hget(&data_key, "rev").await.unwrap();
            release.send(()).unwrap();
            revision
        }
    );
    assert_eq!((leader.unwrap(), follower.unwrap()), (41, 41));
    let handoff = format!("orbic:{{{namespace}}}:handoff:{revision}");
    assert_eq!(
        inspector
            .exists::<u32, _>([data_key, handoff].to_vec())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        b.get_or_try_init(key, opts(), || async { Ok(80u32) })
            .await
            .unwrap(),
        80
    );
    driver.abort();
}

#[tokio::test]
#[ignore = "needs a dedicated Redis/Valkey service; scripts/test-cache.sh"]
async fn independent_clients_share_loads_invalidation_limits_and_reservations() {
    let url = std::env::var("HYPERISH_TEST_REDIS_URL").unwrap();
    let namespace = Ulid::new().to_string();
    let a = Cache::tiered(&url, &namespace).await.unwrap();
    let b = Cache::tiered(&url, &namespace).await.unwrap();
    let key = CacheKey::new("items");
    let tag = Tag::new("projects");
    let opts = || CacheOpts::ttl(Duration::from_secs(20)).tag(tag.clone());
    let (begin, begun) = tokio::sync::oneshot::channel();
    let load = || async {
        begin.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        Ok(17u32)
    };
    let (left, right) = tokio::join!(a.get_or_try_init(key.clone(), opts(), load), async {
        begun.await.unwrap();
        b.get_or_try_init(key.clone(), opts(), || async { Ok(88u32) })
            .await
    });
    assert_eq!((left.unwrap(), right.unwrap()), (17, 17));
    assert!(
        b.get_or_try_init(key.clone(), opts(), || async { Ok(String::new()) })
            .await
            .is_err()
    );
    a.invalidate_tag(tag.clone()).await.unwrap();
    assert_eq!(
        b.get_or_try_init(key.clone(), opts(), || async { Ok(18u32) })
            .await
            .unwrap(),
        18
    );
    b.invalidate(&key).await.unwrap();

    let (start, started) = tokio::sync::oneshot::channel();
    let (finish, done) = tokio::sync::oneshot::channel();
    let pending = a.get_or_try_init(key.clone(), opts(), || async {
        start.send(()).unwrap();
        done.await.unwrap();
        Ok(19u32)
    });
    let (old, new) = tokio::join!(pending, async {
        started.await.unwrap();
        b.invalidate_tag(tag.clone()).await.unwrap();
        let fresh = b
            .get_or_try_init(key.clone(), opts(), || async { Ok(20u32) })
            .await
            .unwrap();
        finish.send(()).unwrap();
        fresh
    });
    assert_eq!((old.unwrap(), new), (19, 20));
    assert_eq!(
        a.get_or_try_init(key.clone(), opts(), || async {
            Err::<u32, _>(HyveError::internal("unexpected reload"))
        })
        .await
        .unwrap(),
        20
    );
    let window = Duration::from_secs(3);
    assert!(
        a.count_request("scope", "p", 1, window)
            .await
            .unwrap()
            .allowed
    );
    assert!(
        !b.count_request("scope", "p", 1, window)
            .await
            .unwrap()
            .allowed
    );
    let other = Cache::tiered(&url, &Ulid::new().to_string()).await.unwrap();
    assert!(
        other
            .count_request("scope", "p", 1, window)
            .await
            .unwrap()
            .allowed
    );
    let id = IdempotencyKey {
        principal: "alice".into(),
        method: "POST",
        path: "/notes".into(),
        key: "1".into(),
    };
    let RequestBegin::Started(r) = a.begin_request(id.clone(), 11).await.unwrap() else {
        panic!()
    };
    assert!(matches!(
        b.begin_request(id.clone(), 11).await.unwrap(),
        RequestBegin::InProgress
    ));
    assert!(matches!(
        b.begin_request(id.clone(), 12).await.unwrap(),
        RequestBegin::Mismatch
    ));
    r.finish(Some(vec![0, 255])).await.unwrap();
    assert!(
        matches!(b.begin_request(id,11).await.unwrap(),RequestBegin::Replay(bytes) if bytes==[0,255])
    );
}
