// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget's properties over random scripts: random views (quadtree
//! cuts with blockers, ancestors and a ring, each tile wanting some imagery),
//! random arrivals, failures, pressure, thermal and time.
//!
//! The runner only records; each property asserts one thing over the trace,
//! so a failure names the property it breaks. Cases: `PROPTEST_CASES`
//! (256 locally, 10 000 in CI's release job; no nightly job exists yet).

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;
mod reference;

use std::collections::{BTreeMap, BTreeSet};

use budget_common::*;
use proptest::prelude::*;
use reference::Lockstep;
use tuile_budget::{
    BudgetConfig, Bytes, Class, Decisions, Failure, FrameInput, Generation, Micros, NeedDelta,
    NeedSet, Pressure, ResourceId, Role, Source, Thermal, TileId,
};

/// One scripted event.
#[derive(Clone, Debug)]
enum Op {
    /// A decision step on a view drawn from the seed.
    Step(u64),
    /// Some of the loads in flight complete (seeded choice).
    Load(u64),
    /// Some of the delivered resources are uploaded.
    Upload(u64),
    /// Some of the loads in flight fail.
    Fail(u64),
    /// A local read misses.
    Miss(u64),
    /// The platform's memory signal.
    Pressure(u8),
    /// The platform's thermal state.
    Thermal(u8),
    /// Time passes.
    Wait(u16),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => any::<u64>().prop_map(Op::Step),
        4 => any::<u64>().prop_map(Op::Load),
        4 => any::<u64>().prop_map(Op::Upload),
        1 => any::<u64>().prop_map(Op::Fail),
        1 => any::<u64>().prop_map(Op::Miss),
        1 => (0u8..4).prop_map(Op::Pressure),
        1 => (0u8..4).prop_map(Op::Thermal),
        2 => (0u16..3_000).prop_map(Op::Wait),
    ]
}

fn script() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(), 1..80)
}

/// The fixed corpus's scripts: drawn like [`script`]'s (the same ops, the
/// same weights, 1 to 79 of them), by a seeded generator of this file's
/// own, so the corpus never moves with a dependency's version.
const CORPUS: u64 = 128;

fn corpus_script(i: u64) -> Vec<Op> {
    let mut m = Mix(0x7475_696c_6520_0000 ^ i);
    let len = 1 + m.next() % 79;
    (0..len)
        .map(|_| match m.next() % 20 {
            0..=5 => Op::Step(m.next()),
            6..=9 => Op::Load(m.next()),
            10..=13 => Op::Upload(m.next()),
            14 => Op::Fail(m.next()),
            15 => Op::Miss(m.next()),
            16 => Op::Pressure((m.next() % 4) as u8),
            17 => Op::Thermal((m.next() % 4) as u8),
            _ => Op::Wait((m.next() % 3_000) as u16),
        })
        .collect()
}

/// SplitMix64: seeded choices inside an op.
struct Mix(u64);
impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const DEPTH: u32 = 4;

/// A random view: a quadtree cut down to `DEPTH`, its ancestors, blockers
/// under some cut tiles, a ring, and an imagery need per drawn tile.
fn view(seed: u64, permute: bool) -> NeedSet {
    let mut m = Mix(seed);
    let mut need = NeedSet::new();
    let mut stack = vec![t(0, 0, 0)];
    while let Some(tile) = stack.pop() {
        let (l, ..) = tile.terrain_coord();
        if l < DEPTH && m.chance(55) {
            need.ancestors.push((tile, need_of(tile, &mut m)));
            stack.extend(children(tile));
        } else {
            need.drawn.push((tile, need_of(tile, &mut m)));
            if l < DEPTH && m.chance(20) {
                for k in children(tile) {
                    need.blockers.push((k, need_of(k, &mut m)));
                }
            }
        }
    }
    for _ in 0..(m.next() % 4) {
        let l = (m.next() % 3) as u32 + 1;
        let n = 1u64 << l;
        let g = t(l, m.next() % n, m.next() % n);
        need.ring.push((g, need_of(g, &mut m)));
    }
    if permute {
        need.drawn.reverse();
        let half = need.ancestors.len() / 2;
        need.ancestors.rotate_left(half);
        need.blockers.reverse();
    }
    need
}

fn need_of(tile: TileId, m: &mut Mix) -> tuile_budget::Need {
    let (l, ..) = tile.terrain_coord();
    draped(tile, l + (m.next() % 6) as u32)
        .with_sse_excess((m.next() % 40) as f32 / 10.0 - 1.0)
        .with_centrality((m.next() % 11) as f32 / 10.0)
        .with_distance((m.next() % 100_000) as f32)
}

/// What the harness saw at one step.
struct Record {
    need: NeedSet,
    d: Decisions,
    /// Resources on the harness's GPU before the step.
    gpu_before: BTreeSet<ResourceId>,
    /// Resources delivered and not yet uploaded before the step.
    transit_before: BTreeSet<ResourceId>,
    /// Network loads the harness had in flight after the step, per class.
    network_after: BTreeMap<Class, usize>,
    /// The (resource, generation) results the harness reported fresh.
    fresh: BTreeSet<(ResourceId, Generation)>,
    /// Whether the floor (the root's mesh and drape) was drawable before
    /// the step.
    floor_in: bool,
    /// Stored resources at the step.
    stored: BTreeSet<ResourceId>,
    audit: bool,
    tracked: usize,
}

struct Harness {
    /// The budget under test: checked step by step against the recorded
    /// reference (`tests/reference/`) on the fixed corpus, alone on random
    /// scripts.
    b: Lockstep,
    cfg: BudgetConfig,
    now: Micros,
    frame: u64,
    store: Store,
    in_flight: BTreeMap<ResourceId, (Generation, Class, Source)>,
    delivered: BTreeMap<ResourceId, Generation>,
    gpu: BTreeSet<ResourceId>,
    fresh: BTreeSet<(ResourceId, Generation)>,
    /// The need of the last step (empty before the first).
    need_last: NeedSet,
}

impl Harness {
    /// A run of a random script: no recording covers it.
    fn new(cfg: BudgetConfig, store_seed: u64) -> Self {
        let b = Lockstep::unchecked(cfg.clone()).unwrap();
        Harness::with(cfg, store_seed, b)
    }

    /// A run of the fixed corpus, recorded under `key`.
    fn keyed(cfg: BudgetConfig, store_seed: u64, key: String) -> Self {
        let b = Lockstep::keyed(cfg.clone(), key).unwrap();
        Harness::with(cfg, store_seed, b)
    }

    fn with(cfg: BudgetConfig, store_seed: u64, b: Lockstep) -> Self {
        let mut m = Mix(store_seed);
        let mut store = Store::default();
        for l in 0..=DEPTH {
            let n = 1u64 << l;
            for x in 0..n {
                for y in 0..n {
                    if m.chance(15) {
                        store.0.insert(ResourceId::Mesh(t(l, x, y)));
                    }
                }
            }
        }
        Harness {
            b,
            cfg,
            now: Micros::ZERO,
            frame: 0,
            store,
            in_flight: BTreeMap::new(),
            delivered: BTreeMap::new(),
            gpu: BTreeSet::new(),
            fresh: BTreeSet::new(),
            need_last: NeedSet::new(),
        }
    }

    fn pick<T: Clone>(items: Vec<T>, seed: u64) -> Vec<T> {
        let mut m = Mix(seed);
        items.into_iter().filter(|_| m.chance(60)).collect()
    }

    fn run(mut self, ops: &[Op], permute: bool) -> Vec<Record> {
        let mut out = Vec::new();
        for op in ops {
            match *op {
                Op::Step(seed) => {
                    let need = view(seed, permute);
                    let gpu_before = self.gpu.clone();
                    let transit_before: BTreeSet<_> = self.delivered.keys().copied().collect();
                    // The floor (D-A1): the root's mesh and its drape.
                    let floor_in = self.gpu.contains(&ResourceId::Mesh(t(0, 0, 0)))
                        && self.gpu.contains(&dr(t(0, 0, 0), 0));
                    // A step reads the whole need one time in four (the
                    // first step, a resynchronisation), else what changed
                    // since the step before: the same decisions either way.
                    let delta = NeedDelta::between(&self.need_last, &need);
                    let input = if seed % 4 == 0 {
                        FrameInput::new(self.frame, self.now, &need)
                    } else {
                        FrameInput::from_delta(self.frame, self.now, &delta)
                    }
                    .with_presence(&self.store);
                    let d = self.b.decide_delta(input, &need);
                    self.need_last = need.clone();
                    for (r, g) in &d.cancel {
                        if self.in_flight.get(r).map(|x| x.0) == Some(*g) {
                            self.in_flight.remove(r);
                        }
                        if self.delivered.get(r) == Some(g) {
                            self.delivered.remove(r);
                        }
                    }
                    for s in &d.start {
                        self.in_flight
                            .insert(s.resource, (s.gen, s.class, s.source));
                    }
                    for (r, g) in &d.deliver {
                        self.delivered.insert(*r, *g);
                    }
                    for r in &d.evict {
                        self.gpu.remove(r);
                    }
                    let mut network_after = BTreeMap::new();
                    for (_, class, src) in self.in_flight.values() {
                        if *src == Source::Network {
                            *network_after.entry(*class).or_insert(0) += 1;
                        }
                    }
                    out.push(Record {
                        need,
                        gpu_before,
                        transit_before,
                        network_after,
                        fresh: self.fresh.clone(),
                        floor_in,
                        stored: self.store.0.clone(),
                        audit: self.b.audit().is_ok(),
                        tracked: self.b.tracked(),
                        d,
                    });
                    self.frame += 1;
                    self.now = self.now.plus(Micros(16_667));
                }
                Op::Load(seed) => {
                    let done: Vec<_> = self.in_flight.iter().map(|(r, x)| (*r, x.0)).collect();
                    for (r, g) in Self::pick(done, seed) {
                        self.in_flight.remove(&r);
                        self.fresh.insert((r, g));
                        self.b.on_loaded(r, g, est(r));
                    }
                }
                Op::Upload(seed) => {
                    let plan: Vec<_> = out
                        .last()
                        .map(|rec: &Record| rec.d.upload_plan.items.clone())
                        .unwrap_or_default();
                    for r in Self::pick(plan, seed) {
                        if let Some(g) = self.delivered.remove(&r) {
                            self.gpu.insert(r);
                            self.b.on_drawable(r, g, est(r).gpu);
                        }
                    }
                }
                Op::Fail(seed) => {
                    let all: Vec<_> = self.in_flight.iter().map(|(r, x)| (*r, x.0)).collect();
                    for (r, g) in Self::pick(all, seed).into_iter().take(1) {
                        self.in_flight.remove(&r);
                        self.b.on_failed(r, g, Failure::Transient);
                    }
                }
                Op::Miss(seed) => {
                    let local: Vec<_> = self
                        .in_flight
                        .iter()
                        .filter(|(_, x)| x.2 == Source::Local)
                        .map(|(r, x)| (*r, x.0))
                        .collect();
                    for (r, g) in Self::pick(local, seed).into_iter().take(1) {
                        self.in_flight.remove(&r);
                        self.store.0.remove(&r);
                        self.b.on_store_miss(r, g);
                    }
                }
                Op::Pressure(p) => self.b.set_pressure(
                    [
                        Pressure::Normal,
                        Pressure::Warning,
                        Pressure::Critical,
                        Pressure::Background,
                    ][usize::from(p % 4)],
                ),
                Op::Thermal(th) => self.b.set_thermal(
                    [
                        Thermal::Nominal,
                        Thermal::Fair,
                        Thermal::Serious,
                        Thermal::Critical,
                    ][usize::from(th % 4)],
                ),
                Op::Wait(ms) => self.now = self.now.plus(Micros::from_millis(u64::from(ms))),
            }
        }
        let _ = &self.cfg;
        out
    }
}

fn interactive() -> BudgetConfig {
    interactive_on(Bytes(MESH.0 * 24 + drape().0 * 24))
}

fn interactive_on(total: Bytes) -> BudgetConfig {
    let mut c = BudgetConfig::interactive(total);
    c.pinned_level = Some(0);
    c.max_in_flight = tuile_budget::PerClass([4, 2, 4, 1]);
    c.max_local_reads = 2;
    c.request_heap = 32;
    c.upload_bytes_per_frame = Bytes(MESH.0 * 3);
    c.delivery_credit = Bytes(MESH.0 * 6);
    c
}

/// [`interactive`] on a third of its memory: eviction and `make_room` run
/// on most steps, so ties in the eviction order are decided often.
fn squeezed() -> BudgetConfig {
    interactive_on(Bytes(MESH.0 * 8 + drape().0 * 8))
}

fn batch() -> BudgetConfig {
    let mut c = BudgetConfig::batch();
    c.pinned_level = Some(0);
    c
}

fn trace(cfg: BudgetConfig, ops: &[Op]) -> Vec<Record> {
    Harness::new(cfg, 7).run(ops, false)
}

/// Covered: the tile, an ancestor of it, or all four children are drawn.
fn covered(d: &Decisions, tile: TileId, depth: u32) -> bool {
    let mut up = Some(tile);
    while let Some(x) = up {
        if d.draw.draws(x) {
            return true;
        }
        up = parent(x);
    }
    depth > 0
        && children(tile)
            .iter()
            .all(|&k| covered_down(d, k, depth - 1))
}

fn covered_down(d: &Decisions, tile: TileId, depth: u32) -> bool {
    d.draw.draws(tile)
        || (depth > 0
            && children(tile)
                .iter()
                .all(|&k| covered_down(d, k, depth - 1)))
}

fn decided_something(tr: &[Record]) -> bool {
    tr.iter().any(|r| !r.d.start.is_empty())
}

fn config() -> ProptestConfig {
    let base = ProptestConfig::default();
    ProptestConfig {
        // Integration tests have no lib.rs to anchor a regressions file.
        failure_persistence: None,
        // About a third of random scripts never reach what a property
        // checks (the floor never comes in, no frame completes) and are
        // rejected as vacuous; the cap must grow with the case count, or
        // 10 000 cases abort on rejects rather than on a counterexample.
        max_global_rejects: base.cases.saturating_mul(4).max(1_024),
        ..base
    }
}

/// O1's guard on a fixed corpus: [`CORPUS`] seeded scripts, each on both
/// policies and a squeezed memory, the need in its order and permuted, every
/// step checked against the reference (`tests/reference/`).
#[test]
fn decide_agrees_with_the_reference_on_a_fixed_corpus() {
    for i in 0..CORPUS {
        let ops = corpus_script(i);
        for (name, cfg) in [
            ("interactive", interactive()),
            ("squeezed", squeezed()),
            ("batch", batch()),
        ] {
            for permute in [false, true] {
                let key = format!("corpus/{i:03}/{name}/{permute}");
                let tr = Harness::keyed(cfg.clone(), 7, key).run(&ops, permute);
                assert!(tr.len() <= ops.len());
            }
        }
    }
}

proptest! {
    #![proptest_config(config())]

    /// I11, C5, C6: nothing protected is ever evicted: not drawn (mesh or
    /// drape worn), not in transit, not the floor.
    #[test]
    fn protected_resources_are_never_evicted(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in &tr {
            for e in &rec.d.evict {
                prop_assert!(!rec.d.draw.items.iter().any(|i| i.mesh == *e || i.drapes.iter().any(|x| x.resource == *e)));
                prop_assert!(rec.gpu_before.contains(e), "{:?} evicted but not on the GPU", e);
                prop_assert!(!rec.transit_before.contains(e), "{:?} evicted in transit", e);
                prop_assert_ne!(e.tile(), t(0, 0, 0));
            }
        }
        prop_assume!(decided_something(&tr));
    }

    /// I12, C11, C25: decisions are stable under any permutation of the need.
    #[test]
    fn decisions_do_not_depend_on_need_order(ops in script()) {
        let a: Vec<u64> = Harness::new(interactive(), 7).run(&ops, false).iter().map(|r| r.d.digest()).collect();
        let tr = Harness::new(interactive(), 7).run(&ops, true);
        prop_assume!(decided_something(&tr));
        let b: Vec<u64> = tr.iter().map(|r| r.d.digest()).collect();
        prop_assert_eq!(a, b);
    }

    /// I12: the same script gives the same digest stream.
    #[test]
    fn the_same_script_gives_the_same_digests(ops in script()) {
        let tr = trace(interactive(), &ops);
        prop_assume!(decided_something(&tr));
        let a: Vec<u64> = tr.iter().map(|r| r.d.digest()).collect();
        let b: Vec<u64> = trace(interactive(), &ops).iter().map(|r| r.d.digest()).collect();
        prop_assert_eq!(a, b);
    }

    /// I2, §8: the ledger equals the sum of the states, every step.
    #[test]
    fn the_ledger_equals_the_sum_of_the_states(ops in script()) {
        let tr = trace(interactive(), &ops);
        prop_assert!(tr.iter().all(|r| r.audit));
        prop_assume!(decided_something(&tr));
    }

    /// I5, C25: a cancelled or stale result is never delivered.
    #[test]
    fn only_fresh_results_are_delivered(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in &tr {
            for (r, g) in &rec.d.deliver {
                prop_assert!(rec.fresh.contains(&(*r, *g)), "{:?} {:?} delivered without a fresh result", r, g);
            }
        }
        prop_assume!(decided_something(&tr));
    }

    /// I1, I9, C27: once the floor is drawable, every wanted tile is covered
    /// on the ground, every step, in both swap directions.
    #[test]
    fn the_ground_is_always_covered_once_the_floor_is_in(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in tr.iter().filter(|r| r.floor_in) {
            prop_assert_eq!(rec.d.stats.lost, 0);
            for (tile, _) in &rec.need.drawn {
                prop_assert!(covered(&rec.d, *tile, DEPTH), "{:?} uncovered", tile);
            }
        }
        prop_assume!(tr.iter().any(|r| r.floor_in));
    }

    /// I1 (D-A1): every drawn item wears its drapes, from the first step:
    /// an item standing for a tile that asks for imagery wears a drape on
    /// the GPU, and the budget counts nothing bare.
    #[test]
    fn every_drawn_item_wears_its_drapes_once_the_floor_is_in(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in &tr {
            prop_assert_eq!(rec.d.stats.bare, 0);
            let draped: BTreeSet<TileId> = rec.need.drawn.iter().filter(|(_, n)| n.imagery.is_some()).map(|(t, _)| *t).collect();
            for i in &rec.d.draw.items {
                if i.serves.iter().any(|t| draped.contains(t)) {
                    prop_assert!(!i.drapes.is_empty(), "{:?} drawn bare", i.mesh);
                }
                // Drawn and worn only once acknowledged on the GPU.
                prop_assert!(rec.gpu_before.contains(&i.mesh), "{:?} drawn, not uploaded", i.mesh);
                for x in &i.drapes {
                    prop_assert!(rec.gpu_before.contains(&x.resource), "{:?} worn, not uploaded", x.resource);
                }
            }
        }
        prop_assume!(tr.iter().any(|r| !r.d.draw.items.is_empty()));
    }

    /// I14: no network request for anything held locally, no re-upload of
    /// anything on the GPU.
    #[test]
    fn nothing_held_is_fetched_again(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in &tr {
            for s in &rec.d.start {
                prop_assert!(!rec.gpu_before.contains(&s.resource));
                if s.source == Source::Network {
                    prop_assert!(!rec.stored.contains(&s.resource), "{:?} fetched while stored", s.resource);
                }
            }
            for (r, _) in &rec.d.deliver {
                prop_assert!(!rec.gpu_before.contains(r));
            }
        }
        prop_assume!(decided_something(&tr));
    }

    /// C16, I15: quotas hold per class, and local reads have their own.
    #[test]
    fn quotas_hold_per_class(ops in script()) {
        let cfg = interactive();
        let tr = trace(cfg.clone(), &ops);
        for rec in &tr {
            for (class, n) in &rec.network_after {
                prop_assert!(*n <= cfg.max_in_flight[*class] as usize);
            }
            prop_assert!(rec.d.stats.local_reads <= cfg.max_local_reads);
            prop_assert_eq!(rec.d.stats.duplicates_in_flight, 0);
        }
        prop_assume!(decided_something(&tr));
    }

    /// I2: what is not protected fits in the target after every step; the
    /// overflow is the protected set's alone.
    #[test]
    fn only_the_protected_set_overflows(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in &tr {
            let s = &rec.d.stats;
            for class in Class::ALL {
                prop_assert!(s.gpu[class].minus(s.protected[class]) <= s.target[class]);
            }
        }
        prop_assume!(decided_something(&tr));
    }

    /// I2, review of point 9: no class ever crosses its hard limit, whatever
    /// the urgency, the pressure or the arrivals.
    #[test]
    fn no_class_ever_crosses_its_hard_limit(ops in script()) {
        let mut cfg = interactive();
        cfg.hard_limit[Class::TerrainMesh] = Bytes(MESH.0 * 6);
        cfg.target[Class::TerrainMesh] = Bytes(MESH.0 * 3);
        cfg.overflow[Class::TerrainMesh] = Bytes(MESH.0);
        let tr = trace(cfg.clone(), &ops);
        for rec in &tr {
            for class in Class::ALL {
                prop_assert!(rec.d.stats.gpu[class] <= cfg.hard_limit[class]);
            }
            // The pool's members together (D-A3) never cross the pool's.
            let pooled = Class::ALL
                .iter()
                .filter(|c| cfg.pool.holds(**c))
                .fold(Bytes::ZERO, |a, c| a.plus(rec.d.stats.gpu[*c]));
            prop_assert!(pooled <= cfg.pool.hard_limit);
        }
        prop_assume!(decided_something(&tr));
    }

    /// C23, I7: the upload plan and the delivery respect their bytes, but for
    /// their first item; the wish list is bounded.
    #[test]
    fn plans_and_queues_are_bounded(ops in script()) {
        let cfg = interactive();
        let tr = trace(cfg.clone(), &ops);
        for rec in &tr {
            let p = &rec.d.upload_plan;
            prop_assert!(p.items.len() <= 1 || p.bytes <= cfg.upload_bytes_per_frame);
            prop_assert!(rec.d.stats.heap_len <= cfg.request_heap);
            prop_assert!(rec.d.deliver.len() <= 1 || rec.d.stats.in_transit <= cfg.delivery_credit);
            prop_assert!(rec.tracked <= 4 * 85 + 16);
        }
        prop_assume!(decided_something(&tr));
    }

    /// C24: each mesh is drawn once, in one role, and a fallback never serves
    /// a tile drawn as itself.
    #[test]
    fn each_mesh_is_drawn_once_in_one_role(ops in script()) {
        let tr = trace(interactive(), &ops);
        for rec in &tr {
            let items = &rec.d.draw.items;
            prop_assert!(items.windows(2).all(|w| w[0].mesh < w[1].mesh));
            for i in items.iter().filter(|i| i.role == Role::Fallback) {
                prop_assert!(i.serves.iter().all(|s| !rec.d.draw.draws(*s)));
            }
        }
        prop_assume!(tr.iter().any(|r| !r.d.draw.items.is_empty()));
    }

    /// I13: under Batch, a ready frame is exact: every wanted tile drawn as
    /// itself with its needed drape; nothing it needs is evicted.
    #[test]
    fn a_ready_batch_frame_is_exact(ops in script(), view_seed in any::<u64>()) {
        // A film holds its frame until complete: end on one view, completed.
        // Failures end a film (tested by hand); here every load succeeds.
        let mut ops: Vec<Op> = ops.into_iter().filter(|o| !matches!(o, Op::Fail(_) | Op::Wait(_))).collect();
        for k in 0..16u64 {
            ops.extend([Op::Step(view_seed), Op::Load(k), Op::Step(view_seed), Op::Upload(k)]);
        }
        let tr = trace(batch(), &ops);
        for rec in &tr {
            let wanted: BTreeSet<ResourceId> = rec.need.drawn.iter().map(|(t, _)| ResourceId::Mesh(*t)).collect();
            for e in &rec.d.evict {
                prop_assert!(!wanted.contains(e));
            }
            if rec.d.frame_ready {
                for (tile, _) in &rec.need.drawn {
                    let item = rec.d.draw.get(ResourceId::Mesh(*tile));
                    prop_assert_eq!(item.map(|i| i.role), Some(Role::Exact));
                }
            }
        }
        prop_assume!(tr.iter().any(|r| r.d.frame_ready));
    }
}

/// The trace is not vacuous: over a long completing script the budget does
/// decide, and the floor comes in.
#[test]
fn a_completing_script_brings_the_floor_in() {
    let mut ops = Vec::new();
    for k in 0..60u64 {
        ops.extend([Op::Step(k % 5), Op::Load(k), Op::Step(k % 5), Op::Upload(k)]);
    }
    let tr = trace(interactive(), &ops);
    assert!(decided_something(&tr));
    assert!(tr.iter().any(|r| r.floor_in));
}

/// I2, a counterexample `only_the_protected_set_overflows` found on
/// 2026-10-20 (random seed), shrunk by proptest and kept as it came.
///
/// At step 15, the first under critical pressure, the pool's target falls to
/// 1 556 425 bytes: the terrain holds 1 024 000, all protected, so the drape's
/// effective target is its floor, 40 % of the pool, 622 570. The drapes hold
/// two unprotected drapes of 349 524, 699 048 bytes: evicting one would leave
/// 349 524, under the floor, so the sweep keeps both (`evict_pooled`: for the
/// pool's sake, only down to its floor) and I2 broke by less than one drape.
/// The floor now yields to I2 (Laurent, 2026-10-21; spec §15.1): a member
/// goes below its floor when, and only as far as, its unprotected bytes
/// would otherwise stay over its target.
#[test]
fn only_the_protected_set_overflows_under_critical_pressure() {
    use Op::*;
    let ops = [
        Step(14221606094138882575),
        Fail(9247058294758764962),
        Step(407097014948790927),
        Step(4602890098571774514),
        Wait(467),
        Fail(2380909755978544125),
        Load(2931322212256554779),
        Step(476479648722239456),
        Load(5583206321300288920),
        Fail(2551855388808382338),
        Step(14912161159751138441),
        Step(7362518111398382727),
        Wait(484),
        Load(6195026662679607468),
        Step(9641043605914621264),
        Fail(1977524987867638),
        Fail(0),
        Step(87764535625442709),
        Upload(981173675061591633),
        Load(4002932718903631496),
        Step(2106386299728441571),
        Step(7251024675101495551),
        Upload(0),
        Step(488645390518),
        Upload(0),
        Upload(0),
        Step(905582427387015119),
        Thermal(0),
        Upload(0),
        Step(1618427100598063901),
        Fail(647557445613936138),
        Load(10851023134809040177),
        Step(1636695720543371659),
        Load(0),
        Load(0),
        Upload(2008301256151071165),
        Step(907489178961695111),
        Pressure(2),
        Step(0),
        Step(0),
        Step(0),
        Step(0),
        Step(0),
    ];
    let tr = trace(interactive(), &ops);
    for (k, rec) in tr.iter().enumerate() {
        let s = &rec.d.stats;
        for class in Class::ALL {
            assert!(
                s.gpu[class].minus(s.protected[class]) <= s.target[class],
                "step {k}, {class:?}: {} bytes on the GPU, {} protected, target {}",
                s.gpu[class].0,
                s.protected[class].0,
                s.target[class].0
            );
        }
    }
}
