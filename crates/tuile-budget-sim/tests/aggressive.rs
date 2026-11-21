// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Laurent's acceptance sequence (spec §9.3, head of the catalogue): an
//! aggressive zoom-in, pan, tilt, rotation and zoom-out, with zero hole at
//! every step by the footprint coverage check — the gate of every change of
//! the budget's behaviour.

use tuile_budget::Specification;
use tuile_budget_sim::catalogue::named;
use tuile_budget_sim::{run, Device, Network};

fn holes(device: Device, network: Network, lag: usize) -> Vec<(&'static str, u64)> {
    let s = named("aggressive-sequence").expect("in the catalogue");
    let o = run(&s, device, network, &Specification::interactive(), 101, lag);
    assert!(o.broke.is_none(), "{:?}", o.broke);
    o.phases
        .iter()
        .map(|p| (p.phase, p.hole_tile_steps))
        .filter(|(_, h)| *h > 0)
        .collect()
}

/// With the selection drawn the step it is made, no phase shows a hole.
#[test]
#[cfg_attr(debug_assertions, ignore = "release only: cargo test --release -p tuile-budget-sim")]
fn the_aggressive_sequence_shows_no_hole_drawn_at_once() {
    for (d, n) in [(Device::Desktop, Network::Fast), (Device::Phone, Network::Slow)] {
        assert_eq!(holes(d, n, 0), vec![], "{d:?} {n:?}");
    }
}

/// With the selection a quarter of a second late — what an asynchronous
/// geometry server's steps cost in the viewer (seconds there, measured
/// 2026-11-21) — the zoom-out and the rotation expose ground: the holes
/// Laurent saw. Red until the coarse neighbourhood (law 2b) covers the
/// reachable view whatever the selection's lag.
#[test]
#[ignore = "red: law 2b (the reachable coarse neighbourhood drawn under a late selection) is not implemented yet"]
fn the_aggressive_sequence_shows_no_hole_with_a_late_selection() {
    for (d, n) in [(Device::Desktop, Network::Fast), (Device::Phone, Network::Slow)] {
        assert_eq!(holes(d, n, 15), vec![], "{d:?} {n:?}");
    }
}
