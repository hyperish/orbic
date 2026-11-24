//! `Cache`: the L1 in-memory cache (`quick_cache`, S3-FIFO) with Singleflight
//! and tag invalidation, plus the non-evicting stores guards and the pipeline
//! use (`docs/04-open-questions.md` 결정 2).

use std::any::{Any, type_name};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hyve_api::Wire;
use hyve_core::{HyveError, HyveResult};

use crate::clock::Clock;
use crate::idempotency::IdempotencyStore;
use crate::key::{CacheKey, Tag};
use crate::rate::RateLimitStore;
use crate::session::SessionStore;

/// Entries [`Cache::in_memory`] holds before evicting.
pub const DEFAULT_CAPACITY: usize = 10_000;

const MIN_PRUNE: usize = 1024;

/// How an entry is cached: `CacheOpts::ttl(ttl!("30s")).tag(tag!("projects", user.id))`.
#[derive(Debug, Clone)]
pub struct CacheOpts {
    ttl: Duration,
    tags: Vec<Tag>,
}

impl CacheOpts {
    /// Keeps the value for `ttl`, counted from when loading started. A zero
    /// TTL shares one load among concurrent callers but stores nothing.
    pub fn ttl(ttl: Duration) -> Self {
        CacheOpts {
            ttl,
            tags: Vec::new(),
        }
    }

    /// Adds a tag; [`Cache::invalidate_tag`] drops every entry carrying it.
    pub fn tag(mut self, tag: Tag) -> Self {
        self.tags.push(tag);
        self
    }
}

struct Entry {
    value: Arc<dyn Any + Send + Sync>,
    type_name: &'static str,
    expires: Instant,
    /// The invalidation sequence when loading started.
    loaded: u64,
    tags: Box<[Tag]>,
}

#[derive(Default)]
struct Tags {
    /// Tag -> (sequence of its last invalidation, when).
    invalidated: HashMap<Tag, (u64, Instant)>,
    max_ttl: Duration,
    prune_at: usize,
}

/// The app's cache and stores. Provide one (`.provide(Cache::in_memory())`);
/// routes that accept `Idempotency-Key` and guards such as `RateLimit` and
/// `AuthGuard` take their stores from it, and assembly fails without it.
/// Cheap to clone.
#[derive(Clone)]
pub struct Cache(Arc<Inner>);

struct Inner {
    l1: quick_cache::sync::Cache<CacheKey, Arc<Entry>>,
    tags: Mutex<Tags>,
    seq: AtomicU64,
    clock: Clock,
    sessions: SessionStore,
    idempotency: IdempotencyStore,
    rate_limits: RateLimitStore,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cache")
            .field("entries", &self.0.l1.len())
            .field("clock", &self.0.clock)
            .finish()
    }
}

impl Cache {
    /// An in-process cache of [`DEFAULT_CAPACITY`] entries on the system clock.
    pub fn in_memory() -> Self {
        Self::in_memory_with(DEFAULT_CAPACITY, Clock::system())
    }

    /// An in-process cache of `capacity` entries reading `clock`.
    pub fn in_memory_with(capacity: usize, clock: Clock) -> Self {
        Cache(Arc::new(Inner {
            l1: quick_cache::sync::Cache::new(capacity.max(1)),
            tags: Mutex::new(Tags::default()),
            seq: AtomicU64::new(0),
            sessions: SessionStore::new(clock.clone()),
            idempotency: IdempotencyStore::new(clock.clone()),
            rate_limits: RateLimitStore::new(clock.clone()),
            clock,
        }))
    }

    /// The cached value for `key`, or the result of `init`. Concurrent misses
    /// for one key run `init` once (Singleflight); if that caller fails or is
    /// cancelled, a waiting caller loads instead. Errors are never cached.
    pub async fn get_or_try_init<T, F, Fut>(
        &self,
        key: CacheKey,
        opts: CacheOpts,
        init: F,
    ) -> HyveResult<T>
    where
        T: Wire + Clone + Sync,
        F: FnOnce() -> Fut,
        Fut: Future<Output = HyveResult<T>>,
    {
        let inner = &self.0;
        loop {
            match inner.l1.get_value_or_guard_async(&key).await {
                Ok(entry) => {
                    if self.is_fresh(&entry) {
                        return read(&key, &entry);
                    }
                    inner.l1.remove_if(&key, |e| Arc::ptr_eq(e, &entry));
                }
                Err(guard) => {
                    let started = inner.clock.now();
                    let loaded = inner.seq.load(Ordering::SeqCst);
                    self.note_ttl(opts.ttl);
                    let value = init().await?;
                    if !opts.ttl.is_zero() {
                        let _ = guard.insert(Arc::new(Entry {
                            value: Arc::new(value.clone()),
                            type_name: type_name::<T>(),
                            expires: started + opts.ttl,
                            loaded,
                            tags: opts.tags.into(),
                        }));
                    }
                    return Ok(value);
                }
            }
        }
    }

    /// Drops every entry tagged `tag`, including values still loading.
    pub async fn invalidate_tag(&self, tag: Tag) -> HyveResult<()> {
        let inner = &self.0;
        let now = inner.clock.now();
        let mut tags = inner.tags.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        tags.invalidated.insert(tag, (seq, now));
        if tags.invalidated.len() >= tags.prune_at.max(MIN_PRUNE) {
            // An invalidation matters only while an entry loaded before it can
            // live, and entries live at most the longest TTL from load start.
            let keep = tags.max_ttl;
            tags.invalidated.retain(|_, (_, at)| *at + keep > now);
            tags.prune_at = tags.invalidated.len() * 2;
        }
        Ok(())
    }

    /// Drops the entry for `key`.
    pub async fn invalidate(&self, key: &CacheKey) -> HyveResult<()> {
        self.0.l1.remove(key);
        Ok(())
    }

    /// Session and refresh records (`orbic-auth`).
    pub fn sessions(&self) -> &SessionStore {
        &self.0.sessions
    }

    /// `Idempotency-Key` outcomes (the pipeline's gate).
    pub fn idempotency(&self) -> &IdempotencyStore {
        &self.0.idempotency
    }

    /// `RateLimit` counters.
    pub fn rate_limits(&self) -> &RateLimitStore {
        &self.0.rate_limits
    }

    /// The clock the cache and stores read.
    pub fn clock(&self) -> &Clock {
        &self.0.clock
    }

    fn note_ttl(&self, ttl: Duration) {
        let mut tags = self.0.tags.lock().unwrap_or_else(PoisonError::into_inner);
        tags.max_ttl = tags.max_ttl.max(ttl);
    }

    fn is_fresh(&self, e: &Entry) -> bool {
        if e.expires <= self.0.clock.now() {
            return false;
        }
        let tags = self.0.tags.lock().unwrap_or_else(PoisonError::into_inner);
        e.tags.iter().all(|t| {
            tags.invalidated
                .get(t)
                .is_none_or(|(seq, _)| *seq <= e.loaded)
        })
    }
}

fn read<T: Clone + 'static>(key: &CacheKey, e: &Entry) -> HyveResult<T> {
    e.value.downcast_ref::<T>().cloned().ok_or_else(|| {
        HyveError::internal(format!(
            "cache key `{key}` holds a `{}`, but was read as `{}`; give each value type its own key",
            e.type_name,
            type_name::<T>()
        ))
    })
}
