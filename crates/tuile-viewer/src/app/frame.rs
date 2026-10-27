// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One frame: ease, stream, resolve, draw, present.
//!
//! The order in here is not arrangement, it is meaning. The camera is eased by
//! the measured frame duration before anything reads it; the stream is drained
//! and this frame's share of uploads taken before the selection is resolved,
//! because a tile that arrived this frame must be drawable this frame; and the
//! backdrop is drawn before the selection so that ground is never bare.

use super::{App, AMBIENT, ATMOSPHERE_STRENGTH, DIAGNOSTICS};
use std::fmt;
use tuile_wgpu::OverlayVertex;

impl App {
    /// Renders one frame — unless the window is hidden.
    ///
    /// The order of the calls below is not arrangement, it is meaning, and it is
    /// the reason this reads as a list rather than as prose. The camera is eased
    /// by the measured frame duration before anything reads it; this frame's
    /// share of uploads is taken before the selection is resolved, because a
    /// tile that arrived now must be drawable now; and the backdrop is drawn
    /// before the selection, so that ground is never bare.
    ///
    /// Apple's Metal driver leaks memory in proportion to the number of render
    /// passes created (gfx-rs/wgpu#8768), and nothing on the wgpu side reclaims
    /// it. A hidden window still accepts frames, and its surface does not block
    /// on vsync, so drawing into one burns render passes at CPU speed for no
    /// pixels: minutes of that exhausts the driver and takes the machine with
    /// it. Skipping the frame entirely is the workaround the ecosystem settled
    /// on (the wgpu examples carry the same guard).
    pub(super) fn render(&mut self) {
        // Covered on a desktop, or not in front on a phone — where the system
        // ends an application that uses the GPU from the background.
        if self.occluded || !self.lifecycle.may_draw() {
            return;
        }
        let dt = self.start_the_frame();
        self.ease_the_camera(dt);
        if self.active.is_none() {
            return;
        }
        self.take_this_frames_share_of_the_stream();
        self.upload_the_view();
        // After the eye has moved for this frame — the host's drive, the
        // ease, a glide, a tape — so the packing origin is where it is drawn.
        self.upload_the_world_overlays();
        let indicator = self.the_loading_indicator();
        self.upload_the_control(indicator);
        let drew = self.draw_and_present();
        self.note_what_needs_frames(indicator);
        let Some((rendered, resolution, old_imagery)) = drew else {
            return;
        };
        #[cfg(feature = "control")]
        {
            self.presented += 1;
        }
        self.read_the_eyes();
        self.resolution = Some(resolution);
        self.note_the_turn_over(old_imagery);
        // After the draw, and deliberately: neither of these is drawing, and
        // both want the whole of `self` while the drawn tiles still borrow the
        // pump.
        self.note_what_could_not_be_drawn(resolution);
        self.record_the_traced_frame();
        self.publish_the_view(Some((
            rendered,
            resolution.coarser == 0 && resolution.lost == 0,
        )));
        self.report_once_a_second(rendered, resolution);
    }

    /// Closes the previous frame's clock and opens this one's.
    ///
    /// Returns the interval, which is the only number the easing is allowed to
    /// depend on.
    fn start_the_frame(&mut self) -> std::time::Duration {
        let dt = self.pacing.last_frame.elapsed();
        self.pacing.last_frame = web_time::Instant::now();
        let m = tuile_core::metrics::metrics();
        m.frames.inc();
        m.frame_seconds.record(dt);
        m.worst_frame_millis.record(dt.as_secs_f64() * 1000.0);
        dt
    }

    /// Gives an embedding application the frame before it is prepared, and
    /// carries out what it asked for.
    ///
    /// The host is taken out of `self` for the call and put back after: it is
    /// handed the camera and the preload list, which live in `self` too, and
    /// one of them has to let go. Taking the host is the cheap end — a box
    /// moved twice — and it also means a host that panics does not leave a
    /// half-borrowed application behind.
    fn ask_the_host(&mut self, dt: std::time::Duration) {
        let Some(mut host) = self.frame_host.take() else {
            return;
        };
        let (w, h) = self.viewport();
        let mut camera =
            crate::frame_host::HostCamera::new(self.controller.camera, self.fixed_aspect)
                .with_altitude_range(self.controller.min_altitude, self.controller.max_altitude);
        let mut preload = std::mem::take(&mut self.preload);
        preload.clear();
        // Overlays are the other way round: taken out and put back as they
        // are, because a layer the host does not touch this frame keeps what
        // it had.
        let mut overlays = std::mem::take(&mut self.overlays);
        {
            let mut frame = crate::frame_host::HostFrame {
                camera: &mut camera,
                preload: &mut preload,
                overlays: &mut overlays,
                atmosphere: &mut self.atmosphere,
                wall: super::wall_seconds(),
                viewport: (w as u32, h as u32),
                // The viewer's own relief, the one the controller keeps its
                // floor against: a host placing anything on the ground
                // measures the same terrain the viewer is drawing, not a
                // second idea of it.
                ground: self.controller.ground().map(|g| g.as_ref()),
            };
            host.before_frame(dt.as_secs_f64(), &mut frame);
        }
        // The bounds first: a place this frame is held to the new ones.
        let (min, max) = camera.taken_altitudes();
        if let Some(min) = min {
            self.controller.min_altitude = min;
        }
        if let Some(max) = max {
            self.controller.set_max_altitude(max);
        }
        match camera.taken() {
            Some(crate::frame_host::Asked::Drive(pose)) => self.controller.drive(pose),
            // Placing is a jump with the floor kept, and it ends any glide:
            // two things moving the same eye is the one state nobody can
            // reason about.
            Some(crate::frame_host::Asked::Place(pose)) => {
                self.flight = None;
                self.controller.jump_to(pose);
            }
            Some(crate::frame_host::Asked::Release) => self.controller.release(),
            None => {}
        }
        // A shape the host set survives the frame; one it said nothing about
        // is whatever it was.
        if let (aspect, true) = camera.taken_aspect() {
            self.fixed_aspect = aspect;
        }
        self.preload = preload;
        self.overlays = overlays;
        self.frame_host = Some(host);
    }

    /// Hands this frame's overlay layers to the renderer, packed against the
    /// eye.
    ///
    /// Only when a host changed something: a layer set once and left alone is
    /// drawn every frame from the buffers it is already in, which is the
    /// whole point of layers. The view is built against the origin the set
    /// was packed at, so it stays where it was put while the eye moves; once
    /// the eye is [`super::OVERLAY_REBASE_METRES`] from that origin, the set
    /// is packed again against the eye, to keep its f32 offsets short.
    ///
    /// A faulty mesh is reported and the **previous** set stays on screen.
    /// The alternative is a frame with no overlays in it, which for a host
    /// whose HUD is an overlay is a frame with no HUD.
    fn upload_the_world_overlays(&mut self) {
        let eye = self.controller.camera.position;
        // Before the layers are taken: a change made before the session is up
        // waits for it rather than being marked done and lost.
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let world = &mut active.world_overlay;
        if self.overlays.changed() {
            let meshes = self.overlays.taken();
            if let Err(e) = world.set_meshes(&active.gpu, &meshes, eye, None) {
                tracing::warn!("the host's overlays were refused, keeping the last set: {e}");
            }
        } else if !world.is_empty() && super::overlays_need_rebase(eye, world.origin()) {
            world.rebase(&active.gpu, eye);
        }
    }

    /// Moves the eye toward where the gestures have put it, and watches how it
    /// went.
    fn ease_the_camera(&mut self, dt: std::time::Duration) {
        let before = self.controller.camera.position;
        // Before the ease: a host that drives sets the pose this frame is
        // drawn and traversed from, and the ease then finds nothing to do.
        self.ask_the_host(dt);
        self.controller.advance(dt.as_secs_f64());
        // A glide puts the eye where it is due, frame by frame.
        if let Some(flight) = self.flight {
            let (view, over) = flight.at(web_time::Instant::now());
            crate::steer::jump(&mut self.controller, view.camera());
            if over {
                self.flight = None;
            }
        }
        self.record_the_pacing(before, dt);
        // A tape, if one is running, overrides the camera the gestures produced
        // — so the render, the traversal and the log line all come from the
        // same recorded numbers, with nothing to drift between them.
        self.run_the_tape();
        self.watch_for_a_sudden_turn();
    }

    /// Sends the camera, takes whatever the server has produced up to this
    /// frame's upload budget, and counts what that cost.
    fn take_this_frames_share_of_the_stream(&mut self) {
        let cam = self.controller.camera;
        // One view per eye: the traversal selects the union, at the detail the
        // most demanding eye needs, and every eye is drawn from it.
        let views = self.eye_views();
        self.uploaded = 0;
        // Anti-jitter: render origin = eye, so the f32 the GPU sees is small.
        let origin = cam.position;
        let Some(active) = self.active.as_mut() else {
            return;
        };
        self.stats.frames += 1;
        // The shell too, and forgetting it is why it was invisible: the pump
        // rebases the tiles it owns, the shell is not one of them, and a mesh
        // still anchored to the Earth's centre while the view is relative to
        // the eye is drawn six thousand kilometres from where it belongs. And
        // before the session is up: on a phone the shell is the whole first
        // frame.
        active.shell.rebase(&active.gpu.queue, origin);
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let before = active.pump.prepared_count();
        // This frame's upload deadline, from the display's refresh (I18):
        // only the render side knows its frame.
        let deadline = active.pump.deadline(self.refresh_hz);
        // The frame's streaming step, shared with the headless tests — send the
        // camera, take this frame's share of uploads, rebase. Freezing simply
        // withholds the camera, which is what makes `F` a test of the renderer
        // against a fixed selection.
        let uploaded = if self.views.freeze {
            active.pump.pump(&mut session.stream, &active.gpu, deadline)
        } else {
            active
                .pump
                .advance(&mut session.stream, &active.gpu, &views, origin, deadline)
        };
        self.uploaded = uploaded;
        self.stats.uploads += uploaded as u64;
        // A drop in the prepared count is the server reclaiming: worth counting
        // separately from uploads, since a healthy session does far more of the
        // former than the latter once the view settles.
        self.stats.evictions +=
            (before + uploaded).saturating_sub(active.pump.prepared_count()) as u64;
        if self.views.freeze {
            active.pump.rebase(&active.gpu.queue, origin);
        }
    }

    /// Uploads the view matrix, the sun and the air — the first eye's, which
    /// is a window's only one.
    fn upload_the_view(&mut self) {
        // The frame's instant, from the scene's clock — after the host, which
        // may have set it.
        let wall = super::wall_seconds();
        self.instant = self.atmosphere.instant(wall);
        let sky = self.sky_view();
        let mut atmosphere = self.atmosphere;
        // The atmosphere checkbox, over what the host asked.
        atmosphere.enabled &= self.nav.atmosphere_enabled();
        // The flat light's sun, for the path without an atmosphere, is the
        // atmosphere's own.
        self.sun = tuile_atmosphere::Sun::from_direction(
            atmosphere.lights.at(self.instant, sky.eye).to_sun,
        );
        let views = self.view_uniforms();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if let Some(view) = views.first() {
            active.renderer.set_view(&active.gpu.queue, view);
        }
        active
            .renderer
            .draw_atmosphere(&active.gpu, &atmosphere, &sky, wall);
    }

    /// What the camera sees, for the sky: the first eye.
    fn sky_view(&self) -> tuile_wgpu::SkyView {
        let cam = self.controller.camera;
        let (_, h) = self.viewport();
        let eye = cam.position;
        let height = tuile_core::geo::ecef_to_geodetic(eye).height;
        tuile_wgpu::SkyView {
            eye,
            render_origin: cam.position,
            forward: cam.direction,
            up: cam.up,
            fov_y: cam.fovy,
            aspect: f64::from(self.aspect()),
            height_px: h as u32,
            ground_height: height - self.controller.height_above_ground(),
        }
    }

    /// Each eye's view matrix, sun and air, against the one render origin
    /// every tile is rebased onto: the camera's.
    fn view_uniforms(&self) -> Vec<tuile_wgpu::ViewUniform> {
        let cam = self.controller.camera;
        let origin = cam.position;
        let aspect = self.aspect();
        let clearance = self.controller.height_above_ground();
        let sun = self.sun.light_travel_direction().as_vec3();
        // The closed-form haze is the path without the physical atmosphere
        // when the person turned it off; a host that turns the atmosphere
        // off (`HostFrame::atmosphere`) asks for no air at all, as the
        // checkbox does. With the physical atmosphere drawn, the shader
        // never reads this strength.
        let strength = if self.nav.atmosphere_enabled() && self.atmosphere.enabled {
            ATMOSPHERE_STRENGTH
        } else {
            0.0
        };
        self.eyes()
            .iter()
            .map(|eye| tuile_wgpu::ViewUniform {
                // The controller's own projection, which places the near plane
                // against the ground; an eye is the camera moved sideways, at
                // the same clearance.
                view_proj: eye.view_proj_with_clearance(origin, aspect, clearance),
                sun_dir: [sun.x, sun.y, sun.z, 0.0],
                params: [AMBIENT, self.views.diagnostic as f32, 0.0, 0.0],
                // Strength 0 is what turns it off, so the checkbox needs no
                // second path through the renderer — the shader already
                // short-circuits on it.
                atmosphere: tuile_atmosphere::AerialPerspective::of_air(
                    eye.position,
                    origin,
                    &self.sun,
                    &self.air,
                    strength,
                ),
            })
            .collect()
    }

    /// Builds the on-screen control's geometry for the camera as it now stands,
    /// so the needle agrees with the frame it is drawn over.
    fn upload_the_control(&mut self, indicator: Option<tuile_ui::Spinner>) {
        let ui = self.ui_frame();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        // The control reads the camera as it now stands, so the needle and the
        // horizon bar agree with the frame they are drawn over. Drawn in its
        // own units and placed in the safe area — see `UiFrame`.
        let controls = if self.controls {
            self.nav.mesh(ui.size, &self.controller)
        } else {
            Vec::new()
        };
        let mut nav_mesh: Vec<OverlayVertex> = controls
            .into_iter()
            .map(|v| OverlayVertex {
                position: ui.from(v.position),
                color: v.color,
            })
            .collect();
        // The loading indicator over everything, already in device pixels.
        if let Some(spinner) = indicator {
            nav_mesh.extend(spinner.mesh().into_iter().map(|v| OverlayVertex {
                position: v.position,
                color: v.color,
            }));
        }
        active
            .overlay
            .set_geometry(&active.gpu, &nav_mesh, active.size);
    }

    /// Resolves what to draw, draws it, and hands the frame to the compositor.
    ///
    /// One method because the resolved tiles borrow the pump: choosing them and
    /// drawing them cannot be separated without either copying the list or
    /// resolving twice. Returns how many were drawn and how well, for the
    /// status line — and how many of them still wear imagery other than the
    /// layer now being served, which is how far a switch has left to go.
    fn draw_and_present(&mut self) -> Option<(usize, tuile_wgpu::Resolution, usize)> {
        let serving = self.imagery.generation;
        let views = self.view_uniforms();
        let picture = self.picture();
        let viewport = self.viewport();
        // The same clearance the tiles' view uniform is built with, so an
        // overlay and the ground share one clip space.
        let clearance = self.controller.height_above_ground();
        let active = self.active.as_mut()?;
        // For any selected tile not yet uploaded, fall back to its nearest ready
        // ancestor so refinement never flashes the background.
        //
        // This host used to supply the parent function itself, by taking the
        // terrain encoding apart. That is right for a globe and silently wrong
        // for anything else: a 3D Tiles handle is an arena index, `z` came back
        // 0, the closure returned `None` at the first step and there was **no
        // fallback chain at all** — every unready tile reported lost, which is
        // the black square. The tree is the only thing that can answer, so the
        // server sends it and the pump reads it. Nothing here to get wrong.
        let (drawn, resolution) = active.pump.resolve(&active.gpu);
        let rendered = drawn.exact.len() + drawn.fallback.len();
        let old_imagery = drawn
            .exact
            .iter()
            .chain(drawn.fallback.iter())
            .filter(|tile| tile.imagery_source.is_some_and(|source| source != serving))
            .count();
        // The floor behind the draw set, each tile in its own drape: `resolve`
        // has brought it onto the render origin with everything else.
        let backdrop = crate::backdrop::shell_enabled();
        let coarse: Vec<&tuile_wgpu::PreparedTile> = drawn.backdrop.clone();
        let wireframe = self.views.wireframe;
        let trace = self.recording.trace.is_some();
        // Space black.
        let clear = wgpu::Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        let renderer = &active.renderer;
        let overlay = &active.overlay;
        let shell = &active.shell;
        // In a cell because the frame's two hooks are handed over together
        // and want it differently: the pass borrows it to draw, and the pass
        // that follows — the overlays' own depth — takes it mutably. The two
        // never run at once, which a cell says and the borrow checker cannot
        // see through two closures.
        active.world_overlay.set_view(
            &active.gpu.queue,
            &super::world_overlay_view(
                self.controller.camera,
                active.world_overlay.origin(),
                picture.aspect(),
                clearance,
            ),
        );
        let world = std::cell::RefCell::new(&mut active.world_overlay);
        let world_gpu = &active.gpu;
        // The picture, inside whatever the window is: everything of the scene
        // is drawn in it, and the bars around it keep the clear.
        let box_ = picture;
        let paint = |pass: &mut wgpu::RenderPass<'_>| {
            if box_.width != 0 && box_.height != 0 {
                // A plain clear outside, the scene in full inside: the
                // scissor keeps the bars, and the viewport maps the picture.
                pass.set_viewport(
                    box_.x as f32,
                    box_.y as f32,
                    box_.width as f32,
                    box_.height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(box_.x, box_.y, box_.width, box_.height);
            }
            renderer.paint(
                pass,
                // The shell first — the only thing that can cover ground
                // before any tile has arrived at all — then the complete
                // coarse level, which turns a flat patch into blurry
                // imagery, then the fallback ancestors: each stands in for
                // ground that is not its own and must never win a pixel
                // from a surface that owns it. See `tuile_wgpu::Drawn`.
                backdrop
                    .then_some(shell)
                    .into_iter()
                    .chain(coarse.iter().copied()),
                drawn.exact.iter().copied(),
                // Last, testing depth and never writing it: an ancestor
                // borrowing ground it does not own must be occluded by the
                // planet in front of it. See `TileRenderer::render_fallback`.
                drawn.fallback.iter().copied(),
                // The 2-D control is drawn after the world overlays, below,
                // because it owns the last word in the picture.
                None,
                wireframe,
            );
            // A host's shapes, after the ground that may hide them. What asked
            // for `OverlaysOnly` is not here: it goes in the pass that
            // follows, which has a depth of its own.
            world.borrow().render(pass);
            // The on-screen control, over everything and in the whole frame:
            // it is laid out in device pixels, not in the picture.
            if box_.width != 0 && box_.height != 0 {
                let (w, h) = (viewport.0 as f32, viewport.1 as f32);
                pass.set_viewport(0.0, 0.0, w, h, 0.0, 1.0);
                pass.set_scissor_rect(0, 0, w as u32, h as u32);
            }
            overlay.render(pass);
        };
        // The overlays' own pass, on the frame's encoder once its pass has
        // closed: the HUD's mode needs a depth attachment the ground never
        // reached, and that is a second pass or nothing.
        let after = |encoder: &mut wgpu::CommandEncoder, frame: &tuile_wgpu::FramePass<'_>| {
            world.borrow_mut().render_overlays_only(
                world_gpu,
                encoder,
                frame.colour,
                Some(frame.resolve),
                frame.size,
                // The picture, not the frame: this pass is held to the same
                // rectangle the frame's own pass was, or a HUD laid out in
                // fractions of the film would be laid out in fractions of
                // the window instead.
                box_.into(),
            );
        };
        let outcome = match &mut active.screen {
            super::screen::Screen::Window(windowed) => {
                let surface = windowed.surface.as_ref()?;
                let window = &windowed.window;
                // The acquire, the pass, the submit and the present — including
                // the four ways a drawable can fail to arrive — belong to the
                // backend now. What is left here is the one winit call in the
                // sequence, and the choice of what goes into the frame.
                tuile_wgpu::draw_to_surface(
                    tuile_wgpu::FrameOnSurface {
                        gpu: &active.gpu,
                        surface,
                        targets: &active.targets,
                        clear,
                        trace: trace.then_some(&mut active.readback),
                    },
                    || super::setup::configure(surface, &active.gpu, active.format, active.size),
                    || window.pre_present_notify(),
                    paint,
                    after,
                )
            }
            // Every eye, from the one resolved set of tiles, each with its own
            // view: what one eye sees, the other is not left without.
            #[cfg(feature = "control")]
            super::screen::Screen::Offscreen(offscreen) => {
                let _ = trace;
                let gpu = &active.gpu;
                offscreen.draw(
                    gpu,
                    &active.targets,
                    &views,
                    |view| renderer.set_view(&gpu.queue, view),
                    clear,
                    (paint, after),
                );
                tuile_wgpu::Presented::Yes
            }
        };
        let _ = &views;
        if outcome == tuile_wgpu::Presented::Invalid {
            tracing::warn!("the surface refused the frame");
        }
        if outcome != tuile_wgpu::Presented::Yes {
            return None;
        }
        Some((rendered, resolution, old_imagery))
    }

    /// Reads a headless frame back, eye by eye, and scans it: see the
    /// `offscreen` module. A window's frame is on screen and is not.
    fn read_the_eyes(&mut self) {
        #[cfg(feature = "control")]
        {
            let eyes = self.eyes();
            if let Some(active) = self.active.as_mut() {
                if let super::screen::Screen::Offscreen(offscreen) = &mut active.screen {
                    offscreen.read(&active.gpu, &eyes);
                }
            }
        }
    }

    /// The one line a person watching a globe wants, at most once a second.
    fn report_once_a_second(&mut self, rendered: usize, resolution: tuile_wgpu::Resolution) {
        let cam = self.controller.camera;
        if self.watch.last_log.elapsed().as_secs_f32() > 1.0 {
            self.watch_that_the_server_is_still_turning(cam);
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if self.watch.last_log.elapsed().as_secs_f32() > 1.0 {
            // Captured before the reset below, because the rate printed further
            // down divides by it.
            let interval = self.watch.last_log.elapsed().as_secs_f64();
            self.watch.last_log = web_time::Instant::now();
            if let Some(bar) = self.statusbar.as_ref() {
                bar.refresh();
                // Where the eye is, and how it is held. The second half is what
                // a tilt-only bug needs: a position alone cannot reproduce one.
                let g = tuile_core::geo::ecef_to_geodetic(cam.position);
                bar.set_camera(
                    g.lon,
                    g.lat,
                    self.controller.height_above_ground(),
                    cam.heading(),
                    cam.pitch(),
                );
            }
            let s = &active.pump.stats;
            let (_imagery_textures, imagery_bytes) =
                active.gpu.imagery.lock().expect("imagery textures").live();
            let budget = BudgetLine::of(
                tuile_core::metrics::budget().as_ref(),
                &self.profile.budget(),
            );
            // One line, and only what a person watching a globe would ask:
            // where am I, is the picture complete, is it sharp, and is the
            // engine still turning. Everything else — visited, culled, bytes,
            // cumulative counters — now lives behind `/metrics`, where a rate
            // can be taken instead of being subtracted by eye across two lines.
            let complete = Coverage::of(resolution);
            let sharpness = Sharpness::of(resolution);
            // The active view is named on every line, not only when it changes:
            // a diagnostic view left on is indistinguishable from a broken globe
            // for anyone reading the log later, including me an hour from now.
            let view = if self.views.diagnostic == 0 {
                String::new()
            } else {
                format!(" | VIEW: {}", DIAGNOSTICS[self.views.diagnostic].0)
            };
            // The clip planes, because ground beyond `far` is not drawn dark —
            // it is not drawn at all, and the black region that leaves is
            // indistinguishable from a tile that never arrived. `far` must stay
            // comfortably beyond the horizon distance printed beside it.
            let (near, far) = self
                .controller
                .camera
                .clip_planes(self.controller.height_above_ground());
            // The pacing, taken and reset here — this line defines the window
            // every other reader of those peaks sees.
            //
            // Two numbers, because they answer different questions and a
            // session needs both. `worst` is the longest frame in the last
            // second: it says the machine hitched. `jerk` is how much further
            // the eye travelled in its worst frame than an even pace would have
            // carried it: it says the hitch *reached the picture*. A large
            // `worst` with a `jerk` near one is a stall the easing absorbed,
            // which is the outcome being aimed at.
            let m = tuile_core::metrics::metrics();
            let worst = m.worst_frame_millis.take();
            let jerk = m.worst_camera_step.take();
            let drawn = m.frames.get();
            let since = drawn.saturating_sub(self.pacing.frames_at_last_log);
            self.pacing.frames_at_last_log = drawn;
            let pacing = format!(
                " | {:.0} fps, worst {worst:.0} ms, jerk {jerk:.1}x",
                since as f64 / interval.max(1.0e-3),
            );
            tracing::info!(
                "alt {:.0} km | ground {:.0} m | {} tiles, {complete}, {sharpness} | \
                 {} bare | {} loading | {:.0} MiB | {budget} | \
                 near {:.0} m far {:.0} km{pacing}{view}",
                cam.altitude() / 1000.0,
                self.controller.height_above_ground(),
                rendered,
                resolution.bare,
                s.requested,
                (active.pump.gpu_bytes as f32 + imagery_bytes as f32) / (1024.0 * 1024.0),
                near,
                far / 1000.0,
            );
            let errors = std::mem::take(&mut active.pump.errors);
            self.stats.errors += errors.len() as u64;
            for err in errors {
                tracing::warn!("server: {err}");
            }
        }
    }

    /// The **picture's** aspect ratio: the fixed shape a host asked for with
    /// `HostCamera::set_aspect`, or the frame's own in device pixels.
    ///
    /// The picture, not the window, because this is what reaches the
    /// projection: a camera held to 16:9 in a tall window sees 16:9 of world,
    /// and the bars above and below it are outside the picture entirely.
    fn aspect(&self) -> f32 {
        self.picture().aspect() as f32
    }

    /// Where the picture sits in the frame: the whole frame unless a host
    /// held the camera to an aspect, and then the largest rectangle of that
    /// shape, centred.
    fn picture(&self) -> crate::frame_host::Letterbox {
        let (w, h) = self.viewport();
        crate::frame_host::Letterbox::of((w as u32, h as u32), self.fixed_aspect)
    }

    /// Counts, and shouts about, ground the traversal asked for and nothing
    /// could draw.
    ///
    /// On screen that is the clear colour, and the clear colour is black — so
    /// this is the count that says the picture is *broken* rather than merely
    /// soft. Only on a change, because a per-frame warning during a hole buries
    /// its own onset.
    fn note_what_could_not_be_drawn(&mut self, resolution: tuile_wgpu::Resolution) {
        if resolution.coarser > 0 {
            self.stats.frames_with_gaps += 1;
        }
        // Ground the traversal asked for with nothing at all on its ancestor
        // chain to stand in. On screen that is the clear colour, and the clear
        // colour is black — so this is the count that says the picture is
        // *broken*, as opposed to merely soft. Shouted, and only on a change,
        // because a per-frame warning during a hole would bury its own onset.
        self.stats.holes = self.stats.holes.max(resolution.lost as u64);
        if resolution.has_holes() != self.watch.had_holes {
            self.watch.had_holes = resolution.has_holes();
            if resolution.has_holes() {
                tracing::warn!(
                    lost = resolution.lost,
                    deepest = resolution.deepest_lost,
                    coarser = resolution.coarser,
                    exact = resolution.exact,
                    frame = self.stats.frames,
                    "BLACK GROUND: selected tiles with no ancestor to draw"
                );
            } else {
                tracing::info!(frame = self.stats.frames, "black ground cleared");
            }
        }
    }

    /// Reads the frame that was just drawn back off the GPU and files it beside
    /// the camera that produced it.
    ///
    /// **Blocks**, which is why tracing is opt-in: a frame that waits for its
    /// own readback runs at a fraction of the speed of the session being
    /// diagnosed.
    fn record_the_traced_frame(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if self.recording.trace.is_some() {
            let size = active.size;
            let camera = self.controller.camera;
            if let Some(pixels) = active.readback.take(&active.gpu, size) {
                if let Some(trace) = self.recording.trace.as_mut() {
                    // The camera first, then the picture taken under it — the
                    // order `Tape::push_image` documents, and what makes the two
                    // land on the same frame index.
                    trace.push(tuile_tape::Frame {
                        position: camera.position.to_array(),
                        direction: camera.direction.to_array(),
                        up: camera.up.to_array(),
                        fovy: camera.fovy,
                    });
                    trace.push_image(&pixels, size.0, size.1);
                }
            }
        }
    }

    /// Says so, loudly, if the camera is moving and the server is not.
    ///
    /// Every other server-sourced number is a *last received* value, and those
    /// all sit perfectly still when the server is dead — which is
    /// indistinguishable from a server with nothing to do. This one is not.
    fn watch_that_the_server_is_still_turning(&mut self, cam: tuile_camera::GlobeCamera) {
        // No session, no server to have stopped.
        if self.session.is_none() {
            return;
        }
        // The server's passes, so a loop that has stopped turning is one
        // glance away instead of an afternoon. Every other server-sourced
        // number is a *last received* value: they all sit perfectly still
        // when the server is dead, which is indistinguishable from a server
        // with nothing to do. This one is not.
        //
        // Counted as the selections that *arrived* — every pass ends in one —
        // rather than off the server's own counter: a server in a browser's
        // worker keeps its counters in the worker's memory, where the page
        // would read zero for ever and cry wolf.
        let traversals = self
            .active
            .as_ref()
            .map_or(0, |active| active.pump.selects_seen());
        let moved = (cam.altitude() - self.watch.last_altitude.unwrap_or(f64::MIN)).abs() > 1.0;
        if moved && traversals == self.watch.last_traversals {
            self.watch.silent_passes += 1;
            if self.watch.silent_passes >= 3 {
                tracing::error!(
                    traversals,
                    seconds = self.watch.silent_passes,
                    "THE GEOMETRY SERVER HAS STOPPED: the camera is moving and no \
                         traversal has run"
                );
            }
        } else {
            self.watch.silent_passes = 0;
        }
        self.watch.last_traversals = traversals;
        self.watch.last_altitude = Some(cam.altitude());
    }
}

/// The budget's signals in one clause of the status line: what the GPU holds
/// against what the budget trims it to, how many loads are in flight, the
/// relaxation, and whether the view has converged — the numbers the overlay
/// shows, read from the same published snapshot as the metrics endpoint.
///
/// The target is the configured one: the ground's pool (terrain and imagery
/// together) and the other classes' own. The pool's members' effective
/// targets are each what the pool leaves the other — their sum moves with
/// what is held and says nothing about the room left.
struct BudgetLine(Option<tuile_core::budget::BudgetSignals>, u64, u64);

impl BudgetLine {
    fn of(
        signals: Option<&tuile_core::budget::BudgetSignals>,
        config: &tuile_core::budget::BudgetConfig,
    ) -> Self {
        let pool = &config.pool;
        let others: u64 = config
            .target
            .iter()
            .filter(|(c, _)| !pool.members[*c])
            .map(|(_, t)| t.0)
            .sum();
        let total: u64 = config
            .hard_limit
            .iter()
            .filter(|(c, _)| !pool.members[*c])
            .map(|(_, h)| h.0)
            .sum::<u64>()
            + pool.hard_limit.0;
        Self(signals.cloned(), pool.target.0 + others, total)
    }
}

impl fmt::Display for BudgetLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(s) = &self.0 else {
            return f.write_str("budget: not yet");
        };
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        let held: u64 = s.bytes.iter().map(|(_, [d, t])| d + t).sum();
        let flight: u32 = s.in_flight.iter().map(|(_, n)| n).sum();
        let evicted: u64 = s
            .evictions_total
            .iter()
            .map(|(_, e)| e.iter().sum::<u64>())
            .sum();
        let failed: u64 = s
            .failures_total
            .iter()
            .map(|(_, f)| f.iter().sum::<u64>())
            .sum();
        write!(
            f,
            "budget {:.0}/{:.0} MiB (hard {:.0}), {} in flight + {} local, {} evicted, \
             {} timed out, {} failed, scale {:.2}, {}",
            mib(held),
            mib(self.1),
            mib(self.2),
            flight,
            s.local_reads,
            evicted,
            s.cancels_total[1],
            failed,
            f64::from(s.sse_scale_milli) / 1000.0,
            if s.converged { "converged" } else { "settling" }
        )
    }
}

/// Whether every selected tile was drawn by *something*.
///
/// A named pair rather than two string literals inside a `format!`, because the
/// distinction is the most important one the status line makes and the two
/// spellings have to stay in step: this is the difference between ground that is
/// merely soft and ground that is **black**, which is the one output
/// `CLAUDE.md` forbids. Shouted in capitals for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    Complete,
    Holes,
}

impl Coverage {
    fn of(resolution: tuile_wgpu::Resolution) -> Self {
        if resolution.lost == 0 {
            Self::Complete
        } else {
            Self::Holes
        }
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Complete => "complete",
            Self::Holes => "HOLES",
        })
    }
}

/// Whether the ground is drawn at the detail the traversal asked for.
///
/// Distinct from [`Coverage`] and often confused with it: a coarse tile standing
/// in for a finer one is *soft*, which is a normal state during streaming, while
/// a tile with nothing on its ancestor chain is a hole. Reporting them in one
/// word is how a session that is merely loading gets read as one that is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sharpness {
    Sharp,
    Coarse(usize),
}

impl Sharpness {
    fn of(resolution: tuile_wgpu::Resolution) -> Self {
        if resolution.coarser == 0 {
            Self::Sharp
        } else {
            Self::Coarse(resolution.coarser)
        }
    }
}

impl fmt::Display for Sharpness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sharp => f.write_str("sharp"),
            Self::Coarse(n) => write!(f, "{n} tiles still coarse"),
        }
    }
}
