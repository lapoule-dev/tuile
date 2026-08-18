// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Resident-content cache: an LRU bounded by a tile count, with a byte ceiling
//! behind it.
//!
//! The shape is the reference implementation's `TileReplacementQueue`: entries
//! ordered by when they were last used, a cap on how many may stay, and a sweep
//! from the least-recently-used end that stops the moment the cap holds. What
//! the current pass touched is never a victim, even if that leaves the cache
//! over its cap — the reference is explicit that going over beats dropping
//! something in use.
//!
//! The count is what actually bounds a session, and the bytes are a backstop.
//! Size does not distinguish a level-19 tile nobody is looking at from a
//! level-3 tile covering a continent, so a byte budget alone never singles the
//! first one out: measured, one session held 3386 MiB of imagery and evicted
//! nothing at all. Recency does distinguish them, which is the whole argument
//! for an LRU over a high-water mark.

use crate::raster::ImageryCoord;
use crate::source::TileId;
use std::collections::{HashMap, HashSet};

/// Where a sweep starts, as a fraction of a ceiling.
///
/// Below the ceiling on purpose: waiting for the cache to be full means every
/// eviction happens under pressure, in the same pass as the loads that caused
/// it, so the sweep competes with exactly the work it is meant to make room
/// for. Starting early means reclaiming happens in the quiet between bursts.
const HIGH_WATER: f64 = 0.9;

/// Where a sweep stops, as a fraction of a ceiling.
///
/// Clearing a margin rather than stopping at the mark makes eviction occasional
/// instead of continuous: stop exactly at the trigger and the next insert trips
/// it again, forever. The gap between the two marks is the hysteresis.
const LOW_WATER: f64 = 0.8;

#[derive(Debug, Clone)]
struct Entry {
    size: usize,
    /// The tile's level **as the tree gave it**, never decoded from the id.
    ///
    /// A [`TileId`] is an opaque handle whose payload only the owning
    /// [`TileTree`] can read — for a 3D Tiles arena it is an *index*, not a
    /// quadtree coordinate. This cache used to call `terrain_coord()` on it,
    /// which shifts right by 54 and so answered level 0 for every arena index
    /// below 2^54. With a pinned floor set, `is_pinned` was then true for
    /// every tile in the tileset: the sweep never found a victim, the residency
    /// grew for the life of the session, and `trim_except` re-sorted the whole
    /// map on every insert to evict nothing.
    ///
    /// See [`crate::source::TileTree::level`].
    level: u32,
    last_used: u64,
    /// The pass that last touched this tile. Compared against
    /// [`ResidentCache::pass`] to answer "was this used *now*", which is a
    /// different question from "was this used recently" and the only one that
    /// makes a sweep safe.
    touched_pass: u64,
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

#[derive(Debug)]
pub struct ResidentCache {
    /// A backstop, not the working bound. See [`ResidentCache::tile_limit`].
    budget: usize,
    /// How many tiles may stay, whatever they weigh — the reference
    /// implementation's `tileCacheSize`, and what actually bounds a session.
    /// See [`crate::traversal::Config::resident_tile_limit`].
    tile_limit: usize,
    /// Levels at or above this are never evicted. See
    /// [`crate::traversal::Config::pinned_level`].
    pinned_level: Option<u32>,
    /// Geometry, charged per tile.
    used: usize,
    tick: u64,
    /// Which traversal pass is current. See [`ResidentCache::start_pass`].
    pass: u64,
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
        Self::with_limits(budget_bytes, usize::MAX)
    }

    pub fn with_limits(budget_bytes: usize, tile_limit: usize) -> Self {
        Self::pinning(budget_bytes, tile_limit, None)
    }

    /// As [`ResidentCache::with_limits`], plus a level that is never reclaimed.
    pub fn pinning(budget_bytes: usize, tile_limit: usize, pinned_level: Option<u32>) -> Self {
        Self {
            budget: budget_bytes,
            tile_limit,
            pinned_level,
            used: 0,
            tick: 0,
            pass: 0,
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
        // Coarse imagery outlives whatever happened to be holding it.
        //
        // A level-5 texture is the floor of the layer stack — the thing that
        // shows through wherever a sharp tile is missing — and it is typically
        // referenced by one deep tile at a time. Freeing it when that tile is
        // evicted means the safety net is collected exactly when the churn that
        // needs it begins. There are at most 4^5 of them for the whole planet,
        // so keeping every one ever fetched is bounded and small.
        let pinned = self.pinned_level;
        for coord in held {
            let Some(entry) = self.imagery.get_mut(coord) else {
                continue;
            };
            entry.refs = entry.refs.saturating_sub(1);
            let evictable = pinned.is_none_or(|floor| coord.level > floor);
            if entry.refs == 0 && evictable {
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
        let (tick, pass) = (self.tick, self.pass);
        if let Some(e) = self.entries.get_mut(&t) {
            e.last_used = tick;
            e.touched_pass = pass;
        }
    }

    /// Whether this tile sits at or above the pinned level, and so may never be
    /// reclaimed. Reads the level the tree supplied at insertion — see
    /// [`Entry::level`].
    fn is_pinned(&self, level: u32) -> bool {
        self.pinned_level.is_some_and(|floor| level <= floor)
    }

    /// Opens a new pass. Everything touched from here until the next call is
    /// off limits to a sweep.
    ///
    /// The reference implementation's `markStartOfRenderFrame`: it snapshots
    /// the head of its queue, and `trimTiles` refuses to walk past that mark —
    /// "*Tiles that were used last frame will not be unloaded, even if that
    /// puts the number of tiles above the specified maximum*". Going over the
    /// ceiling is the lesser evil; reclaiming something the camera is looking
    /// at is not a trade-off, it is a hole.
    pub fn start_pass(&mut self) {
        self.pass += 1;
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
        level: u32,
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
                level,
                last_used: self.tick,
                // What has just arrived was, by definition, asked for by the
                // pass that is running: it may not be swept out from under it.
                touched_pass: self.pass,
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

    /// How many tiles are resident.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether either ceiling has reached its high-water mark.
    fn over_high_water(&self) -> bool {
        self.used_bytes() as f64 > self.budget as f64 * HIGH_WATER
            || self.entries.len() as f64 > self.tile_limit as f64 * HIGH_WATER
    }

    /// Whether both ceilings are back under their low-water marks.
    fn under_low_water(&self) -> bool {
        (self.used_bytes() as f64) <= self.budget as f64 * LOW_WATER
            && (self.entries.len() as f64) <= self.tile_limit as f64 * LOW_WATER
    }

    /// Evicts, worst first, from the high-water mark down to the low-water one.
    ///
    /// **Whether** to sweep is the ceilings, and a sweep starts before either is
    /// reached — waiting for full means every eviction lands in the same pass as
    /// the loads that caused it. **Who** goes is least-recently-used, and
    /// nothing else.
    ///
    /// Two sets are untouchable. `protected` — what this pass selected, its
    /// ancestors, what it asked for — and anything [`ResidentCache::touch`]ed
    /// during the current pass, even if `protected` does not name it. Both hold
    /// even when that leaves the cache over its ceilings: going over budget is a
    /// trade-off, reclaiming ground the camera is looking at is a hole.
    ///
    /// A residency can be well under the bytes and far over the count — ten
    /// thousand deep tiles weigh less than a few hundred coarse ones — so either
    /// ceiling is enough to start a sweep, and both must clear to stop one.
    ///
    /// [`Session`]: crate::runtime
    fn trim_except(&mut self, protected: &HashSet<TileId>, keep: Option<TileId>) -> Vec<TileId> {
        let mut evicted = Vec::new();
        if !self.over_high_water() {
            return evicted;
        }
        // Ordered once, not searched per eviction: a sweep can take hundreds of
        // tiles, and a scan of the whole residency for each of them is the
        // difference between a sweep costing nothing and a sweep costing a
        // frame.
        let pass = self.pass;
        let mut victims: Vec<(TileId, u32, u64)> = self
            .entries
            .iter()
            .filter(|(id, e)| {
                !protected.contains(id)
                    && Some(**id) != keep
                    && e.touched_pass != pass
                    && !self.is_pinned(e.level)
            })
            .map(|(id, e)| (*id, e.level, e.last_used))
            .collect();
        // **Deepest first, then least recently used.**
        //
        // The depth term is a deliberate departure from the reference, whose
        // queue is 119 lines and mentions neither level nor distance nor error.
        // It earns its place on one asymmetry: what a tile is worth to the
        // picture is not what it costs to hold. A level-18 tile covers a few
        // hundred metres and stands in for nothing; a level-3 tile covers a
        // continent and is what *every* unready tile beneath it falls back to.
        // Reclaiming the deep one costs a patch of sharpness somewhere nobody
        // is looking; reclaiming the coarse one can take ground off the screen
        // across a whole region.
        //
        // A weight of `1/screen-space-error` sat here once and did exactly
        // that, in the opposite direction: under a tilted camera the distance
        // term made most of the frame the most depletable thing in the cache,
        // and the coarse ancestor everything falls back to is by construction
        // the lowest-error tile there is — so it went first of all. Measured: a
        // selection of 866 tiles collapsed to 1 in a single frame, with 1792
        // tiles sitting ready on the GPU and undrawn. The lesson was not "never
        // consider level"; it was "never make the fallback the first victim".
        //
        // Recency still decides *within* a level, so a session that stays at
        // one depth behaves exactly as it did before.
        //
        // Imagery follows for free. Textures are freed by reference count when
        // the last tile naming them goes, so evicting the deep tiles first
        // releases the fine textures first — which is where the memory actually
        // is: measured at 1.14 GB of imagery against 6.9 MB of geometry.
        victims.sort_by(|a, b| {
            b.1.cmp(&a.1) // deepest level first
                .then(a.2.cmp(&b.2)) // then least recently used
                .then(a.0.cmp(&b.0)) // then by id, so a sweep is reproducible
        });
        for (id, _, _) in victims {
            // Checked as we go: evicting a tile always frees its geometry but
            // frees its imagery only if it was the last to hold it, so how far a
            // sweep gets per tile is not known in advance.
            if self.under_low_water() {
                break;
            }
            self.remove(id);
            evicted.push(id);
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    /// Inserts with the level read out of the handle.
    ///
    /// Legitimate **here and nowhere else**: every fixture in this module builds
    /// its ids with [`TileId::from_terrain`], so the level really is packed in
    /// them. Production takes it from [`crate::source::TileTree::level`],
    /// because a handle from any other tree does not carry one.
    fn insert(
        c: &mut ResidentCache,
        t: TileId,
        size: usize,
        imagery: &[(ImageryCoord, usize)],
        protected: &HashSet<TileId>,
    ) -> Vec<TileId> {
        c.insert(t, t.terrain_coord().0, size, imagery, protected)
    }

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

    /// A sweep starts at the high-water mark, not at the ceiling. Waiting for
    /// full means every eviction happens in the same pass as the loads that
    /// filled the cache, so reclaiming competes with exactly the work it is
    /// making room for.
    #[test]
    fn a_sweep_starts_before_the_ceiling_is_reached() {
        // 100 bytes: high water 90, low water 80.
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        assert!(insert(&mut c, t(1), 60, NO_IMAGERY, &none).is_empty());
        assert!(
            insert(&mut c, t(2), 25, NO_IMAGERY, &none).is_empty(),
            "85 bytes is under the high-water mark: still nothing to do"
        );
        // A later pass: what arrived above is now history, not in use.
        c.start_pass();

        // 95 bytes — under the 100-byte ceiling, over the 90-byte mark.
        let evicted = insert(&mut c, t(3), 10, NO_IMAGERY, &none);
        assert_eq!(evicted, vec![t(1)], "the sweep fired without being full");
        assert!(
            c.used_bytes() <= 80,
            "and ran to the low-water mark, left {} bytes",
            c.used_bytes()
        );
    }

    #[test]
    fn touch_changes_eviction_order() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        insert(&mut c, t(1), 50, NO_IMAGERY, &none);
        insert(&mut c, t(2), 50, NO_IMAGERY, &none);
        c.start_pass();
        c.touch(t(1)); // t2 becomes the LRU
        let evicted = insert(&mut c, t(3), 50, NO_IMAGERY, &none);
        // Order, not count: a sweep clears down to the low-water mark, so how
        // many go depends on their sizes. What this pins is that the touched
        // tile is not the one taken first.
        assert_eq!(evicted.first(), Some(&t(2)));
    }

    /// The gap between the marks is hysteresis. Stop a sweep at the mark that
    /// triggered it and the next insert trips it again, forever; clearing a
    /// margin makes eviction occasional instead of continuous.
    #[test]
    fn a_sweep_clears_a_margin_so_the_next_insert_is_free() {
        // 1000 bytes: high water 900, low water 800.
        let mut c = ResidentCache::new(1000);
        let none = HashSet::new();
        for i in 1..=9 {
            insert(&mut c, t(i), 100, NO_IMAGERY, &none);
        }
        assert_eq!(
            c.used_bytes(),
            900,
            "at the mark, not over it: no sweep yet"
        );
        // A later pass: what arrived above is history, not in use.
        c.start_pass();

        insert(&mut c, t(10), 100, NO_IMAGERY, &none);
        assert_eq!(
            c.used_bytes(),
            800,
            "the sweep must run to the low-water mark"
        );

        // Having cleared it, the next insert costs nothing. That is the whole
        // point of the two marks being different numbers.
        let quiet = insert(&mut c, t(11), 100, NO_IMAGERY, &none);
        assert!(
            quiet.is_empty(),
            "insert right after a sweep should not evict, took {quiet:?}"
        );
    }

    /// A tile the current pass has touched is never a victim, even when that
    /// leaves the cache over its ceiling.
    ///
    /// This is the reference implementation's `markStartOfRenderFrame` barrier,
    /// and it is the difference between a degraded picture and a hole. Without
    /// it a sweep can reclaim ground the camera is looking at *right now*, and
    /// the coarse ancestor everything unready falls back to goes with it —
    /// measured in the viewer as a selection of 866 tiles collapsing to 1 in one
    /// frame, with 1792 tiles ready on the GPU and undrawn.
    /// The floor of the fallback chain is never reclaimed, geometry or imagery.
    ///
    /// Without this the guarantee is empty: an ancestor walk reaches level 5,
    /// finds nothing, and the ground is bare — which is the black square. The
    /// count is what makes it affordable: `4^5` is 1024 tiles for the whole
    /// planet, and only those under ground the camera has visited are ever
    /// fetched.
    #[test]
    fn the_pinned_level_survives_any_pressure() {
        // One tile of room, so every sweep is as aggressive as it can be.
        let mut c = ResidentCache::pinning(100, 1, Some(5));
        let none = HashSet::new();
        let coarse = TileId::from_terrain(4, 3, 3);
        let deep = TileId::from_terrain(18, 7, 7);
        let shared = &[(img(1), 40)][..];

        insert(&mut c, coarse, 40, shared, &none);
        c.start_pass();
        // Enough deep tiles to force sweep after sweep.
        for n in 0..8 {
            insert(
                &mut c,
                TileId::from_terrain(18, n, 0),
                40,
                NO_IMAGERY,
                &none,
            );
            c.start_pass();
        }
        assert!(c.contains(coarse), "the pinned tile was reclaimed");

        // And its imagery outlives the last deep tile that referenced it.
        insert(&mut c, deep, 40, shared, &none);
        c.start_pass();
        c.remove(deep);
        c.remove(coarse);
        assert_eq!(
            c.imagery_bytes(),
            (1, 40),
            "pinned imagery must outlive every holder"
        );
    }

    /// **A pinned floor pins a floor, not the whole tileset.**
    ///
    /// A [`TileId`] is opaque, and only the tree that made it can say how deep
    /// it sits. This cache used to ask the *handle* — `terrain_coord()`, which
    /// shifts right by 54 — and a 3D Tiles arena index is a small integer, so
    /// every tile in a tileset answered level 0. With the default
    /// `pinned_level: Some(5)` that made `is_pinned` true for all of them: the
    /// sweep never found a victim, and the residency grew for the whole
    /// session while `trim_except` re-sorted the entire map on every insert to
    /// evict nothing.
    ///
    /// Arena-style ids here on purpose — small consecutive integers, exactly
    /// what [`crate::tileset::Tileset`] hands out.
    #[test]
    fn an_arena_index_is_not_a_level() {
        let mut c = ResidentCache::pinning(100, 1, Some(5));
        let none = HashSet::new();
        // Deep tiles of a tileset: the tree calls them level 9, their handles
        // are the indices 1..9.
        for n in 1..9u64 {
            c.insert(TileId(n), 9, 40, NO_IMAGERY, &none);
            c.start_pass();
        }
        assert!(
            c.len() < 8,
            "nothing was reclaimed: {} tiles resident under a limit of 1, \
             because every arena index read as level 0 and so as pinned",
            c.len()
        );

        // And the floor still holds when the tree really does say it is coarse.
        let mut c = ResidentCache::pinning(100, 1, Some(5));
        c.insert(TileId(100), 4, 40, NO_IMAGERY, &none);
        c.start_pass();
        for n in 1..9u64 {
            c.insert(TileId(n), 9, 40, NO_IMAGERY, &none);
            c.start_pass();
        }
        assert!(
            c.contains(TileId(100)),
            "the tile the tree put at level 4 was reclaimed under a floor of 5"
        );
    }

    #[test]
    fn a_tile_touched_this_pass_survives_a_full_cache() {
        let mut c = ResidentCache::new(1000);
        let none = HashSet::new();
        for i in 1..=9 {
            insert(&mut c, t(i), 100, NO_IMAGERY, &none);
        }

        // Every one of them is in use *now*. Note they are touched in order, so
        // the recency ranking among them is unchanged — a sweep that only knew
        // about recency would still happily take the first few. Only the
        // barrier can save them.
        c.start_pass();
        for i in 1..=9 {
            c.touch(t(i));
        }

        let evicted = insert(&mut c, t(10), 100, NO_IMAGERY, &none);
        assert!(
            evicted.is_empty(),
            "the sweep took ground the pass is using: {evicted:?}"
        );
        assert!(
            c.used_bytes() > 900,
            "and it must go over its ceiling to do so, which is the trade the \
             reference implementation makes explicitly"
        );
    }

    /// The barrier lasts one pass, not for ever: open the next one and the same
    /// tiles are ordinary LRU candidates again. Otherwise nothing is ever
    /// evictable and the ceiling bounds nothing.
    #[test]
    fn the_barrier_lifts_on_the_next_pass() {
        // 300 bytes: high water 270, low water 240, so a third 100-byte tile
        // forces exactly one eviction.
        let mut c = ResidentCache::new(300);
        let none = HashSet::new();
        insert(&mut c, t(1), 100, NO_IMAGERY, &none);
        insert(&mut c, t(2), 100, NO_IMAGERY, &none);

        c.start_pass();
        c.touch(t(1)); // in use *now*, and incidentally the most recent
        let evicted = insert(&mut c, t(3), 100, NO_IMAGERY, &none);
        assert_eq!(evicted, vec![t(2)], "the tile in use was spared");

        // Next pass: t1 was not touched, so it is an ordinary candidate again —
        // and being the oldest of what is left, it goes first.
        c.start_pass();
        let evicted = insert(&mut c, t(4), 100, NO_IMAGERY, &none);
        assert_eq!(
            evicted,
            vec![t(1)],
            "the barrier must last one pass, or nothing is ever evictable"
        );
    }

    /// Order is least-recently-used, and only that.
    #[test]
    fn tiles_go_least_recently_used_first() {
        let mut c = ResidentCache::new(1000);
        let none = HashSet::new();
        for i in 1..=9 {
            insert(&mut c, t(i), 100, NO_IMAGERY, &none);
        }
        c.start_pass();
        c.touch(t(1)); // now the most recently used, though the first inserted

        let evicted = insert(&mut c, t(10), 100, NO_IMAGERY, &none);
        assert_eq!(
            evicted,
            vec![t(2), t(3)],
            "the touched tile survived and the next-oldest went instead"
        );
    }

    /// The count sweeps on its own account. A residency can sit far under its
    /// byte ceiling and still hold far too many tiles — ten thousand deep tiles
    /// weigh less than a few hundred coarse ones — and the count is the ceiling
    /// that bounds a long session.
    #[test]
    fn the_tile_count_sweeps_with_bytes_to_spare() {
        // 10 tiles: high water 9, low water 8. Bytes effectively unlimited.
        let mut c = ResidentCache::with_limits(1_000_000, 10);
        let none = HashSet::new();
        for i in 1..=9 {
            insert(&mut c, t(i), 1, NO_IMAGERY, &none);
        }
        assert_eq!(c.len(), 9, "at the mark, nothing swept");
        c.start_pass();

        let evicted = insert(&mut c, t(10), 1, NO_IMAGERY, &none);
        assert_eq!(evicted.len(), 2, "swept to the low-water count");
        assert_eq!(c.len(), 8);
        assert!(
            c.used_bytes() < 100,
            "bytes were never remotely the reason: {} used",
            c.used_bytes()
        );
    }

    #[test]
    fn selected_tiles_are_never_evicted() {
        let mut c = ResidentCache::new(100);
        let protected: HashSet<TileId> = [t(1), t(2)].into();
        insert(&mut c, t(1), 60, NO_IMAGERY, &protected);
        insert(&mut c, t(2), 60, NO_IMAGERY, &protected); // over budget, but both protected
        let evicted = insert(&mut c, t(3), 60, NO_IMAGERY, &protected);
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
        insert(&mut c, t(1), 10, shared, &none);
        insert(&mut c, t(2), 10, shared, &none);
        insert(&mut c, t(3), 10, shared, &none);
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
        insert(&mut c, t(1), 10, shared, &none);
        insert(&mut c, t(2), 10, shared, &none);

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
        insert(&mut c, t(1), 10, repeated, &none);
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
            insert(&mut c, t(i), 10, &[(img(i), 200)], &none);
            c.start_pass();
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
        insert(&mut c, t(1), 10, same, &none);
        insert(&mut c, t(1), 20, same, &none);
        assert_eq!(c.imagery_bytes(), (1, 1000));
        assert_eq!(c.used_bytes(), 20 + 1000);
    }

    #[test]
    fn reinsert_replaces_size() {
        let mut c = ResidentCache::new(100);
        let none = HashSet::new();
        insert(&mut c, t(1), 80, NO_IMAGERY, &none);
        insert(&mut c, t(1), 20, NO_IMAGERY, &none);
        assert_eq!(c.used_bytes(), 20);
    }
}
