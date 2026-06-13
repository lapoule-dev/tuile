// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Orbital camera in ECEF f64, with f32 relative-to-origin output.
//!
//! The camera orbits a target in a local frame (ENU when the target is
//! georeferenced, world axes for test tilesets near the geocenter). All
//! state is f64; only the final view-projection matrix is cast to f32,
//! rebased onto the render origin — the second half of the anti-jitter
//! protocol (`docs/01-architecture.md`).

use glam::{DMat3, DVec2, DVec3, Mat4, Vec3};
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_core::traversal::ViewState;
use tuile_wgpu::ViewUniform;

pub struct OrbitCamera {
    pub target: DVec3,
    pub distance: f64,
    /// Radians around the local up axis.
    pub yaw: f64,
    /// Radians above the local horizon, clamped away from the poles.
    pub pitch: f64,
    pub fovy: f64,
    local_frame: DMat3,
}

impl OrbitCamera {
    /// Frames `target` (ECEF) at `distance` meters. Picks a local frame:
    /// ENU for georeferenced targets, world axes near the geocenter.
    pub fn new(target: DVec3, distance: f64) -> Self {
        let local_frame = if target.length() > 1.0 {
            enu_frame(ecef_to_geodetic(target))
        } else {
            DMat3::IDENTITY
        };
        Self {
            target,
            distance,
            yaw: 0.6,
            pitch: 0.5,
            fovy: 60f64.to_radians(),
            local_frame,
        }
    }

    /// Camera position in ECEF.
    pub fn position(&self) -> DVec3 {
        let (east, north, up) = (
            self.local_frame.col(0),
            self.local_frame.col(1),
            self.local_frame.col(2),
        );
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        let dir = east * (cp * cy) + north * (cp * sy) + up * sp;
        self.target + dir * self.distance
    }

    fn up(&self) -> DVec3 {
        self.local_frame.col(2)
    }

    pub fn orbit(&mut self, dyaw: f64, dpitch: f64) {
        self.yaw += dyaw;
        self.pitch = (self.pitch + dpitch).clamp(-1.5, 1.5);
    }

    /// Logarithmic zoom (mouse wheel). `delta` > 0 zooms in.
    pub fn zoom(&mut self, delta: f64) {
        self.distance = (self.distance * (-delta * 0.1).exp()).max(0.01);
    }

    /// Pans the target in the local east/up plane (right drag).
    pub fn pan(&mut self, dx: f64, dy: f64) {
        let scale = self.distance * 0.001;
        let right = self.local_frame.col(0);
        let up = self.local_frame.col(2);
        self.target += right * (-dx * scale) + up * (dy * scale);
    }

    /// View for the traversal (ECEF f64).
    pub fn view_state(&self, viewport: DVec2) -> ViewState {
        let position = self.position();
        ViewState::perspective(
            position,
            self.target - position,
            self.up(),
            viewport,
            self.fovy,
        )
    }

    /// View-projection uniform for rendering, relative to `render_origin`.
    pub fn view_uniform(&self, render_origin: DVec3, aspect: f32, sun_dir: Vec3) -> ViewUniform {
        let eye = (self.position() - render_origin).as_vec3();
        let target = (self.target - render_origin).as_vec3();
        let up = self.up().as_vec3();
        let view = Mat4::look_at_rh(eye, target, up);
        let proj = Mat4::perspective_rh(self.fovy as f32, aspect.max(1e-3), 0.05, 1.0e9);
        ViewUniform {
            view_proj: (proj * view).to_cols_array(),
            sun_dir: [sun_dir.x, sun_dir.y, sun_dir.z, 0.0],
            params: [0.3, 0.0, 0.0, 0.0],
        }
    }
}
