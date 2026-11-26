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
use quick_cache::sync::{EntryAction, EntryResult};

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
    #[cfg(feature = "redis")]
    revision: Option<String>,
    value: Arc<dyn Any + Send + Sync>,
    type_name: &'static str,
    /// None delivers only to callers already waiting on this load.
    expires: Option<Instant>,
    /// Last generation at which every tag was checked, initially the start of the load.
    validated: AtomicU64,
    tags: Box<[Tag]>,
}

#[derive(Clone)]
struct Admission;
impl quick_cache::Weighter<CacheKey, Arc<Entry>> for Admission {
    fn weight(&self, _: &CacheKey, entry: &Arc<Entry>) -> u64 {
        // quick_cache publishes to its placeholder's waiters before checking admission.
        if entry.expires.is_some() { 1 } else { u64::MAX }
    }
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
    #[cfg(feature = "redis")]
    redis: Option<Arc<crate::redis::Redis>>,
    #[cfg(feature = "redis")]
    listener: std::sync::OnceLock<tokio::task::AbortHandle>,
    l1: quick_cache::sync::Cache<CacheKey, Arc<Entry>, Admission>,
    tags: Mutex<Tags>,
    seq: AtomicU64,
    clock: Clock,
    sessions: SessionStore,
    idempotency: IdempotencyStore,
    rate_limits: RateLimitStore,
}

#[cfg(feature = "redis")]
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.get() {
            listener.abort();
        }
    }
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
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "redis")]
            listener: Default::default(),
            l1: quick_cache::sync::Cache::with_weighter(
                capacity.max(1),
                capacity.max(1) as u64,
                Admission,
            ),
            tags: Mutex::new(Tags::default()),
            seq: AtomicU64::new(0),
            sessions: SessionStore::new(clock.clone()),
            idempotency: IdempotencyStore::new(clock.clone()),
            rate_limits: RateLimitStore::new(clock.clone()),
            clock,
        }))
    }

    /// Connects independent replicas to one Redis/Valkey namespace.
    #[cfg(feature = "redis")]
    pub async fn tiered(url: &str, namespace: &str) -> HyveResult<Self> {
        let remote = crate::redis::Redis::connect(url, namespace).await?;
        let mut cache = Self::in_memory();
        Arc::get_mut(&mut cache.0).expect("new cache").redis = Some(remote.clone());
        let weak = Arc::downgrade(&cache.0);
        let task = remote
            .subscribe(move |message| {
                if let Some(inner) = weak.upgrade() {
                    let cache = Cache(inner);
                    match message.and_then(|m| m.split_once(':')) {
                        Some(("key", k)) => {
                            cache.0.l1.remove(&CacheKey::encoded(k));
                        }
                        Some(("tag", t)) => cache.mark_tag(Tag::encoded(t)),
                        _ => cache.0.l1.clear(),
                    }
                }
            })
            .await?;
        cache.0.listener.set(task).expect("new listener");
        Ok(cache)
    }

    #[cfg(feature = "redis")]
    pub(crate) fn redis(&self) -> Option<&Arc<crate::redis::Redis>> {
        self.0.redis.as_ref()
    }

    /// Whether sessions and other guard state are authoritative on this process.
    pub fn is_local(&self) -> bool {
        #[cfg(feature = "redis")]
        {
            self.redis().is_none()
        }
        #[cfg(not(feature = "redis"))]
        {
            true
        }
    }

    /// Constructs the app's cache from its explicit runtime configuration.
    /// Set `HYPERISH_ORBIC_CACHE_URL` and `HYPERISH_ORBIC_CACHE_NAMESPACE`
    /// for shared state. Multiple replicas require a shared store.
    pub async fn from_env() -> HyveResult<Self> {
        let replicas = std::env::var("HYPERISH_ORBIC_REPLICAS")
            .map_or(Ok(1), |v| {
                v.parse::<std::num::NonZeroU32>().map(|v| v.get())
            })
            .map_err(|_| HyveError::internal("HYPERISH_ORBIC_REPLICAS must be positive"))?;
        let Some(url) = std::env::var_os("HYPERISH_ORBIC_CACHE_URL") else {
            if replicas > 1 {
                return Err(HyveError::internal(
                    "multiple replicas require HYPERISH_ORBIC_CACHE_URL",
                ));
            }
            return Ok(Self::in_memory());
        };
        #[cfg(feature = "redis")]
        {
            let namespace = std::env::var("HYPERISH_ORBIC_CACHE_NAMESPACE").map_err(|_| {
                HyveError::internal(
                    "set HYPERISH_ORBIC_CACHE_NAMESPACE for the application's shared keys",
                )
            })?;
            Self::tiered(
                url.to_str()
                    .ok_or_else(|| HyveError::internal("invalid shared cache URL"))?,
                &namespace,
            )
            .await
        }
        #[cfg(not(feature = "redis"))]
        {
            let _ = url;
            Err(HyveError::internal(
                "enable the orbic cache-redis feature to use HYPERISH_ORBIC_CACHE_URL",
            ))
        }
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
        let guard = loop {
            let entry = match inner.l1.get_value_or_guard_async(&key).await {
                Ok(entry) => entry,
                Err(guard) => break guard,
            };
            if self.is_fresh(&entry) {
                #[cfg(feature = "redis")]
                if let (Some(remote), Some(revision)) = (self.redis(), &entry.revision) {
                    if remote.current(&key, revision).await? {
                        return read(&key, &entry);
                    }
                } else {
                    return read(&key, &entry);
                }
                #[cfg(not(feature = "redis"))]
                return read(&key, &entry);
            }
            // Acquire the next load atomically. A different caller may have already
            // installed a replacement while this caller was checking its revision.
            match inner
                .l1
                .entry_async(&key, |_, current| {
                    if Arc::ptr_eq(current, &entry) {
                        EntryAction::ReplaceWithGuard
                    } else {
                        EntryAction::Retain(())
                    }
                })
                .await
            {
                EntryResult::Replaced(guard, _) | EntryResult::Vacant(guard) => break guard,
                EntryResult::Retained(()) => continue,
                EntryResult::Removed(..) | EntryResult::Timeout => {
                    unreachable!("async replace or retain")
                }
            }
        };
        let started = inner.clock.now();
        let generation = self.start_load(&opts);
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            let crate::redis::Loaded {
                value,
                expires,
                revision,
            } = remote.load(&key, opts.ttl, &opts.tags, init).await?;
            if expires.is_none_or(|end| end > inner.clock.now()) {
                let _ = guard.insert(Arc::new(Entry {
                    revision: expires.map(|_| revision),
                    value: Arc::new(value.clone()),
                    type_name: type_name::<T>(),
                    expires,
                    validated: AtomicU64::new(generation),
                    tags: opts.tags.into(),
                }));
            }
            return Ok(value);
        }
        let value = init().await?;
        let _ = guard.insert(Arc::new(Entry {
            #[cfg(feature = "redis")]
            revision: None,
            value: Arc::new(value.clone()),
            type_name: type_name::<T>(),
            expires: (!opts.ttl.is_zero()).then(|| started + opts.ttl),
            validated: AtomicU64::new(generation),
            tags: opts.tags.into(),
        }));
        Ok(value)
    }

    /// Drops every entry tagged `tag`, including values still loading.
    pub async fn invalidate_tag(&self, tag: Tag) -> HyveResult<()> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            remote.invalidate_tag(&tag).await?;
        }
        self.mark_tag(tag);
        Ok(())
    }

    fn mark_tag(&self, tag: Tag) {
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
    }

    /// Drops the entry for `key`.
    pub async fn invalidate(&self, key: &CacheKey) -> HyveResult<()> {
        #[cfg(feature = "redis")]
        if let Some(remote) = self.redis() {
            remote.invalidate(key).await?;
        }
        self.0.l1.remove(key);
        Ok(())
    }

    /// In-process session records. Request guards use the authoritative async operations.
    pub fn sessions(&self) -> &SessionStore {
        &self.0.sessions
    }

    /// In-process idempotency records. Use `begin_request` for replicated applications.
    pub fn idempotency(&self) -> &IdempotencyStore {
        &self.0.idempotency
    }

    /// In-process counters. Use `count_request` for replicated applications.
    pub fn rate_limits(&self) -> &RateLimitStore {
        &self.0.rate_limits
    }

    /// The clock the cache and stores read.
    pub fn clock(&self) -> &Clock {
        &self.0.clock
    }

    fn start_load(&self, opts: &CacheOpts) -> u64 {
        if opts.tags.is_empty() {
            return 0;
        }
        // Publish the retention window and take its generation under the same lock.
        // An invalidation between these operations could otherwise be pruned too early.
        let mut tags = self.0.tags.lock().unwrap_or_else(PoisonError::into_inner);
        tags.max_ttl = tags.max_ttl.max(opts.ttl);
        self.0.seq.load(Ordering::Acquire)
    }

    fn is_fresh(&self, entry: &Entry) -> bool {
        if entry.expires.is_some_and(|end| end <= self.0.clock.now()) {
            return false;
        }
        if entry.tags.is_empty() {
            return true;
        }
        let validated = entry.validated.load(Ordering::Acquire);
        if self.0.seq.load(Ordering::Acquire) == validated {
            return true;
        }
        // An in-flight-only load has no retention window for invalidation history.
        if entry.expires.is_none() {
            return false;
        }
        let tags = self.0.tags.lock().unwrap_or_else(PoisonError::into_inner);
        if entry.tags.iter().any(|t| {
            tags.invalidated
                .get(t)
                .is_some_and(|(seq, _)| *seq > validated)
        }) {
            return false;
        }
        entry
            .validated
            .store(self.0.seq.load(Ordering::Acquire), Ordering::Release);
        true
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
