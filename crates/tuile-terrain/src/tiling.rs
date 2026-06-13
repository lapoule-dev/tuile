// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Geographic quadtree tiling (EPSG:4326), the scheme Cesium World Terrain
//! uses. Modelled on `GeographicTilingScheme` (cesium-native / CesiumJS).
//!
//! Two root tiles at level 0 (the eastern and western hemispheres), the
//! whole globe. Tiles double in each axis per level. Coordinates here use
//! the **TMS convention** (y = 0 at the south), which is how terrain tile
//! URLs are addressed (`{z}/{x}/{y}.terrain`).

use std::f64::consts::{FRAC_PI_2, PI};

/// A tile address in the geographic quadtree, TMS convention (y up = north).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    pub level: u32,
    pub x: u64,
    /// TMS row: 0 is the southern-most tile.
    pub y: u64,
}

impl TileCoord {
    pub fn new(level: u32, x: u64, y: u64) -> Self {
        Self { level, x, y }
    }

    /// The four children one level down (TMS order).
    pub fn children(self) -> [TileCoord; 4] {
        let (l, x, y) = (self.level + 1, self.x * 2, self.y * 2);
        [
            TileCoord::new(l, x, y),
            TileCoord::new(l, x + 1, y),
            TileCoord::new(l, x, y + 1),
            TileCoord::new(l, x + 1, y + 1),
        ]
    }
}

/// A geographic rectangle in radians (WGS84 longitudes/latitudes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoRect {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl GeoRect {
    pub fn width(&self) -> f64 {
        self.east - self.west
    }
    pub fn height(&self) -> f64 {
        self.north - self.south
    }
    pub fn center(&self) -> (f64, f64) {
        (
            (self.west + self.east) / 2.0,
            (self.south + self.north) / 2.0,
        )
    }
}

/// The geographic tiling scheme of Cesium World Terrain.
#[derive(Debug, Clone, Copy)]
pub struct GeographicTilingScheme {
    pub root_tiles_x: u64,
    pub root_tiles_y: u64,
}

impl Default for GeographicTilingScheme {
    fn default() -> Self {
        Self {
            root_tiles_x: 2,
            root_tiles_y: 1,
        }
    }
}

impl GeographicTilingScheme {
    pub fn tiles_x(&self, level: u32) -> u64 {
        self.root_tiles_x << level
    }
    pub fn tiles_y(&self, level: u32) -> u64 {
        self.root_tiles_y << level
    }

    /// Geographic rectangle covered by a tile (radians, TMS convention).
    pub fn tile_rect(&self, c: TileCoord) -> GeoRect {
        let nx = self.tiles_x(c.level) as f64;
        let ny = self.tiles_y(c.level) as f64;
        let tile_w = (2.0 * PI) / nx;
        let tile_h = PI / ny;
        let west = -PI + c.x as f64 * tile_w;
        // TMS: y = 0 is the south edge.
        let south = -FRAC_PI_2 + c.y as f64 * tile_h;
        GeoRect {
            west,
            south,
            east: west + tile_w,
            north: south + tile_h,
        }
    }
}

/// Maximum geometric error (meters) of a level-0 quadtree tile for a 65×65
/// heightmap whose vertical error is ~25% of the horizontal sample spacing
/// at the equator. Mirrors cesium's `calcQuadtreeMaxGeometricError`.
///
/// `ellipsoid_max_radius` is the semi-major axis (WGS84_A).
pub fn level_zero_maximum_geometric_error(ellipsoid_max_radius: f64, root_tiles_x: u64) -> f64 {
    ellipsoid_max_radius * 2.0 * PI * 0.25 / (65.0 * root_tiles_x as f64)
}

/// Geometric error (meters) at a given level: halves each level down.
pub fn level_geometric_error(level: u32, ellipsoid_max_radius: f64, root_tiles_x: u64) -> f64 {
    level_zero_maximum_geometric_error(ellipsoid_max_radius, root_tiles_x) / (1u64 << level) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::WGS84_A;

    #[test]
    fn root_covers_two_hemispheres() {
        let s = GeographicTilingScheme::default();
        // West hemisphere tile.
        let w = s.tile_rect(TileCoord::new(0, 0, 0));
        assert!((w.west + PI).abs() < 1e-12);
        assert!(w.east.abs() < 1e-12); // 0 longitude
        assert!((w.south + FRAC_PI_2).abs() < 1e-12);
        assert!((w.north - FRAC_PI_2).abs() < 1e-12);
        // East hemisphere tile.
        let e = s.tile_rect(TileCoord::new(0, 1, 0));
        assert!(e.west.abs() < 1e-12);
        assert!((e.east - PI).abs() < 1e-12);
    }

    #[test]
    fn children_partition_parent() {
        let s = GeographicTilingScheme::default();
        let parent = s.tile_rect(TileCoord::new(0, 0, 0));
        let kids = TileCoord::new(0, 0, 0).children();
        // Union of the four children equals the parent rectangle.
        let (mut w, mut e, mut n, mut so) = (f64::MAX, f64::MIN, f64::MIN, f64::MAX);
        for k in kids {
            let r = s.tile_rect(k);
            w = w.min(r.west);
            e = e.max(r.east);
            n = n.max(r.north);
            so = so.min(r.south);
            // Each child is a quarter of the parent area.
            assert!((r.width() - parent.width() / 2.0).abs() < 1e-12);
            assert!((r.height() - parent.height() / 2.0).abs() < 1e-12);
        }
        assert!((w - parent.west).abs() < 1e-12 && (e - parent.east).abs() < 1e-12);
        assert!((n - parent.north).abs() < 1e-12 && (so - parent.south).abs() < 1e-12);
    }

    #[test]
    fn geometric_error_halves_each_level() {
        let s = GeographicTilingScheme::default();
        let e0 = level_geometric_error(0, WGS84_A, s.root_tiles_x);
        let e1 = level_geometric_error(1, WGS84_A, s.root_tiles_x);
        assert!((e0 / e1 - 2.0).abs() < 1e-9);
        // Sanity: level 0 error is on the order of tens of km.
        assert!(e0 > 1.0e4 && e0 < 1.0e5, "e0 = {e0}");
    }
}
