// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The observability contract (spec §16, test plan §2.8): every signal of
//! the `BudgetMeter` moves when its behaviour happens and stays still when
//! it does not. Each test drives the behaviour and its control — through the
//! budget where the budget can be made to do it, through a fabricated
//! record where the budget forbids it (a bare item, a duplicate in flight:
//! the signal is the alarm for what must never happen).

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use budget_common::*;
use tuile_budget::{
    check, BudgetConfig, BudgetMeter, BudgetObserver, BudgetSignals, Bytes, Class, Decisions,
    EyeCoverage, FrameDeadline, FrameRecord, LayerId, Micros, NeedSet, Pressure, ResourceId,
    StepRecord, Thermal, Violations,
};

const RED: &str = "the signal did not move";

/// A rig whose every step is shown to a meter, as a host would.
struct Metered {
    r: Rig,
    meter: BudgetMeter,
    step: u64,
    still: bool,
    pressure: Pressure,
    thermal: Thermal,
    wall: Micros,
}

impl Metered {
    fn new(cfg: BudgetConfig) -> Self {
        Metered {
            meter: BudgetMeter::new(&cfg),
            r: Rig::new(cfg),
            step: 0,
            still: true,
            pressure: Pressure::Normal,
            thermal: Thermal::Nominal,
            wall: Micros(100),
        }
    }

    fn interactive() -> Self {
        Metered::new(Rig::interactive_config())
    }

    fn step(&mut self, need: &NeedSet) -> Decisions {
        let now = self.r.now;
        let frame = self.r.frame;
        let d = self.r.step(need);
        let rec = StepRecord {
            step: self.step,
            frame,
            now,
            views_changed: !self.still,
            eyes: 1,
            need,
            events: &[],
            presence: &[],
            decisions: &d,
            ledger: self.r.b.ledger(),
            pressure: self.pressure,
            thermal: self.thermal,
            decide_wall: Some(self.wall),
        };
        self.meter.step(&rec);
        self.step += 1;
        d
    }

    fn settle(&mut self, need: &NeedSet, max: usize) -> Decisions {
        for (r, g) in std::mem::take(&mut self.r.pending) {
            self.r.b.on_loaded(r, g, est(r));
        }
        let mut last = self.step(need);
        for _ in 0..max {
            if last.start.is_empty() && last.deliver.is_empty() && last.upload_plan.items.is_empty()
            {
                break;
            }
            self.r.complete(&last);
            last = self.step(need);
        }
        last
    }

    fn idle(&mut self, need: &NeedSet, d: Micros) {
        let end = self.r.now.plus(d);
        while self.r.now < end {
            self.step(need);
        }
    }

    fn s(&self) -> BudgetSignals {
        self.meter.signals()
    }
}

/// A record of fabricated decisions, for what the budget forbids.
fn fabricated(meter: &mut BudgetMeter, need: &NeedSet, d: &Decisions, step: u64) {
    meter.step(&StepRecord {
        step,
        frame: step,
        now: Micros(step * 16_667),
        views_changed: false,
        eyes: 1,
        need,
        events: &[],
        presence: &[],
        decisions: d,
        ledger: Default::default(),
        pressure: Pressure::Normal,
        thermal: Thermal::Nominal,
        decide_wall: None,
    });
}

fn floored() -> BudgetConfig {
    let mut c = Rig::interactive_config();
    c.pinned_level = Some(0);
    c
}

/// I1: `lost_tiles` counts a wanted tile with no cover once the floor is
/// in, and `lost_tile_steps_total` keeps it.
#[test]
fn the_lost_tiles_signal_counts_a_wanted_tile_with_no_cover() {
    let mut m = Metered::new(floored());
    let root = t(0, 0, 0);
    let floor = drawn_draped(&[root], 0);
    m.settle(&floor, 20);
    // A 3D Tiles tile of another tree: no ancestor can stand for it.
    let other = tuile_budget::TileId(u64::MAX - 7);
    let mut need = floor.clone();
    need.drawn
        .push((other, tuile_budget::Need::new(5, Class::Tiles3dMesh)));
    let d = m.step(&need);
    assert!(d.stats.floor_in, "the floor is in");
    assert_eq!(m.s().lost_tiles, 1, "{RED}");
    assert_eq!(m.s().lost_tile_steps_total, 1);
    m.settle(&need, 20);
    assert_eq!(m.s().lost_tiles, 0, "drawable now");
    let mut control = Metered::new(floored());
    control.settle(&floor, 20);
    assert_eq!(
        control.s().lost_tile_steps_total,
        0,
        "no lost tile, no count"
    );
}

/// I1 (D-A1): `bare_items` counts an item drawn without its drapes. The
/// budget never draws one; the signal is the alarm, fed a record that does.
#[test]
fn the_bare_items_signal_counts_an_item_without_its_drapes() {
    let mut meter = BudgetMeter::new(&floored());
    let need = NeedSet::default();
    let mut d = Decisions::default();
    fabricated(&mut meter, &need, &d, 0);
    assert_eq!(meter.signals().bare_item_steps_total, 0);
    d.stats.bare = 2;
    fabricated(&mut meter, &need, &d, 1);
    assert_eq!(meter.signals().bare_items, 2, "{RED}");
    assert_eq!(meter.signals().bare_item_steps_total, 2);
    assert_eq!(
        meter.signals().invariant_violations_total[0],
        1,
        "I1 counted"
    );
}

/// P1.1: `floor_in` rises once the floor's mesh and drape are both
/// drawable, and `floor_seconds` holds when.
#[test]
fn the_floor_signal_rises_once_mesh_and_drape_are_drawable() {
    let mut m = Metered::new(floored());
    let root = t(0, 0, 0);
    let need = drawn_draped(&[root], 0);
    m.step(&need);
    m.r.load_where(|x| x == ResourceId::Mesh(root));
    for _ in 0..3 {
        let d = m.step(&need);
        m.r.upload(&d);
    }
    assert!(!m.s().floor_in, "the mesh alone is not the floor");
    assert_eq!(m.s().floor_micros, None);
    m.settle(&need, 20);
    assert!(m.s().floor_in, "{RED}");
    assert!(m.s().floor_micros.is_some_and(|f| f > 0));
}

/// I2: `bytes{class,state}` splits drawable from in transit.
#[test]
fn the_bytes_signal_splits_drawable_from_in_transit() {
    let mut m = Metered::interactive();
    let a = t(3, 1, 1);
    let need = drawn(&[a]);
    let d = m.step(&need);
    m.r.load(&d);
    let d = m.step(&need);
    assert!(!d.deliver.is_empty());
    let mesh = m.s().bytes[Class::TerrainMesh];
    assert_eq!(mesh, [0, MESH.0], "in transit, not drawable: {RED}");
    m.r.upload(&d);
    m.step(&need);
    assert_eq!(m.s().bytes[Class::TerrainMesh], [MESH.0, 0], "drawable");
    assert_eq!(m.s().bytes[Class::Drape], [0, 0], "no drape asked for");
}

/// I2, P3.2: `over_target_seconds_total` runs only while a class is over
/// its target.
#[test]
fn the_over_target_seconds_run_only_while_over() {
    let mut c = Rig::interactive_config();
    c.target[Class::TerrainMesh] = Bytes(MESH.0 * 2);
    let mut m = Metered::new(c);
    let tiles: Vec<_> = (0..4).map(|x| t(4, x, 0)).collect();
    m.settle(&drawn(&tiles), 30);
    let before = m.s().over_target_micros_total[Class::TerrainMesh];
    m.idle(&drawn(&tiles), Micros::from_millis(500));
    let over = m.s().over_target_micros_total[Class::TerrainMesh] - before;
    assert!(over >= 450_000, "{RED}: {over} µs over target in 500 ms");
    let mut control = Metered::interactive();
    control.settle(&drawn(&tiles), 30);
    control.idle(&drawn(&tiles), Micros::from_millis(500));
    assert_eq!(control.s().over_target_micros_total[Class::TerrainMesh], 0);
}

/// I3: `converged_seconds` measures from the views going still, once per
/// still interval; `unconverged_seconds` grows while still and not there.
#[test]
fn the_converged_seconds_measure_from_the_stop() {
    let mut m = Metered::interactive();
    let a = t(3, 2, 2);
    m.still = false;
    m.step(&drawn(&[a]));
    m.still = true;
    for _ in 0..3 {
        m.step(&drawn(&[a]));
    }
    assert!(m.s().unconverged_micros > 0, "still and not converged");
    m.settle(&drawn(&[a]), 20);
    m.idle(&drawn(&[a]), Micros::from_millis(200));
    let s = m.s();
    assert!(s.converged, "{RED}");
    assert_eq!(s.converged_micros.count, 1, "once per still interval");
    assert!(s.converged_micros.sum > 0);
    assert_eq!(s.unconverged_micros, 0);
    let mut moving = Metered::interactive();
    moving.still = false;
    moving.settle(&drawn(&[a]), 20);
    assert_eq!(
        moving.s().converged_micros.count,
        0,
        "never still, never timed"
    );
}

/// I4: `flaps_total{kind}` counts a tile's second level change within ten
/// seconds while the views are still.
#[test]
fn the_flaps_signal_counts_a_second_change_within_ten_seconds() {
    let a = t(3, 2, 2);
    let mut m = Metered::interactive();
    for level in [3, 6, 3] {
        m.settle(&drawn_draped(&[a], level), 20);
    }
    assert_eq!(m.s().flaps_total, [0, 1], "{RED}");
    let mut once = Metered::interactive();
    for level in [3, 6] {
        once.settle(&drawn_draped(&[a], level), 20);
    }
    assert_eq!(once.s().flaps_total, [0, 0], "one change is no flap");
    let mut slow = Metered::interactive();
    slow.settle(&drawn_draped(&[a], 3), 20);
    slow.settle(&drawn_draped(&[a], 6), 20);
    slow.idle(&drawn_draped(&[a], 6), Micros::from_secs(11));
    slow.settle(&drawn_draped(&[a], 3), 20);
    assert_eq!(
        slow.s().flaps_total,
        [0, 0],
        "changes 11 s apart are no flap"
    );
}

/// I5: `cancels_total{reason}` names the grace and the timeout.
#[test]
fn the_cancels_signal_names_the_grace_and_the_timeout() {
    let a = t(3, 1, 1);
    let b = t(3, 6, 6);
    let mut m = Metered::interactive();
    m.step(&drawn(&[a]));
    m.idle(&drawn(&[b]), Micros::from_secs(1));
    assert_eq!(m.s().cancels_total[0], 1, "{RED}: grace");
    let mut m = Metered::interactive();
    m.step(&drawn(&[a]));
    m.r.pending.clear();
    m.idle(&drawn(&[a]), Micros::from_secs(5));
    assert!(m.s().cancels_total[1] >= 1, "{RED}: timeout");
    let mut still = Metered::interactive();
    still.settle(&drawn(&[a]), 20);
    still.idle(&drawn(&[a]), Micros::from_secs(5));
    assert_eq!(still.s().cancels_total, [0, 0, 0]);
}

/// P2.2: `wasted_starts_total{class}` counts a start the view left before
/// it was delivered.
#[test]
fn the_wasted_starts_signal_counts_a_start_the_view_left() {
    let (a, b) = (t(3, 1, 1), t(3, 6, 6));
    let mut m = Metered::interactive();
    m.step(&drawn(&[a]));
    m.idle(&drawn(&[b]), Micros::from_secs(1));
    assert_eq!(m.s().wasted_starts_total[Class::TerrainMesh], 1, "{RED}");
    let mut kept = Metered::interactive();
    kept.settle(&drawn(&[a]), 20);
    kept.idle(&drawn(&[a]), Micros::from_secs(1));
    assert_eq!(kept.s().wasted_starts_total[Class::TerrainMesh], 0);
}

/// I14, I15: `starts_total{class,source}` splits local reads from network
/// loads.
#[test]
fn the_starts_signal_splits_local_from_network() {
    let (a, b) = (t(3, 1, 1), t(3, 6, 6));
    let mut m = Metered::interactive();
    m.r.store.0.insert(ResourceId::Mesh(a));
    m.step(&drawn(&[a, b]));
    assert_eq!(m.s().starts_total[Class::TerrainMesh], [1, 1], "{RED}");
    assert_eq!(m.s().starts_total[Class::Drape], [0, 0]);
}

/// I15: `local_waits_total` counts a stored resource left behind a network
/// load. The budget's separate quotas forbid it; the signal is the alarm.
#[test]
fn the_local_waits_signal_counts_a_stored_tile_left_behind_the_network() {
    let mut meter = BudgetMeter::new(&Rig::interactive_config());
    let need = NeedSet::default();
    let mut d = Decisions::default();
    fabricated(&mut meter, &need, &d, 0);
    assert_eq!(meter.signals().local_waits_total, 0);
    d.stats.local_waits = 1;
    fabricated(&mut meter, &need, &d, 1);
    assert_eq!(meter.signals().local_waits_total, 1, "{RED}");
    // And the budget's own count stays 0 with a full local quota.
    let mut c = Rig::interactive_config();
    c.max_local_reads = 1;
    let mut m = Metered::new(c);
    let tiles: Vec<_> = (0..4).map(|x| t(4, x, 0)).collect();
    for x in &tiles {
        m.r.store.0.insert(ResourceId::Mesh(*x));
    }
    m.step(&drawn(&tiles));
    assert_eq!(m.s().local_waits_total, 0);
}

/// I14: `thrash_total` counts a network refetch within ten seconds of an
/// eviction; `rereads_total` a local one.
#[test]
fn the_thrash_signal_counts_a_refetch_within_ten_seconds() {
    let a = t(3, 1, 1);
    let others: Vec<_> = (0..3).map(|x| t(4, x, 9)).collect();
    let run = |stored: bool| {
        let mut c = Rig::interactive_config();
        c.target[Class::TerrainMesh] = Bytes(MESH.0 * 2);
        c.overflow[Class::TerrainMesh] = Bytes::ZERO;
        c.decoded_target = Bytes::ZERO;
        let mut m = Metered::new(c);
        m.settle(&drawn(&[a]), 20);
        if stored {
            m.r.store.0.insert(ResourceId::Mesh(a));
        }
        m.settle(&drawn(&others), 40);
        m.settle(&drawn(&[a]), 40);
        m.s()
    };
    let net = run(false);
    assert!(net.thrash_total >= 1, "{RED}: thrash");
    assert_eq!(net.rereads_total, 0);
    let local = run(true);
    assert_eq!(local.thrash_total, 0);
    assert!(local.rereads_total >= 1, "{RED}: rereads");
}

/// I11, P3.2: `evictions_total{class,reason}` names the sweep, the
/// pressure and the admission.
#[test]
fn the_evictions_signal_names_its_reason() {
    let tiles: Vec<_> = (0..6).map(|x| t(4, x, 0)).collect();
    let more: Vec<_> = (0..6).map(|x| t(4, x, 9)).collect();
    let mut c = Rig::interactive_config();
    c.target[Class::TerrainMesh] = Bytes(MESH.0 * 4);
    c.overflow[Class::TerrainMesh] = Bytes::ZERO;
    let mut m = Metered::new(c.clone());
    m.settle(&drawn(&tiles), 30);
    m.settle(&drawn(&more), 30);
    let e = m.s().evictions_total[Class::TerrainMesh];
    assert!(e[0] + e[2] > 0, "{RED}: sweep or admission");
    let mut p = Metered::new(c);
    p.settle(&drawn(&tiles), 30);
    p.r.b.set_pressure(Pressure::Warning);
    p.pressure = Pressure::Warning;
    p.settle(&drawn(&tiles[..1]), 30);
    assert!(
        p.s().evictions_total[Class::TerrainMesh][1] > 0,
        "{RED}: pressure"
    );
    let mut quiet = Metered::interactive();
    quiet.settle(&drawn(&tiles), 30);
    assert_eq!(quiet.s().evictions_total[Class::TerrainMesh], [0; 4]);
}

/// I9: `swaps_total{kind}` counts refinements and coarsenings, and
/// `swap_gaps_total` stays still — and moves only for ground covered one
/// step and not the next.
#[test]
fn the_swaps_signal_counts_refines_and_coarsenings_and_never_a_gap() {
    let p = t(2, 1, 1);
    let kids = children(p);
    let mut m = Metered::interactive();
    m.settle(&drawn(&[p]), 20);
    let mut refined = drawn(&kids);
    refined.ancestors.push((p, need(p)));
    m.settle(&refined, 20);
    assert_eq!(m.s().swaps_total[0], 1, "{RED}: refine");
    m.settle(&drawn(&[p]), 20);
    assert!(m.s().swaps_total[1] >= 1, "{RED}: coarsen");
    assert_eq!(m.s().swap_gaps_total, 0);
    // A gap, fabricated: covered, then not.
    let mut meter = BudgetMeter::new(&Rig::interactive_config());
    let a = t(3, 2, 2);
    let need = drawn(&[a]);
    let mut d = Decisions::default();
    d.stats.floor_in = true;
    d.draw.items.push(tuile_budget::DrawItem {
        mesh: ResourceId::Mesh(a),
        role: tuile_budget::Role::Exact,
        drapes: Vec::new(),
        serves: vec![a],
    });
    fabricated(&mut meter, &need, &d, 0);
    d.draw.items.clear();
    fabricated(&mut meter, &need, &d, 1);
    assert_eq!(meter.signals().swap_gaps_total, 1, "{RED}: gap");
}

/// I10: `underdraped_tiles` and `imagery_over_terrain{gap}` follow the
/// drapes worn.
#[test]
fn the_underdraped_and_gap_signals_follow_the_drapes_worn() {
    let a = t(3, 2, 2);
    let mut m = Metered::interactive();
    m.settle(&drawn_draped(&[a], 3), 20);
    let s = m.s();
    assert_eq!(s.underdraped_tiles, 0);
    assert_eq!(
        s.imagery_over_terrain[8], 1,
        "the drape at the terrain's level"
    );
    m.step(&drawn_draped(&[a], 6));
    assert_eq!(m.s().underdraped_tiles, 1, "{RED}: three levels short");
    m.settle(&drawn_draped(&[a], 6), 20);
    assert_eq!(m.s().underdraped_tiles, 0);
    assert_eq!(
        m.s().imagery_over_terrain[11],
        1,
        "three levels over terrain"
    );
}

/// P2, P4.1: `fallback_depth_max` holds the deepest climb until published.
#[test]
fn the_fallback_depth_holds_the_deepest_climb_until_published() {
    let root = t(0, 0, 0);
    let mut m = Metered::interactive();
    m.settle(&drawn(&[root]), 20);
    let kid = children(root)[0];
    let grandkid = children(kid)[0];
    let deep = children(grandkid)[0];
    let mut need = drawn(&[deep]);
    // The chain the traversal walked down, as preload.
    for a in [root, kid, grandkid] {
        need.ancestors.push((a, need_of(a)));
    }
    m.step(&need);
    assert_eq!(m.s().fallback_depth_max, 3, "{RED}");
    m.settle(&drawn(&[root]), 20);
    assert_eq!(m.s().fallback_depth_max, 3, "held until published");
    let published = m.meter.publish();
    assert_eq!(published.fallback_depth_max, 3);
    assert_eq!(m.s().fallback_depth_max, 0, "reset by the publish");
}

fn need_of(id: tuile_budget::TileId) -> tuile_budget::Need {
    need(id)
}

/// I8: `pressure_level`, `thermal_level` and `target_bytes` follow the
/// platform.
#[test]
fn the_pressure_and_thermal_levels_follow_the_platform() {
    let a = t(3, 2, 2);
    let mut m = Metered::interactive();
    m.step(&drawn(&[a]));
    let before = m.s().target_bytes[Class::Drape];
    assert_eq!((m.s().pressure_level, m.s().thermal_level), (0, 0));
    m.r.b.set_pressure(Pressure::Warning);
    m.pressure = Pressure::Warning;
    m.r.b.set_thermal(Thermal::Serious);
    m.thermal = Thermal::Serious;
    m.step(&drawn(&[a]));
    let s = m.s();
    assert_eq!((s.pressure_level, s.thermal_level), (1, 2), "{RED}");
    assert!(s.target_bytes[Class::Drape] < before, "the target shrank");
}

/// P6.4: `decide_work` follows what the step visits, and its total adds
/// up.
#[test]
fn the_decide_work_signal_follows_the_changes() {
    let small = drawn(&[t(3, 2, 2)]);
    let big = drawn(&(0..32).map(|x| t(5, x, 3)).collect::<Vec<_>>());
    let mut m = Metered::interactive();
    m.step(&small);
    let one = m.s().decide_work;
    m.step(&big);
    let many = m.s().decide_work;
    assert!(many > one, "{RED}: {one} → {many}");
    assert_eq!(m.s().decide_work_total, u64::from(one) + u64::from(many));
}

/// P6.4: `decide_peak_seconds` holds the worst step until published.
#[test]
fn the_decide_peak_holds_the_worst_step_until_published() {
    let a = drawn(&[t(3, 2, 2)]);
    let mut m = Metered::interactive();
    for us in [100, 500, 200] {
        m.wall = Micros(us);
        m.step(&a);
    }
    assert_eq!(m.s().decide_peak_micros, 500, "{RED}");
    assert_eq!(m.s().decide_micros.count, 3);
    assert_eq!(m.meter.publish().decide_peak_micros, 500);
    assert_eq!(m.s().decide_peak_micros, 0);
}

fn frame(
    meter: &mut BudgetMeter,
    uploaded: &[(ResourceId, Bytes)],
    deferred: u32,
    eyes: &[EyeCoverage],
) {
    meter.frame(&FrameRecord {
        frame: 0,
        generation: 0,
        deadline: FrameDeadline {
            bytes: Bytes(250),
            time: Micros(1_000),
        },
        uploaded,
        deferred,
        upload_wall: Micros(300),
        eyes,
    });
}

/// I18: `deadline_deferrals_total` counts what a frame deferred, and
/// `frame_upload_bytes` what it uploaded; a frame over its deadline (more
/// than one item) is an I18 violation.
#[test]
fn the_deferrals_signal_counts_a_deadline_overrun() {
    let mut meter = BudgetMeter::new(&Rig::interactive_config());
    let (a, b) = (ResourceId::Mesh(t(3, 0, 0)), ResourceId::Mesh(t(3, 1, 0)));
    frame(&mut meter, &[(a, Bytes(100))], 0, &[]);
    assert_eq!(meter.signals().deadline_deferrals_total, 0);
    frame(&mut meter, &[(a, Bytes(100)), (b, Bytes(100))], 3, &[]);
    let s = meter.signals();
    assert_eq!(s.deadline_deferrals_total, 3, "{RED}");
    assert_eq!(s.frame_upload_bytes.count, 2);
    assert_eq!(s.frame_upload_bytes.sum, 300);
    assert_eq!(s.frame_deadline_bytes, 250);
    assert_eq!(s.invariant_violations_total[17], 0, "within the deadline");
    frame(&mut meter, &[(a, Bytes(200)), (b, Bytes(200))], 0, &[]);
    assert_eq!(meter.signals().invariant_violations_total[17], 1, "I18");
}

/// I17: `eye_lost_tiles{eye}` and `eye_bare_items{eye}` name the eye.
#[test]
fn the_eye_signals_name_the_eye_that_lost_ground() {
    let mut meter = BudgetMeter::new(&Rig::interactive_config());
    let ok = EyeCoverage::default();
    frame(&mut meter, &[], 0, &[ok, ok]);
    assert_eq!(meter.signals().eye_lost_tiles, [0; 4]);
    let lost = EyeCoverage { lost: 2, bare: 1 };
    frame(&mut meter, &[], 0, &[ok, lost]);
    let s = meter.signals();
    assert_eq!(s.eye_lost_tiles, [0, 2, 0, 0], "{RED}");
    assert_eq!(s.eye_bare_items, [0, 1, 0, 0]);
    assert_eq!(s.invariant_violations_total[16], 1, "I17");
}

/// I16: `uploads_total{class}` counts one upload per resource, whatever
/// the number of eyes that draw it, and a mesh with its drape in one frame
/// is a `mesh_drape` swap.
#[test]
fn the_uploads_signal_counts_one_upload_per_resource_for_two_eyes() {
    let mut meter = BudgetMeter::new(&Rig::interactive_config());
    let a = t(3, 0, 0);
    let ok = EyeCoverage::default();
    frame(
        &mut meter,
        &[(ResourceId::Mesh(a), Bytes(10)), (dr(a, 3), Bytes(20))],
        0,
        &[ok, ok],
    );
    let s = meter.signals();
    assert_eq!(s.uploads_total[Class::TerrainMesh], 1, "{RED}");
    assert_eq!(s.uploads_total[Class::Drape], 1);
    assert_eq!(s.upload_bytes_total[Class::Drape], 20);
    assert_eq!(s.swaps_total[2], 1, "the mesh and its drape together");
}

/// Every record-local check of spec §16.2 names the invariant it breaks.
#[test]
fn the_violations_signal_names_the_invariant_broken() {
    let cfg = floored();
    let a = t(3, 2, 2);
    let need = drawn(&[a]);
    fn record<'a>(need: &'a NeedSet, d: &'a Decisions) -> StepRecord<'a> {
        StepRecord {
            step: 0,
            frame: 0,
            now: Micros::ZERO,
            views_changed: false,
            eyes: 1,
            need,
            events: &[],
            presence: &[],
            decisions: d,
            ledger: Default::default(),
            pressure: Pressure::Normal,
            thermal: Thermal::Nominal,
            decide_wall: None,
        }
    }
    let clean = Decisions::default();
    let mut v = Violations::default();
    check(&record(&need, &clean), &cfg, &mut v);
    assert!(v.none(), "a clean record breaks nothing: {v:?}");
    type Breaks = fn(&mut Decisions);
    let cases: [(usize, Breaks); 5] = [
        (1, |d| d.stats.bare = 1),
        (2, |d| d.stats.gpu[Class::Drape] = Bytes(1)),
        (7, |d| d.stats.duplicates_in_flight = 1),
        (11, |d| {
            let a = t(3, 2, 2);
            d.evict.push(ResourceId::Mesh(a));
            d.draw.items.push(tuile_budget::DrawItem {
                mesh: ResourceId::Mesh(a),
                role: tuile_budget::Role::Exact,
                drapes: Vec::new(),
                serves: vec![a],
            });
        }),
        (1, |d| {
            d.stats.floor_in = true;
            d.stats.lost = 1;
        }),
    ];
    for (invariant, break_it) in cases {
        let mut d = Decisions::default();
        break_it(&mut d);
        let mut v = Violations::default();
        check(&record(&need, &d), &cfg, &mut v);
        assert_eq!(v.of(invariant), 1, "I{invariant} {RED}: {v:?}");
    }
    let mut batch = BudgetConfig::batch();
    batch.pinned_level = None;
    let mut d = Decisions::default();
    d.frame_ready = true;
    d.evict.push(ResourceId::Mesh(t(3, 6, 6)));
    let mut v = Violations::default();
    check(&record(&need, &d), &batch, &mut v);
    assert_eq!(v.of(13), 1, "I13");
}

/// Bounded cardinality (spec §16.5): over a random walk of the budget,
/// every label of every series takes its value from its closed set.
#[test]
fn labels_stay_within_their_closed_sets() {
    use tuile_budget::observe::labels;
    let mut m = Metered::new(floored());
    let root = t(0, 0, 0);
    for k in 0..200u64 {
        let x = k % 7;
        let mut need = drawn_draped(&[t(3, x, (k / 7) % 8)], 3 + (k % 4) as u32);
        need.ancestors.push((root, draped(root, 0)));
        let d = m.step(&need);
        if k % 2 == 0 {
            m.r.complete(&d);
        }
        frame(
            &mut m.meter,
            &[(ResourceId::Mesh(root), Bytes(1))],
            (k % 3) as u32,
            &[EyeCoverage::default(); 2],
        );
    }
    let classes: Vec<&str> = Class::ALL.iter().map(|c| labels::class(*c)).collect();
    let allowed = |key: &str, value: &str| -> bool {
        let set: Vec<&str> = match key {
            "class" => classes.clone(),
            "state" => labels::STATE.to_vec(),
            "source" => labels::SOURCE.to_vec(),
            "role" => labels::ROLE.to_vec(),
            "reason" => labels::CANCEL
                .iter()
                .chain(&labels::EVICT)
                .copied()
                .collect(),
            "kind" => labels::FAILURE
                .iter()
                .chain(&labels::FLAP)
                .chain(&labels::SWAP)
                .copied()
                .collect(),
            "gap" => labels::GAP.to_vec(),
            "eye" => labels::EYE.to_vec(),
            "invariant" => labels::INVARIANT.to_vec(),
            _ => Vec::new(),
        };
        set.contains(&value)
    };
    let series = m.meter.signals().series();
    assert!(series.len() > 100, "every signal exposed");
    for (name, labels, value) in &series {
        assert!(value.is_finite(), "{name}");
        for (k, v) in labels {
            assert!(allowed(k, v), "{name}: label {k}={v} outside its set");
        }
    }
    let _ = LayerId(0);
}
