// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What one `decide` costs (A3), on the simulation's world:
//!
//! - `decide/still`: a converged still view, 484 tiles drawn, the
//!   same need every step: the steady state;
//! - `decide/motion`: the soak's tour, the step's `decide` alone timed.
//!
//! Criterion gives the means; the percentiles are printed first
//! (`decide still: mean … p50 … p99 …`), from individually timed calls,
//! since a p99 is what a frame budget feels.
//!
//! `cargo bench -p tuile-budget --bench budget_decide`

#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::panic)]
// The simulation's modules come in whole, its counting allocator with them
// (never installed here: the bench times the system allocator).
#![allow(unsafe_code)]

#[path = "../tests/budget_sim/alloc.rs"]
mod alloc;
#[path = "../tests/budget_sim/detmath.rs"]
mod detmath;
#[path = "../tests/budget_sim/gpu.rs"]
mod gpu;
#[path = "../tests/budget_sim/net.rs"]
mod net;
#[path = "../tests/budget_sim/rng.rs"]
mod rng;
#[path = "../tests/budget_sim/server.rs"]
mod server;
#[path = "../tests/budget_sim/soak.rs"]
mod soak;
#[path = "../tests/budget_sim/world.rs"]
mod world;

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use tuile_budget::{FrameInput, Micros, NeedSet};

use server::{Profile, Sim};
use world::{Camera, SIDE};

/// Over the plain, 2 km up, looking east and 20° down: 484 tiles drawn.
fn still_camera() -> Camera {
    Camera {
        x: 0.6 * SIDE,
        y: 0.5 * SIDE,
        h: 2_000.0,
        heading: 0.0,
        pitch: 20f64.to_radians(),
    }
}

/// A desktop simulation converged on the still camera, and the need it
/// keeps asking.
fn converged() -> (Sim, NeedSet) {
    let mut sim = Sim::new(Profile::desktop(), 7);
    let cam = still_camera();
    let mut need = NeedSet::new();
    while sim.now < Micros::from_secs(30) {
        need = sim.step(cam).0;
    }
    (sim, need)
}

/// One more step of the same need, `decide` alone.
fn decide_still(sim: &mut Sim, need: &NeedSet) -> usize {
    let input = FrameInput::new(sim.frame, sim.now, need).with_presence(&sim.net.store);
    let d = sim.budget.decide(input);
    sim.frame += 1;
    sim.now = sim.now.plus(sim.profile.dt);
    d.draw.items.len()
}

fn percentiles(name: &str, mut us: Vec<f64>, drawn: usize) {
    us.sort_by(f64::total_cmp);
    let at = |p: usize| us.get(us.len() * p / 100).copied().unwrap_or(0.0);
    let mean = us.iter().sum::<f64>() / us.len().max(1) as f64;
    eprintln!(
        "decide {name}: mean {mean:.1} µs, p50 {:.1} µs, p99 {:.1} µs, max {:.1} µs \
         ({} calls, {drawn} drawn)",
        at(50),
        at(99),
        us.last().copied().unwrap_or(0.0),
        us.len()
    );
}

/// The percentiles, from individually timed calls.
fn report() {
    let (mut sim, need) = converged();
    let mut drawn = 0;
    let mut us = Vec::with_capacity(2_000);
    for _ in 0..2_000 {
        let t = Instant::now();
        drawn = black_box(decide_still(&mut sim, &need));
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    percentiles("still", us, drawn);

    let mut sim = Sim::new(Profile::desktop(), 11);
    while sim.now < Micros::from_secs(240) {
        let t = sim.now.0 as f64 / 1e6;
        sim.step(soak::tour(t));
    }
    let us: Vec<f64> = sim
        .decide_us
        .iter()
        .filter(|(t, _)| *t >= Micros::from_secs(10))
        .map(|(_, d)| *d)
        .collect();
    let drawn = sim.decide_allocs.iter().map(|a| a.drawn).max().unwrap_or(0);
    percentiles("motion", us, drawn as usize);
}

fn bench(c: &mut Criterion) {
    report();
    let mut g = c.benchmark_group("decide");
    let (mut sim, need) = converged();
    g.bench_function("still", |b| {
        b.iter(|| black_box(decide_still(&mut sim, &need)))
    });
    let mut sim = Sim::new(Profile::desktop(), 11);
    g.bench_function("motion", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let t = sim.now.0 as f64 / 1e6;
                sim.step(soak::tour(t));
                let us = sim.decide_us.last().map_or(0.0, |(_, d)| *d);
                total += Duration::from_secs_f64(us / 1e6);
            }
            total
        })
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
