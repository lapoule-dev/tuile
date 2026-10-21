// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The winit application: owns the window, surface and render loop, feeds
//! pixel gestures to the render-agnostic [`tuile_camera::CameraController`] and
//! drives the geometry stream through the [`ContentPump`].

mod command;
mod demand;
#[cfg(feature = "control")]
mod driven;
mod eyes;
mod flight;
mod frame;
mod gestures;
mod imagery;
mod input;
mod lifecycle;
mod loading;
mod messages;
#[cfg(feature = "control")]
pub(crate) mod offscreen;
mod pacing;
mod readout;
mod screen;
mod script;
mod setup;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use setup::backend_named;
mod stall;
mod tape;
mod touch;
mod warmup;

pub(crate) use eyes::Rig;
#[cfg(feature = "control")]
pub(crate) use touch::Phase as Finger;

use tuile_camera::CameraController;
use tuile_ui::NavWidget;
use tuile_wgpu::{ContentPump, GpuContext, OverlayRenderer, TileRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::WindowId;

/// How much air to put between the eye and the ground, with the checkbox on:
/// all of [`ViewerConfig::air`]'s, whose own strength
/// (`TUILE_AIR_VIEWER_STRENGTH`, 1 by default) scales it.
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
///
/// The session itself — the stream, the globe's handles — is not here: it
/// arrives through a [`crate::launch::Launch`], at once on a desktop and a few
/// seconds into the window's life on a phone.
pub struct ViewerConfig {
    pub controller: CameraController,
    /// An embedding application that owns each frame: asked for the camera
    /// before the frame is prepared, and given events before the controls.
    ///
    /// `None` is the viewer as it has always been — a person's gestures, the
    /// ease and the floor. See [`crate::frame_host`].
    pub frame_host: Option<Box<dyn crate::frame_host::FrameHost>>,
    pub title: String,
    /// The host's imagery layers, and which of them the session opens on.
    pub layers: &'static [crate::embed::ImageryChoice],
    pub layer: usize,
    /// Set by `--here`: ask for the current location at start, and place the
    /// view like this when it is known.
    pub here: Option<crate::location::Placement>,
    /// Steering from other programs, as it arrives: URLs still unread and
    /// untrusted (see [`crate::steer::parse_url`]), and a script's commands.
    pub requests: std::sync::mpsc::Receiver<crate::steer::Request>,
    /// Whether to leave the last view for the next session: see
    /// `App::remember_the_view`.
    pub remembers: bool,
    /// The instant the scene is lit for, UTC seconds since the Unix epoch —
    /// which sets where the sun is, and so where the terminator falls.
    pub lit_at_unix_seconds: f64,
    /// The air between the eye and the ground: its molecules, its aerosols
    /// and its strength, today's by default — on a desktop, as the
    /// `TUILE_AIR_*` tuning values say ([`tuile_atmosphere::tuning`]).
    pub air: tuile_atmosphere::AerialAir,
    /// The level held in memory for the session, if any.
    pub pinned_level: Option<u32>,
    /// How hard the session may push the machine.
    pub(crate) profile: crate::profile::Profile,
    /// How a failed start-up is tried again; `None` where it is not.
    pub(crate) retries: Option<crate::launch::Retries>,
    /// The window the shell wants: a desktop's or a phone's own, or a
    /// browser page's canvas. The title and the visibility are the
    /// application's to set.
    pub(crate) window: winit::window::WindowAttributes,
    /// Whether the on-screen controls are drawn and take the pointer first.
    pub(crate) controls: bool,
    /// The graphics backends to choose from; `None` is the platform's own
    /// choice. A page may force WebGL2, say, to test it.
    pub(crate) backends: Option<wgpu::Backends>,
}

/// Where the on-screen controls live: the part of the frame not under a
/// notch, a rounded corner or the home indicator, and how many device pixels
/// one of the controls' units is.
///
/// On a desktop that is the whole frame at one pixel per unit — the controls
/// look exactly as they always did. On a phone it is the safe area, at the
/// display's scale, so the compass is a thumb's width and not a third of one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UiFrame {
    /// The safe area's top-left corner, in device pixels.
    pub origin: (f64, f64),
    /// Device pixels per control unit.
    pub scale: f64,
    /// The safe area's size, in control units.
    pub size: (f64, f64),
}

impl UiFrame {
    /// From device pixels to the controls' units.
    pub(super) fn to(&self, px: (f64, f64)) -> (f64, f64) {
        (
            (px.0 - self.origin.0) / self.scale,
            (px.1 - self.origin.1) / self.scale,
        )
    }

    /// From the controls' units to device pixels.
    pub(super) fn from(&self, at: [f32; 2]) -> [f32; 2] {
        [
            (f64::from(at[0]) * self.scale + self.origin.0) as f32,
            (f64::from(at[1]) * self.scale + self.origin.1) as f32,
        ]
    }

    /// The safe area's centre, in device pixels.
    pub(super) fn centre(&self) -> (f64, f64) {
        (
            self.origin.0 + self.size.0 * self.scale * 0.5,
            self.origin.1 + self.size.1 * self.scale * 0.5,
        )
    }
}

struct Active {
    /// Where the frame goes: a window's surface, or a headless run's
    /// textures. See [`screen`].
    screen: screen::Screen,
    /// What the surface is configured with, and what the frame is drawn in.
    format: tuile_wgpu::SurfaceFormat,
    gpu: GpuContext,
    renderer: TileRenderer,
    overlay: OverlayRenderer,
    /// A host's shapes in the world: `tuile_overlay::OverlayMesh`, through
    /// the frame's own camera. Nothing is drawn through it in a session with
    /// no host, and it costs three pipelines.
    world_overlay: tuile_wgpu::WorldOverlayRenderer,
    pump: ContentPump,
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
    /// The safe area, in device pixels: left, top, width, height. The whole
    /// frame on a desktop.
    safe: (f64, f64, f64, f64),
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
pub(crate) const DIAGNOSTICS: [(&str, &str); 4] = [
    ("normal", "the real picture"),
    ("unlit", "imagery only; sun and air off"),
    (
        "coverage",
        "magenta = no imagery here, green = one layer, blue = several",
    ),
    ("geometry", "flat lit surface; black = no mesh drawn"),
];

pub struct App {
    controller: CameraController,
    layer_budget: crate::shell::Budget,
    title: String,
    /// Where the sun is for the instant the scene is lit at. Resolved once: the
    /// hour is fixed for a session, so recomputing the ephemeris every frame
    /// would answer the same question sixty times a second.
    sun: tuile_atmosphere::Sun,
    /// The air between the eye and the ground ([`ViewerConfig::air`]).
    air: tuile_atmosphere::AerialAir,
    /// Everything that only exists once there is a window. `None` until
    /// `resumed`, and that is the only reason this is an `Option`.
    active: Option<Active>,
    /// The session, once it is up: its stream and the globe's handles.
    session: Option<crate::shell::Started>,
    /// The session on its way up; `None` once it has arrived or failed.
    launch: Option<crate::launch::Launch>,
    /// Why the session could not start, once that is known.
    failed: Option<String>,
    /// What the person is to be told, once there is a window to tell them.
    messages: messages::Messages,
    /// Trying a failed start-up again, on a phone.
    retries: Option<crate::launch::Retries>,
    /// When the start-up now under way began.
    launching_since: Option<web_time::Instant>,
    profile: crate::profile::Profile,
    /// The window the shell asked for; see [`ViewerConfig::window`].
    window_attributes: winit::window::WindowAttributes,
    /// The GPU on its way up, where bringing it up cannot block: see `setup`.
    gpu_on_its_way: Option<setup::GpuOnItsWay>,
    /// See [`ViewerConfig::backends`].
    backends: Option<wgpu::Backends>,
    lifecycle: lifecycle::Lifecycle,
    loading: loading::Loading,
    demand: demand::Demand,
    touches: touch::Touches,
    /// A scripted hand, when `TUILE_TOUCH_SCRIPT` names one.
    script: Option<script::TouchScript>,
    /// The last frame's resolution, which the next frame's indicator reads.
    resolution: Option<tuile_wgpu::Resolution>,
    /// Tiles uploaded on the last frame.
    uploaded: usize,
    /// The display's refresh, frames a second: what a frame's upload
    /// deadline is made of (I18). The monitor's when the window knows it.
    pub(crate) refresh_hz: u32,
    /// The device's temperature and power saving, and when they were read.
    thermal: crate::profile::Thermal,
    low_power: bool,
    device_read: Option<web_time::Instant>,
    /// The memory signals acted on, and when the count was last read: in the
    /// background too, where the system's trims matter most.
    pressure: lifecycle::PressureWatch,
    pressure_read: Option<web_time::Instant>,
    /// When the last memory signal was handed to the budget: see
    /// `calm_down`.
    squeezed_at: Option<web_time::Instant>,

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
    /// Whether it is drawn at all: a page embedding the globe may bring its
    /// own.
    controls: bool,
    /// The request for the current location, from the key press or `--here`
    /// to the frame the system's answer arrives on.
    locator: crate::location::Locator,
    /// Whether the view is where it is because the location service put it
    /// there — in which case it is not reported to other programs.
    located: bool,
    /// The imagery: the host's layers, which one is draped, and a change of
    /// layer on its way — see `imagery.rs`.
    imagery: imagery::Imagery,
    /// Steering waiting to be carried out.
    requests: std::sync::mpsc::Receiver<crate::steer::Request>,
    /// A glide under way, which any gesture ends.
    flight: Option<flight::Flight>,
    /// An embedding application that owns the frame: it is asked for the
    /// camera before each one, and sees events before the controls do.
    ///
    /// Boxed rather than a type parameter so the shells, the control API and
    /// the tests all keep one `App`: a host is an option a session has or has
    /// not, not a different application.
    frame_host: Option<Box<dyn crate::frame_host::FrameHost>>,
    /// Poses the host asked to be traversed without drawing, from the last
    /// frame it spoke. Replaced wholesale each time it does.
    preload: Vec<crate::frame_host::PreloadView>,
    /// The shapes the host added to the scene, by layer. **Kept** between
    /// frames: a layer is drawn until it is set again or cleared.
    overlays: crate::frame_host::OverlayLayers,
    /// The shape the picture is held to, letterboxed, when the host asked for
    /// one: see `HostCamera::set_aspect`.
    fixed_aspect: Option<f64>,
    /// Tiles drawn in the last frame, and whether all were the ones selected.
    drawn: (usize, bool),
    /// The last thing said to the person, and when.
    notice: Option<(String, web_time::Instant)>,
    /// What the title bar shows, and when it was last looked at.
    title_shown: String,
    title_at: web_time::Instant,
    /// Why the session is ending, once something has decided that it is.
    pub(crate) ended_by: Option<&'static str>,
    /// Whether the view is written down at the end for the next start.
    remembers: bool,
    /// The window is hidden — another window covers it, the display slept, the
    /// app was minimized. Rendering while occluded leaks GPU memory on Apple
    /// platforms.
    occluded: bool,
    /// The menu-bar readout, if the platform gave us one.
    statusbar: Option<readout::StatusBar>,
    /// The application's menu bar, where the platform has one.
    menu: Option<crate::menu::Installed>,
    /// Running totals, reported once per second and again on the way out.
    stats: crate::session::Stats,
    /// The eyes a frame is drawn for: one, unless a headless run asks for a
    /// headset's pair. See [`eyes`].
    rig: Rig,
    /// How a message for the person is shown: an alert, or — for a headless
    /// run, which has nobody to dismiss one — a line in the log.
    speaker: fn(&str, &str) -> bool,
    /// Frames that reached the screen, which a headless run counts its
    /// frames by.
    #[cfg(feature = "control")]
    presented: u64,
}

/// What a turn of the loop leaves the loop to do.
pub(crate) enum Turn {
    /// The session is over: leave the loop.
    End,
    /// Wait as this says, for the next turn.
    Flow(ControlFlow),
}

impl App {
    /// `launch` brings the session: already arrived on a desktop, on its way
    /// on a phone.
    pub(crate) fn new(mut config: ViewerConfig, launch: crate::launch::Launch) -> Self {
        // The queue is taken out of the configuration: there is one reader.
        let requests = std::mem::replace(&mut config.requests, std::sync::mpsc::channel().1);
        // Asked for here, on the main thread and before the loop turns, so the
        // system's question — if it has one — is up while the globe warms.
        let mut locator = crate::location::Locator::platform();
        if let Some(placement) = config.here {
            locator.request(web_time::Instant::now(), placement);
        }
        let now = web_time::Instant::now();
        let mut app = Self {
            imagery: imagery::Imagery::new(config.layers, config.layer),
            locator,
            located: false,
            requests,
            flight: None,
            frame_host: config.frame_host.take(),
            preload: Vec::new(),
            overlays: crate::frame_host::OverlayLayers::default(),
            fixed_aspect: None,
            drawn: (0, false),
            notice: None,
            title_shown: String::new(),
            title_at: now,
            ended_by: None,
            remembers: config.remembers,
            controller: config.controller.clone(),
            layer_budget: Default::default(),
            title: config.title.clone(),
            sun: tuile_atmosphere::Sun::at_unix_seconds(config.lit_at_unix_seconds),
            air: config.air,
            active: None,
            session: None,
            launch: Some(launch),
            failed: None,
            messages: messages::Messages::default(),
            retries: None,
            launching_since: Some(now),
            lifecycle: lifecycle::Lifecycle::new(&config.profile),
            loading: loading::Loading::new(now),
            demand: demand::Demand::default(),
            touches: touch::Touches::default(),
            script: script::TouchScript::from_environment(),
            resolution: None,
            uploaded: 0,
            thermal: crate::profile::Thermal::Nominal,
            low_power: false,
            device_read: None,
            pressure: lifecycle::PressureWatch::default(),
            squeezed_at: None,
            refresh_hz: 60,
            pressure_read: None,
            pointer: input::Pointer::default(),
            views: input::Views::default(),
            recording: tape::Recording {
                tape: crate::recording::open_tape(),
                trace: crate::recording::open_trace(),
                finished: false,
            },
            warmup: warmup::Warmup {
                // Only a desktop holds its window shut; a phone opens at once
                // with the loading indicator over the globe.
                holding: config.profile.hold_window_until_warm,
                pinned_level: config.pinned_level,
                since: now,
                last_report: now,
            },
            pacing: pacing::Pacing {
                last_frame: now,
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
                last_log: now,
            },
            nav: NavWidget::new(),
            controls: config.controls,
            occluded: false,
            // Built with the window rather than here: a status item wants the
            // main thread and an event loop already running.
            statusbar: None,
            menu: None,
            stats: crate::session::Stats {
                started: Some(now),
                ..crate::session::Stats::default()
            },
            profile: config.profile,
            window_attributes: config.window,
            gpu_on_its_way: None,
            backends: config.backends,
            rig: Rig::MONO,
            speaker: crate::desktop::try_alert,
            #[cfg(feature = "control")]
            presented: 0,
        };
        app.retries = config.retries;
        // A desktop's launch has already arrived: adopt it before the first
        // turn of the loop, as the session always did.
        app.take_the_session();
        app
    }

    /// Takes the session if it has arrived since the last turn of the loop,
    /// or the reason it never will. Never waits.
    fn take_the_session(&mut self) {
        let now = web_time::Instant::now();
        // A start-up that failed, due again: on a phone, and only in front.
        if self.launch.is_none()
            && self.session.is_none()
            && self.lifecycle.phase() == lifecycle::Phase::Active
        {
            if let Some(launch) = self.retries.as_mut().and_then(|r| r.due(now)) {
                tracing::info!("trying to start the session again");
                self.launch = Some(launch);
                self.launching_since = Some(now);
            }
        }
        let Some(outcome) = self.launch.as_mut().and_then(crate::launch::Launch::poll) else {
            return;
        };
        self.launch = None;
        self.launching_since = None;
        match outcome {
            Ok(started) => {
                if let Some(retries) = self.retries.as_mut() {
                    retries.succeeded();
                }
                self.failed = None;
                self.adopt(started);
            }
            Err(why) => {
                crate::journal::note(&format!("session failed: {why}"));
                tracing::error!("the session could not start: {why}");
                let first = match self.retries.as_mut() {
                    Some(retries) => {
                        let wait = retries.failed(now);
                        tracing::info!(seconds = wait.as_secs(), "trying again later");
                        retries.failures() == 1
                    }
                    None => true,
                };
                // Said once, in a window: see `messages`. After that the
                // caption says it is still trying.
                if first {
                    self.messages
                        .say(format!("{} cannot start yet", self.title), why.clone());
                }
                self.failed = Some(why);
            }
        }
    }

    /// What the caption says while the session is not up and is being tried
    /// again: the explicit "offline, retrying" state.
    fn waiting_note(&self, now: web_time::Instant) -> Option<String> {
        self.failed.as_ref()?;
        let retries = self.retries.as_ref()?;
        Some(match retries.next() {
            Some(next) => format!(
                "cannot start — trying again in {} s",
                next.saturating_duration_since(now).as_secs() + 1
            ),
            None => "cannot start — trying again".to_owned(),
        })
    }

    /// Takes over a session that has come up: its stream, the relief the
    /// camera stands on, how many layers a drape may carry, and the budget's
    /// meter the pump records its frames into.
    fn adopt(&mut self, started: crate::shell::Started) {
        self.controller = self.controller.clone().with_ground(started.heights.clone());
        if let (Some(active), Some(meter)) = (self.active.as_mut(), started.meter()) {
            active.pump.observe(meter);
        }
        self.layer_budget = started.layer_budget.clone();
        self.fit_the_layer_budget();
        // Away already: the loads wait until the application is back.
        if self.lifecycle.phase() != lifecycle::Phase::Active {
            started.gate.shut();
        }
        if self.session.is_none() && !self.profile.hold_window_until_warm {
            tracing::info!(
                seconds = self
                    .stats
                    .started
                    .map_or(0.0, |t| t.elapsed().as_secs_f32()),
                "session up; the window was drawing all along"
            );
        }
        self.session = Some(started);
        self.demand.poke();
    }

    /// Tells the loader how many imagery layers the device binds, once both
    /// exist.
    fn fit_the_layer_budget(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        self.layer_budget.set_from_device(
            active
                .gpu
                .device
                .limits()
                .max_sampled_textures_per_shader_stage,
        );
    }

    /// Closes the session down: the stream, the store, the server. A phone, on
    /// its way out, is given seconds for all of it.
    pub fn shut_down(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let desktop = self.profile.hold_window_until_warm;
        let patience = std::time::Duration::from_secs(if desktop { 60 } else { 3 });
        session.close(patience, desktop);
    }

    /// A one-line summary of the session, for the exit log.
    pub fn report(&self) -> String {
        self.stats.to_string()
    }

    /// Moves the view if the location that was asked for has arrived, and
    /// says how the request ended either way.
    fn take_the_location(&mut self) {
        let now = web_time::Instant::now();
        if let Some(outcome) = self.locator.tick(now, &mut self.controller) {
            self.located |= matches!(outcome, crate::location::Outcome::Centred { .. });
            self.say(&outcome.to_string());
        }
    }

    /// Leaves the view this session ends on for the next one to open on.
    ///
    /// Only for a session that opens where the last one ended — a desktop's
    /// started from an icon, a page's unless it says where to open: from a
    /// terminal, the view a command opens on is what the command says and
    /// nothing else. And never a view centred on the current location — that
    /// one is not written anywhere, and the one from before is forgotten so
    /// that the next start does not pretend to return to it.
    fn remember_the_view(&self) {
        if !self.remembers {
            return;
        }
        let link = (!self.located)
            .then(|| crate::steer::link(&crate::steer::view_of(self.controller.target())));
        crate::shell::keep_the_view(link.as_deref());
    }

    /// Tells the person something they asked about, where they are looking.
    ///
    /// In the title bar, beside the position, because that is the one piece of
    /// text this window has and because the log may be going nowhere: a
    /// session started from an icon has no terminal to read. The message
    /// stays a few seconds, or until the next one replaces it.
    fn say(&mut self, message: &str) {
        tracing::info!("{message}");
        let now = web_time::Instant::now();
        self.notice = Some((message.to_owned(), now));
        // Shown on the next turn of the loop, not a quarter of a second on.
        self.title_at = now - std::time::Duration::from_secs(1);
    }

    /// Ends the session: says why, closes the tape, leaves the view for the
    /// next start, and closes the session where the loop will not return to
    /// do it — a phone's and a page's.
    ///
    /// Run as the loop unwinds, and by a page destroying its viewer, whose
    /// loop goes on for the next one. Written here and not after the loop
    /// returns: quitting from the application menu ends the process inside
    /// the loop, and what follows `run_app` never runs.
    pub(crate) fn end(&mut self) {
        crate::journal::note(&format!(
            "session ended: {}",
            self.ended_by.unwrap_or("the application was asked to quit")
        ));
        self.close_the_tape();
        self.remember_the_view();
        if !self.profile.hold_window_until_warm {
            self.shut_down();
        }
    }

    /// The view has gone off screen, or come back: a covered window, a phone
    /// in the background, a hidden tab.
    pub(crate) fn hidden(&mut self, hidden: bool) {
        // Possibly the last moment the application runs: a tab that is
        // closed is hidden first, and a phone ends what it put away.
        if hidden {
            self.remember_the_view();
        }
        // Rendering while covered leaks GPU memory on Apple platforms (see
        // `render`), and a covered surface does not block on vsync, so the
        // loop would free-run and leak all the faster. Where the platform asks
        // more of an application away — holding loads, trimming — the
        // lifecycle says what that takes.
        if self.profile.pause_loads_when_hidden {
            let steps = if hidden {
                self.lifecycle.background()
            } else {
                self.lifecycle.foreground()
            };
            self.carry_out(steps);
        } else {
            self.occluded = hidden;
            if let Some(active) = self.active.as_ref() {
                active.screen.request_redraw();
            }
        }
    }

    /// Asks for a frame now, whatever the pacing was waiting for.
    pub(crate) fn draw_soon(&mut self) {
        self.demand.poke();
        if let Some(active) = self.active.as_ref() {
            active.screen.request_redraw();
        }
    }

    /// The eyes this frame is drawn for, each a camera of its own: the
    /// camera itself, for a window.
    fn eyes(&self) -> Vec<tuile_camera::GlobeCamera> {
        self.rig.cameras(&self.controller.camera)
    }

    /// Each eye's view, for the traversal: what it is asked to select for.
    fn eye_views(&self) -> Vec<tuile_core::traversal::ViewState> {
        let (width, height) = self.viewport();
        let viewport = glam::DVec2::new(width, height);
        // The display's density: the need is counted in logical pixels
        // (D-L1), so a window moved between displays asks for nothing new.
        let scale = self.active.as_ref().map_or(1.0, |a| a.scale);
        let mut views: Vec<_> = self
            .eyes()
            .iter()
            .map(|eye| eye.view_state(viewport).with_scale(scale))
            .collect();
        // A host's preload views join the union: traversed, so their tiles
        // are asked for, and never drawn — nothing downstream reads past the
        // eyes. They are added after, so an empty list leaves the selection
        // exactly as it was and a session without a host is untouched.
        views.extend(self.preload.iter().map(|view| {
            // The height is the drawn frame's; only the shape differs, and a
            // view's need is counted in pixels.
            let size = glam::DVec2::new(height * view.aspect.max(1e-6), height);
            view.pose.view_state(size).with_scale(scale)
        }));
        views
    }

    /// The application is back in front: the lifecycle's way back.
    pub(crate) fn come_back(&mut self) {
        let steps = self.lifecycle.activate();
        self.carry_out(steps);
        // Back in front: a start-up that failed is tried at once — the
        // person may have just typed a token in the system's settings.
        if let Some(retries) = self.retries.as_mut() {
            retries.foreground(web_time::Instant::now());
        }
    }

    /// The application is about to leave the front — and on Android, its
    /// surface is being destroyed.
    pub(crate) fn step_away(&mut self) {
        let steps = if self.profile.suspend_takes_the_surface {
            self.lifecycle.surface_lost()
        } else {
            self.lifecycle.resign()
        };
        self.carry_out(steps);
    }

    /// The system says it is short of memory.
    pub(crate) fn short_of_memory(&mut self) {
        tracing::warn!("memory warning; the budget shrinks");
        crate::journal::note("memory warning");
        self.squeezed_at = Some(web_time::Instant::now());
        let steps = self.lifecycle.memory_warning();
        self.carry_out(steps);
    }

    /// The system says how short of memory it is, as Android grades it.
    #[cfg(feature = "control")]
    pub(crate) fn under_pressure(&mut self, pressure: crate::profile::Pressure) {
        self.squeezed_at = Some(web_time::Instant::now());
        let steps = self.lifecycle.memory_pressure(pressure);
        self.carry_out(steps);
    }

    /// One turn of the loop, short of drawing: what has arrived is taken,
    /// what is due is done, and a frame is asked for when one is due. What
    /// the loop should do next is the answer.
    ///
    /// The window's loop runs this from `about_to_wait`, and draws on the
    /// redraw it asks for; a headless run runs it on every step, and draws
    /// the same way.
    pub(crate) fn turn(&mut self) -> Turn {
        // Before anything that can return early: an answer about the current
        // location is taken on whatever turn of the loop it arrives, hidden
        // and warming included.
        self.take_the_session();
        self.take_the_gpu();
        self.messages.offer(
            self.active.is_some() || self.ended_by.is_some(),
            self.speaker,
        );
        if self.ended_by.is_some() && self.profile.owns_its_window {
            return Turn::End;
        }
        self.take_the_location();
        self.take_the_commands();
        self.take_the_imagery();
        // Also on the turns that draw nothing — hidden and warming, or
        // occluded — so a script always has a view to read.
        self.publish_the_view(None);
        // A flown tape ends the session. Without this a replay leaves a window
        // sitting on its last frame, and the run has to be closed by hand —
        // which is exactly what stops anyone from putting it in a script.
        if self.recording.finished {
            self.ended_by = Some("the replayed tape reached its end");
            tracing::info!("{}", self.report());
            self.close_the_tape();
            return Turn::End;
        }
        // Ctrl-C or `kill`: unwind rather than die, so the recording is closed.
        if crate::signals::INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            self.ended_by = Some("a signal asked it to stop (interrupt, terminate or hang-up)");
            tracing::info!("interrupted; closing the session");
            self.close_the_tape();
            return Turn::End;
        }
        // Hidden and warming: the window is not drawing, so nothing else is
        // draining the stream — and the uploads being waited for happen in the
        // pump. Driving it from here is what keeps the wait from being a
        // deadlock.
        if self.warmup.holding {
            self.pump_while_hidden();
            return Turn::Flow(ControlFlow::Poll);
        }
        // A phone draws on demand, and only in the foreground.
        if self.profile.render_on_demand {
            return Turn::Flow(self.pace_the_frames());
        }
        if !self.occluded {
            if let Some(active) = self.active.as_ref() {
                active.screen.request_redraw();
            }
        }
        Turn::Flow(if self.occluded {
            ControlFlow::Wait
        } else {
            ControlFlow::Poll
        })
    }

    fn viewport(&self) -> (f64, f64) {
        self.active
            .as_ref()
            .map_or((1920.0, 1080.0), |a| (a.size.0 as f64, a.size.1 as f64))
    }

    /// Where the on-screen controls live this frame. See [`UiFrame`].
    fn ui_frame(&self) -> UiFrame {
        let (w, h) = self.viewport();
        let Some(active) = self.active.as_ref() else {
            return UiFrame {
                origin: (0.0, 0.0),
                scale: 1.0,
                size: (w, h),
            };
        };
        if self.profile.hold_window_until_warm {
            // A desktop: as it always was.
            return UiFrame {
                origin: (0.0, 0.0),
                scale: 1.0,
                size: (w, h),
            };
        }
        let (x, y, sw, sh) = active.safe;
        let scale = active.scale.max(1.0);
        UiFrame {
            origin: (x, y),
            scale,
            size: (sw / scale, sh / scale),
        }
    }
}

impl ApplicationHandler for App {
    /// The first time, the window; after that — on a phone, every time the
    /// application comes back to the front — the lifecycle's way back.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            self.come_back();
            return;
        }
        self.start_window(event_loop);
    }

    /// A phone's application is about to leave the front — and on Android,
    /// its surface is being destroyed: everything drawn into it must be let
    /// go of before this returns.
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.step_away();
    }

    /// The system is short of memory: the server lets go of what the view
    /// does not draw. Never what it draws, nor its ancestors.
    fn memory_warning(&mut self, _event_loop: &ActiveEventLoop) {
        self.short_of_memory();
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
        self.end();
    }

    /// Drives the animation: one redraw per loop iteration while the window is
    /// visible. Requesting from here rather than from the `RedrawRequested`
    /// handler is what lets an occluded window stop cleanly — the loop parks on
    /// `Wait` until the next event instead of spinning.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.turn() {
            Turn::End => event_loop.exit(),
            Turn::Flow(flow) => event_loop.set_control_flow(flow),
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

/// The view a host's world overlays are drawn through: the camera's own, at
/// the picture's aspect, against the render origin every tile was prepared
/// with — the eye.
///
/// `clearance` is the height above the **ground**, as the tiles' own view
/// uniform is given it: the near plane comes from it, and a near plane of
/// its own would put an overlay and the ridge it stands against a pixel
/// apart at every edge. One projection, both.
///
/// The camera's axes go with it because the overlays' light goes with the
/// camera, as the film's does: `tuile_overlay::gpu::LIGHT_WGSL` is the same
/// text in both, so a shape's relief comes out the same way.
fn world_overlay_view(
    camera: tuile_camera::GlobeCamera,
    aspect: f64,
    clearance: f64,
) -> tuile_wgpu::WorldOverlayView {
    let origin = camera.position;
    let view_proj = glam::Mat4::from_cols_array(&camera.view_proj_with_clearance(
        origin,
        aspect as f32,
        clearance,
    ));
    // The axes the light is built on, orthonormalised from the pose: the
    // film takes the same three from its own frame.
    let fwd = camera.direction.as_vec3().normalize_or_zero();
    let right = fwd.cross(camera.up.as_vec3()).normalize_or_zero();
    tuile_wgpu::WorldOverlayView::new(view_proj, right, right.cross(fwd))
}
