// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The observability contract's core (spec §16): one vocabulary for tests
//! and production.
//!
//! - [`BudgetObserver`]: watch-only, called by the host after each `decide`
//!   ([`StepRecord`]) and after each render frame ([`FrameRecord`]); it
//!   takes nothing back. No I/O, no allocation per step beyond bounded
//!   maps, wasm32.
//! - [`BudgetMeter`]: the observer that accumulates every signal of §16.3,
//!   read as a [`BudgetSignals`] snapshot by the sinks — Prometheus, the
//!   MCAP tape, tracing, the control API, the overlay, the web worker's
//!   channel — and by the tests, under the same names (without the
//!   `tuile_budget_` prefix).
//! - [`check`]: the invariants one record allows, so that a device counts
//!   the violations a test asserts on.
//!
//! Counts and digests come from the decisions and are the same on every
//! platform. Wall-clock measures (`decide_seconds`, `frame_upload_seconds`)
//! are named as such and never feed a decision. Labels take their values
//! from closed sets ([`BudgetSignals::series`]).

use std::collections::{BTreeMap, BTreeSet};

use super::config::BudgetConfig;
use super::cost::Cost;
use super::deadline::FrameDeadline;
use super::decisions::{Decisions, Role};
use super::need::NeedSet;
use super::score::DISTANCE_BANDS;
use super::stats::{
    CancelReason, EvictReason, LayerStats, LevelStats, GAP_BUCKETS, LATENCY_BUCKETS, LAYERS, LEVELS,
};
use super::tier::Presence;
use super::types::{
    Bytes, Class, Failure, Generation, LayerId, Micros, PerClass, Policy, Pressure, ResourceId,
    Thermal, TileId,
};
use super::Ledger;

/// What the host handed the budget between two steps, recorded as it calls
/// it. The order does not matter: the budget applies arrivals canonically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BudgetEvent {
    /// `on_loaded`.
    Loaded(ResourceId, Generation, Cost),
    /// `on_failed`.
    Failed(ResourceId, Generation, Failure),
    /// `on_store_miss`.
    StoreMiss(ResourceId, Generation),
    /// `on_drawable`.
    Drawable(ResourceId, Generation, Bytes),
    /// `on_discarded`.
    Discarded(ResourceId, Generation),
    /// `set_pressure`.
    Pressure(Pressure),
    /// `set_thermal`.
    Thermal(Thermal),
    /// `retarget`.
    Retarget(Bytes),
    /// `reserve`: what the executor holds of a class outside any resource
    /// from this step on.
    Reserved(Class, Bytes),
    /// `new_epoch`.
    NewEpoch(Class, Option<LayerId>),
    /// `release_beyond`, and how many GPU copies it dropped.
    ReleaseBeyond(Bytes, u32),
    /// `specify`: the kind and the digest of the specification given (its
    /// whole text travels in the host's record, as its query), and whether
    /// a diagnostic override gave it rather than the host's context.
    Specify(crate::spec::SpecKind, u64, bool),
    /// `measure`: the samples given.
    Measured(crate::inputs::Measured),
    /// A fallback specification put in force: its digest
    /// ([`crate::FallbackSpec::digest`]).
    Fallbacks(u64),
    /// `on_compressed` (CB9).
    Compressed(ResourceId, Generation, Bytes),
    /// The numbers predicting the views ahead changed: their digest
    /// (`tuile_camera::predict::PredictConfig::digest`), as the consumer
    /// sent it with them.
    Predictor(u64),
}

/// One decision step, as the host saw it.
#[derive(Clone, Copy, Debug)]
pub struct StepRecord<'a> {
    /// The step's ordinal, from 0.
    pub step: u64,
    /// The frame number given to `decide`.
    pub frame: u64,
    /// The step's time.
    pub now: Micros,
    /// Some eye's pose or lens moved since the last step.
    pub views_changed: bool,
    /// How many views the need was built from.
    pub eyes: u8,
    /// The need `decide` read.
    pub need: &'a NeedSet,
    /// The events applied by this step.
    pub events: &'a [BudgetEvent],
    /// The presence answers the step used.
    pub presence: &'a [(ResourceId, Presence)],
    /// The decisions, statistics included.
    pub decisions: &'a Decisions,
    /// The ledger after the step.
    pub ledger: Ledger,
    /// The platform's memory signal.
    pub pressure: Pressure,
    /// The platform's thermal state.
    pub thermal: Thermal,
    /// The wall time around `decide`, measured by the host.
    pub decide_wall: Option<Micros>,
}

/// One eye's picture in a render frame: wanted tiles inside its frustum
/// with no cover, and drawn items without their drapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EyeCoverage {
    /// Wanted tiles inside the eye's frustum with nothing drawn over them.
    pub lost: u32,
    /// Items the eye drew bare.
    pub bare: u32,
}

/// One render frame, as the executor saw it.
#[derive(Clone, Copy, Debug)]
pub struct FrameRecord<'a> {
    /// The render frame.
    pub frame: u64,
    /// The step whose draw set it drew.
    pub generation: u64,
    /// Its upload deadline.
    pub deadline: FrameDeadline,
    /// What it uploaded, with the actual bytes.
    pub uploaded: &'a [(ResourceId, Bytes)],
    /// Plan items its byte lane deferred: what did not fit its deadline.
    pub deferred: u32,
    /// Plan items its encode lane deferred: what did not fit the encodes'
    /// share, whole swap groups. Counted apart from [`Self::deferred`].
    pub deferred_encode: u32,
    /// Imagery tiles it encoded on the GPU, and the 4×4 blocks they wrote.
    pub encoded_tiles: u32,
    pub encode_blocks: u64,
    /// The encodes' GPU time: measured by the executor (timestamps, or the
    /// blocks times its measured cost of a block). Zero when nothing is
    /// encoded.
    pub encode_gpu: Micros,
    /// The encode lane's share this frame, blocks (`u64::MAX`: no lane),
    /// and its carry after the frame's encodes — negative, the debt.
    pub encode_share: u64,
    pub encode_carry: i64,
    /// The wall time its uploads took.
    pub upload_wall: Micros,
    /// Per eye, at most four.
    pub eyes: &'a [EyeCoverage],
}

/// Watch-only: called by the host, takes nothing back (the pattern of
/// `tuile-film-native`'s `Observer`).
pub trait BudgetObserver {
    /// After each `decide`.
    fn step(&mut self, _s: &StepRecord<'_>) {}
    /// After each render frame.
    fn frame(&mut self, _f: &FrameRecord<'_>) {}
}

impl BudgetObserver for () {}

impl BudgetObserver for Vec<Box<dyn BudgetObserver + Send>> {
    fn step(&mut self, s: &StepRecord<'_>) {
        for o in self.iter_mut() {
            o.step(s);
        }
    }
    fn frame(&mut self, f: &FrameRecord<'_>) {
        for o in self.iter_mut() {
            o.frame(f);
        }
    }
}

/// A histogram of integer values (microseconds, bytes, steps) in
/// power-of-two buckets: bucket `i` counts values in `[2^(i-1), 2^i)`,
/// bucket 0 the zeros. Deterministic: no float decides a bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram {
    /// The counts per bucket.
    pub buckets: [u64; 64],
    /// The sum of the values.
    pub sum: u64,
    /// How many values.
    pub count: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: [0; 64],
            sum: 0,
            count: 0,
        }
    }
}

impl Histogram {
    /// Records `v`.
    pub fn record(&mut self, v: u64) {
        let i = (64 - v.leading_zeros()) as usize;
        self.buckets[i.min(63)] += 1;
        self.sum = self.sum.saturating_add(v);
        self.count += 1;
    }

    /// The upper bound of the bucket holding the `q`-th quantile
    /// (`q` in thousandths), or 0 when empty.
    pub fn quantile_upper(&self, q_permille: u32) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = (self.count * u64::from(q_permille.min(1_000))).div_ceil(1_000);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank.max(1) {
                return if i == 0 { 0 } else { (1u64 << i.min(63)) - 1 };
            }
        }
        u64::MAX
    }
}

/// The invariants one record allows, as `check` counts them, indexed by
/// invariant: `[0]` is I1, `[17]` I18.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Violations {
    /// Counts per invariant.
    pub counts: [u64; 18],
}

impl Violations {
    /// The violations of `In` (1-based).
    pub fn of(&self, invariant: usize) -> u64 {
        self.counts
            .get(invariant.wrapping_sub(1))
            .copied()
            .unwrap_or(0)
    }

    fn add(&mut self, invariant: usize) {
        if let Some(c) = self.counts.get_mut(invariant - 1) {
            *c += 1;
        }
    }

    /// No violation at all.
    pub fn none(&self) -> bool {
        self.counts.iter().all(|c| *c == 0)
    }
}

/// The checks one record and the configuration allow (spec §16.2): I1 (lost
/// and bare once the floor is in, bare from the first step), I2 (unprotected
/// bytes over the target, a class over its hard limit), I7 (the heap over
/// its cap, duplicates in flight), I11 (an evicted resource drawn or worn,
/// or a floor resource evicted), I13 (an eviction or an inexact item in a
/// ready Batch frame). What needs the world — each eye's frustum, the
/// network's truth for I5 and I14 — stays in the simulation and the
/// headless harness.
pub fn check(s: &StepRecord<'_>, cfg: &BudgetConfig, out: &mut Violations) {
    let d = s.decisions;
    let st = &d.stats;
    if st.bare > 0 || (st.floor_in && st.lost > 0) {
        out.add(1);
    }
    for c in Class::ALL {
        if st.gpu[c].minus(st.protected[c]) > st.target[c] || st.gpu[c] > st.hard_limit[c] {
            out.add(2);
            break;
        }
    }
    if st.heap_len > cfg.request_heap || st.duplicates_in_flight > 0 {
        out.add(7);
    }
    // What I11 compares evictions with, built only when something is
    // evicted: a still step evicts nothing and allocates nothing here.
    if !d.evict.is_empty() {
        let worn: BTreeSet<ResourceId> = d
            .draw
            .items
            .iter()
            .flat_map(|i| std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource)))
            .collect();
        let floor_tiles: BTreeSet<TileId> = s
            .need
            .entries()
            .filter(|(_, _, n)| {
                n.class.is_ground() && cfg.pinned_level.is_some_and(|p| n.level <= p)
            })
            .map(|(_, t, _)| t)
            .collect();
        for e in &d.evict {
            if worn.contains(e) || floor_tiles.contains(&e.tile()) {
                out.add(11);
                break;
            }
        }
    }
    if cfg.policy == Policy::Batch && d.frame_ready {
        let exact =
            s.need.drawn.iter().all(|(t, _)| {
                d.draw.get(ResourceId::Mesh(*t)).map(|i| i.role) == Some(Role::Exact)
            });
        if !d.evict.is_empty() || !exact || !s.need.blockers.is_empty() {
            out.add(13);
        }
    }
}

/// Every signal of spec §16.3, by its name without the `tuile_budget_`
/// prefix. Gauges are the last step's or frame's; `_total`s count from the
/// meter's start; peaks hold since the last [`BudgetMeter::publish`].
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct BudgetSignals {
    /// `bytes{class,state}`: `[drawable, in_transit]` per class.
    pub bytes: PerClass<[u64; 2]>,
    /// `bytes_by_level{class,state,level}`: `bytes` split by the level the
    /// budget files each resource at, `[drawable, in_transit]` per level.
    pub bytes_by_level: PerClass<[[u64; 2]; LEVELS]>,
    /// The levels of each class that have held bytes since the meter's
    /// start, one bit each: only these are exposed, and once exposed a level
    /// stays so (a gauge left behind would keep its last value).
    pub bytes_levels: PerClass<u32>,
    /// `evictions_by_level_total{class,level}`: the evictions of
    /// `evictions_total`'s sweep, pressure and admission, by the evicted
    /// resource's level. Exposed from a level's first.
    pub evictions_by_level_total: PerClass<[u64; LEVELS]>,
    /// The last step per level ([`LevelStats`]): the gauges
    /// `copies_by_level{class,tier,level}` (gpu, decoded),
    /// `decoded_bytes_by_level{class,level}`,
    /// `tiles_by_level{class,state,level}` (drawn, selected, under_need) and
    /// `loads_by_level{class,state,kind,level}` (in_flight, queued × need,
    /// prefetch), each exposed from the level's first non-zero value, as
    /// `bytes_by_level` is.
    pub levels: LevelStats,
    /// The levels of each class [`Self::levels`] has named since the
    /// meter's start, one bit each.
    pub levels_seen: PerClass<u32>,
    /// The reasons ([`crate::blocked::Blocked`]) each level of each class
    /// has had a tile under its need for since the meter's start, one bit
    /// each: the gauges `blocked_tiles{class,level,reason}` (from
    /// `levels.blocked`) and `blocked_oldest_seconds{class,reason}` (the
    /// oldest of the class's levels, from `levels.blocked_oldest_ms`) are
    /// exposed from a reason's first tile, and stay so.
    pub blocked_seen: PerClass<[u16; LEVELS]>,
    /// `level_events_total{class,kind,level}`, the sums of
    /// [`Self::levels`]' counters: `[start, completion, wasted, release]`.
    pub level_events_total: PerClass<[[u64; 4]; LEVELS]>,
    /// The last step's drapes per imagery layer ([`LayerStats`]), by the
    /// layer's index and never its name: `drapes_by_layer{layer,state}`
    /// (gpu, drawn, in_flight) and `drape_bytes_by_layer{layer}`, exposed
    /// from the layer's first.
    pub layers: LayerStats,
    /// The layers named since the meter's start, one bit each.
    pub layers_seen: u32,
    /// The floor's last level (`BudgetConfig::pinned_level`): the levels
    /// up to it are protected ground.
    pub floor_level: Option<u32>,
    /// `layer_loads_total{layer,kind}`: `[store, network, ram, completion,
    /// failure]` — requests by source, RAM hits, completions, failures.
    pub layer_loads_total: [[u64; 5]; LAYERS],
    /// `decoded_bytes`.
    pub decoded_bytes: u64,
    /// `protected_bytes{class}`.
    pub protected_bytes: PerClass<u64>,
    /// `target_bytes{class}`, effective.
    pub target_bytes: PerClass<u64>,
    /// `overflow_bytes{class}`, effective.
    pub overflow_bytes: PerClass<u64>,
    /// `hard_limit_bytes{class}`, effective.
    pub hard_limit_bytes: PerClass<u64>,
    /// `reserved_bytes{class}`: held by the executor outside any resource
    /// (the encoder's scratch), off the class's limits.
    pub reserved_bytes: PerClass<u64>,
    /// `decoded_reserved_bytes`: off the decoded tier's target.
    pub decoded_reserved_bytes: u64,
    /// `over_target_seconds_total{class}`, in microseconds.
    pub over_target_micros_total: PerClass<u64>,
    /// `lost_tiles`: wanted tiles with no cover, once the floor is in.
    pub lost_tiles: u32,
    /// `lost_tile_steps_total`.
    pub lost_tile_steps_total: u64,
    /// `bare_items`.
    pub bare_items: u32,
    /// `bare_item_steps_total`.
    pub bare_item_steps_total: u64,
    /// `drawn_tiles{role}`: exact, held, fallback.
    pub drawn_tiles: [u32; 3],
    /// `eye_lost_tiles{eye}`.
    pub eye_lost_tiles: [u32; 4],
    /// `eye_bare_items{eye}`.
    pub eye_bare_items: [u32; 4],
    /// `floor_in`.
    pub floor_in: bool,
    /// `floor_seconds`, in microseconds from the first step.
    pub floor_micros: Option<u64>,
    /// `fallback_depth_max`, since the last publish.
    pub fallback_depth_max: u32,
    /// `underdraped_tiles`.
    pub underdraped_tiles: u32,
    /// `imagery_over_terrain{gap}`, gap −8 to 8.
    pub imagery_over_terrain: [u32; GAP_BUCKETS],
    /// `converged`.
    pub converged: bool,
    /// `converged_seconds`: from the views going still to convergence, µs.
    pub converged_micros: Histogram,
    /// `unconverged_seconds`: still and not converged, for how long, µs.
    pub unconverged_micros: u64,
    /// `converge_bound_seconds`: the profile's T_conv, µs (host-set).
    pub converge_bound_micros: u64,
    /// `flaps_total{kind}`: terrain, imagery.
    pub flaps_total: [u64; 2],
    /// `sse_scale`, thousandths.
    pub sse_scale_milli: u32,
    /// `pressure_level`, 0–3.
    pub pressure_level: u8,
    /// `thermal_level`, 0–3.
    pub thermal_level: u8,
    /// `starts_total{class,source}`: `[local, network]`.
    pub starts_total: PerClass<[u64; 2]>,
    /// `wasted_starts_total{class}`.
    pub wasted_starts_total: PerClass<u64>,
    /// `local_waits_total`.
    pub local_waits_total: u64,
    /// `cancels_total{reason}`: grace, timeout, preempted.
    pub cancels_total: [u64; 3],
    /// `completions_total`.
    pub completions_total: u64,
    /// `failures_total{class,kind}`: `[transient, permanent]`.
    pub failures_total: PerClass<[u64; 2]>,
    /// `stale_results_total`.
    pub stale_results_total: u64,
    /// `load_seconds`: load latency, milliseconds, the budget's buckets.
    pub load_millis: [u64; LATENCY_BUCKETS],
    /// `in_flight{class,source="network"}`.
    pub in_flight: PerClass<u32>,
    /// `in_flight{source="local"}`, all classes.
    pub local_reads: u32,
    /// `request_heap_entries`.
    pub request_heap_entries: u32,
    /// `tracked_resources`.
    pub tracked_resources: u32,
    /// `duplicates_in_flight`.
    pub duplicates_in_flight: u32,
    /// `decide_seconds`, wall clock, µs.
    pub decide_micros: Histogram,
    /// `decide_peak_seconds`, since the last publish, µs.
    pub decide_peak_micros: u64,
    /// `decide_share_seconds`: the profile's share of a frame, µs (host-set).
    pub decide_share_micros: u64,
    /// `decide_work`.
    pub decide_work: u32,
    /// `decide_work_total`.
    pub decide_work_total: u64,
    /// `evictions_total{class,reason}`: sweep, pressure, admission,
    /// release_beyond.
    pub evictions_total: PerClass<[u64; 4]>,
    /// `releases_total{class}`.
    pub releases_total: PerClass<u64>,
    /// `thrash_total`.
    pub thrash_total: u64,
    /// `rereads_total`.
    pub rereads_total: u64,
    /// `network_bytes_avoided_total`.
    pub network_bytes_avoided_total: u64,
    /// `store_misses_total`.
    pub store_misses_total: u64,
    /// `upload_plan_bytes`.
    pub upload_plan_bytes: u64,
    /// `uploads_total{class}`.
    pub uploads_total: PerClass<u64>,
    /// `upload_bytes_total{class}`.
    pub upload_bytes_total: PerClass<u64>,
    /// `frame_upload_bytes`.
    pub frame_upload_bytes: Histogram,
    /// `frame_deadline_bytes`.
    pub frame_deadline_bytes: u64,
    /// `deadline_deferrals_total`: the byte lane's deferrals.
    pub deadline_deferrals_total: u64,
    /// `upload_deferred_total{lane}`: plan items a frame deferred, by lane
    /// (`bytes`, `encode`).
    pub upload_deferred_total: [u64; 2],
    /// `encoded_tiles_total`: imagery tiles the executor encoded.
    pub encoded_tiles_total: u64,
    /// `encode_blocks_total`: the 4×4 blocks those encodes wrote.
    pub encode_blocks_total: u64,
    /// `frame_encode_seconds`: a frame's encodes' GPU time, µs, for the
    /// frames that encoded something.
    pub frame_encode_micros: Histogram,
    /// The last frame's encodes: `frame_encoded_tiles`,
    /// `frame_encode_blocks`, `frame_encode_gpu_seconds` (µs).
    pub frame_encoded_tiles: u32,
    pub frame_encode_blocks: u64,
    pub frame_encode_gpu_micros: u64,
    /// The last frame's deferrals by lane: `frame_deferred{lane}`.
    pub frame_deferred: [u32; 2],
    /// The encode lane: `encode_share_blocks` (`u64::MAX` without a
    /// lane) and `encode_carry_blocks` (negative: in debt) after the last
    /// frame.
    pub encode_share_blocks: u64,
    pub encode_carry_blocks: i64,
    /// `transient_bytes{class}`: of the bytes in transit, what encodes on
    /// the GPU hold beside the blocks until they have run.
    pub transient_bytes: PerClass<u64>,
    /// `transfer_bytes{class}`: what the copies in transit cross in.
    pub transfer_bytes: PerClass<u64>,
    /// `frame_upload_seconds`, wall clock, µs.
    pub frame_upload_micros: Histogram,
    /// `estimate_drift_exceeded_total{class}`.
    pub estimate_drift_exceeded_total: PerClass<u64>,
    /// `encode_fallbacks_total{class}`: acknowledgements of an encode that
    /// fell back to the form it crossed in.
    pub encode_fallbacks_total: PerClass<u64>,
    /// `swaps_total{kind}`: refine, coarsen, mesh_drape, layer.
    pub swaps_total: [u64; 4],
    /// `swap_gaps_total`: ground covered at the last step, uncovered now.
    pub swap_gaps_total: u64,
    /// `batch_steps_per_frame`.
    pub batch_steps_per_frame: Histogram,
    /// `batch_fatal_total`.
    pub batch_fatal_total: u64,
    /// `invariant_violations_total{invariant}`, I1 to I18.
    pub invariant_violations_total: [u64; 18],
    /// The progressive score (`docs/explorations/budget-progressive-coarse-preload.md`
    /// §7): `param{name}` and `need_level`, the parameters in force.
    pub score_params: crate::score::ScoreParams,
    /// `preload_by_level_distance{level,distance}`: preloads in flight
    /// after the last step.
    pub preload_by_level_distance: [[u32; DISTANCE_BANDS]; LEVELS],
    /// `evictions_by_level_distance{level,distance}`: the last step's GPU
    /// evictions.
    pub evictions_by_level_distance: [[u32; DISTANCE_BANDS]; LEVELS],
    /// `released_by_level_distance{level,distance,reason}`: the last
    /// step's releases, GPU and decoded, by reason.
    pub released_by_level_distance: [[[u32; DISTANCE_BANDS]; LEVELS]; 3],
    /// `released_total{reason}`.
    pub released_total: [u64; 3],
    /// `score_decisions_total{level,distance,side}` and
    /// `score_sum_total{level,distance,side,term}`: every preload started
    /// and copy evicted since the meter's start, with its terms (sides
    /// preload and evict).
    pub score_total: crate::score::ScoreBoard,
    /// `score_min{…}` and `score_max{…}`: the same since the last publish.
    pub score_recent: crate::score::ScoreBoard,
    /// The present need's waits since the meter's start, first loads and
    /// reloads after an eviction (`crate::waits`): `waits_total{kind,
    /// class,level}`, `wait_p50_ms{kind}`, `wait_p95_ms{kind}` (from the
    /// durations' histogram by kind) and `reloads_total{reason}`.
    pub waits: crate::waits::WaitTotals,
    /// The waits ended since the last publish, by kind, and the time they
    /// span, µs: a rate per second.
    pub waits_recent: [u64; 3],
    pub waits_recent_micros: u64,
    /// `exchange_threshold`: λ, the score at the memory boundary since
    /// the last publish — the highest score released to make room or keep
    /// the target (sweep, pressure, admission, swap): what is held under it
    /// would be released, a candidate over it loaded. `None` without
    /// pressure.
    pub score_lambda: Option<u64>,
    /// The decisions since the last publish, each with its terms, at most
    /// [`SCORE_EVENTS_KEPT`]: the log records' source.
    pub score_events: Vec<crate::score::ScoreEvent>,
    /// `score_events_dropped_total`: decisions not kept in `score_events`.
    pub score_events_dropped_total: u64,
    /// The GPU-held set by score as the host last reported it
    /// ([`BudgetMeter::set_held`]): `score_held_*{level,distance,term}`.
    pub score_held: Option<crate::score::HeldReport>,
    /// `rejected_inputs_total`.
    pub rejected_inputs_total: u64,
    /// `under_need_tiles{kind}`: mesh, imagery — drawn tiles under the level
    /// the step needs (the quality deficit, `BudgetStats::deficit`).
    pub under_need_tiles: [u32; 2],
    /// `over_need_tiles{kind}`.
    pub over_need_tiles: [u32; 2],
    /// `deficit_levels{kind}`: the levels under need, summed.
    pub deficit_levels: [u32; 2],
    /// `deficit_level_seconds_total{kind}`: the deficit integrated over
    /// time, level·µs — the objective's integral.
    pub deficit_level_micros_total: [u64; 2],
    /// `under_need_still_seconds`: the views still and some tile under its
    /// need, for how long, µs (0 once none is).
    pub under_need_still_micros: u64,
    /// `spec_info{spec,source}`: the specification in force (1 for it, 0
    /// for the others), and whether a diagnostic override set it
    /// (`source` = `override`) rather than the host's context.
    pub spec: crate::spec::SpecKind,
    pub spec_overridden: bool,
    /// `spec_changes_total`: re-specifications recorded.
    pub spec_changes_total: u64,
    /// What the session's local store holds, by class, layer and level —
    /// the disk tier of `tuile_tier_*` ([`crate::tiers`]). Filled by the
    /// session from its loader when it publishes; the meter leaves it
    /// empty.
    pub disk: crate::tiers::DiskTiers,
}

/// The closed label sets of spec §16.5.
/// The decisions [`BudgetSignals::score_events`] keeps between two
/// publishes: the log records' rate limit.
pub const SCORE_EVENTS_KEPT: usize = 512;

pub mod labels {
    use super::super::types::Class;

    /// A class's label.
    pub fn class(c: Class) -> &'static str {
        match c {
            Class::TerrainMesh => "terrain_mesh",
            Class::Tiles3dMesh => "tiles3d_mesh",
            Class::Drape => "drape",
            Class::Overlay => "overlay",
        }
    }
    /// `state`.
    pub const STATE: [&str; 2] = ["drawable", "in_transit"];
    /// `source`.
    pub const SOURCE: [&str; 2] = ["local", "network"];
    /// `role`.
    pub const ROLE: [&str; 3] = ["exact", "held", "fallback"];
    /// `reason` of a cancel.
    pub const CANCEL: [&str; 3] = ["grace", "timeout", "preempted"];
    /// `reason` of an eviction.
    pub const EVICT: [&str; 4] = ["sweep", "pressure", "admission", "release_beyond"];
    /// `kind` of a failure.
    pub const FAILURE: [&str; 2] = ["transient", "permanent"];
    /// `kind` of a flap.
    pub const FLAP: [&str; 2] = ["terrain", "imagery"];
    /// `kind` of a deficit.
    pub const DEFICIT: [&str; 2] = ["mesh", "imagery"];
    /// `source` of the specification in force.
    pub const SPEC_SOURCE: [&str; 2] = ["context", "override"];
    /// `lane` of a deferral.
    pub const LANE: [&str; 2] = ["bytes", "encode"];
    /// `kind` of a swap.
    pub const SWAP: [&str; 4] = ["refine", "coarsen", "mesh_drape", "layer"];
    /// `eye`.
    pub const EYE: [&str; 4] = ["0", "1", "2", "3"];
    /// `name` of a `param`: the progressive score's parameters
    /// (`crate::score::ScoreParams::pairs`).
    pub const PARAM: [&str; 11] = [
        "a0_permille",
        "a_desc_permille",
        "rho_tiles",
        "margin_permille",
        "floor_level",
        "preload_slots_permille",
        "need_level",
        "headroom_permille",
        "floor_value_permille",
        "coarse_from",
        "coarse_boost_permille",
    ];
    /// `reason` of `released_by_level_distance`.
    pub const RELEASE: [&str; 3] = crate::score::RELEASE_REASONS;
    /// `distance`: a band of tiles from the view's footprint.
    pub const DISTANCE: [&str; crate::score::DISTANCE_BANDS] = crate::score::DISTANCE_BAND_LABELS;
    /// `side` of the score.
    pub const SIDE: [&str; 4] = ["preload", "evict", "held", "decoded"];
    /// `term` of the score.
    pub const TERM: [&str; 5] = crate::score::TERM_LABELS;
    /// `kind` of `waits_total`, `wait_p50_ms`, `wait_p95_ms`.
    pub const WAIT_KIND: [&str; 3] = crate::waits::WAIT_KINDS;
    /// `reason` of `blocked_tiles` and `blocked_oldest_seconds`.
    pub const BLOCKED: [&str; crate::blocked::BLOCKED] = crate::blocked::BLOCKED_LABELS;
    /// `reason` of `reloads_total`.
    pub const RELOAD: [&str; 4] = ["headroom", "below_floor_value", "make_room", "recycled"];
    /// `q` of `score_quantile`.
    pub const QUANTILE: [&str; 3] = ["0.1", "0.5", "0.9"];
    /// `tier` of `restore_cost_seconds`.
    pub const RESTORE_TIER: [&str; 4] = crate::score::RESTORE_TIERS;
    /// `level`, 0 to 24: a level of the budget's per-level series
    /// (`stats::level_index`; deeper levels count in the last).
    /// A copy's tier in `copies_by_level`.
    pub const TIER: [&str; 2] = ["gpu", "decoded"];
    /// The states of `tiles_by_level`, `loads_by_level` and
    /// `drapes_by_layer`.
    pub const LEVEL_STATE: [&str; 6] = [
        "drawn",
        "selected",
        "under_need",
        "in_flight",
        "queued",
        "gpu",
    ];
    /// An imagery layer's index (`stats::layer_index`): never its name.
    pub const LAYER: [&str; super::LAYERS] = ["0", "1", "2", "3"];
    /// `layer_loads_total`'s kinds, in its order.
    pub const LAYER_LOAD: [&str; 5] = ["store", "network", "ram", "completion", "failure"];
    /// A load's kind (`stats::prefetch_index`).
    pub const LOAD_KIND: [&str; 2] = ["need", "prefetch"];
    /// The per-level counters, in `level_events_total`'s order.
    pub const LEVEL_EVENT: [&str; 4] = ["start", "completion", "wasted", "release"];
    pub const LEVEL: [&str; super::LEVELS] = [
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
        "17", "18", "19", "20", "21", "22", "23", "24",
    ];
    /// `gap`, −8 to 8.
    pub const GAP: [&str; 17] = [
        "-8", "-7", "-6", "-5", "-4", "-3", "-2", "-1", "0", "1", "2", "3", "4", "5", "6", "7", "8",
    ];
    /// `invariant`.
    pub const INVARIANT: [&str; 18] = [
        "I1", "I2", "I3", "I4", "I5", "I6", "I7", "I8", "I9", "I10", "I11", "I12", "I13", "I14",
        "I15", "I16", "I17", "I18",
    ];
}

/// One exposed series: a name, its labels, its value.
pub type Series = (&'static str, Vec<(&'static str, &'static str)>, f64);

impl BudgetSignals {
    /// The render frames' signals of `frames` — a meter the executor fed
    /// ([`BudgetObserver::frame`]) — over this one's: for a host whose
    /// budget runs in one place and whose frames in another (a web page's
    /// pump, a worker's budget), so that the signals it publishes say what
    /// both did. The steps' signals stay this one's.
    pub fn take_frames_from(&mut self, frames: &BudgetSignals) {
        self.uploads_total = frames.uploads_total;
        self.upload_bytes_total = frames.upload_bytes_total;
        self.frame_upload_bytes = frames.frame_upload_bytes.clone();
        self.frame_deadline_bytes = frames.frame_deadline_bytes;
        self.deadline_deferrals_total = frames.deadline_deferrals_total;
        self.upload_deferred_total = frames.upload_deferred_total;
        self.encoded_tiles_total = frames.encoded_tiles_total;
        self.encode_blocks_total = frames.encode_blocks_total;
        self.frame_encode_micros = frames.frame_encode_micros.clone();
        self.frame_encoded_tiles = frames.frame_encoded_tiles;
        self.frame_encode_blocks = frames.frame_encode_blocks;
        self.frame_encode_gpu_micros = frames.frame_encode_gpu_micros;
        self.frame_deferred = frames.frame_deferred;
        self.encode_share_blocks = frames.encode_share_blocks;
        self.encode_carry_blocks = frames.encode_carry_blocks;
        self.frame_upload_micros = frames.frame_upload_micros.clone();
        self.eye_lost_tiles = frames.eye_lost_tiles;
        self.eye_bare_items = frames.eye_bare_items;
        self.swaps_total[2] = frames.swaps_total[2];
    }

    /// Every series, as a sink exposes it: names without the prefix, labels
    /// from the closed sets of [`labels`], values in the spec's units
    /// (bytes, seconds, counts). Histograms expose their count and sum.
    pub fn series(&self) -> Vec<Series> {
        let mut out: Vec<Series> = Vec::new();
        let secs = |us: u64| us as f64 / 1e6;
        let per = |out: &mut Vec<Series>, name, p: &PerClass<u64>| {
            for (c, v) in p.iter() {
                out.push((name, vec![("class", labels::class(c))], v as f64));
            }
        };
        for (c, v) in self.bytes.iter() {
            for (i, s) in labels::STATE.iter().enumerate() {
                out.push((
                    "bytes",
                    vec![("class", labels::class(c)), ("state", s)],
                    v[i] as f64,
                ));
            }
        }
        for (c, levels) in self.bytes_by_level.iter() {
            let seen = self.bytes_levels[c];
            for (l, v) in levels.iter().enumerate() {
                if seen & (1 << l) == 0 {
                    continue;
                }
                for (i, s) in labels::STATE.iter().enumerate() {
                    out.push((
                        "bytes_by_level",
                        vec![
                            ("class", labels::class(c)),
                            ("state", s),
                            ("level", labels::LEVEL[l]),
                        ],
                        v[i] as f64,
                    ));
                }
            }
        }
        for (c, levels) in self.evictions_by_level_total.iter() {
            for (l, v) in levels.iter().enumerate().filter(|(_, v)| **v > 0) {
                out.push((
                    "evictions_by_level_total",
                    vec![("class", labels::class(c)), ("level", labels::LEVEL[l])],
                    *v as f64,
                ));
            }
        }
        let v = &self.levels;
        // Why the tiles under their need wait (`crate::blocked`).
        for c in Class::ALL {
            let mut oldest = [None::<u32>; crate::blocked::BLOCKED];
            for (l, seen) in self.blocked_seen[c].iter().enumerate() {
                for (k, reason) in labels::BLOCKED.iter().enumerate() {
                    if seen & (1 << k) == 0 {
                        continue;
                    }
                    out.push((
                        "blocked_tiles",
                        vec![
                            ("class", labels::class(c)),
                            ("level", labels::LEVEL[l]),
                            ("reason", reason),
                        ],
                        f64::from(v.blocked[c][l][k]),
                    ));
                    let ms = v.blocked_oldest_ms[c][l][k];
                    oldest[k] = Some(oldest[k].unwrap_or(0).max(ms));
                }
            }
            for (k, ms) in oldest.iter().enumerate() {
                if let Some(ms) = ms {
                    out.push((
                        "blocked_oldest_seconds",
                        vec![("class", labels::class(c)), ("reason", labels::BLOCKED[k])],
                        f64::from(*ms) / 1_000.0,
                    ));
                }
            }
        }
        for c in Class::ALL {
            let seen = self.levels_seen[c];
            for l in (0..LEVELS).filter(|l| seen & (1 << l) != 0) {
                let at = |more: Vec<(&'static str, &'static str)>| {
                    let mut k = vec![("class", labels::class(c)), ("level", labels::LEVEL[l])];
                    k.extend(more);
                    k
                };
                for (tier, n) in [
                    ("gpu", v.gpu_tiles[c][l]),
                    ("decoded", v.decoded_tiles[c][l]),
                ] {
                    out.push(("copies_by_level", at(vec![("tier", tier)]), f64::from(n)));
                }
                out.push((
                    "decoded_bytes_by_level",
                    at(vec![]),
                    v.decoded[c][l].0 as f64,
                ));
                for (state, n) in [
                    ("drawn", v.drawn[c][l]),
                    ("selected", v.selected[c][l]),
                    ("under_need", v.under_need[c][l]),
                ] {
                    out.push(("tiles_by_level", at(vec![("state", state)]), f64::from(n)));
                }
                for (state, n) in [("in_flight", v.in_flight[c][l]), ("queued", v.queued[c][l])] {
                    for (i, kind) in labels::LOAD_KIND.iter().enumerate() {
                        out.push((
                            "loads_by_level",
                            at(vec![("state", state), ("kind", kind)]),
                            f64::from(n[i]),
                        ));
                    }
                }
            }
            for (l, e) in self.level_events_total[c].iter().enumerate() {
                for (i, kind) in labels::LEVEL_EVENT
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| e[*i] > 0)
                {
                    out.push((
                        "level_events_total",
                        vec![
                            ("class", labels::class(c)),
                            ("kind", kind),
                            ("level", labels::LEVEL[l]),
                        ],
                        e[i] as f64,
                    ));
                }
            }
        }
        let y = &self.layers;
        for i in (0..LAYERS).filter(|i| self.layers_seen & (1 << i) != 0) {
            let layer = labels::LAYER[i];
            for (state, n) in [
                ("gpu", y.gpu_tiles[i]),
                ("drawn", y.drawn[i]),
                ("in_flight", y.in_flight[i]),
            ] {
                out.push((
                    "drapes_by_layer",
                    vec![("layer", layer), ("state", state)],
                    f64::from(n),
                ));
            }
            out.push((
                "drape_bytes_by_layer",
                vec![("layer", layer)],
                y.gpu_bytes[i].0 as f64,
            ));
            for (k, kind) in labels::LAYER_LOAD.iter().enumerate() {
                out.push((
                    "layer_loads_total",
                    vec![("layer", layer), ("kind", kind)],
                    self.layer_loads_total[i][k] as f64,
                ));
            }
        }
        out.push(("decoded_bytes", vec![], self.decoded_bytes as f64));
        per(&mut out, "protected_bytes", &self.protected_bytes);
        per(&mut out, "target_bytes", &self.target_bytes);
        per(&mut out, "overflow_bytes", &self.overflow_bytes);
        per(&mut out, "hard_limit_bytes", &self.hard_limit_bytes);
        for (c, v) in self.over_target_micros_total.iter() {
            out.push((
                "over_target_seconds_total",
                vec![("class", labels::class(c))],
                secs(v),
            ));
        }
        let plain: [(&'static str, f64); 9] = [
            ("lost_tiles", f64::from(self.lost_tiles)),
            ("lost_tile_steps_total", self.lost_tile_steps_total as f64),
            ("bare_items", f64::from(self.bare_items)),
            ("bare_item_steps_total", self.bare_item_steps_total as f64),
            ("floor_in", f64::from(u8::from(self.floor_in))),
            ("fallback_depth_max", f64::from(self.fallback_depth_max)),
            ("underdraped_tiles", f64::from(self.underdraped_tiles)),
            ("converged", f64::from(u8::from(self.converged))),
            ("unconverged_seconds", secs(self.unconverged_micros)),
        ];
        for (n, v) in plain {
            out.push((n, vec![], v));
        }
        if let Some(f) = self.floor_micros {
            out.push(("floor_seconds", vec![], secs(f)));
        }
        for (i, r) in labels::ROLE.iter().enumerate() {
            out.push((
                "drawn_tiles",
                vec![("role", r)],
                f64::from(self.drawn_tiles[i]),
            ));
        }
        for (i, e) in labels::EYE.iter().enumerate() {
            out.push((
                "eye_lost_tiles",
                vec![("eye", e)],
                f64::from(self.eye_lost_tiles[i]),
            ));
            out.push((
                "eye_bare_items",
                vec![("eye", e)],
                f64::from(self.eye_bare_items[i]),
            ));
        }
        for (i, g) in labels::GAP.iter().enumerate() {
            out.push((
                "imagery_over_terrain",
                vec![("gap", g)],
                f64::from(self.imagery_over_terrain[i]),
            ));
        }
        out.push((
            "converged_seconds_count",
            vec![],
            self.converged_micros.count as f64,
        ));
        out.push((
            "converged_seconds_sum",
            vec![],
            secs(self.converged_micros.sum),
        ));
        out.push((
            "converge_bound_seconds",
            vec![],
            secs(self.converge_bound_micros),
        ));
        for (i, k) in labels::DEFICIT.iter().enumerate() {
            let kind = || vec![("kind", *k)];
            out.push((
                "under_need_tiles",
                kind(),
                f64::from(self.under_need_tiles[i]),
            ));
            out.push((
                "over_need_tiles",
                kind(),
                f64::from(self.over_need_tiles[i]),
            ));
            out.push(("deficit_levels", kind(), f64::from(self.deficit_levels[i])));
            out.push((
                "deficit_level_seconds_total",
                kind(),
                secs(self.deficit_level_micros_total[i]),
            ));
        }
        out.push((
            "under_need_still_seconds",
            vec![],
            secs(self.under_need_still_micros),
        ));
        let source = labels::SPEC_SOURCE[usize::from(self.spec_overridden)];
        for k in crate::spec::SpecKind::ALL {
            out.push((
                "spec_info",
                vec![("spec", k.label()), ("source", source)],
                f64::from(u8::from(k == self.spec)),
            ));
        }
        out.push(("spec_changes_total", vec![], self.spec_changes_total as f64));
        for (i, k) in labels::FLAP.iter().enumerate() {
            out.push(("flaps_total", vec![("kind", k)], self.flaps_total[i] as f64));
        }
        out.push((
            "sse_scale",
            vec![],
            f64::from(self.sse_scale_milli) / 1000.0,
        ));
        out.push(("pressure_level", vec![], f64::from(self.pressure_level)));
        out.push(("thermal_level", vec![], f64::from(self.thermal_level)));
        for (c, v) in self.starts_total.iter() {
            for (i, s) in labels::SOURCE.iter().enumerate() {
                out.push((
                    "starts_total",
                    vec![("class", labels::class(c)), ("source", s)],
                    v[i] as f64,
                ));
            }
        }
        per(&mut out, "wasted_starts_total", &self.wasted_starts_total);
        out.push(("local_waits_total", vec![], self.local_waits_total as f64));
        for (i, r) in labels::CANCEL.iter().enumerate() {
            out.push((
                "cancels_total",
                vec![("reason", r)],
                self.cancels_total[i] as f64,
            ));
        }
        out.push(("completions_total", vec![], self.completions_total as f64));
        for (c, v) in self.failures_total.iter() {
            for (i, k) in labels::FAILURE.iter().enumerate() {
                out.push((
                    "failures_total",
                    vec![("class", labels::class(c)), ("kind", k)],
                    v[i] as f64,
                ));
            }
        }
        out.push((
            "stale_results_total",
            vec![],
            self.stale_results_total as f64,
        ));
        out.push((
            "load_seconds_count",
            vec![],
            self.load_millis.iter().sum::<u64>() as f64,
        ));
        for (c, v) in self.in_flight.iter() {
            out.push((
                "in_flight",
                vec![("class", labels::class(c)), ("source", "network")],
                f64::from(v),
            ));
        }
        out.push((
            "in_flight",
            vec![("source", "local")],
            f64::from(self.local_reads),
        ));
        let counts: [(&'static str, f64); 12] = [
            ("request_heap_entries", f64::from(self.request_heap_entries)),
            ("tracked_resources", f64::from(self.tracked_resources)),
            ("duplicates_in_flight", f64::from(self.duplicates_in_flight)),
            ("decide_seconds_count", self.decide_micros.count as f64),
            ("decide_seconds_sum", secs(self.decide_micros.sum)),
            ("decide_peak_seconds", secs(self.decide_peak_micros)),
            ("decide_share_seconds", secs(self.decide_share_micros)),
            ("decide_work", f64::from(self.decide_work)),
            ("decide_work_total", self.decide_work_total as f64),
            ("thrash_total", self.thrash_total as f64),
            ("rereads_total", self.rereads_total as f64),
            ("store_misses_total", self.store_misses_total as f64),
        ];
        for (n, v) in counts {
            out.push((n, vec![], v));
        }
        for (c, v) in self.evictions_total.iter() {
            for (i, r) in labels::EVICT.iter().enumerate() {
                out.push((
                    "evictions_total",
                    vec![("class", labels::class(c)), ("reason", r)],
                    v[i] as f64,
                ));
            }
        }
        per(&mut out, "releases_total", &self.releases_total);
        let more: [(&'static str, f64); 8] = [
            (
                "network_bytes_avoided_total",
                self.network_bytes_avoided_total as f64,
            ),
            ("upload_plan_bytes", self.upload_plan_bytes as f64),
            (
                "frame_upload_bytes_count",
                self.frame_upload_bytes.count as f64,
            ),
            ("frame_upload_bytes_sum", self.frame_upload_bytes.sum as f64),
            ("frame_deadline_bytes", self.frame_deadline_bytes as f64),
            (
                "deadline_deferrals_total",
                self.deadline_deferrals_total as f64,
            ),
            (
                "frame_upload_seconds_sum",
                secs(self.frame_upload_micros.sum),
            ),
            ("swap_gaps_total", self.swap_gaps_total as f64),
        ];
        for (n, v) in more {
            out.push((n, vec![], v));
        }
        for (i, l) in labels::LANE.iter().enumerate() {
            out.push((
                "upload_deferred_total",
                vec![("lane", l)],
                self.upload_deferred_total[i] as f64,
            ));
        }
        let encodes: [(&'static str, f64); 4] = [
            ("encoded_tiles_total", self.encoded_tiles_total as f64),
            ("encode_blocks_total", self.encode_blocks_total as f64),
            (
                "frame_encode_seconds_count",
                self.frame_encode_micros.count as f64,
            ),
            (
                "frame_encode_seconds_sum",
                secs(self.frame_encode_micros.sum),
            ),
        ];
        for (n, v) in encodes {
            out.push((n, vec![], v));
        }
        per(&mut out, "uploads_total", &self.uploads_total);
        per(&mut out, "upload_bytes_total", &self.upload_bytes_total);
        per(
            &mut out,
            "estimate_drift_exceeded_total",
            &self.estimate_drift_exceeded_total,
        );
        per(
            &mut out,
            "encode_fallbacks_total",
            &self.encode_fallbacks_total,
        );
        per(&mut out, "reserved_bytes", &self.reserved_bytes);
        per(&mut out, "transient_bytes", &self.transient_bytes);
        per(&mut out, "transfer_bytes", &self.transfer_bytes);
        for (i, l) in labels::LANE.iter().enumerate() {
            out.push((
                "frame_deferred",
                vec![("lane", l)],
                f64::from(self.frame_deferred[i]),
            ));
        }
        let lane: [(&'static str, f64); 5] = [
            ("frame_encoded_tiles", f64::from(self.frame_encoded_tiles)),
            ("frame_encode_blocks", self.frame_encode_blocks as f64),
            (
                "frame_encode_gpu_seconds",
                secs(self.frame_encode_gpu_micros),
            ),
            (
                "encode_share_blocks",
                if self.encode_share_blocks == u64::MAX {
                    -1.0
                } else {
                    self.encode_share_blocks as f64
                },
            ),
            ("encode_carry_blocks", self.encode_carry_blocks as f64),
        ];
        for (n, v) in lane {
            out.push((n, vec![], v));
        }
        out.push((
            "decoded_reserved_bytes",
            vec![],
            self.decoded_reserved_bytes as f64,
        ));
        for (i, k) in labels::SWAP.iter().enumerate() {
            out.push(("swaps_total", vec![("kind", k)], self.swaps_total[i] as f64));
        }
        out.push((
            "batch_steps_per_frame_count",
            vec![],
            self.batch_steps_per_frame.count as f64,
        ));
        out.push(("batch_fatal_total", vec![], self.batch_fatal_total as f64));
        for (i, n) in labels::INVARIANT.iter().enumerate() {
            out.push((
                "invariant_violations_total",
                vec![("invariant", n)],
                self.invariant_violations_total[i] as f64,
            ));
        }
        out.push((
            "rejected_inputs_total",
            vec![],
            self.rejected_inputs_total as f64,
        ));
        self.score_series(&mut out);
        out
    }

    /// The progressive score's series
    /// (`docs/explorations/budget-progressive-coarse-preload.md` §7): labels
    /// `level`, `distance`, `side`, `term` and `name` only, never a tile.
    fn score_series(&self, out: &mut Vec<Series>) {
        use crate::score::{Side, DISTANCE_BAND_LABELS, TERM_LABELS};
        for (name, v) in self.score_params.pairs() {
            out.push(("param", vec![("name", name)], v as f64));
        }
        if let Some(n) = self.score_params.need_level {
            out.push(("need_level", vec![], f64::from(n)));
        }
        // The waits, by kind (`crate::waits`).
        for (k, kind) in crate::waits::WAIT_KINDS.iter().enumerate() {
            for c in Class::ALL {
                for (l, per) in self.waits.count[c].iter().enumerate() {
                    let n = per[k];
                    if n > 0 {
                        out.push((
                            "waits_total",
                            vec![
                                ("kind", kind),
                                ("class", labels::class(c)),
                                ("level", labels::LEVEL[l]),
                            ],
                            n as f64,
                        ));
                    }
                }
            }
            for (q, name) in [(50, "wait_p50_ms"), (95, "wait_p95_ms")] {
                if let Some(ms) = crate::waits::percentile_ms(&self.waits.millis[k], q) {
                    out.push((name, vec![("kind", kind)], ms as f64));
                }
            }
        }
        for (i, reason) in labels::RELOAD.iter().enumerate() {
            out.push((
                "reloads_total",
                vec![("reason", reason)],
                self.waits.reload_reasons[i] as f64,
            ));
        }
        if let Some(l) = self.score_lambda {
            out.push(("exchange_threshold", vec![], l as f64));
        }
        // The score's quantiles per level and side, from the histograms:
        // `score_quantile{side,q,level}`.
        let qs = |out: &mut Vec<Series>, side: Side, l: usize, c: &crate::score::Cell| {
            if let Some(q) = crate::score::quantiles(c) {
                for (name, v) in labels::QUANTILE.iter().zip(q) {
                    out.push((
                        "score_quantile",
                        vec![
                            ("side", side.label()),
                            ("q", name),
                            ("level", labels::LEVEL[l]),
                        ],
                        v as f64,
                    ));
                }
            }
        };
        for (tier, us) in crate::score::RESTORE_TIERS
            .iter()
            .zip(self.score_params.restore_us)
        {
            out.push((
                "restore_cost_seconds",
                vec![("tier", tier)],
                us as f64 / 1e6,
            ));
        }
        for l in 0..LEVELS {
            for (b, band) in DISTANCE_BAND_LABELS.iter().enumerate() {
                let at = vec![("level", labels::LEVEL[l]), ("distance", *band)];
                let p = self.preload_by_level_distance[l][b];
                let e = self.evictions_by_level_distance[l][b];
                let total = |side| self.score_total.cell(l, b, side).count;
                // A level and band is exposed from its first decision on:
                // a gauge left behind keeps its last value.
                if total(Side::Preload) > 0 || p > 0 {
                    out.push(("preload_by_level_distance", at.clone(), f64::from(p)));
                }
                if total(Side::Evict) > 0 || e > 0 {
                    out.push(("evictions_by_level_distance", at.clone(), f64::from(e)));
                }
                for side in [Side::Preload, Side::Evict] {
                    let c = self.score_total.cell(l, b, side);
                    if c.count == 0 {
                        continue;
                    }
                    let mut k = at.clone();
                    k.push(("side", side.label()));
                    out.push(("score_decisions_total", k.clone(), f64::from(c.count)));
                    let r = self.score_recent.cell(l, b, side);
                    for (t, term) in TERM_LABELS.iter().enumerate() {
                        let mut kt = k.clone();
                        kt.push(("term", term));
                        out.push(("score_sum_total", kt.clone(), c.term(t).sum as f64));
                        if r.count > 0 {
                            out.push(("score_min", kt.clone(), r.term(t).min as f64));
                            out.push(("score_max", kt, r.term(t).max as f64));
                        }
                    }
                }
                if let Some(h) = &self.score_held {
                    let c = h.board.cell(l, b, Side::Held);
                    let mut k = at.clone();
                    k.push(("side", Side::Held.label()));
                    out.push(("score_held_tiles", k.clone(), f64::from(c.count)));
                    if c.count == 0 {
                        continue;
                    }
                    for (t, term) in TERM_LABELS.iter().enumerate() {
                        let mut kt = k.clone();
                        kt.push(("term", term));
                        out.push(("score_held_sum", kt.clone(), c.term(t).sum as f64));
                        out.push(("score_held_min", kt.clone(), c.term(t).min as f64));
                        out.push(("score_held_max", kt, c.term(t).max as f64));
                    }
                }
            }
        }
        for (i, reason) in crate::score::RELEASE_REASONS.iter().enumerate() {
            out.push((
                "released_total",
                vec![("reason", reason)],
                self.released_total[i] as f64,
            ));
            for l in 0..LEVELS {
                for (b, band) in DISTANCE_BAND_LABELS.iter().enumerate() {
                    let v = self.released_by_level_distance[i][l][b];
                    if v > 0 || self.score_total.cell(l, b, Side::Evict).count > 0 {
                        out.push((
                            "released_by_level_distance",
                            vec![
                                ("level", labels::LEVEL[l]),
                                ("distance", *band),
                                ("reason", reason),
                            ],
                            f64::from(v),
                        ));
                    }
                }
            }
        }
        for l in 0..LEVELS {
            let mut held = crate::score::Cell::default();
            let (mut pre, mut ev) = (crate::score::Cell::default(), crate::score::Cell::default());
            for b in 0..DISTANCE_BANDS {
                if let Some(h) = &self.score_held {
                    held.add(h.board.cell(l, b, Side::Held));
                }
                pre.add(self.score_recent.cell(l, b, Side::Preload));
                ev.add(self.score_recent.cell(l, b, Side::Evict));
            }
            qs(out, Side::Held, l, &held);
            qs(out, Side::Preload, l, &pre);
            qs(out, Side::Evict, l, &ev);
        }
        out.push((
            "score_events_dropped_total",
            vec![],
            self.score_events_dropped_total as f64,
        ));
    }
}

/// The flap window (I4).
const FLAP_WINDOW: Micros = Micros::from_secs(10);

/// Per tile, while the views are still: the mesh level and the drape level
/// it was drawn at, and when each last changed (I4).
#[derive(Clone, Debug)]
struct Shown {
    mesh: u32,
    drape: u32,
    mesh_changed: Option<Micros>,
    drape_changed: Option<Micros>,
}

/// The observer that accumulates the signals: bounded per-tile maps for
/// flaps, swaps and gaps, counters for the rest.
#[derive(Clone, Debug, Default)]
pub struct BudgetMeter {
    s: BudgetSignals,
    cfg_pinned: Option<u32>,
    cfg_policy: Option<Policy>,
    start: Option<Micros>,
    last_now: Option<Micros>,
    still_since: Option<Micros>,
    converged_counted: bool,
    shown: BTreeMap<TileId, Shown>,
    /// Each drawn mesh's tile last step and its role, by tile.
    drawn_last: Vec<(TileId, Role)>,
    /// The tiles drawn items served last step, sorted.
    served_last: Vec<TileId>,
    /// The layers each drawn mesh wore last step, by tile then layer.
    worn_layers_last: Vec<(TileId, LayerId)>,
    /// The tree as the last need knew it, by child: a held child is no
    /// longer listed once its parent is wanted.
    parents_last: Vec<(TileId, TileId)>,
    /// This step's buffers, swapped with the `_last` ones: a step allocates
    /// nothing once they have grown to the view (the zero-cost contract).
    scratch: MeterScratch,
    batch_steps: u64,
    violations: Violations,
    /// The present need's waits, followed (`crate::waits`).
    waits: crate::waits::WaitTracker,
}

/// The meter's per-step buffers.
#[derive(Clone, Debug, Default)]
struct MeterScratch {
    /// The need's entries: (tile, order listed, level, parent).
    listed: Vec<(TileId, u32, u32, Option<TileId>)>,
    /// Each listed tile's level, the last listing's, by tile.
    levels: Vec<(TileId, u32)>,
    /// The tiles wanted drawn, sorted.
    wanted: Vec<TileId>,
    drawn: Vec<(TileId, Role)>,
    served: Vec<TileId>,
    worn_layers: Vec<(TileId, LayerId)>,
    parents: Vec<(TileId, TileId)>,
    /// Parents refined or coarsened this step, counted once each.
    swapped: Vec<TileId>,
}

/// The value at `k` of a vector of pairs sorted by key.
fn lookup<'v, K: Ord, V>(v: &'v [(K, V)], k: &K) -> Option<&'v V> {
    v.binary_search_by(|e| e.0.cmp(k)).ok().map(|i| &v[i].1)
}

/// The run of a vector of pairs sorted by key whose key is `k`.
fn run_of<'v, K: Ord, V>(v: &'v [(K, V)], k: &K) -> &'v [(K, V)] {
    let from = v.partition_point(|e| e.0 < *k);
    let to = from + v[from..].partition_point(|e| e.0 == *k);
    &v[from..to]
}

impl BudgetMeter {
    /// A meter for a budget on `cfg`.
    pub fn new(cfg: &BudgetConfig) -> Self {
        let mut m = BudgetMeter {
            cfg_pinned: cfg.pinned_level,
            cfg_policy: Some(cfg.policy),
            ..BudgetMeter::default()
        };
        m.s.spec = crate::spec::Specification::for_policy(cfg.policy).kind;
        m.s.floor_level = cfg.pinned_level;
        m.s.reserved_bytes = PerClass::from_fn(|c| cfg.reserved[c].0);
        m.s.decoded_reserved_bytes = cfg.decoded_reserved.0;
        m
    }

    /// The profile's convergence bound and `decide`'s share of a frame:
    /// host-set gauges.
    pub fn set_bounds(&mut self, converge: Micros, decide_share: Micros) {
        self.s.converge_bound_micros = converge.0;
        self.s.decide_share_micros = decide_share.0;
    }

    /// The signals now.
    pub fn signals(&self) -> BudgetSignals {
        let mut s = self.s.clone();
        s.invariant_violations_total = self.violations.counts;
        s
    }

    /// The signals now; the peaks start again.
    pub fn publish(&mut self) -> BudgetSignals {
        let s = self.signals();
        self.s.decide_peak_micros = 0;
        self.s.fallback_depth_max = 0;
        self.s.score_recent = crate::score::ScoreBoard::default();
        self.s.score_lambda = None;
        self.s.waits_recent = [0; 3];
        self.s.waits_recent_micros = 0;
        self.s.score_events.clear();
        s
    }

    /// The GPU-held set by score ([`crate::Budget::held_report`]), as the
    /// host took it: published with the next signals.
    pub fn set_held(&mut self, report: crate::score::HeldReport) {
        self.s.score_held = Some(report);
    }

    /// The violations `check` counted.
    pub fn violations(&self) -> &Violations {
        &self.violations
    }

    /// The need's levels and parents, by tile: a tile listed twice keeps
    /// its last listing's, as a map collected from the entries would.
    fn read_need(x: &mut MeterScratch, need: &NeedSet) {
        x.listed.clear();
        x.listed.extend(
            need.entries()
                .enumerate()
                .map(|(i, (_, t, n))| (t, u32::try_from(i).unwrap_or(u32::MAX), n.level, n.parent)),
        );
        // By tile, the last listing first.
        x.listed
            .sort_unstable_by_key(|e| (e.0, std::cmp::Reverse(e.1)));
        x.levels.clear();
        x.parents.clear();
        for run in x.listed.chunk_by(|a, b| a.0 == b.0) {
            x.levels.push((run[0].0, run[0].2));
            // The last listing that names a parent.
            if let Some(p) = run.iter().find_map(|e| e.3) {
                x.parents.push((run[0].0, p));
            }
        }
    }
}

impl BudgetObserver for BudgetMeter {
    fn step(&mut self, r: &StepRecord<'_>) {
        let d = r.decisions;
        let st = &d.stats;
        let s = &mut self.s;
        // The progressive score: one source, the step's records.
        s.score_params = st.score.params;
        s.preload_by_level_distance = st.score.preload_in_flight;
        s.evictions_by_level_distance = st.score.evictions;
        s.released_by_level_distance = st.score.released;
        for (i, r) in st.score.released.iter().enumerate() {
            s.released_total[i] += r.iter().flatten().map(|v| u64::from(*v)).sum::<u64>();
        }
        crate::score::board_of(&d.scores, &mut s.score_total);
        crate::score::board_of(&d.scores, &mut s.score_recent);
        for e in d.scores.iter().filter(|e| {
            e.side == crate::score::Side::Evict
                && matches!(
                    e.reason,
                    crate::score::Reason::Sweep
                        | crate::score::Reason::Pressure
                        | crate::score::Reason::Fit
                        | crate::score::Reason::Admission
                        | crate::score::Reason::Swap
                        | crate::score::Reason::Envelope
                )
        }) {
            s.score_lambda = Some(
                s.score_lambda
                    .map_or(e.terms.score, |l| l.max(e.terms.score)),
            );
        }
        for e in &d.scores {
            if s.score_events.len() < SCORE_EVENTS_KEPT {
                s.score_events.push(*e);
            } else {
                s.score_events_dropped_total += 1;
            }
        }
        let start = *self.start.get_or_insert(r.now);
        let dt = self.last_now.map_or(0, |l| r.now.since(l).0);
        self.last_now = Some(r.now);
        // Memory.
        for c in Class::ALL {
            let transit = st.transit[c].0;
            s.bytes[c] = [st.gpu[c].0.saturating_sub(transit), transit];
            s.protected_bytes[c] = st.protected[c].0;
            s.target_bytes[c] = st.target[c].0;
            s.overflow_bytes[c] = st.overflow[c].0;
            s.hard_limit_bytes[c] = st.hard_limit[c].0;
            if st.gpu[c] > st.target[c] {
                s.over_target_micros_total[c] += dt;
            }
            for i in 0..2 {
                s.starts_total[c][i] += u64::from(st.starts[c][i]);
                s.failures_total[c][i] += u64::from(st.failures[c][i]);
            }
            s.wasted_starts_total[c] += u64::from(st.wasted_starts[c]);
            for i in 0..3 {
                s.evictions_total[c][i] += u64::from(st.evictions[c][i]);
            }
            s.releases_total[c] += u64::from(st.releases[c]);
            for l in 0..LEVELS {
                let (drawable, transit) =
                    (st.drawable_by_level[c][l].0, st.transit_by_level[c][l].0);
                s.bytes_by_level[c][l] = [drawable, transit];
                if drawable != 0 || transit != 0 {
                    s.bytes_levels[c] |= 1 << l;
                }
                s.evictions_by_level_total[c][l] += u64::from(st.evictions_by_level[c][l]);
                let v = &st.levels;
                let events = [
                    v.starts[c][l],
                    v.completions[c][l],
                    v.wasted[c][l],
                    v.releases[c][l],
                ];
                for (t, n) in s.level_events_total[c][l].iter_mut().zip(events) {
                    *t += u64::from(n);
                }
                let named = v.gpu_tiles[c][l] != 0
                    || v.decoded_tiles[c][l] != 0
                    || v.drawn[c][l] != 0
                    || v.selected[c][l] != 0
                    || v.in_flight[c][l] != [0, 0]
                    || v.queued[c][l] != [0, 0];
                if named {
                    s.levels_seen[c] |= 1 << l;
                }
            }
            s.estimate_drift_exceeded_total[c] += u64::from(st.drift_exceeded[c]);
            s.encode_fallbacks_total[c] += u64::from(st.fallbacks[c]);
            s.transient_bytes[c] = st.transient[c].0;
            s.transfer_bytes[c] = st.transfer[c].0;
        }
        s.levels = st.levels;
        for c in Class::ALL {
            for (l, seen) in s.blocked_seen[c].iter_mut().enumerate() {
                for (k, n) in st.levels.blocked[c][l].iter().enumerate() {
                    if *n > 0 {
                        *seen |= 1 << k;
                    }
                }
            }
        }
        let y = &st.layers;
        s.layers = *y;
        for i in 0..LAYERS {
            let events = [
                y.requests[i][0],
                y.requests[i][1],
                y.ram_hits[i],
                y.completions[i],
                y.failures[i],
            ];
            for (t, n) in s.layer_loads_total[i].iter_mut().zip(events) {
                *t += u64::from(n);
            }
            if y.gpu_tiles[i] != 0 || y.drawn[i] != 0 || y.in_flight[i] != 0 || events != [0; 5] {
                s.layers_seen |= 1 << i;
            }
        }
        s.in_flight = st.in_flight;
        s.local_reads = st.local_reads;
        s.decoded_bytes = st.decoded.0;
        // Coverage.
        s.floor_in = st.floor_in;
        if st.floor_in && s.floor_micros.is_none() {
            s.floor_micros = Some(r.now.since(start).0);
        }
        s.lost_tiles = if st.floor_in { st.lost } else { 0 };
        s.lost_tile_steps_total += u64::from(s.lost_tiles);
        s.bare_items = st.bare;
        s.bare_item_steps_total += u64::from(st.bare);
        s.drawn_tiles = [st.exact, st.held, st.fallback];
        s.fallback_depth_max = s.fallback_depth_max.max(st.fallback_depth_max);
        for e in r.events {
            // The reservation in force, as the budget now takes it off the
            // limits: a switch of the consumer's encoder is seen here.
            if let BudgetEvent::Reserved(class, bytes) = e {
                s.reserved_bytes[*class] = bytes.0;
            }
            if let BudgetEvent::Specify(kind, _, overridden) = e {
                s.spec = *kind;
                s.spec_overridden = *overridden;
                s.spec_changes_total += 1;
            }
        }
        s.underdraped_tiles = st.underdraped;
        s.imagery_over_terrain = st.imagery_over_terrain;
        let q = &st.deficit;
        s.under_need_tiles = [q.mesh_under_tiles, q.imagery_under()];
        s.over_need_tiles = [q.mesh_over_tiles, q.imagery_over_tiles];
        s.deficit_levels = [
            q.mesh_under_levels,
            q.imagery_under_levels
                .saturating_add(q.imagery_short_levels),
        ];
        for (t, l) in s
            .deficit_level_micros_total
            .iter_mut()
            .zip(s.deficit_levels)
        {
            *t = t.saturating_add(u64::from(l).saturating_mul(dt));
        }
        s.under_need_still_micros = if r.views_changed || q.under_tiles() == 0 {
            0
        } else {
            s.under_need_still_micros.saturating_add(dt)
        };
        // Convergence, from the views going still.
        s.converged = st.converged;
        if r.views_changed {
            // Moving: nothing to time until the views hold still.
            self.still_since = None;
            self.converged_counted = false;
            s.unconverged_micros = 0;
        } else {
            let still_since = *self.still_since.get_or_insert(r.now);
            if st.converged {
                if !self.converged_counted {
                    s.converged_micros.record(r.now.since(still_since).0);
                    self.converged_counted = true;
                }
                s.unconverged_micros = 0;
            } else {
                s.unconverged_micros = r.now.since(still_since).0;
            }
        }
        s.sse_scale_milli = d.sse_scale_milli;
        s.pressure_level = r.pressure as u8;
        s.thermal_level = r.thermal as u8;
        // Work.
        s.local_waits_total += u64::from(st.local_waits);
        for (i, n) in st.cancels.iter().enumerate() {
            s.cancels_total[i] += u64::from(*n);
        }
        let _ = (CancelReason::ALL, EvictReason::ALL);
        s.completions_total += u64::from(st.completed);
        s.stale_results_total += u64::from(st.stale);
        for (i, n) in st.latency_ms.iter().enumerate() {
            s.load_millis[i] += u64::from(*n);
        }
        s.request_heap_entries = st.heap_len;
        s.tracked_resources = st.tracked;
        s.duplicates_in_flight = st.duplicates_in_flight;
        if let Some(w) = r.decide_wall {
            s.decide_micros.record(w.0);
            s.decide_peak_micros = s.decide_peak_micros.max(w.0);
        }
        s.decide_work = st.work;
        s.decide_work_total += u64::from(st.work);
        s.thrash_total += u64::from(st.thrash);
        s.rereads_total += u64::from(st.rereads);
        s.network_bytes_avoided_total += st.network_bytes_avoided.0;
        s.store_misses_total += u64::from(st.store_misses);
        s.upload_plan_bytes = d.upload_plan.bytes.0;
        s.rejected_inputs_total += u64::from(st.rejected_inputs);
        for e in r.events {
            if let BudgetEvent::ReleaseBeyond(_, n) = e {
                // Batch drops between frames: their class is not recorded,
                // so they count under the terrain mesh's release_beyond.
                s.evictions_total[Class::TerrainMesh][3] += u64::from(*n);
            }
        }
        // Batch.
        if self.cfg_policy == Some(Policy::Batch) {
            self.batch_steps += 1;
            if d.frame_ready {
                s.batch_steps_per_frame.record(self.batch_steps);
                self.batch_steps = 0;
            }
            if d.fatal.is_some() {
                s.batch_fatal_total += 1;
            }
        }
        // Flaps (I4): a tile's second level change within 10 s, still.
        let mut x = std::mem::take(&mut self.scratch);
        Self::read_need(&mut x, r.need);
        if r.views_changed {
            self.shown.clear();
        } else {
            for i in &d.draw.items {
                let Some(&mesh) = lookup(&x.levels, &i.mesh.tile()) else {
                    continue;
                };
                let drape = i.drapes.first().map_or(0, |x| x.level);
                for t in &i.serves {
                    let e = self.shown.entry(*t).or_insert(Shown {
                        mesh,
                        drape,
                        mesh_changed: None,
                        drape_changed: None,
                    });
                    if e.mesh != mesh {
                        if e.mesh_changed.is_some_and(|c| r.now.since(c) < FLAP_WINDOW) {
                            s.flaps_total[0] += 1;
                        }
                        e.mesh = mesh;
                        e.mesh_changed = Some(r.now);
                    }
                    if e.drape != drape {
                        if e.drape_changed
                            .is_some_and(|c| r.now.since(c) < FLAP_WINDOW)
                        {
                            s.flaps_total[1] += 1;
                        }
                        e.drape = drape;
                        e.drape_changed = Some(r.now);
                    }
                }
            }
            x.wanted.clear();
            x.wanted.extend(r.need.drawn.iter().map(|(t, _)| *t));
            x.wanted.sort_unstable();
            let wanted = &x.wanted;
            self.shown.retain(|t, _| wanted.binary_search(t).is_ok());
        }
        // Swaps and gaps (I9). Items are sorted by mesh, so by tile.
        x.drawn.clear();
        x.drawn
            .extend(d.draw.items.iter().map(|i| (i.mesh.tile(), i.role)));
        x.served.clear();
        x.served
            .extend(d.draw.items.iter().flat_map(|i| i.serves.iter().copied()));
        x.served.sort_unstable();
        x.served.dedup();
        // The waits: the draw ends them, the need opens them.
        s.waits_recent_micros += dt;
        self.waits.step(
            r.now,
            r.need,
            d,
            &x.drawn,
            &mut s.waits,
            &mut s.waits_recent,
        );
        let drawn = &x.drawn;
        let parents = &x.parents;
        x.swapped.clear();
        for (t, role) in drawn {
            if *role == Role::Exact {
                // A parent drawn last step, gone now that its child is
                // drawn as itself: a refinement completed.
                if let Some(p) = lookup(parents, t) {
                    if lookup(&self.drawn_last, p).is_some() && lookup(drawn, p).is_none() {
                        x.swapped.push(*p);
                    }
                }
            }
        }
        x.swapped.sort_unstable();
        x.swapped.dedup();
        s.swaps_total[0] += x.swapped.len() as u64;
        x.swapped.clear();
        for (t, role) in &self.drawn_last {
            // A finer tile drawn last step (as itself, or held), gone now
            // that its parent is drawn as itself: a coarsening completed.
            if *role != Role::Fallback && lookup(drawn, t).is_none() {
                if let Some(p) = lookup(parents, t).or_else(|| lookup(&self.parents_last, t)) {
                    let was = lookup(&self.drawn_last, p);
                    if lookup(drawn, p) == Some(&Role::Exact) && was != Some(&Role::Exact) {
                        x.swapped.push(*p);
                    }
                }
            }
        }
        x.swapped.sort_unstable();
        x.swapped.dedup();
        s.swaps_total[1] += x.swapped.len() as u64;
        // The rows of a tile, sorted and distinct, are the set of layers it
        // wears.
        x.worn_layers.clear();
        x.worn_layers.extend(
            d.draw
                .items
                .iter()
                .flat_map(|i| i.drapes.iter().map(move |y| (i.mesh.tile(), y.layer))),
        );
        x.worn_layers.sort_unstable();
        x.worn_layers.dedup();
        for i in &d.draw.items {
            let t = i.mesh.tile();
            let layers = run_of(&x.worn_layers, &t);
            let was = run_of(&self.worn_layers_last, &t);
            if !was.is_empty() && !layers.is_empty() && was != layers {
                s.swaps_total[3] += 1;
            }
        }
        if st.floor_in {
            let gaps = r
                .need
                .drawn
                .iter()
                .filter(|(t, _)| {
                    self.served_last.binary_search(t).is_ok() && x.served.binary_search(t).is_err()
                })
                .count();
            s.swap_gaps_total += gaps as u64;
        }
        std::mem::swap(&mut self.drawn_last, &mut x.drawn);
        std::mem::swap(&mut self.served_last, &mut x.served);
        std::mem::swap(&mut self.worn_layers_last, &mut x.worn_layers);
        std::mem::swap(&mut self.parents_last, &mut x.parents);
        self.scratch = x;
        // The record-local checks.
        let cfg = BudgetConfig {
            pinned_level: self.cfg_pinned,
            policy: self.cfg_policy.unwrap_or(Policy::Interactive),
            ..BudgetConfig::interactive(Bytes::ZERO)
        };
        let mut cfg = cfg;
        cfg.request_heap = u32::MAX;
        check(r, &cfg, &mut self.violations);
    }

    fn frame(&mut self, f: &FrameRecord<'_>) {
        let s = &mut self.s;
        let mut bytes = 0u64;
        let mut meshes = BTreeSet::new();
        let mut drapes = BTreeSet::new();
        for (r, b) in f.uploaded {
            let c = match r {
                ResourceId::Mesh(_) => Class::TerrainMesh,
                ResourceId::Drape { .. } => Class::Drape,
            };
            s.uploads_total[c] += 1;
            s.upload_bytes_total[c] += b.0;
            bytes += b.0;
            match r {
                ResourceId::Mesh(t) => {
                    meshes.insert(*t);
                }
                ResourceId::Drape { tile, .. } => {
                    drapes.insert(*tile);
                }
            }
        }
        s.swaps_total[2] += meshes.intersection(&drapes).count() as u64;
        s.frame_upload_bytes.record(bytes);
        s.frame_deadline_bytes = f.deadline.bytes.0;
        s.deadline_deferrals_total += u64::from(f.deferred);
        s.upload_deferred_total[0] += u64::from(f.deferred);
        s.upload_deferred_total[1] += u64::from(f.deferred_encode);
        s.encoded_tiles_total += u64::from(f.encoded_tiles);
        s.encode_blocks_total += f.encode_blocks;
        if f.encode_blocks > 0 {
            s.frame_encode_micros.record(f.encode_gpu.0);
        }
        s.frame_encoded_tiles = f.encoded_tiles;
        s.frame_encode_blocks = f.encode_blocks;
        s.frame_encode_gpu_micros = f.encode_gpu.0;
        s.frame_deferred = [f.deferred, f.deferred_encode];
        s.encode_share_blocks = f.encode_share;
        s.encode_carry_blocks = f.encode_carry;
        s.frame_upload_micros.record(f.upload_wall.0);
        s.eye_lost_tiles = [0; 4];
        s.eye_bare_items = [0; 4];
        for (i, e) in f.eyes.iter().take(4).enumerate() {
            s.eye_lost_tiles[i] = e.lost;
            s.eye_bare_items[i] = e.bare;
        }
        if f.uploaded.len() > 1 && bytes > f.deadline.bytes.0 {
            self.violations.add(18);
        }
        if f.eyes.iter().any(|e| e.lost > 0 || e.bare > 0) {
            self.violations.add(17);
        }
    }
}
