// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Resident-content cache: LRU bounded by a byte budget.
//!
//! Invariant: a currently selected tile is never evicted — the budget may
//! be temporarily exceeded rather than dropping visible content.

use crate::raster::ImageryCoord;
use crate::source::TileId;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
struct Entry {
    size: usize,
    last_used: u64,
    /// The imagery this tile references, with repeats. Repeats are real — four
    /// siblings standing on one ancestor name it four times — and are kept
    /// rather than deduplicated so that release is the exact inverse of admit.
    imagery: Vec<ImageryCoord>,
}

/// One imagery texture, and how many resident tiles reference it.
#[derive(Debug, Clone, Copy)]
struct ImageryEntry {
    bytes: usize,
    refs: u32,
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
    /// Geometry, charged per tile.
    used: usize,
    tick: u64,
    entries: HashMap<TileId, Entry>,
    /// Draped imagery, charged **once per texture** however many tiles drape it.
    ///
    /// It has to be in the budget and it has to be in it once. Leaving it out
    /// entirely — which is where this briefly was — bounds nothing: an orbit at
    /// six kilometres holds around 700 MiB of imagery, governed only by how many
    /// tiles happen to be resident. Charging it per tile instead reports memory
    /// that was never allocated, and the cache then evicts geometry to reclaim
    /// bytes that do not exist. Refcounting by coord is the only account that is
    /// both bounded and true.
    imagery: HashMap<ImageryCoord, ImageryEntry>,
    imagery_used: usize,
}

impl ResidentCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            low_water: (budget_bytes as f64 * EVICTION_LOW_WATER) as usize,
            used: 0,
            tick: 0,
            entries: HashMap::new(),
            imagery: HashMap::new(),
            imagery_used: 0,
        }
    }

    /// Everything resident: geometry plus the imagery draped on it, each
    /// texture counted once.
    pub fn used_bytes(&self) -> usize {
        self.used + self.imagery_used
    }

    /// Just the imagery half, and how many distinct textures it is. The ratio
    /// against the number of drapes is what sharing is worth.
    pub fn imagery_bytes(&self) -> (usize, usize) {
        (self.imagery.len(), self.imagery_used)
    }

    /// Charges a tile's imagery, counting each texture once however many tiles
    /// already hold it.
    fn admit_imagery(&mut self, layers: &[(ImageryCoord, usize)]) -> Vec<ImageryCoord> {
        let mut held = Vec::with_capacity(layers.len());
        for (coord, bytes) in layers {
            match self.imagery.get_mut(coord) {
                Some(entry) => entry.refs += 1,
                None => {
                    self.imagery_used += bytes;
                    self.imagery.insert(
                        *coord,
                        ImageryEntry {
                            bytes: *bytes,
                            refs: 1,
                        },
                    );
                }
            }
            held.push(*coord);
        }
        held
    }

    /// Releases a tile's claim. A texture's bytes come back only when the last
    /// tile holding it lets go — until then it is still on the GPU.
    fn release_imagery(&mut self, held: &[ImageryCoord]) {
        for coord in held {
            let Some(entry) = self.imagery.get_mut(coord) else {
                continue;
            };
            entry.refs -= 1;
            if entry.refs == 0 {
                self.imagery_used -= entry.bytes;
                self.imagery.remove(coord);
            }
        }
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
            self.release_imagery(&e.imagery);
        }
    }

    /// Inserts a tile and evicts least-recently-used entries until the
    /// budget holds, never touching `protected` (the selected set).
    ///
    /// `imagery` is what the tile *drapes*, as `(coord, bytes)`. Shared: a
    /// texture named by twenty tiles is charged once, and its bytes come back
    /// only when the last of them is evicted.
    ///
    /// Returns the evicted tiles.
    pub fn insert(
        &mut self,
        t: TileId,
        size: usize,
        imagery: &[(ImageryCoord, usize)],
        protected: &HashSet<TileId>,
    ) -> Vec<TileId> {
        self.tick += 1;
        let held = self.admit_imagery(imagery);
        if let Some(prev) = self.entries.insert(
            t,
            Entry {
                size,
                last_used: self.tick,
                imagery: held,
            },
        ) {
            self.used -= prev.size;
            // Released *after* admitting the new claim, so imagery the tile
            // still drapes never briefly falls to zero references and gets
            // charged again as if it had been uploaded twice.
            self.release_imagery(&prev.imagery);
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
        if self.used_bytes() <= self.budget {
            return evicted;
        }
        // Evicting a tile always frees its geometry but frees its imagery only
        // if it was the last to hold it, so a sweep can shrink slowly. It still
        // terminates: every pass removes an entry, so the victim search runs out.
        while self.used_bytes() > self.low_water {
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

    /// Content that drapes nothing — b3dm, or terrain in the geometry debug
    /// view. Most of these tests are about geometry alone.
    const NO_IMAGERY: &[(ImageryCoord, usize)] = &[];

    fn img(n: u64) -> ImageryCoord {
        ImageryCoord {
            level: 5,
            x: n,
            y: 0,
        }
    }

    #[test]
    fn budget_is_respected_to_the_byte() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        assert!(c.insert(t(1), 60, NO_IMAGERY, &none).is_empty());
        assert!(c.insert(t(2), 40, NO_IMAGERY, &none).is_empty());
        assert_eq!(c.used_bytes(), 100, "exactly at budget: no eviction");
        let evicted = c.insert(t(3), 1, NO_IMAGERY, &none);
        assert_eq!(evicted, vec![t(1)], "LRU goes first");
        assert_eq!(c.used_bytes(), 41);
    }

    #[test]
    fn touch_changes_eviction_order() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        c.insert(t(1), 50, NO_IMAGERY, &none);
        c.insert(t(2), 50, NO_IMAGERY, &none);
        c.touch(t(1)); // t2 becomes the LRU
        let evicted = c.insert(t(3), 50, NO_IMAGERY, &none);
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
            c.insert(t(i), 100, NO_IMAGERY, &none);
        }
        assert_eq!(c.used_bytes(), 1000, "at budget, nothing evicted yet");

        c.insert(t(11), 100, NO_IMAGERY, &none);
        assert!(
            c.used_bytes() <= 800,
            "a sweep must clear to the low-water mark, left {} bytes",
            c.used_bytes()
        );

        // And having cleared it, the next few inserts cost nothing: that is the
        // whole point — eviction becomes occasional instead of continuous.
        let quiet = c.insert(t(12), 100, NO_IMAGERY, &none);
        assert!(
            quiet.is_empty(),
            "insert right after a sweep should not evict, took {quiet:?}"
        );
    }

    #[test]
    fn selected_tiles_are_never_evicted() {
        let mut c = ResidentCache::new(100);
        let protected: HashSet<TileId> = [t(1), t(2)].into();
        c.insert(t(1), 60, NO_IMAGERY, &protected);
        c.insert(t(2), 60, NO_IMAGERY, &protected); // over budget, but both protected
        let evicted = c.insert(t(3), 60, NO_IMAGERY, &protected);
        assert_eq!(evicted, vec![]);
        assert!(c.contains(t(1)) && c.contains(t(2)) && c.contains(t(3)));
        assert!(c.used_bytes() > 100, "over budget rather than holes");
    }

    /// Imagery is charged once however many tiles drape it — the whole point of
    /// sharing it. Charging per tile would report memory that was never
    /// allocated and make the cache evict geometry to reclaim it.
    #[test]
    fn shared_imagery_is_charged_once() {
        let mut c = ResidentCache::new(10_000);
        let none = HashSet::new();
        let shared = &[(img(1), 1000)][..];
        c.insert(t(1), 10, shared, &none);
        c.insert(t(2), 10, shared, &none);
        c.insert(t(3), 10, shared, &none);
        assert_eq!(
            c.used_bytes(),
            30 + 1000,
            "three tiles over one texture cost one texture"
        );
        assert_eq!(c.imagery_bytes(), (1, 1000));
    }

    /// And its bytes come back only when the last holder lets go. Freeing on the
    /// first eviction would under-report memory that is still on the GPU, which
    /// is the same mistake as over-reporting, pointing the other way.
    #[test]
    fn shared_imagery_is_freed_by_the_last_holder_only() {
        let mut c = ResidentCache::new(10_000);
        let none = HashSet::new();
        let shared = &[(img(1), 1000)][..];
        c.insert(t(1), 10, shared, &none);
        c.insert(t(2), 10, shared, &none);

        c.remove(t(1));
        assert_eq!(
            c.imagery_bytes(),
            (1, 1000),
            "one holder left, the texture is still resident"
        );
        c.remove(t(2));
        assert_eq!(c.imagery_bytes(), (0, 0), "the last holder freed it");
        assert_eq!(c.used_bytes(), 0);
    }

    /// Four descendants standing on one ancestor name it four times. The repeat
    /// is real, so admit and release have to be exact inverses of each other —
    /// deduplicating on the way in would leak the texture on the way out.
    #[test]
    fn a_tile_naming_one_texture_repeatedly_still_balances() {
        let mut c = ResidentCache::new(10_000);
        let none = HashSet::new();
        let repeated = &[(img(1), 500), (img(1), 500), (img(1), 500), (img(1), 500)][..];
        c.insert(t(1), 10, repeated, &none);
        assert_eq!(
            c.imagery_bytes(),
            (1, 500),
            "named four times, charged once"
        );
        c.remove(t(1));
        assert_eq!(c.imagery_bytes(), (0, 0), "released four times, freed once");
    }

    /// Imagery counts against the same budget as geometry, or it is bounded by
    /// nothing at all — which is where it briefly was, at roughly 700 MiB an
    /// orbit governed only by how many tiles happened to be resident.
    #[test]
    fn imagery_can_drive_an_eviction_on_its_own() {
        let mut c = ResidentCache::new(1000);
        let none = HashSet::new();
        // Tiny geometry, heavy and *unshared* imagery: nothing about the
        // geometry total says this cache is full.
        for i in 1..=5u64 {
            c.insert(t(i), 10, &[(img(i), 200)], &none);
        }
        assert!(
            c.used_bytes() <= 800,
            "imagery never triggered a sweep, left {} bytes",
            c.used_bytes()
        );
        assert!(
            c.entries.len() < 5,
            "the budget was held without evicting anything"
        );
    }

    /// Re-loading a tile that drapes the same imagery must not charge for it
    /// twice, however briefly — the release has to follow the new claim.
    #[test]
    fn reinserting_a_tile_does_not_double_charge_its_imagery() {
        let mut c = ResidentCache::new(10_000);
        let none = HashSet::new();
        let same = &[(img(1), 1000)][..];
        c.insert(t(1), 10, same, &none);
        c.insert(t(1), 20, same, &none);
        assert_eq!(c.imagery_bytes(), (1, 1000));
        assert_eq!(c.used_bytes(), 20 + 1000);
    }

    #[test]
    fn reinsert_replaces_size() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        c.insert(t(1), 80, NO_IMAGERY, &none);
        c.insert(t(1), 20, NO_IMAGERY, &none);
        assert_eq!(c.used_bytes(), 20);
    }
}
