// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimServer`: drives the budget exactly as the runtime will — traverse →
//! need → decide → load → deliver → upload → ack — on a stepped clock, and
//! checks the invariants at every step.

use std::collections::{BTreeMap, BTreeSet};

use tuile_budget::{
    Budget, BudgetConfig, BudgetStats, Bytes, Class, Decisions, FrameDeadline, FrameInput,
    Generation, LayerId, Micros, NeedDelta, NeedSet, Policy, Pressure, ResourceId, Role, Source,
    Thermal, TileId,
};

use super::gpu::SimGpu;
use super::net::{Arrival, NetProfile, SimNet};
use super::reference::Lockstep;
use super::world::{
    children, is_city, parent, sight, traverse, Camera, Lens, Residency, SimPlan, View,
};

/// One eye of a profile's rig: where it sits along the camera's right
/// vector, metres, and its lens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Eye {
    /// Offset along the camera's right vector, metres (half the IPD).
    pub offset_m: f64,
    /// Its lens.
    pub lens: Lens,
}

impl Eye {
    /// A single eye on the camera.
    pub fn mono(lens: Lens) -> Vec<Eye> {
        vec![Eye {
            offset_m: 0.0,
            lens,
        }]
    }

    /// Two eyes `ipd_m` apart, the right one's lens the left one's mirror.
    pub fn pair(ipd_m: f64, left: Lens) -> Vec<Eye> {
        vec![
            Eye {
                offset_m: -ipd_m / 2.0,
                lens: left,
            },
            Eye {
                offset_m: ipd_m / 2.0,
                lens: left.mirrored(),
            },
        ]
    }
}

/// The GPU encoder's scratch for one 256² tile with its chain, as
/// `tuile_wgpu::encode_reservation` counts it: the RGBA8 it reads
/// (349,524), the blocks it writes with rows aligned for the copy (90,368)
/// and a job per level (9 × 256).
pub const ENCODER_SCRATCH: Bytes = Bytes(442_196);

/// A device profile: the budget, the network, the frame rate, the eyes.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Its name, for messages.
    pub name: &'static str,
    /// The budget.
    pub budget: BudgetConfig,
    /// The network.
    pub net: NetProfile,
    /// The step length.
    pub dt: Micros,
    /// The convergence bound of I3 after the camera stops.
    pub converge_within: Micros,
    /// The eyes: one, or a stereo rig.
    pub eyes: Vec<Eye>,
    /// The executor's upload cap per render frame (I18).
    pub deadline: FrameDeadline,
    /// How many steps the executor runs behind the budget: 0 in-process,
    /// 1 for a web page whose budget runs in a worker (a decision arrives
    /// one message late).
    pub lag: usize,
    /// How the client holds its imagery (CB3): RGBA8, or blocks encoded on
    /// the GPU or the CPU.
    pub plan: SimPlan,
    /// Of the GPU encodes, how many in a thousand fall back to RGBA8.
    pub fallback_permille: u32,
}

impl Profile {
    /// A desktop: 2 GiB, a good network, 60 fps, 2 s to converge.
    pub fn desktop() -> Self {
        let mut b = BudgetConfig::interactive(Bytes::gib(2));
        // Today's desktop viewer runs 64 fetches; the same, split by class.
        b.max_in_flight = tuile_budget::PerClass([32, 16, 32, 4]);
        // And reads its local store as widely: at the default 8, a warm
        // start reads at most 8 resources a step and converges later than a
        // cold one on a good network (P1.3).
        b.max_local_reads = 32;
        b.imagery_over_terrain_cap = 3;
        b.max_drape_side = 2_048;
        Profile {
            name: "desktop",
            budget: b,
            net: NetProfile::desktop(),
            dt: Micros(16_667),
            converge_within: Micros::from_secs(2),
            eyes: Eye::mono(Lens::desktop()),
            // A quarter of a 60 Hz frame at 4 MiB/ms: above the plan's cap.
            deadline: FrameDeadline::from_refresh(60, 250, 4 << 20),
            lag: 0,
            plan: SimPlan::Rgba8,
            fallback_permille: 0,
        }
    }

    /// A phone: 512 MiB, cellular, 60 fps, 4.5 s to converge — 4 s until
    /// the screen-space error was priced by view depth (Laurent,
    /// 2026-11-24: the simulation's time bounds follow the new config; a
    /// still phone measured 4.15–4.32 s, the screen's edges being finer).
    pub fn handheld() -> Self {
        let mut b = BudgetConfig::interactive(Bytes::mib(512));
        b.max_in_flight = tuile_budget::PerClass([8, 4, 8, 2]);
        b.upload_bytes_per_frame = Bytes::mib(4);
        b.delivery_credit = Bytes::mib(16);
        b.imagery_over_terrain_cap = 2;
        b.max_drape_side = 1_024;
        Profile {
            name: "handheld",
            budget: b,
            net: NetProfile::handheld(),
            dt: Micros(16_667),
            converge_within: Micros::from_millis(4_500),
            eyes: Eye::mono(Lens::portrait()),
            // A quarter of a 60 Hz frame at 1 MiB/ms: just above the cap.
            deadline: FrameDeadline::from_refresh(60, 250, 1 << 20),
            lag: 0,
            plan: SimPlan::Rgba8,
            fallback_permille: 0,
        }
    }

    /// The handheld profile at its low clamp: 160 MiB (an older, small
    /// device under `os_proc_available_memory`).
    pub fn handheld_low() -> Self {
        let mut p = Profile::handheld();
        p.name = "handheld-low";
        let quotas = p.budget.max_in_flight;
        let upload = p.budget.upload_bytes_per_frame;
        p.budget = BudgetConfig::interactive(Bytes::mib(160));
        p.budget.max_in_flight = quotas;
        p.budget.upload_bytes_per_frame = upload;
        p.budget.delivery_credit = Bytes::mib(8);
        p.budget.imagery_over_terrain_cap = 1;
        p.budget.max_drape_side = 512;
        p
    }

    /// A film: Batch, a clean network.
    pub fn film() -> Self {
        Profile {
            name: "film",
            budget: BudgetConfig::batch(),
            net: NetProfile::desktop().clean(),
            dt: Micros(16_667),
            converge_within: Micros::NEVER,
            eyes: Eye::mono(Lens::desktop()),
            deadline: FrameDeadline::UNBOUNDED,
            lag: 0,
            plan: SimPlan::Rgba8,
            fallback_permille: 0,
        }
    }

    /// A browser: a desktop's memory and network, its budget in a worker a
    /// message away from the page that executes (one step of lag), a
    /// 1280 × 720 canvas at a `devicePixelRatio` of 2; its bound, the
    /// desktop's plus one step.
    pub fn web() -> Self {
        let mut p = Profile::desktop();
        p.name = "web";
        p.eyes = Eye::mono(Lens::horizontal(2560.0, 1440.0, 2.0, 60.0));
        p.lag = 1;
        p.converge_within = p.converge_within.plus(p.dt);
        p
    }

    /// A stereo headset at `hz`: a phone's memory and a desktop's network
    /// (Wi-Fi), two eyes 63 mm apart with asymmetric lenses, a step per
    /// display period, 2 s to converge (spec §14, proposed).
    pub fn headset(hz: u32) -> Self {
        let mut p = Profile::handheld();
        p.name = match hz {
            72 => "headset-72",
            90 => "headset-90",
            120 => "headset-120",
            _ => "headset",
        };
        p.net = NetProfile::desktop();
        p.dt = Micros(1_000_000 / u64::from(hz));
        p.converge_within = Micros::from_secs(2);
        p.eyes = Eye::pair(0.063, Lens::headset_left());
        // The plan may ask more than a frame can take: the deadline binds.
        p.budget.upload_bytes_per_frame = Bytes::mib(8);
        p.deadline = FrameDeadline::from_refresh(hz, 250, 1 << 20);
        p
    }

    /// `self`, its imagery held as `plan` says, named `name`; a GPU plan's
    /// encodes fall back one time in a hundred.
    pub fn compressed(mut self, name: &'static str, plan: SimPlan) -> Self {
        self.name = name;
        self.plan = plan;
        self.fallback_permille = if plan == SimPlan::Gpu { 10 } else { 0 };
        if plan == SimPlan::Gpu {
            // The encoder's scratch, reserved off the drapes (CB4).
            self.budget.reserved[Class::Drape] = ENCODER_SCRATCH;
        }
        self
    }

    /// A desktop holding BC7, encoded on the GPU (CB3).
    pub fn desktop_bc7() -> Self {
        Profile::desktop().compressed("desktop-bc7", SimPlan::Gpu)
    }

    /// A phone holding ASTC 4×4, encoded on the GPU (CB3).
    pub fn handheld_astc_gpu() -> Self {
        Profile::handheld().compressed("handheld-astc-gpu", SimPlan::Gpu)
    }

    /// A page on WebGL2: blocks encoded in its decode workers (CB3).
    pub fn web_webgl2_cpu() -> Self {
        Profile::web().compressed("web-webgl2-cpu", SimPlan::Cpu)
    }

    /// A 90 Hz headset holding ASTC 4×4, encoded on the GPU (CB3).
    pub fn headset_90_astc() -> Self {
        Profile::headset(90).compressed("headset-90-astc", SimPlan::Gpu)
    }
}

/// What one step looked like.
#[derive(Clone, Debug)]
pub struct StepLog {
    /// The step's time.
    pub now: Micros,
    /// The decision digest.
    pub digest: u64,
    /// The budget's statistics.
    pub stats: BudgetStats,
    /// The view is converged in the sense of I3.
    pub converged: bool,
    /// Resources held on the GPU or decoded (bounded by bytes).
    pub held: u32,
    /// The step decided nothing: no start, cancel, delivery or eviction.
    pub idle: bool,
}

/// What the heap saw during one `decide_into` call: the step written into
/// the decisions of the step before, as the geometry server does, so the
/// output's buffers are the host's and the call allocates only for what
/// outgrew them.
#[derive(Clone, Copy, Debug)]
pub struct DecideAllocs {
    /// The step's time.
    pub now: Micros,
    /// The call's own tally.
    pub heap: super::alloc::Tally,
    /// The blocks a fresh copy of its output takes (a clone of the
    /// `Decisions`, with exact capacities), when [`Sim::count_output`] is
    /// set; else 0. What a host that took a new `Decisions` every step
    /// would allocate for the output.
    pub output: u64,
    /// Items drawn.
    pub drawn: u32,
}

struct BudgetResidency<'a>(&'a Budget);
impl Residency for BudgetResidency<'_> {
    fn resident(&self, t: TileId) -> bool {
        self.0.residency().resident(t)
    }
}

/// The calling thread's CPU time, µs; 0 where the platform has no thread
/// clock.
fn thread_cpu_us() -> f64 {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn clock_gettime_nsec_np(clock: u32) -> u64;
        }
        const CLOCK_THREAD_CPUTIME_ID: u32 = 16;
        // SAFETY: a libc call with a valid clock id and no pointer.
        let ns = unsafe { clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID) };
        ns as f64 / 1e3
    }
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Timespec {
            sec: i64,
            nsec: i64,
        }
        extern "C" {
            fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
        }
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        let mut ts = Timespec { sec: 0, nsec: 0 };
        // SAFETY: a libc call writing the `timespec` it is given.
        let ok = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0;
        if ok {
            ts.sec as f64 * 1e6 + ts.nsec as f64 / 1e3
        } else {
            0.0
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0.0
    }
}

/// This process's instructions retired and cycles so far, where the
/// platform counts them (macOS: `proc_pid_rusage`, `rusage_info_v4`); else
/// zeros. The simulation is one thread, so the difference over a `decide`
/// call is the call's own: a count that a loaded machine does not inflate,
/// where its times are.
fn instructions_cycles() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        }
        const RUSAGE_INFO_V4: i32 = 4;
        // `rusage_info_v4` is 37 words; its instructions and cycles are the
        // 32nd and 33rd (after the 16-byte uuid and 29 counters).
        let mut info = [0u64; 64];
        // SAFETY: the buffer is larger than `rusage_info_v4`, which the call
        // writes; the pid is this process's.
        let ok = unsafe {
            proc_pid_rusage(std::process::id() as i32, RUSAGE_INFO_V4, info.as_mut_ptr())
        } == 0;
        if ok {
            (info[31], info[32])
        } else {
            (0, 0)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        (0, 0)
    }
}

/// The simulation.
pub struct Sim {
    /// The profile.
    pub profile: Profile,
    /// The budget under test, checked step by step against the reference
    /// recorded from the engine before O1 (`tests/reference/`).
    pub budget: Lockstep,
    /// The network.
    pub net: SimNet,
    /// The GPU.
    pub gpu: SimGpu,
    /// The clock.
    pub now: Micros,
    /// The frame.
    pub frame: u64,
    /// Every step's log.
    pub log: Vec<StepLog>,
    /// Decisions on their way to a lagging executor.
    pipeline: std::collections::VecDeque<Decisions>,
    /// The eyes now: the profile's, until a resize or a rotation changes
    /// their lenses.
    pub eyes: Vec<Eye>,
    /// The views of the last step.
    pub last_views: Vec<View>,
    /// Decoded resources the server holds.
    decoded: BTreeSet<ResourceId>,
    /// The latest fresh generation loaded for each resource.
    fresh: BTreeMap<ResourceId, Generation>,
    drawn_last: BTreeSet<TileId>,
    sse_scale: f64,
    last_cam: Option<Camera>,
    floor_in: bool,
    /// When the floor first became drawable.
    pub floor_at: Option<Micros>,
    /// When each wanted mesh was first wanted, until it is drawable.
    wanted_at: BTreeMap<ResourceId, Micros>,
    /// When each mesh was last evicted: a wait for it within 30 s is a
    /// reload, told apart from the outside — the same on any budget.
    evicted_at: BTreeMap<ResourceId, Micros>,
    /// The kind of each of [`Sim::waits`] by the time since the mesh's
    /// eviction when its wait began (`tuile_budget::waits::kind`'s).
    pub wait_kinds: Vec<u8>,
    /// The reason of the eviction a reload of [`Sim::waits`] follows (its
    /// decision record's label), `""` for a first load.
    pub wait_reasons: Vec<&'static str>,
    wait_kind_open: BTreeMap<ResourceId, (u8, &'static str)>,
    /// Why each mesh was last evicted, as its decision record says.
    evicted_why: BTreeMap<ResourceId, &'static str>,
    /// When each tile was last listed by the need, any membership: the
    /// sweep's horizon is read against it (I19).
    listed_at: BTreeMap<TileId, u64>,
    /// Wanted-to-drawable times of meshes, ms, with their time: the
    /// latency the budget's scheduling adds to the network's (I6).
    pub waits: Vec<(Micros, f64)>,
    /// Wall time of each `decide` call alone, µs, with the step's time.
    pub decide_us: Vec<(Micros, f64)>,
    /// The calling thread's CPU time over each `decide` call, µs, with the
    /// step's time: what the call computed, without the time the thread
    /// waited for a core (a loaded machine inflates the wall time alone).
    /// 0 where the platform gives no thread clock.
    pub decide_cpu_us: Vec<(Micros, f64)>,
    /// Instructions retired and cycles over each `decide` call, with the
    /// step's time (`instructions_cycles`; zeros where uncounted).
    pub decide_instructions: Vec<(Micros, u64, u64)>,
    /// What the heap saw during each `decide` call, with the step's time.
    pub decide_allocs: Vec<DecideAllocs>,
    /// The host's side of a delta step (O1.3): wall time of building the
    /// delta from the last need and this one (`NeedDelta::diff`, what the
    /// runtime computes before `decide`), µs, and the blocks it allocated,
    /// with the step's time. Absent on a whole-need step.
    pub diff_us: Vec<(Micros, f64, u64)>,
    /// Also count the allocations of the step's output alone (a clone of
    /// its `Decisions`), for `DecideAllocs::output`: off by default, it
    /// costs a clone per step.
    pub count_output: bool,
    /// Per tile, while the camera is still (I4): each (mesh level drawn,
    /// drape level worn) it showed, with the time it began — the first as
    /// first drawn, then one per change.
    levels: BTreeMap<TileId, LevelHistory>,
    /// Arrivals and acknowledgements fed to the budget so far.
    fed: u64,
    /// Those fed at the last `decide`, which it applied.
    fed_at_decide: u64,
    /// The events the last `decide` applied: those fed since the one
    /// before (P6.4's #E_k).
    pub step_events: u64,
    /// The need of the last step, `None` before the first: a step tells
    /// the budget what changed since it, as the geometry server does.
    need_last: Option<NeedSet>,
    /// What changed, in a buffer kept across steps.
    delta: NeedDelta,
    /// The decisions, written into the same buffers every step, as the
    /// geometry server does.
    out: Decisions,
    /// The pressure last set ([`Sim::pressure`]).
    pub pressure_set: Pressure,
    /// The city's tileset is drawn beside the globe (`world::CITY`).
    pub city: bool,
    /// The views ahead for the next step (`ClientMessage::Ahead`): each
    /// camera, when it is due and how likely, thousandths. Their tiles join
    /// the ring with their predicted use, as the runtime lists them. Empty,
    /// the default: no prediction, the need exactly as without one.
    pub ahead: Vec<(Camera, Micros, u16)>,
    /// Preload nothing: the need's ring is dropped before every step, so a
    /// run can be compared with and without preloads (the regulator's T5).
    pub no_ring: bool,
}

impl Sim {
    /// A simulation on `profile`, seeded, its budget checked against the
    /// recorded reference.
    pub fn new(profile: Profile, seed: u64) -> Self {
        let budget = Lockstep::new(profile.budget.clone()).expect("a valid profile");
        Self::with(profile, seed, budget)
    }

    /// The same simulation, the real budget alone, the reference left out:
    /// for the bench, whose timings must not include it.
    #[allow(dead_code)] // The bench's only (`benches/budget_decide.rs`).
    pub fn unchecked(profile: Profile, seed: u64) -> Self {
        let budget = Lockstep::unchecked(profile.budget.clone()).expect("a valid profile");
        Self::with(profile, seed, budget)
    }

    fn with(profile: Profile, seed: u64, budget: Lockstep) -> Self {
        Sim {
            budget,
            net: {
                let mut n = SimNet::new(profile.net, seed);
                n.plan = profile.plan;
                n
            },
            gpu: {
                let mut g = SimGpu::new(profile.deadline, profile.eyes.len());
                g.lagging = profile.lag > 0;
                g.plan = profile.plan;
                g.fallback_permille = profile.fallback_permille;
                g
            },
            now: Micros::ZERO,
            frame: 0,
            log: Vec::new(),
            eyes: profile.eyes.clone(),
            last_views: Vec::new(),
            pipeline: std::collections::VecDeque::new(),
            decoded: BTreeSet::new(),
            fresh: BTreeMap::new(),
            drawn_last: BTreeSet::new(),
            sse_scale: 1.0,
            last_cam: None,
            floor_in: false,
            floor_at: None,
            wanted_at: BTreeMap::new(),
            evicted_at: BTreeMap::new(),
            wait_kinds: Vec::new(),
            wait_reasons: Vec::new(),
            wait_kind_open: BTreeMap::new(),
            evicted_why: BTreeMap::new(),
            listed_at: BTreeMap::new(),
            waits: Vec::new(),
            decide_us: Vec::new(),
            decide_cpu_us: Vec::new(),
            decide_instructions: Vec::new(),
            diff_us: Vec::new(),
            decide_allocs: Vec::new(),
            count_output: false,
            levels: BTreeMap::new(),
            fed: 0,
            fed_at_decide: 0,
            step_events: 0,
            need_last: None,
            delta: NeedDelta::new(),
            out: Decisions::default(),
            pressure_set: Pressure::Normal,
            city: false,
            ahead: Vec::new(),
            no_ring: false,
            profile,
        }
    }

    /// The views ahead's tiles the present need does not list, in its ring,
    /// each with the earliest and likeliest of the views that select it —
    /// the runtime's pass over the views ahead.
    fn add_the_ring_ahead(&self, need: &mut NeedSet, drawn_last: &BTreeSet<TileId>) {
        let listed: BTreeSet<TileId> = need
            .drawn
            .iter()
            .chain(&need.blockers)
            .chain(&need.ancestors)
            .chain(&need.ring)
            .map(|(t, _)| *t)
            .collect();
        let mut ring: BTreeMap<TileId, tuile_budget::Need> = BTreeMap::new();
        for (cam, when, p) in &self.ahead {
            let views = self.views(*cam);
            let ahead = traverse(
                &views,
                &BudgetResidency(&self.budget),
                self.sse_scale,
                drawn_last,
                self.pinned(),
                self.city,
            );
            for (t, n) in ahead.drawn.into_iter().chain(ahead.blockers) {
                if listed.contains(&t) {
                    continue;
                }
                let e = ring.entry(t).or_insert(n);
                let use_ = match e.ahead {
                    Some(a) => tuile_budget::AheadUse {
                        next_use_in: a.next_use_in.min(*when),
                        probability: a.probability.max(*p),
                    },
                    None => tuile_budget::AheadUse {
                        next_use_in: *when,
                        probability: *p,
                    },
                };
                e.ahead = Some(use_);
            }
        }
        need.ring.extend(ring);
    }

    fn pinned(&self) -> u32 {
        self.profile.budget.pinned_level.unwrap_or(0)
    }

    /// Every eye's view of `cam`: each eye shifted along the camera's right
    /// vector, through its own lens.
    pub fn views(&self, cam: Camera) -> Vec<View> {
        let (hs, hc) = super::detmath::sin_cos(cam.heading);
        self.eyes
            .iter()
            .map(|e| View {
                cam: Camera {
                    x: cam.x + e.offset_m * hs,
                    y: cam.y - e.offset_m * hc,
                    ..cam
                },
                lens: e.lens,
            })
            .collect()
    }

    /// Every eye now looks through `lens` (a resize, a rotation, a scale
    /// change), from the next step on.
    pub fn set_lens(&mut self, lens: Lens) {
        for e in &mut self.eyes {
            e.lens = lens;
        }
    }

    /// One step with the camera at `cam`. Panics, with the scenario context,
    /// on any invariant violation.
    pub fn step(&mut self, cam: Camera) -> (NeedSet, Decisions) {
        for a in self.net.poll(self.now) {
            match a {
                Arrival::Loaded(r, g, b) => {
                    self.fresh.insert(r, g);
                    self.decoded.insert(r);
                    self.fed += 1;
                    self.budget.on_loaded(r, g, b);
                }
                Arrival::Failed(r, g) => {
                    self.fed += 1;
                    self.budget
                        .on_failed(r, g, tuile_budget::Failure::Transient)
                }
                Arrival::StoreMiss(r, g) => {
                    self.fed += 1;
                    self.budget.on_store_miss(r, g);
                }
            }
        }
        // A film's frame is a function of its own inputs: no hysteresis on
        // what an earlier frame drew.
        let none = BTreeSet::new();
        let drawn_last = match self.profile.budget.policy {
            Policy::Batch => &none,
            Policy::Interactive => &self.drawn_last,
        };
        let views = self.views(cam);
        let mut need = traverse(
            &views,
            &BudgetResidency(&self.budget),
            self.sse_scale,
            drawn_last,
            self.pinned(),
            self.city,
        );
        if !self.ahead.is_empty() {
            self.add_the_ring_ahead(&mut need, drawn_last);
        }
        if self.no_ring {
            need.ring.clear();
        }
        // The first step reads the whole need; every later one what changed
        // since the step before (built outside what is timed and counted).
        let input = match &self.need_last {
            Some(last) => {
                let clock = std::time::Instant::now();
                let ((), heap) = super::alloc::counted(|| self.delta.diff(last, &need));
                let us = clock.elapsed().as_secs_f64() * 1e6;
                self.diff_us.push((self.now, us, heap.allocs));
                FrameInput::from_delta(self.frame, self.now, &self.delta)
            }
            None => FrameInput::new(self.frame, self.now, &need),
        }
        .with_presence(&self.net.store);
        self.step_events = self.fed - self.fed_at_decide;
        self.fed_at_decide = self.fed;
        // The budget's call alone is timed and counted, into the last
        // step's decisions (their buffers kept, as the geometry server
        // keeps them); its digests are folded after it, and compared with
        // the recorded reference.
        let mut out = std::mem::take(&mut self.out);
        let (heap, us, cpu, ic) =
            self.budget
                .decide_into_with(input, &need, &mut out, |b, i, o| {
                    let cpu = thread_cpu_us();
                    let ic = instructions_cycles();
                    let clock = std::time::Instant::now();
                    let ((), heap) = super::alloc::counted(|| b.decide_into(i, o));
                    let us = clock.elapsed().as_secs_f64() * 1e6;
                    let ic2 = instructions_cycles();
                    let ic = (ic2.0.saturating_sub(ic.0), ic2.1.saturating_sub(ic.1));
                    (heap, us, thread_cpu_us() - cpu, ic)
                });
        self.decide_us.push((self.now, us));
        self.decide_cpu_us.push((self.now, cpu));
        self.decide_instructions.push((self.now, ic.0, ic.1));
        // Outside the counted region, deliberately: what a fresh copy of
        // the output takes (`count_output`), and the copy the simulation
        // keeps for its log and its callers — the host's buffer goes back
        // for the next step.
        let output = if self.count_output {
            super::alloc::counted(|| out.clone()).1.allocs
        } else {
            0
        };
        let d = out.clone();
        self.out = out;
        self.decide_allocs.push(DecideAllocs {
            now: self.now,
            heap,
            output,
            drawn: d.draw.items.len() as u32,
        });
        // The executor: in step, or `lag` steps behind. Either way it has
        // applied every decision before this one when this one is checked.
        let lagged = (self.profile.lag > 0).then(|| {
            self.pipeline.push_back(d.clone());
            if self.pipeline.len() > self.profile.lag {
                self.pipeline.pop_front()
            } else {
                None
            }
        });
        let mut acks = Vec::new();
        if let Some(Some(old)) = &lagged {
            acks = self.execute(old, self.frame - self.profile.lag as u64);
        }
        self.check(&need, &d, &views);
        self.check_sweep(&need, &d);
        for (r, g) in &d.cancel {
            self.net.cancel(*r, *g);
        }
        for s in &d.start {
            self.net.start(s, self.now);
        }
        for r in &d.release {
            self.decoded.remove(r);
        }
        if lagged.is_none() {
            acks = self.execute(&d, self.frame);
            self.check_executor(&d, &acks);
        }
        let mut seen = BTreeSet::new();
        for (t, _) in need.drawn.iter().chain(&need.blockers) {
            let r = ResourceId::Mesh(*t);
            seen.insert(r);
            if !self.gpu.drawable.contains_key(&r) && !self.wanted_at.contains_key(&r) {
                self.wanted_at.insert(r, self.now);
                let since = self.evicted_at.get(&r).map(|t| self.now.since(*t));
                // 0 a first load, 1 a reload evicted < 5 s before, 2 < 30 s.
                let kind = match since {
                    Some(t) if t < Micros::from_secs(5) => 1,
                    Some(t) if t < Micros::from_secs(30) => 2,
                    _ => 0,
                };
                let why = if kind > 0 {
                    self.evicted_why.get(&r).copied().unwrap_or("?")
                } else {
                    ""
                };
                self.wait_kind_open.insert(r, (kind, why));
            }
        }
        self.wanted_at.retain(|r, _| seen.contains(r));
        self.wait_kind_open.retain(|r, _| seen.contains(r));
        for r in &d.evict {
            self.evicted_at.insert(*r, self.now);
        }
        for e in &d.scores {
            if e.side == tuile_budget::score::Side::Evict {
                self.evicted_why.insert(e.resource, e.reason.label());
            }
        }
        if self.frame.is_multiple_of(600) {
            let now = self.now;
            self.evicted_at
                .retain(|_, t| now.since(*t) < Micros::from_secs(30));
            let at = &self.evicted_at;
            self.evicted_why.retain(|r, _| at.contains_key(r));
        }
        for (r, g, _, b) in std::mem::take(&mut self.gpu.compressed) {
            self.budget.on_compressed(r, g, b);
        }
        for (r, g, b) in acks {
            if let Some(at) = self.wanted_at.remove(&r) {
                self.waits
                    .push((self.now, self.now.since(at).0 as f64 / 1_000.0));
                let (kind, why) = self.wait_kind_open.remove(&r).unwrap_or((0, ""));
                self.wait_kinds.push(kind);
                self.wait_reasons.push(why);
            }
            self.fed += 1;
            self.budget.on_drawable(r, g, b);
        }
        self.sse_scale = f64::from(d.sse_scale_milli) / 1000.0;
        self.drawn_last = d
            .draw
            .items
            .iter()
            .filter(|i| i.role != Role::Fallback)
            .map(|i| i.mesh.tile())
            .collect();
        let still = self.last_cam == Some(cam) && self.last_views == views;
        self.track_levels(still, &d);
        let converged = d.stats.converged
            && need.blockers.is_empty()
            && d.stats.fallback == 0
            && d.stats.held == 0
            && d.stats.lost == 0
            && d.stats.underdraped == 0;
        self.log.push(StepLog {
            now: self.now,
            digest: d.digest(),
            stats: d.stats.clone(),
            converged,
            idle: d.start.is_empty()
                && d.cancel.is_empty()
                && d.deliver.is_empty()
                && d.evict.is_empty()
                && d.upload_plan.items.is_empty()
                && d.stats.in_flight.iter().all(|(_, n)| n == 0)
                && d.stats.local_reads == 0
                && d.stats.in_transit == Bytes::ZERO,
            held: self
                .gpu
                .drawable
                .keys()
                .chain(self.decoded.iter())
                .collect::<BTreeSet<_>>()
                .len() as u32,
        });
        self.last_cam = Some(cam);
        self.last_views = views;
        self.floor_in = self.floor_drawable();
        if self.floor_in && self.floor_at.is_none() {
            self.floor_at = Some(self.now);
        }
        self.now = self.now.plus(self.profile.dt);
        self.frame += 1;
        match &mut self.need_last {
            Some(last) => last.clone_from(&need),
            None => self.need_last = Some(need.clone()),
        }
        (need, d)
    }

    /// The floor is in (P1.1, D-A1): every tile at a level up to the
    /// pinned one has its mesh and its drape on the GPU.
    fn floor_drawable(&self) -> bool {
        let Some(pinned) = self.profile.budget.pinned_level else {
            return false;
        };
        (0..=pinned).all(|l| {
            let n = 1u64 << l;
            (0..n).all(|x| {
                (0..n).all(|y| {
                    let t = super::world::tile(l, x, y);
                    self.gpu.drawable.contains_key(&ResourceId::Mesh(t))
                        && self
                            .gpu
                            .drawable
                            .contains_key(&ResourceId::drape(t, LayerId(0), l))
                })
            })
        })
    }

    /// The executor applies the decisions of step `frame`, as one render
    /// frame; checks I17 and I18 for it; returns its acknowledgements.
    fn execute(&mut self, d: &Decisions, frame: u64) -> Vec<(ResourceId, Generation, Bytes)> {
        let acks = self
            .gpu
            .apply(d, frame)
            .unwrap_or_else(|e| panic!("{} frame {}: {e}", self.profile.name, self.frame));
        // I18: a frame uploads at most its deadline, but for a lone first item.
        if self.gpu.frame_items > 1 && self.gpu.frame_bytes > self.gpu.deadline.bytes {
            self.fail(&format!(
                "I18: {} bytes in {} uploads, deadline {}",
                self.gpu.frame_bytes.0, self.gpu.frame_items, self.gpu.deadline.bytes.0
            ));
        }
        // I17: every eye drew the draw set of the step being executed.
        if self.gpu.eye_drew.iter().any(|e| *e != frame) {
            self.fail(&format!(
                "I17: eyes drew {:?}, not frame {frame}",
                self.gpu.eye_drew
            ));
        }
        acks
    }

    /// I16, in step with the executor: the budget charges what the executor
    /// holds, each resource once — its drawable bytes plus what it was
    /// handed and has not uploaded yet, class by class.
    ///
    /// An upload acknowledged this frame is charged its estimate's peak
    /// until the budget reads the acknowledgement.
    fn check_executor(&self, d: &Decisions, acks: &[(ResourceId, Generation, Bytes)]) {
        for c in Class::ALL {
            // Uploaded this frame, its acknowledgement not read yet: the
            // budget still charges its estimate's peak (resident, and what
            // an encode on the GPU holds beside it, CB4), the executor what
            // it acknowledged — a fallback's RGBA8 included (CB3).
            let (estimated, acknowledged) = acks.iter().filter(|(r, _, _)| class_of(*r) == c).fold(
                (Bytes::ZERO, Bytes::ZERO),
                |(e, a), (r, _, b)| {
                    let c = super::world::cost_for(*r, self.profile.plan);
                    (e.plus(c.gpu.plus(c.transient)), a.plus(*b))
                },
            );
            let pending =
                self.gpu
                    .pending
                    .keys()
                    .filter(|r| class_of(**r) == c)
                    .fold(Bytes::ZERO, |a, r| {
                        // Delivered, not uploaded: charged at its peak (CB4).
                        let c = super::world::cost_for(*r, self.profile.plan);
                        a.plus(c.gpu.plus(c.transient))
                    });
            // Compressed this frame, its acknowledgement not read yet: the
            // budget still charges both copies (CB9).
            let compressed = self
                .gpu
                .compressed
                .iter()
                .filter(|(r, ..)| class_of(*r) == c)
                .fold(Bytes::ZERO, |a, (_, _, charged, now)| {
                    a.plus(charged.minus(*now))
                });
            let held = self.gpu.bytes[c]
                .plus(pending)
                .plus(estimated)
                .plus(compressed)
                .minus(acknowledged);
            if d.stats.gpu[c] != held {
                self.fail(&format!(
                    "I16: the budget charges {c:?} {} bytes, the executor holds {}",
                    d.stats.gpu[c].0, held.0
                ));
            }
        }
    }

    fn fail(&self, what: &str) -> ! {
        panic!(
            "{} frame {} t={:?}: {what}",
            self.profile.name, self.frame, self.now
        )
    }

    /// I19 (Laurent, 2026-11-24): the proactive sweep never takes a mesh
    /// the need lists this step, or listed within the specification's
    /// horizon (`Params::sweep_keep_poses`: the last poses, a step each).
    /// Meshes only: a drape's tile listed
    /// says nothing of which of its levels is wanted. Then records the
    /// step's listing.
    fn check_sweep(&mut self, need: &NeedSet, d: &Decisions) {
        use tuile_budget::score::{Reason, Side};
        let keep = u64::from(self.budget.specification().params.sweep_keep_poses);
        let listed: BTreeSet<TileId> = need
            .drawn
            .iter()
            .chain(&need.blockers)
            .chain(&need.ancestors)
            .chain(&need.ring)
            .map(|(t, _)| *t)
            .collect();
        for e in &d.scores {
            let ResourceId::Mesh(t) = e.resource else {
                continue;
            };
            if e.side != Side::Evict || e.reason != Reason::Sweep {
                continue;
            }
            let recent = self
                .listed_at
                .get(&t)
                .is_some_and(|at| self.frame - at < keep);
            if listed.contains(&t) || recent {
                self.fail(&format!(
                    "I19: the sweep took {t:?}, listed {}",
                    if listed.contains(&t) {
                        "now"
                    } else {
                        "within the horizon"
                    }
                ));
            }
        }
        let pose = self.frame;
        for t in listed {
            self.listed_at.insert(t, pose);
        }
        if self.frame.is_multiple_of(600) {
            self.listed_at.retain(|_, at| pose - *at < keep);
        }
    }

    /// The per-step invariants.
    fn check(&self, need: &NeedSet, d: &Decisions, views: &[View]) {
        let s = &d.stats;
        let cfg = &self.profile.budget;
        // CB9: only a drape is compressed in place, each once a step, what
        // the policy found eligible, within the step's lane — and nothing
        // where nothing can be compressed.
        let c = &s.compress;
        let mut asked = BTreeSet::new();
        for (r, _) in &d.compress {
            if !matches!(r, ResourceId::Drape { .. }) || !asked.insert(*r) {
                self.fail(&format!("CB9: {r:?} asked to be compressed"));
            }
        }
        if c.started as usize != d.compress.len() || c.started > c.eligible {
            self.fail(&format!("CB9: {c:?} for {} asked", d.compress.len()));
        }
        if self.profile.plan != SimPlan::Gpu && (c.waiting > 0 || !d.compress.is_empty()) {
            self.fail(&format!("CB9: {c:?} with nothing compressible"));
        }
        // I1 (D-A1): every drawn item wears its drapes, from the first step.
        if s.bare != 0 {
            self.fail(&format!("I1: {} items drawn bare", s.bare));
        }
        let draped: BTreeSet<TileId> = need
            .drawn
            .iter()
            .filter(|(_, n)| n.imagery.is_some())
            .map(|(t, _)| *t)
            .collect();
        for i in &d.draw.items {
            if i.drapes.is_empty() && i.serves.iter().any(|t| draped.contains(t)) {
                self.fail(&format!("I1: {:?} drawn bare", i.mesh));
            }
        }
        // P4.2: nothing still wanted on screen, or waited on, is evicted.
        let on_screen: BTreeSet<TileId> = need
            .drawn
            .iter()
            .chain(&need.blockers)
            .map(|(t, _)| *t)
            .collect();
        for e in &d.evict {
            if matches!(e, ResourceId::Mesh(t) if on_screen.contains(t)) {
                self.fail(&format!("P4.2: {e:?}, wanted on screen, evicted"));
            }
        }
        // I1: once the floor is in, nothing is lost and the ground is covered
        // (where the city has nothing yet, the ground under it is drawn).
        if self.floor_in {
            if s.lost != 0 {
                self.fail(&format!("I1: {} wanted tiles lost", s.lost));
            }
            for (t, _) in need.drawn.iter().filter(|(t, _)| !is_city(*t)) {
                if !covered(d, *t) {
                    self.fail(&format!("I1: {t:?} uncovered"));
                }
            }
            // I17: and in each eye, against that eye's own frustum.
            if views.len() > 1 {
                for (e, v) in views.iter().enumerate() {
                    for (t, _) in need.drawn.iter().filter(|(t, _)| !is_city(*t)) {
                        if sight(v, *t, 0.0).visible && !covered(d, *t) {
                            self.fail(&format!("I17: eye {e}: {t:?} uncovered"));
                        }
                    }
                }
            }
        }
        // CB4: the peak — resident, and in transit at its peak — plus what
        // the executor reserves never crosses a class's configured hard
        // limit.
        for c in Class::ALL {
            let hard = cfg.hard_limit[c];
            if hard != Bytes::MAX && s.gpu[c].plus(cfg.reserved[c]) > hard {
                self.fail(&format!(
                    "CB4: {c:?} peak {} + reserved {} over its hard limit {}",
                    s.gpu[c].0, cfg.reserved[c].0, hard.0
                ));
            }
        }
        // CB3: under compression, the ledger is the sum of the states, the
        // decoded copies' extras included, every step.
        if self.profile.plan != SimPlan::Rgba8 {
            if let Err(e) = self.budget.audit_both() {
                self.fail(&format!("CB3: the ledger against its recount: {e:?}"));
            }
        }
        // CB3: an encode that fell back is a fallback, never drift: the
        // executor acknowledges every upload exactly.
        if s.drift_exceeded.iter().any(|(_, n)| n > 0) {
            self.fail("CB3: an acknowledgement counted as drift");
        }
        // I2: what is not protected fits in the target.
        for c in Class::ALL {
            if s.gpu[c].minus(s.protected[c]) > s.target[c] {
                self.fail(&format!("I2: {c:?} unprotected bytes over target"));
            }
        }
        // I2: no class ever crosses its hard limit (the effective one), and
        // the device's total never crosses the configured total.
        for c in Class::ALL {
            let hard = s.hard_limit[c];
            if s.gpu[c] > hard || self.gpu.bytes[c] > hard {
                self.fail(&format!("I2: {c:?} over its hard limit"));
            }
        }
        // And the pool's members together never cross the pool's.
        let pool = cfg.pool;
        let pooled = Class::ALL
            .iter()
            .filter(|c| pool.holds(**c))
            .fold(Bytes::ZERO, |a, c| a.plus(s.gpu[*c]));
        if pooled > pool.hard_limit && Class::ALL.iter().any(|c| pool.holds(*c)) {
            self.fail("I2: the pool over its hard limit");
        }
        // I5: only the fresh generation is delivered.
        for (r, g) in &d.deliver {
            if self.fresh.get(r) != Some(g) {
                self.fail(&format!("I5: {r:?} delivered at stale {g:?}"));
            }
        }
        // I7: bounded state.
        if s.heap_len > cfg.request_heap {
            self.fail("I7: request heap over its cap");
        }
        if s.duplicates_in_flight != 0 {
            self.fail("I7: duplicates in flight");
        }
        let bound = 4
            * (self.gpu.drawable.len()
                + self.gpu.pending.len()
                + need.drawn.len()
                + need.blockers.len()
                + need.ancestors.len()
                + need.ring.len())
            + 64;
        if s.tracked as usize > bound {
            self.fail(&format!(
                "I7: tracking {} resources, bound {bound}",
                s.tracked
            ));
        }
        // I11: nothing protected is evicted.
        for e in &d.evict {
            let drawn = d
                .draw
                .items
                .iter()
                .any(|i| i.mesh == *e || i.drapes.iter().any(|x| x.resource == *e));
            if drawn
                || self.gpu.pending.contains_key(e)
                || e.tile().terrain_coord().0 <= self.pinned()
                    && cfg.pinned_level.is_some()
                    && !is_city(e.tile())
            {
                self.fail(&format!("I11: protected {e:?} evicted"));
            }
        }
        // I14: nothing held is fetched again.
        for st in &d.start {
            if st.source == Source::Network {
                let held = self.gpu.drawable.contains_key(&st.resource)
                    || self.decoded.contains(&st.resource)
                    || self.net.store.held.contains(&st.resource);
                if held {
                    self.fail(&format!("I14: {:?} fetched while held", st.resource));
                }
            }
            if self.net.in_flight(st.resource) {
                self.fail(&format!("I14: {:?} started twice", st.resource));
            }
        }
        // I13: a ready batch frame is exact.
        if cfg.policy == Policy::Batch {
            if !d.evict.is_empty() {
                self.fail("I13: Batch evicted inside a frame");
            }
            if d.frame_ready {
                for (t, _) in &need.drawn {
                    let exact =
                        d.draw.get(ResourceId::Mesh(*t)).map(|i| i.role) == Some(Role::Exact);
                    if !exact {
                        self.fail(&format!("I13: ready frame draws {t:?} inexactly"));
                    }
                }
                if !need.blockers.is_empty() {
                    self.fail("I13: ready frame with blockers");
                }
            }
        }
    }

    fn track_levels(&mut self, still: bool, d: &Decisions) {
        if !still {
            self.levels.clear();
            return;
        }
        for i in &d.draw.items {
            let mesh = i.mesh.tile().terrain_coord().0;
            let drape = i.drapes.first().map(|x| x.level).unwrap_or(0);
            for t in &i.serves {
                let h = self.levels.entry(*t).or_default();
                if h.last().is_none_or(|x| x.1 != (mesh, drape)) {
                    h.push((self.now, (mesh, drape)));
                }
            }
        }
    }

    /// I4: the worst flapping of any tile while still (`flapping_of`).
    pub fn worst_flapping(&self) -> usize {
        self.levels
            .values()
            .map(|h| flapping_of(h.as_slice()))
            .max()
            .unwrap_or(0)
    }

    /// The drapes on the GPU of the tiles the last step drew (CB9).
    pub fn drawn_drapes(&self) -> Vec<ResourceId> {
        self.gpu
            .drawable
            .keys()
            .filter(
                |r| matches!(r, ResourceId::Drape { tile, .. } if self.drawn_last.contains(tile)),
            )
            .copied()
            .collect()
    }

    /// Runs `script` from `t0` for `secs` seconds of simulated time.
    pub fn run(&mut self, secs: f64, script: &dyn Fn(f64) -> Camera) {
        let end = self.now.plus(Micros((secs * 1e6) as u64));
        while self.now < end {
            let t = self.now.0 as f64 / 1e6;
            self.step(script(t));
        }
    }

    /// Applies a memory signal.
    pub fn pressure(&mut self, p: Pressure) {
        self.budget.set_pressure(p);
        self.pressure_set = p;
    }

    /// Applies a thermal state.
    pub fn thermal(&mut self, t: Thermal) {
        self.budget.set_thermal(t);
    }

    /// Seconds from `since` to the first converged step, if any.
    pub fn converged_after(&self, since: Micros) -> Option<Micros> {
        self.log
            .iter()
            .find(|l| l.now >= since && l.converged)
            .map(|l| l.now.since(since))
    }

    /// From `since`, how long until the budget settles: one simulated second
    /// in which it decides nothing (the view is as good as the budget allows,
    /// and nothing churns).
    pub fn settled_after(&self, since: Micros) -> Option<Micros> {
        // Two idle steps in a row: the acknowledgements of the last plan
        // have landed.
        let mut quiet_from: Option<Micros> = None;
        let mut prev_idle = false;
        for l in self.log.iter().filter(|l| l.now >= since) {
            let both = l.idle && prev_idle;
            prev_idle = l.idle;
            if both {
                let from = *quiet_from.get_or_insert(l.now);
                if l.now.since(from) >= Micros::from_secs(1) {
                    return Some(from.since(since));
                }
            } else {
                quiet_from = None;
            }
        }
        None
    }

    /// The digest of the whole run.
    pub fn run_digest(&self) -> u64 {
        self.log.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, l| {
            (h ^ l.digest).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// A Batch frame: steps with the camera at `cam` until the frame is ready;
    /// returns the draw set's (mesh, drape) pairs and the steps it took.
    pub fn film_frame(
        &mut self,
        cam: Camera,
        max_steps: usize,
    ) -> (Vec<(ResourceId, Option<ResourceId>)>, usize) {
        for k in 0..max_steps {
            let (_, d) = self.step(cam);
            if let Some(e) = &d.fatal {
                self.fail(&format!("I13: the film ended: {e}"));
            }
            if d.frame_ready {
                let drawn = d
                    .draw
                    .items
                    .iter()
                    .map(|i| (i.mesh, i.drapes.first().map(|x| x.resource)))
                    .collect();
                let release = self.budget.release_beyond(Bytes::mib(256));
                for r in &release.gpu {
                    if let Some(b) = self.gpu.drawable.remove(r) {
                        let c = class_of(*r);
                        self.gpu.bytes[c] = self.gpu.bytes[c].minus(b);
                    }
                }
                return (drawn, k + 1);
            }
        }
        self.fail("I13: the frame never became ready")
    }
}

/// Covered: the tile or an ancestor drawn, or all its children covered
/// (three levels down at most).
fn covered(d: &Decisions, t: TileId) -> bool {
    let mut up = Some(t);
    while let Some(x) = up {
        if d.draw.draws(x) {
            return true;
        }
        up = parent(x);
    }
    down(d, t, 3)
}

fn down(d: &Decisions, t: TileId, depth: u32) -> bool {
    depth > 0
        && children(t)
            .iter()
            .all(|&k| d.draw.draws(k) || down(d, k, depth - 1))
}

fn class_of(r: ResourceId) -> Class {
    match r {
        ResourceId::Mesh(t) if super::world::is_city(t) => Class::Tiles3dMesh,
        ResourceId::Mesh(_) => Class::TerrainMesh,
        ResourceId::Drape { .. } => Class::Drape,
    }
}

/// One tile's levels while the camera is still: each (mesh level, drape
/// level) it showed, with the time it began (I4).
pub type LevelHistory = Vec<(Micros, (u32, u32))>;

/// I4's measure on one tile's history while still — each (mesh level,
/// drape level) it showed, with the time it began, the first as first
/// drawn: the most returns within any 10 s window, a return being a change
/// back to a level the tile showed and left less than 10 s before
/// (A -> B -> A). A tile that only goes one way (a refinement through a
/// stand-in, coarse to fine) or changes once does not flap.
pub fn flapping_of(history: &[(Micros, (u32, u32))]) -> usize {
    let window = Micros::from_secs(10);
    let returns: Vec<Micros> = (1..history.len())
        .filter(|&j| {
            let (t, level) = history[j];
            // Shown at `i`, left at `i + 1`, before the change `j - 1`
            // that led away from what `j` returns from.
            (0..j - 1).any(|i| history[i].1 == level && t.since(history[i + 1].0) < window)
        })
        .map(|j| history[j].0)
        .collect();
    returns
        .iter()
        .enumerate()
        .map(|(i, t)| {
            returns[i..]
                .iter()
                .take_while(|u| u.since(*t) < window)
                .count()
        })
        .max()
        .unwrap_or(0)
}

/// I4 is flapping, not refinement: a tile that only goes finer while the
/// camera is still — the floor's drape, its parent's (the stand-in), then
/// its own, within a third of a second, as `budget_sim`'s still views do
/// with the stand-in — does not flap.
#[test]
fn i4_a_monotone_refinement_is_not_flapping() {
    let ms = |m: u64| Micros(m * 1_000);
    let h = [(ms(17), (11, 10)), (ms(33), (11, 11)), (ms(267), (11, 12))];
    assert_eq!(flapping_of(&h), 0, "a refinement counted as flapping");
}

/// I4 still catches a flap: a tile that goes back to a level it left,
/// A -> B -> A, within 10 s while the camera is still.
#[test]
fn i4_a_return_to_a_level_left_is_flapping() {
    let s = |x: u64| Micros::from_secs(x);
    let h = [(s(0), (11, 12)), (s(2), (11, 11)), (s(5), (11, 12))];
    assert!(flapping_of(&h) >= 1, "a flap A -> B -> A not counted");
    // The same return after 10 s is not within the window.
    let late = [(s(0), (11, 12)), (s(2), (11, 11)), (s(13), (11, 12))];
    assert_eq!(flapping_of(&late), 0, "a return after 10 s counted");
}
