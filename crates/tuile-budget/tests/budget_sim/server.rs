// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimServer`: drives the budget exactly as the runtime will — traverse →
//! need → decide → load → deliver → upload → ack — on a stepped clock, and
//! checks the invariants at every step.

use std::collections::{BTreeMap, BTreeSet};

use tuile_budget::{
    Budget, BudgetConfig, BudgetStats, Bytes, Class, Decisions, FrameDeadline, FrameInput,
    Generation, LayerId, Micros, NeedSet, Policy, Pressure, ResourceId, Role, Source, Thermal,
    TileId,
};

use super::gpu::SimGpu;
use super::net::{Arrival, NetProfile, SimNet};
use super::world::{children, parent, sight, traverse, Camera, Lens, Residency, View};

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
        }
    }

    /// A phone: 512 MiB, cellular, 60 fps, 4 s to converge.
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
            converge_within: Micros::from_secs(4),
            eyes: Eye::mono(Lens::portrait()),
            // A quarter of a 60 Hz frame at 1 MiB/ms: just above the cap.
            deadline: FrameDeadline::from_refresh(60, 250, 1 << 20),
            lag: 0,
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

/// What the heap saw during one `decide` call.
#[derive(Clone, Copy, Debug)]
pub struct DecideAllocs {
    /// The step's time.
    pub now: Micros,
    /// The call's own tally.
    pub heap: super::alloc::Tally,
    /// The blocks its output alone takes (a clone of the `Decisions`, with
    /// exact capacities), when [`Sim::count_output`] is set; else 0.
    pub output: u64,
    /// Items drawn.
    pub drawn: u32,
}

impl DecideAllocs {
    /// The blocks `decide` asked for beyond its output: its own scratch.
    pub fn internal(&self) -> u64 {
        self.heap.allocs.saturating_sub(self.output)
    }
}

struct BudgetResidency<'a>(&'a Budget);
impl Residency for BudgetResidency<'_> {
    fn resident(&self, t: TileId) -> bool {
        self.0.residency().resident(t)
    }
}

/// The simulation.
pub struct Sim {
    /// The profile.
    pub profile: Profile,
    /// The budget under test.
    pub budget: Budget,
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
    /// Wanted-to-drawable times of meshes, ms, with their time: the
    /// latency the budget's scheduling adds to the network's (I6).
    pub waits: Vec<(Micros, f64)>,
    /// Wall time of each `decide` call alone, µs, with the step's time.
    pub decide_us: Vec<(Micros, f64)>,
    /// What the heap saw during each `decide` call, with the step's time.
    pub decide_allocs: Vec<DecideAllocs>,
    /// Also count the allocations of the step's output alone (a clone of
    /// its `Decisions`), for [`DecideAllocs::output`]: off by default, it
    /// costs a clone per step.
    pub count_output: bool,
    /// Per tile: (mesh level drawn, drape level worn, last change time),
    /// tracked while the camera is still (I4).
    levels: BTreeMap<TileId, (u32, u32, Vec<Micros>)>,
}

impl Sim {
    /// A simulation on `profile`, seeded.
    pub fn new(profile: Profile, seed: u64) -> Self {
        Sim {
            budget: Budget::new(profile.budget.clone()).expect("a valid profile"),
            net: SimNet::new(profile.net, seed),
            gpu: {
                let mut g = SimGpu::new(profile.deadline, profile.eyes.len());
                g.lagging = profile.lag > 0;
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
            waits: Vec::new(),
            decide_us: Vec::new(),
            decide_allocs: Vec::new(),
            count_output: false,
            levels: BTreeMap::new(),
            profile,
        }
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
                    self.budget.on_loaded(r, g, b);
                }
                Arrival::Failed(r, g) => {
                    self.budget
                        .on_failed(r, g, tuile_budget::Failure::Transient)
                }
                Arrival::StoreMiss(r, g) => self.budget.on_store_miss(r, g),
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
        let need = traverse(
            &views,
            &BudgetResidency(&self.budget),
            self.sse_scale,
            drawn_last,
            self.pinned(),
        );
        let input = FrameInput::new(self.frame, self.now, &need).with_presence(&self.net.store);
        let clock = std::time::Instant::now();
        let (d, heap) = super::alloc::counted(|| self.budget.decide(input));
        self.decide_us
            .push((self.now, clock.elapsed().as_secs_f64() * 1e6));
        let output = if self.count_output {
            super::alloc::counted(|| d.clone()).1.allocs
        } else {
            0
        };
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
            self.check_executor(&d);
        }
        let mut seen = BTreeSet::new();
        for (t, _) in need.drawn.iter().chain(&need.blockers) {
            let r = ResourceId::Mesh(*t);
            seen.insert(r);
            if !self.gpu.drawable.contains_key(&r) {
                self.wanted_at.entry(r).or_insert(self.now);
            }
        }
        self.wanted_at.retain(|r, _| seen.contains(r));
        for (r, g, b) in acks {
            if let Some(at) = self.wanted_at.remove(&r) {
                self.waits
                    .push((self.now, self.now.since(at).0 as f64 / 1_000.0));
            }
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
    fn check_executor(&self, d: &Decisions) {
        for c in Class::ALL {
            let pending = self
                .gpu
                .pending
                .keys()
                .filter(|r| class_of(**r) == c)
                .fold(Bytes::ZERO, |a, r| a.plus(super::world::cost(*r).gpu));
            let held = self.gpu.bytes[c].plus(pending);
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

    /// The per-step invariants.
    fn check(&self, need: &NeedSet, d: &Decisions, views: &[View]) {
        let s = &d.stats;
        let cfg = &self.profile.budget;
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
        // I1: once the floor is in, nothing is lost and the ground is covered.
        if self.floor_in {
            if s.lost != 0 {
                self.fail(&format!("I1: {} wanted tiles lost", s.lost));
            }
            for (t, _) in &need.drawn {
                if !covered(d, *t) {
                    self.fail(&format!("I1: {t:?} uncovered"));
                }
            }
            // I17: and in each eye, against that eye's own frustum.
            if views.len() > 1 {
                for (e, v) in views.iter().enumerate() {
                    for (t, _) in &need.drawn {
                        if sight(v, *t, 0.0).visible && !covered(d, *t) {
                            self.fail(&format!("I17: eye {e}: {t:?} uncovered"));
                        }
                    }
                }
            }
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
                || e.tile().terrain_coord().0 <= self.pinned() && cfg.pinned_level.is_some()
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
                let e = self.levels.entry(*t).or_insert((mesh, drape, Vec::new()));
                if (e.0, e.1) != (mesh, drape) {
                    e.0 = mesh;
                    e.1 = drape;
                    e.2.push(self.now);
                }
            }
        }
    }

    /// I4: the most level changes of any tile within any 10 s window while
    /// still.
    pub fn worst_flapping(&self) -> usize {
        self.levels
            .values()
            .map(|(_, _, changes)| {
                changes
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        changes[i..]
                            .iter()
                            .take_while(|u| u.since(*t) < Micros::from_secs(10))
                            .count()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0)
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
                        let c = match r {
                            ResourceId::Mesh(_) => Class::TerrainMesh,
                            ResourceId::Drape { .. } => Class::Drape,
                        };
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
        ResourceId::Mesh(_) => Class::TerrainMesh,
        ResourceId::Drape { .. } => Class::Drape,
    }
}
