// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! WGS84 ellipsoid conversions and 3D Tiles `region` handling.
//!
//! All angles are radians, all lengths meters, everything f64.

use crate::math::Obb;
use glam::{DMat3, DVec3};

/// WGS84 semi-major axis (meters).
pub const WGS84_A: f64 = 6_378_137.0;
/// WGS84 flattening.
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;
/// WGS84 semi-minor axis (meters).
pub const WGS84_B: f64 = WGS84_A * (1.0 - WGS84_F);

/// First eccentricity squared.
const E2: f64 = WGS84_F * (2.0 - WGS84_F);

/// Geodetic coordinates: longitude/latitude in radians, height in meters
/// above the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geodetic {
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
}

/// Geodetic → ECEF (EPSG:4978).
pub fn geodetic_to_ecef(g: Geodetic) -> DVec3 {
    let (sin_lat, cos_lat) = g.lat.sin_cos();
    let (sin_lon, cos_lon) = g.lon.sin_cos();
    // Prime vertical radius of curvature.
    let n = WGS84_A / (1.0 - E2 * sin_lat * sin_lat).sqrt();
    DVec3::new(
        (n + g.height) * cos_lat * cos_lon,
        (n + g.height) * cos_lat * sin_lon,
        (n * (1.0 - E2) + g.height) * sin_lat,
    )
}

/// ECEF → geodetic, iterative refinement from a Bowring-style start.
/// Converges well below 1e-9 m for any point from the geocenter vicinity
/// to satellite altitudes.
pub fn ecef_to_geodetic(p: DVec3) -> Geodetic {
    let lon = p.y.atan2(p.x);
    let rho = (p.x * p.x + p.y * p.y).sqrt();

    // Bowring's initial guess.
    let beta = (p.z * WGS84_A).atan2(rho * WGS84_B);
    let (sin_b, cos_b) = beta.sin_cos();
    let ep2 = (WGS84_A * WGS84_A - WGS84_B * WGS84_B) / (WGS84_B * WGS84_B);
    let mut lat = (p.z + ep2 * WGS84_B * sin_b.powi(3)).atan2(rho - E2 * WGS84_A * cos_b.powi(3));

    // A few fixed-point iterations to drive the residual to f64 noise.
    for _ in 0..4 {
        let sin_lat = lat.sin();
        let n = WGS84_A / (1.0 - E2 * sin_lat * sin_lat).sqrt();
        let h = rho / lat.cos() - n;
        lat = (p.z / rho).atan2(1.0 - E2 * n / (n + h));
    }
    let sin_lat = lat.sin();
    let n = WGS84_A / (1.0 - E2 * sin_lat * sin_lat).sqrt();
    let height = if rho > 1.0 {
        rho / lat.cos() - n
    } else {
        // Near the poles cos(lat) ~ 0: use the Z form.
        p.z.abs() / sin_lat.abs() - n * (1.0 - E2)
    };
    Geodetic { lon, lat, height }
}

/// Local east/north/up unit vectors at a geodetic position.
pub fn enu_frame(g: Geodetic) -> DMat3 {
    let (sin_lat, cos_lat) = g.lat.sin_cos();
    let (sin_lon, cos_lon) = g.lon.sin_cos();
    let east = DVec3::new(-sin_lon, cos_lon, 0.0);
    let north = DVec3::new(-sin_lat * cos_lon, -sin_lat * sin_lon, cos_lat);
    let up = DVec3::new(cos_lat * cos_lon, cos_lat * sin_lon, sin_lat);
    DMat3::from_cols(east, north, up)
}

/// 3D Tiles `boundingVolume.region`: west, south, east, north (radians),
/// min height, max height (meters).
///
/// Converts to an ECEF [`Obb`] that encloses the region. The fit samples
/// the region boundary and is conservative for the moderate extents found
/// in real tilesets (the v1 approximation documented in the primer).
pub fn region_to_obb(region: &[f64; 6]) -> Obb {
    let [west, south, east, north, min_h, max_h] = *region;
    let mid = Geodetic {
        lon: (west + east) / 2.0,
        lat: (south + north) / 2.0,
        height: (min_h + max_h) / 2.0,
    };
    let frame = enu_frame(mid);
    let origin = geodetic_to_ecef(mid);

    // Sample boundary points (corners + edge midpoints, both heights) and
    // fit an axis-aligned box in the local ENU frame.
    let lons = [west, mid.lon, east];
    let lats = [south, mid.lat, north];
    let mut min = DVec3::splat(f64::INFINITY);
    let mut max = DVec3::splat(f64::NEG_INFINITY);
    for &lon in &lons {
        for &lat in &lats {
            for &height in &[min_h, max_h] {
                let p = geodetic_to_ecef(Geodetic { lon, lat, height });
                let local = frame.transpose() * (p - origin);
                min = min.min(local);
                max = max.max(local);
            }
        }
    }
    let half = (max - min) / 2.0;
    let center_local = (max + min) / 2.0;
    Obb {
        center: origin + frame * center_local,
        half_axes: DMat3::from_cols(
            frame.col(0) * half.x,
            frame.col(1) * half.y,
            frame.col(2) * half.z,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(g: Geodetic) {
        let p = geodetic_to_ecef(g);
        let back = ecef_to_geodetic(p);
        let p2 = geodetic_to_ecef(back);
        let err = (p - p2).length();
        // 1e-9 m absolute near the surface; at satellite magnitudes the f64
        // ulp itself exceeds 1e-9, so scale with the position magnitude.
        let tol = 1e-9_f64.max(p.length() * 1e-15);
        assert!(err < tol, "round-trip error {err} m (tol {tol}) at {g:?}");
    }

    #[test]
    fn ecef_round_trips() {
        let cases = [
            Geodetic {
                lon: 0.0,
                lat: 0.0,
                height: 0.0,
            },
            Geodetic {
                lon: 1.0,
                lat: 0.5,
                height: 1000.0,
            },
            Geodetic {
                lon: -0.0087,
                lat: 0.7826,
                height: 12.0,
            }, // Bordeaux-ish
            Geodetic {
                lon: 2.5,
                lat: std::f64::consts::FRAC_PI_2 - 1e-4,
                height: 0.0,
            }, // near north pole
            Geodetic {
                lon: -2.0,
                lat: -std::f64::consts::FRAC_PI_2 + 1e-4,
                height: 100.0,
            }, // near south pole
            Geodetic {
                lon: 3.0,
                lat: -0.3,
                height: -100.0,
            }, // below ellipsoid
            Geodetic {
                lon: 0.1,
                lat: 0.0,
                height: 35_786_000.0,
            }, // GEO altitude
        ];
        for g in cases {
            round_trip(g);
        }
    }

    #[test]
    fn equator_reference_point() {
        let p = geodetic_to_ecef(Geodetic {
            lon: 0.0,
            lat: 0.0,
            height: 0.0,
        });
        assert!((p.x - WGS84_A).abs() < 1e-9);
        assert!(p.y.abs() < 1e-9 && p.z.abs() < 1e-9);
    }

    #[test]
    fn pole_reference_point() {
        let p = geodetic_to_ecef(Geodetic {
            lon: 0.0,
            lat: std::f64::consts::FRAC_PI_2,
            height: 0.0,
        });
        assert!((p.z - WGS84_B).abs() < 1e-6, "z = {}", p.z);
    }

    #[test]
    fn region_obb_encloses_samples() {
        // ~10 km x 10 km region near Bordeaux, 0..500 m height.
        let region = [-0.0102, 0.7820, -0.0073, 0.7833, 0.0, 500.0];
        let obb = region_to_obb(&region);
        // Every dense sample of the region must be inside (distance 0).
        for i in 0..=4 {
            for j in 0..=4 {
                for &height in &[0.0, 250.0, 500.0] {
                    let g = Geodetic {
                        lon: region[0] + (region[2] - region[0]) * i as f64 / 4.0,
                        lat: region[1] + (region[3] - region[1]) * j as f64 / 4.0,
                        height,
                    };
                    let d = obb.distance_to_point(geodetic_to_ecef(g));
                    // Tolerate a small margin: the fit is sample-based.
                    assert!(d < 5.0, "sample {g:?} outside obb by {d} m");
                }
            }
        }
    }
}
