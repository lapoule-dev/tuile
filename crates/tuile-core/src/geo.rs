// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! WGS84 ellipsoid conversions and 3D Tiles `region` handling.
//!
//! All angles are radians, all lengths meters, everything f64.

use crate::math::Obb;
use glam::{DMat3, DVec3, Mat4};
use std::f64::consts::{FRAC_PI_2, PI};

/// Anti-jitter model matrix (the second half of the protocol): places a tile —
/// whose vertices are stored relative to `origin_ecef`, with intrinsic
/// `transform_local` — into a frame rebased on `render_origin`, as the f32
/// translation `(origin_ecef - render_origin)`. Keep `render_origin` near the
/// eye so the f32 the renderer sees stays small (sub-meter precise) however far
/// the tile is from the geocenter. Render-agnostic: every backend (wgpu today,
/// THREE.js / RealityKit / Hydra tomorrow) applies this same formula.
pub fn rebased_model(origin_ecef: DVec3, transform_local: Mat4, render_origin: DVec3) -> Mat4 {
    Mat4::from_translation((origin_ecef - render_origin).as_vec3()) * transform_local
}

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
/// min height, max height (meters). Converts to an ECEF [`Obb`].
///
/// Thin wrapper over [`obb_from_rectangle`] taking the packed 6-float layout.
pub fn region_to_obb(region: &[f64; 6]) -> Obb {
    obb_from_rectangle(
        region[0], region[1], region[2], region[3], region[4], region[5],
    )
}

/// An oriented bounding box enclosing a geographic rectangle (radians) on the
/// WGS84 ellipsoid, extruded by `min_height`..`max_height` (meters).
///
/// Reimplementation of `OrientedBoundingBox.fromRectangle` (Cesium, modelled
/// — not ported): a tangent-plane fit for rectangles up to π wide, and a
/// distinct equator-oriented construction beyond π (hemispheres and larger),
/// so the box stays tight and correctly centered at every level — from a
/// street tile to a whole hemisphere.
pub fn obb_from_rectangle(
    west: f64,
    south: f64,
    east: f64,
    north: f64,
    min_height: f64,
    max_height: f64,
) -> Obb {
    let width = east - west;

    if width <= PI {
        // Tangent plane at the rectangle center; axes = east/north/up there.
        let lon_center = (west + east) / 2.0;
        let lat_mid = (south + north) / 2.0;
        let origin = geodetic_to_ecef(Geodetic {
            lon: lon_center,
            lat: lat_mid,
            height: 0.0,
        });
        let frame = enu_frame(Geodetic {
            lon: lon_center,
            lat: lat_mid,
            height: 0.0,
        });
        let (east_ax, north_ax, up_ax) = (frame.col(0), frame.col(1), frame.col(2));

        // The equator sticks out farthest; align CW to it when straddling.
        let lat_center = if south < 0.0 && north > 0.0 {
            0.0
        } else {
            lat_mid
        };
        let at = |lon: f64, lat: f64, h: f64| {
            geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: h,
            })
        };
        // Projected (x, y) of a point onto the tangent plane.
        let proj = |p: DVec3| ((p - origin).dot(east_ax), (p - origin).dot(north_ax));

        let (_, nc_y) = proj(at(lon_center, north, max_height));
        let (nw_x, nw_y) = proj(at(west, north, max_height));
        let (cw_x, _) = proj(at(west, lat_center, max_height));
        let (sw_x, sw_y) = proj(at(west, south, max_height));
        let (_, sc_y) = proj(at(lon_center, south, max_height));

        let min_x = nw_x.min(cw_x).min(sw_x);
        let max_x = -min_x; // symmetrical
        let max_y = nw_y.max(nc_y);
        let min_y = sw_y.min(sc_y);

        // Min Z from the corners at min_height (they dip below the plane).
        let plane_dist = |p: DVec3| (p - origin).dot(up_ax);
        let min_z =
            plane_dist(at(west, north, min_height)).min(plane_dist(at(west, south, min_height)));
        let max_z = max_height; // plane touches the surface at height 0

        return from_plane_extents(
            origin, east_ax, north_ax, up_ax, min_x, max_x, min_y, max_y, min_z, max_z,
        );
    }

    // width > π: a plane at the center longitude and the latitude nearest the
    // equator, rotating around Z — a better fit than a center-normal box.
    let fully_above = south > 0.0;
    let fully_below = north < 0.0;
    let lat_nearest = if fully_above {
        south
    } else if fully_below {
        north
    } else {
        0.0
    };
    let center_lon = (west + east) / 2.0;

    let mut plane_origin = geodetic_to_ecef(Geodetic {
        lon: center_lon,
        lat: lat_nearest,
        height: max_height,
    });
    plane_origin.z = 0.0; // center on the equatorial plane
    let is_pole = plane_origin.x.abs() < 1e-10 && plane_origin.y.abs() < 1e-10;
    let plane_normal = if is_pole {
        DVec3::X
    } else {
        plane_origin.normalize()
    };
    let plane_y = DVec3::Z;
    let plane_x = plane_normal.cross(plane_y);

    let at = |lon: f64, lat: f64, h: f64| {
        geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: h,
        })
    };
    // Orthogonal projection onto the plane (point − (signed dist)·normal).
    let signed_dist = |p: DVec3| (p - plane_origin).dot(plane_normal);
    let project_onto = |p: DVec3| p - signed_dist(p) * plane_normal;

    let horizon = at(center_lon + FRAC_PI_2, lat_nearest, max_height);
    let max_x = (project_onto(horizon) - plane_origin).dot(plane_x);
    let min_x = -max_x;

    let max_y = at(
        0.0,
        north,
        if fully_below { min_height } else { max_height },
    )
    .z;
    let min_y = at(
        0.0,
        south,
        if fully_above { min_height } else { max_height },
    )
    .z;

    let far = at(east, lat_nearest, max_height);
    let min_z = signed_dist(far);
    let max_z = 0.0;

    from_plane_extents(
        plane_origin,
        plane_x,
        plane_y,
        plane_normal,
        min_x,
        max_x,
        min_y,
        max_y,
        min_z,
        max_z,
    )
}

/// Builds an [`Obb`] from a plane origin, three orthonormal axes, and the
/// box extents along each (Cesium `fromPlaneExtents`).
#[allow(clippy::too_many_arguments)]
fn from_plane_extents(
    origin: DVec3,
    x_axis: DVec3,
    y_axis: DVec3,
    z_axis: DVec3,
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
    min_z: f64,
    max_z: f64,
) -> Obb {
    let center_offset = DVec3::new(
        (min_x + max_x) / 2.0,
        (min_y + max_y) / 2.0,
        (min_z + max_z) / 2.0,
    );
    let scale = DVec3::new(
        (max_x - min_x) / 2.0,
        (max_y - min_y) / 2.0,
        (max_z - min_z) / 2.0,
    );
    let center =
        origin + x_axis * center_offset.x + y_axis * center_offset.y + z_axis * center_offset.z;
    Obb {
        center,
        half_axes: DMat3::from_cols(x_axis * scale.x, y_axis * scale.y, z_axis * scale.z),
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
                    assert!(d < 1.0, "sample {g:?} outside obb by {d} m");
                }
            }
        }
    }

    /// Containment must hold at ALL scales — including a hemisphere and the
    /// whole ellipsoid (the case `region_to_obb`'s old sampling fit broke on).
    #[test]
    fn obb_encloses_large_rectangles() {
        let cases: [[f64; 6]; 3] = [
            // Western hemisphere (width = π), pole to pole.
            [-PI, -FRAC_PI_2, 0.0, FRAC_PI_2, 0.0, 0.0],
            // Three-quarters around, mid latitudes (width = 1.5π > π branch).
            [-PI, -0.5, PI / 2.0, 0.5, 0.0, 1000.0],
            // The entire ellipsoid (width = 2π, height = π).
            [-PI, -FRAC_PI_2, PI, FRAC_PI_2, -500.0, 9000.0],
        ];
        for region in cases {
            let obb = region_to_obb(&region);
            let [w, s, e, n, min_h, max_h] = region;
            // Dense surface samples of the rectangle must all sit inside.
            for i in 0..=6 {
                for j in 0..=6 {
                    for &h in &[min_h, max_h] {
                        let g = Geodetic {
                            lon: w + (e - w) * i as f64 / 6.0,
                            lat: s + (n - s) * j as f64 / 6.0,
                            height: h,
                        };
                        let d = obb.distance_to_point(geodetic_to_ecef(g));
                        assert!(d < 1.0, "region {region:?}: sample outside obb by {d} m");
                    }
                }
            }
        }
    }

    #[test]
    fn whole_ellipsoid_obb_is_centered_at_origin() {
        // Cesium's "spans over half the ellipsoid" test: full globe → center
        // at the geocenter.
        let obb = region_to_obb(&[-PI, -FRAC_PI_2, PI, FRAC_PI_2, 0.0, 0.0]);
        assert!(obb.center.length() < 1.0, "center {:?}", obb.center);
    }
}
