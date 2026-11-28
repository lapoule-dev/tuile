// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! CB9 in the simulation: display at once, compress what lasts
//! (`docs/explorations/compress-what-lasts.md`). Every step is held to
//! `Sim::check`'s compression invariants besides these.

use tuile_budget::{Class, ResourceId};

use super::motion::fast_dive;
use super::scenarios::home;
use super::server::{Profile, Sim};
use super::world::{cost_for, SimPlan};

const RED: &str = "the compression in place regressed";

fn clean(mut p: Profile) -> Profile {
    p.net = p.net.clean();
    p
}

/// Whether the step's drapes were over their line (the target less the
/// present need's reserve, `regulator`): where the age
/// is waived (law 4).
fn pressed(l: &super::server::StepLog) -> bool {
    let line = l.stats.target[Class::Drape].minus(l.stats.envelope[Class::Drape]);
    l.stats.gpu[Class::Drape] > line
}

/// **A still view compresses what survives:** on a desktop holding BC7,
/// the drapes drawn are compressed once they have survived their decisions
/// with a very high score, and by the end are held compressed — but the
/// few the encoder refused. No compression is decided in the steps before
/// the first drape could have survived its count, unless the drapes are
/// over their line.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_view_compresses_what_survives() {
    let profile = clean(Profile::desktop_bc7());
    let n = usize::from(tuile_budget::spec::COMPRESS.decisions);
    let mut sim = Sim::new(profile, 241);
    sim.run(8.0, &|_| home());
    let first_drawn = sim
        .log
        .iter()
        .position(|l| l.stats.gpu[Class::Drape].0 > 0)
        .expect("drapes drawn");
    let first = sim
        .log
        .iter()
        .position(|l| l.stats.compress.started > 0)
        .unwrap_or_else(|| panic!("nothing compressed in eight still seconds: {RED}"));
    assert!(
        first >= first_drawn + n || pressed(&sim.log[first]),
        "compressed at step {first}, {n} decisions after {first_drawn} not yet passed, with room"
    );
    let last = sim.log.last().unwrap();
    assert_eq!(last.stats.compress.in_flight, 0, "all done once still");
    // What the drapes drawn hold, against them held RGBA8 and compressed.
    let drawn: Vec<ResourceId> = sim.drawn_drapes();
    let (held, rgba8, blocks) = drawn.iter().fold((0u64, 0u64, 0u64), |a, r| {
        let c = cost_for(*r, SimPlan::Gpu);
        (
            a.0 + sim.gpu.drawable.get(r).map_or(0, |b| b.0),
            a.1 + c.gpu.0,
            a.2 + c.compressed.0,
        )
    });
    assert!(!drawn.is_empty());
    assert!(
        held * 10 <= blocks * 11 + rgba8 / 20,
        "drawn drapes hold {held}: {blocks} compressed, {rgba8} as RGBA8 — {RED}"
    );
}

/// **Under pressure, compressing comes before evicting:** a phone short of
/// memory diving fast holds more of what it loaded when its drapes can be
/// compressed — fewer evictions than the same dive held RGBA8, its picture
/// as whole (no hole, checked every step).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn under_pressure_compression_spares_evictions() {
    let run = |plan: SimPlan| {
        let p = clean(Profile::handheld_low()).compressed("handheld-low-cb9", plan);
        let mut sim = Sim::new(p, 242);
        sim.run(6.0, &fast_dive);
        sim.run(4.0, &|_| fast_dive(6.0));
        let evicted: u64 = sim.log.iter().map(|l| u64::from(l.stats.evicted)).sum();
        let compressed: u64 = sim
            .log
            .iter()
            .map(|l| u64::from(l.stats.compress.started))
            .sum();
        (evicted, compressed)
    };
    let (rgba8, none) = run(SimPlan::Rgba8);
    let (gpu, compressed) = run(SimPlan::Gpu);
    assert_eq!(none, 0);
    assert!(compressed > 0, "{RED}");
    assert!(
        gpu < rgba8,
        "evictions: {gpu} compressing, {rgba8} held RGBA8"
    );
}
