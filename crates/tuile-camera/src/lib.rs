// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-camera
//!
//! A render-agnostic globe camera and input controller. Everything is f64
//! ECEF; the only outputs are a [`tuile_core::traversal::ViewState`] (for the
//! SSE traversal) and a rebased f32 view-projection (for any renderer). There
//! is **no** dependency on a windowing or graphics library: a façade feeds
//! pixel-space gestures in and reads the camera out, so the very same control
//! logic drives winit, an egui panel, a web front-end or touch.
//!
//! The gestures follow Cesium's `ScreenSpaceCameraController`:
//! - **drag** = pick the ellipsoid under the cursor at the gesture's start and
//!   end, then rotate the camera about the geocenter so the start point lands
//!   under the end cursor — exact 1:1 surface dragging, independent of zoom
//!   (Cesium `pan3D`);
//! - **zoom** scales the *altitude* toward the point under the cursor, clamped
//!   above the surface (never through it);
//! - **tilt** / **heading** orbit the look-at point on the surface.

use glam::{DQuat, DVec3, Mat4};
use tuile_core::geo::{
    ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic, WGS84_A, WGS84_B,
};
use tuile_core::traversal::ViewState;

/// WGS84 ellipsoid radii (x = y = equatorial, z = polar).
fn radii() -> DVec3 {
    DVec3::new(WGS84_A, WGS84_A, WGS84_B)
}

/// A free camera in ECEF: eye position, view direction and up — the minimal
/// state Cesium's `Camera` carries, enough to build any view.
#[derive(Debug, Clone, Copy)]
pub struct GlobeCamera {
    pub position: DVec3,
    pub direction: DVec3,
    pub up: DVec3,
    pub fovy: f64,
}

impl GlobeCamera {
    /// From an eye looking at a target, with an up hint.
    pub fn look_at(eye: DVec3, target: DVec3, up: DVec3, fovy: f64) -> Self {
        let direction = (target - eye).normalize();
        Self::from_dir(eye, direction, up, fovy)
    }

    fn from_dir(position: DVec3, direction: DVec3, up_hint: DVec3, fovy: f64) -> Self {
        let direction = direction.normalize();
        // Orthonormalize up against direction, falling back if parallel.
        let mut right = direction.cross(up_hint);
        if right.length_squared() < 1e-12 {
            right = direction.cross(DVec3::Z);
            if right.length_squared() < 1e-12 {
                right = direction.cross(DVec3::X);
            }
        }
        let right = right.normalize();
        let up = right.cross(direction).normalize();
        Self {
            position,
            direction,
            up,
            fovy,
        }
    }

    /// A camera over a geodetic point at `alt` meters, `heading` clockwise from
    /// north and `pitch` below the horizon (`PI/2` = straight down), in radians.
    pub fn from_geodetic(
        lat: f64,
        lon: f64,
        alt: f64,
        heading: f64,
        pitch: f64,
        fovy: f64,
    ) -> Self {
        let g = Geodetic {
            lon,
            lat,
            height: 0.0,
        };
        let enu = enu_frame(g);
        let (east, north, up) = (enu.col(0), enu.col(1), enu.col(2));
        let position = geodetic_to_ecef(g) + up * alt;
        let horizontal = north * heading.cos() + east * heading.sin();
        let direction = horizontal * pitch.cos() - up * pitch.sin();
        Self::from_dir(position, direction, up, fovy)
    }

    pub fn right(&self) -> DVec3 {
        self.direction.cross(self.up).normalize()
    }

    /// Geodetic height of the eye above the ellipsoid (meters).
    pub fn altitude(&self) -> f64 {
        ecef_to_geodetic(self.position).height
    }

    /// The view for the SSE traversal (ECEF f64).
    pub fn view_state(&self, viewport: glam::DVec2) -> ViewState {
        ViewState::perspective(self.position, self.direction, self.up, viewport, self.fovy)
    }

    /// The f32 view-projection for a renderer, rebased onto `render_origin`
    /// (anti-jitter). Near/far are derived from altitude and the horizon
    /// distance so depth precision holds from orbit to street level.
    pub fn view_proj(&self, render_origin: DVec3, aspect: f32) -> [f32; 16] {
        let eye = (self.position - render_origin).as_vec3();
        let target = (self.position + self.direction - render_origin).as_vec3();
        let view = Mat4::look_at_rh(eye, target, self.up.as_vec3());
        let altitude = self.altitude().max(1.0);
        let horizon = (altitude * (2.0 * WGS84_A + altitude)).sqrt();
        let near = (altitude * 0.25).max(1.0) as f32;
        let far = (horizon * 1.5 + 10_000.0) as f32;
        let proj = Mat4::perspective_rh(self.fovy as f32, aspect.max(1e-3), near, far);
        (proj * view).to_cols_array()
    }
}

/// Maps pixel-space gestures onto a [`GlobeCamera`]. Façade-agnostic: feed it
/// cursor pixels + the viewport, read the camera back.
#[derive(Debug, Clone)]
pub struct CameraController {
    pub camera: GlobeCamera,
    /// Closest the eye may get to the surface (meters) — never zooms through.
    pub min_altitude: f64,
    /// Eased toward by [`Self::update`] for smooth motion (optional).
    target: GlobeCamera,
}

impl CameraController {
    pub fn new(camera: GlobeCamera) -> Self {
        Self {
            camera,
            min_altitude: 100.0,
            target: camera,
        }
    }

    pub fn with_min_altitude(mut self, m: f64) -> Self {
        self.min_altitude = m;
        self
    }

    /// The world ray (origin, normalized direction) through a screen pixel
    /// (top-left origin), for the *current* camera.
    pub fn ray(&self, px: (f64, f64), viewport: (f64, f64)) -> (DVec3, DVec3) {
        let cam = &self.target;
        let aspect = viewport.0 / viewport.1.max(1.0);
        let tan_y = (cam.fovy * 0.5).tan();
        let tan_x = tan_y * aspect;
        // NDC in [-1,1], y up.
        let ndc_x = 2.0 * px.0 / viewport.0 - 1.0;
        let ndc_y = 1.0 - 2.0 * px.1 / viewport.1;
        let right = cam.right();
        let dir = (cam.direction + right * (ndc_x * tan_x) + cam.up * (ndc_y * tan_y)).normalize();
        (cam.position, dir)
    }

    /// The ECEF point where the pixel's ray meets the ellipsoid, if any.
    pub fn pick(&self, px: (f64, f64), viewport: (f64, f64)) -> Option<DVec3> {
        let (o, d) = self.ray(px, viewport);
        ray_ellipsoid(o, d)
    }

    /// Drag the globe: the surface point under `start` follows the cursor to
    /// `end` — Cesium's `pan3D` core (pick both, rotate about the geocenter).
    pub fn drag(&mut self, start: (f64, f64), end: (f64, f64), viewport: (f64, f64)) {
        let (Some(p0), Some(p1)) = (self.pick(start, viewport), self.pick(end, viewport)) else {
            return;
        };
        let a = p1.normalize();
        let b = p0.normalize();
        let axis = a.cross(b);
        let len = axis.length();
        if len < 1e-12 {
            return;
        }
        // Rotate the camera by the rotation taking p1 → p0: the end-cursor ray,
        // which currently hits p1, then hits p0 instead.
        let q = DQuat::from_axis_angle(axis / len, a.dot(b).clamp(-1.0, 1.0).acos());
        let c = &mut self.target;
        c.position = q * c.position;
        c.direction = (q * c.direction).normalize();
        c.up = (q * c.up).normalize();
    }

    /// Zoom toward the point under `cursor`, scaling altitude (gentle near the
    /// ground), clamped above the surface. `delta` > 0 zooms in.
    pub fn zoom(&mut self, delta: f64, cursor: (f64, f64), viewport: (f64, f64)) {
        let toward = self
            .pick(cursor, viewport)
            .unwrap_or_else(|| self.target.position.normalize() * WGS84_A);
        let factor = (-delta * 0.15).exp();
        let candidate = toward + (self.target.position - toward) * factor;
        // Always allow zooming out; only zoom in while above the floor.
        if factor >= 1.0 || ecef_to_geodetic(candidate).height >= self.min_altitude {
            self.target.position = candidate;
        }
    }

    /// Tilt the view (angle from nadir toward the horizon) about the surface
    /// point under the screen centre. `delta` radians, + tilts up.
    pub fn tilt(&mut self, delta: f64, viewport: (f64, f64)) {
        let center = match self.pick((viewport.0 * 0.5, viewport.1 * 0.5), viewport) {
            Some(c) => c,
            None => return,
        };
        let q = DQuat::from_axis_angle(self.target.right(), delta);
        let c = &mut self.target;
        c.position = center + q * (c.position - center);
        c.direction = (q * c.direction).normalize();
        c.up = (q * c.up).normalize();
        // Keep the eye above the surface.
        if c.altitude() < self.min_altitude {
            *c = self.camera; // revert to last committed if it dipped under
        }
    }

    /// Rotate heading about the surface point under the screen centre (spin the
    /// view around the local vertical). `delta` radians.
    pub fn rotate_heading(&mut self, delta: f64, viewport: (f64, f64)) {
        let Some(center) = self.pick((viewport.0 * 0.5, viewport.1 * 0.5), viewport) else {
            return;
        };
        let axis = center.normalize();
        let q = DQuat::from_axis_angle(axis, delta);
        let c = &mut self.target;
        c.position = center + q * (c.position - center);
        c.direction = (q * c.direction).normalize();
        c.up = (q * c.up).normalize();
    }

    /// Jump the view to look straight down at a geodetic point.
    pub fn set_geodetic(&mut self, lat: f64, lon: f64, alt: f64) {
        let cam = GlobeCamera::from_geodetic(
            lat,
            lon,
            alt.max(self.min_altitude),
            0.0,
            std::f64::consts::FRAC_PI_2,
            self.target.fovy,
        );
        self.target = cam;
        self.camera = cam;
    }

    /// Eases the live camera toward the gesture target — call once per frame
    /// for smooth motion. `k` in (0,1]; 1 = instant.
    pub fn update(&mut self, k: f64) {
        let k = k.clamp(0.0, 1.0);
        let c = &mut self.camera;
        let t = &self.target;
        c.position = c.position.lerp(t.position, k);
        c.direction = c.direction.lerp(t.direction, k).normalize();
        c.up = c.up.lerp(t.up, k).normalize();
        c.fovy = t.fovy;
    }
}

/// Smallest positive ray/ellipsoid intersection (ECEF), via the scaled-space
/// trick: divide by the radii to reduce it to a unit sphere.
pub fn ray_ellipsoid(origin: DVec3, dir: DVec3) -> Option<DVec3> {
    let inv = DVec3::ONE / radii();
    let o = origin * inv;
    let d = dir * inv;
    let a = d.dot(d);
    let b = 2.0 * o.dot(d);
    let c = o.dot(o) - 1.0;
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let sq = disc.sqrt();
    let t0 = (-b - sq) / (2.0 * a);
    let t1 = (-b + sq) / (2.0 * a);
    let t = if t0 > 0.0 {
        t0
    } else if t1 > 0.0 {
        t1
    } else {
        return None;
    };
    Some(origin + dir * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::dvec2;
    use std::f64::consts::FRAC_PI_2;

    fn nadir_over(lat: f64, lon: f64, alt: f64) -> CameraController {
        let cam = GlobeCamera::from_geodetic(lat, lon, alt, 0.0, FRAC_PI_2, 60f64.to_radians());
        CameraController::new(cam).with_min_altitude(100.0)
    }

    #[test]
    fn ray_straight_down_hits_surface_below() {
        let lat = 42.7_f64.to_radians();
        let lon = 2.9_f64.to_radians();
        let ctrl = nadir_over(lat, lon, 100_000.0);
        let hit = ctrl.pick((400.0, 300.0), (800.0, 600.0)).expect("hit");
        let g = ecef_to_geodetic(hit);
        assert!(g.height.abs() < 1.0, "surface height {}", g.height);
        assert!((g.lat - lat).abs() < 1e-3 && (g.lon - lon).abs() < 1e-3);
    }

    #[test]
    fn drag_keeps_the_grabbed_point_under_the_cursor() {
        let ctrl0 = nadir_over(42.7_f64.to_radians(), 2.9_f64.to_radians(), 200_000.0);
        let vp = (800.0, 600.0);
        let start = (400.0, 300.0);
        let end = (520.0, 360.0);
        let mut ctrl = ctrl0.clone();
        let grabbed = ctrl.pick(start, vp).expect("p0");
        ctrl.drag(start, end, vp);
        // After the drag the grabbed surface point sits under the end cursor.
        let now = ctrl.pick(end, vp).expect("hit after drag");
        let d = (now.normalize() - grabbed.normalize()).length();
        assert!(d < 1.0e-3, "grabbed point should track the cursor (drift {d})");
    }

    #[test]
    fn zoom_in_lowers_altitude_and_clamps_at_the_floor() {
        let mut ctrl = nadir_over(0.0, 0.0, 100_000.0);
        let vp = (800.0, 600.0);
        let center = (400.0, 300.0);
        let a0 = ctrl.target.altitude();
        ctrl.zoom(1.0, center, vp);
        assert!(ctrl.target.altitude() < a0, "zoom in lowers altitude");

        // Hammer zoom-in: never dips below the floor.
        for _ in 0..200 {
            ctrl.zoom(1.0, center, vp);
        }
        assert!(
            ctrl.target.altitude() >= ctrl.min_altitude - 1.0,
            "altitude {} stays above floor {}",
            ctrl.target.altitude(),
            ctrl.min_altitude
        );
    }

    #[test]
    fn zoom_out_always_allowed() {
        let mut ctrl = nadir_over(0.0, 0.0, 100_000.0);
        let a0 = ctrl.target.altitude();
        ctrl.zoom(-1.0, (400.0, 300.0), (800.0, 600.0));
        assert!(ctrl.target.altitude() > a0);
    }

    #[test]
    fn view_state_round_trips_into_traversal() {
        let ctrl = nadir_over(45f64.to_radians(), 5f64.to_radians(), 500_000.0);
        let vs = ctrl.target.view_state(dvec2(1024.0, 768.0));
        // The camera looks down: the view position is the eye.
        assert!((vs.position() - ctrl.target.position).length() < 1.0);
    }
}
