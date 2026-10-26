// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the control API's headless viewer asks of the application, and reads
//! from it, beyond what a window's loop does.
//!
//! Small on purpose. Everything the headless viewer *does* goes through the
//! application's own entries — a turn of the loop ([`App::turn`]), a frame
//! ([`App::render`]), a finger, a wheel, a resize, a lifecycle step, a
//! steering command on the queue every other command arrives on — so what is
//! here is only the handful of things a window gets from winit instead: when
//! to draw, where to say things, and the pictures it drew.

use tuile_camera::GlobeCamera;

use super::App;

impl App {
    /// Draws the frame a turn asked for, if it asked for one. Says whether a
    /// frame reached the eyes and was on screen.
    ///
    /// A desktop's window is held shut until the globe has a floor, and is
    /// still drawn into once before that — the first frame asked for, which
    /// is what sends the server its first camera. That frame is drawn here
    /// too, as it is there, and is not one anybody saw.
    pub(crate) fn render_if_asked(&mut self) -> bool {
        let asked = self
            .active
            .as_ref()
            .is_some_and(|active| match &active.screen {
                super::screen::Screen::Offscreen(offscreen) => offscreen.take_redraw(),
                super::screen::Screen::Window(_) => false,
            });
        if !asked {
            return false;
        }
        let shown = !self.warmup.holding;
        let before = self.presented;
        self.render();
        shown && self.presented > before
    }

    /// Where a message for the person goes: a headless run has nobody to
    /// dismiss an alert, and a modal one would hold its thread for ever.
    pub(crate) fn speak_through(&mut self, speaker: fn(&str, &str) -> bool) {
        self.speaker = speaker;
    }

    /// The camera as drawn.
    pub(crate) fn camera(&self) -> GlobeCamera {
        self.controller.camera
    }

    /// Each eye's camera, as the last frame drew it.
    pub(crate) fn eye_cameras(&self) -> Vec<GlobeCamera> {
        self.eyes()
    }

    /// The frame's size in device pixels, and how many of them a point is.
    pub(crate) fn screen_size(&self) -> Option<((u32, u32), f64)> {
        self.active
            .as_ref()
            .map(|active| (active.size, active.scale))
    }

    /// The picture eye `eye` last drew: tightly packed RGBA8, sRGB, and its
    /// size.
    pub(crate) fn eye_pixels(&self, eye: usize) -> Option<(&[u8], (u32, u32))> {
        let active = self.active.as_ref()?;
        let super::screen::Screen::Offscreen(offscreen) = &active.screen else {
            return None;
        };
        let pixels = offscreen.eyes.get(eye)?.pixels.as_deref()?;
        Some((pixels, offscreen.size()))
    }

    /// Frames that reached the screen since the session began.
    pub(crate) fn frames_presented(&self) -> u64 {
        self.presented
    }

    /// Why the session ended, once it has.
    pub(crate) fn ending(&self) -> Option<&'static str> {
        self.ended_by
    }

    /// Flies `tape`, one camera a frame, through the path a replay takes.
    pub(crate) fn fly(&mut self, tape: tuile_tape::Tape) {
        self.recording.tape = Some(tape);
        self.recording.finished = false;
    }

    /// Whether a tape is being flown and has frames left.
    pub(crate) fn flying(&self) -> bool {
        self.recording
            .tape
            .as_ref()
            .is_some_and(|tape| !tape.is_recording() && tape.progress().0 < tape.progress().1)
    }

    /// Ends a flown tape where it left the camera, rather than ending the
    /// session: a replay from the command line winds the session up at its
    /// end, a headless run goes on from where the path stopped.
    pub(crate) fn land(&mut self) {
        let flown = self
            .recording
            .tape
            .as_ref()
            .is_some_and(|tape| !tape.is_recording() && tape.progress().0 >= tape.progress().1);
        if flown {
            self.recording.tape = None;
            // The gesture target joins the camera, or the easing would pull
            // the eye back to where the path began.
            let camera = self.controller.camera;
            crate::steer::jump(&mut self.controller, camera);
        }
    }

    /// Holds a recorded frame in place of the camera and the clocks, from
    /// the next frame drawn on; `None` lets go.
    pub(crate) fn hold(&mut self, replayed: Option<super::Replayed>) {
        self.recording.replay = replayed;
    }

    /// Draws a frame now, whether or not the loop asked for one. Says
    /// whether it reached the eyes.
    pub(crate) fn draw_now(&mut self) -> bool {
        let before = self.presented;
        self.render();
        self.presented > before
    }

    /// What the atmosphere lit the last frame with.
    pub(crate) fn sky(&self) -> Option<tuile_tape::session::SkyState> {
        self.active
            .as_ref()
            .and_then(|active| super::tape::sky_state(&active.renderer))
    }

    /// The instant the last frame was lit for, and the wall it was drawn at.
    pub(crate) fn lit(&self) -> (f64, f64) {
        (self.instant, self.recording.wall)
    }
}
