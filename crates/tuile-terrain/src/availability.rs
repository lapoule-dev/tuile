// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Growing tile availability.
//!
//! Cesium World Terrain advertises only a shallow slice of its quadtree in
//! `layer.json` (often just the root); the existence of deeper tiles is
//! delivered *per tile*, in each tile's `metadata` extension. So availability
//! is not static — it grows as tiles download.
//!
//! [`Availability`] is the shared, interior-mutable structure that captures
//! this: the [`TerrainTree`](crate::tree::TerrainTree) reads it to decide
//! children during traversal, and the loader writes the ranges it parses from
//! each downloaded tile. Both hold the same `Arc`, so refinement reaches the
//! finest LOD as the path streams in — mirroring `CesiumTerrainProvider`'s
//! `TileAvailability`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use crate::layer::{AvailabilityRange, LayerJson};
use crate::tiling::TileCoord;

/// Available tile ranges per level, growing as tiles reveal their descendants.
#[derive(Debug, Default)]
pub struct Availability {
    /// `levels[z]` = the ranges of existing tiles known at level `z`.
    levels: RwLock<Vec<Vec<AvailabilityRange>>>,
    /// The same ranges as a set, purely to reject repeats on the way in.
    ///
    /// Neighbouring tiles describe overlapping slices of the quadtree, so the
    /// identical rectangle arrives again and again. Appending blindly makes
    /// `is_available` — a linear scan, run for every child of every visited
    /// tile, on every traversal — grow without bound, and the session slows to
    /// a halt long before memory becomes the complaint.
    seen: RwLock<HashSet<RangeKey>>,
    /// Whether reveals are held for [`Availability::apply_held`] rather than
    /// folded in as they arrive. See `TileTree::defer_edits`.
    deferred: AtomicBool,
    /// Reveals waiting for the gap between two passes.
    held: Mutex<Vec<(u32, Vec<Vec<AvailabilityRange>>)>>,
    /// Every range folded in since the layer, with its level, in the order
    /// it was: what [`Availability::added_since`] reads.
    added: Mutex<Vec<(u32, AvailabilityRange)>>,
}

impl Availability {
    /// Seeds from a parsed `layer.json`. Uses its static `available` table when
    /// present; when the layer is metadata-driven (only a shallow seed, or
    /// none), ensures level 0 at least holds the `root_tiles` so traversal can
    /// start and discover the rest.
    pub fn from_layer(layer: &LayerJson, root_tiles_x: u64, root_tiles_y: u64) -> Self {
        let mut levels = layer.available.clone();
        if levels.first().is_none_or(|l| l.is_empty()) {
            if levels.is_empty() {
                levels.push(Vec::new());
            }
            levels[0] = vec![AvailabilityRange {
                start_x: 0,
                start_y: 0,
                end_x: root_tiles_x.saturating_sub(1),
                end_y: root_tiles_y.saturating_sub(1),
            }];
        }
        let seen = levels
            .iter()
            .enumerate()
            .flat_map(|(level, ranges)| ranges.iter().map(move |r| key(level as u32, r)))
            .collect();
        Self {
            levels: RwLock::new(levels),
            seen: RwLock::new(seen),
            deferred: AtomicBool::new(false),
            held: Mutex::new(Vec::new()),
            added: Mutex::new(Vec::new()),
        }
    }

    /// The ranges folded in since the first `from` (with their level), onto
    /// `out`; returns how many have been, the `from` of the next call. What
    /// a reader that keeps answers by tile (a loader's presence index) needs
    /// to know which of them a reveal may have changed.
    pub fn added_since(&self, from: usize, out: &mut Vec<(u32, AvailabilityRange)>) -> usize {
        let added = self.added.lock().unwrap_or_else(|e| e.into_inner());
        out.extend_from_slice(added.get(from..).unwrap_or(&[]));
        added.len()
    }

    /// How many distinct ranges are held, all levels together. Diagnostics: a
    /// number that keeps climbing means dedup is failing to bite.
    pub fn range_count(&self) -> usize {
        self.levels
            .read()
            .expect("availability")
            .iter()
            .map(Vec::len)
            .sum()
    }

    /// Whether a tile is known to exist.
    pub fn is_available(&self, c: TileCoord) -> bool {
        let levels = self.levels.read().expect("availability");
        levels
            .get(c.level as usize)
            .is_some_and(|ranges| ranges.iter().any(|r| r.contains(c.x, c.y)))
    }

    /// Folds in a tile's `metadata` availability: `ranges[offset]` describes the
    /// existing tiles at level `base_level + offset + 1` (quantized-mesh
    /// `metadata` extension convention).
    ///
    /// Held instead, once [`defer`](Self::defer) has been called, until the
    /// next [`apply_held`](Self::apply_held).
    pub fn add_descendant_ranges(&self, base_level: u32, ranges: &[Vec<AvailabilityRange>]) {
        if self.deferred.load(Ordering::Acquire) {
            self.held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((base_level, ranges.to_vec()));
            return;
        }
        self.fold(base_level, ranges);
    }

    /// From now on, hold every reveal until [`apply_held`](Self::apply_held):
    /// a traversal reads this structure tile by tile, and a reveal landing
    /// halfway through one gives a pass that saw two different trees.
    pub fn defer(&self) {
        self.deferred.store(true, Ordering::Release);
    }

    /// Folds in every reveal held since the last call. The result does not
    /// depend on their order: availability is a union of ranges.
    pub fn apply_held(&self) {
        let held = std::mem::take(&mut *self.held.lock().unwrap_or_else(|e| e.into_inner()));
        for (base_level, ranges) in held {
            self.fold(base_level, &ranges);
        }
    }

    fn fold(&self, base_level: u32, ranges: &[Vec<AvailabilityRange>]) {
        let before = {
            let levels = self.levels.read().expect("availability");
            levels.iter().map(Vec::len).sum::<usize>()
        };
        let mut deepest = 0u32;
        {
            let mut levels = self.levels.write().expect("availability");
            let mut seen = self.seen.write().expect("availability");
            for (offset, at_level) in ranges.iter().enumerate() {
                let level = base_level as usize + offset + 1;
                if level >= levels.len() {
                    levels.resize_with(level + 1, Vec::new);
                }
                for range in at_level {
                    if seen.insert(key(level as u32, range)) {
                        levels[level].push(*range);
                        deepest = deepest.max(level as u32);
                        self.added
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((level as u32, *range));
                    }
                }
            }
        }
        let after = {
            let levels = self.levels.read().expect("availability");
            levels.iter().map(Vec::len).sum::<usize>()
        };
        // The shape of the tree changing UNDER the traversal.
        //
        // This is not bookkeeping: `TileTree::children` asks this structure
        // what exists, so every range folded in here is a place the next
        // traversal may descend where the last one could not. A selection is
        // therefore a function of (view, config, residency) *and of how much
        // of this had arrived when the pass ran* — which is how the same stage
        // rendered 106 tiles on one run and 7 on the next.
        if after != before {
            tuile_core::det!(
                "avail",
                base_level = base_level,
                added = after - before,
                total = after,
                deepest = deepest,
            );
        }
    }
}

/// Identity of a range at a level: the level plus the rectangle's corners.
type RangeKey = (u32, u64, u64, u64, u64);

fn key(level: u32, r: &AvailabilityRange) -> RangeKey {
    (level, r.start_x, r.start_y, r.end_x, r.end_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer_with(available: &str, extra: &str) -> LayerJson {
        let json = format!(
            r#"{{ "format":"quantized-mesh-1.0","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
                  "maxzoom":15 {extra}, "available":{available} }}"#
        );
        LayerJson::from_slice(json.as_bytes()).expect("layer")
    }

    /// **The ranges folded in are read in order from where a reader
    /// stood**: the layer's own are not among them, a repeat is not added
    /// twice, and a held reveal is read once it is folded in.
    #[test]
    fn the_ranges_folded_in_are_read_from_where_a_reader_stood() {
        let layer = layer_with("[]", r#", "metadataAvailability": 10"#);
        let a = Availability::from_layer(&layer, 2, 1);
        let mut read = Vec::new();
        assert_eq!(
            a.added_since(0, &mut read),
            0,
            "the layer's own are not added"
        );
        let range = |x| AvailabilityRange {
            start_x: x,
            start_y: 0,
            end_x: x,
            end_y: 0,
        };
        a.add_descendant_ranges(0, &[vec![range(1)]]);
        a.add_descendant_ranges(0, &[vec![range(1)]]);
        let at = a.added_since(0, &mut read);
        assert_eq!((at, read.clone()), (1, vec![(1, range(1))]));
        a.defer();
        a.add_descendant_ranges(1, &[vec![range(5)]]);
        read.clear();
        assert_eq!(a.added_since(at, &mut read), at, "held, not folded");
        a.apply_held();
        assert_eq!(a.added_since(at, &mut read), 2);
        assert_eq!(read, vec![(2, range(5))]);
    }

    #[test]
    fn metadata_layer_seeds_roots_then_grows() {
        // Pure metadata-availability: empty static table.
        let layer = layer_with("[]", r#", "metadataAvailability": 10"#);
        let a = Availability::from_layer(&layer, 2, 1);
        // Roots seeded.
        assert!(a.is_available(TileCoord::new(0, 0, 0)));
        assert!(a.is_available(TileCoord::new(0, 1, 0)));
        // Nothing deeper yet.
        assert!(!a.is_available(TileCoord::new(1, 0, 0)));

        // A level-0 tile reveals levels 1 and 2.
        a.add_descendant_ranges(
            0,
            &[
                vec![AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: 3,
                    end_y: 1,
                }],
                vec![AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: 7,
                    end_y: 3,
                }],
            ],
        );
        assert!(a.is_available(TileCoord::new(1, 3, 1)));
        assert!(a.is_available(TileCoord::new(2, 7, 3)));
        assert!(!a.is_available(TileCoord::new(2, 8, 3)));
    }

    #[test]
    fn static_table_is_used_as_is() {
        let layer = layer_with(r#"[[{"startX":0,"startY":0,"endX":1,"endY":0}]]"#, "");
        let a = Availability::from_layer(&layer, 2, 1);
        assert!(a.is_available(TileCoord::new(0, 1, 0)));
        assert!(!a.is_available(TileCoord::new(1, 0, 0)));
    }

    /// Neighbouring tiles keep describing the same slices of the quadtree.
    /// Storing each repeat makes `is_available` — a linear scan run per child
    /// per traversal — grow without bound, and the session grinds to a halt.
    #[test]
    fn repeated_ranges_are_recorded_once() {
        let a = Availability::from_layer(&layer_with("[]", ""), 2, 1);
        let ranges = vec![vec![AvailabilityRange {
            start_x: 0,
            start_y: 0,
            end_x: 3,
            end_y: 1,
        }]];
        let before = a.range_count();
        for _ in 0..100 {
            a.add_descendant_ranges(0, &ranges);
        }
        assert_eq!(
            a.range_count() - before,
            1,
            "100 identical reveals should leave one range"
        );
        assert!(a.is_available(TileCoord::new(1, 3, 1)));
    }

    /// Deferred, a reveal changes nothing a traversal can read until it is
    /// applied — and applied, it is the same availability as folded at once.
    #[test]
    fn deferred_reveals_wait_for_apply() {
        let a = Availability::from_layer(&layer_with("[]", ""), 2, 1);
        a.defer();
        let ranges = vec![vec![AvailabilityRange {
            start_x: 0,
            start_y: 0,
            end_x: 3,
            end_y: 1,
        }]];
        a.add_descendant_ranges(0, &ranges);
        assert!(
            !a.is_available(TileCoord::new(1, 3, 1)),
            "a held reveal is invisible"
        );
        a.apply_held();
        assert!(a.is_available(TileCoord::new(1, 3, 1)));
        // Still deferred after an apply: the next reveal waits as well.
        a.add_descendant_ranges(
            1,
            &[vec![AvailabilityRange {
                start_x: 0,
                start_y: 0,
                end_x: 7,
                end_y: 3,
            }]],
        );
        assert!(!a.is_available(TileCoord::new(2, 7, 3)));
        a.apply_held();
        assert!(a.is_available(TileCoord::new(2, 7, 3)));
    }

    #[test]
    fn distinct_ranges_are_all_kept() {
        let a = Availability::from_layer(&layer_with("[]", ""), 2, 1);
        let before = a.range_count();
        for x in 0..4u64 {
            a.add_descendant_ranges(
                0,
                &[vec![AvailabilityRange {
                    start_x: x,
                    start_y: 0,
                    end_x: x,
                    end_y: 0,
                }]],
            );
        }
        assert_eq!(a.range_count() - before, 4);
    }
}
