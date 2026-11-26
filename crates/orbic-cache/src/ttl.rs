//! Non-evicting authority records. Shard construction and hash-bit selection adapted
//! from quick_cache 0.7.0 `sync.rs` and `options.rs`, 99809ceaeb8b494a5425054711b7bb02fceb79ee
//! (MIT, see ttl/LICENSE-MIT). Expiry and reclamation are local to the selected shard.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

const MIN_SWEEP: usize = 1024;

pub(crate) struct TtlMap<K, V> {
    shards: Box<[Mutex<Inner<K, V>>]>,
    hash_builder: RandomState,
    sweep_floor: usize,
}

pub(crate) struct Inner<K, V> {
    pub(crate) map: HashMap<K, (V, Instant)>,
    sweep_at: usize,
}

impl<K: Hash + Eq, V> TtlMap<K, V> {
    pub(crate) fn new() -> Self {
        static SHARDS: OnceLock<usize> = OnceLock::new();
        Self::with_shards(*SHARDS.get_or_init(|| {
            let cores = std::thread::available_parallelism().map_or(1, usize::from);
            // Keep at least 32 records per shard before triggering a sweep.
            (cores.min(8) * 4).next_power_of_two()
        }))
    }

    fn with_shards(count: usize) -> Self {
        debug_assert!(count.is_power_of_two());
        let sweep_floor = MIN_SWEEP / count;
        let shards = (0..count)
            .map(|_| {
                Mutex::new(Inner {
                    map: HashMap::new(),
                    sweep_at: sweep_floor,
                })
            })
            .collect();
        Self {
            shards,
            hash_builder: RandomState::new(),
            sweep_floor,
        }
    }

    pub(crate) fn lock(&self, key: &K, now: Instant) -> MutexGuard<'_, Inner<K, V>> {
        let mask = self.shards.len() - 1;
        let index = if mask == 0 {
            0
        } else {
            // Leave the low bits and the high tag bits available for in-shard probing.
            self.hash_builder
                .hash_one(key)
                .rotate_right(usize::BITS / 2) as usize
                & mask
        };
        let mut g = self.shards[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if g.map.len() >= g.sweep_at {
            g.map.retain(|_, (_, expires)| *expires > now);
            g.sweep_at = g.map.len().saturating_mul(2).max(self.sweep_floor);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    fn count(map: &TtlMap<usize, usize>, threads: usize, rounds: usize, keys: usize) {
        let now = Instant::now();
        thread::scope(|scope| {
            for worker in 0..threads {
                scope.spawn(move || {
                    for i in 0..rounds {
                        let key = (worker * 41 + i) % keys;
                        let mut shard = map.lock(&key, now);
                        shard
                            .map
                            .entry(key)
                            .or_insert((0, now + Duration::from_secs(60)))
                            .0 += 1;
                    }
                });
            }
        });
        let total: usize = (0..keys).map(|key| map.lock(&key, now).map[&key].0).sum();
        assert_eq!(total, threads * rounds);
    }
    #[test]
    fn contended_keys_keep_every_update() {
        count(&TtlMap::new(), 8, 256, 7);
    }
    #[test]
    #[ignore = "local contention measurement, run with --release --ignored --nocapture"]
    fn compare_shards() {
        let mut runs = [Vec::new(), Vec::new()];
        for trial in 0..21 {
            for path in [trial % 2, 1 - trial % 2] {
                let map = if path == 0 {
                    TtlMap::new()
                } else {
                    TtlMap::with_shards(1)
                };
                let start = Instant::now();
                count(&map, 8, 8192, 1024);
                runs[path].push(start.elapsed().as_nanos() / (8 * 8192));
            }
        }
        for (name, mut ns) in ["partitioned", "single mutex"].into_iter().zip(runs) {
            ns.sort_unstable();
            println!(
                "{name} ns per authority update {} ({}..{})",
                ns[10], ns[0], ns[20]
            );
        }
    }
}
