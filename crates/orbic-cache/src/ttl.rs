//! A map whose entries expire, swept when it grows (the non-evicting stores).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

const MIN_SWEEP: usize = 1024;

pub(crate) struct TtlMap<K, V> {
    inner: Mutex<Inner<K, V>>,
}

pub(crate) struct Inner<K, V> {
    pub(crate) map: HashMap<K, (V, Instant)>,
    sweep_at: usize,
}

impl<K: Hash + Eq, V> TtlMap<K, V> {
    pub(crate) fn new() -> Self {
        TtlMap {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                sweep_at: MIN_SWEEP,
            }),
        }
    }

    /// Locks the map, first dropping expired entries when it has doubled
    /// since the last sweep (amortized O(1) per insert).
    pub(crate) fn lock(&self, now: Instant) -> MutexGuard<'_, Inner<K, V>> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.map.len() >= g.sweep_at {
            g.map.retain(|_, (_, expires)| *expires > now);
            g.sweep_at = (g.map.len() * 2).max(MIN_SWEEP);
        }
        g
    }
}

impl<K: Hash + Eq, V> Inner<K, V> {
    /// The live entry for `key`; an expired one is removed.
    pub(crate) fn live(&mut self, key: &K, now: Instant) -> Option<&mut (V, Instant)> {
        if self
            .map
            .get(key)
            .is_some_and(|(_, expires)| *expires <= now)
        {
            self.map.remove(key);
        }
        self.map.get_mut(key)
    }
}
