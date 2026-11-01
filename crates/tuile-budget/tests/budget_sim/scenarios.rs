// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The scenarios of spec §8.

use tuile_budget::{Bytes, Class, Micros, Pressure, Thermal, BACKOFF_KEEP_MAX};

use super::detmath;
use super::net::NetProfile;
use super::server::{Profile, Sim};
use super::world::{Camera, SIDE};

const RED: &str = "the budget core regressed";

/// Over the coastal plain, 2 km up, looking east and 30° down.
pub fn home() -> Camera {
    Camera {
        x: 0.6 * SIDE,
        y: 0.5 * SIDE,
        h: 2_000.0,
        heading: 0.0,
        pitch: 30f64.to_radians(),
    }
}

/// A five-second approach to `home`, then still.
pub fn approach(t: f64) -> Camera {
    let k = (t / 5.0).min(1.0);
    Camera {
        x: home().x - 20_000.0 * (1.0 - k),
        ..home()
    }
}

pub fn settle(sim: &mut Sim, secs: f64) -> Micros {
    sim.run(5.0, &approach);
    let stop = sim.now;
    sim.run(secs, &|_| home());
    stop
}

/// A flight that grows the tracked set, then shrinks it: the approach, up
/// to 60 km and a turn over the plain (thousands of tiles tracked), back
/// down to the still view, then still.
pub fn grow_then_shrink(t: f64) -> Camera {
    let h0 = home();
    let lg = |a: f64, b: f64, k: f64| a * detmath::exp2(detmath::log2(b / a) * k);
    if t < 5.0 {
        approach(t)
    } else if t < 10.0 {
        Camera {
            h: lg(2_000.0, 60_000.0, (t - 5.0) / 5.0),
            ..h0
        }
    } else if t < 25.0 {
        Camera {
            h: 60_000.0,
            heading: (t - 10.0) / 15.0 * std::f64::consts::TAU,
            ..h0
        }
    } else if t < 30.0 {
        Camera {
            h: lg(60_000.0, 2_000.0, (t - 25.0) / 5.0),
            ..h0
        }
    } else {
        h0
    }
}

/// Flies `sim` through [`grow_then_shrink`] for a minute, the app sent to
/// the background at 40 s (only the floor and what is drawn kept) and back
/// at 42 s; `each` sees the simulation after every step. Returns the
/// tracked peak.
pub fn fly_grow_then_shrink(sim: &mut Sim, mut each: impl FnMut(&Sim)) -> usize {
    let mut peak = 0;
    while sim.now < Micros::from_secs(60) {
        let t = sim.now.0 as f64 / 1e6;
        if (40.0..42.0).contains(&t) {
            sim.pressure(Pressure::Background);
        } else if (42.0..42.1).contains(&t) {
            sim.pressure(Pressure::Normal);
        }
        sim.step(grow_then_shrink(t));
        peak = peak.max(sim.budget.tracked());
        each(sim);
    }
    peak
}

/// The arena gives its room back, in the budget, checked: on the flight
/// that grows the tracked set to thousands and falls back to the still
/// view, the room the budget has for tracked resources follows — it
/// stays far over the set (sparse) for a while, then is cut to twice the
/// set — and every decision and the whole state are the recorded ones
/// (the reference, recorded with this scenario). The checked simulation
/// (release with debug assertions) runs the arena's and the kept sets'
/// renumbering checks here, every step.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn the_arena_gives_its_room_back_on_the_flight_that_shrinks_the_tracked_set() {
    let mut sim = Sim::new(Profile::desktop(), 7);
    // (t, tracked, room) after every step.
    let mut trace: Vec<(Micros, usize, usize)> = Vec::new();
    let peak = fly_grow_then_shrink(&mut sim, |s| {
        trace.push((s.now, s.budget.tracked(), s.budget.tracked_room()));
    });
    let room_max = trace.iter().map(|x| x.2).max().unwrap_or(0);
    let sparse = trace
        .iter()
        .filter(|(_, tracked, room)| *room > 256 && *room > 4 * tracked)
        .count();
    // A compaction: the room falls from one step to the next, to twice the
    // tracked set (256 at least), which the budget then grows from.
    let compactions: Vec<&(Micros, usize, usize)> = trace
        .windows(2)
        .filter(|w| w[1].2 < w[0].2)
        .map(|w| &w[1])
        .collect();
    let (_, tracked, room) = *trace.last().expect("steps");
    eprintln!(
        "grow then shrink: tracked peak {peak}, room at most {room_max}, sparse on {sparse} \
         steps, compacted {} time(s) ({}), ending at {tracked} tracked in a room of {room}",
        compactions.len(),
        compactions
            .iter()
            .map(|(t, n, r)| format!("t = {:.2} s: {n} tracked, room {r}", t.0 as f64 / 1e6))
            .collect::<Vec<_>>()
            .join("; ")
    );
    assert!(peak >= 3 * tracked, "{RED}: the flight shrinks the set");
    // Hysteresis: the eighth prune in a row (one every eighth step) that
    // finds the set sparse compacts, so at least seven whole prune
    // intervals pass sparse first.
    assert!(
        sparse >= 7 * 8,
        "{RED}: the set under a quarter of the room on {sparse} steps"
    );
    assert!(
        !compactions.is_empty(),
        "{RED}: the room was never given back"
    );
    for (_, n, r) in &compactions {
        assert_eq!(*r, (2 * n).max(256), "{RED}: the room after a compaction");
    }
    assert!(room < room_max / 2, "{RED}: room {room} of {room_max} kept");
}

pub fn converges(mut profile: Profile) {
    // I3 holds at the p95 latency: no host stalls here (they are the lossy
    // scenario's), failures still retried.
    for s in [&mut profile.net.meshes, &mut profile.net.imagery] {
        s.stall_permille = 0;
    }
    let within = profile.converge_within;
    let mut sim = Sim::new(profile, 1);
    let stop = settle(&mut sim, 6.0);
    // From the moment the camera stops and the floor is drawable.
    let stop = stop.max(sim.floor_at.expect(RED));
    let took = sim.converged_after(stop).expect(RED);
    assert!(
        took <= within,
        "{}: converged after {took:?}, bound {within:?}",
        sim.profile.name
    );
}

/// I3: on a desktop, a still camera converges within two simulated seconds.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_camera_converges_within_two_seconds_on_desktop() {
    converges(Profile::desktop());
}

/// I3: on a phone, within four. Ignored while it measured 6.4 s; it
/// converges in 3.15 s under the pooled ledger (measured 2026-10-21).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_camera_converges_within_four_seconds_on_a_phone() {
    converges(Profile::handheld());
}

/// I3, audit: at the handheld low clamp (160 MiB), within four too.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_camera_converges_at_the_handheld_low_clamp() {
    converges(Profile::handheld_low());
}

/// I3, I4, I5, S2 (M2.1): ten still minutes cancel nothing, change no
/// tile's level twice in ten seconds, and hold the decision digest.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn ten_still_minutes_cancel_nothing_flap_nothing_and_hold_the_digest() {
    let mut sim = Sim::new(Profile::desktop(), 2);
    let stop = settle(&mut sim, 600.0);
    let after = sim.converged_after(stop).expect(RED);
    let from = stop.plus(after);
    let still: Vec<_> = sim.log.iter().filter(|l| l.now >= from).collect();
    let cancelled: u32 = still.iter().map(|l| l.stats.cancelled).sum();
    assert_eq!(cancelled, 0, "I5: a still camera cancelled loads");
    assert!(
        sim.worst_flapping() <= 1,
        "I4: a tile changed level {} times in 10 s",
        sim.worst_flapping()
    );
    // Once the budget has nothing left to do (preload and ring included),
    // its decisions do not move.
    let idle = stop.plus(sim.settled_after(stop).expect("settled"));
    let rest: Vec<_> = sim.log.iter().filter(|l| l.now >= idle).collect();
    let first = rest.first().map(|l| l.digest);
    assert!(
        rest.iter().all(|l| Some(l.digest) == first),
        "I3: the digest moved at rest"
    );
    assert!(rest.len() > 30_000, "at rest for most of the ten minutes");
}

pub fn turn(t: f64) -> Camera {
    let k = ((t - 6.0) / 1.0).clamp(0.0, 1.0);
    Camera {
        heading: std::f64::consts::PI * k,
        ..home()
    }
}

/// S3 (M3.2, M3.3): a 180° turn in one second converges within the bound.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_half_turn_in_one_second_converges() {
    let mut p = Profile::desktop();
    p.net.meshes.stall_permille = 0;
    p.net.imagery.stall_permille = 0;
    let mut sim = Sim::new(p, 3);
    sim.run(5.0, &approach);
    sim.run(1.0, &|_| home());
    sim.run(1.0, &turn);
    let stop = sim.now;
    sim.run(6.0, &|_| Camera {
        heading: std::f64::consts::PI,
        ..home()
    });
    let took = sim.converged_after(stop).expect(RED);
    assert!(took <= Micros::from_secs(2), "converged after {took:?}");
}

pub fn zoom(t: f64) -> Camera {
    // 0–5 s approach; 5–10 s climb to 200 km; 10–12 s hold; 12–17 s back.
    let h = if t < 5.0 {
        2_000.0
    } else if t < 10.0 {
        2_000.0 * detmath::exp2(detmath::log2(100.0) * (t - 5.0) / 5.0)
    } else if t < 12.0 {
        200_000.0
    } else if t < 17.0 {
        200_000.0 * detmath::exp2(detmath::log2(0.01) * (t - 12.0) / 5.0)
    } else {
        2_000.0
    };
    Camera {
        h,
        ..approach(t.min(5.0))
    }
}

/// I1, I9: a zoom out to 200 km and back never shows a hole (checked every
/// step, in both swap directions).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_zoom_out_to_200_km_and_back_never_shows_a_hole() {
    let mut sim = Sim::new(Profile::desktop(), 4);
    sim.run(20.0, &zoom);
    let lost: u32 = sim.log.iter().map(|l| l.stats.lost).sum();
    assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    let first_in = sim.log.iter().position(|l| l.stats.exact > 0).unwrap_or(0);
    let after: u32 = sim.log[first_in..].iter().map(|l| l.stats.lost).sum();
    assert_eq!(after, 0, "{lost} lost in all");
}

/// S3 (M3.1), I10: back down after a zoom-out, the drapes are sharp again.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn after_a_zoom_out_and_back_the_drapes_are_sharp_again() {
    let mut sim = Sim::new(Profile::desktop(), 5);
    sim.run(17.0, &zoom);
    let stop = sim.now;
    sim.run(6.0, &zoom);
    let took = sim.converged_after(stop).expect(RED);
    assert!(took <= Micros::from_secs(2), "{took:?}");
    let last = sim.log.last().expect("steps");
    assert_eq!(last.stats.underdraped, 0);
}

/// C21, I10: over the plain, imagery is deeper than terrain; everywhere it
/// matches its need.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn over_the_plain_imagery_is_deeper_than_terrain() {
    let mut sim = Sim::new(Profile::desktop(), 6);
    let pan = |t: f64| Camera {
        x: 0.5 * SIDE + 2_000.0 * t,
        heading: std::f64::consts::FRAC_PI_2,
        ..home()
    };
    sim.run(10.0, &pan);
    let stop = sim.now;
    sim.run(6.0, &|_| pan(10.0));
    sim.converged_after(stop).expect(RED);
    let last = sim.log.last().expect("steps");
    let deeper: u32 = last.stats.imagery_over_terrain[9..].iter().sum();
    assert!(deeper > 0, "no tile wears imagery deeper than its terrain");
    assert_eq!(last.stats.underdraped, 0);
}

pub fn back_and_forth(t: f64) -> Camera {
    let phase = (t / 8.0).floor() as u64 % 2;
    let k = (t % 8.0 / 4.0).min(1.0);
    let k = if phase == 0 { k } else { 1.0 - k };
    Camera {
        y: home().y + 40_000.0 * k,
        ..home()
    }
}

/// I14, B anti-patterns: going back and forth reuses what is held: the
/// returns cost a fraction of the first pass, and little is thrashed.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn back_and_forth_reuses_what_is_held() {
    let mut sim = Sim::new(Profile::desktop(), 7);
    sim.run(16.0, &back_and_forth);
    let first = sim.net.network_bytes;
    sim.run(32.0, &|t| back_and_forth(t + 16.0));
    let returns = sim.net.network_bytes - first;
    assert!(first > 0, "{RED}");
    assert!(
        returns * 4 < first,
        "two returns refetched {returns} bytes, first pass {first}"
    );
    let thrash: u32 = sim.log.iter().map(|l| l.stats.thrash).sum();
    assert!(thrash < 50, "{thrash} evict-then-request");
}

/// I8, I1: a memory warning mid-flight brings bytes under the new target
/// within a second, without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_memory_warning_mid_flight_trims_without_a_hole() {
    let mut sim = Sim::new(Profile::handheld(), 8);
    sim.run(8.0, &zoom);
    sim.pressure(Pressure::Warning);
    let at = sim.now;
    sim.run(4.0, &|t| zoom(t + 8.0));
    let after = sim
        .log
        .iter()
        .filter(|l| l.now >= at.plus(Micros::from_secs(1)))
        .collect::<Vec<_>>();
    assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    for l in after {
        for c in Class::ALL {
            let limit = l.stats.target[c].plus(l.stats.overflow[c]);
            assert!(
                l.stats.gpu[c] <= limit.max(l.stats.protected[c]),
                "{c:?} over after the warning"
            );
        }
    }
    assert!(sim
        .log
        .iter()
        .any(|l| l.stats.target[Class::Drape] < Profile::handheld().budget.target[Class::Drape]));
}

/// I8: thermal Serious halves uploads, without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn thermal_serious_halves_uploads_without_a_hole() {
    let mut sim = Sim::new(Profile::handheld(), 9);
    sim.thermal(Thermal::Serious);
    let cap = Profile::handheld()
        .budget
        .upload_bytes_per_frame
        .scaled(1, 2);
    let mut over = 0;
    for _ in 0..600 {
        let (_, d) = sim.step(home());
        if d.upload_plan.items.len() > 1 && d.upload_plan.bytes > cap {
            over += 1;
        }
    }
    assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    assert_eq!(over, 0);
}

/// I8, audit: backgrounding drops to the floor and the drawn set;
/// foregrounding refills without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn backgrounding_and_foregrounding_refill_without_a_hole() {
    let mut sim = Sim::new(Profile::handheld(), 10);
    // Converged first: the refill is what is measured.
    settle(&mut sim, 30.0);
    sim.pressure(Pressure::Background);
    sim.run(2.0, &|_| home());
    let bg = sim.log.last().expect("steps").stats.gpu;
    let total = bg.iter().fold(Bytes::ZERO, |a, (_, b)| a.plus(b));
    sim.pressure(Pressure::Normal);
    let fg = sim.now;
    sim.run(6.0, &|_| home());
    assert!(total > Bytes::ZERO, "{RED}");
    sim.converged_after(fg).expect("refilled");
}

/// D1, M2.7: a lossy network with stalled hosts never blocks the view.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_lossy_stalling_network_never_blocks_the_view() {
    let mut p = Profile::desktop();
    for s in [&mut p.net.meshes, &mut p.net.imagery] {
        s.fail_permille = 50;
        s.stall_permille = 20;
    }
    let mut sim = Sim::new(p, 11);
    let stop = settle(&mut sim, 30.0);
    let took = sim.converged_after(stop).expect(RED);
    assert!(took <= Micros::from_secs(20), "{took:?}");
}

/// The tracked resources holding nothing — no copy, nothing in transit, no
/// load — whose want went: the slots the prune keeps for their failure
/// history alone.
fn kept_for_their_failures(sim: &Sim) -> usize {
    sim.budget
        .snapshot()
        .resources
        .iter()
        .filter(|r| {
            r.gpu.is_none()
                && r.cpu.is_none()
                && r.transit.is_none()
                && r.load.is_none()
                && r.unneeded_since.is_some()
        })
        .count()
}

/// The camera of the outage scenario: the approach, then a jump to a new
/// place every two seconds, 94 km away, at the home view's height (a search
/// box's "go to": each one reveals ground the last place did not share, and
/// the loads of the place left have failed three or four times by then);
/// from 35 s the camera stays at the fifteenth place.
fn jumps(t: f64) -> Camera {
    if t < 5.0 {
        approach(t)
    } else {
        let k = ((t - 5.0) / 2.0).floor().min(14.0);
        Camera {
            x: (0.15 + 0.045 * k) * SIDE,
            ..home()
        }
    }
}

/// I7, D1: a network outage. Every network load fails at once for half a
/// minute (a refused connection) while the view jumps from place to place
/// (nothing arriving, so each place wants its coarse tiles and fails them
/// over and over), then the network is back and the camera stays at the
/// last place, whose tiles then load through their backoffs. Every wanted resource fails and backs off,
/// and the view leaves them behind: the budget keeps their failure history
/// for a while (so a server that is down is not hammered the moment a tile
/// comes back into view), but not without bound — the slots kept for
/// resources the view left are capped by count, and so by the bytes of
/// their metadata (`BACKOFF_KEEP_MAX`), the ones left longest let go first;
/// the wanted ones are the view's own. Once the network is back the view
/// converges, the backoffs of the wanted resources (30 s at most) included.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_network_outage_leaves_the_tracked_state_bounded_and_recovers() {
    let mut sim = Sim::new(Profile::desktop(), 21);
    let (from, to) = (Micros::from_secs(5), Micros::from_secs(35));
    sim.net.outage = Some((from, to));
    let mut report = Micros::from_secs(5);
    let (mut tracked_max, mut kept_max, mut failed) = (0usize, 0usize, 0u64);
    while sim.now < to {
        let t = sim.now.0 as f64 / 1e6;
        let (_, d) = sim.step(jumps(t));
        failed += u64::from(d.stats.failed);
        tracked_max = tracked_max.max(sim.budget.tracked());
        // The prune runs every eighth step: its cap holds right after one.
        if sim.budget.steps.is_multiple_of(8) {
            let kept = kept_for_their_failures(&sim);
            kept_max = kept_max.max(kept);
            assert!(
                kept <= BACKOFF_KEEP_MAX,
                "{RED}: {kept} slots kept for their failures alone at t = {t:.1} s, \
                 the cap {BACKOFF_KEEP_MAX}"
            );
        }
        if sim.now >= report {
            eprintln!(
                "outage t = {t:.0} s: tracked {}, drawable {}, kept for their failures {}, \
                 failed so far {failed}",
                sim.budget.tracked(),
                sim.gpu.drawable.len(),
                kept_for_their_failures(&sim)
            );
            report = report.plus(Micros::from_secs(5));
        }
    }
    let back = sim.now;
    sim.run(45.0, &|_| jumps(45.0));
    let took = sim.converged_after(back).expect(RED);
    eprintln!(
        "outage: tracked at most {tracked_max}, kept for their failures at most {kept_max} \
         (cap {BACKOFF_KEEP_MAX}, {} B each), {failed} loads failed; converged {took:?} after \
         the network came back, {} tracked at the end",
        tuile_budget::BACKOFF_KEPT_BYTES,
        sim.budget.tracked()
    );
    assert!(
        took <= Micros::from_secs(35),
        "{RED}: converged {took:?} after the network came back"
    );
}

/// I14, I15: a relaunch fills the view from the store, without network for
/// what the store holds.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_relaunch_fills_the_view_from_the_store_without_network() {
    let mut first = Sim::new(Profile::handheld(), 12);
    // The first session converges, so its store holds the view.
    settle(&mut first, 30.0);
    let store = first.net.store.clone();
    assert!(!store.held.is_empty(), "{RED}");
    let mut again = Sim::new(Profile::handheld(), 13);
    again.net.store = store;
    let start = again.now;
    again.run(6.0, &|_| home());
    // Nothing the store holds goes to the network (I14, checked every
    // step); what the first session never loaded (ring, other levels) may.
    let local: u32 = again.log.iter().map(|l| l.stats.started_local).sum();
    assert!(
        u64::from(local) > again.net.network_starts,
        "{local} local reads, {} network starts",
        again.net.network_starts
    );
    let took = again.converged_after(start).expect("converged");
    assert!(took <= Profile::handheld().converge_within, "{took:?}");
}

/// §4b: a lying store index costs one local miss per lie, then the network.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_lying_store_index_costs_one_miss_per_lie() {
    let mut first = Sim::new(Profile::desktop().clone(), 14);
    settle(&mut first, 6.0);
    let mut store = first.net.store.clone();
    let lies: Vec<_> = store.held.iter().copied().step_by(3).collect();
    for r in &lies {
        store.held.remove(r);
        store.lies.insert(*r);
    }
    store.generation += 1;
    let mut again = Sim::new(Profile::desktop(), 15);
    again.net.store = store;
    again.run(6.0, &|_| home());
    let misses: u32 = again.log.iter().map(|l| l.stats.store_misses).sum();
    assert!(misses > 0, "{RED}");
    assert!(misses as usize <= lies.len());
}

pub fn film_cam(frame: u64) -> Camera {
    let t = frame as f64 / 30.0;
    Camera {
        x: home().x + 300.0 * t,
        heading: 0.3 * t,
        ..home()
    }
}

/// I13: a 300-frame film under Batch is exact every frame.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_300_frame_film_is_exact_every_frame() {
    let mut sim = Sim::new(Profile::film(), 16);
    for f in 0..300 {
        let (drawn, _) = sim.film_frame(film_cam(f), 2_000);
        assert!(!drawn.is_empty());
        assert!(drawn.iter().all(|(_, drape)| drape.is_some()));
    }
}

fn film(
    seed: u64,
    net: NetProfile,
    frames: std::ops::Range<u64>,
) -> Vec<Vec<(tuile_budget::ResourceId, Option<tuile_budget::ResourceId>)>> {
    let mut p = Profile::film();
    p.net = net;
    let mut sim = Sim::new(p, seed);
    frames
        .map(|f| sim.film_frame(film_cam(f), 2_000).0)
        .collect()
}

/// I13: a film frame is a function of its inputs, not of the network.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_film_does_not_depend_on_the_network() {
    let fast = film(17, NetProfile::desktop().clean(), 0..60);
    let slow = film(18, NetProfile::handheld().clean(), 0..60);
    assert_eq!(fast, slow);
}

/// I13: nor on how frames are split across machines.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_film_does_not_depend_on_how_frames_are_split() {
    let whole = film(19, NetProfile::desktop().clean(), 0..120);
    let tail = film(20, NetProfile::desktop().clean(), 60..120);
    assert_eq!(whole[60..], tail[..]);
}

/// Golden decision digests per scenario: a change needs a reviewed update
/// (`TUILE_BLESS_BUDGET=1`). The simulation's inputs use no `libm`
/// (`detmath`), so the digests hold on every platform, the Linux CI included.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn golden_decision_digests_hold() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/budget_sim/goldens.txt");
    let mut got = Vec::new();
    for (name, seed, script) in [
        ("still", 21u64, &approach as &dyn Fn(f64) -> Camera),
        ("turn", 22, &turn),
        ("zoom", 23, &zoom),
        ("back-and-forth", 24, &back_and_forth),
    ] {
        let mut sim = Sim::new(Profile::desktop(), seed);
        sim.run(20.0, script);
        got.push(format!("{name} {:016x}", sim.run_digest()));
    }
    let got = got.join("\n") + "\n";
    if std::env::var_os("TUILE_BLESS_BUDGET").is_some() {
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).expect("goldens recorded");
    assert_eq!(got, want);
}

/// I1, I2, I9, review of point 9: at the hard ceiling of a small phone, a
/// zoom-out never deadlocks: the parents of the drawn cut stay resident (the
/// protected chain), so coarsening never waits on bytes it cannot get; no
/// class crosses its ceiling and nothing is lost (checked every step); the
/// budget settles, with nothing churning, at the best view it can hold.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_zoom_out_at_the_hard_ceiling_never_deadlocks() {
    let mut p = Profile::handheld_low();
    p.budget.hard_limit[Class::TerrainMesh] = Bytes::mib(40);
    p.budget.target[Class::TerrainMesh] = Bytes::mib(28);
    p.budget.pinned_level = Some(2);
    let mut sim = Sim::new(p, 25);
    let out = |t: f64| {
        let k = ((t - 8.0) / 3.0).clamp(0.0, 1.0);
        Camera {
            h: 2_000.0 * detmath::exp2(detmath::log2(25.0) * k),
            ..approach(t.min(5.0))
        }
    };
    let mut parents_resident = true;
    let mut strained = false;
    while sim.now < Micros::from_secs(11) {
        let t = sim.now.0 as f64 / 1e6;
        let (need, d) = sim.step(out(t));
        strained |= d.sse_scale_milli > 1_000;
        for (tile, n) in &need.drawn {
            let _ = tile;
            if let Some(p) = n.parent {
                parents_resident &= sim.budget.residency().drawable(p) || p.terrain_coord().0 == 0;
            }
        }
    }
    let stop = sim.now;
    sim.run(20.0, &out);
    assert!(strained, "{RED}: the ceiling was never reached");
    assert!(parents_resident, "a drawn tile's parent was not resident");
    let took = sim.settled_after(stop).expect("settled");
    assert!(took <= Micros::from_secs(10), "{took:?}");
}
