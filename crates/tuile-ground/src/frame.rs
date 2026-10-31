// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Frames: where the ground's coordinates live, declared by its source.
//!
//! A position is `(u, v, h)`: two horizontal coordinates that name a
//! **column**, and a height in metres along that column. What `u` and `v`
//! mean is the frame's business:
//!
//! - [`PlanarFrame`]: an abstract flat world, `u` east and `v` north in
//!   metres, optionally anchored on the globe for a renderer;
//! - [`GeodeticFrame`]: a region of the globe, `u` longitude and `v`
//!   latitude in radians, `h` metres above the ellipsoid.
//!
//! The contract assumes neither: queries and edits are expressed in the
//! source's frame, and the few things that need distances in metres (brush
//! radii, normals) ask the frame for its [`Frame::metres_per_unit`] at a
//! column. Conversions to geodetic coordinates exist where the frame has a
//! place on the globe; an abstract world has none.
//!
//! The tile grid is the quadtree of the frame's bounds: level 0 is the whole
//! rectangle, level `l` splits it into `2^l × 2^l` tiles. Edits are versioned
//! per tile at one level ([`Frame::edit_level`]); data derived from the
//! ground uses tiles of the same grid.

use glam::{DVec2, DVec3};

/// A column: the frame's horizontal coordinates.
pub type Column = DVec2;

/// A position: the column and the height in metres.
pub type Position = DVec3;

/// Longitude and latitude in radians, height in metres above the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geodetic {
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
}

/// The coordinates a ground's source declares.
pub trait Frame: Send + Sync + std::fmt::Debug {
    /// The rectangle of columns the ground covers, min and max corners;
    /// the max edges are excluded.
    fn bounds(&self) -> (Column, Column);

    /// The level of the grid whose tiles carry versions and change events.
    fn edit_level(&self) -> u8;

    /// Metres per unit of `u` and of `v` at a column. Authoritative paths
    /// call it: it must be deterministic (see the crate's discipline).
    fn metres_per_unit(&self, column: Column) -> DVec2;

    /// The geodetic position of a frame position, where the frame has a
    /// place on the globe. For rendering and interchange, not for decisions.
    fn to_geodetic(&self, p: Position) -> Option<Geodetic>;

    /// The frame position of a geodetic one, where the frame has a place on
    /// the globe.
    fn geodetic_to_position(&self, g: Geodetic) -> Option<Position>;

    /// Whether a column lies inside the bounds.
    fn contains(&self, c: Column) -> bool {
        let (lo, hi) = self.bounds();
        c.x >= lo.x && c.y >= lo.y && c.x < hi.x && c.y < hi.y
    }

    /// The tile of `level` holding a column (clamped to the bounds).
    fn tile_of(&self, c: Column, level: u8) -> TileKey {
        let (lo, hi) = self.bounds();
        let n = 1u32 << level.min(30);
        let index = |v: f64, a: f64, b: f64| -> u32 {
            let t = ((v - a) / (b - a) * f64::from(n)).floor();
            if t.is_nan() || t < 0.0 {
                0
            } else if t >= f64::from(n - 1) {
                n - 1
            } else {
                // In 0..n-1 and integral after the checks above.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let i = t as u32;
                i
            }
        };
        TileKey {
            level,
            x: index(c.x, lo.x, hi.x),
            y: index(c.y, lo.y, hi.y),
        }
    }

    /// A tile's min and max corners.
    fn tile_bounds(&self, key: TileKey) -> (Column, Column) {
        let (lo, hi) = self.bounds();
        let n = f64::from(1u32 << key.level.min(30));
        let step = (hi - lo) / n;
        let a = lo + step * DVec2::new(f64::from(key.x), f64::from(key.y));
        (a, a + step)
    }
}

/// The tiles of `level` overlapping the rectangle `[lo, hi]`, west to east
/// then south to north.
pub fn tiles_over<F: Frame + ?Sized>(
    frame: &F,
    lo: Column,
    hi: Column,
    level: u8,
) -> impl Iterator<Item = TileKey> + use<F> {
    let a = frame.tile_of(lo, level);
    let b = frame.tile_of(hi, level);
    (a.y..=b.y).flat_map(move |y| (a.x..=b.x).map(move |x| TileKey { level, x, y }))
}

/// A tile of the frame's quadtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileKey {
    pub level: u8,
    /// Index along `u`, from the min edge, `0..2^level`.
    pub x: u32,
    /// Index along `v`, from the min edge, `0..2^level`.
    pub y: u32,
}

/// A tile's edit generation: 0 until an edit touches it, then +1 per edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64);

/// A flat world, `u` east and `v` north in metres, around the origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanarFrame {
    /// Half the side of the square world, metres.
    pub half_extent: f64,
    pub edit_level: u8,
    /// Where the origin sits on the globe, for a renderer that wants to
    /// place the world there; `None` for an abstract world.
    pub anchor: Option<Geodetic>,
}

/// The WGS84 semi-major axis, metres: the radius planar anchors and the
/// geodetic metric use (a sphere, not the ellipsoid; see
/// [`GeodeticFrame::metres_per_unit`]).
pub const EARTH_RADIUS: f64 = 6_378_137.0;

impl Frame for PlanarFrame {
    fn bounds(&self) -> (Column, Column) {
        (
            DVec2::splat(-self.half_extent),
            DVec2::splat(self.half_extent),
        )
    }

    fn edit_level(&self) -> u8 {
        self.edit_level
    }

    fn metres_per_unit(&self, _column: Column) -> DVec2 {
        DVec2::ONE
    }

    /// An equirectangular placement around the anchor: right near it, off by
    /// `d²/2R` at distance `d` (≈ 320 m at 64 km). A planar world is drawn
    /// flat; this only says where.
    fn to_geodetic(&self, p: Position) -> Option<Geodetic> {
        let a = self.anchor?;
        let cos = libm::cos(a.lat).max(1e-9);
        Some(Geodetic {
            lon: a.lon + p.x / (EARTH_RADIUS * cos),
            lat: a.lat + p.y / EARTH_RADIUS,
            height: a.height + p.z,
        })
    }

    fn geodetic_to_position(&self, g: Geodetic) -> Option<Position> {
        let a = self.anchor?;
        let cos = libm::cos(a.lat);
        Some(DVec3::new(
            (g.lon - a.lon) * EARTH_RADIUS * cos,
            (g.lat - a.lat) * EARTH_RADIUS,
            g.height - a.height,
        ))
    }
}

/// A region of the globe: `u` longitude and `v` latitude in radians,
/// heights above the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeodeticFrame {
    /// West-south corner (radians).
    pub min: Column,
    /// East-north corner (radians), excluded.
    pub max: Column,
    pub edit_level: u8,
}

impl Frame for GeodeticFrame {
    fn bounds(&self) -> (Column, Column) {
        (self.min, self.max)
    }

    fn edit_level(&self) -> u8 {
        self.edit_level
    }

    /// A spherical metric (radius [`EARTH_RADIUS`]): within 0.7 % of the
    /// ellipsoid's, and computed with `libm`'s cosine, the same bits on every
    /// target.
    fn metres_per_unit(&self, column: Column) -> DVec2 {
        DVec2::new(EARTH_RADIUS * libm::cos(column.y), EARTH_RADIUS)
    }

    fn to_geodetic(&self, p: Position) -> Option<Geodetic> {
        Some(Geodetic {
            lon: p.x,
            lat: p.y,
            height: p.z,
        })
    }

    fn geodetic_to_position(&self, g: Geodetic) -> Option<Position> {
        Some(DVec3::new(g.lon, g.lat, g.height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planar() -> PlanarFrame {
        PlanarFrame {
            half_extent: 65_536.0,
            edit_level: 8,
            anchor: None,
        }
    }

    #[test]
    fn tiles_cover_the_bounds_without_gaps() {
        let f = planar();
        assert_eq!(
            f.tile_of(DVec2::splat(-65_536.0), 8),
            TileKey {
                level: 8,
                x: 0,
                y: 0
            }
        );
        assert_eq!(
            f.tile_of(DVec2::splat(65_535.999), 8),
            TileKey {
                level: 8,
                x: 255,
                y: 255
            }
        );
        let k = TileKey {
            level: 8,
            x: 17,
            y: 200,
        };
        let (lo, hi) = f.tile_bounds(k);
        assert_eq!(hi - lo, DVec2::splat(512.0));
        assert_eq!(f.tile_of(lo, 8), k);
        assert_eq!(f.tile_of(hi - DVec2::splat(1e-6), 8), k);
        assert_ne!(f.tile_of(hi, 8), k);
    }

    #[test]
    fn rectangles_list_their_tiles_in_order() {
        let f = planar();
        let tiles: Vec<TileKey> =
            tiles_over(&f, DVec2::splat(-10.0), DVec2::splat(10.0), 8).collect();
        assert_eq!(tiles.len(), 4, "{tiles:?}");
        assert!(tiles
            .windows(2)
            .all(|w| (w[0].y, w[0].x) < (w[1].y, w[1].x)));
    }

    #[test]
    fn an_abstract_world_has_no_place_on_the_globe() {
        let f = planar();
        assert_eq!(f.to_geodetic(DVec3::ZERO), None);
        let anchored = PlanarFrame {
            anchor: Some(Geodetic {
                lon: 0.1,
                lat: 0.8,
                height: 100.0,
            }),
            ..f
        };
        let g = anchored
            .to_geodetic(DVec3::new(1000.0, -500.0, 3.0))
            .expect("anchored");
        let back = anchored.geodetic_to_position(g).expect("anchored");
        assert!(
            (back - DVec3::new(1000.0, -500.0, 3.0)).length() < 1e-6,
            "{back:?}"
        );
    }

    #[test]
    fn a_geodetic_frame_shrinks_longitude_toward_the_poles() {
        let f = GeodeticFrame {
            min: DVec2::new(0.0, 0.0),
            max: DVec2::new(0.1, 1.2),
            edit_level: 6,
        };
        let equator = f.metres_per_unit(DVec2::new(0.05, 0.0));
        let north = f.metres_per_unit(DVec2::new(0.05, 1.0));
        assert_eq!(equator.x, EARTH_RADIUS);
        assert!(north.x < equator.x * 0.6 && north.y == EARTH_RADIUS);
    }
}
