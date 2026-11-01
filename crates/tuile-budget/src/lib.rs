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
mod downstream;
mod engine;
pub mod error;
mod held;
pub mod imagery;
pub mod need;
mod need_state;
pub mod observe;
pub mod priority;
#[doc(hidden)]
pub mod snapshot;
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
    Decisions, DrapeChoice, Drapes, DrawItem, DrawSet, Role, Serves, Start, SwapGroup, UploadPlan,
};
pub use error::{AuditError, ConfigError, SessionError};
pub use imagery::{DrapeCandidate, GeoRect, LevelClamp, TexelFootprint, UvTransform};
pub use need::{ImageryNeed, LayerReach, Membership, Need, NeedDelta, NeedSet};
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
    /// What the traversal wants: the whole need, or what changed in it.
    pub need: NeedInput<'a>,
    /// The local stores' presence index.
    pub presence: &'a dyn PresenceOracle,
}

/// What a step is told of the need.
#[derive(Clone, Copy, Debug)]
pub enum NeedInput<'a> {
    /// The whole need: what the budget kept of the last one is replaced.
    /// The first step, a resynchronisation, or a host that only has whole
    /// needs.
    Full(&'a NeedSet),
    /// What changed since the need of the step before, which the budget
    /// kept: applied to it, it gives this step's need. Only valid when that
    /// step was this budget's, and the delta is from its need.
    Delta(&'a NeedDelta),
}

static NO_STORE: NoStore = NoStore;

impl<'a> FrameInput<'a> {
    /// A step at `now` for the whole need `need`, with no local store.
    pub fn new(frame: u64, now: Micros, need: &'a NeedSet) -> Self {
        FrameInput {
            frame,
            now,
            need: NeedInput::Full(need),
            presence: &NO_STORE,
        }
    }

    /// A step at `now` for the need of the step before changed by `delta`,
    /// with no local store. It decides exactly as [`FrameInput::new`] on
    /// the whole need the delta leads to, without reading what did not
    /// change.
    pub fn from_delta(frame: u64, now: Micros, delta: &'a NeedDelta) -> Self {
        FrameInput {
            frame,
            now,
            need: NeedInput::Delta(delta),
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

/// The most resources the budget keeps backing off after a failed load once
/// the view has left them (for 30 s at most each), so that a server that is
/// down is not asked again the moment a tile comes back into view. Past
/// it, the ones the view left longest are forgotten first: a want that
/// comes back retries at once. What one costs the budget is its metadata
/// alone — its slot in the arena's columns, its entry in the arena's index,
/// in the prune's kept map and in its queue, and the entry of its tile's
/// place, which it keeps alive, [`BACKOFF_KEPT_BYTES`] — and the cap holds
/// those under 128 KiB, whatever the network does. Left out, bounded
/// multiples of the same count: the index's spare room (hashbrown keeps its
/// table under 7/8 full, at a power of two: up to twice its entries'
/// bytes) and the queue's stale entries (at most twice the live ones, plus
/// 64). The resources the view still wants are its own: counted with the
/// need, not here.
pub const BACKOFF_KEEP_MAX: usize = (128 << 10) / BACKOFF_KEPT_BYTES;

/// What one resource kept backing off costs: its slot across the arena's
/// columns, its index entry (a slot and a control byte, hashbrown's), the
/// kept map's entry (slot and time), its entry in the queue by due time
/// (time and slot), and its tile's place (its `TileInfo` and its key in the
/// tiles' map), kept as long as a resource of the tile is tracked — at
/// worst one tile per kept resource.
pub const BACKOFF_KEPT_BYTES: usize = state::SLOT_BYTES
    + 5
    + 12
    + 16
    + std::mem::size_of::<TileInfo>()
    + std::mem::size_of::<TileId>();

/// What the budget knows of a tile's place in its tree: its parent, level
/// and footprint, packed (48 bytes, not 64: the budget keeps one per tile
/// it knows of), and how many resources of the tile the arena tracks (for
/// the prune: a tile with one is kept).
#[derive(Clone, Copy, Debug, Default)]
struct TileInfo {
    /// The parent's id, when `has_parent`.
    parent: u64,
    /// West, south, east, north, when `has_rect`.
    rect: [f64; 4],
    level: u32,
    has_parent: bool,
    has_rect: bool,
    /// The tile's tracked resources (saturating: the prune then falls back
    /// on its full walk, `downstream::Support`).
    slots: u16,
}

impl TileInfo {
    fn new(parent: Option<TileId>, level: u32, rect: Option<GeoRect>) -> Self {
        TileInfo {
            parent: parent.map_or(0, |p| p.0),
            rect: rect.map_or([0.0; 4], |r| [r.west, r.south, r.east, r.north]),
            level,
            has_parent: parent.is_some(),
            has_rect: rect.is_some(),
            slots: 0,
        }
    }

    fn parent(&self) -> Option<TileId> {
        self.has_parent.then_some(TileId(self.parent))
    }

    fn rect(&self) -> Option<GeoRect> {
        let [west, south, east, north] = self.rect;
        self.has_rect.then_some(GeoRect {
            west,
            south,
            east,
            north,
        })
    }
}

/// Bit for bit: a position told again is the same position.
impl PartialEq for TileInfo {
    fn eq(&self, o: &Self) -> bool {
        let key = |i: &Self| (i.parent(), i.level, i.rect.map(f64::to_bits), i.has_rect);
        key(self) == key(o)
    }
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
    /// Every tile the budget knows the place of: what the need told it, as
    /// long as the tile or a descendant is referenced. Changed only by what
    /// the need changes.
    tiles: sorted::SortedMap<TileId, TileInfo>,
    /// The need last read, each wanted tile's best entry: a step changes
    /// it by what changed.
    need: need_state::NeedState,
    /// Each drawn or blocking tile's imagery level per layer, as last
    /// chosen (the hysteresis reads it), and those of the tiles held.
    imagery_prev: std::collections::BTreeMap<(TileId, LayerId), u32>,
    /// The tiles whose meshes were drawn last step as themselves or held
    /// (not as a fallback).
    held_last: sorted::SortedMap<TileId, ()>,
    /// Resources loading or in transit (an index, so a step need not scan),
    /// each marked `Active` on its slot.
    active: std::collections::BTreeSet<ResourceId>,
    /// Resources ranked last step (their urgency is cleared next step).
    ranked_last: Vec<ResourceId>,
    /// Steps taken, for the amortised prune.
    steps: u64,
    /// What the passes after the need keep from one step to the next: the
    /// draw set, the wants, the work, and what changed (O1.4).
    down: downstream::Downstream,
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
            need: Default::default(),
            imagery_prev: Default::default(),
            held_last: Default::default(),
            active: Default::default(),
            ranked_last: Vec::new(),
            steps: 0,
            down: Default::default(),
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
    /// [`Budget::decide_into`] with a fresh `Decisions`.
    pub fn decide(&mut self, input: FrameInput<'_>) -> Decisions {
        let mut d = Decisions::default();
        self.decide_into(input, &mut d);
        d
    }

    /// One decision step, its decisions written into `out`: emptied first,
    /// its buffers kept — the lists, the plan's items and groups, the draw
    /// set's items, each overwritten in place. A host that hands the same
    /// `Decisions` to every step gives the step nothing to allocate for its
    /// output once they have grown to the view. The same decisions as
    /// [`Budget::decide`], bit for bit.
    pub fn decide_into(&mut self, input: FrameInput<'_>, out: &mut Decisions) {
        out.clear();
        self.step(input, out);
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
        // Every copy's freshness moved: the next step reads it again.
        self.down.everything_again();
    }

    /// The platform's memory signal; takes effect at the next step.
    pub fn set_pressure(&mut self, p: Pressure) {
        self.pressure = p;
    }

    /// Re-derives every class's target, overflow and hard limit, and the
    /// decoded tier's target, from a new total, keeping the configured
    /// split: the host calls it when the
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

    /// How many resources the budget has room to track before its state
    /// grows. The room follows the tracked set: it grows with it, and is
    /// given back once the set stayed under a quarter of it for about a
    /// second (then twice the set).
    pub fn tracked_room(&self) -> usize {
        self.arena.room()
    }

    /// The whole internal state, in a canonical form: for the tests that
    /// compare it against a reference step by step. Not part of the API.
    #[doc(hidden)]
    pub fn snapshot(&self) -> snapshot::Snapshot {
        let a = &self.arena;
        // The sets a step keeps, as the marks on their members' slots.
        let marked = |m: state::Mark| -> Vec<ResourceId> {
            let mut v: Vec<ResourceId> = a
                .iter()
                .filter(|&s| a.marked(s, m))
                .map(|s| a.id(s))
                .collect();
            v.sort_unstable();
            v
        };
        let mut resources: Vec<snapshot::ResourceSnapshot> = a
            .iter()
            .map(|s| {
                let id = a.id(s);
                let (level, distance, last_needed) = a.order_fields(s);
                let c = a.cold(s);
                snapshot::ResourceSnapshot {
                    id,
                    class: a.class(s),
                    level,
                    distance,
                    last_needed,
                    first_wanted: c.first_wanted(),
                    unneeded_since: c.unneeded_since(),
                    load: match a.load(s) {
                        state::Load::Idle => None,
                        state::Load::Loading {
                            gen,
                            source,
                            started,
                            epoch,
                        } => Some((gen, source, started, epoch)),
                    },
                    cpu: a
                        .cpu(s)
                        .map(|c| (c.gen, c.cost, c.epoch, c.delivered_before)),
                    transit: a.transit(s).map(|t| (t.gen, t.cost, t.epoch)),
                    gpu: a.gpu(s).map(|g| (g.gen, g.bytes, g.epoch)),
                    presence: c.presence,
                    store_missed: c.store_missed,
                    failures: c.failures,
                    retry_at: c.retry_at,
                    discards: c.discards,
                    deliver_after: c.deliver_after,
                    urgency: a.urgency(s),
                    priority: a.priority(s).map(|p| p.key(id)),
                }
            })
            .collect();
        resources.sort_unstable_by_key(|r| r.id);
        snapshot::Snapshot {
            resources,
            tiles: self
                .tiles
                .iter()
                .map(|(t, i)| (*t, i.parent(), i.level, i.rect().map(snapshot::rect_bits)))
                .collect(),
            imagery_prev: self.imagery_prev.iter().map(|(k, v)| (*k, *v)).collect(),
            held_last: self.held_last.keys().copied().collect(),
            drawn_last: marked(state::Mark::Drawn),
            worn_last: self
                .down
                .items()
                .flat_map(|(m, i)| i.drapes().map(move |d| (*m, *d)))
                .collect(),
            swap_last: marked(state::Mark::Swap),
            chain_last: marked(state::Mark::Chain),
            active: self.active.iter().copied().collect(),
            floor_set: marked(state::Mark::FloorSet),
            ranked_last: self.ranked_last.clone(),
            // Every resource wanted or drawn, tracked or not: the marks are
            // on tracked slots only.
            wanted_last: {
                let mut w: Vec<ResourceId> = self
                    .down
                    .wanted()
                    .chain(marked(state::Mark::Drawn))
                    .collect();
                w.sort_unstable();
                w.dedup();
                w
            },
            recent: self.recent.iter().map(|(k, v)| (*k, *v)).collect(),
            epochs: self.epochs,
            layer_epochs: self.layer_epochs.iter().map(|(k, v)| (*k, *v)).collect(),
            next_gen: self.next_gen,
            last_now: self.last_now,
            scale: self.scale,
            scale_fine: self.scale_fine,
            ledger: self.ledger,
            in_flight: self.in_flight,
            local_reads: self.local_reads,
            underflows: self.underflows,
            fatal: self.fatal.clone(),
            pending: self.pending.clone(),
            pressure: self.pressure,
            thermal: self.thermal,
            factor: self.factor,
            steps: self.steps,
            events: self.events.len(),
        }
    }

    /// Drops the buffers a step reuses from one step to the next, which
    /// have grown to the size of the view: a host freeing memory (sent to
    /// the background) can give them back; the next step grows them again.
    /// Decides nothing.
    pub fn release_scratch(&mut self) {
        self.scratch = Default::default();
        self.need.release_scratch();
        self.down.release_scratch();
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
