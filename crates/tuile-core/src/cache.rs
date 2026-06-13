// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Resident-content cache: LRU bounded by a byte budget.
//!
//! Invariant: a currently selected tile is never evicted — the budget may
//! be temporarily exceeded rather than dropping visible content.

use crate::source::TileId;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy)]
struct Entry {
    size: usize,
    last_used: u64,
}

#[derive(Debug)]
pub struct ResidentCache {
    budget: usize,
    used: usize,
    tick: u64,
    entries: HashMap<TileId, Entry>,
}

impl ResidentCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            used: 0,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    pub fn contains(&self, t: TileId) -> bool {
        self.entries.contains_key(&t)
    }

    /// Marks a tile as recently used (call when it is selected).
    pub fn touch(&mut self, t: TileId) {
        self.tick += 1;
        let tick = self.tick;
        if let Some(e) = self.entries.get_mut(&t) {
            e.last_used = tick;
        }
    }

    pub fn remove(&mut self, t: TileId) {
        if let Some(e) = self.entries.remove(&t) {
            self.used -= e.size;
        }
    }

    /// Inserts a tile and evicts least-recently-used entries until the
    /// budget holds, never touching `protected` (the selected set).
    /// Returns the evicted tiles.
    pub fn insert(&mut self, t: TileId, size: usize, protected: &HashSet<TileId>) -> Vec<TileId> {
        self.tick += 1;
        if let Some(prev) = self.entries.insert(
            t,
            Entry {
                size,
                last_used: self.tick,
            },
        ) {
            self.used -= prev.size;
        }
        self.used += size;

        let mut evicted = Vec::new();
        while self.used > self.budget {
            // LRU among evictable entries; tile id breaks ties deterministically.
            let victim = self
                .entries
                .iter()
                .filter(|(id, _)| !protected.contains(id) && **id != t)
                .min_by(|a, b| a.1.last_used.cmp(&b.1.last_used).then(a.0.cmp(b.0)))
                .map(|(id, _)| *id);
            match victim {
                Some(id) => {
                    self.remove(id);
                    evicted.push(id);
                }
                // Everything left is protected: accept being over budget.
                None => break,
            }
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u64) -> TileId {
        TileId(n)
    }

    #[test]
    fn budget_is_respected_to_the_byte() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        assert!(c.insert(t(1), 60, &none).is_empty());
        assert!(c.insert(t(2), 40, &none).is_empty());
        assert_eq!(c.used_bytes(), 100, "exactly at budget: no eviction");
        let evicted = c.insert(t(3), 1, &none);
        assert_eq!(evicted, vec![t(1)], "LRU goes first");
        assert_eq!(c.used_bytes(), 41);
    }

    #[test]
    fn touch_changes_eviction_order() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        c.insert(t(1), 50, &none);
        c.insert(t(2), 50, &none);
        c.touch(t(1)); // t2 becomes the LRU
        let evicted = c.insert(t(3), 50, &none);
        assert_eq!(evicted, vec![t(2)]);
    }

    #[test]
    fn selected_tiles_are_never_evicted() {
        let mut c = ResidentCache::new(100);
        let protected: HashSet<TileId> = [t(1), t(2)].into();
        c.insert(t(1), 60, &protected);
        c.insert(t(2), 60, &protected); // over budget, but both protected
        let evicted = c.insert(t(3), 60, &protected);
        assert_eq!(evicted, vec![]);
        assert!(c.contains(t(1)) && c.contains(t(2)) && c.contains(t(3)));
        assert!(c.used_bytes() > 100, "over budget rather than holes");
    }

    #[test]
    fn reinsert_replaces_size() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        c.insert(t(1), 80, &none);
        c.insert(t(1), 20, &none);
        assert_eq!(c.used_bytes(), 20);
    }
}
