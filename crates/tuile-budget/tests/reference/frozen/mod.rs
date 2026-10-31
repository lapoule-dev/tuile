// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// FROZEN REFERENCE — today's budget engine, frozen at commit 3d1a028e
// ("refactor(budget): the budget is a crate of its own, tuile-budget").
//
// What it is: a mechanical copy of the decision path of `tuile-budget` — the
// `Budget` struct and its methods (src/lib.rs from `enum Event` to the end),
// src/engine.rs, src/state.rs and src/sorted.rs — kept beside the real one as
// the oracle of the O1 optimisation pass (plan O1, "guard first"). The real
// `Budget` and this one are driven step by step on the same inputs by
// `super::Lockstep`, and every step's `Decisions` and ledger must agree.
//
// It shares the crate's public data types (`NeedSet`, `BudgetConfig`,
// `Decisions`, `ResourceId`…): only the engine is frozen, so both sides read
// and write the same values.
//
// Never edited, except to follow a deliberate, documented behaviour change of
// the real engine; record each one below, with its PR.
//
// Adaptations to the copy, none of them in the decision path:
// - the public items of src/lib.rs (`FrameInput`, `Ledger`, `Released`) are
//   the real crate's, imported, not copied;
// - the crate-private helpers `types::sat_u32` and `imagery::drape_rank` are
//   copied into the `types` and `imagery` shims below (the rest of those
//   modules is the real crate's, re-exported);
// - the unit tests at the foot of engine.rs and sorted.rs are dropped (they
//   test private helpers, not decisions);
// - `ResourceView` is `#[non_exhaustive]`, so it cannot be built outside the
//   crate: the `view` shim below declares a field-for-field twin of it (the
//   copy's `Budget::resource` answers with the twin, compared field by field).
//
// Behaviour changes followed since the freeze: none.

#![allow(dead_code, unreachable_pub)]
#![deny(clippy::iter_over_hash_type)]
#![deny(clippy::cast_possible_truncation)]

mod engine;
mod sorted;
mod state;

use tuile_budget::AuditError;
use tuile_budget::{config, cost, decisions, error, need, priority, stats, tier};
use tuile_budget::{
    BudgetConfig, BudgetStats, Bytes, Class, ConfigError, Decisions, DrapeChoice, Failure,
    FrameInput, Generation, GeoRect, LayerId, Ledger, Micros, PerClass, Policy, Pressure, Released,
    ResourceId, SessionError, Thermal, TileId,
};

/// `tuile_budget::types`, plus its crate-private helper (copied).
mod types {
    pub use tuile_budget::types::*;

    /// A length as a `u32`, saturating: `as` would wrap above `u32::MAX` on a
    /// 64-bit target and not on wasm32, where a `usize` is 32 bits.
    pub(crate) fn sat_u32(n: usize) -> u32 {
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

/// `tuile_budget::view`, with a twin of the `#[non_exhaustive]` view.
pub mod view {
    pub use tuile_budget::view::{Protection, ResourceState};
    use tuile_budget::{Bytes, Generation, PriorityKey, Tier, Urgency};

    /// Field for field, `tuile_budget::ResourceView` (frozen at 3d1a028e).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ResourceView {
        /// Its state.
        pub state: ResourceState,
        /// The nearest tier holding it.
        pub tier: Tier,
        /// The generation of its latest load, if any.
        pub gen: Option<Generation>,
        /// GPU bytes held.
        pub gpu: Bytes,
        /// Decoded bytes held.
        pub decoded: Bytes,
        /// Why it is protected, if it is.
        pub protected: Option<Protection>,
        /// Its urgency.
        pub urgency: Option<Urgency>,
        /// Its priority key.
        pub priority: Option<PriorityKey>,
    }
}
use view::ResourceView;

/// `tuile_budget::imagery`, plus its crate-private helper (copied).
mod imagery {
    pub use tuile_budget::imagery::*;

    /// The order of [`best_drape`], smaller first: at or above the need,
    /// coarsest first; below it, sharpest; at equal level, the closest owner.
    pub(crate) fn drape_rank(need: u32, x: DrapeCandidate) -> ((u8, u32), u32) {
        let rank = if x.level >= need {
            (0, x.level - need)
        } else {
            (1, need - x.level)
        };
        (rank, x.up)
    }
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
    entries: std::collections::BTreeMap<ResourceId, state::Entry>,
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
            cfg,
            pressure: Pressure::Normal,
            thermal: Thermal::Nominal,
            factor: (1, 1),
            events: Vec::new(),
            entries: Default::default(),
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
        self.entries.len()
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
        self.budget
            .entries
            .get(&ResourceId::Mesh(tile))
            .is_some_and(|e| e.gpu.is_some())
    }

    /// Whether `tile`'s mesh is decoded or on the GPU.
    pub fn loaded(&self, tile: TileId) -> bool {
        self.budget
            .entries
            .get(&ResourceId::Mesh(tile))
            .is_some_and(|e| e.gpu.is_some() || e.cpu.is_some())
    }
}
