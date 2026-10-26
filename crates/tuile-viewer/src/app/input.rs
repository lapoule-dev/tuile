// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Turning window events into camera gestures.
//!
//! Everything here is deliberately thin: a gesture becomes a call on the
//! render-agnostic [`tuile_camera::CameraController`], and the on-screen control
//! gets first refusal on every pointer event so that the globe never also acts
//! on one the widget has taken.

use super::setup::configure_surface;
use super::App;
use crate::steer::Command;
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
    /// Whether Command, Control or Option is held: a key pressed with one is
    /// not one of the application's bare keys.
    pub(super) chord: bool,
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
        self.note_the_input(&event);
        match event {
            WindowEvent::CloseRequested => {
                self.ended_by = Some("the window was closed");
                event_loop.exit();
            }
            // Dragging the window to a display of a different density changes
            // how many device pixels a point is worth, and so how deep the
            // imagery should go. Winit sends the new size straight after, so
            // there is nothing to rebuild here.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => self.rescaled(scale_factor),
            // On a phone, a rotation: the drawable turns, and with it the
            // safe area the controls keep to.
            WindowEvent::Resized(new) => self.resized((new.width, new.height)),
            WindowEvent::Touch(touch) => self.on_touch(touch),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    // One table for the keys and the menu: see `crate::menu`.
                    // A key held with Command, Control or Option is a
                    // shortcut of the system's or the menu's, not one of
                    // these.
                    Key::Character(ref c) if !self.pointer.chord => {
                        let mut typed = c.chars();
                        let ask = match (typed.next(), typed.next()) {
                            (Some(only), None) => crate::menu::key(only),
                            _ => None,
                        };
                        if let Some(ask) = ask {
                            let command = ask.command(&self.published());
                            self.apply(command);
                        }
                    }
                    Key::Named(NamedKey::Escape) if self.profile.owns_its_window => {
                        self.ended_by = Some("Esc was pressed");
                        event_loop.exit();
                    }
                    _ => {}
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let held = modifiers.state();
                self.pointer.chord = held.super_key() || held.control_key() || held.alt_key();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                // A hand on the globe ends any glide.
                if pressed {
                    self.flight = None;
                }
                match button {
                    MouseButton::Left if pressed => {
                        // The control gets first refusal; only what it declines
                        // becomes a globe drag.
                        let ui = self.ui_frame();
                        let cursor = ui.to(self.pointer.cursor);
                        self.pointer.dragging = !(self.controls
                            && self.nav.press(cursor, ui.size, &mut self.controller));
                    }
                    MouseButton::Left => {
                        self.pointer.dragging = false;
                        // A ring let go where it was pressed was clicked, not
                        // turned — and a click on a compass means north up.
                        if self.nav.release() == Some(tuile_ui::Part::Ring) {
                            self.apply(Command::NorthUp);
                        }
                    }
                    MouseButton::Right => self.pointer.tilting = pressed,
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let vp = self.viewport();
                let ui = self.ui_frame();
                let prev = self.pointer.cursor;
                let cur = (position.x, position.y);
                if self.controls
                    && self
                        .nav
                        .drag(ui.to(prev), ui.to(cur), ui.size, &mut self.controller)
                {
                    // The control owns this gesture.
                } else if self.pointer.dragging {
                    // Drag the globe: the grabbed point follows the cursor.
                    self.controller.drag(prev, cur, vp);
                } else if self.pointer.tilting {
                    // Right-drag: vertical = tilt, horizontal = heading.
                    self.controller.tilt((cur.1 - prev.1) * 0.005, vp);
                    self.controller.rotate_heading((cur.0 - prev.0) * 0.005, vp);
                }
                self.nav.hover(ui.to(cur), ui.size);
                self.pointer.cursor = cur;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                self.wheel(amount);
            }
            // The display slept, the window was minimized or another window
            // covers it; on a phone, the application left the front; in a
            // browser, the tab was hidden.
            WindowEvent::Occluded(occluded) => self.hidden(occluded),
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    /// The display's density changed: how many device pixels a point is
    /// worth, and so how deep the imagery should go. A window is sent its
    /// new size straight after, so there is nothing to rebuild here.
    pub(crate) fn rescaled(&mut self, scale_factor: f64) {
        if let Some(active) = self.active.as_mut() {
            active.scale = scale_factor.max(1.0);
            tracing::info!(scale = active.scale, "display density changed");
        }
    }

    /// The drawable has a new size, in device pixels — on a phone, a
    /// rotation, which turns the safe area the controls keep to with it.
    pub(crate) fn resized(&mut self, size: (u32, u32)) {
        if let Some(active) = self.active.as_mut() {
            active.size = (size.0.max(1), size.1.max(1));
            active.safe = active.screen.safe_area(active.size);
            // A released surface is configured when it is made again.
            if !active.screen.is_released() {
                configure_surface(active);
                active.targets.resize(&active.gpu, active.size);
            }
            active.screen.request_redraw();
        }
        self.demand.poke();
    }

    /// The wheel turned by `amount` lines, at the pointer: a zoom about the
    /// ground under it. A hand on the globe ends any glide.
    pub(crate) fn wheel(&mut self, amount: f64) {
        self.flight = None;
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

    /// Puts the pointer at `at`, in device pixels, as a mouse moving there
    /// with no button held does.
    #[cfg(feature = "control")]
    pub(crate) fn point_at(&mut self, at: (f64, f64)) {
        self.pointer.cursor = at;
    }
}
