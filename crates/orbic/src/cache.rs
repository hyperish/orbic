//! `orbic::cache` — L1 in-memory (`quick_cache` model) + L2 simulated Redis/Valkey Tiered Cache,
//! Singleflight stampede prevention, tag invalidation, and `key!`/`tag!`/`ttl!` macros.

use hyve::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

/// Strongly-typed cache key assembled via `key!(...)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(pub String);

/// Strongly-typed invalidation tag assembled via `tag!(...)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheTag(pub String);

/// Trait formatting cache key/tag components by shared reference without moving ownership.
pub trait CacheKeyPart {
    /// Appends a deterministic string representation of `self` to `out`.
    fn push_key_part(&self, out: &mut String);
}

impl CacheKeyPart for &str {
    fn push_key_part(&self, out: &mut String) {
        out.push_str(self);
    }
}
impl CacheKeyPart for String {
    fn push_key_part(&self, out: &mut String) {
        out.push_str(self);
    }
}
impl<T: ?Sized> CacheKeyPart for Id<T> {
    fn push_key_part(&self, out: &mut String) {
        out.push_str(self.as_str());
    }
}
impl<T: CacheKeyPart> CacheKeyPart for Option<T> {
    fn push_key_part(&self, out: &mut String) {
        match self {
            Some(v) => {
                out.push_str("some(");
                v.push_key_part(out);
                out.push(')');
            }
            None => out.push_str("none"),
        }
    }
}
impl CacheKeyPart for u32 {
    fn push_key_part(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}
impl CacheKeyPart for u64 {
    fn push_key_part(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}
impl CacheKeyPart for i64 {
    fn push_key_part(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}
impl CacheKeyPart for usize {
    fn push_key_part(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}
impl CacheKeyPart for bool {
    fn push_key_part(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}

/// Joins parts by shared reference into a [`CacheKey`].
#[doc(hidden)]
pub fn build_cache_key(parts: &[&dyn CacheKeyPart]) -> CacheKey {
    let mut s = String::new();
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            s.push(':');
        }
        part.push_key_part(&mut s);
    }
    CacheKey(s)
}

/// Joins parts by shared reference into a [`CacheTag`].
#[doc(hidden)]
pub fn build_cache_tag(parts: &[&dyn CacheKeyPart]) -> CacheTag {
    let mut s = String::new();
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            s.push(':');
        }
        part.push_key_part(&mut s);
    }
    CacheTag(s)
}

/// Parses human-readable TTL strings (`"30s"`, `"500ms"`, `"5m"`, `"1h"`) into `Duration`.
#[must_use]
pub fn parse_ttl(spec: &str) -> Duration {
    let s = spec.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return Duration::from_millis(ms.trim().parse().unwrap_or(1_000));
    }
    if let Some(secs) = s.strip_suffix('s') {
        return Duration::from_secs(secs.trim().parse().unwrap_or(30));
    }
    if let Some(mins) = s.strip_suffix('m') {
        return Duration::from_secs(mins.trim().parse::<u64>().unwrap_or(1) * 60);
    }
    if let Some(hrs) = s.strip_suffix('h') {
        return Duration::from_secs(hrs.trim().parse::<u64>().unwrap_or(1) * 3_600);
    }
    Duration::from_secs(s.parse().unwrap_or(30))
}

/// Builds a [`CacheKey`] without moving or cloning caller arguments (`key!("projects", user.id, cursor.after)`).
#[macro_export]
macro_rules! key {
    ($($part:expr),+ $(,)?) => {
        $crate::cache::build_cache_key(&[$(&$part as &dyn $crate::cache::CacheKeyPart),+])
    };
}

/// Builds a [`CacheTag`] without moving caller arguments (`tag!("projects", user.id)`).
#[macro_export]
macro_rules! tag {
    ($($part:expr),+ $(,)?) => {
        $crate::cache::build_cache_tag(&[$(&$part as &dyn $crate::cache::CacheKeyPart),+])
    };
}

/// Parses a compile-time/static duration literal (`ttl!("30s")`).
#[macro_export]
macro_rules! ttl {
    ($spec:expr) => {
        $crate::cache::parse_ttl($spec)
    };
}

/// Options attached to a cached entry (TTL + invalidation tags).
#[derive(Debug, Clone)]
pub struct CacheOpts {
    ttl: Duration,
    tags: Vec<CacheTag>,
}

impl CacheOpts {
    /// Creates cache options with an expiration TTL.
    #[must_use]
    pub fn ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            tags: Vec::new(),
        }
    }

    /// Attaches an invalidation tag (`tag!(...)`).
    #[must_use]
    pub fn tag(mut self, tag: CacheTag) -> Self {
        self.tags.push(tag);
        self
    }
}

#[derive(Clone)]
struct CacheEntry {
    bytes: Vec<u8>,
    expires_at: Instant,
}

#[derive(Default)]
struct CacheStore {
    l1: HashMap<String, CacheEntry>,
    l2: HashMap<String, CacheEntry>,
    tag_index: HashMap<String, HashSet<String>>,
    in_flight: HashMap<String, Arc<AtomicBool>>,
}

/// Tiered L1 (in-memory) + L2 (Redis/Valkey-modeled) cache with Singleflight and tag invalidation.
#[derive(Clone)]
pub struct Cache {
    store: Arc<Mutex<CacheStore>>,
    hits: Arc<AtomicUsize>,
    misses: Arc<AtomicUsize>,
    init_calls: Arc<AtomicUsize>,
    invalidations: Arc<AtomicUsize>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl Cache {
    /// Creates an isolated in-memory L1 + L2 cache engine.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            store: Arc::new(Mutex::new(CacheStore::default())),
            hits: Arc::new(AtomicUsize::new(0)),
            misses: Arc::new(AtomicUsize::new(0)),
            init_calls: Arc::new(AtomicUsize::new(0)),
            invalidations: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn get_cached_bytes(&self, key: &str, l2_only: bool) -> Option<Vec<u8>> {
        let now = Instant::now();
        let mut guard = self.store.lock().ok()?;
        if !l2_only && let Some(entry) = guard.l1.get(key) {
            if entry.expires_at > now {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(entry.bytes.clone());
            }
            guard.l1.remove(key);
        }
        if let Some(entry) = guard.l2.get(key).cloned() {
            if entry.expires_at > now {
                self.hits.fetch_add(1, Ordering::Relaxed);
                if !l2_only {
                    guard.l1.insert(key.to_owned(), entry.clone());
                }
                return Some(entry.bytes);
            }
            guard.l2.remove(key);
        }
        None
    }

    /// Reads directly from L2 (bypassing L1) — used by `orbic::auth` for instant revocation guarantees.
    #[must_use]
    pub fn l2_get_raw(&self, key: &str) -> Option<Vec<u8>> {
        self.get_cached_bytes(key, true)
    }

    /// Writes an entry to L2 and optionally populates L1.
    pub fn set_raw(
        &self,
        key: &str,
        bytes: Vec<u8>,
        ttl: Duration,
        tags: &[String],
        populate_l1: bool,
    ) {
        let expires_at = Instant::now() + ttl;
        let entry = CacheEntry { bytes, expires_at };
        if let Ok(mut guard) = self.store.lock() {
            for t in tags {
                guard
                    .tag_index
                    .entry(t.clone())
                    .or_default()
                    .insert(key.to_owned());
            }
            guard.l2.insert(key.to_owned(), entry.clone());
            if populate_l1 {
                guard.l1.insert(key.to_owned(), entry);
            }
        }
    }

    /// Deletes a key synchronously across both L1 and L2 (used for immediate session revocation).
    pub fn delete_raw(&self, key: &str) {
        if let Ok(mut guard) = self.store.lock() {
            guard.l1.remove(key);
            guard.l2.remove(key);
            self.invalidations.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Fetches cached value or invokes `init` at most once per concurrent key miss (Singleflight).
    pub async fn get_or_try_init<T, F, Fut>(
        &self,
        key: CacheKey,
        opts: CacheOpts,
        init: F,
    ) -> HyveResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = HyveResult<T>>,
        T: BinaryEncode + BinaryDecode,
    {
        if let Some(bytes) = self.get_cached_bytes(&key.0, false) {
            return from_bytes(&bytes);
        }

        enum Role {
            Leader(Arc<AtomicBool>),
            Follower(Arc<AtomicBool>),
        }

        let role = {
            let mut guard = self
                .store
                .lock()
                .map_err(|_| HyveError::internal("cache lock poisoned"))?;
            if let Some(flag) = guard.in_flight.get(&key.0) {
                Role::Follower(Arc::clone(flag))
            } else {
                let flag = Arc::new(AtomicBool::new(true));
                guard.in_flight.insert(key.0.clone(), Arc::clone(&flag));
                Role::Leader(flag)
            }
        };

        if let Role::Follower(flag) = role {
            std::future::poll_fn(|cx| {
                if !flag.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            if let Some(bytes) = self.get_cached_bytes(&key.0, false) {
                return from_bytes(&bytes);
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
            self.init_calls.fetch_add(1, Ordering::Relaxed);
            return init().await;
        }

        let Role::Leader(flag) = role else {
            unreachable!()
        };

        struct FlightCleanup<'a> {
            store: &'a Mutex<CacheStore>,
            key: &'a str,
            flag: Arc<AtomicBool>,
        }
        impl Drop for FlightCleanup<'_> {
            fn drop(&mut self) {
                self.flag.store(false, Ordering::Release);
                if let Ok(mut guard) = self.store.lock() {
                    guard.in_flight.remove(self.key);
                }
            }
        }
        let _cleanup = FlightCleanup {
            store: &self.store,
            key: &key.0,
            flag,
        };

        if let Some(bytes) = self.get_cached_bytes(&key.0, false) {
            return from_bytes(&bytes);
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        self.init_calls.fetch_add(1, Ordering::Relaxed);
        let value = init().await?;
        let encoded = to_bytes(&value);
        let tag_names: Vec<String> = opts.tags.into_iter().map(|t| t.0).collect();
        self.set_raw(&key.0, encoded, opts.ttl, &tag_names, true);
        Ok(value)
    }

    /// Evicts every cache key associated with `tag` from both L1 and L2.
    pub async fn invalidate_tag(&self, tag: CacheTag) -> HyveResult<()> {
        let mut guard = self
            .store
            .lock()
            .map_err(|_| HyveError::internal("cache lock poisoned"))?;
        if let Some(keys) = guard.tag_index.remove(&tag.0) {
            for k in keys {
                guard.l1.remove(&k);
                guard.l2.remove(&k);
            }
            self.invalidations.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Returns total cache hits observed.
    #[must_use]
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    /// Returns total cache misses observed.
    #[must_use]
    pub fn misses(&self) -> usize {
        self.misses.load(Ordering::Relaxed)
    }

    /// Returns total initializer executions (lower than concurrent misses under Singleflight).
    #[must_use]
    pub fn init_calls(&self) -> usize {
        self.init_calls.load(Ordering::Relaxed)
    }

    /// Returns total invalidation operations performed.
    #[must_use]
    pub fn invalidations(&self) -> usize {
        self.invalidations.load(Ordering::Relaxed)
    }
}
