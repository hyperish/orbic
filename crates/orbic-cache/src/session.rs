//! The authoritative in-process session store (`docs/03-adapters-and-pipeline.md`
//! §2, 인증·세션): records keyed by an opaque handle (the auth layer stores
//! token hashes, never tokens), grouped into families so revoking a session
//! also revokes the refresh tokens derived from it. Never evicted; entries
//! expire by TTL. Restarting the process drops every session.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::clock::Clock;

const MIN_SWEEP: usize = 1024;

/// Session and refresh records.
#[derive(Clone)]
pub struct SessionStore(Arc<Inner>);

struct Inner {
    state: Mutex<State>,
    clock: Clock,
}

struct Record {
    value: Vec<u8>,
    family: String,
    expires: Instant,
}

#[derive(Default)]
struct State {
    records: HashMap<String, Record>,
    families: HashMap<String, HashSet<String>>,
    sweep_at: usize,
}

impl State {
    fn insert(&mut self, key: String, family: &str, value: Vec<u8>, expires: Instant) {
        self.unlink(&key);
        self.families
            .entry(family.to_owned())
            .or_default()
            .insert(key.clone());
        self.records.insert(
            key,
            Record {
                value,
                family: family.to_owned(),
                expires,
            },
        );
    }

    fn revoke(&mut self, family: &str, now: Instant) -> usize {
        self.families
            .remove(family)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|key| self.records.remove(&key))
            .filter(|r| r.expires > now)
            .count()
    }

    fn sweep(&mut self, now: Instant) {
        if self.records.len() < self.sweep_at.max(MIN_SWEEP) {
            return;
        }
        let expired: Vec<String> = self
            .records
            .iter()
            .filter(|(_, r)| r.expires <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            self.unlink(&k);
        }
        self.sweep_at = self.records.len() * 2;
    }

    fn unlink(&mut self, key: &str) -> Option<Record> {
        let r = self.records.remove(key)?;
        if let Some(keys) = self.families.get_mut(&r.family) {
            keys.remove(key);
            if keys.is_empty() {
                self.families.remove(&r.family);
            }
        }
        Some(r)
    }

    fn live(&mut self, key: &str, now: Instant) -> Option<&mut Record> {
        if self.records.get(key).is_some_and(|r| r.expires <= now) {
            self.unlink(key);
        }
        self.records.get_mut(key)
    }
}

impl SessionStore {
    pub(crate) fn new(clock: Clock) -> Self {
        SessionStore(Arc::new(Inner {
            state: Mutex::new(State::default()),
            clock,
        }))
    }

    fn with<R>(&self, f: impl FnOnce(&mut State, Instant) -> R) -> R {
        let now = self.0.clock.now();
        let mut s = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.sweep(now);
        f(&mut s, now)
    }

    pub(crate) fn record(&self, key: &str) -> Option<(Vec<u8>, String)> {
        self.with(|s, now| {
            s.live(key, now)
                .map(|r| (r.value.clone(), r.family.clone()))
        })
    }

    pub(crate) fn write(
        &self,
        family: &str,
        records: &[crate::SessionWrite],
        expected: Option<(&str, &[u8])>,
    ) -> bool {
        self.with(|s, now| {
            if let Some((key, bytes)) = expected {
                let Some(old) = s.live(key, now) else {
                    return false;
                };
                if old.family != family {
                    return false;
                }
                if old.value != bytes {
                    s.revoke(family, now);
                    return false;
                }
            }
            for r in records {
                s.insert(r.key.clone(), family, r.value.clone(), now + r.ttl);
            }
            true
        })
    }

    /// Stores `value` under `key` in `family` for `ttl`, replacing any record.
    pub fn put(
        &self,
        key: impl Into<String>,
        family: impl Into<String>,
        value: Vec<u8>,
        ttl: Duration,
    ) {
        let (key, family) = (key.into(), family.into());
        self.with(|s, now| s.insert(key, &family, value, now + ttl));
    }

    /// The live record's value.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.with(|s, now| s.live(key, now).map(|r| r.value.clone()))
    }

    /// The live record's family.
    pub fn family(&self, key: &str) -> Option<String> {
        self.with(|s, now| s.live(key, now).map(|r| r.family.clone()))
    }

    /// Extends a live record to expire `ttl` from now (sliding expiry).
    pub fn touch(&self, key: &str, ttl: Duration) -> bool {
        self.with(|s, now| s.live(key, now).map(|r| r.expires = now + ttl).is_some())
    }

    /// Removes and returns a live record with its family, at most once (for
    /// single-use refresh tokens).
    pub fn take(&self, key: &str) -> Option<(Vec<u8>, String)> {
        self.with(|s, now| {
            s.live(key, now)?;
            s.unlink(key).map(|r| (r.value, r.family))
        })
    }

    /// Removes a record.
    pub fn remove(&self, key: &str) -> bool {
        self.with(|s, _| s.unlink(key).is_some())
    }

    /// Removes every record of `family`; returns how many were live.
    pub fn revoke_family(&self, family: &str) -> usize {
        self.with(|s, now| s.revoke(family, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn records_expire_slide_and_revoke_by_family() {
        let time = ManualClock::new();
        let store = SessionStore::new(time.clock());
        let hour = Duration::from_secs(3600);
        store.put("s1", "f1", b"ada".to_vec(), hour);
        store.put("r1", "f1", b"refresh".to_vec(), hour * 24);
        store.put("s2", "f2", b"bob".to_vec(), hour);
        assert_eq!(store.get("s1").as_deref(), Some(&b"ada"[..]));
        assert_eq!(store.family("r1").as_deref(), Some("f1"));

        time.advance(hour / 2);
        assert!(store.touch("s1", hour));
        time.advance(hour * 3 / 4);
        assert!(store.get("s1").is_some(), "slid forward");
        assert!(store.get("s2").is_none(), "expired");
        assert!(!store.touch("s2", hour));

        assert_eq!(store.take("r1"), Some((b"refresh".to_vec(), "f1".into())));
        assert_eq!(store.take("r1"), None, "single use");
        store.put("r2", "f1", vec![], hour);
        assert_eq!(store.revoke_family("f1"), 2);
        assert!(store.get("s1").is_none() && store.get("r2").is_none());
        assert!(!store.remove("s1"));
    }
}
