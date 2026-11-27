// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The progressive coarse and neighbour preload
//! (`docs/explorations/budget-progressive-coarse-preload.md`): the floor
//! through level 3, the levels above the need and the neighbours around the
//! view preloaded by one score, released by the same score.
//!
//! The need is built here as the geometry server's traversal builds it
//! (`tuile_core::neighbourhood`): on the unit-square quadtree of
//! `budget_common`, a view at a point `F` of the ground needing level `n`
//! draws the level-`n` tiles around `F`, and lists as preloads, per level
//! from the floor to `n + 1`, the `⌊π(ρ+1)²⌋` tiles nearest `F` with their
//! distance in tiles of their own level.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use std::collections::BTreeSet;

use budget_common::*;
use proptest::prelude::*;
use tuile_budget::score::Side;
use tuile_budget::{
    BudgetConfig, Bytes, Class, Decisions, Need, NeedSet, ResourceId, TileId, CERTAIN,
};

/// The pinned floor of these tests: levels `0..=2`, as the viewer's; level
/// 3 and up are graded by the score (`coarse_from`, `coarse_boost`).
const FLOOR: u32 = 2;

/// The tiles listed per level at radius `rho`.
fn per_level(rho: u32) -> usize {
    let r = f64::from(rho) + 1.0;
    (std::f64::consts::PI * r * r).floor() as usize
}

/// The distance from `(fx, fy)` (unit square) to tile `(l, x, y)`, in
/// tiles of level `l`.
fn tiles_away(l: u32, x: u64, y: u64, fx: f64, fy: f64) -> f64 {
    let n = f64::from(1u32 << l);
    let (px, py) = (fx * n, fy * n);
    let dx = (x as f64 - px).max(px - (x as f64 + 1.0)).max(0.0);
    let dy = (y as f64 - py).max(py - (y as f64 + 1.0)).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// The tiles of level `l` within `r` tiles of `(fx, fy)`, nearest first,
/// ties by id.
fn around(l: u32, fx: f64, fy: f64, r: f64) -> Vec<(f64, TileId)> {
    let n = 1u64 << l;
    let (cx, cy) = ((fx * n as f64) as i64, (fy * n as f64) as i64);
    let span = r.ceil() as i64 + 1;
    let mut out = Vec::new();
    for x in (cx - span).max(0)..=(cx + span).min(n as i64 - 1) {
        for y in (cy - span).max(0)..=(cy + span).min(n as i64 - 1) {
            let d = tiles_away(l, x as u64, y as u64, fx, fy);
            if d <= r {
                out.push((d, t(l, x as u64, y as u64)));
            }
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    out
}

/// The view: at `(fx, fy)` needing level `n`, drawing the level-`n` tiles
/// within `reach` of it whose centre lies in the half plane `look` points
/// to (`None`: all around).
#[derive(Clone, Copy, Debug)]
struct View {
    fx: f64,
    fy: f64,
    n: u32,
    reach: f64,
    look: Option<(f64, f64)>,
}

impl View {
    fn at(fx: f64, fy: f64, n: u32) -> Self {
        View {
            fx,
            fy,
            n,
            reach: 1.5,
            look: None,
        }
    }

    fn drawn(&self) -> Vec<TileId> {
        around(self.n, self.fx, self.fy, self.reach)
            .into_iter()
            .map(|(_, t)| t)
            .filter(|t| {
                let Some((lx, ly)) = self.look else {
                    return true;
                };
                let (l, x, y) = t.terrain_coord();
                let n = f64::from(1u32 << l);
                let (cx, cy) = (
                    (x as f64 + 0.5) / n - self.fx,
                    (y as f64 + 0.5) / n - self.fy,
                );
                cx * lx + cy * ly >= 0.0
            })
            .collect()
    }

    /// The need as the traversal lists it at radius `rho`.
    fn need(&self, rho: u32) -> NeedSet {
        let drawn = self.drawn();
        let drawn_set: BTreeSet<TileId> = drawn.iter().copied().collect();
        let mut ns = NeedSet {
            drawn: drawn.iter().map(|&t| (t, need(t))).collect(),
            ..NeedSet::default()
        };
        let mut listed: BTreeSet<TileId> = BTreeSet::new();
        // The floor.
        for l in 0..=FLOOR {
            for x in 0..(1u64 << l) {
                for y in 0..(1u64 << l) {
                    let id = t(l, x, y);
                    if !drawn_set.contains(&id) && listed.insert(id) {
                        ns.ancestors.push((id, need(id)));
                    }
                }
            }
        }
        // The chain of the drawn tiles: their fallbacks.
        let mut chain: BTreeSet<TileId> = BTreeSet::new();
        for &d in &drawn {
            let mut p = parent(d);
            while let Some(a) = p {
                if !drawn_set.contains(&a) {
                    chain.insert(a);
                }
                p = parent(a);
            }
        }
        // The neighbourhood, from the floor down to one under the need:
        // the ring, never protected.
        if rho > 0 {
            for l in FLOOR + 1..=self.n + 1 {
                for (d, id) in around(l, self.fx, self.fy, f64::from(rho))
                    .into_iter()
                    .take(per_level(rho))
                {
                    if !drawn_set.contains(&id) && !chain.contains(&id) && listed.insert(id) {
                        let n: Need = need(id).with_proximity(self.n, d as f32);
                        ns.ring.push((id, n));
                    }
                }
            }
        }
        // The chain of every drawn tile.
        for &d in &drawn {
            let mut p = parent(d);
            while let Some(a) = p {
                if !drawn_set.contains(&a) && listed.insert(a) {
                    let (l, x, y) = a.terrain_coord();
                    let away = tiles_away(l, x, y, self.fx, self.fy) as f32;
                    ns.ancestors.push((a, need(a).with_proximity(self.n, away)));
                }
                p = parent(a);
            }
        }
        ns
    }
}

fn config(total: Bytes, rho: u32) -> BudgetConfig {
    let mut c = BudgetConfig::interactive(total);
    c.pinned_level = Some(FLOOR);
    c.reach_tiles = rho;
    c
}

fn gpu(r: &Rig, id: TileId) -> bool {
    r.b.resource(ResourceId::Mesh(id))
        .is_some_and(|v| v.gpu > Bytes::ZERO)
}

/// Steps on `need`, completing every load and upload, until a step asks for
/// nothing; at most `max` steps.
fn settle(r: &mut Rig, need: &NeedSet, max: usize) -> Decisions {
    r.settle(need, max)
}

/// Test 1 — The pinned floor (levels 0 to 2) stays resident under pressure: it is
/// loaded first, and the pressure that evicts the neighbourhood never takes
/// it; the graded coarse levels go last, level 4 before level 3.
#[test]
fn the_pinned_floor_stays_under_pressure_and_the_coarse_levels_go_last() {
    let mut r = Rig::new(config(Bytes::mib(256), 4));
    let v = View::at(0.37, 0.61, 13);
    let need = v.need(4);
    settle(&mut r, &need, 400);
    // A memory signal, then a budget a twentieth as large: the sweep runs.
    r.b.retarget(Bytes::mib(12));
    let mut evicted = 0;
    for _ in 0..30 {
        let d = r.step(&need);
        evicted += d.evict.len();
        for e in &d.evict {
            if let ResourceId::Mesh(id) = e {
                assert!(id.terrain_coord().0 > FLOOR, "the floor's {id:?} evicted");
            }
        }
        r.complete(&d);
    }
    assert!(
        evicted > 0,
        "the pressure evicted nothing: the test proves nothing"
    );
    // The graded coarse levels outrank everything finer but the need.
    let p = tuile_budget::spec::PROGRESSIVE;
    let a = |l| tuile_budget::progressive_terms(&p, l, 13, 0, 4).0;
    assert!(a(3) > a(4) && a(4) > a(5) && a(5) > a(8) && a(12) > a(9));
    assert!(
        a(3) > a(12) && a(3) > a(14),
        "level 3 is released after the levels by the need"
    );
    for l in 0..=FLOOR {
        for x in 0..(1u64 << l) {
            for y in 0..(1u64 << l) {
                assert!(gpu(&r, t(l, x, y)), "floor tile {l}/{x}/{y} not resident");
            }
        }
    }
}

/// Test 2 — An aggressive zoom-out from level 13, one level every two steps with
/// no load completing on the way: every step's view is drawn exactly, with
/// its own tiles — the neighbourhood held them already — and levels 11 and
/// 12 were resident before the zoom-out began.
#[test]
fn an_aggressive_zoom_out_from_level_13_finds_its_ground_resident() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let (fx, fy) = (0.4137, 0.5871);
    let start = View::at(fx, fy, 13);
    settle(&mut r, &start.need(4), 600);
    for l in [11, 12] {
        for (_, id) in around(l, fx, fy, 2.0) {
            assert!(
                gpu(&r, id),
                "level {l}: {id:?} not resident before the zoom-out"
            );
        }
    }
    for n in (5..13).rev() {
        let v = View::at(fx, fy, n);
        let need = v.need(4);
        for _ in 0..2 {
            let d = r.step(&need);
            assert_eq!(d.stats.lost, 0, "level {n}: a hole");
            assert_eq!(
                d.stats.fallback, 0,
                "level {n}: {} tiles drawn by a coarser ancestor",
                d.stats.fallback
            );
            // Nothing completes: the network stalled for the whole zoom-out.
        }
    }
}

/// Test 3 — A rotation near the ground: the view turns from east to west; the
/// tiles the turn brings in were neighbours, resident, and draw at once.
#[test]
fn a_rotation_near_the_ground_finds_its_neighbours_resident() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let (fx, fy) = (0.6213, 0.3149);
    let east = View {
        reach: 3.0,
        look: Some((1.0, 0.0)),
        ..View::at(fx, fy, 13)
    };
    settle(&mut r, &east.need(4), 600);
    let west = View {
        look: Some((-1.0, 0.0)),
        ..east
    };
    let d = r.step(&west.need(4));
    assert!(west.drawn().iter().any(|t| !east.drawn().contains(t)));
    assert_eq!(d.stats.lost, 0);
    assert_eq!(d.stats.fallback, 0, "the turn found coarser ground only");
}

/// Test 4 — The release order follows the score: under a shrinking budget, every
/// preload evicted scores no more than any preload still held (the
/// present need and the floor apart).
#[test]
fn the_release_order_follows_the_score() {
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: 12,
        ..ProptestConfig::default()
    });
    let strategy = (
        0.05f64..0.95,
        0.05f64..0.95,
        9u32..14,
        2u32..5,
        1_800u64..2_800,
    );
    runner
        .run(&strategy, |(fx, fy, n, rho, permille)| {
            let mut r = Rig::new(config(Bytes::mib(512), rho));
            let v = View::at(fx, fy, n);
            let need = v.need(rho);
            let held = settle(&mut r, &need, 600).stats.gpu[Class::TerrainMesh];
            // A device total under which terrain's share (about a third)
            // holds a third to four fifths of what is held now.
            r.b.retarget(Bytes(held.0 * permille / 1_000));
            let d = r.step(&need);
            let evicted: Vec<u64> = d
                .scores
                .iter()
                .filter(|e| e.side == Side::Evict && e.class == Class::TerrainMesh)
                .map(|e| e.terms.score)
                .collect();
            let held = r.b.held_report();
            let kept_least = held
                .lowest
                .iter()
                .filter(|h| h.state == tuile_budget::score::HeldState::Wanted)
                .filter(|h| h.class == Class::TerrainMesh)
                .map(|h| h.terms.score)
                .min();
            // Not vacuous: the pressure evicted preloads and kept some.
            prop_assert!(!evicted.is_empty(), "nothing evicted");
            let (Some(max_evicted), Some(least)) = (evicted.iter().max(), kept_least) else {
                return Err(TestCaseError::fail("no preload kept"));
            };
            prop_assert!(
                *max_evicted <= least,
                "evicted a score of {max_evicted} while keeping {least}"
            );
            Ok(())
        })
        .unwrap();
}

/// Test 5 — The bound: what the need lists as preloads is at most `⌊π(ρ+1)²⌋` a
/// level, and so at most the floor plus that many per level; the
/// probabilities the budget gives them are those of the formula.
#[test]
fn the_preloads_are_bounded_per_profile() {
    for (rho, k) in [(2u32, 28usize), (4, 78)] {
        assert_eq!(per_level(rho), k);
        for n in 5..=16u32 {
            let v = View::at(0.3711, 0.6283, n);
            let need = v.need(rho);
            let floor = (0..=FLOOR).map(|l| 1usize << (2 * l)).sum::<usize>();
            let placed: Vec<_> = need
                .ancestors
                .iter()
                .chain(need.ring.iter())
                .filter(|(_, n)| n.proximity.is_some())
                .collect();
            for l in FLOOR + 1..=n + 1 {
                let at = placed
                    .iter()
                    .filter(|(t, _)| t.terrain_coord().0 == l)
                    .count();
                // The chain of the drawn tiles adds at most the drawn ones'.
                assert!(at <= k + v.drawn().len(), "ρ {rho}, level {l}: {at}");
            }
            let bound = floor + (n + 1 - FLOOR) as usize * k + v.drawn().len() * n as usize;
            assert!(
                need.ancestors.len() + need.ring.len() <= bound,
                "ρ {rho}, n {n}"
            );
        }
    }
    // A(ℓ, n) and D(d) as the document writes them.
    let p = tuile_budget::spec::PROGRESSIVE;
    let (a, d) = tuile_budget::progressive_terms(&p, 12, 13, 0, 4);
    assert_eq!((a, d), (CERTAIN * 8 / 10, CERTAIN));
    let (a, _) = tuile_budget::progressive_terms(&p, 11, 13, 0, 4);
    assert_eq!(a, CERTAIN * 4 / 10);
    let (_, d) = tuile_budget::progressive_terms(&p, 13, 13, 64, 4);
    assert_eq!(d, CERTAIN / 4, "¼ at d = ρ");
    let (a, _) = tuile_budget::progressive_terms(&p, 14, 13, 0, 4);
    assert_eq!(a, CERTAIN * 2 / 10);
}

/// Test 6 — Preloads go on in the background while the view is still, and never
/// delay the present need: a hole that opens while preloads hold the slots
/// starts on the step it opens.
#[test]
fn preloads_continue_when_idle_and_never_delay_the_present_need() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let v = View::at(0.5531, 0.4412, 13);
    let need = v.need(4);
    // Loads take three steps; the view does not move.
    let mut steps_starting_preloads = 0;
    for i in 0..120 {
        let d = r.step(&need);
        if d.scores.iter().any(|e| e.side == Side::Preload) {
            steps_starting_preloads += 1;
        }
        if i % 3 == 2 {
            r.load_where(|_| true);
        }
        r.upload(&d);
    }
    assert!(
        steps_starting_preloads > 20,
        "the network idled: preloads started on {steps_starting_preloads} steps"
    );
    // The neighbourhood came in: the level under the need, around the view.
    for (_, id) in around(12, v.fx, v.fy, 2.0) {
        assert!(gpu(&r, id), "{id:?} not preloaded");
    }
    // Preloads in flight now, and a hole opens beside the view.
    let d = r.step(&need);
    r.upload(&d);
    // A hole opens beside the view: it starts at once.
    let (l, x, y) = v.drawn()[0].terrain_coord();
    let hole = t(l, x + 9, y);
    let mut urgent = need.clone();
    urgent.drawn.push((hole, budget_common::need(hole)));
    let d = r.step(&urgent);
    assert!(
        d.start.iter().any(|s| s.resource == ResourceId::Mesh(hole)),
        "the hole waited for the preloads"
    );
}

/// The margin's swap: room full of preloads around a level-9 view, the
/// view zooms in to level 12; the levels just above the new need score more
/// than the coarse ones held by more than the margin, and take their place
/// — evicted as swaps, lowest score first — while nothing the present need
/// wants is displaced.
#[test]
fn a_preload_worth_more_by_the_margin_takes_a_weaker_ones_place() {
    let (fx, fy) = (0.3141, 0.2718);
    let mut r = Rig::new(config(Bytes::mib(64), 4));
    let far = View::at(fx, fy, 9);
    settle(&mut r, &far.need(4), 600);
    let near = View::at(fx, fy, 12);
    let need = near.need(4);
    let mut swaps = 0;
    for _ in 0..400 {
        let d = r.step(&need);
        swaps += d
            .scores
            .iter()
            .filter(|e| e.reason == tuile_budget::score::Reason::Swap)
            .count();
        assert_eq!(d.stats.lost, 0);
        r.complete(&d);
    }
    assert!(swaps > 0, "no preload took a weaker one's place");
    for (_, id) in around(11, fx, fy, 1.0) {
        assert!(
            gpu(&r, id),
            "level 11: {id:?} not resident after the zoom-in"
        );
    }
}

/// A grazing view: tiles near the eye at level 13, the far ground towards
/// the horizon drawn at level 7. The traversal places each ancestor against
/// the need *at its place* — the finest level drawn under it — so the far
/// ground's coarse ancestors (level 6, `A` = a0) load before a refinement
/// under the eye (level 14, `A` = a↓).
#[test]
fn a_grazing_view_loads_the_far_coarse_ground_before_a_refinement_under_the_eye() {
    let mut c = config(Bytes::mib(512), 4);
    c.max_in_flight = tuile_budget::PerClass::splat(2);
    let mut r = Rig::new(c);
    let near: Vec<TileId> = (0..3).map(|x| t(13, 4000 + x, 3000)).collect();
    let far: Vec<TileId> = (0..3).map(|x| t(7, 70 + x, 40)).collect();
    let mut ns = NeedSet {
        drawn: near.iter().chain(&far).map(|&t| (t, need(t))).collect(),
        ..NeedSet::default()
    };
    // The far ground's parents, on the footprint, needing level 7 there.
    let far_parents: BTreeSet<TileId> = far.iter().filter_map(|t| parent(*t)).collect();
    for &p in &far_parents {
        ns.ancestors.push((p, need(p).with_proximity(7, 0.0)));
    }
    // A refinement under the eye: level 14, on the footprint, needing 13.
    let fine: Vec<TileId> = near.iter().flat_map(|t| children(*t)).take(4).collect();
    for &f in &fine {
        ns.ring.push((f, need(f).with_proximity(13, 0.0)));
    }
    // The drawn tiles in first.
    let drawn: BTreeSet<TileId> = ns.drawn.iter().map(|(t, _)| *t).collect();
    let mut order: Vec<TileId> = Vec::new();
    for _ in 0..40 {
        let d = r.step(&ns);
        for s in &d.start {
            if let ResourceId::Mesh(t) = s.resource {
                if !drawn.contains(&t) {
                    order.push(t);
                }
            }
        }
        r.load_where(|_| true);
        r.upload(&d);
    }
    let first_fine = order.iter().position(|t| fine.contains(t)).unwrap();
    let last_far = order.iter().rposition(|t| far_parents.contains(t)).unwrap();
    assert!(
        last_far < first_fine,
        "the refinement came first: {order:?}"
    );
}

/// `R` is measured: a slower network, measured, raises the restore cost of
/// what comes back from it — its copies held score higher (kept longer) and
/// its preloads gain more lead (started sooner) — and the specification's
/// constant stands until a tier has samples.
#[test]
fn a_slower_measured_network_raises_its_restore_cost() {
    use tuile_budget::{Measured, TierSample};
    let run = |latency_us: Option<u32>| {
        // A small decoded tier: the copies held lose their decoded copy and
        // would come back from the network.
        let mut c = config(Bytes::mib(512), 4);
        c.decoded_target = Bytes::mib(2);
        let mut r = Rig::new(c);
        if let Some(l) = latency_us {
            r.b.measure(Measured {
                interval: 1,
                network: TierSample {
                    latency_us: l,
                    bytes_per_ms: 0,
                    failures_permille: 0,
                    samples: 20,
                },
                ..Measured::default()
            });
        }
        let v = View::at(0.4, 0.6, 11);
        let need = v.need(4);
        let mut preload = None;
        for _ in 0..400 {
            let d = r.step(&need);
            let lead = d
                .scores
                .iter()
                .filter(|e| e.side == Side::Preload)
                .map(|e| e.terms.r_us)
                .max();
            preload = preload.max(lead);
            let idle = d.start.is_empty() && d.deliver.is_empty() && d.upload_plan.items.is_empty();
            r.complete(&d);
            if idle {
                break;
            }
        }
        let held = r.b.held_report();
        let restore = r.b.restore_costs();
        // The copies held without their decoded copy come back from the
        // network: the largest R held, and the largest score.
        let score = held
            .board
            .cells()
            .fold(None, |acc: Option<(u64, u64)>, (.., c)| {
                let (r, sc) = acc.unwrap_or((0, 0));
                Some((r.max(c.r_us.max), sc.max(c.score.max)))
            });
        (restore, preload, score)
    };
    let (default, ..) = run(None);
    assert_eq!(
        default.remote,
        tuile_budget::Micros::from_millis(150),
        "the constant until samples"
    );
    let (fast, fast_preload, fast_held) = run(Some(80_000));
    let (slow, slow_preload, slow_held) = run(Some(1_200_000));
    assert!(slow.remote > fast.remote, "{slow:?} {fast:?}");
    assert!(
        slow_preload > fast_preload,
        "{slow_preload:?} {fast_preload:?}"
    );
    let (fast_r, fast_s) = fast_held.unwrap();
    let (slow_r, slow_s) = slow_held.unwrap();
    assert!(
        slow_r > fast_r && slow_s > fast_s,
        "{slow_held:?} {fast_held:?}"
    );
}

/// The release order follows the camera without re-keying what is held:
/// a turn re-places only the wants the need named — the neighbourhood —
/// and re-keys the persistent index only for those filed whose probability
/// moved, as many with 4 000 as with 40 000 copies held.
#[test]
#[ignore = "slow: a turn re-keys what it names, ~68 s in debug"]
fn a_turn_re_keys_what_it_names_not_what_is_held() {
    let rekeys_with = |held: u64| -> (u32, u32) {
        let mut c = config(Bytes::gib(64), 4);
        c.max_in_flight = tuile_budget::PerClass::splat(8_192);
        c.request_heap = 65_536;
        c.upload_bytes_per_frame = Bytes::gib(64);
        c.delivery_credit = Bytes::gib(64);
        let mut r = Rig::new(c);
        // `held` copies at level 9 far from the view, then left.
        let side = (held as f64).sqrt().ceil() as u64;
        let bulk: Vec<TileId> = (0..held).map(|i| t(9, i % side, 300 + i / side)).collect();
        let first = NeedSet {
            drawn: bulk.iter().map(|&t| (t, need(t))).collect(),
            ..NeedSet::default()
        };
        settle(&mut r, &first, 50);
        let east = View {
            reach: 3.0,
            look: Some((1.0, 0.0)),
            ..View::at(0.21, 0.13, 13)
        };
        settle(&mut r, &east.need(4), 200);
        // The turn: the neighbourhood's distances move (the motion's side
        // nearer), every tile of it named.
        let mut turned = east.need(4);
        for (_, n) in turned.ring.iter_mut() {
            if let Some(p) = n.proximity.as_mut() {
                p.tiles_away = (p.tiles_away + 0.75).min(4.0);
            }
        }
        let d = r.step(&turned);
        (d.stats.score.places, d.stats.score.rekeys)
    };
    let (places_small, rekeys_small) = rekeys_with(4_000);
    let (places_large, rekeys_large) = rekeys_with(40_000);
    assert!(places_small > 0, "the turn placed nothing");
    assert_eq!(places_small, places_large, "places depend on what is held");
    assert_eq!(rekeys_small, rekeys_large, "re-keys depend on what is held");
    // Bounded by the neighbourhood: K per level over the levels listed.
    assert!(
        places_large as usize <= per_level(4) * 14 * 2,
        "{places_large}"
    );
}

/// Laurent's Tuile.app, 4.3 km over a coast (main 9a5ee85e): the drapes'
/// class at its target, nearly all of it undrawn fine drapes left from a
/// zoom-in, 220 tiles under their imagery need, and nothing loading. The
/// pool full of what nobody draws must give way to the need: still, the
/// view's own drapes load and every tile wears its own level.
#[test]
fn a_pool_full_of_undrawn_fine_drapes_gives_way_to_the_need() {
    let mut r = Rig::new(config(Bytes::mib(96), 4));
    // Zoomed in: tiles of level 16 wearing level-18 imagery, as many as
    // the drapes' class holds.
    let fine: Vec<TileId> = (0..12u64)
        .flat_map(|x| (0..12u64).map(move |y| t(16, 21_000 + x, 14_000 + y)))
        .collect();
    let zoomed = NeedSet {
        drawn: fine.iter().map(|&id| (id, draped(id, 18))).collect(),
        ..NeedSet::default()
    };
    settle(&mut r, &zoomed, 600);
    // Out: other tiles, level 11, wanting level-13 imagery; the fine drapes
    // are drawn by nobody now.
    let wide: Vec<TileId> = (0..10u64)
        .flat_map(|x| (0..10u64).map(move |y| t(11, 600 + x, 400 + y)))
        .collect();
    let out = NeedSet {
        drawn: wide.iter().map(|&id| (id, draped(id, 13))).collect(),
        ..NeedSet::default()
    };
    let mut started = 0;
    let mut last = None;
    for _ in 0..600 {
        let d = r.step(&out);
        started += d.start.len();
        r.complete(&d);
        last = Some(d);
    }
    let d = last.unwrap();
    assert!(started > 0, "nothing loaded");
    assert_eq!(d.stats.lost + d.stats.bare, 0);
    assert_eq!(
        d.stats.deficit.imagery_under(),
        0,
        "tiles under their imagery need while undrawn drapes hold the pool"
    );
    for &id in &wide {
        assert!(
            r.b.resource(dr(id, 13))
                .is_some_and(|v| v.gpu > Bytes::ZERO),
            "{id:?}'s own drape not resident"
        );
    }
}

/// The headroom: a still view whose neighbourhood outgrows the budget
/// settles with the unprotected copies under `target − headroom` in every
/// class, and holds no copy left behind under the floor value.
#[test]
fn a_still_view_keeps_its_pools_under_the_headroom() {
    let mut r = Rig::new(config(Bytes::mib(48), 4));
    let v = View::at(0.3333, 0.6666, 14);
    let need = v.need(4);
    let mut d = settle(&mut r, &need, 800);
    for _ in 0..30 {
        d = r.step(&need);
        r.complete(&d);
    }
    let headroom = tuile_budget::spec::HEADROOM;
    for c in [Class::TerrainMesh, Class::Drape] {
        let free = d.stats.gpu[c].minus(d.stats.protected[c]);
        let under = d.stats.target[c].scaled(u64::from(1_000 - headroom), 1_000);
        assert!(free <= under, "{c:?}: {free:?} unprotected over {under:?}");
    }
    let floor = CERTAIN / 1_000 * u32::from(tuile_budget::spec::FLOOR_VALUE);
    let held = r.b.held_report();
    for h in held
        .lowest
        .iter()
        .filter(|h| h.state == tuile_budget::score::HeldState::Left)
    {
        assert!(
            h.terms.p >= floor,
            "{:?} held under the floor value",
            h.resource
        );
    }
}

/// After a zoom-out, the preloads left behind — the finer level the old
/// view anticipated — are released, room or not, once out of the need for
/// the thrash window (10 s: a turn and back finds them still): within a
/// bounded number of steps.
#[test]
fn the_fine_preloads_left_behind_by_a_zoom_out_are_released() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let (fx, fy) = (0.7083, 0.2917);
    let low = View::at(fx, fy, 13);
    settle(&mut r, &low.need(4), 600);
    let finer: Vec<TileId> = around(14, fx, fy, 4.0)
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    assert!(
        finer.iter().any(|t| gpu(&r, *t)),
        "level 14 never preloaded"
    );
    let high = View::at(fx, fy, 9);
    let need = high.need(4);
    for _ in 0..30 {
        let d = r.step(&need);
        r.complete(&d);
    }
    assert!(
        finer.iter().any(|t| gpu(&r, *t)),
        "released inside the thrash window"
    );
    // 10 s at 60 steps a second, and a few steps for the 64 looked at each.
    for _ in 0..640 {
        let d = r.step(&need);
        r.complete(&d);
    }
    let left = finer.iter().filter(|t| gpu(&r, **t)).count();
    assert_eq!(
        left, 0,
        "{left} level-14 preloads still held 11 s after the zoom-out"
    );
}

/// Every number of the score is the specification's: re-specified at run
/// time (the query `tuile://budget?…` carries), the next step reads it —
/// its statistics carry the parameters in force, and its order follows.
#[test]
fn a_re_specification_changes_the_next_steps_order() {
    use tuile_budget::Specification;
    let first_preload = |query: Option<&str>| {
        let mut c = config(Bytes::mib(512), 4);
        c.max_in_flight = tuile_budget::PerClass::splat(1);
        let mut r = Rig::new(c);
        if let Some(q) = query {
            r.b.specify(Specification::from_query(q).unwrap()).unwrap();
        }
        let v = View::at(0.45, 0.55, 12);
        let need = v.need(4);
        let drawn: BTreeSet<TileId> = v.drawn().into_iter().collect();
        for _ in 0..400 {
            let d = r.step(&need);
            if let Some(e) = d.scores.iter().find(|e| {
                e.side == Side::Preload
                    && matches!(e.resource, ResourceId::Mesh(t) if !drawn.contains(&t))
            }) {
                return (e.level, d.stats.score.params);
            }
            r.complete(&d);
        }
        panic!("no preload");
    };
    let (level_default, params) = first_preload(None);
    assert_eq!(params.a0_permille, 800);
    // Levels above the need worth nothing more than the floor: the first
    // preload is no longer the level above the need.
    let (level_flat, params) = first_preload(Some("spec=interactive&a0=10&a_desc=1000"));
    assert_eq!(params.a0_permille, 10);
    assert_eq!(params.descend_permille, 1000);
    assert_ne!(
        level_default, level_flat,
        "the order did not follow the specification"
    );
}

/// Laurent's stall (Tuile.app on main 9a5ee85e, 2026-11-23, twice): the
/// drapes' class at 97–99 % of its target, the picture relaxed, imagery
/// under its need and nothing loading or even queued. The level of detail
/// comes back only once every class is under its low watermark, and nothing
/// evicted between the watermarks: a relaxed budget whose class sits
/// between them stays relaxed for ever. Relaxed, the lowest scores go down
/// to the low watermark: the detail comes back, the needs load.
#[test]
fn a_relaxed_budget_between_its_watermarks_comes_back_to_full_detail() {
    use tuile_budget::Pressure;
    let mut c = config(Bytes::mib(64), 4);
    c.pinned_level = None;
    let mut r = Rig::new(c);
    // Zoomed in: fine drapes filling their class.
    let fine: Vec<TileId> = (0..9u64)
        .flat_map(|x| (0..9u64).map(move |y| t(14, 9_000 + x, 5_000 + y)))
        .collect();
    let zoomed = NeedSet {
        drawn: fine.iter().map(|&id| (id, draped(id, 16))).collect(),
        ..NeedSet::default()
    };
    let d = settle(&mut r, &zoomed, 600);
    let ratio = |d: &Decisions| {
        d.stats.gpu[Class::Drape].0 as f64 / d.stats.target[Class::Drape].0.max(1) as f64
    };
    eprintln!("zoomed in: {:.3} of the drapes' target", ratio(&d));
    // Out to a few tiles elsewhere, under a memory warning that relaxes
    // the picture, then normal again: the fine drapes, drawn by nobody,
    // still hold their class between its watermarks.
    let wide: Vec<TileId> = (0..3u64)
        .flat_map(|x| (0..3u64).map(move |y| t(12, 700 + x, 300 + y)))
        .collect();
    let out = NeedSet {
        drawn: wide.iter().map(|&id| (id, draped(id, 14))).collect(),
        ..NeedSet::default()
    };
    r.b.set_pressure(Pressure::Warning);
    for _ in 0..30 {
        let d = r.step(&zoomed);
        r.complete(&d);
    }
    r.b.set_pressure(Pressure::Normal);
    let d = r.step(&zoomed);
    r.complete(&d);
    let mut last = r.step(&out);
    eprintln!(
        "relaxed: {:.3} of the target, scale {}",
        ratio(&last),
        last.sse_scale_milli
    );
    assert!(
        last.sse_scale_milli > 1_000,
        "the warning relaxed nothing: the test proves nothing"
    );
    for _ in 0..600 {
        r.complete(&last);
        last = r.step(&out);
    }
    eprintln!(
        "after: {:.3} of the target, scale {}",
        ratio(&last),
        last.sse_scale_milli
    );
    assert_eq!(
        last.sse_scale_milli, 1_000,
        "still relaxed: the detail never came back"
    );
    assert_eq!(
        last.stats.deficit.imagery_under(),
        0,
        "imagery still under its need"
    );
}

/// Laurent's capture over Bordeaux (main, 2026-11-23): at rest, terrain
/// levels 17–21 holding thousands of tiles nobody drew or selected, the
/// decoded tier at its cap. Scripted: a dive to level 19, then out to
/// level 12 and still. Within the thrash window and a little more, the
/// fine levels left are gone from both tiers; what stays is the need, its
/// ring and the coarse stack. Prints the numbers.
#[test]
fn at_rest_the_fine_levels_left_behind_are_released_in_both_tiers() {
    let (fx, fy) = (0.4983, 0.6231);
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    for n in 13..=19 {
        let v = View::at(fx, fy, n);
        let need = v.need(4);
        for _ in 0..40 {
            let d = r.step(&need);
            r.complete(&d);
        }
    }
    let out = View::at(fx, fy, 12).need(4);
    let count = |r: &Rig| {
        let h = r.b.held_report();
        let mut by = [0u32; 25];
        for (l, n) in by.iter_mut().enumerate() {
            for b in 0..tuile_budget::score::DISTANCE_BANDS {
                *n += h.board.cell(l, b, Side::Held).count;
            }
        }
        (by[14..=19].iter().sum::<u32>(), by)
    };
    let mut d = r.step(&out);
    let (fine_before, _) = count(&r);
    let decoded_before = d.stats.decoded;
    // 11 s still.
    for _ in 0..660 {
        r.complete(&d);
        d = r.step(&out);
    }
    let (fine_after, _) = count(&r);
    eprintln!(
        "levels 14-19 held: {fine_before} -> {fine_after}; decoded {:?} -> {:?}",
        decoded_before, d.stats.decoded
    );
    assert!(
        fine_before > 0,
        "nothing fine held: the test proves nothing"
    );
    assert_eq!(fine_after, 0, "fine levels still held at rest");
    // The decoded copies of the fine levels went too.
    let fine_decoded: u32 = (14..=19)
        .map(|l| d.stats.levels.decoded_tiles[Class::TerrainMesh][l])
        .sum();
    assert_eq!(fine_decoded, 0, "fine decoded copies still held at rest");
}

/// The dynamic headroom: nothing at rest, the next second of uploads in
/// motion.
#[test]
fn the_headroom_follows_the_uploads() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let mut d = settle(&mut r, &View::at(0.3, 0.3, 12).need(4), 600);
    for _ in 0..900 {
        r.complete(&d);
        d = r.step(&View::at(0.3, 0.3, 12).need(4));
    }
    let rest = d.stats.score.headroom_bytes;
    assert!(rest < 256 << 10, "a headroom of {rest} B at rest");
    let mut most = 0;
    for i in 0..60 {
        let v = View::at(0.3 + f64::from(i) * 0.0005, 0.3, 12);
        d = r.step(&v.need(4));
        r.complete(&d);
        most = most.max(d.stats.score.headroom_bytes);
    }
    assert!(
        most > 4 * rest.max(1 << 16),
        "no headroom in motion: {most} B"
    );
    eprintln!("headroom: {rest} B at rest, {most} B in motion");
}

/// Every wait of the present need is classified by the meter
/// (`tuile_budget::waits`): a first load, or a reload of a copy evicted
/// less than 5 s or 30 s before, with the eviction's reason — what
/// explains a latency live.
#[test]
fn a_wait_is_classified_first_load_or_reload_after_an_eviction() {
    let mut c = config(Bytes::mib(64), 4);
    c.pinned_level = None;
    let run = |gap_s: u64| {
        let mut r = Rig::new(c.clone());
        let a: Vec<TileId> = (0..4u64).map(|x| t(10, 100 + x, 100)).collect();
        let b: Vec<TileId> = (0..4u64).map(|x| t(10, 900 + x, 900)).collect();
        let here = drawn(&a);
        let there = drawn(&b);
        let mut meter = tuile_budget::BudgetMeter::new(r.b.config());
        let mut n = 0u64;
        // A step of the rig, observed by a meter: the waits are the meter's.
        let mut step = |r: &mut Rig, need: &NeedSet, meter: &mut tuile_budget::BudgetMeter| {
            let (now, frame) = (r.now, r.frame);
            let d = r.step(need);
            let rec = tuile_budget::StepRecord {
                step: n,
                frame,
                now,
                views_changed: true,
                eyes: 1,
                need,
                events: &[],
                presence: &[],
                decisions: &d,
                ledger: r.b.ledger(),
                pressure: tuile_budget::Pressure::Normal,
                thermal: tuile_budget::Thermal::Nominal,
                decide_wall: None,
            };
            tuile_budget::BudgetObserver::step(meter, &rec);
            n += 1;
            r.complete(&d);
            d
        };
        let kinds = |m: &tuile_budget::BudgetMeter| -> [u64; 3] {
            let w = m.signals().waits;
            [0, 1, 2].map(|k| w.millis[k].iter().sum())
        };
        for _ in 0..30 {
            step(&mut r, &here, &mut meter);
        }
        assert_eq!(kinds(&meter), [4, 0, 0], "the first loads");
        // Away, and the budget cut: `a`'s meshes are evicted.
        r.b.retarget(Bytes::kib(64));
        let mut evicted = false;
        for _ in 0..30 {
            let d = step(&mut r, &there, &mut meter);
            evicted |= a.iter().any(|t| d.evict.contains(&ResourceId::Mesh(*t)));
        }
        assert!(evicted, "nothing of `a` evicted: the test proves nothing");
        r.b.retarget(Bytes::mib(64));
        // Still away for the gap, then back.
        for _ in 0..gap_s * 60 {
            step(&mut r, &there, &mut meter);
        }
        let before = kinds(&meter);
        for _ in 0..30 {
            step(&mut r, &here, &mut meter);
        }
        let after = kinds(&meter);
        (
            [
                after[0] - before[0],
                after[1] - before[1],
                after[2] - before[2],
            ],
            meter.signals().waits.reload_reasons,
        )
    };
    let (soon, reasons) = run(1);
    assert!(
        soon[1] > 0 && soon[0] == 0,
        "back within 5 s: reloads <5 s, {soon:?}"
    );
    assert!(
        reasons[..3].iter().sum::<u64>() > 0,
        "the reloads name their eviction's reason"
    );
    let (later, _) = run(12);
    assert!(
        later[2] > 0 && later[1] == 0,
        "back after 12 s: reloads <30 s, {later:?}"
    );
    let (long, _) = run(40);
    assert!(
        long[0] > 0 && long[1] + long[2] == 0,
        "back after 40 s: first loads again, {long:?}"
    );
}

/// The proactive sweep never takes what the view wants now or wanted within
/// the horizon (Laurent, 2026-11-24): a view swinging between east and west
/// every second under a budget its need and ring overfill. Before the law
/// (#289 at a7a4f720) every copy the sweep took was of a tile the need
/// listed that very step (335 of 335), and every one was loaded again. With
/// it the proactive sweep takes none; what the need holds over the target
/// still goes back within it (192 copies, `fit_target`): at 24 MiB this
/// need does not fit.
#[test]
fn the_sweep_never_takes_what_the_view_wants_within_the_horizon() {
    use tuile_budget::score::Reason;
    let mut r = Rig::new(config(Bytes::mib(24), 4));
    let keep = u64::from(
        tuile_budget::spec::Specification::interactive()
            .params
            .sweep_keep_poses,
    );
    let (fx, fy) = (0.6213, 0.3149);
    let east = View {
        reach: 3.0,
        look: Some((1.0, 0.0)),
        ..View::at(fx, fy, 13)
    };
    let west = View {
        look: Some((-1.0, 0.0)),
        ..east
    };
    let mut last: std::collections::BTreeMap<TileId, u64> = Default::default();
    let (mut swept, mut bad, mut reloaded) = (0, Vec::new(), 0);
    let mut gone: BTreeSet<ResourceId> = BTreeSet::new();
    let mut over = 0;
    for k in 0..20 {
        let v = if k % 2 == 0 { east } else { west };
        let need = v.need(4);
        let listed: BTreeSet<TileId> = need
            .drawn
            .iter()
            .chain(&need.blockers)
            .chain(&need.ancestors)
            .chain(&need.ring)
            .map(|x| x.0)
            .collect();
        for _ in 0..60 {
            let (now, pose) = (r.now, r.frame);
            let d = r.step(&need);
            // Over the low watermark of its target: the sweep's to trim.
            let l = r.b.ledger();
            over += usize::from(
                Class::ALL
                    .iter()
                    .any(|c| l.gpu[*c] > d.stats.target[*c].scaled(9, 10)),
            );
            for e in d.scores.iter().filter(|e| e.side == Side::Evict) {
                if e.reason != Reason::Sweep {
                    continue;
                }
                swept += 1;
                let tile = e.resource.tile();
                let recent = last.get(&tile).is_some_and(|p| pose - p < keep);
                if listed.contains(&tile) || recent {
                    bad.push((now, e.resource));
                    gone.insert(e.resource);
                }
            }
            for s in &d.start {
                reloaded += usize::from(gone.remove(&s.resource));
            }
            for t in &listed {
                last.insert(*t, pose);
            }
            r.complete(&d);
        }
    }
    assert!(
        bad.is_empty(),
        "the sweep took {} copies the view wanted now or within {keep} poses (of {swept} swept), \
         {reloaded} loaded again; first {:?}",
        bad.len(),
        bad.first()
    );
    // Not vacuous: the need and its ring held a class over its low
    // watermark — what the sweep took before the law, it now keeps.
    assert!(
        over > 100,
        "{over} steps over a low watermark: the sweep had nothing to do"
    );
}

// The views ahead (#282's `Need.ahead`): a prediction is an input of the
// need, read by the one score — `P` is the larger of the place's and the
// prediction's (its probability × the time term, `H² / (H + slack)²`).

/// `n`, named by a prediction: used in `in_ms` milliseconds with a
/// probability of `permille` thousandths.
fn predicted(n: Need, in_ms: u64, permille: u16) -> Need {
    n.with_ahead(tuile_budget::AheadUse {
        next_use_in: tuile_budget::Micros::from_millis(in_ms),
        probability: permille,
    })
}

/// `need` with its ring cleared: what the view draws and its chain.
fn without_ring(need: &NeedSet) -> NeedSet {
    NeedSet {
        ring: Vec::new(),
        ..need.clone()
    }
}

/// The ring tile the place weighs least: one level under the need, the
/// farthest from the view.
fn least_placed(need: &NeedSet) -> TileId {
    need.ring
        .iter()
        .filter(|(id, _)| id.terrain_coord().0 == 14)
        .max_by(|a, b| {
            let d = |n: &Need| n.proximity.map_or(0.0, |p| p.tiles_away);
            d(&a.1).total_cmp(&d(&b.1)).then(a.0.cmp(&b.0))
        })
        .map(|(id, _)| *id)
        .expect("a ring at level 14")
}

/// A prediction raises the preload of the tile it names: with the view and
/// its chain resident, the ring's first step starts as many preloads as
/// the slots take; the ring tile the place weighs least is not among them,
/// and is once a prediction says it is used now. Red with the prediction
/// not read.
#[test]
fn a_prediction_raises_the_preload_of_the_tile_it_names() {
    let v = View::at(0.5531, 0.4412, 13);
    let need = v.need(4);
    let x = least_placed(&need);
    let first = |need: &NeedSet| {
        let mut r = Rig::new(config(Bytes::mib(512), 4));
        settle(&mut r, &without_ring(need), 600);
        r.step(need)
    };
    let plain = first(&need);
    // Not vacuous: the slots take a few of the ring's tiles only.
    assert!(
        plain.start.len() < need.ring.len() / 4,
        "{} starts for {} ring tiles: no contention",
        plain.start.len(),
        need.ring.len()
    );
    assert!(
        !plain
            .start
            .iter()
            .any(|s| s.resource == ResourceId::Mesh(x)),
        "{x:?} started without a prediction: the test proves nothing"
    );
    let mut named = need.clone();
    for (id, n) in &mut named.ring {
        if *id == x {
            *n = predicted(n.clone(), 0, 1_000);
        }
    }
    let d = first(&named);
    assert!(
        d.start.iter().any(|s| s.resource == ResourceId::Mesh(x)),
        "the predicted {x:?} waited behind the place's preloads"
    );
}

/// A tile predicted soon and likely is released after one predicted late
/// and unlikely, at the same place: under a cut budget the unlikely one
/// goes first; swap the predictions and the order swaps. Red with the
/// prediction not read (the two tie, and the id decides).
#[test]
fn a_likely_soon_tile_is_released_after_an_unlikely_one() {
    let v = View::at(0.3713, 0.6178, 12);
    let need = v.need(4);
    // Two ring tiles of one level, placed alike.
    let mut pair: Vec<TileId> = need
        .ring
        .iter()
        .filter(|(id, _)| id.terrain_coord().0 == 12)
        .map(|(id, _)| *id)
        .collect();
    pair.truncate(2);
    let (a, b) = (pair[0], pair[1]);
    let order = |likely: TileId, unlikely: TileId| -> (Option<usize>, Option<usize>) {
        let mut need = need.clone();
        for (id, n) in &mut need.ring {
            let placed = n.clone().with_proximity(12, 3.0);
            if *id == likely {
                *n = predicted(placed, 100, 900);
            } else if *id == unlikely {
                *n = predicted(placed, 3_000, 100);
            }
        }
        let mut r = Rig::new(config(Bytes::mib(512), 4));
        let held = settle(&mut r, &need, 800).stats.gpu[Class::TerrainMesh];
        assert!(
            gpu(&r, likely) && gpu(&r, unlikely),
            "the pair is not resident"
        );
        r.b.retarget(Bytes(held.0 * 400 / 1_000));
        let mut evicted: Vec<ResourceId> = Vec::new();
        for _ in 0..60 {
            let d = r.step(&need);
            evicted.extend(&d.evict);
            r.complete(&d);
        }
        let at = |t: TileId| evicted.iter().position(|e| *e == ResourceId::Mesh(t));
        (at(likely), at(unlikely))
    };
    for (likely, unlikely) in [(a, b), (b, a)] {
        let (l, u) = order(likely, unlikely);
        let Some(u) = u else {
            panic!("the unlikely {unlikely:?} was kept: the cut proves nothing");
        };
        assert!(
            l.is_none_or(|l| l > u),
            "the likely {likely:?} went at {l:?}, before the unlikely {unlikely:?} at {u}"
        );
    }
}

/// Absolute protections stay outside any prediction: every ring tile
/// predicted certain and now, a budget cut to a twentieth evicts preloads
/// only — never a drawn tile, its chain or the floor — and opens no hole.
#[test]
fn a_prediction_never_releases_what_is_protected() {
    let v = View::at(0.6213, 0.3149, 13);
    let mut need = v.need(4);
    for (_, n) in &mut need.ring {
        *n = predicted(n.clone(), 0, 1_000);
    }
    let mut r = Rig::new(config(Bytes::mib(256), 4));
    settle(&mut r, &need, 800);
    let protected: BTreeSet<TileId> = need
        .drawn
        .iter()
        .chain(&need.ancestors)
        .map(|(id, _)| *id)
        .collect();
    r.b.retarget(Bytes::mib(12));
    let mut evicted = 0;
    for _ in 0..30 {
        let d = r.step(&need);
        assert_eq!(d.stats.lost, 0, "a hole on a prediction");
        for e in &d.evict {
            assert!(
                !protected.contains(&e.tile()),
                "{e:?}, drawn, of the chain or the floor, released on a prediction"
            );
        }
        evicted += d.evict.len();
        r.complete(&d);
    }
    assert!(
        evicted > 0,
        "the cut evicted nothing: the test proves nothing"
    );
}

/// A prediction never delays a current need: the drape slots full of a
/// predicted ring's drapes (preloads of the views ahead), a drawn tile's
/// drape two levels finer than it wears (wanted at once) starts on the step
/// it is wanted, in the slot of a predicted preload cancelled for it. Red
/// with the present need's drape waiting for a slot (only a hole preempted
/// a preload), and with a predicted tile's drapes not preloaded.
#[test]
fn a_drawn_tiles_drape_takes_the_slot_of_a_predicted_preload() {
    let v = View::at(0.4411, 0.5623, 13);
    let base = v.need(4);
    let drape_at = |need: &NeedSet, level: u32| NeedSet {
        drawn: need
            .drawn
            .iter()
            .map(|(id, _)| (*id, draped(*id, level)))
            .collect(),
        ..need.clone()
    };
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    settle(&mut r, &without_ring(&drape_at(&base, 11)), 600);
    // The ring, draped and predicted: its drapes take the drape slots.
    let mut need = drape_at(&base, 11);
    for (id, n) in &mut need.ring {
        *n = predicted(draped(*id, id.terrain_coord().0), 500, 800);
        if let Some(p) = base
            .ring
            .iter()
            .find(|(t, _)| t == id)
            .and_then(|(_, b)| b.proximity)
        {
            *n = n.clone().with_proximity(p.need_level, p.tiles_away);
        }
    }
    let mut ring_drapes = 0;
    for _ in 0..3 {
        let d = r.step(&need);
        ring_drapes += d
            .start
            .iter()
            .filter(|s| matches!(s.resource, ResourceId::Drape { .. }))
            .count();
        r.upload(&d);
    }
    assert!(
        ring_drapes > 0,
        "no predicted drape in flight: the test proves nothing"
    );
    let full = r.step(&need);
    assert!(
        !full
            .start
            .iter()
            .any(|s| matches!(s.resource, ResourceId::Drape { .. })),
        "the drape slots are not full: the test proves nothing"
    );
    // The view wants its drapes two levels finer: the present need.
    let finer = drape_at(&need, 13);
    let d = r.step(&finer);
    let (id, _) = finer.drawn[0];
    let wanted: BTreeSet<ResourceId> = finer.drawn.iter().map(|(t, _)| dr(*t, 13)).collect();
    assert!(
        d.start.iter().any(|s| wanted.contains(&s.resource)),
        "no drawn tile's drape started ({id:?} and the others wait behind predicted preloads)"
    );
    assert!(
        d.cancel
            .iter()
            .any(|(r, _)| need.ring.iter().any(|(t, _)| *t == r.tile())),
        "no predicted preload gave its slot"
    );
}

/// A prediction below what the place gives changes nothing: every ring
/// tile predicted unlikely and far ahead, the same view stepped with and
/// without it decides the same, step for step (the decisions' digests).
/// The runs without views ahead are the goldens' (the reference, the
/// pattern goldens, the decision digests), unchanged.
#[test]
fn a_prediction_under_the_place_changes_nothing() {
    let v = View::at(0.5531, 0.4412, 13);
    let plain = v.need(4);
    let mut named = plain.clone();
    for (_, n) in &mut named.ring {
        *n = predicted(n.clone(), 60_000, 1);
    }
    let run = |need: &NeedSet| -> Vec<u64> {
        let mut r = Rig::new(config(Bytes::mib(512), 4));
        (0..150)
            .map(|i| {
                let d = r.step(need);
                if i % 3 == 2 {
                    r.load_where(|_| true);
                }
                r.upload(&d);
                d.digest()
            })
            .collect()
    };
    assert_eq!(run(&plain), run(&named));
}

/// A predicted tile goes to the GPU whole: its mesh loaded and its drapes
/// not, the mesh waits decoded; its drapes in, both are delivered — so the
/// view it was predicted for never draws its geometry under a coarser
/// drape. Red with the mesh delivered alone.
#[test]
fn a_predicted_tile_is_delivered_with_its_drapes() {
    let v = View::at(0.2719, 0.6331, 12);
    let base = v.need(4);
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    settle(&mut r, &without_ring(&base), 600);
    let mut need = base.clone();
    for (id, n) in &mut need.ring {
        let p = n.proximity;
        *n = predicted(draped(*id, id.terrain_coord().0), 200, 900);
        if let Some(p) = p {
            *n = n.clone().with_proximity(p.need_level, p.tiles_away);
        }
    }
    let ring: BTreeSet<TileId> = need.ring.iter().map(|(t, _)| *t).collect();
    let mesh_of_ring = |r: &ResourceId| matches!(r, ResourceId::Mesh(t) if ring.contains(t));
    // Only the meshes arrive.
    let mut alone = 0;
    for _ in 0..30 {
        let d = r.step(&need);
        alone += d.deliver.iter().filter(|(x, _)| mesh_of_ring(x)).count();
        r.load_where(|x| matches!(x, ResourceId::Mesh(_)));
        r.upload(&d);
    }
    assert_eq!(
        alone, 0,
        "{alone} predicted meshes delivered without their drapes"
    );
    // The drapes arrive: the meshes go with them.
    let mut whole = 0;
    for _ in 0..30 {
        let d = r.step(&need);
        whole += d.deliver.iter().filter(|(x, _)| mesh_of_ring(x)).count();
        r.load_where(|_| true);
        r.upload(&d);
    }
    assert!(
        whole > 0,
        "no predicted mesh delivered once its drapes were in"
    );
}
