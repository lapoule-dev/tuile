// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A layer: one source, one grid, one tile format, one lifetime.
//!
//! A layer is split into **zones** so that no archive grows without bound: a
//! tile at or below the zone level belongs to the zone of its ancestor at that
//! level. A tile coarser than the zone level spans several zones — putting it
//! in one would be arbitrary and in each would duplicate it — so those share a
//! single `top` zone per layer. It stays small: there are only ~350 000 tiles
//! above z10 for the whole globe, and only the ones actually asked for exist.
//!
//! Time is cut into **epochs**. An archive only ever holds tiles fetched in one
//! epoch, and compaction never merges across epochs, so a layer that must
//! forget its tiles after a while (an imagery provider's terms) can drop whole
//! archives by age — a lifecycle rule on the bucket does it — without rewriting
//! anything. A durable layer has a single epoch.

use std::ops::RangeInclusive;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use pmtiles::{Compression, TileType};

use crate::grid::{Grid, OutOfGrid};

/// The epoch of a layer that never expires.
pub const DURABLE_EPOCH: &str = "d";

/// One source of tiles and how its tiles are kept.
#[derive(Debug, Clone)]
pub struct Layer {
    /// The path segment and the key prefix: `bing-aerial`, `terrain`.
    pub name: String,
    pub grid: Grid,
    /// What the bytes are, as written in the archive header.
    pub tile_type: TileType,
    /// How the stored bytes are compressed. They are stored exactly as the
    /// source delivered them; this only describes them.
    pub tile_compression: Compression,
    /// Tiles at this level and below are split into zones of this level.
    pub zone_level: u8,
    /// After how long a tile must be fetched again. `None`: never.
    pub expiry: Option<Duration>,
    /// The media type a server answers with.
    pub content_type: String,
    /// Whether each archive of the layer is indexed for the ledger as it is
    /// written: its tile ids and a stamp of each tile, for a register that a
    /// reader naming tiles without reading them asks (see [`crate::index`]).
    /// Its absence sibling follows it.
    pub index: bool,
}

/// Where a tile lives within its layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Zone {
    /// Tiles coarser than the zone level.
    Top,
    /// The zone whose tile at the zone level is `(x, y)`.
    Cell { x: u32, y: u32 },
}

impl Layer {
    /// The zone of a tile, after checking it against the grid.
    pub fn zone_of(&self, level: u8, x: u32, y: u32) -> Result<Zone, OutOfGrid> {
        self.grid.check(level, x, y)?;
        if level < self.zone_level {
            return Ok(Zone::Top);
        }
        let shift = level - self.zone_level;
        Ok(Zone::Cell {
            x: x >> shift,
            y: y >> shift,
        })
    }

    /// The key prefix of a zone: `bing-aerial/zones/z10/522/373`.
    pub fn zone_prefix(&self, zone: Zone) -> String {
        match zone {
            Zone::Top => format!("{}/top", self.name),
            Zone::Cell { x, y } => format!("{}/zones/z{}/{x}/{y}", self.name, self.zone_level),
        }
    }

    /// Every archive tile id a tile of `zone` can have: one range per
    /// archive level, coarsest first.
    ///
    /// An archive numbers its tiles level by level along a Hilbert curve,
    /// which visits every square of the quadtree whole before the next: the
    /// tiles under one zone tile are, at each finer level, one run of ids.
    /// `top` holds every level coarser than the zone level: one run.
    pub fn zone_id_ranges(&self, zone: Zone) -> Vec<RangeInclusive<u64>> {
        // The archive level a source level is stored at.
        let offset = match self.grid {
            Grid::WebMercator => 0u8,
            Grid::Geographic => 1,
        };
        let deepest = self.grid.max_level() + offset;
        let zone_z = self.zone_level + offset;
        match zone {
            Zone::Top => {
                if self.zone_level == 0 {
                    return Vec::new();
                }
                std::iter::once(level_base(offset)..=level_base(zone_z) - 1).collect()
            }
            Zone::Cell { x, y } => {
                let Ok(first) = self.grid.to_id(self.zone_level, x, y) else {
                    return Vec::new();
                };
                let along = first - level_base(zone_z);
                (zone_z..=deepest)
                    .map(|z| {
                        let span = 1u64 << (2 * u32::from(z - zone_z));
                        let start = level_base(z) + along * span;
                        start..=start + span - 1
                    })
                    .collect()
            }
        }
    }

    /// The epoch a tile fetched at `now` belongs to: the calendar month for an
    /// expiring layer, [`DURABLE_EPOCH`] otherwise.
    pub fn epoch(&self, now: SystemTime) -> String {
        match self.expiry {
            None => DURABLE_EPOCH.to_string(),
            Some(_) => {
                let t: DateTime<Utc> = now.into();
                format!("{:04}{:02}", t.year(), t.month())
            }
        }
    }

    /// The instant every tile of `epoch` has outlived the layer's expiry —
    /// the end of the epoch, plus the expiry. `None` for a layer whose tiles
    /// do not expire, or an epoch that is not a month.
    pub fn expires_at(&self, epoch: &str) -> Option<SystemTime> {
        Some(epoch_end(epoch)? + self.expiry?)
    }

    /// Is every tile of `epoch` past its expiry at `now`? Measured from the
    /// end of the epoch, so no tile is dropped before its full lifetime.
    pub fn is_expired(&self, epoch: &str, now: SystemTime) -> bool {
        let Some(expiry) = self.expiry else {
            return false;
        };
        let Some(end) = epoch_end(epoch) else {
            return false;
        };
        now.duration_since(end)
            .map(|age| age > expiry)
            .unwrap_or(false)
    }
}

/// The first archive tile id of level `z`: the tiles of every coarser level
/// come before, `(4^z − 1) / 3` of them.
pub fn level_base(z: u8) -> u64 {
    ((1u64 << (2 * u32::from(z))) - 1) / 3
}

/// A monthly epoch is `YYYYMM`.
const YEAR_DIGITS: usize = 4;
const EPOCH_DIGITS: usize = YEAR_DIGITS + 2;
const DECEMBER: u32 = 12;

/// The first instant after a monthly epoch `YYYYMM`.
fn epoch_end(epoch: &str) -> Option<SystemTime> {
    if epoch.len() != EPOCH_DIGITS {
        return None;
    }
    let year: i32 = epoch.get(..YEAR_DIGITS)?.parse().ok()?;
    let month: u32 = epoch.get(YEAR_DIGITS..)?.parse().ok()?;
    let (ny, nm) = if month == DECEMBER {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next = NaiveDate::from_ymd_opt(ny, nm, 1)?
        .and_hms_opt(0, 0, 0)?
        .and_utc();
    Some(next.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(grid: Grid, zone_level: u8) -> Layer {
        Layer {
            name: "l".into(),
            grid,
            tile_type: TileType::Png,
            tile_compression: Compression::None,
            zone_level,
            expiry: None,
            content_type: "image/png".into(),
            index: true,
        }
    }

    #[test]
    fn a_level_s_ids_start_where_the_archive_says() {
        for z in 0..=12u8 {
            let n = 1u32 << z;
            let ids: Vec<u64> = (0..n)
                .flat_map(|x| (0..n).map(move |y| (x, y)))
                .map(|(x, y)| {
                    pmtiles::TileId::from(pmtiles::TileCoord::new(z, x, y).expect("coord")).value()
                })
                .collect();
            assert_eq!(ids.iter().min(), Some(&level_base(z)), "z{z}");
            if z < 12 {
                assert_eq!(ids.iter().max(), Some(&(level_base(z + 1) - 1)), "z{z}");
            }
        }
    }

    /// Every tile of every zone falls within the zone's ranges, and every
    /// id of a zone's ranges (down to where it is checked) is a tile of
    /// that zone: on both grids.
    #[test]
    #[ignore = "slow: every id of a zone's ranges, on both grids, ~21 s in debug; run with `cargo nextest run --profile slow --run-ignored only`"]
    fn a_zone_s_tiles_are_exactly_its_id_ranges() {
        for grid in [Grid::WebMercator, Grid::Geographic] {
            let l = layer(grid, 3);
            let mut zones: Vec<Zone> = vec![Zone::Top];
            let (w, h) = grid.size(3);
            for x in 0..w as u32 {
                for y in 0..h as u32 {
                    zones.push(Zone::Cell { x, y });
                }
            }
            let deepest = 7u8;
            for level in 0..=deepest {
                let (w, h) = grid.size(level);
                for x in 0..w as u32 {
                    for y in 0..h as u32 {
                        let id = grid.to_id(level, x, y).expect("id");
                        let zone = l.zone_of(level, x, y).expect("zone");
                        for z in &zones {
                            let inside = l.zone_id_ranges(*z).iter().any(|r| r.contains(&id));
                            assert_eq!(inside, *z == zone, "{grid:?} {level}/{x}/{y} in {z:?}");
                        }
                    }
                }
            }
            // And the other way round: an id a zone's range holds is that
            // zone's tile, wherever the grid can hold it.
            for z in &zones {
                for r in l.zone_id_ranges(*z) {
                    for id in r.clone().take(4096) {
                        if let Some((level, x, y)) = grid.from_id(id) {
                            if level <= deepest {
                                assert_eq!(
                                    l.zone_of(level, x, y).expect("zone"),
                                    *z,
                                    "{grid:?} id {id}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_zone_s_ranges_reach_the_deepest_level() {
        let l = layer(Grid::WebMercator, 10);
        let ranges = l.zone_id_ranges(Zone::Cell { x: 511, y: 377 });
        assert_eq!(ranges.len(), usize::from(pmtiles::MAX_ZOOM - 10 + 1));
        let deepest = Grid::WebMercator
            .to_id(31, 511 << 21, 377 << 21)
            .expect("id");
        assert!(ranges.last().expect("last").contains(&deepest));
        let top = l.zone_id_ranges(Zone::Top);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0], 0..=level_base(10) - 1);
    }
}
