// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The window's side of what a phone adds: fingers on the glass, the
//! application leaving and coming back, the loading indicator, and drawing
//! only when there is something to draw.
//!
//! The decisions are made elsewhere, in modules with no window in them —
//! [`super::touch`], [`super::lifecycle`], [`super::loading`],
//! [`super::demand`] — and tested there. What is here carries them out.

use web_time::Instant;

use tuile_core::protocol::{ClientMessage, GeometryStream};
use winit::event::TouchPhase;
use winit::event_loop::ControlFlow;

use super::demand::{Busy, Pace, IDLE_WAKE};
use super::lifecycle::Step;
use super::loading::{Observed, Style};
use super::touch::{Gesture, Phase};
use super::App;
use crate::steer::Command;

/// How far one double tap zooms, in the units the wheel and the zoom buttons
/// use.
const DOUBLE_TAP_ZOOM: f64 = 4.0;
/// What one unit of [`tuile_camera::CameraController::zoom`] scales the
/// distance to the ground by, as a natural logarithm: `exp(-0.15 · delta)`.
const ZOOM_LOG_PER_UNIT: f64 = 0.15;
/// The launch indicator's radius, and the small one's, in control units.
const LAUNCH_RADIUS: f64 = 22.0;
const REFINING_RADIUS: f64 = 9.0;
/// How long one turn of the indicator takes.
const TURN_SECONDS: f64 = 1.0;
/// How often the device's temperature and power saving are read.
const READ_THE_DEVICE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
/// How long a memory signal must stay quiet before the budget gets its
/// targets back: long enough that a system still short of memory says so
/// again first.
const CALM_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

impl App {
    /// One finger event from the window.
    pub(super) fn on_touch(&mut self, touch: winit::event::Touch) {
        let phase = match touch.phase {
            TouchPhase::Started => Phase::Started,
            TouchPhase::Moved => Phase::Moved,
            TouchPhase::Ended => Phase::Ended,
            TouchPhase::Cancelled => Phase::Cancelled,
        };
        self.finger(touch.id, phase, (touch.location.x, touch.location.y));
    }

    /// One finger event, in device pixels: through the recognizer, then
    /// carried out.
    pub(crate) fn finger(&mut self, id: u64, phase: Phase, at: (f64, f64)) {
        for gesture in self.touches.on(Instant::now(), id, phase, at) {
            self.gesture(gesture);
        }
        self.demand.poke();
    }

    /// Carries a gesture out on the camera, the control getting first refusal
    /// on a press, exactly as it does for the mouse. A hand on the globe ends
    /// any glide.
    pub(super) fn gesture(&mut self, gesture: Gesture) {
        self.flight = None;
        let viewport = self.viewport();
        let ui = self.ui_frame();
        match gesture {
            Gesture::Press(at) => {
                self.pointer.cursor = at;
                self.pointer.dragging =
                    !(self.controls && self.nav.press(ui.to(at), ui.size, &mut self.controller));
            }
            Gesture::Drag { from, to } => {
                if self.controls
                    && self
                        .nav
                        .drag(ui.to(from), ui.to(to), ui.size, &mut self.controller)
                {
                    // The control owns this gesture.
                } else if self.pointer.dragging {
                    self.controller.drag(from, to, viewport);
                }
                self.pointer.cursor = to;
            }
            Gesture::Release => {
                self.pointer.dragging = false;
                if self.nav.release() == Some(tuile_ui::Part::Ring) {
                    self.apply(Command::NorthUp);
                }
            }
            Gesture::Pinch { centre, ratio } => {
                // Fingers twice as far apart bring the ground twice as close.
                self.controller
                    .zoom(ratio.ln() / ZOOM_LOG_PER_UNIT, centre, viewport);
            }
            // Clockwise fingers turn the map clockwise, as the compass ring
            // does when dragged clockwise.
            Gesture::Twist { radians } => self.controller.rotate_heading(radians, viewport),
            Gesture::Tilt { radians } => self.controller.tilt(radians / ui.scale, viewport),
            Gesture::DoubleTap(at) => self.controller.zoom(DOUBLE_TAP_ZOOM, at, viewport),
            Gesture::Hold(_) => self.apply(Command::Here),
        }
    }

    /// Carries out what a change of phase asks for. See [`carry`].
    pub(super) fn carry_out(&mut self, steps: Vec<Step>) {
        for step in &steps {
            tracing::info!(?step, phase = ?self.lifecycle.phase(), "lifecycle");
        }
        carry(steps, self);
    }

    /// Decides this frame's loading indicator, from what the last frame saw.
    ///
    /// `None` on a desktop, whose window is held shut until the globe has a
    /// floor and so never needs one.
    pub(super) fn the_loading_indicator(&mut self) -> Option<tuile_ui::Spinner> {
        if self.profile.hold_window_until_warm {
            return None;
        }
        let now = Instant::now();
        let ready = self.session.is_some();
        // Asked only until the launch is over: it drains the server's errors
        // and reports, which the status line does after that.
        let warm = ready && (self.loading.launched() || !self.still_warming());
        let progress = self.active.as_ref().and_then(|active| {
            let level = self.warmup.pinned_level?;
            let priming = active.pump.priming?;
            let expected = priming.expected().max(1);
            let held = active.pump.prepared_through_level(level) as u32;
            Some((held as f32 / expected as f32).min(1.0))
        });
        let resolution = self.resolution.unwrap_or_default();
        let observed = Observed {
            ready,
            warm,
            selected: resolution.exact + resolution.coarser + resolution.lost,
            lost: resolution.lost,
            coarser: resolution.coarser,
            loading: self
                .active
                .as_ref()
                .map_or(0, |a| a.pump.stats.requested as usize),
            progress,
            failed: self.failed.is_some(),
        };
        let was_launched = self.loading.launched();
        let indicator = self.loading.step(now, observed);
        if !was_launched && self.loading.launched() {
            let seconds = self
                .stats
                .started
                .map_or(0.0, |t| t.elapsed().as_secs_f64());
            tracing::info!(seconds, "first view covered; the indicator fades");
            crate::journal::note(&format!("first view covered in {seconds:.1} s"));
        }
        crate::desktop::loading(indicator.opacity > 0.0, indicator.progress);
        if indicator.opacity <= 0.0 {
            return None;
        }
        let ui = self.ui_frame();
        let turn = self
            .stats
            .started
            .map_or(0.0, |t| t.elapsed().as_secs_f64() / TURN_SECONDS);
        Some(match indicator.style {
            Style::Launch => tuile_ui::Spinner {
                centre: ui.centre(),
                radius: LAUNCH_RADIUS * ui.scale,
                turn,
                progress: indicator.progress,
                opacity: indicator.opacity,
            },
            // Top left of the safe area, clear of the compass in the bottom
            // right and of the caption along the bottom.
            Style::Refining => {
                let r = REFINING_RADIUS * ui.scale;
                tuile_ui::Spinner {
                    centre: (ui.origin.0 + 2.0 * r, ui.origin.1 + 2.0 * r),
                    radius: r,
                    turn,
                    progress: None,
                    opacity: indicator.opacity,
                }
            }
        })
    }

    /// Tells the pacing what this frame saw.
    pub(super) fn note_what_needs_frames(&mut self, indicator: Option<tuile_ui::Spinner>) {
        let target = *self.controller.target();
        let camera = self.controller.camera;
        let moving = (camera.position - target.position).length() > 1e-3
            || camera.direction.dot(target.direction) < 1.0 - 1e-12
            || camera.up.dot(target.up) < 1.0 - 1e-12;
        let busy = Busy {
            moving: moving || self.flight.is_some(),
            touching: self.touches.any(),
            indicator: indicator.is_some(),
            uploaded: self.uploaded,
            pending_uploads: self.active.as_ref().map_or(0, |a| a.pump.pending_uploads()),
            sliding: self.active.as_ref().map_or(0, |a| a.pump.sliding()),
        };
        self.demand.saw(Instant::now(), busy);
    }

    /// A phone's turn of the loop: a frame when one is due, and otherwise a
    /// sleep until it is — never a spin.
    pub(super) fn pace_the_frames(&mut self) -> ControlFlow {
        let now = Instant::now();
        self.read_the_pressure(now);
        // In front with no surface: the one made on the way back failed. Made
        // again on every turn until it exists — a screen that stays empty is
        // not an option.
        if self.lifecycle.phase() == super::lifecycle::Phase::Active
            && self.active.as_ref().is_some_and(|a| a.screen.is_released())
        {
            self.restore_the_surface();
        }
        if !self.lifecycle.may_draw() || self.active.is_none() {
            // Away: no frame and no GPU work, but what the server lets go of
            // after a trim is let go of here too — dropping a surface submits
            // nothing — so the memory is back while the system is judging the
            // application, not on its first frame back.
            if let (Some(active), Some(session)) = (self.active.as_mut(), self.session.as_mut()) {
                active.pump.drain(&mut session.stream);
            }
            return if self.session.is_some() {
                ControlFlow::WaitUntil(now + IDLE_WAKE)
            } else {
                ControlFlow::Wait
            };
        }
        self.read_the_device(now);
        if let Some(held) = self.touches.tick(now) {
            self.gesture(held);
        }
        self.play_the_script(now);
        let interval = match self.demand.pace(now) {
            Pace::Continuous => self.profile.frame_interval(self.thermal, self.low_power),
            Pace::Idle => IDLE_WAKE,
        };
        let interval = super::demand::while_waiting(
            interval,
            self.launching_since.map(|since| now.duration_since(since)),
        );
        let due = self.pacing.last_frame + interval;
        if now >= due {
            if let Some(active) = self.active.as_ref() {
                active.screen.request_redraw();
            }
            ControlFlow::Wait
        } else {
            ControlFlow::WaitUntil(due)
        }
    }

    /// Plays what is due of `TUILE_STEER_SCRIPT`, once the window is drawing:
    /// each line a steering URL, as one from outside would arrive, or the
    /// end of the session. See [`super::script::SteerScript`].
    pub(super) fn play_the_steering(&mut self, now: Instant) {
        let covered = self.active.is_some() && !self.warmup.holding;
        let steered = self
            .steer_script
            .as_mut()
            .map_or_else(Vec::new, |s| s.due(now, covered));
        for line in steered {
            if line == "quit" {
                self.ended_by = Some("the steering script ended");
            } else {
                self.queue_url(line);
            }
            self.demand.poke();
        }
        if self
            .steer_script
            .as_ref()
            .is_some_and(super::script::SteerScript::pending)
        {
            self.demand.poke();
        }
    }

    /// Plays what is due of `TUILE_TOUCH_SCRIPT`, through the same path as
    /// the window's touches. See [`super::script`].
    fn play_the_script(&mut self, now: Instant) {
        let covered = self.loading.launched();
        let size = self.viewport();
        let Some(script) = self.script.as_mut() else {
            return;
        };
        for (id, phase, at) in script.due(now, covered, size) {
            for gesture in self.touches.on(now, id, phase, at) {
                self.gesture(gesture);
            }
            self.demand.poke();
        }
    }

    /// Reads whether the system has said it is short of memory since the last
    /// read, at most once a second, and trims when it has — in front or
    /// away. A system that says so through a warning event instead (iOS)
    /// never has anything to read here.
    fn read_the_pressure(&mut self, now: Instant) {
        if self
            .pressure_read
            .is_some_and(|at| now.duration_since(at) < READ_THE_DEVICE_EVERY)
        {
            return;
        }
        self.pressure_read = Some(now);
        if let Some(pressure) = self.pressure.fresh(crate::device::pressure()) {
            tracing::warn!(
                ?pressure,
                "the system is short of memory; the budget shrinks"
            );
            crate::journal::note(&format!("memory pressure: {pressure:?}"));
            self.squeezed_at = Some(now);
            let steps = self.lifecycle.memory_pressure(pressure);
            self.carry_out(steps);
        }
        self.calm_down(now);
    }

    /// A memory signal that has gone quiet for long enough gives the budget
    /// its targets back — in front only; the background keeps its own.
    pub(super) fn calm_down(&mut self, now: Instant) {
        let quiet = self
            .squeezed_at
            .is_some_and(|at| now.duration_since(at) >= CALM_AFTER);
        if quiet {
            let steps = self.lifecycle.calm();
            if !steps.is_empty() {
                tracing::info!("memory pressure over; the budget's targets come back");
                self.squeezed_at = None;
            }
            self.carry_out(steps);
        }
    }

    /// Reads how hot the device is and whether it saves power, at most once a
    /// second, and says so when either changes.
    fn read_the_device(&mut self, now: Instant) {
        if self
            .device_read
            .is_some_and(|at| now.duration_since(at) < READ_THE_DEVICE_EVERY)
        {
            return;
        }
        self.device_read = Some(now);
        let (thermal, low_power) = (crate::device::thermal(), crate::device::low_power());
        if (thermal, low_power) != (self.thermal, self.low_power) {
            tracing::info!(
                ?thermal,
                low_power,
                frame_ms = self
                    .profile
                    .frame_interval(thermal, low_power)
                    .as_secs_f64()
                    * 1000.0,
                "device state"
            );
            if thermal != self.thermal {
                // The budget slows its uploads and its requests, and
                // coarsens its detail, as the device heats.
                self.send(ClientMessage::Thermal(thermal));
            }
            self.thermal = thermal;
            self.low_power = low_power;
        }
    }
}

/// What carrying out a change of phase needs from the window: the session's
/// gate and stream, the surface, a frame. A trait so the wiring of each
/// [`Step`] is tested against a recording stand-in rather than a window.
pub(super) trait Carry {
    fn gate(&self) -> Option<&crate::shell::Gate>;
    /// A message to the server, best effort.
    fn send(&mut self, message: ClientMessage);
    fn release_surface(&mut self);
    fn restore_surface(&mut self);
    /// Write the tile store down and close it, off the window's thread.
    fn suspend_store(&mut self);
    fn resume_store(&mut self);
    fn redraw(&mut self);
}

/// Each step, carried out on `to`.
pub(super) fn carry(steps: Vec<Step>, to: &mut impl Carry) {
    for step in steps {
        match step {
            Step::PauseLoads => {
                if let Some(gate) = to.gate() {
                    gate.shut();
                }
            }
            Step::ResumeLoads => {
                if let Some(gate) = to.gate() {
                    gate.open();
                }
            }
            // The message is a step of its own on the server: the budget's
            // targets shrink, and its evictions are announced at once.
            Step::Pressure(pressure) => {
                tracing::info!(?pressure, "memory pressure handed to the budget");
                to.send(ClientMessage::Pressure(pressure));
            }
            Step::SuspendStore => to.suspend_store(),
            Step::ResumeStore => to.resume_store(),
            Step::ReleaseSurface => to.release_surface(),
            Step::RestoreSurface => to.restore_surface(),
            Step::Redraw => to.redraw(),
        }
    }
}

impl Carry for App {
    fn gate(&self) -> Option<&crate::shell::Gate> {
        self.session.as_ref().map(|s| &s.gate)
    }

    fn send(&mut self, message: ClientMessage) {
        if let Some(session) = self.session.as_mut() {
            // Best effort: a closed stream means the session is over.
            let _ = session.stream.send(message);
        }
    }

    fn release_surface(&mut self) {
        // The last moment the application is sure to run: a phone ends
        // background applications without a word.
        self.remember_the_view();
        self.release_the_surface();
    }

    fn restore_surface(&mut self) {
        self.restore_the_surface();
    }

    fn suspend_store(&mut self) {
        if let Some(session) = self.session.as_ref() {
            session.suspend_store();
        }
    }

    fn resume_store(&mut self) {
        if let Some(session) = self.session.as_ref() {
            session.resume_store();
        }
    }

    fn redraw(&mut self) {
        self.draw_soon();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Gate;

    /// A window that is a log of what it was asked.
    #[derive(Default)]
    struct Recorded {
        gate: Option<Gate>,
        did: Vec<String>,
    }

    impl Carry for Recorded {
        fn gate(&self) -> Option<&Gate> {
            self.gate.as_ref()
        }
        fn send(&mut self, message: ClientMessage) {
            self.did.push(match message {
                ClientMessage::Pressure(p) => format!("pressure {p:?}"),
                other => format!("{other:?}"),
            });
        }
        fn release_surface(&mut self) {
            self.did.push("release".into());
        }
        fn restore_surface(&mut self) {
            self.did.push("restore".into());
        }
        fn suspend_store(&mut self) {
            self.did.push("suspend".into());
        }
        fn resume_store(&mut self) {
            self.did.push("resume".into());
        }
        fn redraw(&mut self) {
            self.did.push("redraw".into());
        }
    }

    /// Each step reaches the thing it names: the gate shuts and opens, a
    /// memory signal goes to the server's budget, the surface goes and comes
    /// back before the frame.
    #[test]
    fn each_step_reaches_what_it_names() {
        let mut to = Recorded {
            gate: Some(Gate::default()),
            ..Recorded::default()
        };
        carry(
            vec![
                Step::PauseLoads,
                Step::Pressure(crate::profile::Pressure::Background),
                Step::SuspendStore,
                Step::ReleaseSurface,
            ],
            &mut to,
        );
        assert!(to.gate.as_ref().is_some_and(Gate::is_shut));
        assert_eq!(to.did, ["pressure Background", "suspend", "release"]);

        to.did.clear();
        carry(
            vec![
                Step::RestoreSurface,
                Step::ResumeStore,
                Step::ResumeLoads,
                Step::Redraw,
            ],
            &mut to,
        );
        assert!(to.gate.as_ref().is_some_and(|g| !g.is_shut()));
        assert_eq!(to.did, ["restore", "resume", "redraw"]);

        // Before the session is up there is no gate, and nothing breaks.
        let mut early = Recorded::default();
        carry(vec![Step::PauseLoads, Step::ResumeLoads], &mut early);
        assert!(early.did.is_empty());
    }
}
