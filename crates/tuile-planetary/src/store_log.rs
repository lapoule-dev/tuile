// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What changed in what the loader's presence answers read, as the loader
//! saw it happen: the store events of
//! [`TileLoader::store_events`](tuile_core::source::TileLoader::store_events).
//!
//! A mesh's answer is read up its tile's ancestry: the first tile on the way
//! up that is decoded, or that the source has, answers — decoded, or as the
//! terrain store holds it (a tile past the source's data is built from that
//! ancestor). A drape's answer reads, for every imagery tile it would stack,
//! whether it is decoded or stored: the mosaic over the terrain tile at the
//! drape's level, and the floor under it — the deepest decoded ancestor of
//! the tile covering it whole, or the one at the floor level.
//!
//! So a change is noted where it happened, and a resource is touched by the
//! changes its answer reads: a mesh by one on its climb, or by availability
//! revealed at its tile or an ancestor; a drape by a decoded imagery tile
//! kept or let go over its rectangle, or by a stored one over it at its
//! floor level or at a level its mosaic may be at.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use tuile_core::budget::ResourceId;
use tuile_core::raster::GeoRect;
use tuile_terrain::{AvailabilityRange, TileCoord};

/// One change to what the presence answers read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Change {
    /// A decoded mesh kept or let go, or the terrain store's index changed,
    /// at this tile: a mesh whose climb reaches it may answer differently.
    Mesh(TileCoord),
    /// The availability grew by this range of tiles at this level: a mesh
    /// at or under one of them may now be read from a tile of its own climb
    /// rather than an ancestor's.
    Revealed(u32, AvailabilityRange),
    /// An imagery tile at `level` over `rect`: decoded and kept or let go
    /// (`decoded`), or the imagery store's index changed.
    Imagery {
        rect: GeoRect,
        level: u32,
        decoded: bool,
    },
    /// Everything: the imagery served was switched.
    All,
}

/// What [`StoreLog::touched`] reads of the loader now.
pub(crate) trait Reads {
    /// A terrain tile's rectangle.
    fn rect(&self, t: TileCoord) -> GeoRect;
    /// Whether a mesh's climb stops at `t`: decoded, or a tile the source
    /// has.
    fn stops(&self, t: TileCoord) -> bool;
    /// For terrain tile `t`'s drape at the budget's imagery `level`: the
    /// floor level under it, and the levels its mosaic may be at — the
    /// provider's level for `level`, or as coarse as the tile covering `t`
    /// whole when that many tiles are too many.
    fn levels(&self, t: TileCoord, level: u32) -> (u32, u32, u32);
}

/// The changes kept for an asker behind: past this many, an asker further
/// back is told every resource may have changed.
const KEPT: usize = 4_096;

/// The changes, numbered from the loader's first: the change numbered `n`
/// is `changes[n - first]`.
#[derive(Default)]
struct Log {
    first: u64,
    changes: VecDeque<Change>,
    /// What the polled sources stood at when last looked at: the imagery's
    /// generation, the availability's ranges read.
    generation: Option<u64>,
    revealed: Option<usize>,
}

impl Log {
    fn count(&self) -> u64 {
        self.first + self.changes.len() as u64
    }

    fn note(&mut self, c: Change) {
        // The same change twice running says nothing more.
        if self.changes.back() == Some(&c) {
            return;
        }
        self.changes.push_back(c);
        if self.changes.len() > KEPT {
            self.changes.pop_front();
            self.first += 1;
        }
    }
}

/// The loader's store events: noted as each change is made (after it, so
/// an asker that reads the count and then asks sees the change or is told
/// of it), and the two sources that change without the loader — the
/// imagery's generation, the availability the tree folds in — polled.
#[derive(Default)]
pub(crate) struct StoreLog(Mutex<Log>);

impl StoreLog {
    fn log(&self) -> std::sync::MutexGuard<'_, Log> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Notes `c`, made just now.
    pub(crate) fn note(&self, c: Change) {
        self.log().note(c);
    }

    /// The count of changes, after looking at the polled sources: the
    /// imagery serving `generation`, and the ranges folded into the
    /// availability since the ones read, read by `revealed` (given where to
    /// start and where to put them, it says how many there are).
    pub(crate) fn count(
        &self,
        generation: u64,
        revealed: impl FnOnce(usize, &mut Vec<(u32, AvailabilityRange)>) -> usize,
    ) -> u64 {
        let mut log = self.log();
        if log.generation.is_some_and(|g| g != generation) {
            log.note(Change::All);
        }
        log.generation = Some(generation);
        let mut ranges = Vec::new();
        let read = revealed(log.revealed.unwrap_or(usize::MAX), &mut ranges);
        // The first look takes the availability as it stands.
        if log.revealed.is_some() {
            for (level, range) in ranges {
                log.note(Change::Revealed(level, range));
            }
        }
        log.revealed = Some(read);
        log.count()
    }

    /// Pushes onto `touched` those of `asked` the changes numbered `since`
    /// and after may touch (every one when they are no longer all kept),
    /// reading the loader as it is now through `reads`; returns the count.
    pub(crate) fn touched(
        &self,
        since: u64,
        asked: &[ResourceId],
        reads: &dyn Reads,
        touched: &mut Vec<ResourceId>,
    ) -> u64 {
        let log = self.log();
        let count = log.count();
        if since >= count {
            return count;
        }
        if since < log.first {
            touched.extend_from_slice(asked);
            return count;
        }
        let skip = usize::try_from(since - log.first).unwrap_or(usize::MAX);
        let mut all = false;
        let mut at: HashSet<(u32, u64, u64)> = HashSet::new();
        let mut revealed: Vec<(u32, AvailabilityRange)> = Vec::new();
        let mut over: Vec<(GeoRect, u32, bool)> = Vec::new();
        for c in log.changes.iter().skip(skip) {
            match *c {
                Change::All => all = true,
                Change::Mesh(t) => {
                    at.insert((t.level, t.x, t.y));
                }
                Change::Revealed(level, range) => revealed.push((level, range)),
                Change::Imagery {
                    rect,
                    level,
                    decoded,
                } => over.push((rect, level, decoded)),
            }
        }
        drop(log);
        if all {
            touched.extend_from_slice(asked);
            return count;
        }
        for &r in asked {
            let (z, x, y) = r.tile().terrain_coord();
            let hit = match r {
                ResourceId::Mesh(_) => {
                    revealed.iter().any(|&(level, range)| {
                        level <= z && range.contains(x >> (z - level), y >> (z - level))
                    }) || (!at.is_empty() && climb_meets(z, x, y, &at, reads))
                }
                ResourceId::Drape { level: asked, .. } => {
                    let t = TileCoord::new(z, x, y);
                    let tile = reads.rect(t);
                    let mut levels = None;
                    over.iter().any(|(o, level, decoded)| {
                        meet(o, &tile)
                            && (*decoded || {
                                let (floor, lo, hi) =
                                    *levels.get_or_insert_with(|| reads.levels(t, asked));
                                *level == floor || (lo..=hi).contains(level)
                            })
                    })
                }
            };
            if hit {
                touched.push(r);
            }
        }
        count
    }
}

/// Whether the climb from `(z, x, y)` — up to the tile that stops it, that
/// one included — meets a tile in `at`.
fn climb_meets(z: u32, x: u64, y: u64, at: &HashSet<(u32, u64, u64)>, reads: &dyn Reads) -> bool {
    for up in 0..=z {
        let t = (z - up, x >> up, y >> up);
        if at.contains(&t) {
            return true;
        }
        if reads.stops(TileCoord::new(t.0, t.1, t.2)) {
            return false;
        }
    }
    false
}

/// Whether two rectangles meet, edges included and a hair beyond: an
/// imagery tile touching a terrain tile along an edge may be in its mosaic,
/// and naming one too many costs one more answer.
fn meet(a: &GeoRect, b: &GeoRect) -> bool {
    const HAIR: f64 = 1e-9;
    a.west <= b.east + HAIR
        && b.west <= a.east + HAIR
        && a.south <= b.north + HAIR
        && b.south <= a.north + HAIR
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::budget::{LayerId, TileId};

    fn mesh(z: u32, x: u64, y: u64) -> ResourceId {
        ResourceId::Mesh(TileId::from_terrain(z, x, y))
    }

    fn drape(z: u32, x: u64, y: u64) -> ResourceId {
        ResourceId::drape(TileId::from_terrain(z, x, y), LayerId(0), z)
    }

    /// Unit squares: tile `(z, x, y)` spans `[x, x + 1] × [y, y + 1]`
    /// scaled by `2^-z`; a climb stops at the tiles listed; the floor is
    /// level 1, the provider's level the budget's, a tile's covering level
    /// its own.
    struct Square(Vec<(u32, u64, u64)>);

    impl Reads for Square {
        fn rect(&self, t: TileCoord) -> GeoRect {
            let s = 1.0 / f64::from(1u32 << t.level);
            GeoRect {
                west: t.x as f64 * s,
                south: t.y as f64 * s,
                east: (t.x + 1) as f64 * s,
                north: (t.y + 1) as f64 * s,
            }
        }
        fn stops(&self, t: TileCoord) -> bool {
            self.0.contains(&(t.level, t.x, t.y))
        }
        fn levels(&self, t: TileCoord, level: u32) -> (u32, u32, u32) {
            (1, level.min(t.level), level)
        }
    }

    fn touched(
        log: &StoreLog,
        since: u64,
        asked: &[ResourceId],
        reads: &Square,
    ) -> Vec<ResourceId> {
        let mut out = Vec::new();
        log.touched(since, asked, reads, &mut out);
        out
    }

    fn count(log: &StoreLog, generation: u64, ranges: &[(u32, AvailabilityRange)]) -> u64 {
        log.count(generation, |from, out| {
            out.extend_from_slice(ranges.get(from..).unwrap_or(&[]));
            ranges.len()
        })
    }

    fn square(x: u64, y: u64) -> AvailabilityRange {
        AvailabilityRange {
            start_x: x,
            start_y: y,
            end_x: x,
            end_y: y,
        }
    }

    /// **A mesh is touched by a change on its climb, and only there.** The
    /// climb from (3, 5, 2) passes (2, 2, 1) and stops at (1, 1, 0); the
    /// climb from (3, 4, 2) stops at once, at its own tile.
    #[test]
    fn a_mesh_is_touched_by_a_change_on_its_climb() {
        let reads = Square(vec![(1, 1, 0), (3, 4, 2)]);
        let log = StoreLog::default();
        let start = count(&log, 0, &[]);
        let asked = [mesh(3, 5, 2), mesh(3, 4, 2)];
        assert_eq!(touched(&log, start, &asked, &reads), vec![]);
        log.note(Change::Mesh(TileCoord::new(2, 2, 1)));
        assert_eq!(touched(&log, start, &asked, &reads), vec![mesh(3, 5, 2)]);
        // Past the tile that stops the climb: nothing.
        let n = count(&log, 0, &[]);
        log.note(Change::Mesh(TileCoord::new(0, 0, 0)));
        assert_eq!(touched(&log, n, &asked, &reads), vec![]);
        // At the stop itself: the one that climbs there.
        let n = count(&log, 0, &[]);
        log.note(Change::Mesh(TileCoord::new(1, 1, 0)));
        assert_eq!(touched(&log, n, &asked, &reads), vec![mesh(3, 5, 2)]);
    }

    /// **Availability revealed touches the meshes at or under it, and no
    /// drape**: read from the availability, the ranges folded in since the
    /// last look, the first look taking it as it stands.
    #[test]
    fn availability_revealed_touches_the_meshes_under_it() {
        let reads = Square(vec![]);
        let log = StoreLog::default();
        let mut ranges = vec![(1, square(0, 0))];
        let start = count(&log, 0, &ranges);
        assert_eq!(
            count(&log, 0, &ranges),
            start,
            "the first look is no change"
        );
        ranges.push((2, square(2, 1)));
        let n = count(&log, 0, &ranges);
        assert_eq!(n, start + 1);
        let asked = [
            mesh(3, 5, 2),
            mesh(3, 4, 2),
            mesh(2, 2, 1),
            mesh(1, 1, 0),
            drape(3, 5, 2),
        ];
        assert_eq!(
            touched(&log, start, &asked, &reads),
            vec![mesh(3, 5, 2), mesh(3, 4, 2), mesh(2, 2, 1)]
        );
    }

    /// **A drape is touched by a decoded imagery tile over it at any level,
    /// and by a stored one only at its floor level or at its mosaic's
    /// (level 3 here).**
    #[test]
    fn a_drape_is_touched_by_the_imagery_it_reads() {
        let reads = Square(vec![]);
        let log = StoreLog::default();
        let start = count(&log, 0, &[]);
        // Both at level 3, their floor at level 1.
        let asked = [drape(3, 1, 1), drape(3, 7, 7)];
        let over = |level: u32, decoded: bool| Change::Imagery {
            rect: GeoRect {
                west: 0.15,
                south: 0.15,
                east: 0.16,
                north: 0.16,
            },
            level,
            decoded,
        };
        log.note(over(2, true));
        assert_eq!(touched(&log, start, &asked, &reads), vec![drape(3, 1, 1)]);
        for (level, hit) in [(1, true), (2, false), (3, true), (4, false)] {
            let n = count(&log, 0, &[]);
            log.note(over(level, false));
            assert_eq!(
                !touched(&log, n, &asked, &reads).is_empty(),
                hit,
                "a stored tile at level {level}"
            );
        }
    }

    /// **The imagery switched touches everything; an asker further back
    /// than the changes kept is told everything.**
    #[test]
    fn a_switch_or_an_asker_too_far_behind_is_told_everything() {
        let reads = Square(vec![]);
        let log = StoreLog::default();
        let asked = [mesh(3, 5, 2), drape(2, 1, 1)];
        let start = count(&log, 7, &[]);
        let switched = count(&log, 8, &[]);
        assert_eq!(switched, start + 1);
        assert_eq!(touched(&log, start, &asked, &reads), asked.to_vec());
        for i in 0..=KEPT as u64 {
            log.note(Change::Mesh(TileCoord::new(20, i, 0)));
        }
        assert_eq!(touched(&log, switched, &asked, &reads), asked.to_vec());
        assert_eq!(touched(&log, switched + 2, &asked, &reads), vec![]);
    }
}
