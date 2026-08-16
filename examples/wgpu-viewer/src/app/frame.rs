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
use glam::DVec2;
use tuile_core::source::TileId;
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
        if self.occluded {
            return;
        }
        let dt = self.start_the_frame();
        self.ease_the_camera(dt);
        if self.active.is_none() {
            return;
        }
        self.choose_the_imagery_detail();
        self.take_this_frames_share_of_the_stream();
        self.upload_the_view();
        self.upload_the_control();
        let Some((rendered, resolution)) = self.draw_and_present() else {
            return;
        };
        // After the draw, and deliberately: neither of these is drawing, and
        // both want the whole of `self` while the drawn tiles still borrow the
        // pump.
        self.note_what_could_not_be_drawn(resolution);
        self.record_the_traced_frame();
        self.report_once_a_second(rendered, resolution);
    }

    /// Closes the previous frame's clock and opens this one's.
    ///
    /// Returns the interval, which is the only number the easing is allowed to
    /// depend on.
    fn start_the_frame(&mut self) -> std::time::Duration {
        let dt = self.pacing.last_frame.elapsed();
        self.pacing.last_frame = std::time::Instant::now();
        let m = tuile_core::metrics::metrics();
        m.frames.inc();
        m.frame_seconds.record(dt);
        m.worst_frame_millis.record(dt.as_secs_f64() * 1000.0);
        dt
    }

    /// Moves the eye toward where the gestures have put it, and watches how it
    /// went.
    fn ease_the_camera(&mut self, dt: std::time::Duration) {
        let before = self.controller.camera.position;
        self.controller.advance(dt.as_secs_f64());
        self.record_the_pacing(before, dt);
        // A tape, if one is running, overrides the camera the gestures produced
        // — so the render, the traversal and the log line all come from the
        // same recorded numbers, with nothing to drift between them.
        self.run_the_tape();
        self.watch_for_a_sudden_turn();
    }

    /// Tells the loader how sharp the ground should be.
    fn choose_the_imagery_detail(&mut self) {
        let cam = self.controller.camera;
        let (_, height) = self.viewport();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let viewport = DVec2::new(active.size.0 as f64, height);
        // Drive imagery resolution by altitude: the ground metres that map to
        // one screen point ≈ 2·altitude·tan(fovy/2) / viewport_height. The
        // loader drapes imagery at (at least) that texel spacing — a giant fine
        // mosaic up close, coarse from orbit.
        //
        // Points, not device pixels. A map tile is authored for points: its type
        // and its line weights are sized the way a stylesheet sizes them, to be
        // read at that scale. Dividing by the device height instead asks for one
        // level deeper on a Retina panel — four times the tiles, carrying labels
        // at half the size they were drawn to be legible at, which is how they
        // became unreadable. Geometry is unaffected and keeps every device pixel:
        // it is the *mesh* that is rasterised, and its silhouette shows.
        let points_high = (viewport.y / active.scale).max(1.0);
        let target_texel = 2.0 * cam.altitude() * (cam.fovy * 0.5).tan() / points_high;
        self.detail.set_target_texel_spacing(target_texel);
    }

    /// Sends the camera, takes whatever the server has produced up to this
    /// frame's upload budget, and counts what that cost.
    fn take_this_frames_share_of_the_stream(&mut self) {
        let cam = self.controller.camera;
        let (width, height) = self.viewport();
        let viewport = DVec2::new(width, height);
        let Some(active) = self.active.as_mut() else {
            return;
        };
        self.stats.frames += 1;
        let before = active.pump.prepared_count();
        // Anti-jitter: render origin = eye, so the f32 the GPU sees is small.
        let origin = cam.position;
        // The frame's streaming step, shared with the headless tests — send the
        // camera, take this frame's share of uploads, rebase. Freezing simply
        // withholds the camera, which is what makes `F` a test of the renderer
        // against a fixed selection.
        let uploaded = if self.views.freeze {
            active
                .pump
                .pump(&mut active.stream, &active.gpu, tuile_wgpu::UPLOADS_PER_FRAME)
        } else {
            active.pump.advance(
                &mut active.stream,
                &active.gpu,
                cam.view_state(viewport),
                origin,
            )
        };
        self.stats.uploads += uploaded as u64;
        // A drop in the prepared count is the server reclaiming: worth counting
        // separately from uploads, since a healthy session does far more of the
        // former than the latter once the view settles.
        self.stats.evictions +=
            (before + uploaded).saturating_sub(active.pump.prepared_count()) as u64;
        if self.views.freeze {
            active.pump.rebase(&active.gpu.queue, origin);
        }
        // The shell too, and forgetting it is why it was invisible: the pump
        // rebases the tiles it owns, the shell is not one of them, and a mesh
        // still anchored to the Earth's centre while the view is relative to
        // the eye is drawn six thousand kilometres from where it belongs.
        active.shell.rebase(&active.gpu.queue, origin);
    }

    /// Uploads the view matrix, the sun and the air.
    fn upload_the_view(&mut self) {
        let cam = self.controller.camera;
        let origin = cam.position;
        let view_proj = self.controller.view_proj(origin, self.aspect());
        let sun = self.sun.light_travel_direction().as_vec3();
        let Some(active) = self.active.as_mut() else {
            return;
        };

        active.renderer.set_view(
            &active.gpu.queue,
            &tuile_wgpu::ViewUniform {
                view_proj,
                sun_dir: [sun.x, sun.y, sun.z, 0.0],
                params: [AMBIENT, self.views.diagnostic as f32, 0.0, 0.0],
                // Strength 0 is what turns it off, so the checkbox needs no
                // second path through the renderer — the shader already
                // short-circuits on it.
                atmosphere: tuile_atmosphere::AerialPerspective::new(
                    cam.position,
                    origin,
                    &self.sun,
                    if self.nav.atmosphere_enabled() {
                        ATMOSPHERE_STRENGTH
                    } else {
                        0.0
                    },
                ),
            },
        );
    }

    /// Builds the on-screen control's geometry for the camera as it now stands,
    /// so the needle agrees with the frame it is drawn over.
    fn upload_the_control(&mut self) {
        let (width, height) = self.viewport();
        let viewport = DVec2::new(width, height);
        let Some(active) = self.active.as_mut() else {
            return;
        };
        // The control reads the camera as it now stands, so the needle and the
        // horizon bar agree with the frame they are drawn over.
        let nav_mesh: Vec<OverlayVertex> = self
            .nav
            .mesh((viewport.x, viewport.y), &self.controller)
            .into_iter()
            .map(|v| OverlayVertex {
                position: v.position,
                color: v.color,
            })
            .collect();
        active
            .overlay
            .set_geometry(&active.gpu, &nav_mesh, active.size);
    }

    /// Resolves what to draw, draws it, and hands the frame to the compositor.
    ///
    /// One method because the resolved tiles borrow the pump: choosing them and
    /// drawing them cannot be separated without either copying the list or
    /// resolving twice. Returns how many were drawn and how well, for the
    /// status line.
    fn draw_and_present(&mut self) -> Option<(usize, tuile_wgpu::Resolution)> {
        let active = self.active.as_mut()?;
        // For any selected terrain tile not yet uploaded, fall back to its
        // nearest ready ancestor so refinement never flashes the background.
        let parent_of = |id: TileId| {
            let (z, x, y) = id.terrain_coord();
            (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2))
        };
        let (drawn, resolution) = active.pump.resolve(&active.gpu.queue, parent_of);
        let rendered = drawn.exact.len() + drawn.fallback.len();
        // `resolve` has already brought the selection onto the current render
        // origin. The coarse layer behind it is drawn without going through
        // `resolve`, so it asks for itself.
        if crate::backdrop::shell_enabled() {
            tuile_core::metrics::metrics().rebase_seconds.time(|| {
                active
                    .pump
                    .rebase_for_drawing(&active.gpu.queue, active.pump.at_level(crate::backdrop::BASE_LEVEL));
            });
        }
        let backdrop = crate::backdrop::shell_enabled();
        // Collected rather than lazily iterated, because the paint closure and
        // the trace's `&mut readback` are handed to the same call: a live
        // iterator over `pump` would hold a borrow across it.
        let coarse: Vec<&tuile_wgpu::PreparedTile> = if backdrop {
            active.pump.at_level(crate::backdrop::BASE_LEVEL).collect()
        } else {
            Vec::new()
        };
        let wireframe = self.views.wireframe;
        let trace = self.recording.trace.is_some();
        // The acquire, the pass, the submit and the present — including the four
        // ways a drawable can fail to arrive — belong to the backend now. What
        // is left here is the one winit call in the sequence, and the choice of
        // what goes into the frame.
        let outcome = tuile_wgpu::draw_to_surface(
            tuile_wgpu::FrameOnSurface {
                gpu: &active.gpu,
                surface: &active.surface,
                targets: &active.targets,
                // Space black.
                clear: wgpu::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                trace: trace.then_some(&mut active.readback),
            },
            || {
                super::setup::configure(
                    &active.surface,
                    &active.gpu,
                    active.surface_format,
                    active.size,
                )
            },
            || active.window.pre_present_notify(),
            |pass| {
                active.renderer.paint(
                    pass,
                    // The shell first — the only thing that can cover ground
                    // before any tile has arrived at all — then the complete
                    // coarse level, which turns a flat patch into blurry
                    // imagery, then the fallback ancestors: each stands in for
                    // ground that is not its own and must never win a pixel
                    // from a surface that owns it. See `tuile_wgpu::Drawn`.
                    backdrop
                        .then_some(&active.shell)
                        .into_iter()
                        .chain(coarse.iter().copied())
                        .chain(drawn.fallback.iter().copied()),
                    drawn.exact.iter().copied(),
                    Some(&active.overlay),
                    wireframe,
                );
            },
        );
        if outcome == tuile_wgpu::Presented::Invalid {
            tracing::warn!("the surface refused the frame");
        }
        if outcome != tuile_wgpu::Presented::Yes {
            return None;
        }
        Some((rendered, resolution))
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
            self.watch.last_log = std::time::Instant::now();
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
                 {} loading | {:.0} MiB | near {:.0} m far {:.0} km{pacing}{view}",
                cam.altitude() / 1000.0,
                self.controller.height_above_ground(),
                rendered,
                s.requested,
                (active.pump.gpu_bytes as f32 + imagery_bytes as f32) / (1024.0 * 1024.0),
                near,
                far / 1000.0,
            );
            let errors = active.pump.errors.drain(..).collect::<Vec<_>>();
            self.stats.errors += errors.len() as u64;
            for err in errors {
                tracing::warn!("server: {err}");
            }
        }
    }

    /// The frame's aspect ratio, in device pixels.
    fn aspect(&self) -> f32 {
        let (w, h) = self.viewport();
        (w / h.max(1.0)) as f32
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
            // The server's own pass counter, so a loop that has stopped turning
            // is one glance away instead of an afternoon. Every server-sourced
            // number below is a *last received* value: they all sit perfectly
            // still when the server is dead, which is indistinguishable from a
            // server with nothing to do. This one is not.
            let traversals = tuile_core::metrics::metrics().traversals.get();
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
