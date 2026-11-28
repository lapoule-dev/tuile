// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Growing terrain relief, sampled from the tiles that have streamed in.
//!
//! Like [`Availability`](crate::Availability), this is shared and
//! interior-mutable: the loader records what each decoded tile says about its
//! own relief, and a camera controller reads it to keep the eye above ground.
//! Both hold the same `Arc`, so the surface a camera is clamped against sharpens
//! as the globe refines.
//!
//! It also keeps each decoded tile's height *range*, for the
//! [`TerrainTree`](crate::TerrainTree)'s bounding volumes: see
//! [`TerrainHeights::record_range`] and [`TerrainHeights::range_of`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use tuile_core::ground::GroundHeight;

use crate::tiling::{GeographicTilingScheme, TileCoord};

/// A tile, `(level, x, y)`, and a height range, `(lowest, highest)` metres.
type Key = (u32, u64, u64);
type Range = (f32, f32);

/// Per-tile relief, keyed by tile, deepest answer wins.
///
/// Records each tile's **maximum** height rather than its geometry. That is
/// deliberately conservative: the answer for a point is the highest ground
/// anywhere in the smallest tile covering it, so a camera clamped against it
/// stops short of a nearby peak rather than clipping through it. The margin
/// shrinks as tiles refine — a level-15 tile spans a few hundred metres — and
/// erring high is the only direction that is safe.
///
/// Keeping the meshes instead would be exact and far heavier: the geometry
/// server already streams them to a consumer and drops them, and holding a
/// second copy purely to answer camera queries would undo the residency budget.
#[derive(Debug)]
pub struct TerrainHeights {
    scheme: GeographicTilingScheme,
    /// `(level, x, y) → maximum height in metres`.
    tiles: RwLock<HashMap<(u32, u64, u64), f32>>,
    /// Deepest level recorded so far, so a query starts where the data is
    /// finest instead of walking every level from the root.
    deepest: RwLock<u32>,
    /// `(level, x, y) → (lowest, highest)` metres, from each decoded tile's
    /// own header: what the tree's bounding volumes read. Changed only
    /// between two passes once [`defer`](Self::defer) has been called.
    ranges: RwLock<HashMap<Key, Range>>,
    /// Whether ranges are held for [`apply_held`](Self::apply_held).
    deferred: AtomicBool,
    /// Ranges waiting for the gap between two passes.
    held: Mutex<Vec<(Key, Range)>>,
    /// Bumped each time a range is folded in that was not there: the tree's
    /// revision, since its bounding volumes read the ranges.
    generation: AtomicU64,
}

impl TerrainHeights {
    pub fn new(scheme: GeographicTilingScheme) -> Self {
        Self {
            scheme,
            tiles: RwLock::new(HashMap::new()),
            deepest: RwLock::new(0),
            ranges: RwLock::new(HashMap::new()),
            deferred: AtomicBool::new(false),
            held: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Records a decoded tile's height range, `lowest..=highest` metres
    /// (the quantized-mesh header's `minHeight` and `maxHeight`): its highest
    /// point for the camera, as [`record`](Self::record), and the whole range
    /// for the tree's bounding volumes ([`range_of`](Self::range_of)),
    /// held until the next [`apply_held`](Self::apply_held) once
    /// [`defer`](Self::defer) has been called.
    pub fn record_range(&self, c: TileCoord, lowest: f32, highest: f32) {
        self.record(c, highest);
        if !(lowest.is_finite() && highest.is_finite() && lowest <= highest) {
            return;
        }
        let entry = ((c.level, c.x, c.y), (lowest, highest));
        if self.deferred.load(Ordering::Acquire) {
            self.held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(entry);
            return;
        }
        self.fold([entry]);
    }

    /// From now on, hold every range until [`apply_held`](Self::apply_held):
    /// a traversal reads the ranges tile by tile, and one landing halfway
    /// through gives a pass that saw two different trees. The camera's
    /// heights ([`GroundHeight`]) are not held.
    pub fn defer(&self) {
        self.deferred.store(true, Ordering::Release);
    }

    /// Folds in every range held since the last call. The result does not
    /// depend on their order: a tile's range is its header's, the same
    /// whenever it lands.
    pub fn apply_held(&self) {
        let held = std::mem::take(&mut *self.held.lock().unwrap_or_else(|e| e.into_inner()));
        self.fold(held);
    }

    fn fold(&self, entries: impl IntoIterator<Item = (Key, Range)>) {
        let mut ranges = self.ranges.write().expect("terrain heights");
        let mut changed = false;
        for (key, range) in entries {
            changed |= ranges.insert(key, range) != Some(range);
        }
        if changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// How many times the ranges have changed: the same number, the same
    /// ranges.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The range decoded for `c`, once folded in; `None` before. Never
    /// forgotten: a range is a few bytes against the mesh that gave it.
    pub fn range_of(&self, c: TileCoord) -> Option<(f32, f32)> {
        self.ranges
            .read()
            .expect("terrain heights")
            .get(&(c.level, c.x, c.y))
            .copied()
    }

    /// Records what a decoded tile says about its own relief. The
    /// quantized-mesh header carries this, so nothing has to be computed from
    /// the geometry.
    pub fn record(&self, c: TileCoord, max_height: f32) {
        if !max_height.is_finite() {
            return;
        }
        self.tiles
            .write()
            .expect("terrain heights")
            .insert((c.level, c.x, c.y), max_height);
        let mut deepest = self.deepest.write().expect("terrain heights");
        *deepest = (*deepest).max(c.level);
    }

    /// Forgets a tile.
    ///
    /// Nothing calls this when content is evicted, and that is a decision
    /// rather than an oversight: relief is worth keeping longer than the
    /// geometry that revealed it. An entry is a coordinate and an `f32`, against
    /// the megabytes of mesh and texture it summarises — and dropping it makes
    /// the camera floor *worse*, falling back to a coarser tile or to the
    /// ellipsoid over ground that has not moved.
    ///
    /// (It could be wired: a `TileLoader` hook called on eviction would reach
    /// here without the core learning about terrain. The reason not to is the
    /// one above, not the plumbing.)
    ///
    /// Here for hosts that stream far enough to care, and for tests.
    pub fn forget(&self, c: TileCoord) {
        self.tiles
            .write()
            .expect("terrain heights")
            .remove(&(c.level, c.x, c.y));
    }

    /// The tile containing a geodetic position at `level`, or `None` if the
    /// position falls outside the scheme's extent.
    fn tile_at(&self, level: u32, lon: f64, lat: f64) -> Option<TileCoord> {
        use std::f64::consts::{FRAC_PI_2, PI};
        let (nx, ny) = (self.scheme.tiles_x(level), self.scheme.tiles_y(level));
        // Normalized west→east and south→north across the full extent.
        let u = (lon + PI) / (2.0 * PI);
        let v = (lat + FRAC_PI_2) / PI;
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            return None;
        }
        let x = ((u * nx as f64) as u64).min(nx - 1);
        let y = ((v * ny as f64) as u64).min(ny - 1);
        Some(TileCoord::new(level, x, y))
    }
}

impl GroundHeight for TerrainHeights {
    /// The height from the finest tile covering the point, or `None` when no
    /// tile there has streamed in yet.
    fn height_at(&self, lon: f64, lat: f64) -> Option<f64> {
        let tiles = self.tiles.read().expect("terrain heights");
        let deepest = *self.deepest.read().expect("terrain heights");
        // Finest first: a coarse tile's maximum covers a whole continent, and
        // would hold the camera kilometres above ground long after the tile
        // under it arrived.
        (0..=deepest).rev().find_map(|level| {
            let c = self.tile_at(level, lon, lat)?;
            tiles.get(&(c.level, c.x, c.y)).map(|h| f64::from(*h))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heights() -> TerrainHeights {
        TerrainHeights::new(GeographicTilingScheme::default())
    }

    /// Somewhere in the eastern hemisphere, northern latitudes.
    const LON: f64 = 0.1;
    const LAT: f64 = 0.8;

    #[test]
    fn an_unseen_position_has_no_height() {
        assert_eq!(heights().height_at(LON, LAT), None);
    }

    #[test]
    fn a_recorded_tile_answers_for_points_inside_it() {
        let h = heights();
        let c = h.tile_at(0, LON, LAT).expect("in extent");
        h.record(c, 1500.0);
        assert_eq!(h.height_at(LON, LAT), Some(1500.0));
    }

    /// The finest tile wins: a coarse tile's maximum is the highest peak over a
    /// huge area, and would strand the camera far above the ground beneath it.
    #[test]
    fn a_finer_tile_overrides_a_coarser_one() {
        let h = heights();
        h.record(h.tile_at(0, LON, LAT).expect("l0"), 8000.0);
        h.record(h.tile_at(6, LON, LAT).expect("l6"), 200.0);
        assert_eq!(h.height_at(LON, LAT), Some(200.0));
    }

    /// Coverage is patchy while streaming: a query must fall back to whatever
    /// coarser tile it does have rather than reporting nothing.
    #[test]
    fn a_gap_at_a_fine_level_falls_back_to_a_coarse_one() {
        let h = heights();
        h.record(h.tile_at(0, LON, LAT).expect("l0"), 8000.0);
        // A deep tile elsewhere, so `deepest` is high but this point is absent.
        h.record(h.tile_at(9, -2.0, -0.5).expect("elsewhere"), 10.0);
        assert_eq!(h.height_at(LON, LAT), Some(8000.0));
    }

    #[test]
    fn a_forgotten_tile_stops_answering() {
        let h = heights();
        let c = h.tile_at(4, LON, LAT).expect("in extent");
        h.record(c, 1200.0);
        h.forget(c);
        assert_eq!(h.height_at(LON, LAT), None);
    }

    /// Deferred, a range waits for the gap between two passes, and the
    /// generation says when it lands; the camera's height does not wait.
    #[test]
    fn a_deferred_range_waits_for_apply() {
        let h = heights();
        h.defer();
        let c = h.tile_at(6, LON, LAT).expect("in extent");
        h.record_range(c, 5.0, 400.0);
        assert_eq!(h.range_of(c), None);
        assert_eq!(h.generation(), 0);
        assert_eq!(h.height_at(LON, LAT), Some(400.0));
        h.apply_held();
        assert_eq!(h.range_of(c), Some((5.0, 400.0)));
        assert_eq!(h.generation(), 1);
        h.record_range(c, 5.0, 400.0);
        h.apply_held();
        assert_eq!(h.generation(), 1, "the same range again changes nothing");
    }

    #[test]
    fn a_nonsense_height_is_ignored() {
        let h = heights();
        let c = h.tile_at(3, LON, LAT).expect("in extent");
        h.record(c, f32::NAN);
        assert_eq!(h.height_at(LON, LAT), None, "NaN would poison the clamp");
    }
}
