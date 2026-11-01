// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What changed in what the loader's presence answers read, as the loader
//! saw it happen: the store events of
//! [`TileLoader::store_events`](tuile_core::source::TileLoader::store_events).
//!
//! A mesh's answer reads the decoded meshes, the availability and the
//! terrain store at the tile and up its ancestry (a tile past the source's
//! data is built from an ancestor); a drape's reads the decoded imagery and
//! the imagery store for every imagery tile over the terrain tile's
//! rectangle, the floor under it included. So a change is noted where it
//! happened — a terrain coordinate, an imagery tile's rectangle — and a
//! resource is touched by the changes its answer reads: a mesh by one at
//! its tile or an ancestor, a drape by one whose rectangle meets its tile's.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use tuile_core::budget::ResourceId;
use tuile_core::raster::GeoRect;
use tuile_terrain::TileCoord;

/// One change to what the presence answers read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Change {
    /// A decoded mesh kept or dropped, or the terrain store's index changed,
    /// at this tile: every mesh at it or below it may answer differently.
    Mesh(TileCoord),
    /// The availability grew: any mesh may now be read from its own tile
    /// rather than built from an ancestor's.
    Meshes,
    /// A decoded imagery tile kept or dropped, or the imagery store's index
    /// changed, over this rectangle.
    Imagery(GeoRect),
    /// Everything: the imagery served was switched.
    All,
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
    /// generation, the availability's ranges.
    generation: Option<u64>,
    ranges: Option<usize>,
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

/// The loader's store events: noted as each change is made (after it, so an
/// asker that reads the count and then asks sees the change or is told of
/// it), and the two sources that change without telling the loader — the
/// imagery's generation, the availability folded in by the tree — polled.
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
    /// imagery serving `generation`, the availability holding `ranges`.
    pub(crate) fn count(&self, generation: u64, ranges: usize) -> u64 {
        let mut log = self.log();
        if log.generation.is_some_and(|g| g != generation) {
            log.note(Change::All);
        }
        log.generation = Some(generation);
        if log.ranges.is_some_and(|r| r != ranges) {
            log.note(Change::Meshes);
        }
        log.ranges = Some(ranges);
        log.count()
    }

    /// Pushes onto `touched` those of `asked` the changes numbered `since`
    /// and after may touch (every one when they are no longer all kept);
    /// returns the count. `rect` is a terrain tile's rectangle.
    pub(crate) fn touched(
        &self,
        since: u64,
        asked: &[ResourceId],
        rect: impl Fn(TileCoord) -> GeoRect,
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
        let (mut all, mut meshes) = (false, false);
        let mut at: HashSet<(u32, u64, u64)> = HashSet::new();
        let mut over: Vec<GeoRect> = Vec::new();
        for c in log.changes.iter().skip(skip) {
            match *c {
                Change::All => all = true,
                Change::Meshes => meshes = true,
                Change::Mesh(t) => {
                    at.insert((t.level, t.x, t.y));
                }
                Change::Imagery(r) => over.push(r),
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
                    meshes || (0..=z).any(|up| at.contains(&(z - up, x >> up, y >> up)))
                }
                ResourceId::Drape { .. } => {
                    let tile = rect(TileCoord::new(z, x, y));
                    over.iter().any(|o| meet(o, &tile))
                }
            };
            if hit {
                touched.push(r);
            }
        }
        count
    }
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

    /// A unit square per tile at its level: tile `(z, x, y)` spans
    /// `[x, x + 1] × [y, y + 1]` scaled by `2^-z`.
    fn rect(t: TileCoord) -> GeoRect {
        let s = 1.0 / f64::from(1u32 << t.level);
        GeoRect {
            west: t.x as f64 * s,
            south: t.y as f64 * s,
            east: (t.x + 1) as f64 * s,
            north: (t.y + 1) as f64 * s,
        }
    }

    fn touched(log: &StoreLog, since: u64, asked: &[ResourceId]) -> (Vec<ResourceId>, u64) {
        let mut out = Vec::new();
        let n = log.touched(since, asked, rect, &mut out);
        (out, n)
    }

    /// **A mesh is touched by a change at its tile or an ancestor, a drape
    /// by one over its rectangle, and nothing by what came before `since`.**
    #[test]
    fn a_change_touches_what_reads_it() {
        let log = StoreLog::default();
        let start = log.count(0, 1);
        let asked = [mesh(3, 5, 2), mesh(3, 4, 2), drape(2, 1, 1), drape(2, 3, 3)];
        assert_eq!(touched(&log, start, &asked), (vec![], start), "nothing yet");
        // A decoded mesh at the parent of (3, 5, 2) and (3, 4, 2).
        log.note(Change::Mesh(TileCoord::new(2, 2, 1)));
        let (t, n) = touched(&log, start, &asked);
        assert_eq!(t, vec![mesh(3, 5, 2), mesh(3, 4, 2)]);
        // An imagery tile over (2, 1, 1) only.
        log.note(Change::Imagery(GeoRect {
            west: 0.3,
            south: 0.3,
            east: 0.4,
            north: 0.4,
        }));
        assert_eq!(touched(&log, n, &asked).0, vec![drape(2, 1, 1)]);
        // A mesh at a sibling touches neither.
        let at = log.count(0, 1);
        log.note(Change::Mesh(TileCoord::new(3, 6, 2)));
        assert_eq!(touched(&log, at, &asked).0, vec![]);
    }

    /// **The polled sources:** the availability growing touches every mesh
    /// and no drape; the imagery switched touches everything.
    #[test]
    fn the_polled_sources_touch_what_they_change() {
        let log = StoreLog::default();
        let asked = [mesh(3, 5, 2), drape(2, 1, 1)];
        let start = log.count(7, 1);
        assert_eq!(log.count(7, 1), start, "nothing moved");
        let grown = log.count(7, 2);
        assert_eq!(grown, start + 1);
        assert_eq!(touched(&log, start, &asked).0, vec![mesh(3, 5, 2)]);
        let switched = log.count(8, 2);
        assert_eq!(touched(&log, grown, &asked), (asked.to_vec(), switched));
    }

    /// **An asker further back than the changes kept is told every resource
    /// may have changed.**
    #[test]
    fn an_asker_too_far_behind_is_told_everything() {
        let log = StoreLog::default();
        let start = log.count(0, 1);
        for i in 0..=KEPT as u64 {
            log.note(Change::Mesh(TileCoord::new(20, i, 0)));
        }
        let asked = [drape(2, 1, 1)];
        assert_eq!(touched(&log, start, &asked).0, asked.to_vec());
        assert_eq!(touched(&log, start + 1, &asked).0, vec![]);
    }
}
