// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! f64 geometry primitives: oriented bounding boxes, spheres, frustums.
//!
//! Everything here is in ECEF (or any metric frame) and stays in f64:
//! geospatial magnitudes (~6.4e6 m) lose ~0.5 m of precision in f32.

use glam::{DMat3, DMat4, DVec3, DVec4};

/// Oriented bounding box, matching the 3D Tiles `box` layout:
/// a center and three half-axis vectors (columns of `half_axes`),
/// each vector's length being the half-extent along that axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Obb {
    pub center: DVec3,
    pub half_axes: DMat3,
}

impl Obb {
    /// Builds an OBB from the 12 numbers of a 3D Tiles `boundingVolume.box`.
    pub fn from_tiles_box(v: &[f64; 12]) -> Self {
        Self {
            center: DVec3::new(v[0], v[1], v[2]),
            half_axes: DMat3::from_cols(
                DVec3::new(v[3], v[4], v[5]),
                DVec3::new(v[6], v[7], v[8]),
                DVec3::new(v[9], v[10], v[11]),
            ),
        }
    }

    /// The eight corners of the box.
    pub fn corners(&self) -> [DVec3; 8] {
        let (x, y, z) = (
            self.half_axes.col(0),
            self.half_axes.col(1),
            self.half_axes.col(2),
        );
        let c = self.center;
        [
            c + x + y + z,
            c + x + y - z,
            c + x - y + z,
            c + x - y - z,
            c - x + y + z,
            c - x + y - z,
            c - x - y + z,
            c - x - y - z,
        ]
    }

    /// Transforms the box by an affine matrix.
    pub fn transformed(&self, m: &DMat4) -> Self {
        Self {
            center: m.transform_point3(self.center),
            half_axes: DMat3::from_cols(
                m.transform_vector3(self.half_axes.col(0)),
                m.transform_vector3(self.half_axes.col(1)),
                m.transform_vector3(self.half_axes.col(2)),
            ),
        }
    }

    /// Distance from a point to the surface of the box (0 if inside).
    pub fn distance_to_point(&self, p: DVec3) -> f64 {
        let d = p - self.center;
        // Project onto each (normalized) axis and clamp to the half-extent.
        let mut closest = self.center;
        for i in 0..3 {
            let axis = self.half_axes.col(i);
            let len = axis.length();
            if len <= f64::EPSILON {
                continue;
            }
            let dir = axis / len;
            let t = d.dot(dir).clamp(-len, len);
            closest += dir * t;
        }
        (p - closest).length()
    }

    /// A sphere that bounds this box.
    pub fn bounding_sphere(&self) -> Sphere {
        let r = (self.half_axes.col(0) + self.half_axes.col(1) + self.half_axes.col(2)).length();
        // Conservative: corner radius is the max over corner distances; the sum
        // of column vectors is one specific corner, take the max of all eight.
        let r = self
            .corners()
            .iter()
            .map(|c| (*c - self.center).length())
            .fold(r, f64::max);
        Sphere {
            center: self.center,
            radius: r,
        }
    }
}

/// Bounding sphere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sphere {
    pub center: DVec3,
    pub radius: f64,
}

impl Sphere {
    /// Distance from a point to the surface of the sphere (0 if inside).
    pub fn distance_to_point(&self, p: DVec3) -> f64 {
        ((p - self.center).length() - self.radius).max(0.0)
    }
}

/// A plane in `ax + by + cz + d = 0` form; the half-space `n·p + d >= 0`
/// is the "inside".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plane {
    pub normal: DVec3,
    pub d: f64,
}

impl Plane {
    fn from_vec4(v: DVec4) -> Self {
        let n = DVec3::new(v.x, v.y, v.z);
        let len = n.length();
        Self {
            normal: n / len,
            d: v.w / len,
        }
    }

    /// Signed distance from the plane (positive = inside half-space).
    pub fn signed_distance(&self, p: DVec3) -> f64 {
        self.normal.dot(p) + self.d
    }
}

/// View frustum as six inward-facing planes, extracted from a
/// `projection * view` matrix with 0..1 depth (wgpu/Metal convention).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frustum {
    pub planes: [Plane; 6],
}

impl Frustum {
    /// Gribb–Hartmann plane extraction (0..1 depth range).
    pub fn from_view_proj(m: &DMat4) -> Self {
        let r0 = m.row(0);
        let r1 = m.row(1);
        let r2 = m.row(2);
        let r3 = m.row(3);
        Self {
            planes: [
                Plane::from_vec4(r3 + r0), // left
                Plane::from_vec4(r3 - r0), // right
                Plane::from_vec4(r3 + r1), // bottom
                Plane::from_vec4(r3 - r1), // top
                Plane::from_vec4(r2),      // near (0..1 depth)
                Plane::from_vec4(r3 - r2), // far
            ],
        }
    }

    /// Conservative sphere test: false only if certainly outside.
    pub fn intersects_sphere(&self, s: &Sphere) -> bool {
        self.planes
            .iter()
            .all(|p| p.signed_distance(s.center) >= -s.radius)
    }

    /// Conservative OBB test (effective-radius per plane): false only if
    /// the box is certainly outside.
    pub fn intersects_obb(&self, b: &Obb) -> bool {
        self.planes.iter().all(|p| {
            let r = b.half_axes.col(0).dot(p.normal).abs()
                + b.half_axes.col(1).dot(p.normal).abs()
                + b.half_axes.col(2).dot(p.normal).abs();
            p.signed_distance(b.center) >= -r
        })
    }
}

/// A bounding volume in world (ECEF) coordinates.
///
/// 3D Tiles `region` volumes are converted to an [`Obb`] at load time
/// (see [`crate::geo::region_to_obb`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoundingVolume {
    Obb(Obb),
    Sphere(Sphere),
}

impl BoundingVolume {
    pub fn distance_to_point(&self, p: DVec3) -> f64 {
        match self {
            Self::Obb(b) => b.distance_to_point(p),
            Self::Sphere(s) => s.distance_to_point(p),
        }
    }

    pub fn intersects_frustum(&self, f: &Frustum) -> bool {
        match self {
            Self::Obb(b) => f.intersects_obb(b),
            Self::Sphere(s) => f.intersects_sphere(s),
        }
    }

    pub fn center(&self) -> DVec3 {
        match self {
            Self::Obb(b) => b.center,
            Self::Sphere(s) => s.center,
        }
    }

    pub fn transformed(&self, m: &DMat4) -> Self {
        match self {
            Self::Obb(b) => Self::Obb(b.transformed(m)),
            Self::Sphere(s) => {
                // Conservative under non-uniform scale: scale radius by the
                // largest singular value approximation (max column length).
                let scale = (0..3)
                    .map(|i| m.transform_vector3(DMat3::IDENTITY.col(i)).length())
                    .fold(0.0, f64::max);
                Self::Sphere(Sphere {
                    center: m.transform_point3(s.center),
                    radius: s.radius * scale,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::dvec3;

    fn unit_obb() -> Obb {
        Obb {
            center: DVec3::ZERO,
            half_axes: DMat3::from_diagonal(dvec3(1.0, 2.0, 3.0)),
        }
    }

    #[test]
    fn obb_distance_inside_is_zero() {
        assert_eq!(unit_obb().distance_to_point(dvec3(0.5, -1.0, 2.0)), 0.0);
    }

    #[test]
    fn obb_distance_outside_face() {
        let d = unit_obb().distance_to_point(dvec3(3.0, 0.0, 0.0));
        assert!((d - 2.0).abs() < 1e-12, "got {d}");
    }

    #[test]
    fn obb_distance_outside_corner() {
        let d = unit_obb().distance_to_point(dvec3(2.0, 3.0, 4.0));
        assert!((d - (3.0f64).sqrt()).abs() < 1e-12, "got {d}");
    }

    #[test]
    fn obb_distance_on_face_is_zero() {
        assert_eq!(unit_obb().distance_to_point(dvec3(1.0, 0.0, 0.0)), 0.0);
    }

    #[test]
    fn rotated_obb_distance() {
        // 45° around Z, half extents (1, 1, 1): the box +X face now points
        // along (1,1)/√2. The point (2, 2, 0) projects onto that axis at
        // 2√2, so the closest point is the face at 1: distance 2√2 − 1.
        let rot = DMat3::from_rotation_z(std::f64::consts::FRAC_PI_4);
        let obb = Obb {
            center: DVec3::ZERO,
            half_axes: rot,
        };
        let d = obb.distance_to_point(dvec3(2.0, 2.0, 0.0));
        let expected = (8.0f64).sqrt() - 1.0;
        assert!((d - expected).abs() < 1e-12, "got {d}, want {expected}");
    }

    #[test]
    fn frustum_sphere_culling() {
        // Camera at origin looking down -Z (right-handed), 90° fov, square.
        let proj = DMat4::perspective_rh(std::f64::consts::FRAC_PI_2, 1.0, 0.1, 1e9);
        let view = DMat4::look_at_rh(DVec3::ZERO, dvec3(0.0, 0.0, -1.0), DVec3::Y);
        let f = Frustum::from_view_proj(&(proj * view));

        let visible = Sphere {
            center: dvec3(0.0, 0.0, -10.0),
            radius: 1.0,
        };
        let behind = Sphere {
            center: dvec3(0.0, 0.0, 10.0),
            radius: 1.0,
        };
        let far_left = Sphere {
            center: dvec3(-100.0, 0.0, -10.0),
            radius: 1.0,
        };
        let straddles_near = Sphere {
            center: DVec3::ZERO,
            radius: 5.0,
        };
        assert!(f.intersects_sphere(&visible));
        assert!(!f.intersects_sphere(&behind));
        assert!(!f.intersects_sphere(&far_left));
        assert!(f.intersects_sphere(&straddles_near));
    }

    #[test]
    fn frustum_obb_culling() {
        let proj = DMat4::perspective_rh(std::f64::consts::FRAC_PI_2, 1.0, 0.1, 1e9);
        let view = DMat4::look_at_rh(DVec3::ZERO, dvec3(0.0, 0.0, -1.0), DVec3::Y);
        let f = Frustum::from_view_proj(&(proj * view));

        let visible = Obb {
            center: dvec3(0.0, 0.0, -10.0),
            half_axes: DMat3::IDENTITY,
        };
        let outside = Obb {
            center: dvec3(0.0, 200.0, -10.0),
            half_axes: DMat3::IDENTITY,
        };
        // Large box enclosing the whole camera: must not be culled.
        let enclosing = Obb {
            center: DVec3::ZERO,
            half_axes: DMat3::from_diagonal(dvec3(1e6, 1e6, 1e6)),
        };
        assert!(f.intersects_obb(&visible));
        assert!(!f.intersects_obb(&outside));
        assert!(f.intersects_obb(&enclosing));
    }
}
