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
//! rectangle, level `l` splits it into `2^l × 2^l` tiles, up to
//! [`MAX_LEVEL`]. Tile `x` along `u` covers `[edge(x), edge(x + 1))`, with
//! `edge(x) = min + (max − min) × (x / 2^l)` computed in `f64` exactly that
//! way (and the last edge is `max` itself), so [`Frame::tile_of`] and
//! [`Frame::tile_bounds`] agree to the bit on any bounds whose edges at that
//! level are strictly increasing — tiles wider than the rounding of the
//! bounds, which a ground checks at its edit level when it is built:
//! `tile_of(tile_bounds(k).0) == k`, and a tile's max corner belongs to the
//! next tile. Edits are versioned per tile at one level
//! ([`Frame::edit_level`]); data derived from the ground uses tiles of the
//! same grid.

use glam::{DVec2, DVec3};
use thiserror::Error;

/// A column: the frame's horizontal coordinates.
pub type Column = DVec2;

/// A position: the column and the height in metres.
pub type Position = DVec3;

/// The deepest level of the tile grid: `2^30` tiles per side, indices that
/// fit a `u32` with room to name the next edge.
pub const MAX_LEVEL: u8 = 30;

/// Longitude and latitude in radians, height in metres above the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geodetic {
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
}

/// Why a frame cannot hold a ground.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidFrame {
    #[error("the bounds are not finite, or min is not below max on both axes")]
    Bounds,
    #[error("edit level {0} is above the deepest level, {MAX_LEVEL}")]
    Level(u8),
    /// Latitudes beyond ±90°, or longitudes beyond ±2π or spanning more.
    #[error("a geodetic region lies within ±90° of latitude, ±2π and 2π of longitude")]
    Geodetic,
    #[error("the anchor is not finite")]
    Anchor,
}

/// The coordinates a ground's source declares.
pub trait Frame: Send + Sync + std::fmt::Debug {
    /// The rectangle of columns the ground covers, min and max corners;
    /// the max edges are excluded.
    fn bounds(&self) -> (Column, Column);

    /// The level of the grid whose tiles carry versions and change events,
    /// at most [`MAX_LEVEL`].
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

    /// The horizontal offset of column `p` from column `at`, in metres, in
    /// the local east-north tangent plane at `at`; `None` where `p` is too
    /// far to be seen from that plane (beyond the horizon of a globe).
    /// Brushes are measured with it, column by column. The default scales
    /// the difference by the metric at `at`, exact for a constant metric;
    /// a frame whose metric varies overrides it.
    fn tangent_offset(&self, at: Column, p: Column) -> Option<DVec2> {
        Some((p - at) * self.metres_per_unit(at))
    }

    /// The rectangle of columns within `r` metres (by
    /// [`Self::tangent_offset`]) of some column of `[lo, hi]`. It may extend
    /// beyond the bounds, and spans a whole period of `u` when it reaches
    /// every `u` (a cap around a pole). The default is exact for a constant
    /// metric; a frame whose metric varies overrides it.
    fn reach(&self, lo: Column, hi: Column, r: f64) -> (Column, Column) {
        let m = self.metres_per_unit((lo + hi) / 2.0);
        let d = DVec2::new(r / m.x, r / m.y);
        (lo - d, hi + d)
    }

    /// The period of `u` when the frame wraps around it (a geodetic frame
    /// spanning the whole globe: 2π); `None` for a frame with edges.
    fn u_period(&self) -> Option<f64> {
        None
    }

    /// Whether a column names a place in the frame, inside its bounds or
    /// not (a latitude beyond ±90° names none). Brushes are checked with it.
    fn is_place(&self, c: Column) -> bool {
        c.is_finite()
    }

    /// Whether the frame can hold a ground: finite bounds with min below
    /// max, and an edit level at most [`MAX_LEVEL`]. A ground checks its
    /// frame when it is built.
    fn validate(&self) -> Result<(), InvalidFrame> {
        let (lo, hi) = self.bounds();
        if !(lo.is_finite() && hi.is_finite() && lo.x < hi.x && lo.y < hi.y) {
            return Err(InvalidFrame::Bounds);
        }
        if self.edit_level() > MAX_LEVEL {
            return Err(InvalidFrame::Level(self.edit_level()));
        }
        Ok(())
    }

    /// Whether a column lies inside the bounds.
    fn contains(&self, c: Column) -> bool {
        let (lo, hi) = self.bounds();
        c.x >= lo.x && c.y >= lo.y && c.x < hi.x && c.y < hi.y
    }

    /// The tile of `level` holding a column; a column outside the bounds
    /// gets the nearest tile. `None` for a level above [`MAX_LEVEL`] or a
    /// NaN column.
    fn tile_of(&self, c: Column, level: u8) -> Option<TileKey> {
        if level > MAX_LEVEL || c.is_nan() {
            return None;
        }
        let (lo, hi) = self.bounds();
        let n = 1u64 << level;
        let index = |v: f64, a: f64, b: f64| {
            // Below 2^30 by construction.
            u32::try_from(grid_index(v, a, b, n)).unwrap_or(u32::MAX)
        };
        Some(TileKey {
            level,
            x: index(c.x, lo.x, hi.x),
            y: index(c.y, lo.y, hi.y),
        })
    }

    /// A tile's min corner (included) and max corner (excluded: it belongs
    /// to the next tile). `None` for a key outside the grid.
    fn tile_bounds(&self, key: TileKey) -> Option<(Column, Column)> {
        if key.level > MAX_LEVEL {
            return None;
        }
        let n = 1u64 << key.level;
        let (x, y) = (u64::from(key.x), u64::from(key.y));
        if x >= n || y >= n {
            return None;
        }
        let (lo, hi) = self.bounds();
        let corner =
            |x: u64, y: u64| DVec2::new(grid_edge(lo.x, hi.x, x, n), grid_edge(lo.y, hi.y, y, n));
        Some((corner(x, y), corner(x + 1, y + 1)))
    }
}

/// Edge `i` of a grid of `count` equal divisions of `[lo, hi]`, computed one
/// way everywhere so that [`grid_index`] agrees with it to the bit.
pub(crate) fn grid_edge(lo: f64, hi: f64, i: u64, count: u64) -> f64 {
    if i >= count {
        // `lo + (hi − lo)` may round away from `hi`.
        return hi;
    }
    lo + (hi - lo) * (to_f64(i) / to_f64(count))
}

/// The division `i` of a grid of `count` (at least 1) with
/// `grid_edge(i) <= v < grid_edge(i + 1)`, clamped to `0..count`.
pub(crate) fn grid_index(v: f64, lo: f64, hi: f64, count: u64) -> u64 {
    let last = count - 1;
    let guess = ((v - lo) / (hi - lo) * to_f64(count)).floor();
    let mut i = if guess.is_nan() || guess <= 0.0 {
        0
    } else if guess >= to_f64(last) {
        last
    } else {
        // In 0..last and integral after the checks above.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let i = guess as u64;
        i
    };
    // The guess is a division away at most; the edges decide.
    while i > 0 && v < grid_edge(lo, hi, i, count) {
        i -= 1;
    }
    while i < last && v >= grid_edge(lo, hi, i + 1, count) {
        i += 1;
    }
    i
}

pub(crate) fn to_f64(n: u64) -> f64 {
    // Grid sizes stay far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    f
}

/// The tiles of `level` overlapping the rectangle `[lo, hi]` (both corners
/// included), west to east then south to north.
///
/// Lazy: it yields `(Δx + 1) × (Δy + 1)` keys, up to `4^level` for a
/// rectangle over the whole frame, and costs nothing until iterated. Empty
/// for a level above [`MAX_LEVEL`] or a NaN corner.
pub fn tiles_over<F: Frame + ?Sized>(
    frame: &F,
    lo: Column,
    hi: Column,
    level: u8,
) -> impl Iterator<Item = TileKey> + use<F> {
    frame
        .tile_of(lo, level)
        .zip(frame.tile_of(hi, level))
        .into_iter()
        .flat_map(move |(a, b)| {
            (a.y..=b.y).flat_map(move |y| (a.x..=b.x).map(move |x| TileKey { level, x, y }))
        })
}

/// A tile of the frame's quadtree.
///
/// Indices count from the frame's min corner. This grid is the frame's own:
/// it does not share edges with the globe's tiling schemes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileKey {
    pub level: u8,
    /// Index along `u`, from the min edge, `0..2^level`.
    pub x: u32,
    /// Index along `v`, from the min edge, `0..2^level`.
    pub y: u32,
}

/// A tile's edit generation: 0 until an edit changes it, then +1 per edit.
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

/// The cosine of an anchor's latitude, kept away from zero at the poles; the
/// same value both ways so the two conversions stay inverse.
fn anchor_cos(lat: f64) -> f64 {
    libm::cos(lat).max(1e-9)
}

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
        Some(Geodetic {
            lon: a.lon + p.x / (EARTH_RADIUS * anchor_cos(a.lat)),
            lat: a.lat + p.y / EARTH_RADIUS,
            height: a.height + p.z,
        })
    }

    fn geodetic_to_position(&self, g: Geodetic) -> Option<Position> {
        let a = self.anchor?;
        Some(DVec3::new(
            (g.lon - a.lon) * EARTH_RADIUS * anchor_cos(a.lat),
            (g.lat - a.lat) * EARTH_RADIUS,
            g.height - a.height,
        ))
    }

    fn validate(&self) -> Result<(), InvalidFrame> {
        let (lo, hi) = self.bounds();
        if !(lo.is_finite() && hi.is_finite() && lo.x < hi.x && lo.y < hi.y) {
            return Err(InvalidFrame::Bounds);
        }
        if self.edit_level > MAX_LEVEL {
            return Err(InvalidFrame::Level(self.edit_level));
        }
        if self
            .anchor
            .is_some_and(|a| !(a.lon.is_finite() && a.lat.is_finite() && a.height.is_finite()))
        {
            return Err(InvalidFrame::Anchor);
        }
        Ok(())
    }
}

/// A region of the globe: `u` longitude and `v` latitude in radians,
/// heights above the ellipsoid.
///
/// Its longitudes lie within ±2π with `min.x < max.x` and span at most 2π,
/// and its latitudes within ±90° (poles included). A region spanning exactly
/// 2π of longitude is the whole globe: brushes wrap around it
/// ([`Frame::u_period`]); a narrower region has edges, and a brush is
/// clipped to them. Brushes are measured in each column's tangent plane, so
/// a ball at a pole carves a disc, not a longitude-latitude rectangle;
/// segments (rays, a capsule's axis) are straight in longitude and latitude
/// and do not wrap.
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

    /// The orthographic projection of `p` on the tangent plane at `at`, on
    /// the sphere of radius [`EARTH_RADIUS`]: a distance `R·sin θ` for an
    /// arc `R·θ`, short by `θ²/6` of it (4·10⁻⁵ at 100 km). Longitudes
    /// wrap by construction. `None` beyond 90° of arc.
    fn tangent_offset(&self, at: Column, p: Column) -> Option<DVec2> {
        let (sa, ca) = (libm::sin(at.y), libm::cos(at.y));
        let (sp, cp) = (libm::sin(p.y), libm::cos(p.y));
        let dl = p.x - at.x;
        let (sdl, cdl) = (libm::sin(dl), libm::cos(dl));
        let east = cp * sdl;
        let north = sp * ca - cp * sa * cdl;
        let up = sp * sa + cp * ca * cdl;
        (up > 0.0).then(|| DVec2::new(east, north) * EARTH_RADIUS)
    }

    /// A spherical cap of angular radius `asin(r/R)` around the rectangle:
    /// latitudes widened by it, longitudes by `asin(sin θ / cos φ)` with `φ`
    /// the rectangle's latitude closest to a pole, and every longitude when
    /// the cap contains a pole.
    fn reach(&self, lo: Column, hi: Column, r: f64) -> (Column, Column) {
        use core::f64::consts::{FRAC_PI_2, PI};
        let s = r / EARTH_RADIUS;
        let every =
            |south: f64, north: f64| (DVec2::new(lo.x - PI, south), DVec2::new(hi.x + PI, north));
        if s >= 1.0 {
            return every(-FRAC_PI_2, FRAC_PI_2);
        }
        let dlat = libm::asin(s);
        let (south, north) = (lo.y - dlat, hi.y + dlat);
        let c = libm::cos(lo.y.abs().max(hi.y.abs()));
        if south <= -FRAC_PI_2 || north >= FRAC_PI_2 || s >= c {
            return every(south, north);
        }
        let dlon = libm::asin(s / c);
        (
            DVec2::new(lo.x - dlon, south),
            DVec2::new(hi.x + dlon, north),
        )
    }

    fn u_period(&self) -> Option<f64> {
        let tau = core::f64::consts::TAU;
        (self.max.x - self.min.x == tau).then_some(tau)
    }

    fn is_place(&self, c: Column) -> bool {
        c.is_finite() && c.y.abs() <= core::f64::consts::FRAC_PI_2
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

    fn validate(&self) -> Result<(), InvalidFrame> {
        let (lo, hi) = (self.min, self.max);
        if !(lo.is_finite() && hi.is_finite() && lo.x < hi.x && lo.y < hi.y) {
            return Err(InvalidFrame::Bounds);
        }
        if self.edit_level > MAX_LEVEL {
            return Err(InvalidFrame::Level(self.edit_level));
        }
        let (lat, tau) = (core::f64::consts::FRAC_PI_2, core::f64::consts::TAU);
        if lo.y < -lat || hi.y > lat || lo.x < -tau || hi.x > tau || hi.x - lo.x > tau {
            return Err(InvalidFrame::Geodetic);
        }
        Ok(())
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
        let key = |x, y| TileKey { level: 8, x, y };
        assert_eq!(f.tile_of(DVec2::splat(-65_536.0), 8), Some(key(0, 0)));
        assert_eq!(f.tile_of(DVec2::splat(65_535.999), 8), Some(key(255, 255)));
        let k = key(17, 200);
        let (lo, hi) = f.tile_bounds(k).unwrap();
        assert_eq!(hi - lo, DVec2::splat(512.0));
        assert_eq!(f.tile_of(lo, 8), Some(k));
        assert_eq!(f.tile_of(hi - DVec2::splat(1e-6), 8), Some(k));
        assert_ne!(f.tile_of(hi, 8), Some(k));
    }

    /// Bounds in radians are not powers of two: the tile a corner falls in
    /// must still be the tile whose corner it is, at every level.
    #[test]
    fn tile_corners_round_trip_on_arbitrary_bounds() {
        let lat = 45f64.to_radians();
        let f = GeodeticFrame {
            min: DVec2::new(0.1, lat),
            max: DVec2::new(0.1 + 0.000_909_4, lat + 0.000_642_2),
            edit_level: 4,
        };
        for level in 0..=12u8 {
            let n = 1u32 << level;
            for x in (0..n).step_by(usize::try_from((n / 64).max(1)).unwrap()) {
                for y in [0, n / 3, n - 1] {
                    let k = TileKey { level, x, y };
                    let (lo, hi) = f.tile_bounds(k).unwrap();
                    assert_eq!(f.tile_of(lo, level), Some(k), "{k:?}");
                    if x + 1 < n && y + 1 < n {
                        assert_eq!(
                            f.tile_of(hi, level),
                            Some(TileKey {
                                level,
                                x: x + 1,
                                y: y + 1
                            }),
                            "the max corner belongs to the next tile: {k:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn levels_beyond_the_deepest_are_refused() {
        let f = planar();
        assert_eq!(f.tile_of(DVec2::ZERO, MAX_LEVEL + 1), None);
        assert!(f.tile_of(DVec2::ZERO, MAX_LEVEL).is_some());
        assert_eq!(
            f.tile_bounds(TileKey {
                level: 31,
                x: 0,
                y: 0
            }),
            None
        );
        assert_eq!(
            f.tile_bounds(TileKey {
                level: 2,
                x: 4,
                y: 0
            }),
            None,
            "outside the grid"
        );
        assert_eq!(
            tiles_over(&f, DVec2::ZERO, DVec2::ONE, MAX_LEVEL + 1).count(),
            0
        );
        assert_eq!(
            PlanarFrame {
                edit_level: 31,
                ..f
            }
            .validate(),
            Err(InvalidFrame::Level(31))
        );
    }

    #[test]
    fn frames_are_validated() {
        assert_eq!(planar().validate(), Ok(()));
        let bad = PlanarFrame {
            half_extent: -1.0,
            ..planar()
        };
        assert_eq!(bad.validate(), Err(InvalidFrame::Bounds));
        let wrap = GeodeticFrame {
            min: DVec2::new(3.0, 0.0),
            max: DVec2::new(-3.0, 0.1),
            edit_level: 2,
        };
        assert_eq!(wrap.validate(), Err(InvalidFrame::Bounds));
        let polar = GeodeticFrame {
            min: DVec2::new(0.0, 1.5),
            max: DVec2::new(0.1, 1.6),
            edit_level: 2,
        };
        assert_eq!(polar.validate(), Err(InvalidFrame::Geodetic));
        let far = GeodeticFrame {
            min: DVec2::new(100.0, 0.0),
            max: DVec2::new(100.1, 0.1),
            edit_level: 2,
        };
        assert_eq!(
            far.validate(),
            Err(InvalidFrame::Geodetic),
            "longitudes within ±2π"
        );
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
