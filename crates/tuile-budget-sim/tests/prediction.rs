// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The views ahead on Laurent's acceptance sequence (zoom-in, pan, tilt,
//! rotation, zoom-out): with the camera's motion model predicting where it
//! goes, fewer tiles wait under their need than with no prediction, and no
//! phase shows a hole; the model's error grows with the horizon.

use tuile_budget::Specification;
use tuile_budget_sim::catalogue::named;
use tuile_budget_sim::{run_ahead, Ahead, Device, Network, Outcome};
use tuile_camera::predict::PredictConfig;

fn sequence(device: Device, network: Network, lag: usize, ahead: &Ahead) -> Outcome {
    let s = named("aggressive-sequence").expect("in the catalogue");
    let o = run_ahead(
        &s,
        device,
        network,
        &Specification::interactive(),
        101,
        lag,
        ahead,
    );
    assert!(o.broke.is_none(), "{:?}", o.broke);
    o
}

fn report(what: &str, o: &Outcome) {
    eprintln!(
        "{what}: under-need {} tile·steps, deficit {:.1} level·s, holes {}, starts {} (wasted {}), thrash {}, work/step {:.0} (peak {}), settle {:?}",
        o.under_tile_steps,
        o.deficit_level_seconds,
        o.hole_tile_steps,
        o.phases.iter().map(|p| p.starts).sum::<u64>(),
        o.phases.iter().map(|p| p.wasted_starts).sum::<u64>(),
        o.thrash,
        o.work_sum as f64 / o.steps.max(1) as f64,
        o.work_max,
        o.settle_seconds,
    );
    for p in &o.phases {
        eprintln!(
            "    {:9} holes {:4} deficit {:7.2} starts {:5} wasted {:4}",
            p.phase, p.hole_tile_steps, p.deficit_level_seconds, p.starts, p.wasted_starts
        );
    }
}

/// With the motion model's views ahead, the sequence leaves fewer tiles
/// under their need than without, and shows no hole.
///
/// Red on the budget as it is. Measured 2026-11-23 on #282's decider: the
/// ring shared the value band with the present need at the specification's
/// flat probability — 74 196 under-need tile·steps with the model against
/// 23 271 without. On main after #289 (2026-11-27, Desktop/Fast): 29 369
/// with the model, 80 196 with the exact path, 17 368 without. With the
/// decider reading `AheadUse` (the prediction's `P`, its drapes preloaded,
/// a predicted tile delivered whole, the present need preempting predicted
/// loads): 21 362 with the model, 47 173 with the path; Phone/Slow 1 576
/// and 1 846 against 1 073 without (1 335 and 1 691 before). What remains:
/// the simulation's selection draws what is resident, so a predicted mesh
/// resident early is drawn early and its geometry gain is not counted,
/// while its wait for its drape is; and a present need brought to full
/// detail sooner meets the mesh class's hard limit (all kept: drawn, swap,
/// chain) and relaxes the picture, whose short layers count as under need.
#[test]
#[ignore = "red: under-need with the model 21 362 vs 17 368 without (Desktop/Fast), 1 576 vs 1 073 (Phone/Slow)"]
fn predicted_views_ahead_leave_fewer_tiles_under_their_need() {
    let model = Ahead::Model(PredictConfig::default());
    let known = Ahead::Known(PredictConfig::default());
    let mut runs = Vec::new();
    for (d, n) in [
        (Device::Desktop, Network::Fast),
        (Device::Phone, Network::Slow),
    ] {
        let off = sequence(d, n, 0, &Ahead::Nothing);
        let on = sequence(d, n, 0, &model);
        let exact = sequence(d, n, 0, &known);
        report(&format!("{d:?} {n:?} off"), &off);
        report(&format!("{d:?} {n:?} model"), &on);
        report(&format!("{d:?} {n:?} known path"), &exact);
        runs.push((d, n, off, on, exact));
    }
    // Every run reported before the first assertion.
    for (d, n, off, on, exact) in &runs {
        assert_eq!(on.hole_tile_steps, 0, "{d:?} {n:?}: holes with the model");
        assert_eq!(exact.hole_tile_steps, 0, "{d:?} {n:?}: holes with the path");
        assert!(
            on.under_tile_steps < off.under_tile_steps,
            "{d:?} {n:?}: {} under-need tile·steps with the model, {} without",
            on.under_tile_steps,
            off.under_tile_steps
        );
    }
}

/// The model's measured error on the sequence grows with the horizon.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget-sim"
)]
fn the_models_error_on_the_sequence_grows_with_the_horizon() {
    let on = sequence(
        Device::Desktop,
        Network::Fast,
        0,
        &Ahead::Model(PredictConfig::default()),
    );
    for e in &on.ahead_errors {
        eprintln!(
            "{:.2} s: n {} · ground km {:.3}/{:.3} · heading ° {:.2}/{:.2} · pitch ° {:.2}/{:.2} · height ×{:.3}/{:.3}",
            e.horizon_s,
            e.n,
            e.ground_km[0],
            e.ground_km[1],
            e.heading_deg[0],
            e.heading_deg[1],
            e.pitch_deg[0],
            e.pitch_deg[1],
            e.height_ratio[0],
            e.height_ratio[1],
        );
    }
    let p90: Vec<f64> = on.ahead_errors.iter().map(|e| e.ground_km[1]).collect();
    assert_eq!(p90.len(), 4);
    for w in p90.windows(2) {
        assert!(w[1] >= w[0], "{p90:?}");
    }
    let heights: Vec<f64> = on.ahead_errors.iter().map(|e| e.height_ratio[1]).collect();
    for w in heights.windows(2) {
        assert!(w[1] >= w[0], "{heights:?}");
    }
}

/// The same inputs give the same run, predictions included.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget-sim"
)]
fn a_predicted_run_is_reproducible() {
    let model = Ahead::Model(PredictConfig::default());
    let a = sequence(Device::Desktop, Network::Fast, 0, &model);
    let b = sequence(Device::Desktop, Network::Fast, 0, &model);
    assert_eq!(a, b);
}

/// With the selection a quarter of a second late, what the views ahead
/// change — reported, not asserted: the late selection's holes are the
/// coarse neighbourhood's to close (law 2b), not the prediction's.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget-sim"
)]
fn a_late_selection_with_and_without_the_views_ahead() {
    for (d, n) in [
        (Device::Desktop, Network::Fast),
        (Device::Phone, Network::Slow),
    ] {
        report(
            &format!("{d:?} {n:?} lag 15 off"),
            &sequence(d, n, 15, &Ahead::Nothing),
        );
        report(
            &format!("{d:?} {n:?} lag 15 model"),
            &sequence(d, n, 15, &Ahead::Model(PredictConfig::default())),
        );
    }
}
