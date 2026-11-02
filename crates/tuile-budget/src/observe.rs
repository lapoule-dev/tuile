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
use super::stats::{CancelReason, EvictReason, GAP_BUCKETS, LATENCY_BUCKETS};
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
    /// `new_epoch`.
    NewEpoch(Class, Option<LayerId>),
    /// `release_beyond`, and how many GPU copies it dropped.
    ReleaseBeyond(Bytes, u32),
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
    /// Plan items it deferred.
    pub deferred: u32,
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
    /// `deadline_deferrals_total`.
    pub deadline_deferrals_total: u64,
    /// `frame_upload_seconds`, wall clock, µs.
    pub frame_upload_micros: Histogram,
    /// `estimate_drift_exceeded_total{class}`.
    pub estimate_drift_exceeded_total: PerClass<u64>,
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
    /// `rejected_inputs_total`.
    pub rejected_inputs_total: u64,
}

/// The closed label sets of spec §16.5.
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
    /// `kind` of a swap.
    pub const SWAP: [&str; 4] = ["refine", "coarsen", "mesh_drape", "layer"];
    /// `eye`.
    pub const EYE: [&str; 4] = ["0", "1", "2", "3"];
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
        per(&mut out, "uploads_total", &self.uploads_total);
        per(&mut out, "upload_bytes_total", &self.upload_bytes_total);
        per(
            &mut out,
            "estimate_drift_exceeded_total",
            &self.estimate_drift_exceeded_total,
        );
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
        out
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
        BudgetMeter {
            cfg_pinned: cfg.pinned_level,
            cfg_policy: Some(cfg.policy),
            ..BudgetMeter::default()
        }
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
        s
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
            s.estimate_drift_exceeded_total[c] += u64::from(st.drift_exceeded[c]);
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
        s.underdraped_tiles = st.underdraped;
        s.imagery_over_terrain = st.imagery_over_terrain;
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
