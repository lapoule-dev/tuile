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

/// A sensible vertical field of view for looking at a globe: 35°, radians.
///
/// The apps used 60°, which is a ~24 mm lens. On any ordinary subject that is
/// merely wide; on a *sphere* it is wrong in a way people notice without being
/// able to name, because the horizon's curvature across the frame is a function
/// of the angle subtended, not of the altitude. A 60° frame bends the Earth
/// visibly at heights where the real horizon is nearly flat, and the globe reads
/// as a small ball rather than as a planet.
///
/// 35° is around a 60 mm lens — slightly long, which is what aerial and
/// satellite imagery is shot at and therefore what the eye expects of this
/// subject.
///
/// Not free: the imagery detail target goes as `tan(fovy / 2)`, so narrowing the
/// lens asks for **sharper** imagery over a smaller area. Anything that changes
/// this should re-measure with `examples/orbit-probe` rather than assume.
pub const DEFAULT_GLOBE_FOVY: f64 = 35.0 * std::f64::consts::PI / 180.0;

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

    /// The camera's orientation as a rotation.
    ///
    /// Exists so easing can interpolate the basis as one thing. Lerping
    /// `direction` and `up` separately and normalising each does not keep them
    /// perpendicular — the basis skews a little every frame — and the repair for
    /// that skew is what turned the view. Slerping a rotation cannot skew.
    pub fn orientation(&self) -> glam::DQuat {
        let right = self.direction.cross(self.up).normalize_or(DVec3::X);
        let up = right.cross(self.direction).normalize_or(self.up);
        glam::DQuat::from_mat3(&glam::DMat3::from_cols(right, up, -self.direction))
    }

    /// Sets the basis from a rotation, the inverse of [`Self::orientation`].
    pub fn set_orientation(&mut self, q: glam::DQuat) {
        let m = glam::DMat3::from_quat(q);
        self.direction = -m.col(2);
        self.up = m.col(1);
    }

    /// Undoes any twist a move of the eye introduced, restoring a heading the
    /// caller read before moving.
    ///
    /// Moving over a sphere changes which way north is. A gesture that
    /// translates the eye and leaves `direction` and `up` alone therefore *does*
    /// turn the view relative to the ground, however untouched the vectors look.
    /// At the nadir it is at its worst, because there the heading is carried by
    /// `up` alone: twelve zooms toward a corner swung it 46°, which is the globe
    /// spinning under the hand.
    ///
    /// A pure yaw about the local vertical is the whole correction, and the
    /// restraint matters. It leaves the position untouched and the pitch
    /// untouched — rotating about the vertical cannot change the angle to it —
    /// so it cannot feed back into where the next zoom picks. Rebuilding the
    /// basis outright from the heading and pitch also stops the spin, and walked
    /// the eye twenty-one kilometres across France over one zoom out and back,
    /// because each restated direction moved the picked point that the next step
    /// aimed at.
    pub fn hold_heading(&mut self, wanted: f64) {
        use std::f64::consts::{PI, TAU};
        let drift = {
            let d = wrap_angle(self.heading() - wanted);
            if d > PI {
                d - TAU
            } else {
                d
            }
        };
        if drift.abs() < 1e-12 {
            return;
        }
        // Heading runs clockwise from north seen from above, and the vertical
        // points away from the planet, so a positive rotation about it unwinds a
        // positive drift.
        let unwind = DQuat::from_axis_angle(self.up_axis(), drift);
        self.direction = (unwind * self.direction).normalize_or(self.direction);
        self.up = (unwind * self.up).normalize_or(self.up);
    }

    /// Removes any roll about the view direction, measuring against the local
    /// geodetic vertical. Heading and pitch are untouched — only the horizon is
    /// put back level.
    ///
    /// # Why this has to be called rather than maintained
    ///
    /// Two things break the basis, and both are ordinary use rather than bugs
    /// in the gestures themselves:
    ///
    /// - **Moving the eye without rotating the basis.** [`zoom`](Self::zoom)
    ///   translates toward a picked point; the geodetic vertical at the new
    ///   position is a different direction, but `up` still describes the old
    ///   one. Zoom into a corner of the screen and back out, and the horizon
    ///   stays tilted — the reported symptom.
    /// - **Easing.** Interpolating `direction` and `up` separately and
    ///   normalising each does not preserve their perpendicularity, so the
    ///   basis skews a little on every eased frame and never recovers.
    ///
    /// # At the nadir there is nothing to level
    ///
    /// Looking straight down, the view direction is parallel to the vertical and
    /// roll about it is undefined, so the cross product below vanishes and this
    /// leaves the basis alone.
    ///
    /// It once carried a dead zone and a fade around that point, to stop a
    /// near-degenerate cross product from swinging the view. That was the wrong
    /// repair and it was worse than the disease: blending between the current
    /// `up` and the levelled one *is itself a rotation*, and the fade band sat
    /// at three to nine degrees off the nadir — exactly where the pitch drifts
    /// as a zoom walks the eye. Crossing it turned the map. What the guard was
    /// really protecting against was an unpinned heading, and
    /// [`hold_heading`](Self::hold_heading) pins it now, at the nadir included.
    pub fn level(&mut self) {
        let right = self.direction.cross(self.up_axis());
        if right.length_squared() < 1e-12 {
            return;
        }
        self.up = right
            .normalize()
            .cross(self.direction)
            .normalize_or(self.up);
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
        (-self.direction.dot(self.up_axis()))
            .clamp(-1.0, 1.0)
            .asin()
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
        self.clip_planes(clearance);
        self.projection(render_origin, aspect, clearance)
    }

    /// The near and far planes this view would use, in metres.
    ///
    /// Exposed because a clip plane is invisible until it is wrong, and then it
    /// is indistinguishable from missing geometry: ground beyond `far` is not
    /// drawn dark, it is not drawn at all, and the result is a black region that
    /// looks exactly like a tile that never arrived. A reader watching these two
    /// numbers against the altitude can tell the two apart in one glance.
    pub fn clip_planes(&self, clearance: f64) -> (f32, f32) {
        let altitude = self.altitude().max(1.0);
        let horizon = (altitude * (2.0 * WGS84_A + altitude)).sqrt();
        let near = (clearance.max(1.0) * 0.25).max(1.0) as f32;
        let far = (horizon * 1.5 + 10_000.0) as f32;
        (near, far)
    }

    fn projection(&self, render_origin: DVec3, aspect: f32, clearance: f64) -> [f32; 16] {
        let eye = (self.position - render_origin).as_vec3();
        let target = (self.position + self.direction - render_origin).as_vec3();
        let view = Mat4::look_at_rh(eye, target, self.up.as_vec3());
        // Far still comes from the ellipsoid: it is the horizon, which is set
        // by how high the eye is over the globe, not by what is underfoot.
        let (near, far) = self.clip_planes(clearance);
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
            // A metre. Low enough to stand on the ground rather than hover over
            // it; above zero because the clamp is against *sampled* relief, and
            // a floor of zero would let the eye sink through a summit the
            // moment a finer tile raised it.
            min_altitude: 1.0,
            target: camera,
            ground: None,
        }
    }

    /// Clamps against terrain relief instead of the bare ellipsoid.
    ///
    /// The surface is read live, so it sharpens as the globe streams: pass the
    /// same shared handle the terrain loader writes into.
    /// The relief this controller was given, if any.
    ///
    /// Exposed so a renderer can sample the same surface the camera stands on.
    /// Two samplers would be two answers to "how high is the ground here", and
    /// a stand-in surface built from one while the camera flies over the other
    /// is a stand-in that floats or sinks.
    pub fn ground(&self) -> Option<&Arc<dyn GroundHeight>> {
        self.ground.as_ref()
    }

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
        // Read the orientation before the eye moves, so it can be restated after
        // — zooming must not turn the map.
        let (heading, pitch) = (self.target.heading(), self.target.pitch());
        let toward = self
            .pick(cursor, viewport)
            .unwrap_or_else(|| self.target.position.normalize() * WGS84_A);
        let factor = (-delta * 0.15).exp();
        let candidate = toward + (self.target.position - toward) * factor;
        // Always allow zooming out; zooming in stops at the floor rather than
        // being refused, so the gesture still travels as far as it legally can
        // instead of dying the moment the next step would clip.
        // A zoom is meant to change how far away the ground is, and only
        // incidentally which ground. Toward an off-centre cursor it does both,
        // and at altitude the second overwhelms the first: from 40 000 km the
        // picked point is most of a hemisphere away, so one wheel step moved the
        // eye 360 km sideways against 145 km of descent. Repeat that and the
        // camera crosses the Pacific — which is not felt as travel, because the
        // compass bearing swings as the local frame turns underneath, and what
        // it looks like is the map spinning. It was reported as exactly that.
        //
        // So the sideways part is capped against the part that is doing the
        // actual zooming. Near the ground the ratio sits around a fifth and
        // nothing is touched, which is the point: zoom-to-cursor keeps working
        // where it is useful and stops being a catapult where it is not.
        let candidate = self.bound_the_sideways_travel(candidate);
        self.target.position = if factor >= 1.0 {
            candidate
        } else {
            self.lifted(candidate)
        };
        // Zooming toward an off-centre point walks the eye across the globe, so
        // the local vertical at the end is not the one the basis was built
        // against. Carry the orientation along rather than leaving it behind.
        // Zooming toward an off-centre point walks the eye across the globe, so
        // the basis it was built against no longer describes the vertical here.
        // Two separate consequences, and they need separate answers: the view
        // has been twisted relative to the ground, and it has been rolled.
        self.target.hold_heading(heading);
        self.target.level();
        let _ = pitch;
    }

    /// Trims a zoom's sideways travel to the size of its up-and-down travel.
    ///
    /// Splits the step into the part along the local vertical — the zoom proper
    /// — and the part across it, and shortens the second so it never exceeds the
    /// first. A step that is genuinely mostly vertical passes through unchanged.
    fn bound_the_sideways_travel(&self, candidate: DVec3) -> DVec3 {
        /// How far a zoom may carry the eye sideways, as a multiple of how far
        /// it carries it up or down. One, because beyond that a zoom is mostly
        /// a journey, and a journey is what dragging is for.
        const MOST_SIDEWAYS: f64 = 1.0;

        let from = self.target.position;
        let step = candidate - from;
        let vertical = from.normalize_or_zero();
        if vertical == DVec3::ZERO {
            return candidate;
        }
        let climb = step.dot(vertical);
        let sideways = step - vertical * climb;
        let allowed = climb.abs() * MOST_SIDEWAYS;
        let travelled = sideways.length();
        if travelled <= allowed || travelled < 1e-9 {
            return candidate;
        }
        from + vertical * climb + sideways * (allowed / travelled)
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
    /// The camera the gestures act on, before easing.
    ///
    /// `camera` is what reaches the screen; this is where it is heading. A
    /// gesture that misbehaves shows here one frame before it shows there, so
    /// anything instrumenting a gesture wants this one.
    pub fn target(&self) -> &GlobeCamera {
        &self.target
    }

    /// Eases the live camera toward the gesture target by a **duration**, not by
    /// a fixed fraction — call once a frame with the time since the last one.
    ///
    /// [`Self::update`] takes the fraction directly, and that fraction is only
    /// equivalent to a speed while the frame time holds still. It does not hold
    /// still here: frames stretch and snap back as tiles decode and upload, so a
    /// constant fraction moved the eye a different distance every frame for the
    /// same gesture. What that reads as on screen is stutter — and the cause is
    /// not the amount of smoothing but its unevenness, which is why turning the
    /// fraction up or down never helped.
    ///
    /// An exponential ease has one honest parameter, a time, and that time is
    /// what stays fixed while the frame rate moves under it.
    pub fn advance(&mut self, dt: f64) {
        /// How long the eye takes to cover most of the gap to its target.
        ///
        /// The time constant: 63 % of the distance after one, 95 % after three.
        /// Short enough that a drag stays attached to the hand, long enough to
        /// absorb a wheel notch and an uneven frame.
        const SETTLE_SECONDS: f64 = 0.08;
        /// The longest step the ease will take in one call.
        ///
        /// A frame that took a quarter of a second — a burst of uploads, a
        /// shader compile, a window drag — must not be repaid by teleporting the
        /// eye across the gap it missed. Arriving late is better than jumping.
        const LONGEST_STEP: f64 = 1.0 / 30.0;

        let dt = dt.clamp(0.0, LONGEST_STEP);
        self.update(1.0 - (-dt / SETTLE_SECONDS).exp());
    }

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
        // One rotation, slerped — not two axes lerped independently. Lerping
        // them separately does not keep them perpendicular, so the basis skewed
        // a little every frame and needed levelling to repair it; and levelling
        // near the nadir is exactly what turned the view. A rotation cannot
        // skew, so there is nothing to repair.
        c.set_orientation(c.orientation().slerp(t.orientation(), k));
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
        let cam =
            GlobeCamera::from_geodetic(0.7, -0.2, 5_000.0, -0.5, 0.4, std::f64::consts::FRAC_PI_3);
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

    /// Roll about the view direction, measured against the local vertical.
    /// Zero means the horizon is level; this is the quantity the user sees as
    /// "the ground is crooked".
    fn roll(cam: &GlobeCamera) -> f64 {
        let vertical = cam.up_axis();
        // Undefined looking straight down — the caller must not ask there.
        let level_right = cam.direction.cross(vertical).normalize();
        let level_up = level_right.cross(cam.direction).normalize();
        cam.up.dot(level_right).atan2(cam.up.dot(level_up))
    }

    fn oblique_over(lat: f64, lon: f64, alt: f64) -> CameraController {
        // Pitched well off nadir, so roll is defined and measurable.
        let cam = GlobeCamera::from_geodetic(lat, lon, alt, 0.7, 0.6, 60f64.to_radians());
        CameraController::new(cam).with_min_altitude(100.0)
    }

    /// The reported symptom: zoom into an off-centre point, zoom back out, and
    /// the horizon comes back tilted. `zoom` moved the eye without rotating the
    /// basis, so `up` went on describing the vertical somewhere else.
    #[test]
    fn zooming_off_centre_and_back_leaves_the_horizon_level() {
        let mut ctrl = oblique_over(45f64.to_radians(), 6f64.to_radians(), 300_000.0);
        let viewport = (1600.0, 900.0);
        let corner = (1300.0, 250.0);

        for _ in 0..12 {
            ctrl.zoom(1.0, corner, viewport);
        }
        for _ in 0..12 {
            ctrl.zoom(-1.0, corner, viewport);
        }

        let roll = roll(&ctrl.target);
        assert!(
            roll.abs() < 1e-9,
            "horizon rolled by {} rad after zooming in and out",
            roll
        );
    }

    /// **The same elapsed time lands in the same place, however it is cut up.**
    ///
    /// This is what "frame-rate independent" means, and it is the whole reason
    /// [`CameraController::advance`] exists. [`CameraController::update`] takes
    /// the fraction directly, so a second of easing at sixty frames covers far
    /// more of the gap than the same second at thirty — the eye's *speed* then
    /// depends on how busy the machine is, and every hitch in the frame time
    /// becomes a visible hitch in the motion.
    ///
    /// The window is deliberately **shorter** than the 0.08 s time constant. Over
    /// a long one every scheme converges — the eye arrives whatever the rate, and
    /// the test passes while measuring nothing. It was written that way first and
    /// went green with the fix reverted, which is exactly the failure
    /// `CLAUDE.md` warns about. The difference lives in the approach, so the
    /// approach is what has to be sampled.
    #[test]
    fn the_same_easing_time_lands_in_the_same_place_at_any_frame_rate() {
        let start = || {
            let mut c = oblique_over(48f64.to_radians(), 2f64.to_radians(), 200_000.0);
            c.zoom(4.0, (600.0, 600.0), (1600.0, 900.0));
            c
        };

        // A twentieth of a second, cut two ways.
        let mut fast = start();
        for _ in 0..6 {
            fast.advance(1.0 / 120.0);
        }
        let mut slow = start();
        for _ in 0..3 {
            slow.advance(1.0 / 60.0);
        }

        let apart = (fast.camera.position - slow.camera.position).length();
        let travelled = (fast.camera.position - start().camera.position).length();
        assert!(
            travelled > 1.0,
            "the fixture did not move, so it cannot show a difference"
        );
        assert!(
            apart < travelled * 1.0e-3,
            "the same easing time landed {apart:.1} m apart at 120 fps and 60 fps, \
             having travelled {travelled:.0} m — the motion depends on the frame \
             rate, so every uneven frame is a visible jerk"
        );
    }

    /// The eased basis stays square.
    ///
    /// `update` slerps one rotation rather than interpolating `direction` and
    /// `up` separately — separate interpolation does not keep them
    /// perpendicular, so the basis skewed a little every frame and never
    /// recovered. A rotation cannot skew, and this asserts it over enough
    /// frames for any drift to show.
    #[test]
    fn easing_does_not_skew_the_basis() {
        let mut ctrl = oblique_over(48f64.to_radians(), 2f64.to_radians(), 200_000.0);
        let viewport = (1600.0, 900.0);

        ctrl.drag((800.0, 450.0), (1100.0, 300.0), viewport);
        ctrl.zoom(3.0, (600.0, 600.0), viewport);
        for _ in 0..200 {
            ctrl.update(0.2);
        }

        let cam = &ctrl.camera;
        assert!(
            cam.direction.dot(cam.up).abs() < 1e-9,
            "direction and up are not perpendicular: {}",
            cam.direction.dot(cam.up)
        );
        assert!(
            roll(cam).abs() < 1e-9,
            "horizon rolled by {} rad after easing",
            roll(cam)
        );
    }

    /// Looking straight down there is no horizon, so roll is undefined and
    /// levelling must leave the basis alone rather than snap it to an arbitrary
    /// heading. The viewer starts exactly here, so this is the common case.
    #[test]
    fn levelling_at_nadir_preserves_heading() {
        let mut cam =
            GlobeCamera::from_geodetic(0.0, 0.0, 500_000.0, 1.2, FRAC_PI_2, 60f64.to_radians());
        let before = cam.up;
        cam.level();
        assert!(
            (cam.up - before).length() < 1e-12,
            "levelling at nadir moved up from {before:?} to {:?}",
            cam.up
        );
    }

    /// The reported symptom, reproduced: zooming in near the nadir must not
    /// swing the heading.
    ///
    /// Exact nadir was already covered and passed even while the globe was
    /// visibly spinning, because there the cross product is exactly zero and any
    /// guard catches it. The failure lives just *outside* exact — a degree or so
    /// off, where the product is small but nonzero, its direction is decided by
    /// that smallness, and every metre the eye moves rewrites it. A test pinned
    /// to the degenerate point is no test of a degeneracy.
    #[test]
    fn zooming_near_the_nadir_does_not_spin_the_globe() {
        let viewport = (1280.0, 720.0);
        for off_nadir in [0.0, 0.001, 0.01, 0.03] {
            let mut ctrl = CameraController::new(GlobeCamera::from_geodetic(
                46f64.to_radians(),
                6f64.to_radians(),
                2_000_000.0,
                1.2,
                FRAC_PI_2 - off_nadir,
                60f64.to_radians(),
            ));
            let before = ctrl.target.heading();
            // Zoom off-centre and repeatedly, which is what the hand does.
            for _ in 0..12 {
                ctrl.zoom(1.0, (700.0, 300.0), viewport);
            }
            let swing = (ctrl.target.heading() - before).abs();
            let swing = swing.min(std::f64::consts::TAU - swing).to_degrees();
            assert!(
                swing < 1.0,
                "{off_nadir} rad off nadir: twelve zooms swung the heading by {swing}°"
            );
        }
    }

    /// And levelling still has to *work* where a horizon exists — the whole
    /// reason it was added. A dead zone that swallowed the useful range would
    /// pass the test above and fix nothing.
    #[test]
    fn levelling_still_works_once_there_is_a_horizon() {
        let mut ctrl = oblique_over(46f64.to_radians(), 6f64.to_radians(), 200_000.0);
        // Tip the basis about the view direction: a pure roll, nothing else.
        let axis = ctrl.target.direction;
        let tilted = DQuat::from_axis_angle(axis, 0.3) * ctrl.target.up;
        ctrl.target.up = tilted;
        assert!(roll(&ctrl.target).abs() > 0.2, "the fixture is not rolled");

        ctrl.target.level();
        assert!(
            roll(&ctrl.target).abs() < 1e-6,
            "levelling left {} rad of roll at a visible horizon",
            roll(&ctrl.target)
        );
    }

    /// The reported gesture, as reported: sit close above France, dezoom, then
    /// zoom back in, and see whether the camera came back to where it started.
    ///
    /// A round trip is the honest shape for this. Zooming one way and checking
    /// the drift is weaker — it cannot tell a camera that is *wrong* from one
    /// that is merely somewhere else — whereas out-and-back has an answer known
    /// in advance: the same view. It is also what a hand actually does.
    ///
    /// Reported as depending on the zoom level, so it is run from several
    /// altitudes rather than one.
    #[test]
    fn zooming_out_and_back_over_france_returns_the_same_view() {
        let viewport = (1600.0, 900.0);
        // Where the wheel points: dead centre, which is what the viewer sends
        // when the pointer has not moved.
        let cursor = (800.0, 450.0);

        for altitude in [3_000.0, 20_000.0, 200_000.0, 2_000_000.0] {
            for pitch in [FRAC_PI_2, 1.1, 0.7] {
                let start = GlobeCamera::from_geodetic(
                    46.5f64.to_radians(),
                    2.5f64.to_radians(),
                    altitude,
                    0.9,
                    pitch,
                    DEFAULT_GLOBE_FOVY,
                );
                let mut ctrl = CameraController::new(start).with_min_altitude(150.0);
                let (heading0, pitch0) = (ctrl.target.heading(), ctrl.target.pitch());
                let altitude0 = ctrl.target.altitude();

                for _ in 0..15 {
                    ctrl.zoom(-1.0, cursor, viewport);
                }
                for _ in 0..15 {
                    ctrl.zoom(1.0, cursor, viewport);
                }

                let swing = {
                    let d = (ctrl.target.heading() - heading0).abs();
                    d.min(std::f64::consts::TAU - d).to_degrees()
                };
                let tipped = (ctrl.target.pitch() - pitch0).to_degrees().abs();
                let altitude_ratio = ctrl.target.altitude() / altitude0;

                assert!(
                    swing < 0.5,
                    "{altitude} m, pitch {pitch}: out and back swung the heading by {swing}°"
                );
                assert!(
                    tipped < 0.5,
                    "{altitude} m, pitch {pitch}: out and back tipped the pitch by {tipped}°"
                );
                assert!(
                    (0.9..1.1).contains(&altitude_ratio),
                    "{altitude} m, pitch {pitch}: out and back left the altitude at {}× \
                     ({} m)",
                    altitude_ratio,
                    ctrl.target.altitude()
                );
            }
        }
    }

    /// And the same trip must not drift the ground under the screen centre —
    /// heading and pitch can both be right while the camera has quietly walked
    /// across the country.
    #[test]
    fn zooming_out_and_back_stays_over_the_same_ground() {
        let viewport = (1600.0, 900.0);
        let cursor = (800.0, 450.0);
        for altitude in [3_000.0, 200_000.0] {
            let mut ctrl = CameraController::new(GlobeCamera::from_geodetic(
                46.5f64.to_radians(),
                2.5f64.to_radians(),
                altitude,
                0.9,
                1.1,
                DEFAULT_GLOBE_FOVY,
            ))
            .with_min_altitude(150.0);
            let before = ecef_to_geodetic(ctrl.target.position);

            for _ in 0..15 {
                ctrl.zoom(-1.0, cursor, viewport);
            }
            for _ in 0..15 {
                ctrl.zoom(1.0, cursor, viewport);
            }

            let after = ecef_to_geodetic(ctrl.target.position);
            let moved_km = (WGS84_A
                * ((after.lat - before.lat).powi(2)
                    + ((after.lon - before.lon) * before.lat.cos()).powi(2))
                .sqrt())
                / 1000.0;
            assert!(
                moved_km < 5.0,
                "from {altitude} m, out and back walked the eye {moved_km:.1} km"
            );
        }
    }

    /// The gesture as the viewer actually makes it: the wheel zooms toward
    /// **wherever the pointer is**, not the screen centre, and what reaches the
    /// screen is the *eased* camera, not the target.
    ///
    /// Both matter and the earlier round-trip test had neither. Zooming toward
    /// an off-centre point walks the eye sideways as well as down, which is the
    /// path that twists; and easing lerps direction and up independently, so the
    /// rendered basis is not simply the target's.
    #[test]
    fn zooming_at_an_off_centre_pointer_does_not_turn_the_rendered_view() {
        let viewport = (1600.0, 900.0);
        // Where a hand actually leaves the pointer: off to one side, well away
        // from the centre, so the zoom axis is oblique.
        let pointer = (1180.0, 260.0);

        for altitude in [3_000.0, 20_000.0, 200_000.0, 2_000_000.0] {
            for pitch in [FRAC_PI_2, 1.1, 0.7] {
                let mut ctrl = CameraController::new(GlobeCamera::from_geodetic(
                    46.5f64.to_radians(),
                    2.5f64.to_radians(),
                    altitude,
                    0.9,
                    pitch,
                    DEFAULT_GLOBE_FOVY,
                ))
                .with_min_altitude(150.0);
                // Settle the eased camera onto the target before measuring.
                for _ in 0..200 {
                    ctrl.update(0.25);
                }
                let heading0 = ctrl.camera.heading();

                // Fifteen out, fifteen back, easing between every step exactly
                // as a frame loop would.
                for step in 0..30 {
                    ctrl.zoom(if step < 15 { -1.0 } else { 1.0 }, pointer, viewport);
                    for _ in 0..8 {
                        ctrl.update(0.25);
                    }
                }
                for _ in 0..200 {
                    ctrl.update(0.25);
                }

                let swing = {
                    let d = (ctrl.camera.heading() - heading0).abs();
                    d.min(std::f64::consts::TAU - d).to_degrees()
                };
                assert!(
                    swing < 1.0,
                    "{altitude} m, pitch {pitch}: out and back at an off-centre \
                     pointer turned the rendered view by {swing}°"
                );
            }
        }
    }

    /// The rotation the log finally caught, pinned as an invariant: across a
    /// long zoom, the rendered view must not turn **at any point**, not merely
    /// end up where it started.
    ///
    /// Every earlier test here measured the endpoints and passed while the map
    /// visibly turned, because a rotation that goes out and comes back leaves no
    /// trace at the end. Watching the maximum excursion is what catches it.
    ///
    /// The trajectory is the one the instrumented viewer recorded: a few degrees
    /// off the nadir, drifting closer as the eye zooms in. That band is where
    /// levelling used to blend between the current `up` and the levelled one,
    /// and a blend between two `up` vectors *is* a rotation.
    #[test]
    fn a_long_zoom_never_turns_the_view_at_any_point() {
        let viewport = (1600.0, 1200.0);
        let pointer = (1098.0, 811.0);

        // Pitches spanning the old dead zone (0.05) and fade (to 0.15) in sine
        // from the vertical: 87° is 0.052, 85° is 0.087, 81° is 0.156.
        for pitch_deg in [89.0f64, 87.5, 87.0, 86.0, 85.0, 83.0, 81.0, 75.0] {
            let mut ctrl = CameraController::new(GlobeCamera::from_geodetic(
                46.5f64.to_radians(),
                2.5f64.to_radians(),
                2_000_000.0,
                0.0,
                pitch_deg.to_radians(),
                DEFAULT_GLOBE_FOVY,
            ))
            .with_min_altitude(150.0);
            for _ in 0..200 {
                ctrl.update(0.25);
            }
            let reference = ctrl.camera.heading();

            let mut worst: f64 = 0.0;
            let mut worst_at = 0.0;
            for step in 0..40 {
                ctrl.zoom(if step < 20 { 1.0 } else { -1.0 }, pointer, viewport);
                for _ in 0..8 {
                    ctrl.update(0.25);
                    let d = (ctrl.camera.heading() - reference).abs();
                    let d = d.min(std::f64::consts::TAU - d).to_degrees();
                    if d > worst {
                        worst = d;
                        worst_at = ctrl.camera.pitch().to_degrees();
                    }
                }
            }
            assert!(
                worst < 1.0,
                "starting at pitch {pitch_deg}°, the view turned {worst:.1}° \
                 along the way (worst at pitch {worst_at:.1}°)"
            );
        }
    }

    #[test]
    fn view_state_round_trips_into_traversal() {
        let ctrl = nadir_over(45f64.to_radians(), 5f64.to_radians(), 500_000.0);
        let vs = ctrl.target.view_state(dvec2(1024.0, 768.0));
        // The camera looks down: the view position is the eye.
        assert!((vs.position() - ctrl.target.position).length() < 1.0);
    }
}
