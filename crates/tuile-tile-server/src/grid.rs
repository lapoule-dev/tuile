// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How a source's own tile address lands on an archive's `z/x/y`.
//!
//! An archive addresses tiles on one square quadtree: one tile at z0, four at
//! z1. Imagery in Web Mercator is exactly that. Terrain is not: its geographic
//! quadtree has **two** tiles at level 0 (west and east hemispheres), so level
//! `L` is `2^(L+1) × 2^L` tiles. It fits the square one level down — level `L`
//! is stored at `z = L + 1`, in the half of the square with `y < 2^L`. Nothing
//! is lost and nothing is reprojected; the archive only carries the bytes, and
//! its metadata says which grid they are on.

use pmtiles::{TileCoord, MAX_ZOOM};
use serde::{Deserialize, Serialize};

/// The tiling a layer's addresses are expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Grid {
    /// One tile at level 0; the archive's own quadtree.
    WebMercator,
    /// Two tiles at level 0 (2 × 1); stored one level down.
    Geographic,
}

/// A tile address that the grid cannot hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{grid:?} has no tile {level}/{x}/{y}")]
pub struct OutOfGrid {
    pub grid: Grid,
    pub level: u8,
    pub x: u32,
    pub y: u32,
}

impl Grid {
    /// The deepest level this grid can store.
    pub fn max_level(self) -> u8 {
        match self {
            Grid::WebMercator => MAX_ZOOM,
            Grid::Geographic => MAX_ZOOM - 1,
        }
    }

    /// Tiles across and down at `level`.
    pub fn size(self, level: u8) -> (u64, u64) {
        let n = 1u64 << level;
        match self {
            Grid::WebMercator => (n, n),
            Grid::Geographic => (2 * n, n),
        }
    }

    /// Checks an address against the grid.
    pub fn check(self, level: u8, x: u32, y: u32) -> Result<(), OutOfGrid> {
        let out = OutOfGrid { grid: self, level, x, y };
        if level > self.max_level() {
            return Err(out);
        }
        let (w, h) = self.size(level);
        if u64::from(x) >= w || u64::from(y) >= h {
            return Err(out);
        }
        Ok(())
    }

    /// The archive coordinate a source address is stored at.
    pub fn to_archive(self, level: u8, x: u32, y: u32) -> Result<TileCoord, OutOfGrid> {
        self.check(level, x, y)?;
        let z = match self {
            Grid::WebMercator => level,
            Grid::Geographic => level + 1,
        };
        TileCoord::new(z, x, y).map_err(|_| OutOfGrid { grid: self, level, x, y })
    }

    /// The archive tile id a source address is stored at.
    pub fn to_id(self, level: u8, x: u32, y: u32) -> Result<u64, OutOfGrid> {
        let c = self.to_archive(level, x, y)?;
        Ok(pmtiles::TileId::from(c).value())
    }

    /// The source address an archive coordinate holds, if it belongs to this
    /// grid (for a geographic layer, z0 and the lower half hold nothing).
    pub fn from_archive(self, c: TileCoord) -> Option<(u8, u32, u32)> {
        let (level, x, y) = match self {
            Grid::WebMercator => (c.z(), c.x(), c.y()),
            Grid::Geographic => (c.z().checked_sub(1)?, c.x(), c.y()),
        };
        self.check(level, x, y).ok()?;
        Some((level, x, y))
    }
}
