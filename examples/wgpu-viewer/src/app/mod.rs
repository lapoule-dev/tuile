// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The winit application: owns the window, surface and render loop, feeds
//! pixel gestures to the render-agnostic [`tuile_camera::CameraController`] and
//! drives the geometry stream through the [`ContentPump`].

mod frame;
mod input;
mod pacing;
mod setup;
mod tape;
mod warmup;

use std::sync::Arc;
use tuile_camera::CameraController;
use tuile_core::protocol::InProcessStream;
use tuile_ui::NavWidget;
use tuile_wgpu::{ContentPump, GpuContext, OverlayRenderer, TileRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::{Window, WindowId};

/// How much air to put between the eye and the ground.
///
/// 1.0 is the physical atmosphere, and the physical atmosphere is the right
/// answer: Earth's own haze is already the cue people read, and anything above
/// it starts hiding the ground it is meant to place. This sat at 1.6 while the
/// optical depth was computed wrongly, which is the usual way a fudge factor
/// gets in — it was compensating for a bug, not for the physics.
const ATMOSPHERE_STRENGTH: f32 = 1.0;

/// How much of a surface's colour survives facing away from the light.
///
/// Not a physical quantity — it stands in for every bounce the single-bounce
/// lighting above does not model. At 0 an unlit slope is pure black, which no
/// daylit terrain ever is; at 1 the shading disappears and with it every hint of
/// relief the mesh carries.
const AMBIENT: f32 = 0.5;

/// Everything known before the window exists.
pub struct ViewerConfig {
    pub stream: InProcessStream,
    pub controller: CameraController,
    pub detail: tuile_planetary::ImageryDetail,
    /// How many imagery layers a drape may carry, filled in from the device
    /// once there is one — see [`tuile_planetary::LayerBudget`].
    pub layer_budget: tuile_planetary::LayerBudget,
    pub title: String,
    /// The instant the scene is lit for, UTC seconds since the Unix epoch —
    /// which sets where the sun is, and so where the terminator falls.
    pub lit_at_unix_seconds: f64,
}

struct Active {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    gpu: GpuContext,
    renderer: TileRenderer,
    overlay: OverlayRenderer,
    pump: ContentPump,
    stream: InProcessStream,
    /// The multisampled colour target and its depth buffer. The surface itself
    /// has one sample, so the scene is drawn here and *resolved* into it —
    /// which is what turns the staircase on the limb into a smooth edge.
    targets: tuile_wgpu::FrameTargets,
    /// The frame in **device** pixels: what the render targets are sized to and
    /// what the imagery resolution is chosen against.
    size: (u32, u32),
    /// Device pixels per point, as the display reports it — 2 on a Retina panel.
    ///
    /// Geometry and imagery are chosen for different units, and conflating them
    /// costs both ways. The mesh wants device pixels: that is what a triangle is
    /// actually rasterised into, and a coarse silhouette shows. Imagery wants
    /// *points*, because a map tile is drawn for them — its labels and its line
    /// weights are sized in the same units a stylesheet is. Asking for imagery at
    /// device density on a Retina display fetches one level deeper than the
    /// screen can show, which is four times the tiles for type at half the size
    /// it was drawn to be read at.
    scale: f64,
    /// Where a traced frame is copied before being read back.
    readback: tuile_wgpu::Readback,
    /// A whole-planet shell, five hundred metres below the ellipsoid, drawn
    /// before the tiles.
    ///
    /// The guarantee that ground is never bare, and it is a guarantee rather
    /// than a mitigation: whatever the traversal selects, whatever arrives
    /// late, whatever is evicted, this is already drawn behind it. Every
    /// attempt to promise the same thing upstream held only for the cases that
    /// had been thought of.
    shell: tuile_wgpu::PreparedTile,
}

/// The diagnostic views, in the order `D` cycles them.
///
/// Each one removes exactly one explanation for a black pixel, so pressing `D`
/// twice narrows four candidates to one. The order is deliberate: cheapest
/// question first.
///
/// | view | black here means |
/// |---|---|
/// | normal | one of four things, which is the problem |
/// | unlit | not the sun and not the air — they are switched off |
/// | coverage | never black: magenta = no imagery reached this pixel |
/// | geometry | nothing was drawn at all — a mesh is missing |
///
/// So: black in *normal* that turns to picture in *unlit* is a lighting or
/// atmosphere fault. Black that survives *unlit* and shows magenta in
/// *coverage* is missing imagery. Black that survives *unlit*, is green in
/// *coverage*, and is lit in *geometry* is imagery that is genuinely black —
/// look for the `BLACK IMAGERY` warning. Black that stays black in *geometry*
/// is no mesh at all, and belongs to traversal or upload, never to texturing.
const DIAGNOSTICS: [(&str, &str); 4] = [
    ("normal", "the real picture"),
    ("unlit", "imagery only; sun and air off"),
    (
        "coverage",
        "magenta = no imagery here, green = one layer, blue = several",
    ),
    ("geometry", "flat lit surface; black = no mesh drawn"),
];

pub struct App {
    config: Option<ViewerConfig>,
    controller: CameraController,
    detail: tuile_planetary::ImageryDetail,
    layer_budget: tuile_planetary::LayerBudget,
    title: String,
    /// Where the sun is for the instant the scene is lit at. Resolved once: the
    /// hour is fixed for a session, so recomputing the ephemeris every frame
    /// would answer the same question sixty times a second.
    sun: tuile_atmosphere::Sun,
    /// Everything that only exists once there is a window. `None` until
    /// `resumed`, and that is the only reason this is an `Option`.
    active: Option<Active>,

    // The rest is grouped by the module that owns it, so that a field and the
    // code that reads it are never in different files. Each of these is defined
    // beside its behaviour rather than here.
    pointer: input::Pointer,
    views: input::Views,
    recording: tape::Recording,
    warmup: warmup::Warmup,
    pacing: pacing::Pacing,
    watch: pacing::Watch,

    /// The on-screen navigation control. It sees pointer events first and, when
    /// it takes one, the globe must not also act on it.
    nav: NavWidget,
    /// The window is hidden — another window covers it, the display slept, the
    /// app was minimized. Rendering while occluded leaks GPU memory on Apple
    /// platforms.
    occluded: bool,
    /// The menu-bar readout, if the platform gave us one.
    statusbar: Option<tuile_ui::StatusBar>,
    /// Running totals, reported once per second and again on the way out.
    stats: crate::session::Stats,
}

impl App {
    pub fn new(config: ViewerConfig) -> Self {
        Self {
            controller: config.controller.clone(),
            detail: config.detail.clone(),
            layer_budget: config.layer_budget.clone(),
            title: config.title.clone(),
            sun: tuile_atmosphere::Sun::at_unix_seconds(config.lit_at_unix_seconds),
            config: Some(config),
            active: None,
            pointer: input::Pointer::default(),
            views: input::Views::default(),
            recording: tape::Recording {
                tape: crate::recording::open_tape(),
                trace: crate::recording::open_trace(),
                finished: false,
            },
            warmup: warmup::Warmup {
                holding: true,
                pinned_level: crate::settings::pinned_level(),
                since: std::time::Instant::now(),
                last_report: std::time::Instant::now(),
            },
            pacing: pacing::Pacing {
                last_frame: std::time::Instant::now(),
                last_step: None,
                last_seconds: 0.0,
                frames_at_last_log: 0,
                last_heading: None,
            },
            watch: pacing::Watch {
                had_holes: false,
                last_traversals: 0,
                last_altitude: None,
                silent_passes: 0,
                last_log: std::time::Instant::now(),
            },
            nav: NavWidget::new(),
            occluded: false,
            // Built with the window rather than here: a status item wants the
            // main thread and an event loop already running.
            statusbar: None,
            stats: crate::session::Stats {
                started: Some(std::time::Instant::now()),
                ..crate::session::Stats::default()
            },
        }
    }

    /// A one-line summary of the session, for the exit log.
    pub fn report(&self) -> String {
        self.stats.to_string()
    }

    fn viewport(&self) -> (f64, f64) {
        self.active
            .as_ref()
            .map_or((1920.0, 1080.0), |a| (a.size.0 as f64, a.size.1 as f64))
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return;
        }
        self.start_window(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.on_window_event(event_loop, event);
    }

    /// Called once as the loop unwinds, however it was left — the close box,
    /// Escape, or a replay reaching the end of its tape.
    ///
    /// The recording is finished here rather than on `WindowEvent::Destroyed`,
    /// which is not delivered on every platform before the loop returns: the
    /// first session recorded left a zero-byte file, because a container
    /// without its footer is not a file anything will open.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.close_the_tape();
    }

    /// Drives the animation: one redraw per loop iteration while the window is
    /// visible. Requesting from here rather than from the `RedrawRequested`
    /// handler is what lets an occluded window stop cleanly — the loop parks on
    /// `Wait` until the next event instead of spinning.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A flown tape ends the session. Without this a replay leaves a window
        // sitting on its last frame, and the run has to be closed by hand —
        // which is exactly what stops anyone from putting it in a script.
        if self.recording.finished {
            tracing::info!("{}", self.report());
            self.close_the_tape();
            event_loop.exit();
            return;
        }
        // Ctrl-C or `kill`: unwind rather than die, so the recording is closed.
        if crate::signals::INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!("interrupted; closing the session");
            self.close_the_tape();
            event_loop.exit();
            return;
        }
        // Hidden and warming: the window is not drawing, so nothing else is
        // draining the stream — and the uploads being waited for happen in the
        // pump. Driving it from here is what keeps the wait from being a
        // deadlock.
        if self.warmup.holding {
            self.pump_while_hidden();
            event_loop.set_control_flow(ControlFlow::Poll);
            return;
        }
        event_loop.set_control_flow(if self.occluded {
            ControlFlow::Wait
        } else {
            ControlFlow::Poll
        });
        if !self.occluded {
            if let Some(active) = self.active.as_ref() {
                active.window.request_redraw();
            }
        }
    }
}

impl Drop for App {
    /// Last line of defence: a panic unwinding out of the event loop still has
    /// to leave a usable recording. Closing an already-closed tape is a no-op,
    /// so this costs nothing on the ordinary path.
    fn drop(&mut self) {
        self.close_the_tape();
    }
}
