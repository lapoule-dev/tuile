// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Driving a camera path: recording one, or flying one that was recorded.
//!
//! It is the *camera* that is recorded and replayed, not the view state, so the
//! render, the traversal and the log line all come from one set of numbers and
//! a replay has nowhere to diverge from what it is reproducing.
//!
//! A recording is a session's (`tuile_tape::session`): beside each frame's
//! camera, the wall and scene clocks it was drawn at, the viewport, the
//! atmosphere's state and the input that came before it — what a flicker or
//! a star that goes out is correlated with. A replay of one
//! ([`Replayed`]) puts back the camera *and* the two clocks, so the frame is
//! lit as it was, whatever the wall says now.

use super::App;
use glam::DVec3;
use tuile_tape::session::{FrameContext, SkyState};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};

/// One recorded frame, put back: its camera, the wall instant it was drawn
/// at and its scene clock there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Replayed {
    pub(crate) camera: tuile_tape::Frame,
    pub(crate) wall: f64,
    pub(crate) instant: f64,
    pub(crate) rate: f64,
}

/// The two camera recordings a session can have open.
///
/// Separate fields because they are different jobs that happen to share a file
/// format — see the module documentation.
#[derive(Default)]
pub(super) struct Recording {
    pub(super) tape: Option<tuile_tape::Tape>,
    pub(super) trace: Option<tuile_tape::Tape>,
    /// Set when a replay reaches the end of its tape, so the session can wind
    /// up on its own. A reproduction that needs a human to close the window is
    /// one nobody runs twice.
    pub(super) finished: bool,
    /// The input events since the last frame recorded, in a few words each.
    pub(super) inputs: Vec<String>,
    /// The wall instant the frame being drawn reads: the wall's own, or a
    /// replayed frame's.
    pub(super) wall: f64,
    /// A recorded frame held in place of the camera and the clocks.
    pub(super) replay: Option<Replayed>,
    /// `--record` and how its pictures are kept.
    pub(super) settings: crate::recording::RecordSettings,
    /// The pictures of the recording being written.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pictures: Option<super::pictures::Pictures>,
    /// When the recording began: the wall instant and the frames counted.
    pub(super) began: Option<(f64, u64)>,
}

impl App {
    /// Writes what the recording still holds and closes the file.
    ///
    /// An MCAP file without its footer is one most readers refuse, so this cannot
    /// be left to a `Drop` that has nowhere to report a failure — a truncated
    /// path would look exactly like a short one.
    pub fn close_the_tape(&mut self) {
        self.finish_the_pictures();
        // The trace first: it is the artefact someone is waiting to open, and a
        // failure closing the tape must not take it down with it.
        if let Some(trace) = self.recording.trace.take() {
            match trace.finish() {
                Ok(frames) => tracing::info!(frames, "trace written"),
                Err(e) => tracing::error!("the trace was not closed: {e}"),
            }
        }
        let Some(tape) = self.recording.tape.take() else {
            return;
        };
        let recording = tape.is_recording();
        // A replay has nothing to write; saying "written" there would be a
        // claim about the disk that is simply untrue.
        if !recording {
            return;
        }
        match tape.finish() {
            Ok(0) => {}
            Ok(frames) => tracing::info!(frames, "camera path written"),
            Err(e) => tracing::error!("the camera path was not written: {e}"),
        }
    }

    /// Records this frame's camera, or replaces it with the recorded one.
    ///
    /// Driving the *camera* rather than the view state means the render, the
    /// traversal and the log line all come from one set of numbers — there is
    /// nowhere for a replay to diverge from what it is reproducing.
    pub(super) fn run_the_tape(&mut self) {
        if let Some(replayed) = self.recording.replay {
            let camera = &mut self.controller.camera;
            camera.position = DVec3::from_array(replayed.camera.position);
            camera.direction = DVec3::from_array(replayed.camera.direction);
            camera.up = DVec3::from_array(replayed.camera.up);
            camera.fovy = replayed.camera.fovy;
            // Set at the frame's own wall instant, the scene clock reads
            // back exactly the instant recorded: `at(wall)` is
            // `instant + 0 · rate`.
            self.atmosphere.clock.set(replayed.instant, replayed.wall);
            self.atmosphere.clock.set_rate(replayed.rate, replayed.wall);
            return;
        }
        let Some(tape) = self.recording.tape.as_mut() else {
            return;
        };
        let camera = &mut self.controller.camera;
        if tape.is_recording() {
            tape.push(tuile_tape::Frame {
                position: camera.position.to_array(),
                direction: camera.direction.to_array(),
                up: camera.up.to_array(),
                fovy: camera.fovy,
            });
            return;
        }
        match tape.next_frame() {
            Some(frame) => {
                camera.position = DVec3::from_array(frame.position);
                camera.direction = DVec3::from_array(frame.direction);
                camera.up = DVec3::from_array(frame.up);
                camera.fovy = frame.fovy;
            }
            // Winding up on its own is what makes a tape usable in a script: a
            // reproduction that needs someone to close a window is one nobody
            // runs twice.
            None if !self.recording.finished => {
                self.recording.finished = true;
                let (flown, _) = tape.progress();
                tracing::info!(frames = flown, "camera path flown to the end");
            }
            None => {}
        }
    }
}

impl App {
    /// The wall instant this frame is drawn at: a replayed frame's, or the
    /// wall's own. Read once a frame, so the light, the sky and the
    /// recording agree on it.
    pub(super) fn frame_wall(&mut self) -> f64 {
        let wall = self
            .recording
            .replay
            .map_or_else(super::wall_seconds, |r| r.wall);
        self.recording.wall = wall;
        wall
    }

    /// Notes an input event for the recording, if one is being written.
    pub(super) fn note_the_input(&mut self, event: &WindowEvent) {
        if !self
            .recording
            .tape
            .as_ref()
            .is_some_and(tuile_tape::Tape::is_recording)
        {
            return;
        }
        if let Some(said) = described(event) {
            self.recording.inputs.push(said);
        }
    }

    /// Writes what the frame just drawn was drawn under, beside the camera
    /// [`Self::run_the_tape`] pushed for it.
    pub(super) fn record_the_session_frame(&mut self) {
        let Some(tape) = self.recording.tape.as_mut() else {
            return;
        };
        if !tape.is_recording() {
            return;
        }
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let wall = self.recording.wall;
        let instant = self.instant;
        tape.push_context(&FrameContext {
            wall,
            instant,
            rate: self.atmosphere.clock.rate(),
            offset: instant - wall,
            viewport: [active.size.0, active.size.1],
            scale: active.scale,
            render_origin: self.controller.camera.position.to_array(),
        });
        if let Some(sky) = sky_state(&active.renderer) {
            tape.push_sky(&sky);
        }
        tape.push_input(&self.recording.inputs);
        self.recording.inputs.clear();
        // What the GPU has finished, to the encoders; what they have
        // finished, to the tape.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pictures) = self.recording.pictures.as_mut() {
            pictures.pump(&active.gpu, tape, false);
        }
    }
}

/// What the atmosphere lit the last frame with, as the renderer has it:
/// `None` without an atmosphere.
///
/// The exposure itself is not one of the renderer's published numbers: its
/// lights are pre-exposed, so an exposure that steps is read here as a step
/// in the illuminance and the ambient.
pub(crate) fn sky_state(renderer: &tuile_wgpu::TileRenderer) -> Option<SkyState> {
    let u = renderer.sky_uniform()?;
    let (light_updates, view_updates) = renderer.sky_updates();
    let rgb = |v: [f32; 4]| [f64::from(v[0]), f64::from(v[1]), f64::from(v[2])];
    Some(SkyState {
        exposure: None,
        white_balance: rgb(u.grade),
        ambient: rgb(u.ambient),
        harmonic: [rgb(u.sh_x), rgb(u.sh_y), rgb(u.sh_z)],
        illuminance: rgb(u.illuminance),
        to_sun: rgb(u.sun),
        // The Moon's w is its strength: 0 when there is none to draw.
        to_moon: (u.moon[3] > 0.0).then(|| rgb(u.moon)),
        light_updates,
        view_updates,
        stars_drawn: None,
    })
}

/// An input event in a few words, or `None` for one that says nothing
/// about what the person did (a redraw, a focus change).
fn described(event: &WindowEvent) -> Option<String> {
    let pressed = |state: &ElementState| match state {
        ElementState::Pressed => "pressed",
        ElementState::Released => "released",
    };
    Some(match event {
        WindowEvent::KeyboardInput { event, .. } => {
            format!("key {:?} {}", event.logical_key, pressed(&event.state))
        }
        WindowEvent::MouseInput { state, button, .. } => {
            format!("button {button:?} {}", pressed(state))
        }
        WindowEvent::CursorMoved { position, .. } => {
            format!("cursor {:.1} {:.1}", position.x, position.y)
        }
        WindowEvent::MouseWheel { delta, .. } => match delta {
            MouseScrollDelta::LineDelta(x, y) => format!("wheel lines {x} {y}"),
            MouseScrollDelta::PixelDelta(p) => format!("wheel pixels {:.1} {:.1}", p.x, p.y),
        },
        WindowEvent::PinchGesture { delta, .. } => format!("pinch {delta}"),
        WindowEvent::RotationGesture { delta, .. } => format!("rotate {delta}"),
        WindowEvent::Touch(touch) => format!(
            "touch {} {:?} {:.1} {:.1}",
            touch.id, touch.phase, touch.location.x, touch.location.y
        ),
        WindowEvent::Resized(size) => format!("resized {} {}", size.width, size.height),
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => format!("scale {scale_factor}"),
        _ => return None,
    })
}

impl App {
    /// Whether the session is being recorded.
    pub(super) fn is_recording(&self) -> bool {
        self.recording
            .tape
            .as_ref()
            .is_some_and(tuile_tape::Tape::is_recording)
    }

    /// Starts recording the session, or closes the recording: the key `O`
    /// and View > Record Session.
    pub(super) fn record(&mut self, on: bool) {
        if on == self.is_recording() {
            return;
        }
        if !on {
            let frames = self
                .recording
                .tape
                .as_ref()
                .map_or(0, tuile_tape::Tape::frames);
            self.close_the_tape();
            self.say(&format!("recording closed: {frames} frames"));
            return;
        }
        if cfg!(target_arch = "wasm32") {
            self.say("a page has no file to record into");
            return;
        }
        // The file `--record` named, the first time; a new one after.
        let path = self
            .recording
            .settings
            .path
            .take()
            .unwrap_or_else(crate::recording::new_recording_path);
        crate::recording::salvage_any_abandoned_journal(&path.to_string_lossy());
        match tuile_tape::Tape::recording(&path) {
            Ok(tape) => {
                self.recording.tape = Some(tape);
                self.start_the_pictures();
                self.say(&format!("recording to {}", path.display()));
            }
            Err(e) => self.say(&format!("cannot record to {}: {e}", path.display())),
        }
    }

    /// Starts keeping the pictures of the recording just opened, and asks
    /// the surface for frames that can be copied.
    pub(super) fn start_the_pictures(&mut self) {
        if !self.is_recording() {
            return;
        }
        self.recording.began = Some((super::wall_seconds(), self.stats.frames));
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.recording.pictures = Some(super::pictures::Pictures::new(
                self.recording.settings.pictures,
            ));
            if !crate::recording::capturing() {
                crate::recording::set_capturing(true);
                if let Some(active) = self.active.as_mut() {
                    super::setup::configure_surface(active);
                }
            }
        }
    }

    /// Waits for the pictures still in flight, writes them, and says what
    /// recording them cost.
    fn finish_the_pictures(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(mut pictures) = self.recording.pictures.take() {
            if let (Some(active), Some(tape)) = (self.active.as_ref(), self.recording.tape.as_mut())
            {
                pictures.pump(&active.gpu, tape, true);
            }
            let (wall, frames) = self.recording.began.unwrap_or((super::wall_seconds(), 0));
            pictures.report(
                self.stats.frames.saturating_sub(frames),
                super::wall_seconds() - wall,
            );
        }
        self.recording.began = None;
    }

    /// Whether the frame about to be drawn is due a picture, and the index
    /// on the tape it is to carry: the frame whose camera was pushed last.
    pub(super) fn picture_due(&mut self) -> Option<u64> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let index = self.recording.tape.as_ref()?.frames().checked_sub(1)?;
            let wall = self.recording.wall;
            let pictures = self.recording.pictures.as_mut()?;
            pictures.due(wall).then_some(index)
        }
        #[cfg(target_arch = "wasm32")]
        None
    }
}
