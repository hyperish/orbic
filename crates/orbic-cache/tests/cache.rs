//! `Cache`: Singleflight, TTL, tag invalidation and their races.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use hyve::prelude::*;
use orbic_cache::{Cache, CacheKey, CacheOpts, ManualClock, Tag};

fn poll<F: Future>(f: Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

/// A future that stays pending until the gate opens.
fn gate(open: &Arc<AtomicBool>) -> impl Future<Output = ()> + use<> {
    let open = open.clone();
    std::future::poll_fn(move |_| {
        if open.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

fn key(k: &str) -> CacheKey {
    CacheKey::new(k)
}

fn opts() -> CacheOpts {
    CacheOpts::ttl(Duration::from_secs(30)).tag(Tag::new("t"))
}

#[test]
fn concurrent_misses_load_once() {
    let cache = Cache::in_memory();
    let (calls, open) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let load = || {
        let (calls, wait) = (calls.clone(), gate(&open));
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            wait.await;
            Ok::<_, HyveError>(7u32)
        }
    };
    let mut a = pin!(cache.get_or_try_init(key("k"), opts(), load));
    let mut b = pin!(cache.get_or_try_init(key("k"), opts(), load));
    assert!(poll(a.as_mut()).is_pending());
    assert!(poll(b.as_mut()).is_pending());
    open.store(true, Ordering::SeqCst);
    assert_eq!(poll(a.as_mut()), Poll::Ready(Ok(7)));
    assert_eq!(poll(b.as_mut()), Poll::Ready(Ok(7)));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "Singleflight");
}

#[test]
fn zero_ttl_publishes_only_to_existing_waiters() {
    let cache = Cache::in_memory();
    let open = Arc::new(AtomicBool::new(false));
    let zero = || CacheOpts::ttl(Duration::ZERO);
    let mut first = pin!(cache.get_or_try_init(key("one"), zero(), || async {
        gate(&open).await;
        Ok(7u32)
    }));
    let mut waiter = pin!(cache.get_or_try_init(key("one"), zero(), || async { Ok(8u32) }));
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(waiter.as_mut()).is_pending());
    open.store(true, Ordering::Release);
    assert_eq!(poll(first.as_mut()), Poll::Ready(Ok(7)));
    let mut later = pin!(cache.get_or_try_init(key("one"), zero(), || async { Ok(9u32) }));
    assert_eq!(poll(later.as_mut()), Poll::Ready(Ok(9)));
    assert_eq!(poll(waiter.as_mut()), Poll::Ready(Ok(7)));
}

#[test]
fn a_waiter_takes_over_when_the_loader_goes_away() {
    let cache = Cache::in_memory();
    let never = Arc::new(AtomicBool::new(false));
    let mut a = Box::pin(cache.get_or_try_init(key("k"), opts(), || {
        let wait = gate(&never);
        async move {
            wait.await;
            Ok::<_, HyveError>(1u32)
        }
    }));
    let mut b =
        pin!(cache.get_or_try_init(key("k"), opts(), || async { Ok::<_, HyveError>(2u32) }));
    assert!(poll(a.as_mut()).is_pending());
    assert!(poll(b.as_mut()).is_pending(), "waits for the loader");
    drop(a); // cancelled
    assert_eq!(poll(b.as_mut()), Poll::Ready(Ok(2)));
}

#[hyve::test]
async fn errors_are_not_cached_and_entries_expire() {
    let time = ManualClock::new();
    let cache = Cache::in_memory_with(100, time.clock());
    let e = cache
        .get_or_try_init(key("k"), opts(), || async {
            Err::<u32, _>(HyveError::infra("db_down", "down"))
        })
        .await;
    assert_eq!(e.unwrap_err().code(), "db_down");
    assert_eq!(
        cache
            .get_or_try_init(key("k"), opts(), || async { Ok(1u32) })
            .await,
        Ok(1)
    );
    assert_eq!(
        cache
            .get_or_try_init(key("k"), opts(), || async { Ok(2u32) })
            .await,
        Ok(1),
        "cached"
    );
    time.advance(Duration::from_secs(30));
    assert_eq!(
        cache
            .get_or_try_init(key("k"), opts(), || async { Ok(3u32) })
            .await,
        Ok(3),
        "expired"
    );

    // Zero TTL stores nothing.
    let zero = || CacheOpts::ttl(Duration::ZERO);
    assert_eq!(
        cache
            .get_or_try_init(key("z"), zero(), || async { Ok(1u32) })
            .await,
        Ok(1)
    );
    assert_eq!(
        cache
            .get_or_try_init(key("z"), zero(), || async { Ok(2u32) })
            .await,
        Ok(2)
    );

    // One key, two types: an error that names both, not a wrong value.
    let e = cache
        .get_or_try_init(key("k"), opts(), || async { Ok(String::new()) })
        .await
        .unwrap_err();
    assert!(
        e.message().contains("`u32`") && e.message().contains("String"),
        "{e:?}"
    );

    cache.invalidate(&key("k")).await.unwrap();
    assert_eq!(
        cache
            .get_or_try_init(key("k"), opts(), || async { Ok(4u32) })
            .await,
        Ok(4)
    );
}

#[hyve::test]
async fn invalidating_a_tag_drops_its_entries() {
    let cache = Cache::in_memory();
    let other = || CacheOpts::ttl(Duration::from_secs(30)).tag(Tag::new("other"));
    cache
        .get_or_try_init(key("a"), opts(), || async { Ok(1u32) })
        .await
        .unwrap();
    cache
        .get_or_try_init(key("b"), other(), || async { Ok(1u32) })
        .await
        .unwrap();
    cache.invalidate_tag(Tag::new("t")).await.unwrap();
    assert_eq!(
        cache
            .get_or_try_init(key("a"), opts(), || async { Ok(2u32) })
            .await,
        Ok(2)
    );
    assert_eq!(
        cache
            .get_or_try_init(key("b"), other(), || async { Ok(2u32) })
            .await,
        Ok(1)
    );
}

/// The sketch's flow: `list_projects` is loading when `create_project`
/// invalidates the tag. The value loaded before the invalidation must not be
/// served after it.
#[test]
fn a_value_loaded_before_an_invalidation_is_not_served_after_it() {
    let cache = Cache::in_memory();
    let open = Arc::new(AtomicBool::new(false));
    let mut list = Box::pin(cache.get_or_try_init(key("projects"), opts(), || {
        let wait = gate(&open);
        async move {
            wait.await;
            Ok::<_, HyveError>(vec![String::from("old")])
        }
    }));
    assert!(poll(list.as_mut()).is_pending());
    // create_project commits and invalidates while the list is loading.
    assert!(poll(pin!(cache.invalidate_tag(Tag::new("t")))).is_ready());
    open.store(true, Ordering::SeqCst);
    assert_eq!(
        poll(list.as_mut()),
        Poll::Ready(Ok(vec!["old".into()])),
        "that caller still gets its own read"
    );

    let mut again = pin!(cache.get_or_try_init(key("projects"), opts(), || async {
        Ok(vec![String::from("old"), "new".into()])
    }));
    assert_eq!(
        poll(again.as_mut()),
        Poll::Ready(Ok(vec!["old".into(), "new".into()])),
        "a later read sees the change"
    );
}
