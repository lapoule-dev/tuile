// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Turning window events into camera gestures.
//!
//! Everything here is deliberately thin: a gesture becomes a call on the
//! render-agnostic [`tuile_camera::CameraController`], and the on-screen control
//! gets first refusal on every pointer event so that the globe never also acts
//! on one the widget has taken.

use super::setup::configure_surface;
use super::{App, DIAGNOSTICS};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};

/// Where the pointer is and what it is doing.
///
/// The button flags say who owns the current gesture, which matters because
/// the on-screen control gets first refusal on every press: what it declines
/// becomes a globe drag, and what it takes must not also move the globe.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Pointer {
    /// Last seen position, in device pixels. Deltas are taken between two
    /// *events*, not two frames.
    pub(super) cursor: (f64, f64),
    pub(super) dragging: bool,
    pub(super) tilting: bool,
}

/// What the debug keys have switched on.
///
/// Named on every status line rather than only when it changes: a diagnostic
/// view left on is indistinguishable from a broken globe for anyone reading the
/// log later.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Views {
    pub(super) wireframe: bool,
    /// Which diagnostic the shader is drawing. See [`super::DIAGNOSTICS`].
    pub(super) diagnostic: usize,
    /// Whether the traversal is frozen, which is what makes `F` a test of the
    /// renderer against a fixed selection.
    pub(super) freeze: bool,
}

impl App {
    pub(super) fn on_window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        if self.active.is_none() {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // Dragging the window to a display of a different density changes
            // how many device pixels a point is worth, and so how deep the
            // imagery should go. Winit sends the new size straight after, so
            // there is nothing to rebuild here.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(active) = self.active.as_mut() {
                    active.scale = scale_factor.max(1.0);
                    tracing::info!(scale = active.scale, "display density changed");
                }
            }
            WindowEvent::Resized(new) => {
                if let Some(active) = self.active.as_mut() {
                    active.size = (new.width.max(1), new.height.max(1));
                    configure_surface(active);
                    active.targets.resize(&active.gpu, active.size);
                    active.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Character(ref c) if c.eq_ignore_ascii_case("w") => {
                        self.views.wireframe = !self.views.wireframe;
                    }
                    Key::Character(ref c) if c.eq_ignore_ascii_case("d") => {
                        self.views.diagnostic = (self.views.diagnostic + 1) % DIAGNOSTICS.len();
                        let (name, reads) = DIAGNOSTICS[self.views.diagnostic];
                        tracing::info!("view: {name} — {reads}");
                    }
                    Key::Character(ref c) if c.eq_ignore_ascii_case("f") => {
                        self.views.freeze = !self.views.freeze;
                        tracing::info!("traversal freeze: {}", self.views.freeze);
                    }
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    _ => {}
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                match button {
                    MouseButton::Left if pressed => {
                        // The control gets first refusal; only what it declines
                        // becomes a globe drag.
                        let vp = self.viewport();
                        let cursor = self.pointer.cursor;
                        self.pointer.dragging = !self.nav.press(cursor, vp, &mut self.controller);
                    }
                    MouseButton::Left => {
                        self.pointer.dragging = false;
                        self.nav.release();
                    }
                    MouseButton::Right => self.pointer.tilting = pressed,
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let vp = self.viewport();
                let prev = self.pointer.cursor;
                let cur = (position.x, position.y);
                if self.nav.drag(prev, cur, vp, &mut self.controller) {
                    // The control owns this gesture.
                } else if self.pointer.dragging {
                    // Drag the globe: the grabbed point follows the cursor.
                    self.controller.drag(prev, cur, vp);
                } else if self.pointer.tilting {
                    // Right-drag: vertical = tilt, horizontal = heading.
                    self.controller.tilt((cur.1 - prev.1) * 0.005, vp);
                    self.controller.rotate_heading((cur.0 - prev.0) * 0.005, vp);
                }
                self.nav.hover(cur, vp);
                self.pointer.cursor = cur;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                // Instrumented on request: a view that turns under a zoom has
                // survived two rounds of tests that reproduce the gesture and
                // pass, so the gesture being reproduced is evidently not the one
                // being made. Log the input and the camera, per wheel event, and
                // read it rather than reason about it.
                let before = *self.controller.target();
                let picked = self.controller.pick(self.pointer.cursor, self.viewport());
                self.controller
                    .zoom(amount, self.pointer.cursor, self.viewport());
                let after = *self.controller.target();
                let turn = {
                    let d = (after.heading() - before.heading()).abs();
                    d.min(std::f64::consts::TAU - d).to_degrees()
                };
                let at = |c: &tuile_camera::GlobeCamera| {
                    let g = tuile_core::geo::ecef_to_geodetic(c.position);
                    (g.lat.to_degrees(), g.lon.to_degrees(), g.height)
                };
                let (lat0, lon0, alt0) = at(&before);
                let (lat1, lon1, alt1) = at(&after);
                tracing::info!(
                    "WHEEL {amount:+.2} at cursor {:.0},{:.0} of {:?} | picked {} | \
                     alt {alt0:.0} -> {alt1:.0} m | pos {lat0:.5},{lon0:.5} -> \
                     {lat1:.5},{lon1:.5} | heading {:.2} -> {:.2} (TURN {turn:.2} deg) | \
                     pitch {:.2} -> {:.2}",
                    self.pointer.cursor.0,
                    self.pointer.cursor.1,
                    self.viewport(),
                    match picked {
                        Some(p) => {
                            let g = tuile_core::geo::ecef_to_geodetic(p);
                            format!("{:.5},{:.5}", g.lat.to_degrees(), g.lon.to_degrees())
                        }
                        None => "NOTHING — fell back to the point below the eye".to_owned(),
                    },
                    before.heading().to_degrees(),
                    after.heading().to_degrees(),
                    before.pitch().to_degrees(),
                    after.pitch().to_degrees(),
                );
            }
            // The display slept, the window was minimized or another window
            // covers it. Rendering anyway leaks GPU memory on Apple platforms
            // (see `render`), and an occluded surface does not block on vsync,
            // so the loop would free-run and leak all the faster.
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if let Some(active) = self.active.as_ref() {
                    active.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }
}
