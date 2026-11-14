// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **What settling costs in time and in CPU.**
//!
//! The flight of `bake_memory`, settled ([`settle_each`]) and read again
//! ([`read_each`]), each ground read the way an audit reads it: its height
//! over a grid under the camera. One `BENCH` line gives each stage's wall
//! time; the process's CPU time is the caller's to measure around it, which
//! is what it is for — idle threads spinning show there and nowhere else.
//!
//! ```text
//! cargo test -p tuile-bake --release --test settle_cpu -- --ignored --nocapture
//! /usr/bin/time -l target/release/deps/settle_cpu-<hash> --ignored --nocapture
//! ```
//!
//! Knobs, by environment: `BAKE_MEMORY_FRAMES` (32), `BAKE_MEMORY_WIDTH` /
//! `_HEIGHT` (1920 × 1080), `BAKE_MEMORY_SSE` (3), `BAKE_MEMORY_STEPS` (32),
//! `SETTLE_CPU_GRID` (the audit's grid a side, 256).

#[path = "bake_memory_support/mod.rs"]
mod flight;
#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Instant;

use support::{LAT, LON};
use tuile_bake::settle::{read_each, settle_each};
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle};
use tuile_film::ground::Ground;

/// An audit's reading: the ground's height over a `grid` × `grid` patch
/// ahead of frame `i`, folded into one number.
fn audit(i: u32, ground: &Ground, grid: u32) -> u64 {
    let lat = (LAT + f64::from(i) * 0.00036).to_radians();
    let lon = LON.to_radians();
    let mut fold = 0u64;
    for y in 0..grid {
        for x in 0..grid {
            let d = 0.0002 / f64::from(grid);
            if let Some(h) = ground.height_at(lon + f64::from(x) * d, lat + f64::from(y) * d) {
                fold = fold.rotate_left(5) ^ h.to_bits();
            }
        }
    }
    fold
}

#[test]
#[ignore = "a bench: run it alone, in release, under a CPU clock"]
fn what_settling_costs() {
    let frames: u32 = flight::knob("BAKE_MEMORY_FRAMES", 32);
    let viewport = (
        flight::knob("BAKE_MEMORY_WIDTH", 1920.0),
        flight::knob("BAKE_MEMORY_HEIGHT", 1080.0),
    );
    let sse: f64 = flight::knob("BAKE_MEMORY_SSE", 3.0);
    let steps: usize = flight::knob("BAKE_MEMORY_STEPS", 32);
    let grid: u32 = flight::knob("SETTLE_CPU_GRID", 256);

    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(flight::MadeUp { steps })));
    config.session.traversal.maximum_screen_space_error = sse;
    config.session.finish = Finish::Identity;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    let mut drawer = GroundDrawer::new();

    let numbers: Vec<u32> = (0..frames).collect();
    let views = |i: u32, _: &Session| vec![pose_view(&flight::pose(i), viewport)];
    let started = Instant::now();
    let settled = settle_each(
        &mut session,
        &mut drawer,
        flight::DRAWN,
        &numbers,
        views,
        |i, ground: &Arc<Ground>| audit(i, ground, grid),
        |_| true,
    )
    .expect("settled");
    let settle_s = started.elapsed().as_secs_f64();

    let kept: Vec<(u32, &tuile_bake::Selected)> = settled
        .iter()
        .filter_map(|s| Some((s.number, s.selected.as_ref()?)))
        .collect();
    let started = Instant::now();
    let again = read_each(&mut drawer, flight::DRAWN, &kept, |i, ground| {
        audit(i, ground, grid)
    });
    let read_s = started.elapsed().as_secs_f64();

    // What was read, folded: equal before and after a change to settling.
    let digest = settled
        .iter()
        .map(|s| (s.selection, s.value))
        .chain(again.iter().map(|(v, _, _)| (0, *v)))
        .fold(0u64, |d, (a, b)| d.rotate_left(7) ^ a ^ b.rotate_left(13));
    eprintln!(
        "BENCH {{\"frames\":{frames},\"viewport\":[{},{}],\"sse\":{sse},\"grid\":{grid},\
\"settle_s\":{settle_s:.3},\"read_again_s\":{read_s:.3},\"digest\":\"{digest:016x}\"}}",
        viewport.0, viewport.1
    );
}
