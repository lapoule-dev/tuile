// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use tuile_film::{FrameCamera, Look};

/// `Frame` in `common.wgsl`, byte for byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FrameUniform {
    pub view_proj: [[f32; 4]; 4],
    pub right: [f32; 4],
    pub up: [f32; 4],
    pub fwd: [f32; 4],
    pub world: [f32; 4],
    pub sun: [f32; 4],
    pub to_sun: [f32; 4],
    pub size: [u32; 4],
}

impl FrameUniform {
    pub fn new(camera: &FrameCamera, look: &Look, size: [u32; 4]) -> Self {
        // The view matrix is a pure rotation whose rows are the camera's
        // right, up and backward axes; the projection's diagonal holds
        // 1/tan(fovy/2), over the aspect for x. A pixel's ray is then
        // `fwd + x·right + y·up` for its NDC x, y.
        let v = camera.view;
        let p = camera.projection;
        let right = v.row(0).truncate() / p.x_axis.x;
        let up = v.row(1).truncate() / p.y_axis.y;
        let fwd = -v.row(2).truncate();
        Self {
            view_proj: camera.view_projection().to_cols_array_2d(),
            right: right.extend(0.0).to_array(),
            up: up.extend(0.0).to_array(),
            fwd: fwd.extend(0.0).to_array(),
            world: look.world.extend(look.exposure_scale()).to_array(),
            sun: look.sun.extend(0.0).to_array(),
            to_sun: look.to_sun.extend(0.0).to_array(),
            size,
        }
    }
}

/// `TileFrame` in `common.wgsl`, byte for byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TileFrame {
    pub offset: [f32; 3],
    pub flags: u32,
    pub factor: [f32; 4],
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{Vec3, Vec4Swizzles};
    use tuile_film::BakedView;

    /// The ray the resolve builds for a pixel must pass through the point the
    /// raster projected to that pixel — or barycentrics come out of the wrong
    /// triangle's plane.
    #[test]
    fn a_pixel_ray_goes_through_what_projects_there() {
        let view = BakedView {
            position: [7_000_000.0, 10.0, 20.0],
            direction: [-1.0, 0.2, 0.1],
            up: [0.0, 0.0, 1.0],
            viewport_px: [1920.0, 1080.0],
            fovy_rad: 0.9,
        };
        let cam = FrameCamera::of(&view, 16.0 / 9.0);
        let u = FrameUniform::new(&cam, &Look::default(), [1920, 1080, 1, 0]);
        let point = Vec3::new(-3000.0, 900.0, -400.0);
        let clip = cam.view_projection() * point.extend(1.0);
        let ndc = clip.xy() / clip.w;
        let ray = Vec3::from_slice(&u.fwd)
            + ndc.x * Vec3::from_slice(&u.right)
            + ndc.y * Vec3::from_slice(&u.up);
        let angle = ray.normalize().angle_between(point.normalize());
        assert!(angle < 1e-5, "{angle}");
    }
}
