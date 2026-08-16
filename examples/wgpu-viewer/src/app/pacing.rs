// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Watching the camera between frames, so "it feels jerky" becomes a number.
//!
//! Two instruments, and neither costs anything until it has something to say.
//! One measures how evenly the eye is moving; the other shouts when the view
//! turns further in a single frame than any easing could produce.

use super::App;
use glam::DVec3;

/// What the previous frame did, so this one can be compared against it.
///
/// A jerk is not a large movement — a fast gesture moves far every frame and is
/// perfectly smooth. It is one frame moving much further than its neighbours,
/// so every field here exists to make that comparison possible.
pub(super) struct Pacing {
    /// The top of the previous frame, and the only number the easing is allowed
    /// to depend on. See [`tuile_camera::CameraController::advance`].
    pub(super) last_frame: std::time::Instant,
    /// How far the camera moved last frame, and how long that frame took.
    pub(super) last_step: Option<f64>,
    pub(super) last_seconds: f64,
    /// Frames counted when the status line last spoke, so the rate it prints is
    /// the rate over that second rather than over the whole session.
    pub(super) frames_at_last_log: u64,
    /// Heading of the rendered camera last frame, for the sudden-turn watch.
    pub(super) last_heading: Option<f64>,
}

/// The watchdogs: things that are wrong rather than things that are slow.
pub(super) struct Watch {
    /// Whether the previous frame had black ground, so the warning fires on the
    /// transition rather than sixty times a second.
    pub(super) had_holes: bool,
    /// The server's traversal count at the previous line, and how many
    /// consecutive lines it has failed to move while the camera did — which is
    /// how a server that has stopped is told from one with nothing to do.
    pub(super) last_traversals: u64,
    pub(super) last_altitude: Option<f64>,
    pub(super) silent_passes: u32,
    pub(super) last_log: std::time::Instant,
}

impl App {
    /// Watches the *rendered* camera between frames and reports any sudden turn.
    ///
    /// The per-wheel log measures the target, and the target is provably steady:
    /// its heading did not move by a hundredth of a degree over thirteen hundred
    /// wheel events. So whatever turns, turns somewhere else — either in the
    /// eased camera the target feeds, or not in the camera at all. This is the
    /// instrument that tells those apart, and it costs nothing until it fires.
    /// How evenly the eye is actually moving, as a ratio against the frame
    /// before.
    ///
    /// Distance alone says nothing — a fast gesture moves far every frame and is
    /// perfectly smooth. A *jerk* is one frame moving much further than its
    /// neighbours, so the quantity to watch is the ratio, normalised by how much
    /// longer this frame was: a frame that took twice as long is *supposed* to
    /// move twice as far, and counting that as a jerk would report the fix as
    /// the fault.
    ///
    /// One is perfect pacing. Two means a frame moved the eye twice as far as it
    /// should have, and that is what the eye sees.
    pub(super) fn record_the_pacing(&mut self, before: DVec3, dt: std::time::Duration) {
        let step = (self.controller.camera.position - before).length();
        let seconds = dt.as_secs_f64();
        if let Some(last) = self.pacing.last_step {
            // Only while the camera is actually moving: at rest the step is a
            // few microns of easing residue, and the ratio between two of those
            // is noise that would drown every real reading.
            const MOVING_METRES: f64 = 1.0e-3;
            if last > MOVING_METRES && seconds > 0.0 {
                let expected = last * (seconds / self.pacing.last_seconds.max(1.0e-6));
                if expected > MOVING_METRES {
                    tuile_core::metrics::metrics()
                        .worst_camera_step
                        .record(step / expected);
                }
            }
        }
        self.pacing.last_step = Some(step);
        self.pacing.last_seconds = seconds;
    }

    pub(super) fn watch_for_a_sudden_turn(&mut self) {
        /// A frame-to-frame turn no easing should ever produce.
        const SUDDEN_DEGREES: f64 = 2.0;
        let now = self.controller.camera;
        let heading = now.heading().to_degrees();
        if let Some(was) = self.pacing.last_heading {
            let turn = {
                let d = (heading - was).abs();
                d.min(360.0 - d)
            };
            if turn > SUDDEN_DEGREES {
                let g = tuile_core::geo::ecef_to_geodetic(now.position);
                tracing::warn!(
                    "SUDDEN TURN {turn:.1} deg in one frame | heading {was:.2} -> \
                     {heading:.2} | pitch {:.2} | alt {:.0} m | pos {:.5},{:.5}",
                    now.pitch().to_degrees(),
                    g.height,
                    g.lat.to_degrees(),
                    g.lon.to_degrees(),
                );
            }
        }
        self.pacing.last_heading = Some(heading);
    }

}
