// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The navigation control: a compass ring for heading, an inner handle for
//! tilt, and a pair of buttons for zoom.
//!
//! Everything is derived from the viewport each frame rather than stored, so a
//! resize needs no invalidation and the widget has no state beyond which part a
//! gesture grabbed.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use tuile_camera::CameraController;

use crate::{Mesh, Vertex};

/// Distance from the viewport's bottom-right corner to the control's own
/// corner.
const MARGIN: f64 = 24.0;
/// Outer radius of the compass ring.
const RING_OUTER: f64 = 46.0;
/// Inner radius of the compass ring — the band you grab to rotate.
const RING_INNER: f64 = 34.0;
/// Radius of the tilt handle inside the ring.
const HANDLE: f64 = 26.0;
/// Radius of each zoom button.
const BUTTON: f64 = 15.0;
/// Gap between the ring and the zoom buttons, and between the buttons.
const GAP: f64 = 10.0;
/// Segments per full circle. 48 is smooth at these radii and still trivial.
const SEGMENTS: usize = 48;

/// How far a full sweep around the ring turns the view: one turn of the ring is
/// one turn of the camera, which is the only mapping that feels direct.
const HEADING_PER_RADIAN: f64 = 1.0;
/// Pixels of vertical drag on the handle for the full nadir→horizon sweep.
const TILT_TRAVEL_PX: f64 = 90.0;
/// Zoom applied per button press, in the units [`CameraController::zoom`] takes.
const ZOOM_STEP: f64 = 2.0;

const IDLE: [f32; 4] = [0.10, 0.11, 0.13, 0.62];
const HOT: [f32; 4] = [0.22, 0.24, 0.28, 0.80];
const STROKE: [f32; 4] = [0.82, 0.85, 0.90, 0.85];
const NORTH: [f32; 4] = [0.93, 0.35, 0.30, 0.95];
const HANDLE_FILL: [f32; 4] = [0.16, 0.18, 0.21, 0.78];

/// A part of the control a pixel can land on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The outer band: drag around it to swing the heading.
    Ring,
    /// The inner disc: drag up and down to tip between nadir and the horizon.
    Tilt,
    ZoomIn,
    ZoomOut,
}

/// Where every part sits, for one viewport.
#[derive(Debug, Clone, Copy)]
struct Layout {
    center: (f64, f64),
    zoom_in: (f64, f64),
    zoom_out: (f64, f64),
}

impl Layout {
    fn for_viewport(viewport: (f64, f64)) -> Self {
        // Everything below the compass centre: the ring's lower half, then two
        // buttons each preceded by a gap. Derived rather than guessed, so the
        // stack cannot quietly grow past the bottom margin when a size changes.
        let below_center = RING_OUTER + 2.0 * GAP + 4.0 * BUTTON;
        let cx = viewport.0 - MARGIN - RING_OUTER;
        let cy = viewport.1 - MARGIN - below_center;
        Self {
            center: (cx, cy),
            zoom_in: (cx, cy + RING_OUTER + GAP + BUTTON),
            zoom_out: (cx, cy + RING_OUTER + 2.0 * GAP + 3.0 * BUTTON),
        }
    }

    fn hit(&self, px: (f64, f64)) -> Option<Part> {
        if within(px, self.zoom_in, BUTTON) {
            return Some(Part::ZoomIn);
        }
        if within(px, self.zoom_out, BUTTON) {
            return Some(Part::ZoomOut);
        }
        let d = distance(px, self.center);
        // The handle wins inside its radius; the band owns the rest of the ring.
        if d <= HANDLE {
            return Some(Part::Tilt);
        }
        (d <= RING_OUTER).then_some(Part::Ring)
    }
}

/// The navigation control.
///
/// Feed it presses and cursor moves; it reports whether it took them, so a host
/// can fall through to globe gestures when a click lands on the map instead.
#[derive(Debug, Clone, Default)]
pub struct NavWidget {
    /// The part a press grabbed, held until release. A gesture keeps the part
    /// it started on even if the cursor wanders off it — releasing the ring
    /// because the pointer strayed a few pixels outward is maddening.
    grabbed: Option<Part>,
    /// The part under the cursor when nothing is grabbed, for highlighting.
    hovered: Option<Part>,
}

impl NavWidget {
    pub fn new() -> Self {
        Self::default()
    }

    /// The part at a pixel, if any.
    pub fn hit(&self, px: (f64, f64), viewport: (f64, f64)) -> Option<Part> {
        Layout::for_viewport(viewport).hit(px)
    }

    /// Whether a gesture is in progress on the control.
    pub fn is_active(&self) -> bool {
        self.grabbed.is_some()
    }

    /// Records the cursor for highlighting. Returns whether it is over the
    /// control, so a host can suppress its own hover effects.
    pub fn hover(&mut self, px: (f64, f64), viewport: (f64, f64)) -> bool {
        self.hovered = self.hit(px, viewport);
        self.hovered.is_some()
    }

    /// Handles a press. Returns `true` when the control took it — the host must
    /// then leave the event alone, or the globe will spin under the widget.
    ///
    /// Buttons act on press rather than release: a zoom that only fires when
    /// the mouse comes back up feels broken, and there is nothing to cancel.
    pub fn press(
        &mut self,
        px: (f64, f64),
        viewport: (f64, f64),
        controller: &mut CameraController,
    ) -> bool {
        let Some(part) = self.hit(px, viewport) else {
            return false;
        };
        self.grabbed = Some(part);
        match part {
            Part::ZoomIn => controller.zoom(ZOOM_STEP, centre_of(viewport), viewport),
            Part::ZoomOut => controller.zoom(-ZOOM_STEP, centre_of(viewport), viewport),
            Part::Ring | Part::Tilt => {}
        }
        true
    }

    /// Ends the gesture. Harmless when none is in progress.
    pub fn release(&mut self) {
        self.grabbed = None;
    }

    /// Continues a drag. Returns `true` while the control owns the gesture.
    pub fn drag(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        viewport: (f64, f64),
        controller: &mut CameraController,
    ) -> bool {
        let Some(part) = self.grabbed else {
            return false;
        };
        let layout = Layout::for_viewport(viewport);
        match part {
            Part::Ring => {
                // The angle swept about the centre, not the raw cursor motion:
                // dragging along the band turns the view by exactly as much as
                // the hand travelled around it.
                let swept = angle_at(layout.center, to) - angle_at(layout.center, from);
                controller.rotate_heading(shortest_way(swept) * HEADING_PER_RADIAN, viewport);
            }
            Part::Tilt => {
                let swept = (to.1 - from.1) / TILT_TRAVEL_PX * FRAC_PI_2;
                controller.tilt(swept, viewport);
            }
            // A button has nothing to drag; it already fired on press.
            Part::ZoomIn | Part::ZoomOut => {}
        }
        true
    }

    /// The triangles to draw, for the camera's current orientation.
    pub fn mesh(&self, viewport: (f64, f64), controller: &CameraController) -> Mesh {
        let layout = Layout::for_viewport(viewport);
        let mut mesh = Mesh::new();

        // The band, then the needle riding on it.
        ring(&mut mesh, layout.center, RING_INNER, RING_OUTER, self.tint(Part::Ring));
        needle(&mut mesh, layout.center, controller.camera.heading());

        // The tilt handle, with a bar whose lean shows the current pitch.
        disc(&mut mesh, layout.center, HANDLE, self.tint(Part::Tilt));
        horizon_bar(&mut mesh, layout.center, controller.camera.pitch());

        disc(&mut mesh, layout.zoom_in, BUTTON, self.tint(Part::ZoomIn));
        plus(&mut mesh, layout.zoom_in);
        disc(&mut mesh, layout.zoom_out, BUTTON, self.tint(Part::ZoomOut));
        minus(&mut mesh, layout.zoom_out);

        mesh
    }

    /// A part is lit while grabbed, or while merely hovered.
    fn tint(&self, part: Part) -> [f32; 4] {
        let active = self.grabbed == Some(part) || (self.grabbed.is_none() && self.hovered == Some(part));
        let base = if part == Part::Tilt { HANDLE_FILL } else { IDLE };
        if active {
            HOT
        } else {
            base
        }
    }
}

fn centre_of(viewport: (f64, f64)) -> (f64, f64) {
    (viewport.0 * 0.5, viewport.1 * 0.5)
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn within(px: (f64, f64), centre: (f64, f64), radius: f64) -> bool {
    distance(px, centre) <= radius
}

/// Screen angle of a point about a centre, measured clockwise from straight up
/// — the same sense a compass bearing runs in, so ring and needle agree.
fn angle_at(centre: (f64, f64), px: (f64, f64)) -> f64 {
    let dx = px.0 - centre.0;
    // Screen y grows downward, so up is negative.
    let dy = centre.1 - px.1;
    dx.atan2(dy)
}

/// Folds a swept angle into `(-π, π]`, so crossing the top of the ring turns
/// the short way instead of spinning the view almost all the way round.
fn shortest_way(radians: f64) -> f64 {
    let wrapped = radians.rem_euclid(TAU);
    if wrapped > PI {
        wrapped - TAU
    } else {
        wrapped
    }
}

fn push_tri(mesh: &mut Mesh, a: (f64, f64), b: (f64, f64), c: (f64, f64), color: [f32; 4]) {
    mesh.push(Vertex::new(a.0, a.1, color));
    mesh.push(Vertex::new(b.0, b.1, color));
    mesh.push(Vertex::new(c.0, c.1, color));
}

fn on_circle(centre: (f64, f64), radius: f64, angle: f64) -> (f64, f64) {
    (
        centre.0 + radius * angle.sin(),
        centre.1 - radius * angle.cos(),
    )
}

fn disc(mesh: &mut Mesh, centre: (f64, f64), radius: f64, color: [f32; 4]) {
    for i in 0..SEGMENTS {
        let a = TAU * i as f64 / SEGMENTS as f64;
        let b = TAU * (i + 1) as f64 / SEGMENTS as f64;
        push_tri(
            mesh,
            centre,
            on_circle(centre, radius, a),
            on_circle(centre, radius, b),
            color,
        );
    }
}

fn ring(mesh: &mut Mesh, centre: (f64, f64), inner: f64, outer: f64, color: [f32; 4]) {
    for i in 0..SEGMENTS {
        let a = TAU * i as f64 / SEGMENTS as f64;
        let b = TAU * (i + 1) as f64 / SEGMENTS as f64;
        let (ia, ib) = (on_circle(centre, inner, a), on_circle(centre, inner, b));
        let (oa, ob) = (on_circle(centre, outer, a), on_circle(centre, outer, b));
        push_tri(mesh, ia, oa, ob, color);
        push_tri(mesh, ia, ob, ib, color);
    }
}

/// The north needle: a wedge on the ring, pointing where north currently lies.
///
/// The camera's heading is where the *view* points, so north sits at the
/// opposite bearing on screen.
fn needle(mesh: &mut Mesh, centre: (f64, f64), heading: f64) {
    let north = -heading;
    let tip = on_circle(centre, RING_OUTER - 2.0, north);
    let half = 0.16;
    push_tri(
        mesh,
        tip,
        on_circle(centre, RING_INNER + 1.0, north - half),
        on_circle(centre, RING_INNER + 1.0, north + half),
        NORTH,
    );
}

/// A bar across the tilt handle that leans with the pitch: flat when looking at
/// the horizon, edge-on when looking straight down.
fn horizon_bar(mesh: &mut Mesh, centre: (f64, f64), pitch: f64) {
    let span = HANDLE - 8.0;
    // Looking down flattens the bar toward the centre; looking at the horizon
    // lifts it to full width.
    let lift = pitch.clamp(0.0, FRAC_PI_2) / FRAC_PI_2;
    let y = centre.1 - span * 0.55 * (1.0 - lift);
    let thickness = 1.6;
    let (x0, x1) = (centre.0 - span, centre.0 + span);
    push_tri(
        mesh,
        (x0, y - thickness),
        (x1, y - thickness),
        (x1, y + thickness),
        STROKE,
    );
    push_tri(
        mesh,
        (x0, y - thickness),
        (x1, y + thickness),
        (x0, y + thickness),
        STROKE,
    );
}

fn bar(mesh: &mut Mesh, centre: (f64, f64), half_w: f64, half_h: f64) {
    let (x0, x1) = (centre.0 - half_w, centre.0 + half_w);
    let (y0, y1) = (centre.1 - half_h, centre.1 + half_h);
    push_tri(mesh, (x0, y0), (x1, y0), (x1, y1), STROKE);
    push_tri(mesh, (x0, y0), (x1, y1), (x0, y1), STROKE);
}

fn plus(mesh: &mut Mesh, centre: (f64, f64)) {
    bar(mesh, centre, BUTTON * 0.5, 1.4);
    bar(mesh, centre, 1.4, BUTTON * 0.5);
}

fn minus(mesh: &mut Mesh, centre: (f64, f64)) {
    bar(mesh, centre, BUTTON * 0.5, 1.4);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_camera::GlobeCamera;

    const VIEWPORT: (f64, f64) = (1280.0, 720.0);

    fn layout() -> Layout {
        Layout::for_viewport(VIEWPORT)
    }

    fn controller() -> CameraController {
        CameraController::new(GlobeCamera::from_geodetic(
            0.8,
            0.1,
            50_000.0,
            0.0,
            FRAC_PI_2,
            std::f64::consts::FRAC_PI_3,
        ))
    }

    #[test]
    fn the_control_sits_inside_the_viewport() {
        let l = layout();
        assert!(l.center.0 + RING_OUTER < VIEWPORT.0);
        assert!(l.zoom_out.1 + BUTTON < VIEWPORT.1, "buttons run off-screen");
        assert!(l.center.1 - RING_OUTER > 0.0);
    }

    #[test]
    fn each_part_is_reachable_and_they_do_not_overlap() {
        let l = layout();
        assert_eq!(l.hit(l.center), Some(Part::Tilt));
        assert_eq!(l.hit(l.zoom_in), Some(Part::ZoomIn));
        assert_eq!(l.hit(l.zoom_out), Some(Part::ZoomOut));
        // Between the handle and the outer edge is the band.
        let band = (l.center.0, l.center.1 - (HANDLE + RING_OUTER) * 0.5);
        assert_eq!(l.hit(band), Some(Part::Ring));
    }

    #[test]
    fn a_pixel_on_the_map_hits_nothing() {
        assert_eq!(layout().hit((10.0, 10.0)), None);
        // Just outside the ring, in the gap before the buttons.
        let l = layout();
        assert_eq!(l.hit((l.center.0, l.center.1 - RING_OUTER - 4.0)), None);
    }

    /// A press that misses must be refused, or the widget would swallow clicks
    /// meant for the globe.
    #[test]
    fn a_press_on_the_map_is_not_taken() {
        let mut w = NavWidget::new();
        let mut c = controller();
        assert!(!w.press((10.0, 10.0), VIEWPORT, &mut c));
        assert!(!w.is_active());
    }

    #[test]
    fn zoom_buttons_fire_on_press_and_move_the_camera() {
        let mut w = NavWidget::new();
        let mut c = controller();
        let before = c.camera.altitude();

        assert!(w.press(layout().zoom_in, VIEWPORT, &mut c));
        c.update(1.0);
        assert!(c.camera.altitude() < before, "zoom in should descend");

        w.release();
        let mid = c.camera.altitude();
        assert!(w.press(layout().zoom_out, VIEWPORT, &mut c));
        c.update(1.0);
        assert!(c.camera.altitude() > mid, "zoom out should climb");
    }

    #[test]
    fn dragging_the_ring_turns_the_heading() {
        let mut w = NavWidget::new();
        let mut c = controller();
        let l = layout();
        let start = (l.center.0, l.center.1 - RING_OUTER + 4.0);
        let end = (l.center.0 + RING_OUTER - 4.0, l.center.1);

        assert!(w.press(start, VIEWPORT, &mut c));
        let before = c.camera.heading();
        assert!(w.drag(start, end, VIEWPORT, &mut c));
        c.update(1.0);
        assert!(
            (c.camera.heading() - before).abs() > 1e-3,
            "a quarter turn of the ring left the heading at {before}"
        );
    }

    /// Crossing the top of the ring must not read as an almost-full turn the
    /// other way.
    #[test]
    fn sweeping_past_north_takes_the_short_way() {
        assert!(shortest_way(0.1) > 0.0);
        assert!(shortest_way(-0.1) < 0.0);
        // A hair past a full turn is a hair, not a full turn.
        assert!(shortest_way(TAU - 0.05) < 0.0);
        assert!((shortest_way(TAU - 0.05) + 0.05).abs() < 1e-9);
    }

    /// A gesture keeps the part it started on: letting the pointer stray a few
    /// pixels off the band must not drop the drag.
    #[test]
    fn a_drag_survives_the_cursor_leaving_the_part() {
        let mut w = NavWidget::new();
        let mut c = controller();
        let l = layout();
        let start = (l.center.0, l.center.1 - RING_OUTER + 4.0);
        assert!(w.press(start, VIEWPORT, &mut c));
        // Far away, over the map.
        assert!(w.drag(start, (5.0, 5.0), VIEWPORT, &mut c));
        w.release();
        assert!(!w.drag(start, (6.0, 6.0), VIEWPORT, &mut c));
    }

    #[test]
    fn the_mesh_is_whole_triangles_and_not_empty() {
        let mesh = NavWidget::new().mesh(VIEWPORT, &controller());
        assert!(!mesh.is_empty());
        assert_eq!(mesh.len() % 3, 0, "a stray vertex would drop a triangle");
    }

    /// The needle has to move when the view turns, or it is decoration.
    #[test]
    fn the_needle_follows_the_heading() {
        let w = NavWidget::new();
        let north = controller();
        let mut east = controller();
        east.rotate_heading(FRAC_PI_2, VIEWPORT);
        east.update(1.0);

        let a = w.mesh(VIEWPORT, &north);
        let b = w.mesh(VIEWPORT, &east);
        let needle_of = |m: &Mesh| -> Vec<[f32; 2]> {
            m.iter()
                .filter(|v| v.color == NORTH)
                .map(|v| v.position)
                .collect()
        };
        assert_eq!(needle_of(&a).len(), 3, "the needle is one triangle");
        assert_ne!(needle_of(&a), needle_of(&b));
    }

    #[test]
    fn hovering_lights_a_part_and_the_map_does_not() {
        let mut w = NavWidget::new();
        assert!(w.hover(layout().zoom_in, VIEWPORT));
        assert_eq!(w.hovered, Some(Part::ZoomIn));
        assert!(!w.hover((10.0, 10.0), VIEWPORT));
        assert_eq!(w.hovered, None);
    }

    #[test]
    fn the_control_follows_a_resize() {
        let small = Layout::for_viewport((800.0, 600.0));
        let large = Layout::for_viewport((1920.0, 1080.0));
        assert!(large.center.0 > small.center.0);
        assert!(large.center.1 > small.center.1);
    }
}
