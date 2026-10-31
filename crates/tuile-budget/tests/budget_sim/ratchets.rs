// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Allocation ratchets (A3.g): what `decide` asks of the heap, and what a
//! budget holds per tracked resource, measured by the counting allocator and
//! frozen in `alloc_ratchets.txt` like the goldens.
//!
//! A ratchet only goes down. A measure above its frozen value is a
//! regression; one below it is an improvement the same change must record
//! (`TUILE_BLESS_BUDGET=1`), so a frozen value is always the current one and
//! the next regression is seen at its first block. Allocation counts do not
//! depend on the allocator underneath, so they hold on the Linux CI as on
//! the Mac that recorded them.

use tuile_budget::{Budget, BudgetMeter, BudgetObserver, Micros, Pressure, StepRecord, Thermal};

use super::scenarios::{approach, home};
use super::server::{DecideAllocs, Profile, Sim};

/// The still view: the approach, then still until nothing churns, then 120
/// steps measured. The worst step's internal blocks (beyond its output),
/// its output's, what it drew, and the worst step of a `BudgetMeter`
/// watching it (past its first two, which fill its maps).
fn still() -> (u64, u64, u32, u64) {
    let mut sim = Sim::new(Profile::desktop(), 7);
    sim.run(5.0, &approach);
    let stop = sim.now;
    sim.run(25.0, &|_| home());
    assert!(
        sim.settled_after(stop).is_some(),
        "the still view never settled"
    );
    sim.count_output = true;
    let from = sim.decide_allocs.len();
    let mut meter = BudgetMeter::new(sim.budget.config());
    let mut metered = Vec::new();
    for step in 0..120u64 {
        let (need, d) = sim.step(home());
        let rec = StepRecord {
            step,
            frame: sim.frame,
            now: sim.now,
            views_changed: false,
            eyes: 1,
            need: &need,
            events: &[],
            presence: &[],
            decisions: &d,
            ledger: sim.budget.ledger(),
            pressure: Pressure::Normal,
            thermal: Thermal::Nominal,
            decide_wall: None,
        };
        let ((), t) = super::alloc::counted(|| meter.step(&rec));
        metered.push(t.allocs);
    }
    let meter_worst = metered.iter().skip(2).copied().max().unwrap_or(0);
    let w = &sim.decide_allocs[from..];
    let worst = w.iter().map(DecideAllocs::internal).max().unwrap_or(0);
    let output = w.iter().map(|a| a.output).max().unwrap_or(0);
    let drawn = w.iter().map(|a| a.drawn).max().unwrap_or(0);
    (worst, output, drawn, meter_worst)
}

/// The soak's tour for a simulated minute: the p99 of the internal blocks
/// per step past the first ten seconds, the step buffers the budget holds
/// then (`release_scratch`), and the heap the budget frees when dropped
/// without them, per tracked resource.
fn motion() -> (u64, i64, i64, usize) {
    let mut sim = Sim::new(Profile::desktop(), 11);
    sim.count_output = true;
    let end = Micros::from_secs(60);
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        sim.step(super::soak::tour(t));
    }
    let mut internal: Vec<u64> = sim
        .decide_allocs
        .iter()
        .filter(|a| a.now >= Micros::from_secs(10))
        .map(DecideAllocs::internal)
        .collect();
    internal.sort_unstable();
    let p99 = internal
        .get(internal.len() * 99 / 100)
        .copied()
        .unwrap_or(0);
    let tracked = sim.budget.tracked();
    let ((), t) = super::alloc::counted(|| sim.budget.release_scratch());
    let scratch = -t.live;
    let cfg = sim.budget.config().clone();
    let fresh = Budget::new(cfg).unwrap();
    // `Budget::new` holds nothing on the heap (empty maps and vectors), so
    // what the swap and drop give back is the old budget's whole heap.
    let ((), t) = super::alloc::counted(|| drop(std::mem::replace(&mut sim.budget, fresh)));
    let per = -t.live / (tracked.max(1) as i64);
    (p99, scratch, per, tracked)
}

/// A3.g: the converged still view, the tour's p99 and the bytes per tracked
/// resource hold their frozen values exactly — never above (a regression),
/// never below (an improvement left unrecorded).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn allocation_ratchets_hold() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/budget_sim/alloc_ratchets.txt");
    let (still_worst, still_output, still_drawn, meter_worst) = still();
    let (motion_p99, scratch, per_tracked, tracked) = motion();
    eprintln!(
        "still: {still_worst} internal blocks per step at worst ({still_output} for the output, \
         {still_drawn} drawn), a meter watching it {meter_worst}; tour: p99 {motion_p99} internal blocks per step, then \
         {scratch} B of step buffers and {per_tracked} B per tracked resource ({tracked} tracked)"
    );
    let got = format!(
        "still-internal-allocs-per-step {still_worst}\n\
         meter-allocs-per-still-step {meter_worst}\n\
         motion-internal-allocs-per-step-p99 {motion_p99}\n\
         scratch-bytes-after-tour {scratch}\n\
         bytes-per-tracked-resource {per_tracked}\n"
    );
    if std::env::var_os("TUILE_BLESS_BUDGET").is_some() {
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).expect("ratchets recorded");
    for (g, w) in got.lines().zip(want.lines()) {
        let (name, g) = g.split_once(' ').unwrap();
        let (wname, w) = w.split_once(' ').unwrap();
        assert_eq!(name, wname, "ratchet names");
        let (g, w): (i64, i64) = (g.parse().unwrap(), w.parse().unwrap());
        assert!(g <= w, "{name}: {g}, above its ratchet {w}: a regression");
        assert!(
            g >= w,
            "{name}: {g}, below its ratchet {w}: an improvement — record it \
             (TUILE_BLESS_BUDGET=1) in the same change"
        );
    }
    assert_eq!(got.lines().count(), want.lines().count(), "ratchet count");
}
