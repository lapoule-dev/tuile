// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Moves of the view that are not gestures: put north back at the top, and
//! put the eye somewhere else entirely.
//!
//! Both are written against the controller's public gestures and
//! constructors. Nothing here reaches into the camera crate, so nothing here
//! can disagree with what a drag or a wheel does to the same state.

use std::f64::consts::{PI, TAU};

use tuile_camera::{CameraController, GlobeCamera};

/// Turns the view north-up about the ground point at the centre of the screen.
///
/// That point, its distance from the eye and the tilt all stay as they are —
/// the picture pivots, it does not travel. It is the same rotation as dragging
/// the compass ring, by exactly the heading there is to lose, the short way
/// round.
///
/// Done in a few passes rather than one. The gesture turns about the vertical
/// *at the centre of the screen*, the heading is read against north *at the
/// eye*, and on a globe those two norths are not parallel: seen from high up
/// and tilted, one turn by the heading leaves a residue of a few degrees (the
/// convergence of the meridians between the two points). Each pass removes the
/// heading that is left, and the residue shrinks geometrically: a handful of
/// passes near the ground, a few dozen from orbit with the horizon in frame.
/// The loop stops when there is nothing left to remove, and is bounded so that
/// a view it cannot converge on — there is none known — costs microseconds,
/// not a hang.
///
/// Acts on the gesture target, like every gesture: the eye eases there over
/// the controller's usual settle time, so the turn is a short swing and not a
/// cut.
pub(crate) fn north_up(controller: &mut CameraController, viewport: (f64, f64)) {
    for _ in 0..64 {
        let heading = controller.target().heading();
        // Signed, in (-π, π]: 350° is ten degrees to undo, not three hundred
        // and fifty.
        let remaining = if heading > PI { heading - TAU } else { heading };
        if remaining.abs() < 1e-13 {
            break;
        }
        // The gesture turns counter-clockwise for a positive angle and
        // headings count clockwise, so turning *by* the heading removes it.
        controller.rotate_heading(remaining, viewport);
    }
}

/// Puts the eye at `camera` outright: no easing, nothing kept of where it was.
///
/// A jump across a country is not a gesture, and easing it would drag the eye
/// through the planet in a straight line. The controller is rebuilt rather
/// than patched because its gesture target is private — and that is right: a
/// target left behind would pull the eye straight back. The floor and the
/// relief it is measured against carry over.
pub(crate) fn jump(controller: &mut CameraController, camera: GlobeCamera) {
    let mut moved = CameraController::new(camera).with_min_altitude(controller.min_altitude);
    if let Some(ground) = controller.ground() {
        moved = moved.with_ground(ground.clone());
    }
    *controller = moved;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::start::StartView;

    const VIEWPORT: (f64, f64) = (1280.0, 800.0);
    const CENTRE: (f64, f64) = (640.0, 400.0);

    fn controller(view: StartView) -> CameraController {
        CameraController::new(view.camera())
    }

    /// What north-up must leave alone, read off the settled camera.
    struct Framing {
        centre: glam::DVec3,
        range: f64,
        pitch: f64,
    }

    fn framing(controller: &mut CameraController) -> Framing {
        controller.update(1.0);
        let centre = controller
            .pick(CENTRE, VIEWPORT)
            .expect("the view is aimed at the ground");
        Framing {
            centre,
            range: (controller.camera.position - centre).length(),
            pitch: controller.camera.pitch(),
        }
    }

    /// From every quarter, low and tilted, high and tilted, and straight down:
    /// afterwards north is up, and the framing has not moved.
    ///
    /// Tolerances. Heading: 1e-9°. Centre and range: a millimetre — they are
    /// exactly preserved by a rotation about an axis through the centre, so
    /// this is rounding. Pitch: half a degree, and that one is not rounding.
    /// The controller's heading gesture turns about the *geocentric* vertical
    /// of the pivot, pitch is read against the *geodetic* vertical at the eye,
    /// and at mid latitudes those differ by a fifth of a degree: a half turn
    /// shows up to twice that as tilt (0.32° measured at 52° north). It is the
    /// same drift as dragging the compass ring round by hand, since this is
    /// that gesture; the bound is here so that it cannot quietly grow.
    #[test]
    fn north_up_zeroes_the_heading_and_keeps_the_centre_the_range_and_the_tilt() {
        for (heading, pitch, altitude) in [
            (135.0, 30.0, 1200.0),
            (200.0, 30.0, 1200.0),
            (-90.0, 55.0, 40_000.0),
            (359.0, 20.0, 300_000.0),
            (77.0, 90.0, 2_000_000.0),
            (180.0, 45.0, 5000.0),
        ] {
            let mut c = controller(StartView {
                lon: -2.86,
                lat: 52.51,
                altitude,
                heading,
                pitch,
            });
            let before = framing(&mut c);
            north_up(&mut c, VIEWPORT);
            let after = framing(&mut c);

            let case = format!("heading {heading}, pitch {pitch}, altitude {altitude}");
            let heading_left = (c.camera.heading().to_degrees() + 180.0).rem_euclid(360.0) - 180.0;
            assert!(heading_left.abs() < 1e-9, "{case}: {heading_left}° left");
            assert!(
                (after.centre - before.centre).length() < 1e-3,
                "{case}: the centre moved {} m",
                (after.centre - before.centre).length()
            );
            assert!(
                (after.range - before.range).abs() < 1e-3,
                "{case}: the range changed by {} m",
                after.range - before.range
            );
            assert!(
                (after.pitch - before.pitch).to_degrees().abs() < 0.5,
                "{case}: the pitch changed by {}°",
                (after.pitch - before.pitch).to_degrees()
            );
        }
    }

    #[test]
    fn north_up_on_a_view_already_north_up_changes_nothing() {
        let mut c = controller(StartView::default());
        let before = *c.target();
        north_up(&mut c, VIEWPORT);
        assert_eq!(c.target().position, before.position);
        assert_eq!(c.target().direction, before.direction);
        assert_eq!(c.target().up, before.up);
    }

    /// A jump leaves nothing behind to ease back to, and keeps the floor.
    #[test]
    fn a_jump_moves_the_eye_and_its_target_together_and_keeps_the_floor() {
        let mut c = controller(StartView::default()).with_min_altitude(7.0);
        c.zoom(3.0, CENTRE, VIEWPORT);
        let there = StartView {
            lon: 10.0,
            lat: -20.0,
            altitude: 9000.0,
            heading: 40.0,
            pitch: 60.0,
        }
        .camera();
        jump(&mut c, there);
        assert_eq!(c.camera.position, there.position);
        assert_eq!(c.target().position, there.position);
        assert_eq!(c.target().direction, there.direction);
        assert_eq!(c.min_altitude, 7.0);
        // And it stays there: easing has nowhere else to go.
        c.update(1.0);
        assert!((c.camera.position - there.position).length() < 1e-6);
    }
}
