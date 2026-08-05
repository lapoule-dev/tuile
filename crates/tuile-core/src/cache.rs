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

/// How far below the budget an eviction sweep goes, as a fraction of it.
///
/// Evicting to exactly the budget means evicting again on the very next insert,
/// forever, once the cache is full — and each sweep drops the least-recently
/// used tile, which on a turning camera is often one about to be wanted again.
/// Clearing a margin instead makes eviction occasional rather than continuous.
///
/// It also closes a way a bulk frame can fail to converge: that mode blocks
/// until every selected tile is resident, so if eviction keeps firing while
/// tiles arrive, the outstanding-request count need never reach zero. A margin
/// bounds how much of the frame's own working set a sweep can take back.
///
/// 0.8 is the SportsTrackLive viewer's figure, arrived at independently.
const EVICTION_LOW_WATER: f64 = 0.8;

#[derive(Debug)]
pub struct ResidentCache {
    budget: usize,
    /// Where a sweep stops. Derived from the budget once, not per call.
    low_water: usize,
    used: usize,
    tick: u64,
    entries: HashMap<TileId, Entry>,
}

impl ResidentCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            low_water: (budget_bytes as f64 * EVICTION_LOW_WATER) as usize,
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
        // The tile just admitted is never its own victim.
        self.trim_except(protected, Some(t))
    }

    /// Evicts down to the budget without admitting anything.
    ///
    /// Insertion is not enough on its own: it is the only other place eviction
    /// happens, so a session that stops loading — a camera that settles on a
    /// view it already holds — stays over budget indefinitely, however much of
    /// the residency has stopped being needed. Call this whenever the protected
    /// set changes, which is once per traversal.
    pub fn trim(&mut self, protected: &HashSet<TileId>) -> Vec<TileId> {
        self.trim_except(protected, None)
    }

    fn trim_except(&mut self, protected: &HashSet<TileId>, keep: Option<TileId>) -> Vec<TileId> {
        let mut evicted = Vec::new();
        // Nothing to do until the budget is actually exceeded — the margin
        // governs how far a sweep goes, not when one starts. Sweeping down to
        // the low-water mark on every insert would evict far more than the
        // budget asks for.
        if self.used <= self.budget {
            return evicted;
        }
        while self.used > self.low_water {
            // LRU among evictable entries; tile id breaks ties deterministically.
            let victim = self
                .entries
                .iter()
                .filter(|(id, _)| !protected.contains(id) && Some(**id) != keep)
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
        // Order, not count: a sweep clears down to the low-water mark, so how
        // many go depends on their sizes. What this pins is that the touched
        // tile is not the one taken first.
        assert_eq!(evicted.first(), Some(&t(2)));
    }

    /// A sweep starts only when the budget is exceeded, and then clears a
    /// margin. Evicting to exactly the budget means evicting again on the next
    /// insert, forever — and each sweep takes the least-recently-used tile,
    /// which on a turning camera is often one about to be wanted again.
    #[test]
    fn a_sweep_clears_a_margin_below_the_budget() {
        let mut c = ResidentCache::new(1000);
        let none = HashSet::new();
        for i in 1..=10 {
            c.insert(t(i), 100, &none);
        }
        assert_eq!(c.used_bytes(), 1000, "at budget, nothing evicted yet");

        c.insert(t(11), 100, &none);
        assert!(
            c.used_bytes() <= 800,
            "a sweep must clear to the low-water mark, left {} bytes",
            c.used_bytes()
        );

        // And having cleared it, the next few inserts cost nothing: that is the
        // whole point — eviction becomes occasional instead of continuous.
        let quiet = c.insert(t(12), 100, &none);
        assert!(
            quiet.is_empty(),
            "insert right after a sweep should not evict, took {quiet:?}"
        );
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
