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
use std::sync::Arc;
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic, WGS84_A, WGS84_B};
use tuile_core::ground::{lift_above_ground, FlatGround, GroundHeight};
use tuile_core::traversal::ViewState;

/// WGS84 ellipsoid radii (x = y = equatorial, z = polar).
fn radii() -> DVec3 {
    DVec3::new(WGS84_A, WGS84_A, WGS84_B)
}

/// How far below the horizon the flattest view must still aim.
///
/// The floor cannot be a fixed pitch: the horizon sits at a dip angle that
/// grows with altitude — 0.018 rad at 1 km, 0.079 rad at 20 km — so any
/// constant small enough to feel flat near the ground aims at empty sky from
/// altitude. A margin below the dip keeps ground under the screen centre,
/// which every orbiting gesture needs to have something to pivot about.
const PITCH_BELOW_HORIZON: f64 = 0.02;

/// Steepest the view may get: straight down. Past nadir the camera rolls over
/// and comes back up inverted, which no gesture then undoes.
const MAX_PITCH: f64 = std::f64::consts::FRAC_PI_2;

/// Folds an angle into `[0, 2π)`, so a heading is a bearing and never a
/// negative number a compass would have to interpret.
fn wrap_angle(radians: f64) -> f64 {
    use std::f64::consts::TAU;
    radians.rem_euclid(TAU)
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

    /// Compass heading of the view, radians clockwise from north, in `[0, 2π)`.
    ///
    /// Read from the local east-north-up frame under the eye, so it is the
    /// heading a compass held at that point would show — the inverse of what
    /// [`Self::from_geodetic`] takes.
    pub fn heading(&self) -> f64 {
        let (east, north, _) = self.local_frame();
        let flat = self.direction - self.up_axis() * self.direction.dot(self.up_axis());
        if flat.length_squared() < 1e-18 {
            // Looking straight down or up: the heading is whatever the camera's
            // own up says, since the view direction has stopped choosing one.
            let up_flat = self.up - self.up_axis() * self.up.dot(self.up_axis());
            return wrap_angle(up_flat.dot(east).atan2(up_flat.dot(north)));
        }
        wrap_angle(flat.dot(east).atan2(flat.dot(north)))
    }

    /// Pitch of the view below **horizontal**, radians: `0` looks level,
    /// `π/2` straight down. Negative looks up into the sky.
    pub fn pitch(&self) -> f64 {
        (-self.direction.dot(self.up_axis())).clamp(-1.0, 1.0).asin()
    }

    /// How far the view is outside the usable pitch band, in radians; `0` when
    /// it is inside. Aiming above the horizon and diving past nadir are both
    /// violations, and both are measured the same way so a gesture can be told
    /// to reduce whichever it has.
    pub fn pitch_violation(&self) -> f64 {
        let pitch = self.pitch();
        let floor = self.horizon_dip() + PITCH_BELOW_HORIZON;
        if pitch < floor {
            floor - pitch
        } else if pitch > MAX_PITCH {
            pitch - MAX_PITCH
        } else {
            0.0
        }
    }

    /// Angle below horizontal at which the horizon lies, for this altitude.
    ///
    /// Level is not the horizon: from 20 km the ground curves away and the
    /// skyline sits a full 0.08 rad down. A view aimed flatter than this sees
    /// only sky — and nothing for a gesture that pivots about the ground to
    /// hold on to.
    pub fn horizon_dip(&self) -> f64 {
        let altitude = self.altitude().max(0.0);
        (WGS84_A / (WGS84_A + altitude)).clamp(-1.0, 1.0).acos()
    }

    /// The geodetic up at the eye — the local vertical, not the camera's `up`.
    fn up_axis(&self) -> DVec3 {
        self.local_frame().2
    }

    /// East, north, up at the eye's ground point.
    fn local_frame(&self) -> (DVec3, DVec3, DVec3) {
        let f = enu_frame(ecef_to_geodetic(self.position));
        (f.col(0), f.col(1), f.col(2))
    }

    /// The view for the SSE traversal (ECEF f64).
    pub fn view_state(&self, viewport: glam::DVec2) -> ViewState {
        ViewState::perspective(self.position, self.direction, self.up, viewport, self.fovy)
    }

    /// The f32 view-projection for a renderer, rebased onto `render_origin`
    /// (anti-jitter). Near/far are derived from altitude and the horizon
    /// distance so depth precision holds from orbit to street level.
    ///
    /// Measures clearance from the **ellipsoid**, which is only the same thing
    /// as clearance from the scene over water. Where terrain rises, prefer
    /// [`CameraController::view_proj`], which knows how far the ground actually
    /// is.
    pub fn view_proj(&self, render_origin: DVec3, aspect: f32) -> [f32; 16] {
        self.view_proj_with_clearance(render_origin, aspect, self.altitude())
    }

    /// As [`Self::view_proj`], with the distance to the nearest thing worth
    /// drawing given explicitly.
    ///
    /// The near plane is a fraction of `clearance`, so anything closer than
    /// that fraction is clipped away. Deriving it from ellipsoid altitude is
    /// what put a black wedge through mountains: an eye 150 m above a 3000 m
    /// summit is 3150 m above the ellipsoid, and a near plane sized for *that*
    /// slices 800 m into the peak in front of it. Pass the height above the
    /// **ground** and the plane sits where the geometry does.
    pub fn view_proj_with_clearance(
        &self,
        render_origin: DVec3,
        aspect: f32,
        clearance: f64,
    ) -> [f32; 16] {
        let eye = (self.position - render_origin).as_vec3();
        let target = (self.position + self.direction - render_origin).as_vec3();
        let view = Mat4::look_at_rh(eye, target, self.up.as_vec3());
        // Far still comes from the ellipsoid: it is the horizon, which is set
        // by how high the eye is over the globe, not by what is underfoot.
        let altitude = self.altitude().max(1.0);
        let horizon = (altitude * (2.0 * WGS84_A + altitude)).sqrt();
        let near = (clearance.max(1.0) * 0.25).max(1.0) as f32;
        let far = (horizon * 1.5 + 10_000.0) as f32;
        let proj = Mat4::perspective_rh(self.fovy as f32, aspect.max(1e-3), near, far);
        (proj * view).to_cols_array()
    }
}

/// Maps pixel-space gestures onto a [`GlobeCamera`]. Façade-agnostic: feed it
/// cursor pixels + the viewport, read the camera back.
#[derive(Clone)]
pub struct CameraController {
    pub camera: GlobeCamera,
    /// Closest the eye may get to the surface (metres) — never zooms through.
    ///
    /// Measured from the **ground** when a [`GroundHeight`] is set, and from the
    /// ellipsoid otherwise. The difference is the whole point: a floor measured
    /// from the ellipsoid puts the eye kilometres inside anything mountainous.
    pub min_altitude: f64,
    /// Eased toward by [`Self::update`] for smooth motion (optional).
    target: GlobeCamera,
    /// The surface to stay above. `None` clamps against the ellipsoid alone.
    ground: Option<Arc<dyn GroundHeight>>,
}

/// The terrain where it has an opinion, sea level where it does not.
///
/// Always answers, so a camera is clamped even over unstreamed ocean — but the
/// distinction the [`GroundHeight`] contract draws is preserved underneath:
/// only the camera decides that "unknown" means "assume the ellipsoid", and
/// only because refusing to clamp at all would be worse.
struct OrEllipsoid<'a>(Option<&'a dyn GroundHeight>);

impl GroundHeight for OrEllipsoid<'_> {
    fn height_at(&self, lon: f64, lat: f64) -> Option<f64> {
        Some(
            self.0
                .and_then(|ground| ground.height_at(lon, lat))
                .unwrap_or(FlatGround::default().0),
        )
    }
}

impl std::fmt::Debug for CameraController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraController")
            .field("camera", &self.camera)
            .field("min_altitude", &self.min_altitude)
            .field("target", &self.target)
            .field("ground", &self.ground.is_some())
            .finish()
    }
}

impl CameraController {
    pub fn new(camera: GlobeCamera) -> Self {
        Self {
            camera,
            min_altitude: 100.0,
            target: camera,
            ground: None,
        }
    }

    /// Clamps against terrain relief instead of the bare ellipsoid.
    ///
    /// The surface is read live, so it sharpens as the globe streams: pass the
    /// same shared handle the terrain loader writes into.
    pub fn with_ground(mut self, ground: Arc<dyn GroundHeight>) -> Self {
        self.ground = Some(ground);
        self
    }

    /// Raises a position to the floor if it has sunk below it.
    ///
    /// With terrain, the floor is the ground plus [`Self::min_altitude`]; where
    /// terrain has not streamed in, and with no ground at all, it falls back to
    /// the ellipsoid — the old behaviour, and the best available answer when
    /// there is nothing to say about the relief.
    fn lifted(&self, position: DVec3) -> DVec3 {
        lift_above_ground(position, &self.surface(), self.min_altitude)
    }

    /// The surface point the orbiting gestures pivot about: what the screen
    /// centre looks at, or the ground directly below when it looks at sky.
    ///
    /// Without the fallback a camera aimed above the horizon cannot be tilted
    /// or turned at all — the gesture needs a pivot, finds none, and returns.
    /// That is a trap, not a guard: the only way out would be the very gesture
    /// that is refused.
    fn center_of_interest(&self, viewport: (f64, f64)) -> DVec3 {
        self.pick((viewport.0 * 0.5, viewport.1 * 0.5), viewport)
            .unwrap_or_else(|| self.target.position.normalize() * WGS84_A)
    }

    /// The terrain where it is known, the ellipsoid everywhere else.
    fn surface(&self) -> OrEllipsoid<'_> {
        OrEllipsoid(self.ground.as_deref())
    }

    /// How far the eye stands above the ground beneath it, in metres.
    ///
    /// Over water this is the ellipsoid altitude; over land it is what is left
    /// after the relief. The two diverge by kilometres in mountains, which is
    /// exactly where the difference matters.
    pub fn height_above_ground(&self) -> f64 {
        let g = ecef_to_geodetic(self.camera.position);
        let ground = self.surface().height_at(g.lon, g.lat).unwrap_or(0.0);
        g.height - ground
    }

    /// The view-projection for a renderer, with the near plane placed against
    /// the **ground** rather than the ellipsoid.
    ///
    /// Prefer this to [`GlobeCamera::view_proj`] whenever there is terrain: it
    /// is what keeps a near plane sized for a 3000 m altitude from slicing
    /// through the summit the eye is hovering 150 m above.
    pub fn view_proj(&self, render_origin: DVec3, aspect: f32) -> [f32; 16] {
        self.camera
            .view_proj_with_clearance(render_origin, aspect, self.height_above_ground())
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
        // Always allow zooming out; zooming in stops at the floor rather than
        // being refused, so the gesture still travels as far as it legally can
        // instead of dying the moment the next step would clip.
        self.target.position = if factor >= 1.0 {
            candidate
        } else {
            self.lifted(candidate)
        };
    }

    /// Tilt the view (angle from nadir toward the horizon) about the surface
    /// point under the screen centre. `delta` radians, + tilts up.
    pub fn tilt(&mut self, delta: f64, viewport: (f64, f64)) {
        let center = self.center_of_interest(viewport);
        let q = DQuat::from_axis_angle(self.target.right(), delta);
        let mut tilted = self.target;
        tilted.position = center + q * (tilted.position - center);
        tilted.direction = (q * tilted.direction).normalize();
        tilted.up = (q * tilted.up).normalize();

        // Refuse anything outside the usable band. Tilting is unbounded
        // rotation about the look-at point, so far enough in either direction
        // takes the camera under the surface or hangs it upside down — states
        // with no way back, since the gesture that got you there needs a
        // surface point at the screen centre to work at all.
        //
        // Pitch alone does not catch it: the gesture *orbits*, so enough of it
        // carries the eye round to the far side of the planet, where the same
        // orientation reads as a perfectly ordinary pitch against a local
        // vertical that now points the other way. Demanding that up still
        // points away from the planet is what pins the horizon down.
        // Never make it worse — but always allow making it better. Refusing
        // every step that does not already land inside the band would strand a
        // camera that starts outside it: small steps never reach the floor in
        // one go, and the gesture that would recover is the one being refused.
        if tilted.up.dot(tilted.up_axis()) <= 0.0 {
            return;
        }
        let after = tilted.pitch_violation();
        if after > 0.0 && after >= self.target.pitch_violation() {
            return;
        }
        self.target = tilted;
    }

    /// Rotate heading about the surface point under the screen centre (spin the
    /// view around the local vertical). `delta` radians.
    pub fn rotate_heading(&mut self, delta: f64, viewport: (f64, f64)) {
        let center = self.center_of_interest(viewport);
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
        // One clamp for every gesture. Tilting and dragging move the eye
        // laterally, so a camera legally placed a moment ago can end up inside
        // a ridge without any zoom at all; and the ground itself rises under a
        // stationary camera as finer tiles stream in.
        self.target.position = self.lifted(self.target.position);
        let c = &mut self.camera;
        let t = &self.target;
        c.position = c.position.lerp(t.position, k);
        c.direction = c.direction.lerp(t.direction, k).normalize();
        c.up = c.up.lerp(t.up, k).normalize();
        c.fovy = t.fovy;
        // The eased position is between two legal points, but the ground between
        // them may stand higher than either.
        self.camera.position = self.lifted(self.camera.position);
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
        assert!(
            d < 1.0e-3,
            "grabbed point should track the cursor (drift {d})"
        );
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

    /// A mountain range under the camera. Everest is 8848 m; an ellipsoid-based
    /// floor of a few hundred metres puts the eye kilometres inside it.
    struct Mountain(f64);
    impl GroundHeight for Mountain {
        fn height_at(&self, _lon: f64, _lat: f64) -> Option<f64> {
            Some(self.0)
        }
    }

    fn over_mountain(alt: f64, peak: f64) -> CameraController {
        let cam = GlobeCamera::from_geodetic(
            0.48,
            1.5,
            alt,
            0.0,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_3,
        );
        CameraController::new(cam)
            .with_min_altitude(150.0)
            .with_ground(Arc::new(Mountain(peak)))
    }

    /// The bug this exists for: `min_altitude` measured from the ellipsoid let
    /// the eye sit at 200 m over an 8848 m peak — 8.6 km of rock above it.
    #[test]
    fn the_eye_is_pushed_above_the_terrain_not_the_ellipsoid() {
        let mut ctrl = over_mountain(200.0, 8848.0);
        ctrl.update(1.0);
        assert!(
            ctrl.camera.altitude() >= 8848.0 + 150.0 - 1.0,
            "eye at {:.0} m is inside an 8848 m peak",
            ctrl.camera.altitude()
        );
    }

    #[test]
    fn zooming_in_stops_at_the_terrain_floor() {
        let mut ctrl = over_mountain(20_000.0, 4000.0);
        let viewport = (1024.0, 768.0);
        for _ in 0..200 {
            ctrl.zoom(1.0, (512.0, 384.0), viewport);
        }
        ctrl.update(1.0);
        assert!(
            ctrl.camera.altitude() >= 4000.0 + 150.0 - 1.0,
            "zoomed through the mountain to {:.0} m",
            ctrl.camera.altitude()
        );
    }

    /// Terrain streams in under a stationary camera: a position that was legal
    /// at coarse LOD can end up underground once the real peak arrives.
    #[test]
    fn a_camera_already_placed_is_lifted_when_the_ground_rises() {
        let mut ctrl = over_mountain(1_000.0, 3_000.0);
        ctrl.update(1.0);
        assert!(ctrl.camera.altitude() >= 3_000.0 + 150.0 - 1.0);
    }

    /// Without terrain the old contract holds exactly: clamp to the ellipsoid.
    #[test]
    fn without_ground_the_floor_is_the_ellipsoid() {
        let cam = GlobeCamera::from_geodetic(
            0.48,
            1.5,
            10.0,
            0.0,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_3,
        );
        let mut ctrl = CameraController::new(cam).with_min_altitude(150.0);
        ctrl.update(1.0);
        assert!((ctrl.camera.altitude() - 150.0).abs() < 1.0);
    }

    /// Lifting must not slide the view sideways, or the globe would drift under
    /// the cursor whenever the camera was corrected.
    #[test]
    fn lifting_does_not_move_the_ground_track() {
        let mut ctrl = over_mountain(500.0, 5_000.0);
        let before = ecef_to_geodetic(ctrl.camera.position);
        ctrl.update(1.0);
        let after = ecef_to_geodetic(ctrl.camera.position);
        assert!((after.lon - before.lon).abs() < 1e-9);
        assert!((after.lat - before.lat).abs() < 1e-9);
    }

    /// The black wedge: an eye 150 m above a 3000 m summit sits 3150 m above the
    /// ellipsoid, and a near plane sized for that clipped 800 m into the peak in
    /// front of it — the mountain simply vanished.
    #[test]
    fn the_near_plane_follows_the_ground_not_the_ellipsoid() {
        let mut ctrl = over_mountain(200.0, 3_000.0);
        ctrl.update(1.0);

        let clearance = ctrl.height_above_ground();
        assert!(
            (clearance - 150.0).abs() < 2.0,
            "expected 150 m of clearance, got {clearance:.0} m"
        );
        // The near plane must sit inside that clearance, not inside the peak.
        assert!(
            clearance * 0.25 < 150.0,
            "near plane at {:.0} m clips the ground 150 m away",
            clearance * 0.25
        );
    }

    /// Over water the two measures agree, and the old behaviour is unchanged.
    #[test]
    fn over_the_ellipsoid_the_clearance_is_the_altitude() {
        let cam = GlobeCamera::from_geodetic(
            0.48,
            1.5,
            5_000.0,
            0.0,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_3,
        );
        let ctrl = CameraController::new(cam);
        assert!((ctrl.height_above_ground() - ctrl.camera.altitude()).abs() < 1.0);
    }

    /// A compass must read what the camera actually looks along, so the needle
    /// on screen agrees with the view behind it.
    #[test]
    fn heading_and_pitch_round_trip_through_from_geodetic() {
        let cases = [
            (0.0, std::f64::consts::FRAC_PI_2),
            (std::f64::consts::FRAC_PI_2, 0.3),
            (std::f64::consts::PI, 0.8),
            (4.0, 0.1),
        ];
        for (heading, pitch) in cases {
            let cam = GlobeCamera::from_geodetic(
                0.7,
                -0.2,
                5_000.0,
                heading,
                pitch,
                std::f64::consts::FRAC_PI_3,
            );
            assert!(
                (cam.pitch() - pitch).abs() < 1e-6,
                "pitch {pitch} read back as {}",
                cam.pitch()
            );
            // Straight down has no meaningful heading to compare against.
            if (pitch - std::f64::consts::FRAC_PI_2).abs() > 1e-6 {
                assert!(
                    (cam.heading() - heading).abs() < 1e-6,
                    "heading {heading} read back as {}",
                    cam.heading()
                );
            }
        }
    }

    #[test]
    fn heading_is_a_bearing_never_negative() {
        let cam = GlobeCamera::from_geodetic(
            0.7,
            -0.2,
            5_000.0,
            -0.5,
            0.4,
            std::f64::consts::FRAC_PI_3,
        );
        let h = cam.heading();
        assert!((0.0..std::f64::consts::TAU).contains(&h), "heading {h}");
        assert!((h - (std::f64::consts::TAU - 0.5)).abs() < 1e-6);
    }

    fn looking_down(pitch: f64) -> CameraController {
        CameraController::new(GlobeCamera::from_geodetic(
            0.8,
            0.1,
            20_000.0,
            0.0,
            pitch,
            std::f64::consts::FRAC_PI_3,
        ))
    }

    /// Tilting is unbounded rotation about the look-at point: far enough and
    /// the camera hangs upside down, a state no further gesture undoes.
    #[test]
    fn tilting_cannot_turn_the_camera_over() {
        // Just off nadir: looking exactly straight down leaves `up` undefined
        // (direction × up is degenerate), which is a construction artefact and
        // not what this is about.
        let mut ctrl = looking_down(std::f64::consts::FRAC_PI_2 - 0.05);
        let viewport = (1024.0, 768.0);
        // Lean hard toward the horizon, well past it if nothing stopped us.
        for _ in 0..400 {
            ctrl.tilt(-0.02, viewport);
        }
        ctrl.update(1.0);
        let pitch = ctrl.camera.pitch();
        assert!(
            pitch <= MAX_PITCH && pitch >= ctrl.camera.horizon_dip(),
            "pitch {pitch} left the usable band"
        );
        // Up must still point away from the planet, not into it.
        let up_axis = ctrl.camera.up_axis();
        assert!(ctrl.camera.up.dot(up_axis) > 0.0, "the camera is inverted");
    }

    #[test]
    fn tilting_cannot_bury_the_camera_past_nadir() {
        let mut ctrl = looking_down(std::f64::consts::FRAC_PI_2 - 0.05);
        let viewport = (1024.0, 768.0);
        for _ in 0..400 {
            ctrl.tilt(0.02, viewport);
        }
        ctrl.update(1.0);
        let pitch = ctrl.camera.pitch();
        assert!(
            pitch <= MAX_PITCH && pitch >= ctrl.camera.horizon_dip(),
            "pitch {pitch}"
        );
    }

    /// The band must stay usable: a tilt that lands inside it is not refused.
    /// Which screen direction leans which way is the host's business, so this
    /// asserts only that one of them moves.
    #[test]
    fn tilting_within_the_band_still_works() {
        let viewport = (1024.0, 768.0);
        let moved = |delta: f64| {
            let mut ctrl = looking_down(std::f64::consts::FRAC_PI_2 - 0.3);
            let before = ctrl.camera.pitch();
            ctrl.tilt(delta, viewport);
            ctrl.update(1.0);
            (ctrl.camera.pitch() - before).abs() > 1e-3
        };
        assert!(
            moved(0.2) || moved(-0.2),
            "every legal tilt was refused — the band is unusable"
        );
    }

    /// Both directions must be reachable: a guard that only ever refused would
    /// pass the test above by luck of the sign.
    #[test]
    fn both_tilt_directions_are_usable_from_mid_band() {
        let viewport = (1024.0, 768.0);
        let pitch_after = |delta: f64| {
            let mut ctrl = looking_down(std::f64::consts::FRAC_PI_2 - 0.5);
            for _ in 0..5 {
                ctrl.tilt(delta, viewport);
            }
            ctrl.update(1.0);
            ctrl.camera.pitch()
        };
        let up = pitch_after(0.05);
        let down = pitch_after(-0.05);
        assert!(
            (up - down).abs() > 1e-3,
            "tilting both ways gave the same pitch ({up} vs {down})"
        );
    }

    /// The trap this replaced: a camera parked looking at sky could not be
    /// tilted back down, because tilting needs a ground point at the screen
    /// centre and there was none. The only way out was the refused gesture.
    #[test]
    fn a_camera_aimed_at_the_sky_can_still_be_tilted_back_down() {
        let viewport = (1024.0, 768.0);
        // Level from 20 km: the horizon sits ~0.08 rad down, so this looks at
        // empty sky and the centre ray misses the globe entirely.
        let mut ctrl = looking_down(0.0);
        assert!(
            ctrl.pick((512.0, 384.0), viewport).is_none(),
            "this test is pointless unless the centre ray misses"
        );
        let before = ctrl.camera.pitch();
        for _ in 0..40 {
            ctrl.tilt(0.05, viewport);
            ctrl.tilt(-0.05, viewport);
        }
        ctrl.update(1.0);
        assert!(
            (ctrl.camera.pitch() - before).abs() > 1e-3,
            "camera still stuck at pitch {before}"
        );
    }

    /// The floor must follow altitude: the horizon dips further the higher you
    /// are, and a fixed pitch that feels flat near the ground aims at sky from
    /// altitude.
    #[test]
    fn the_horizon_dips_further_with_altitude() {
        let low = looking_down(0.5);
        let high = CameraController::new(GlobeCamera::from_geodetic(
            0.8,
            0.1,
            400_000.0,
            0.0,
            0.5,
            std::f64::consts::FRAC_PI_3,
        ));
        assert!(high.camera.horizon_dip() > low.camera.horizon_dip() * 2.0);
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
