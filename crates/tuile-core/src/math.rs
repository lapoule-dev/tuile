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

/// A sphere that hides whatever is behind it. For a globe, the planet.
///
/// # Why this exists
///
/// The culling frustum of a ground-level camera is a cone that goes straight
/// through the planet and out the far side, and everything it meets on the way
/// out is inside it. Without this, a camera 5 km up selected 617 tiles reaching
/// 11 205 km — almost antipodal ground, fetched, decoded, draped, encoded and
/// handed to a renderer that could never draw a pixel of it, while the horizon
/// from that altitude is 252 km away (measured, 2026-09-08). Frustum culling
/// cannot see this: those tiles really are inside the frustum. Only occlusion
/// can.
///
/// # Why a sphere for an ellipsoid
///
/// Take the ellipsoid's *smallest* radius. A sphere inscribed in the planet
/// hides strictly less than the planet does, so every tile this culls is one
/// the real planet also hides — which is the only direction in which being
/// wrong is acceptable. Black ground is a bug; a tile drawn that need not have
/// been is a rounding error in the bill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Occluder {
    pub center: DVec3,
    pub radius: f64,
}

impl Occluder {
    /// Whether `volume` is *certainly* hidden behind this sphere, seen from
    /// `eye`.
    ///
    /// Conservative on purpose, twice over: the sphere is inscribed in the real
    /// occluder, and the occludee is tested as a whole rather than as a point.
    /// The whole-volume part is done by shrinking the occluder by the
    /// occludee's radius — the shadow of the smaller sphere is contained in the
    /// shadow of the real one, eroded by that radius, so a volume whose centre
    /// falls inside it has all of itself inside the real shadow. A tile bigger
    /// than the planet is therefore never culled, which is right: a coarse tile
    /// straddling the horizon must be visited so its children can be judged
    /// one by one.
    pub fn hides(&self, volume: &BoundingVolume, eye: DVec3) -> bool {
        let (center, radius) = volume.bounding_sphere();
        let shrunk = self.radius - radius;
        if shrunk <= 0.0 {
            return false;
        }
        // Eye to planet centre, and the squared length of the tangent from the
        // eye to the shrunken sphere — the "horizon distance", squared.
        let to_eye = eye - self.center;
        let horizon_sq = to_eye.length_squared() - shrunk * shrunk;
        if horizon_sq <= 0.0 {
            // Inside the sphere: there is no horizon and nothing is behind it.
            return false;
        }
        // Eye to the occludee, and how far along the eye→centre axis it sits.
        let to_volume = center - eye;
        let depth = -to_volume.dot(to_eye);
        // Beyond the horizon plane, and inside the shadow cone. Both, or the
        // test would hide ground that is merely far away off to one side.
        depth > horizon_sq && depth * depth > horizon_sq * to_volume.length_squared()
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

    /// The volume as a centre and a radius that contains it.
    ///
    /// Exact for a sphere. For a box, the distance to the farthest of its eight
    /// corners — four sign combinations, the other four being their mirrors.
    /// Not the tightest enclosing sphere, which would need an optimisation; it
    /// is an upper bound, which is what a conservative test wants.
    pub fn bounding_sphere(&self) -> (DVec3, f64) {
        match self {
            Self::Sphere(s) => (s.center, s.radius),
            Self::Obb(b) => {
                let (x, y, z) = (b.half_axes.col(0), b.half_axes.col(1), b.half_axes.col(2));
                let radius = [x + y + z, x + y - z, x - y + z, x - y - z]
                    .into_iter()
                    .map(|corner| corner.length())
                    .fold(0.0, f64::max);
                (b.center, radius)
            }
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
    /// Ground truth, computed a completely different way: does the straight
    /// line from the eye to the point pass through the sphere before reaching
    /// it? If it does, the sphere is in the way. `Occluder::hides` must agree
    /// with this for a point-sized occludee, and never claim more than it for
    /// one with size.
    /// `None` where the answer is a tangent — the line of sight grazes the
    /// sphere within a millimetre, and neither this nor the test under test can
    /// be held to an answer at that distance in f64 over 6 000 km. Saying so is
    /// better than picking a side and calling the disagreement a bug.
    fn segment_meets_sphere(
        eye: DVec3,
        point: DVec3,
        center: DVec3,
        radius: f64,
    ) -> Option<bool> {
        let d = point - eye;
        let len = d.length();
        if len == 0.0 {
            return Some(false);
        }
        let dir = d / len;
        let to_centre = center - eye;
        // Where along the segment the closest approach happens, clamped to it.
        let t = to_centre.dot(dir).clamp(0.0, len);
        let closest = (eye + dir * t).distance(center);
        ((closest - radius).abs() > 1e-3).then_some(closest < radius)
    }

    fn earth() -> Occluder {
        Occluder {
            center: DVec3::ZERO,
            radius: 6_356_752.0,
        }
    }

    fn point_volume(p: DVec3) -> BoundingVolume {
        BoundingVolume::Sphere(Sphere {
            center: p,
            radius: 0.0,
        })
    }

    /// A grid of places to look at, from the sub-eye point round to the
    /// antipode, at several heights — surface, aircraft, orbit.
    fn sample_points(radius: f64) -> Vec<DVec3> {
        let mut out = Vec::new();
        for step in 0..=72 {
            let angle = f64::from(step) * std::f64::consts::PI / 72.0;
            // Never exactly 0: a point ON the occluder is a tangent, and a
            // tangent has no answer. Terrain is never on the reference sphere
            // either — it is the sphere the ellipsoid was inscribed in.
            for height in [1.0, 3_000.0, 400_000.0, 2.0 * radius] {
                let r = radius + height;
                out.push(DVec3::new(r * angle.cos(), r * angle.sin(), 0.0));
                out.push(DVec3::new(r * angle.cos(), 0.0, r * angle.sin()));
            }
        }
        out
    }

    #[test]
    fn the_horizon_test_agrees_with_the_line_of_sight() {
        let planet = earth();
        for altitude in [5_000.0, 250_000.0, 35_786_000.0] {
            let eye = DVec3::new(planet.radius + altitude, 0.0, 0.0);
            let mut decided = 0;
            for p in sample_points(planet.radius) {
                let Some(truth) = segment_meets_sphere(eye, p, planet.center, planet.radius)
                else {
                    continue;
                };
                decided += 1;
                assert_eq!(
                    planet.hides(&point_volume(p), eye),
                    truth,
                    "at {altitude} m over {p:?}"
                );
            }
            assert!(decided > 500, "only {decided} samples were decidable");
        }
    }

    #[test]
    fn ground_this_side_of_the_horizon_is_never_hidden() {
        let planet = earth();
        let eye = DVec3::new(planet.radius + 5_000.0, 0.0, 0.0);
        // The horizon from 5 km sits 2.27° round the curve; 1° is short of it.
        let near = 1.0f64.to_radians();
        let visible = DVec3::new(planet.radius * near.cos(), planet.radius * near.sin(), 0.0);
        assert!(!planet.hides(&point_volume(visible), eye));
        // And 5° is well past it — the ground that was costing 617 tiles.
        let far = 5.0f64.to_radians();
        let beyond = DVec3::new(planet.radius * far.cos(), planet.radius * far.sin(), 0.0);
        assert!(planet.hides(&point_volume(beyond), eye));
    }

    #[test]
    fn size_only_ever_makes_the_test_more_cautious() {
        let planet = earth();
        let eye = DVec3::new(planet.radius + 5_000.0, 0.0, 0.0);
        for p in sample_points(planet.radius) {
            let as_point = planet.hides(&point_volume(p), eye);
            for radius in [1.0, 10_000.0, 500_000.0] {
                let sized = BoundingVolume::Sphere(Sphere { center: p, radius });
                assert!(
                    !planet.hides(&sized, eye) || as_point,
                    "a volume of radius {radius} at {p:?} was culled where its \
                     own centre was not — culling ground that may be visible"
                );
            }
        }
    }

    #[test]
    fn nothing_bigger_than_the_planet_is_ever_hidden() {
        let planet = earth();
        let eye = DVec3::new(planet.radius + 5_000.0, 0.0, 0.0);
        // A root tile of a global quadtree, on the far side, enclosing more
        // than the occluder: it has to be visited so its children can be judged.
        let huge = BoundingVolume::Sphere(Sphere {
            center: DVec3::new(-planet.radius, 0.0, 0.0),
            radius: planet.radius * 1.1,
        });
        assert!(!planet.hides(&huge, eye));
    }

    #[test]
    fn an_eye_inside_the_planet_hides_nothing() {
        let planet = earth();
        let eye = DVec3::new(planet.radius * 0.5, 0.0, 0.0);
        for p in sample_points(planet.radius) {
            assert!(!planet.hides(&point_volume(p), eye));
        }
    }

    #[test]
    fn a_box_reports_a_radius_that_contains_its_corners() {
        let b = Obb {
            center: DVec3::new(1.0, 2.0, 3.0),
            half_axes: DMat3::from_diagonal(DVec3::new(2.0, 3.0, 6.0)),
        };
        let (center, radius) = BoundingVolume::Obb(b).bounding_sphere();
        assert_eq!(center, b.center);
        assert!((radius - 7.0).abs() < 1e-9, "{radius}");
    }

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
