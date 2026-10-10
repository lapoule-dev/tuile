// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use glam::{DMat4, DVec3, Mat4, Vec3};
use tuile_pack::BakedView;

/// Near plane as a fraction of the camera's height over the ellipsoid — the
/// stage writer's default (`TUILE_CLIP_NEAR_FRACTION`), so a film framed the
/// same clips the same.
pub const NEAR_FRACTION: f64 = 0.05;

/// One frame's camera, **eye-relative**.
///
/// The eye stays in f64. Everything the GPU sees is relative to it: the view
/// matrix is a pure rotation and each tile is placed by `origin − eye`,
/// subtracted in f64 and only then narrowed. Precision is then best exactly
/// where the picture is — around the camera — whatever the film's extent.
///
/// The projection is reverse-Z with an infinite far plane. Depth precision
/// then no longer depends on a near/far ratio at all, which is what the
/// per-frame clipping range of the stage writer was working around.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameCamera {
    pub eye: DVec3,
    /// World (ECEF axes) to view, rotation only.
    pub view: Mat4,
    /// Reverse-Z, infinite far, right-handed, depth in `0..=1`.
    pub projection: Mat4,
    pub near: f32,
}

impl FrameCamera {
    /// The camera a frame was baked for, at the aspect of the target being
    /// rendered (which may be supersampled, never reshaped).
    pub fn of(view: &BakedView, aspect: f32) -> Self {
        let eye = DVec3::from_array(view.position);
        // `look_to_rh` takes the direction as given and builds the basis
        // from it: an unnormalised one scales the view matrix, and every ray
        // the resolve builds from its rows then misses its pixel.
        let rotation = DMat4::look_to_rh(
            DVec3::ZERO,
            DVec3::from_array(view.direction).normalize(),
            DVec3::from_array(view.up),
        );
        let height = tuile_core::geo::ecef_to_geodetic(eye).height.max(10.0);
        let near = (NEAR_FRACTION * height).clamp(0.5, 100_000.0) as f32;
        Self {
            eye,
            view: rotation.as_mat4(),
            projection: Mat4::perspective_infinite_reverse_rh(view.fovy_rad as f32, aspect, near),
            near,
        }
    }

    /// Where a point stored relative to `origin_ecef` is placed for this frame.
    pub fn offset(&self, origin_ecef: [f64; 3]) -> Vec3 {
        (DVec3::from_array(origin_ecef) - self.eye).as_vec3()
    }

    /// The eye's height over the ellipsoid, in metres.
    pub fn height(&self) -> f64 {
        tuile_core::geo::ecef_to_geodetic(self.eye).height
    }

    pub fn view_projection(&self) -> Mat4 {
        self.projection * self.view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn above_null_island(height: f64) -> BakedView {
        BakedView {
            position: [tuile_core::geo::WGS84_A + height, 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            viewport_px: [1920.0, 1080.0],
            fovy_rad: 0.8,
        }
    }

    #[test]
    fn the_point_below_lands_mid_screen() {
        let cam = FrameCamera::of(&above_null_island(1000.0), 16.0 / 9.0);
        let ground = cam.offset([tuile_core::geo::WGS84_A, 0.0, 0.0]);
        let clip = cam.view_projection() * ground.extend(1.0);
        let ndc = clip.truncate() / clip.w;
        assert!(ndc.x.abs() < 1e-6 && ndc.y.abs() < 1e-6, "{ndc}");
        // Reverse-Z: in front of the camera, strictly between far (0) and near (1).
        assert!(ndc.z > 0.0 && ndc.z < 1.0, "{}", ndc.z);
    }

    #[test]
    fn up_is_up_on_screen() {
        let cam = FrameCamera::of(&above_null_island(1000.0), 1.0);
        // A point north of the nadir (ECEF +Z) must project to +Y.
        let north = cam.offset([tuile_core::geo::WGS84_A, 0.0, 100.0]);
        let clip = cam.view_projection() * north.extend(1.0);
        assert!(clip.y / clip.w > 0.0);
    }

    #[test]
    fn eye_relative_offsets_keep_centimetres_far_from_the_origin() {
        // A camera on the other side of the planet from where f32 ECEF would
        // still be exact: the offset of a point 1 cm away must survive.
        let cam = FrameCamera::of(&above_null_island(5.0), 1.0);
        let at = [tuile_core::geo::WGS84_A + 5.0 - 0.01, 0.0, 0.0];
        assert!((cam.offset(at).x + 0.01).abs() < 1e-6);
    }

    #[test]
    fn an_unnormalised_direction_gives_a_pure_rotation() {
        let mut v = above_null_island(1000.0);
        v.direction = [-3.0, 0.6, 0.3];
        let cam = FrameCamera::of(&v, 1.0);
        for row in 0..3 {
            let len = cam.view.row(row).truncate().length();
            assert!((len - 1.0).abs() < 1e-6, "row {row} has length {len}");
        }
    }

    #[test]
    fn near_follows_height() {
        assert_eq!(FrameCamera::of(&above_null_island(1.0), 1.0).near, 0.5);
        assert_eq!(
            FrameCamera::of(&above_null_island(10_000.0), 1.0).near,
            500.0
        );
    }
}
