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
        Ok(Zone::Cell { x: x >> shift, y: y >> shift })
    }

    /// The key prefix of a zone: `bing-aerial/zones/z10/522/373`.
    pub fn zone_prefix(&self, zone: Zone) -> String {
        match zone {
            Zone::Top => format!("{}/top", self.name),
            Zone::Cell { x, y } => format!("{}/zones/z{}/{x}/{y}", self.name, self.zone_level),
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

    /// Is every tile of `epoch` past its expiry at `now`? Measured from the
    /// end of the epoch, so no tile is dropped before its full lifetime.
    pub fn is_expired(&self, epoch: &str, now: SystemTime) -> bool {
        let Some(expiry) = self.expiry else { return false };
        let Some(end) = epoch_end(epoch) else { return false };
        now.duration_since(end).map(|age| age > expiry).unwrap_or(false)
    }
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
    let (ny, nm) = if month == DECEMBER { (year + 1, 1) } else { (year, month + 1) };
    let next = NaiveDate::from_ymd_opt(ny, nm, 1)?.and_hms_opt(0, 0, 0)?.and_utc();
    Some(next.into())
}
