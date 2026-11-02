// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer headless, and the API that drives it — for tests, and for
//! debug builds a device is driven through.
//!
//! # The same viewer, the same world
//!
//! [`Headless`] is the application every window runs, on the session every
//! window runs:
//!
//! - **the sources** are the host's ([`Start::host`]): the real terrain and
//!   imagery, asked over the network the way the application asks them;
//! - **the store** is the tile store on disk, in the directory a run names
//!   ([`Start::store`]): a fresh one is a cold start, one an earlier run left
//!   is a warm one;
//! - **the session** is brought up as the application brings it up: the
//!   runtime and its threads, the geometry server on a thread of its own, the
//!   loads as tasks;
//! - **time** is the wall's: a step is one refresh of the display the run
//!   says it has ([`Start::refresh_hz`]), and a step that runs long misses
//!   refreshes as a frame on a screen does;
//! - **the device** is said, not asked: a desktop's numbers or a phone's,
//!   from a memory a test states ([`Device`]).
//!
//! Only the ends differ from a window: the frame is drawn into a texture per
//! eye ([`Eyes`]) and read back after every frame, and every eye is scanned
//! for ground left the clear colour.
//!
//! # Reading it
//!
//! Through the numbers the engine publishes — `tuile_core::metrics`, the
//! series `tuile-metrics` exports — never through accessors of its own. A
//! [`State`] is one step's: the view a script reads (the scripting
//! snapshot), the metrics as a [`Snapshot`] since the run began, what each
//! eye's scan found, and what every [`Observer`] added.
//!
//! # The session log
//!
//! A run may write itself down as one MCAP file ([`Start::log`]), on the
//! timeline a tape keeps: the camera on `/camera` — a tape, which
//! `TUILE_REPLAY` and [`Headless::fly`] fly again — each step's [`State`] on
//! `/state`, each observer on its own topic and, as [`Start::pictures`]
//! says, the eyes' pictures: on `/frame` for one eye, which
//! `tuile-scan-black` reads, or `/eye/<n>/frame`.
//!
//! # Driving it
//!
//! [`Action`]s — a jump or a glide of the eye (a steering command, on the
//! queue a URL arrives on), a finger, the wheel, a resize, the density,
//! memory pressure, the background — one at a time with [`Headless::act`],
//! or as a [`Series`] timed in seconds ([`Headless::play`]); a tape with
//! [`Headless::fly`]. [`patterns`] holds the motion patterns of the budget's
//! specification, and [`verdict`] holds a run — live, or read back from its
//! session log — to the specification's obligations. The runner that plays
//! the patterns on the real catalog is the host's: `examples/wgpu-viewer`,
//! `cargo run --example headless`.
//!
//! # The socket
//!
//! [`Headless::listen`] serves the same commands to other programs: JSON, one
//! request a line, on a loopback address only — see [`socket`].
//!
//! One headless viewer per process at a time: the numbers it reads are the
//! process's.

mod log;
pub mod patterns;
mod series;
pub mod socket;
pub mod verdict;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use web_time::Instant;

pub use log::{BudgetTopics, Observer, Pictures};
pub use series::{Action, Finger, Pressure, Series};
pub use tuile_core::metrics::Snapshot;
pub use tuile_wgpu::{compression_named, ImageryCompression};
pub use verdict::{Obligations, Report, Verdict};

use crate::app::{App, Rig, Turn, ViewerConfig};
use crate::embed::ViewerHost;
use crate::steer::{Command, Goto, Request};

/// Where the eye is and how it looks, as the command line and a steering URL
/// say it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Degrees, east positive.
    pub lon: f64,
    /// Degrees, north positive.
    pub lat: f64,
    /// Metres above the ellipsoid.
    pub altitude: f64,
    /// Degrees clockwise from north.
    pub heading: f64,
    /// Degrees below the horizon; 90 looks straight down.
    pub pitch: f64,
}

impl Default for View {
    /// Straight down at France from 2 000 km: what the viewer opens on.
    fn default() -> Self {
        crate::start::StartView::default().into()
    }
}

impl From<crate::start::StartView> for View {
    fn from(v: crate::start::StartView) -> Self {
        Self {
            lon: v.lon,
            lat: v.lat,
            altitude: v.altitude,
            heading: v.heading,
            pitch: v.pitch,
        }
    }
}

impl From<View> for crate::start::StartView {
    fn from(v: View) -> Self {
        Self {
            lon: v.lon,
            lat: v.lat,
            altitude: v.altitude,
            heading: v.heading,
            pitch: v.pitch,
        }
    }
}

impl View {
    /// The steering command that puts the eye here: at once, or gliding over
    /// `seconds`.
    pub(crate) fn command(&self, seconds: Option<f64>) -> Command {
        Command::Goto(Goto {
            lon: Some(self.lon),
            lat: Some(self.lat),
            altitude: Some(self.altitude),
            heading: Some(self.heading),
            pitch: Some(self.pitch),
            seconds,
            time: None,
        })
    }

    pub(crate) fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "lon": self.lon,
            "lat": self.lat,
            "altitude": self.altitude,
            "heading": self.heading,
            "pitch": self.pitch,
        })
    }
}

/// The eyes a frame is drawn for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Eyes {
    /// One, as a window has.
    Mono,
    /// A headset's pair, `separation` metres apart — 0.063 for a person.
    Stereo { separation: f64 },
}

impl Eyes {
    fn rig(self) -> Rig {
        match self {
            Self::Mono => Rig::MONO,
            Self::Stereo { separation } => Rig {
                eyes: 2,
                separation,
            },
        }
    }

    /// How many.
    pub fn count(self) -> usize {
        self.rig().eyes
    }
}

/// The machine the run is to behave as, said rather than asked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Device {
    /// The desktop application, as it always runs.
    Desktop,
    /// A phone or a tablet with `memory` bytes, `available` of them still
    /// left to the process when the system says, and `cores` processors.
    Handheld {
        memory: u64,
        available: Option<u64>,
        cores: usize,
    },
    /// The same, on a system that takes the surface away whenever the
    /// application leaves the front: Android.
    Android {
        memory: u64,
        available: Option<u64>,
        cores: usize,
    },
}

impl Device {
    /// An iPhone of today: 6 GiB, half of it left to the process.
    pub const PHONE: Self = Self::Handheld {
        memory: 6 << 30,
        available: Some(3 << 30),
        cores: 6,
    };

    /// A phone the system has left little: the budget's floor.
    pub const LOW_MEMORY_PHONE: Self = Self::Handheld {
        memory: 2 << 30,
        available: Some(400 << 20),
        cores: 4,
    };

    pub(crate) fn profile(self) -> crate::profile::Profile {
        use crate::profile::{DeviceMemory, Profile};
        let memory = |physical, available| DeviceMemory {
            physical,
            available,
        };
        match self {
            Self::Desktop => Profile::desktop(),
            Self::Handheld {
                memory: m,
                available,
                cores,
            } => Profile::handheld(memory(m, available), cores),
            Self::Android {
                memory: m,
                available,
                cores,
            } => Profile {
                suspend_takes_the_surface: true,
                ..Profile::handheld(memory(m, available), cores)
            },
        }
    }
}

/// The numbers the run's profile holds it to, as today's viewer has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The GPU memory the session's graphics budget is sized from, bytes:
    /// the sum of its classes' hard limits, which nothing it holds crosses.
    pub resident_bytes: u64,
    /// The budget's floor, the level loaded first and never evicted.
    pub pinned_level: Option<u32>,
}

/// What a run starts from.
#[derive(Clone)]
pub struct Start {
    /// Who the application is and where its ground comes from: the
    /// embedding host's three answers, as a window's session has them.
    pub host: Arc<dyn ViewerHost>,
    /// The tile store's directory on disk. A fresh directory is a cold
    /// start; one an earlier run left is a warm one.
    pub store: PathBuf,
    pub view: View,
    /// One eye's frame, in device pixels.
    pub size: (u32, u32),
    /// Device pixels per point: 2 on a Retina panel, 3 on a phone.
    pub scale: f64,
    pub eyes: Eyes,
    pub device: Device,
    /// The display's refresh, frames a second: a step is one refresh.
    pub refresh_hz: f64,
    /// The instant the scene is lit for, UTC seconds.
    pub lit_at: f64,
    /// Where the session log is written, if anywhere: see the module.
    pub log: Option<PathBuf>,
    /// Which pictures go into the log.
    pub pictures: Pictures,
    /// How imagery is held on the GPU: off, as a window's session holds it
    /// unless the person asks.
    pub imagery_compression: tuile_wgpu::ImageryCompression,
    /// The host's 3D Tiles tilesets drawn over the globe, by asset number:
    /// none unless a run asks (`--tileset` in a window).
    pub tilesets: Vec<i64>,
}

/// Midsummer noon over Greenwich, 2026: the sun high over Europe.
pub const MIDSUMMER_NOON: f64 = 1_782_043_200.0;

impl Start {
    /// A desktop's window over France, from `host`, its store in `store`.
    pub fn new(host: Arc<dyn ViewerHost>, store: impl Into<PathBuf>) -> Self {
        Self {
            host,
            store: store.into(),
            view: View::default(),
            size: (1280, 800),
            scale: 2.0,
            eyes: Eyes::Mono,
            device: Device::Desktop,
            refresh_hz: 60.0,
            lit_at: MIDSUMMER_NOON,
            log: None,
            pictures: Pictures::WhenWrong,
            imagery_compression: tuile_wgpu::ImageryCompression::Off,
            tilesets: Vec::new(),
        }
    }
}

impl std::fmt::Debug for Start {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Start")
            .field("store", &self.store)
            .field("view", &self.view)
            .field("size", &self.size)
            .field("scale", &self.scale)
            .field("eyes", &self.eyes)
            .field("device", &self.device)
            .field("refresh_hz", &self.refresh_hz)
            .field("log", &self.log)
            .field("imagery_compression", &self.imagery_compression)
            .finish_non_exhaustive()
    }
}

/// Why a run did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// There is no GPU to draw with.
    NoGpu(String),
    /// Another headless viewer is running in this process.
    Busy,
    /// The session could not be brought up: the host's own words.
    Session(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoGpu(why) => write!(f, "no GPU to draw with: {why}"),
            Self::Busy => f.write_str("another headless viewer runs in this process"),
            Self::Session(why) => write!(f, "the session did not start: {why}"),
        }
    }
}

impl std::error::Error for Refused {}

/// What one eye's scan found: ground pixels, and those left the clear colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EyeReading {
    pub ground: u64,
    pub black: u64,
}

/// One step, read: see the module.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    /// Steps since the run began, this one counted.
    pub frame: u64,
    /// Seconds since the run began, at the end of this step.
    pub seconds: f64,
    /// How long this step's frame took, seconds: turn, draw and read-back.
    pub took: f64,
    /// Whether this step drew a frame anybody could see.
    pub drew: bool,
    /// The view, as a script reads it.
    pub view: View,
    /// One eye's frame, device pixels, and the density.
    pub size: (u32, u32),
    pub scale: f64,
    /// What each eye's scan of the last frame drawn found.
    pub eyes: Vec<EyeReading>,
    /// The engine's numbers since the run began.
    pub metrics: Snapshot,
    /// What each observer read, by topic.
    pub observed: BTreeMap<String, serde_json::Value>,
}

impl State {
    /// As the session log and the socket write it.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "frame": self.frame,
            "seconds": self.seconds,
            "took": self.took,
            "drew": self.drew,
            "view": self.view.to_json(),
            "size": [self.size.0, self.size.1],
            "scale": self.scale,
            "eyes": self.eyes.iter().map(|e| serde_json::json!({
                "ground": e.ground,
                "black": e.black,
            })).collect::<Vec<_>>(),
            "metrics": serde_json::to_value(self.metrics).unwrap_or_default(),
            "observed": self.observed,
        })
    }

    /// The most pixels of ground any eye saw left the clear colour.
    pub fn black(&self) -> u64 {
        self.eyes.iter().map(|e| e.black).max().unwrap_or(0)
    }

    /// Whether this frame broke what every frame is held to: ground left
    /// black in an eye, or selected ground drawn by nothing.
    pub fn wrong(&self) -> bool {
        self.drew && (self.black() > 0 || self.metrics.frame_lost > 0)
    }

    /// Whether the picture is what the selection asked for: every selected
    /// tile drawn by itself, and nothing standing in — the specification's
    /// convergence (I3), short of the digest holding still, which
    /// [`Headless::settle`] watches over frames.
    pub fn complete(&self) -> bool {
        let m = &self.metrics;
        self.drew && m.frame_exact > 0 && m.frame_lost == 0 && m.frame_coarser == 0
    }

    /// Whether nothing is loading and nothing waits to be uploaded.
    pub fn idle(&self) -> bool {
        self.metrics.loads_in_flight == 0 && self.metrics.pending_uploads == 0
    }
}

/// A recorded frame drawn again ([`Headless::replay`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Replayed {
    /// The run's state once the frame was drawn for the last time.
    pub state: State,
    /// Whether the picture was complete and nothing loading before the
    /// bound.
    pub settled: bool,
    /// How many times the frame was drawn.
    pub draws: u32,
    /// The scene instant it was lit for, and the wall instant it was drawn
    /// at: the recording's.
    pub instant: f64,
    pub wall: f64,
    /// What the atmosphere lit it with.
    pub sky: Option<tuile_tape::session::SkyState>,
}

/// What a wait came to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settled {
    /// Steps taken, and the seconds they took.
    pub frames: u64,
    pub seconds: f64,
    /// Whether the picture settled within the bound.
    pub settled: bool,
}

/// Frames drawn the picture must hold still, complete, to count as settled.
const STILL_FOR: usize = 3;

/// Whether a headless viewer is running in this process.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// The process's one headless viewer, claimed: let go of when dropped,
/// however the run ends — a start that fails or panics included.
struct Claim;

impl Claim {
    fn take() -> Option<Self> {
        (!RUNNING.swap(true, Ordering::SeqCst)).then_some(Self)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// The viewer, headless: see the module.
pub struct Headless {
    app: App,
    requests: Sender<Request>,
    /// The session's: its loads, its store, its server's thread.
    runtime: Option<tokio::runtime::Runtime>,
    /// One refresh of the display.
    interval: Duration,
    began: Instant,
    /// When the next refresh is due.
    due: Instant,
    /// How long bringing the run up took, seconds: the GPU, and — on a
    /// desktop, which waits for it — the session.
    launched: f64,
    frame: u64,
    baseline: Snapshot,
    start: Start,
    limits: Limits,
    history: Vec<State>,
    observers: Vec<Box<dyn Observer>>,
    log: Option<log::SessionLog>,
    socket: Option<socket::Listener>,
    /// Last, so it is let go of once everything else has gone.
    _claim: Claim,
}

impl Headless {
    /// Brings a run up as the application brings itself up: the GPU, the
    /// host's sources, the tile store, the session on its runtime — and the
    /// application on its offscreen eyes.
    ///
    /// A desktop waits for its session before its first frame, as its window
    /// does; a phone collects it from its loop, drawing meanwhile.
    pub fn start(start: Start) -> Result<Self, Refused> {
        let claim = Claim::take().ok_or(Refused::Busy)?;
        Self::bring_up(start, claim)
    }

    fn bring_up(start: Start, claim: Claim) -> Result<Self, Refused> {
        let launching = Instant::now();
        let gpu = pollster::block_on(tuile_wgpu::GpuContext::headless_with(
            start.imagery_compression,
        ))
        .map_err(|e| Refused::NoGpu(e.to_string()))?;
        // The process's identity and layers, as `main` adopts the host's: the
        // first host wins, and a test process has one.
        crate::embed::adopt(start.host.identity(), start.host.imagery());
        let layers = crate::embed::layers();
        let profile = start.device.profile();
        let budget = profile.budget();
        let limits = Limits {
            resident_bytes: profile.gpu_bytes,
            pinned_level: budget.pinned_level,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(profile.runtime_workers)
            .max_blocking_threads(profile.blocking_threads)
            .thread_name("tuile-runtime")
            .enable_all()
            .build()
            .map_err(|e| Refused::Session(e.to_string()))?;
        let handle = runtime.handle().clone();
        let pinned = budget.pinned_level;
        let plan = crate::native::launch::Plan {
            host: Arc::clone(&start.host),
            terrain: start.host.terrain(),
            layers,
            opening: 0,
            store: start.store.clone(),
            config: profile.traversal(),
            budget,
            profile: profile.clone(),
            timeout: crate::launch::START_TIMEOUT,
            imagery_plan: gpu.imagery_plan(),
            // The same choice on the same adapter, for tiles stored compressed.
            stored_imagery: pollster::block_on(tuile_wgpu::adapter_plan(
                start.imagery_compression,
                None,
            ))
            .1,
            tilesets: start.tilesets.clone(),
        };
        let launch = crate::native::launch::start(&handle, plan.clone());
        let launch = if profile.hold_window_until_warm {
            crate::launch::Launch::finished(Ok(launch.wait().map_err(Refused::Session)?))
        } else {
            launch
        };
        let retries = (!profile.hold_window_until_warm).then(|| {
            let handle = handle.clone();
            crate::launch::Retries::new(move || crate::native::launch::start(&handle, plan.clone()))
        });
        let (requests, queue) = std::sync::mpsc::channel();
        let config = ViewerConfig {
            // A scripted session drives through the control API's own
            // commands, not through a frame host.
            frame_host: None,
            controller: tuile_camera::CameraController::new(
                crate::start::StartView::from(start.view).camera(),
            )
            .with_min_altitude(1.0),
            title: crate::embed::identity().name.clone(),
            layers,
            layer: 0,
            here: None,
            requests: queue,
            remembers: false,
            // A stated instant, held: two runs of a scenario are lit alike.
            clock: tuile_atmosphere::ClockSettings {
                start: tuile_atmosphere::Start::At(start.lit_at),
                offset: 0.0,
                rate: 0.0,
            },
            air: tuile_atmosphere::AirTuning::default(),
            pinned_level: pinned,
            profile,
            retries,
            window: winit::window::Window::default_attributes(),
            controls: true,
            backends: None,
            imagery_compression: start.imagery_compression,
            // A headless run writes its own session log (`Start::log`).
            record: Default::default(),
            // The run's, all on, and a switch never kept: a run leaves the
            // person's choice alone.
            tilesets: Vec::new(),
            tilesets_on: start.tilesets.clone(),
            keeps_tilesets: false,
        };
        let mut app = App::new(config, launch);
        app.speak_through(said);
        app.start_offscreen(gpu, start.size, start.scale, start.eyes.rig());
        let log = match &start.log {
            Some(path) => Some(
                log::SessionLog::open(path, start.pictures, start.eyes.count())
                    .map_err(Refused::Session)?,
            ),
            None => None,
        };
        let m = tuile_core::metrics::metrics();
        // An eye this run has no reading for yet reads nothing, not the last
        // run's.
        m.eye_ground_pixels.clear();
        m.eye_black_pixels.clear();
        let began = Instant::now();
        let mut app = app;
        // Each frame's upload deadline is the display's the run says it has.
        app.refresh_hz = (start.refresh_hz.round() as u32).clamp(1, 1000);
        Ok(Self {
            app,
            requests,
            runtime: Some(runtime),
            interval: Duration::from_secs_f64(1.0 / start.refresh_hz.max(1.0)),
            began,
            due: began,
            launched: launching.elapsed().as_secs_f64(),
            frame: 0,
            baseline: m.snapshot(),
            limits,
            start,
            history: Vec::new(),
            // The budget's account of every step, in every run's log.
            observers: vec![Box::new(BudgetTopics)],
            log,
            socket: None,
            _claim: claim,
        })
    }

    /// Ends this run — the session closed, its store written down — and
    /// starts another over the same store: the application killed and
    /// opened again, warm. The new run writes `log`, if anywhere.
    pub fn relaunch(self, log: Option<PathBuf>) -> Result<Self, Refused> {
        let start = Start {
            log,
            ..self.start.clone()
        };
        drop(self);
        Self::start(start)
    }

    /// The numbers the run's profile holds it to.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// What the run started from.
    pub fn started_from(&self) -> &Start {
        &self.start
    }

    /// How long bringing the run up took, seconds: the GPU, and — on a
    /// desktop, which waits for it before its first frame, as its window
    /// does — the session: the host's sources, the store, the globe.
    pub fn launched(&self) -> f64 {
        self.launched
    }

    /// Seconds since the run began.
    pub fn seconds(&self) -> f64 {
        self.began.elapsed().as_secs_f64()
    }

    /// One refresh: waits for it, turns the loop, draws the frame the turn
    /// asks for, reads it back and observes it.
    ///
    /// A frame that takes longer than a refresh misses the next ones, as on
    /// a screen: the step after it waits for the refresh after it ends.
    pub fn step(&mut self) -> &State {
        let now = Instant::now();
        if self.due > now {
            std::thread::sleep(self.due - now);
        }
        let began = Instant::now();
        let drew = match self.app.turn() {
            Turn::End => false,
            Turn::Flow(_) => self.app.render_if_asked(),
        };
        self.app.land();
        let ended = Instant::now();
        // The next refresh after this one ended.
        let interval = self.interval.as_secs_f64();
        let refreshes = (ended.duration_since(self.began).as_secs_f64() / interval).floor() + 1.0;
        self.due = self.began + Duration::from_secs_f64(refreshes * interval);
        self.frame += 1;
        let mut state = self.read(drew, ended.duration_since(began).as_secs_f64());
        for observer in &mut self.observers {
            let reading = observer.observe(&state);
            state.observed.insert(observer.topic().to_owned(), reading);
        }
        if let Some(log) = self.log.as_mut() {
            log.write(&state, &self.app);
        }
        self.history.push(state);
        self.answer_the_socket();
        self.history
            .last()
            .unwrap_or_else(|| unreachable!("a step was just recorded"))
    }

    /// `n` refreshes.
    pub fn steps(&mut self, n: u64) -> &State {
        for _ in 1..n {
            self.step();
        }
        self.step()
    }

    /// Steps for `seconds` of the run's time.
    pub fn run_for(&mut self, seconds: f64) -> &State {
        let until = self.seconds() + seconds;
        loop {
            self.step();
            if self.seconds() >= until {
                break;
            }
        }
        self.history
            .last()
            .unwrap_or_else(|| unreachable!("a step was just recorded"))
    }

    /// Steps until the picture is complete and has held still for a few
    /// frames, or `bound` seconds have passed.
    pub fn settle(&mut self, bound: f64) -> Settled {
        let (from, frames) = (self.seconds(), self.frame);
        loop {
            self.step();
            let seconds = self.seconds() - from;
            let settled = self.still();
            if settled || seconds >= bound {
                return Settled {
                    frames: self.frame - frames,
                    seconds,
                    settled,
                };
            }
        }
    }

    /// Steps until `holds` of the state, or `bound` seconds have passed;
    /// says how long it took, or that it did not.
    pub fn wait_until(&mut self, bound: f64, holds: impl Fn(&State) -> bool) -> Result<f64, f64> {
        let from = self.seconds();
        loop {
            let held = holds(self.step());
            let seconds = self.seconds() - from;
            if held {
                return Ok(seconds);
            }
            if seconds >= bound {
                return Err(seconds);
            }
        }
    }

    /// Whether the last few frames drawn were complete and drew the same
    /// tiles: converged, as the specification has it (I3). Frames, not
    /// steps: a phone at rest draws four times a second, and the steps
    /// between draw nothing. Loads may still be in flight — what is
    /// prefetched beside the view is not the picture; [`State::idle`] says
    /// whether they are.
    fn still(&self) -> bool {
        let tail: Vec<&State> = self
            .history
            .iter()
            .rev()
            .filter(|s| s.drew)
            .take(STILL_FOR)
            .collect();
        tail.len() == STILL_FOR
            && tail.iter().all(|s| s.complete())
            && tail
                .windows(2)
                .all(|w| w[0].metrics.frame_digest == w[1].metrics.frame_digest)
    }

    /// Does `action` now, before the next step.
    pub fn act(&mut self, action: &Action) {
        series::carry_out(self, action);
    }

    /// Plays `series` in the run's time: before each step, every action due
    /// by the refresh it waits for; then the step. Returns the states it
    /// produced.
    pub fn play(&mut self, series: &Series) -> &[State] {
        let first = self.history.len();
        let from = self.due.max(Instant::now());
        let mut next = 0;
        let actions = series.actions();
        loop {
            let at = self
                .due
                .max(Instant::now())
                .duration_since(from)
                .as_secs_f64();
            while let Some((when, action)) = actions.get(next) {
                if *when > at {
                    break;
                }
                self.act(action);
                next += 1;
            }
            self.step();
            if next == actions.len() && at >= series.seconds() {
                break;
            }
        }
        &self.history[first..]
    }

    /// Draws one frame of a recorded session again (`tuile_tape::session`):
    /// its camera, lit at its own scene instant from its own wall instant,
    /// through the renderer a window draws with — and draws it again, the
    /// camera and the clocks held, until the picture is complete and nothing
    /// is loading, or `bound` seconds have passed.
    ///
    /// No refresh is waited for: the clocks are the recording's, not the
    /// wall's, so the frame is the same frame however long it takes to
    /// load. A frame recorded without its context (a plain tape) is lit at
    /// the run's own instant ([`Start::lit_at`]), held.
    pub fn replay(&mut self, frame: &tuile_tape::session::Recorded, bound: f64) -> Replayed {
        let (wall, instant, rate) = frame
            .context
            .map_or((self.start.lit_at, self.start.lit_at, 0.0), |c| {
                (c.wall, c.instant, c.rate)
            });
        self.app.hold(Some(crate::app::Replayed {
            camera: frame.camera,
            wall,
            instant,
            rate,
        }));
        let began = Instant::now();
        let mut draws = 0;
        let mut drew;
        let settled = loop {
            let turned = Instant::now();
            drew = match self.app.turn() {
                Turn::End => false,
                Turn::Flow(_) => self.app.draw_now(),
            };
            draws += u32::from(drew);
            let state = self.read(drew, turned.elapsed().as_secs_f64());
            if state.complete() && state.idle() {
                break true;
            }
            if began.elapsed().as_secs_f64() >= bound {
                break false;
            }
            if !drew {
                // Nothing to draw yet — the session still coming up: give
                // its threads the time a refresh would.
                std::thread::sleep(self.interval);
            }
        };
        self.app.hold(None);
        self.frame += 1;
        let mut state = self.read(drew, began.elapsed().as_secs_f64());
        for observer in &mut self.observers {
            let reading = observer.observe(&state);
            state.observed.insert(observer.topic().to_owned(), reading);
        }
        if let Some(log) = self.log.as_mut() {
            log.write(&state, &self.app);
        }
        self.history.push(state.clone());
        let (lit, drawn_at) = self.app.lit();
        Replayed {
            state,
            settled,
            draws,
            instant: lit,
            wall: drawn_at,
            sky: self.app.sky(),
        }
    }

    /// Flies a recorded camera path, one camera a frame, as `TUILE_REPLAY`
    /// does; the run goes on from where it ends.
    pub fn fly(&mut self, tape: tuile_tape::Tape) {
        self.app.fly(tape);
    }

    /// Whether a tape given to [`Headless::fly`] still has cameras to fly.
    pub fn flying(&self) -> bool {
        self.app.flying()
    }

    /// The last step's state, or the state before the first.
    pub fn state(&self) -> State {
        self.history
            .last()
            .cloned()
            .unwrap_or_else(|| self.read(false, 0.0))
    }

    /// Every step's state, in order.
    pub fn history(&self) -> &[State] {
        &self.history
    }

    /// The picture eye `eye` last drew: tightly packed RGBA8, sRGB, and its
    /// size.
    pub fn picture(&self, eye: usize) -> Option<(&[u8], (u32, u32))> {
        self.app.eye_pixels(eye)
    }

    /// Each eye's camera, as the last frame drew it — what its picture is
    /// scanned against.
    pub fn eye_cameras(&self) -> Vec<tuile_tape::Frame> {
        self.app
            .eye_cameras()
            .iter()
            .map(crate::app::offscreen::frame_of)
            .collect()
    }

    /// Frames that reached the eyes since the run began.
    pub fn frames_drawn(&self) -> u64 {
        self.app.frames_presented()
    }

    /// Adds a source of readings: a topic in the session log, a key in every
    /// [`State::observed`] from the next step on.
    pub fn observe_with(&mut self, observer: Box<dyn Observer>) {
        self.observers.push(observer);
    }

    /// Why the session ended, if it has.
    pub fn ended(&self) -> Option<&'static str> {
        self.app.ending()
    }

    /// Closes the session log, and says where it is.
    pub fn finish(mut self) -> Result<Option<PathBuf>, String> {
        let log = self.log.take();
        drop(self);
        match log {
            Some(log) => log.finish().map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn queue(&self, request: Request) {
        // The receiver is the application's, which lives as long as `self`.
        let _ = self.requests.send(request);
    }

    pub(crate) fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    fn read(&self, drew: bool, took: f64) -> State {
        let metrics = tuile_core::metrics::metrics()
            .snapshot()
            .since(&self.baseline);
        let view = crate::snapshot::current()
            .map_or_else(|| crate::steer::view_of(&self.app.camera()), |s| s.view)
            .into();
        let (size, scale) = self.app.screen_size().unwrap_or((self.start.size, 1.0));
        let eyes = (0..self.start.eyes.count())
            .map(|eye| {
                let slot = eye.min(tuile_core::metrics::EYES - 1);
                EyeReading {
                    ground: metrics.eye_ground_pixels[slot],
                    black: metrics.eye_black_pixels[slot],
                }
            })
            .collect();
        State {
            frame: self.frame,
            seconds: self.seconds(),
            took,
            drew,
            view,
            size,
            scale,
            eyes,
            metrics,
            observed: BTreeMap::new(),
        }
    }

    fn answer_the_socket(&mut self) {
        let Some(requests) = self.socket.as_ref().map(socket::Listener::pending) else {
            return;
        };
        for (request, reply) in requests {
            let answer = socket::answer(self, &request);
            let _ = reply.send(answer);
        }
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        // A log nobody finished is finished here: an MCAP file without its
        // footer is one most readers refuse.
        if let Some(log) = self.log.take() {
            if let Err(e) = log.finish() {
                tracing::error!("the session log was not closed: {e}");
            }
        }
        self.socket = None;
        // As a desktop's session ends: the stream, the store written down,
        // the server joined.
        self.app.shut_down();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(10));
        }
    }
}

/// What a headless run says to the person: a line in the log, since there is
/// nobody to dismiss an alert.
fn said(title: &str, text: &str) -> bool {
    tracing::warn!(title, text, "said to the person");
    true
}
