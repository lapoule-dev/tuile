// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What one `decide` costs at the viewer's scale (tens of thousands of
//! tracked resources), not the bench's thousand: the desktop on a large
//! device, warmed by a long tour so that its tracked set grows as the
//! viewer's does, then the aggressive sequence played again and again with
//! every `decide` timed.
//!
//! ```text
//! cargo run --release -p tuile-budget-sim --bin tuile-budget-viewer-scale -- [--gib 8] [--warm 300] [--loops 3]
//! ```
//!
//! Prints the tracked resources, then `decide` mean / p50 / p99 / max (µs)
//! and the work units per step, in motion. Run it under `sample` (macOS) or
//! `perf` for a per-function profile.

use tuile_budget::{Bytes, BudgetConfig};
use tuile_budget_sim::catalogue::{desktop_on, named};
use tuile_budget_sim::Sim;

fn arg(name: &str, or: f64) -> f64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(or)
}

fn main() {
    let gib = arg("--gib", 8.0) as u64;
    let warm = arg("--warm", 300.0);
    let loops = arg("--loops", 3.0) as usize;
    let mut cfg = BudgetConfig::interactive(Bytes::gib(gib));
    cfg.decoded_target = Bytes::gib(gib / 2);
    let mut sim = Sim::unchecked(desktop_on(cfg), 7);
    let tour = tuile_budget_sim::catalogue::tour;
    sim.run(warm, &tour);
    let s = named("aggressive-sequence").expect("in the catalogue");
    let start = sim.now;
    let first = sim.decide_us.len();
    for k in 0..loops {
        let t0 = sim.now.0 as f64 / 1e6;
        let path = s.path;
        let seconds = s.seconds;
        sim.run(seconds + 4.0, &move |t| path((t - t0).clamp(0.0, seconds)));
        eprintln!("loop {k}: tracked {}", sim.log.last().map_or(0, |l| l.stats.tracked));
    }
    let mut us: Vec<f64> = sim.decide_us[first..].iter().map(|(_, u)| *u).collect();
    let work: Vec<u32> = sim
        .log
        .iter()
        .filter(|l| l.now >= start)
        .map(|l| l.stats.work)
        .collect();
    us.sort_by(f64::total_cmp);
    let pct = |p: usize| us.get(us.len() * p / 100).copied().unwrap_or(0.0);
    let mean = us.iter().sum::<f64>() / us.len().max(1) as f64;
    let mut w = work.clone();
    w.sort_unstable();
    let wp = |p: usize| w.get(w.len() * p / 100).copied().unwrap_or(0);
    println!(
        "viewer scale ({gib} GiB, warm {warm} s): tracked {}, steps {}; decide mean {mean:.0} µs, \
         p50 {:.0}, p99 {:.0}, max {:.0}; work p50 {}, p99 {}, max {}",
        sim.log.last().map_or(0, |l| l.stats.tracked),
        us.len(),
        pct(50),
        pct(99),
        us.last().copied().unwrap_or(0.0),
        wp(50),
        wp(99),
        w.last().copied().unwrap_or(0),
    );
}
