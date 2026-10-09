// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A least-recently-used map with a weight budget.
//!
//! Each entry weighs what the caller says (bytes of a tile, bytes of a file,
//! or one for an open reader); inserting past the budget evicts the least
//! recently used entries and hands them back, so a caller that owns something
//! outside memory (a file on disk) can release it.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

struct Slot<V> {
    value: V,
    weight: u64,
    tick: u64,
}

/// A map that forgets its least recently used entries past a weight budget.
pub struct Lru<K, V> {
    budget: u64,
    weight: u64,
    tick: u64,
    map: HashMap<K, Slot<V>>,
    order: BTreeMap<u64, K>,
}

impl<K: Eq + Hash + Clone, V: Clone> Lru<K, V> {
    pub fn new(budget: u64) -> Self {
        Self { budget, weight: 0, tick: 0, map: HashMap::new(), order: BTreeMap::new() }
    }

    fn bump(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The value, marked as just used.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let tick = self.bump();
        let slot = self.map.get_mut(key)?;
        self.order.remove(&slot.tick);
        slot.tick = tick;
        self.order.insert(tick, key.clone());
        Some(slot.value.clone())
    }

    /// The value, without marking it used.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.map.get(key).map(|s| &s.value)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Inserts or replaces, then evicts past the budget. Returns what was
    /// evicted (never the entry just inserted, unless it alone exceeds the
    /// budget, in which case it is not kept at all).
    pub fn insert(&mut self, key: K, value: V, weight: u64) -> Vec<(K, V)> {
        let mut evicted = Vec::new();
        if let Some(old) = self.remove(&key) {
            // A replaced value is not an eviction: the caller replaced it.
            drop(old);
        }
        if weight > self.budget {
            evicted.push((key, value));
            return evicted;
        }
        let tick = self.bump();
        self.weight += weight;
        self.order.insert(tick, key.clone());
        self.map.insert(key, Slot { value, weight, tick });
        while self.weight > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some(slot) = self.map.remove(&oldest) {
                self.weight -= slot.weight;
                evicted.push((oldest, slot.value));
            }
        }
        evicted
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.map.remove(key)?;
        self.order.remove(&slot.tick);
        self.weight -= slot.weight;
        Some(slot.value)
    }

    /// Every key held, in no particular order.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.map.keys()
    }

    /// Total weight held.
    pub fn weight(&self) -> u64 {
        self.weight
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Lru;

    #[test]
    fn the_least_recently_used_goes_first() {
        let mut lru = Lru::new(3);
        assert!(lru.insert("a", 1, 1).is_empty());
        assert!(lru.insert("b", 2, 1).is_empty());
        assert!(lru.insert("c", 3, 1).is_empty());
        assert_eq!(lru.get(&"a"), Some(1), "a is used again");
        assert_eq!(lru.insert("d", 4, 1), vec![("b", 2)], "b is now the oldest");
        assert!(lru.contains(&"a") && lru.contains(&"c") && lru.contains(&"d"));
        assert_eq!(lru.weight(), 3);
    }

    #[test]
    fn weights_count_and_an_oversized_entry_is_refused() {
        let mut lru = Lru::new(10);
        lru.insert("a", (), 6);
        assert_eq!(lru.insert("b", (), 6), vec![("a", ())]);
        assert_eq!(lru.insert("huge", (), 11), vec![("huge", ())]);
        assert!(!lru.contains(&"huge"));
        lru.insert("b", (), 2);
        assert_eq!(lru.weight(), 2, "a replacement is weighed once");
        assert_eq!(lru.remove(&"b"), Some(()));
        assert_eq!(lru.weight(), 0);
    }
}
