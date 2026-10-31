// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]
// Determinism guards (A3.f): a hash container's order is seeded per process
// and differs between targets, so nothing here may iterate one; and a
// `usize` is 32 bits on wasm32, so no length is narrowed by `as` (see
// `types::sat_u32`).
#![deny(clippy::iter_over_hash_type)]
#![deny(clippy::cast_possible_truncation)]

pub mod config;
pub mod cost;
pub mod deadline;
pub mod decisions;
mod engine;
pub mod error;
pub mod imagery;
pub mod need;
pub mod observe;
pub mod priority;
mod sorted;
mod state;
pub mod stats;
pub mod tier;
pub mod tile_id;
pub mod types;
pub mod view;

pub use config::{BudgetConfig, FailurePolicy, Pool, PressureFractions, Relaxation, Watermarks};
pub use deadline::FrameDeadline;
pub use decisions::{
    Decisions, DrapeChoice, DrawItem, DrawSet, Role, Start, SwapGroup, UploadPlan,
};
pub use error::{AuditError, ConfigError, SessionError};
pub use imagery::{DrapeCandidate, GeoRect, LevelClamp, TexelFootprint, UvTransform};
pub use need::{ImageryNeed, LayerReach, Membership, Need, NeedSet};
pub use observe::{
    check, BudgetEvent, BudgetMeter, BudgetObserver, BudgetSignals, EyeCoverage, FrameRecord,
    Histogram, StepRecord, Violations,
};
pub use priority::{PriorityKey, Quantised, Urgency};
pub use stats::{BudgetStats, CancelReason, EvictReason};
pub use tier::{NoStore, Presence, PresenceOracle, Source, Tier};
pub use types::{
    Bytes, Class, Failure, Generation, LayerId, Micros, PerClass, Policy, Pressure, ResourceId,
    Thermal, TileId,
};
pub use view::{Protection, ResourceState, ResourceView};

/// One decision step's input.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct FrameInput<'a> {
    /// The frame number (Batch: the film frame).
    pub frame: u64,
    /// The time of this step, simulated or real. Never read inside.
    pub now: Micros,
    /// What the traversal wants.
    pub need: &'a NeedSet,
    /// The local stores' presence index.
    pub presence: &'a dyn PresenceOracle,
}

static NO_STORE: NoStore = NoStore;

impl<'a> FrameInput<'a> {
    /// A step at `now` for `need`, with no local store.
    pub fn new(frame: u64, now: Micros, need: &'a NeedSet) -> Self {
        FrameInput {
            frame,
            now,
            need,
            presence: &NO_STORE,
        }
    }

    /// The same step, asking `presence` about local stores.
    pub fn with_presence(mut self, presence: &'a dyn PresenceOracle) -> Self {
        self.presence = presence;
        self
    }
}

/// The ledger: bytes charged, per tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// GPU bytes per class: drawable (actual) plus in transit (estimated).
    pub gpu: PerClass<Bytes>,
    /// Decoded CPU bytes, all classes.
    pub decoded: Bytes,
    /// Bytes delivered and not yet acknowledged.
    pub in_transit: Bytes,
}

/// What [`Budget::release_beyond`] let go of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Released {
    /// The GPU copies the consumer drops (announced as `Evict`).
    pub gpu: Vec<ResourceId>,
    /// The decoded copies the session drops from its decoded tier.
    pub decoded: Vec<ResourceId>,
}

/// An arrival, recorded between steps and applied at the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Loaded(ResourceId, Generation, cost::Cost),
    Failed(ResourceId, Generation, Failure),
    StoreMiss(ResourceId, Generation),
    Drawable(ResourceId, Generation, Bytes),
    Discarded(ResourceId, Generation),
}

/// Recently evicted resources, for the thrash count, bounded.
const RECENT_CAP: usize = 4_096;
/// The thrash window.
const THRASH_WINDOW: Micros = Micros::from_secs(10);

/// What the budget knows of a tile's place in its tree.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TileInfo {
    parent: Option<TileId>,
    level: u32,
    rect: Option<GeoRect>,
}

/// The budget. See the [module documentation](self).
#[derive(Debug)]
pub struct Budget {
    cfg: BudgetConfig,
    pressure: Pressure,
    thermal: Thermal,
    /// `retarget`: a factor `num / den` on targets, overflows, hard limits.
    factor: (u64, u64),
    events: Vec<Event>,
    /// Every tracked resource's state, by slot.
    arena: state::Arena,
    tiles: std::collections::BTreeMap<TileId, TileInfo>,
    imagery_prev: std::collections::BTreeMap<(TileId, LayerId), u32>,
    held_last: sorted::SortedSet<TileId>,
    drawn_last: sorted::SortedSet<ResourceId>,
    /// The drapes each drawn mesh wore last step, by mesh, in its stacking
    /// order: on a layer switch, the old layer stays worn until the new one
    /// is drawable.
    worn_last: Vec<(ResourceId, DrapeChoice)>,
    swap_last: sorted::SortedSet<ResourceId>,
    chain_last: sorted::SortedSet<ResourceId>,
    /// Resources loading or in transit (an index, so a step need not scan).
    active: std::collections::BTreeSet<ResourceId>,
    /// Resources of the floor (an index).
    floor_set: std::collections::BTreeSet<ResourceId>,
    /// Resources ranked last step (their urgency is cleared next step).
    ranked_last: Vec<ResourceId>,
    /// Steps taken, for the amortised prune.
    steps: u64,
    wanted_last: sorted::SortedSet<ResourceId>,
    /// A step's buffers, kept across steps.
    scratch: engine::Scratch,
    recent: std::collections::BTreeMap<ResourceId, Micros>,
    epochs: PerClass<u64>,
    /// Per imagery layer, added to the drape class's epoch.
    layer_epochs: std::collections::BTreeMap<LayerId, u64>,
    next_gen: u64,
    last_now: Option<Micros>,
    scale: u32,
    scale_fine: u64,
    ledger: Ledger,
    in_flight: PerClass<u32>,
    local_reads: u32,
    /// Load slots freed below zero (`engine::release`): never, unless an
    /// invariant broke; `audit` reports it.
    underflows: u32,
    fatal: Option<SessionError>,
    /// The statistics of arrivals applied ahead of their step
    /// ([`Budget::apply_arrivals`]), reported by it.
    pending: BudgetStats,
}

impl Budget {
    /// A budget running on `cfg`.
    pub fn new(cfg: BudgetConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Budget {
            arena: state::Arena::new(cfg.pinned_level),
            cfg,
            pressure: Pressure::Normal,
            thermal: Thermal::Nominal,
            factor: (1, 1),
            events: Vec::new(),
            tiles: Default::default(),
            imagery_prev: Default::default(),
            held_last: Default::default(),
            drawn_last: Default::default(),
            worn_last: Vec::new(),
            swap_last: Default::default(),
            chain_last: Default::default(),
            active: Default::default(),
            floor_set: Default::default(),
            ranked_last: Vec::new(),
            steps: 0,
            wanted_last: Default::default(),
            scratch: Default::default(),
            recent: Default::default(),
            epochs: PerClass::splat(0),
            layer_epochs: Default::default(),
            next_gen: 1,
            last_now: None,
            scale: 1_000,
            scale_fine: 1_000_000_000,
            ledger: Ledger::default(),
            in_flight: PerClass::splat(0),
            local_reads: 0,
            underflows: 0,
            fatal: None,
            pending: BudgetStats::default(),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &BudgetConfig {
        &self.cfg
    }

    /// One decision step: applies the recorded arrivals, then decides.
    pub fn decide(&mut self, input: FrameInput<'_>) -> Decisions {
        self.step(input)
    }

    /// Applies the arrivals recorded since the last step now, at `now`, so
    /// that [`Budget::residency`] answers with them: the step's traversal
    /// then walks what this step's acknowledgements made drawable, rather
    /// than what the last step saw. The next [`Budget::decide`] — at the
    /// same `now` — decides exactly as if it had applied them itself, and
    /// reports them in its statistics.
    pub fn apply_arrivals(&mut self, now: Micros) {
        let mut st = std::mem::take(&mut self.pending);
        self.apply_events(now, &mut st);
        self.pending = st;
    }

    /// A load finished decoding; `cost` is the estimator's
    /// ([`cost`]), for the decoded copy, the GPU and staging.
    pub fn on_loaded(&mut self, r: ResourceId, gen: Generation, cost: cost::Cost) {
        self.events.push(Event::Loaded(r, gen, cost));
    }

    /// A load failed.
    pub fn on_failed(&mut self, r: ResourceId, gen: Generation, why: Failure) {
        self.events.push(Event::Failed(r, gen, why));
    }

    /// A local read found nothing despite a `Stored` hint: the resource
    /// degrades to a network fetch, once.
    pub fn on_store_miss(&mut self, r: ResourceId, gen: Generation) {
        self.events.push(Event::StoreMiss(r, gen));
    }

    /// The consumer uploaded a delivered resource; `actual` GPU bytes. Only
    /// from this acknowledgement on is the resource drawn.
    pub fn on_drawable(&mut self, r: ResourceId, gen: Generation, actual: Bytes) {
        self.events.push(Event::Drawable(r, gen, actual));
    }

    /// The consumer dropped a delivered resource without uploading it (an
    /// upload that failed): it stays undrawn, and is delivered again later.
    pub fn on_discarded(&mut self, r: ResourceId, gen: Generation) {
        self.events.push(Event::Discarded(r, gen));
    }

    /// The content of `class` changed upstream (imagery epoch, radiometric
    /// or source update): every resident resource of the class — of
    /// `layer` only, for a drape layer's update (D-A4) — is refreshed at the
    /// lowest urgency, new before old, without a hole.
    pub fn new_epoch(&mut self, class: Class, layer: Option<LayerId>) {
        match (class, layer) {
            (Class::Drape, Some(l)) => *self.layer_epochs.entry(l).or_insert(0) += 1,
            _ => self.epochs[class] += 1,
        }
    }

    /// The platform's memory signal; takes effect at the next step.
    pub fn set_pressure(&mut self, p: Pressure) {
        self.pressure = p;
    }

    /// Re-derives every class's target, overflow and hard limit from a new
    /// total, keeping the configured split: the host calls it when the
    /// platform's available memory changes (`os_proc_available_memory`,
    /// `VK_EXT_memory_budget`, an Android `onTrimMemory` level, a browser's
    /// device-memory hint). The budget itself knows no platform. Takes effect
    /// at the next step.
    pub fn retarget(&mut self, total: Bytes) {
        // The device's whole: the pool's hard limit once, and the hard
        // limits of the classes outside it.
        let pool = self.cfg.pool;
        let outside = self
            .cfg
            .hard_limit
            .iter()
            .filter(|(c, _)| !pool.holds(*c))
            .fold(Bytes::ZERO, |a, (_, b)| a.plus(b));
        let pooled = Class::ALL.iter().any(|c| pool.holds(*c));
        let den = if pooled {
            outside.plus(pool.hard_limit)
        } else {
            outside
        }
        .0
        .max(1);
        self.factor = (total.0, den);
    }

    /// The platform's thermal state; takes effect at the next step.
    pub fn set_thermal(&mut self, t: Thermal) {
        self.thermal = t;
    }

    /// Batch, between frames: lets go of what the last step did not need,
    /// in eviction order, until the GPU and the decoded tier each hold at
    /// most `target` bytes. Each resource let go is let go whole, both its
    /// copies: a batch's decoded target is unbounded, so a copy left there
    /// would never go, and an in-process consumer's copy is the allocation
    /// the decoded one shares. Deterministic.
    pub fn release_beyond(&mut self, target: Bytes) -> Released {
        self.release_beyond_impl(target)
    }

    /// The traversal's residency: drawable (Interactive) or loaded (Batch).
    pub fn residency(&self) -> ResidencyView<'_> {
        ResidencyView { budget: self }
    }

    /// One resource's state, if the budget tracks it.
    pub fn resource(&self, r: ResourceId) -> Option<ResourceView> {
        self.view(r)
    }

    /// The ledger.
    pub fn ledger(&self) -> Ledger {
        self.ledger
    }

    /// Recomputes the ledger from the resource states and compares.
    pub fn audit(&self) -> Result<(), AuditError> {
        self.audit_impl()
    }

    /// How many resources the budget tracks.
    pub fn tracked(&self) -> usize {
        self.arena.len()
    }

    /// Drops the buffers a step reuses from one step to the next, which
    /// have grown to the size of the view: a host freeing memory (sent to
    /// the background) can give them back; the next step grows them again.
    /// Decides nothing.
    pub fn release_scratch(&mut self) {
        self.scratch = Default::default();
    }
}

/// The residency the traversal reads.
#[derive(Clone, Copy, Debug)]
pub struct ResidencyView<'a> {
    budget: &'a Budget,
}

impl ResidencyView<'_> {
    /// Whether the traversal may treat `tile` as resident: its mesh is
    /// drawable (Interactive), or loaded (Batch).
    pub fn resident(&self, tile: TileId) -> bool {
        match self.budget.cfg.policy {
            Policy::Interactive => self.drawable(tile),
            Policy::Batch => self.loaded(tile),
        }
    }

    /// Whether `tile`'s mesh is on the GPU, acknowledged.
    pub fn drawable(&self, tile: TileId) -> bool {
        let a = &self.budget.arena;
        a.find(ResourceId::Mesh(tile)).is_some_and(|s| a.has_gpu(s))
    }

    /// Whether `tile`'s mesh is decoded or on the GPU.
    pub fn loaded(&self, tile: TileId) -> bool {
        let a = &self.budget.arena;
        a.find(ResourceId::Mesh(tile))
            .is_some_and(|s| a.has_gpu(s) || a.has_cpu(s))
    }
}
