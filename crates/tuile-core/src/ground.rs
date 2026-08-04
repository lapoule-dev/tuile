// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground-height seam: what the surface is, where the camera is.
//!
//! A camera must not fly through a mountain, and deciding that needs one fact
//! no camera can hold: how high the ground stands under it. Height belongs to
//! the terrain data; a controller only needs to *ask*. So the question lives
//! here, as a trait, and the answer is supplied by whoever loaded the terrain
//! (`tuile_terrain::TerrainHeights`) — the same split as
//! [`TileFetcher`](crate::fetch::TileFetcher).
//!
//! Keeping this out of the camera crate is what lets it stay pure geometry, and
//! out of the terrain crate what lets a controller work over any surface at all
//! — a bare ellipsoid, a bathymetric grid, a fixed floor for a flat scene.

use crate::geo::Geodetic;

/// Reports how high the ground stands at a position.
///
/// Implementations back onto whatever surface data they have, and answer
/// `None` where they have none — a globe streams, so most of the planet is
/// unknown at any instant. `None` means "no opinion", never "sea level": a
/// caller that read it as zero would happily fly a camera through the Alps the
/// moment a tile fell out of residency.
///
/// Answers are advisory and change as data streams in; expect the height at a
/// point to rise as finer tiles arrive.
pub trait GroundHeight: Send + Sync {
    /// Height of the ground above the ellipsoid, in metres, at `lon`/`lat`
    /// (radians). `None` where nothing is known yet.
    fn height_at(&self, lon: f64, lat: f64) -> Option<f64>;
}

/// A flat surface at a fixed height — the whole ellipsoid, or a chosen datum.
///
/// Useful before terrain streams in, and for scenes that have no terrain at
/// all. Never returns `None`: it always has an opinion, by construction.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlatGround(pub f64);

impl GroundHeight for FlatGround {
    fn height_at(&self, _lon: f64, _lat: f64) -> Option<f64> {
        Some(self.0)
    }
}

/// Raises `position` to sit at least `clearance` metres above the ground,
/// leaving it untouched when it already does.
///
/// Where the ground is unknown the position stands: refusing to move is the
/// honest response to not knowing, and a globe that jerked the camera upward
/// every time a tile was evicted would be worse than one that occasionally
/// clips. The eye rises along the geodetic normal, so longitude and latitude —
/// and therefore what is on screen — do not shift.
pub fn lift_above_ground(
    position: glam::DVec3,
    ground: &dyn GroundHeight,
    clearance: f64,
) -> glam::DVec3 {
    let g = crate::geo::ecef_to_geodetic(position);
    let Some(height) = ground.height_at(g.lon, g.lat) else {
        return position;
    };
    let floor = height + clearance;
    if g.height >= floor {
        return position;
    }
    crate::geo::geodetic_to_ecef(Geodetic {
        lon: g.lon,
        lat: g.lat,
        height: floor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{ecef_to_geodetic, geodetic_to_ecef};

    struct NoIdea;
    impl GroundHeight for NoIdea {
        fn height_at(&self, _lon: f64, _lat: f64) -> Option<f64> {
            None
        }
    }

    fn at(height: f64) -> glam::DVec3 {
        geodetic_to_ecef(Geodetic {
            lon: 0.1,
            lat: 0.8,
            height,
        })
    }

    #[test]
    fn a_position_below_the_ground_is_lifted_to_the_clearance() {
        // Ground at 3000 m, 150 m clearance: an eye at 500 m is underground.
        let lifted = lift_above_ground(at(500.0), &FlatGround(3000.0), 150.0);
        assert!((ecef_to_geodetic(lifted).height - 3150.0).abs() < 1.0);
    }

    #[test]
    fn a_position_already_clear_is_untouched() {
        let start = at(9000.0);
        let lifted = lift_above_ground(start, &FlatGround(3000.0), 150.0);
        assert_eq!(lifted, start, "no nudging when already above");
    }

    #[test]
    fn lifting_preserves_longitude_and_latitude() {
        let before = ecef_to_geodetic(at(500.0));
        let after = ecef_to_geodetic(lift_above_ground(at(500.0), &FlatGround(3000.0), 150.0));
        assert!((after.lon - before.lon).abs() < 1e-12);
        assert!((after.lat - before.lat).abs() < 1e-12);
    }

    /// Unknown ground must not read as sea level, or a camera would be flung
    /// upward — or worse, allowed downward — wherever terrain has not streamed.
    #[test]
    fn unknown_ground_leaves_the_position_alone() {
        let start = at(-2000.0);
        assert_eq!(lift_above_ground(start, &NoIdea, 150.0), start);
    }
}
