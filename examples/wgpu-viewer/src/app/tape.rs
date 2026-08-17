// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Driving a camera path: recording one, or flying one that was recorded.
//!
//! It is the *camera* that is recorded and replayed, not the view state, so the
//! render, the traversal and the log line all come from one set of numbers and
//! a replay has nowhere to diverge from what it is reproducing.

use super::App;
use glam::DVec3;

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
}

impl App {
    /// Writes what the recording still holds and closes the file.
    ///
    /// An MCAP file without its footer is one most readers refuse, so this cannot
    /// be left to a `Drop` that has nowhere to report a failure — a truncated
    /// path would look exactly like a short one.
    pub fn close_the_tape(&mut self) {
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
