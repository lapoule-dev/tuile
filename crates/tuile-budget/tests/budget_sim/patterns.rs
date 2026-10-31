// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The motion patterns of spec §12, scenario by scenario: each test is a
//! camera script and a profile, and asserts its pattern's obligations
//! beyond the invariants `Sim::check` asserts at every step.

use std::collections::{BTreeMap, BTreeSet};

use tuile_budget::{Class, Decisions, Micros, NeedSet, ResourceId, Source, TileId};

use super::scenarios::{approach, home, settle};
use super::server::{Profile, Sim};
use super::world::{seen, Camera, Lens, CULL_MARGIN};

const RED: &str = "the budget core regressed";

/// No host stalls: convergence is measurable (stalls are the lossy
/// scenario's); failures are still retried.
pub fn quiet(mut p: Profile) -> Profile {
    for s in [&mut p.net.meshes, &mut p.net.imagery] {
        s.stall_permille = 0;
    }
    p
}

/// Every tile a need lists.
pub fn listed(need: &NeedSet) -> BTreeSet<TileId> {
    need.entries().map(|(_, t, _)| t).collect()
}

/// Steps `secs` seconds with the camera at `cam`, recording each step.
pub fn record(sim: &mut Sim, secs: f64, cam: Camera) -> Vec<(NeedSet, Decisions)> {
    let end = sim.now.plus(Micros((secs * 1e6) as u64));
    let mut out = Vec::new();
    while sim.now < end {
        out.push(sim.step(cam));
    }
    out
}

/// How long after `since` the view first converged.
pub fn converged_within(sim: &Sim, since: Micros) -> Micros {
    sim.converged_after(since)
        .unwrap_or_else(|| panic!("{}: never converged after {since:?}", sim.profile.name))
}

/// P5.1: across a lens change, nothing the last step drew whose tile the
/// new views still see is evicted at the change (the old draw set stays
/// until the new frustum is covered). I1 against the new lens is
/// `Sim::check`'s, every step.
fn keeps_what_is_still_seen(sim: &Sim, before: &Decisions, at: &Decisions) {
    let drawn: BTreeSet<ResourceId> = before
        .draw
        .items
        .iter()
        .flat_map(|i| std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource)))
        .collect();
    for e in &at.evict {
        // The old draw set stays until the new frustum is covered: a tile
        // drawn and still in view goes at the change only if its ground is
        // covered by the new draw set (a coarser parent, finer children).
        let gone = drawn.contains(e) && seen(&sim.last_views, e.tile(), CULL_MARGIN);
        assert!(
            !gone || covered_by(at, e.tile()),
            "P5.1: {e:?}, drawn and still in view, evicted at the change, its ground uncovered"
        );
    }
}

/// Whether `t`'s ground is drawn at `d`: by `t`, an ancestor, or all its
/// children (three levels down).
fn covered_by(d: &Decisions, t: TileId) -> bool {
    use super::world::{children, parent};
    let mut up = Some(t);
    while let Some(x) = up {
        if d.draw.draws(x) {
            return true;
        }
        up = parent(x);
    }
    fn down(d: &Decisions, t: TileId, depth: u32) -> bool {
        depth > 0
            && super::world::children(t)
                .iter()
                .all(|&k| d.draw.draws(k) || down(d, k, depth - 1))
    }
    let _ = children;
    down(d, t, 3)
}

fn rotation(profile: Profile, seed: u64) {
    let within = profile.converge_within;
    let mut sim = Sim::new(quiet(profile), seed);
    settle(&mut sim, 8.0);
    for lens in [Lens::landscape(), Lens::portrait()] {
        let before = sim.step(home()).1;
        sim.set_lens(lens);
        let change = sim.now;
        let (_, at) = sim.step(home());
        keeps_what_is_still_seen(&sim, &before, &at);
        sim.run(8.0, &|_| home());
        let took = converged_within(&sim, change);
        assert!(
            took <= within,
            "P5.6, {}: converged {took:?} after the rotation, bound {within:?}",
            sim.profile.name
        );
    }
}

/// P5.1, P5.6: a phone turned to landscape and back never shows a hole,
/// keeps what it still sees, and converges within its bound.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_portrait_landscape_rotation_never_shows_a_hole() {
    rotation(Profile::handheld(), 31);
}

/// P5.1, P5.6, at the handheld's low clamp.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_portrait_landscape_rotation_never_shows_a_hole_at_the_low_clamp() {
    rotation(Profile::handheld_low(), 32);
}

/// Ten rotations two seconds apart, after a first cycle that converged in
/// both orientations. Returns the simulation, what the first cycle started
/// and when the ten rotations began.
fn ten_rotations(profile: Profile, seed: u64, store: bool) -> (Sim, BTreeSet<ResourceId>, Micros) {
    let mut sim = Sim::new(quiet(profile), seed);
    sim.net.store_fills = store;
    settle(&mut sim, 8.0);
    let mut first = BTreeSet::new();
    for lens in [Lens::landscape(), Lens::portrait()] {
        sim.set_lens(lens);
        // What the orientation needed once converged: its draw set, meshes
        // and drapes.
        let (_, d) = record(&mut sim, 8.0, home()).pop().expect("steps");
        assert!(
            sim.log.last().is_some_and(|l| l.converged),
            "{RED}: the first cycle did not converge"
        );
        for i in &d.draw.items {
            first.insert(i.mesh);
            first.extend(i.drapes.iter().map(|x| x.resource));
        }
    }
    let from = sim.now;
    for k in 0..10 {
        sim.set_lens(if k % 2 == 0 {
            Lens::landscape()
        } else {
            Lens::portrait()
        });
        sim.run(2.0, &|_| home());
    }
    (sim, first, from)
}

/// The ten rotations, with every start they made.
fn ten_rotations_starts(
    profile: Profile,
    seed: u64,
) -> (Sim, BTreeSet<ResourceId>, Vec<tuile_budget::Start>) {
    let mut sim = Sim::new(quiet(profile), seed);
    settle(&mut sim, 8.0);
    let mut first = BTreeSet::new();
    for lens in [Lens::landscape(), Lens::portrait()] {
        sim.set_lens(lens);
        // What the orientation needed once converged: its draw set, meshes
        // and drapes.
        let (_, d) = record(&mut sim, 8.0, home()).pop().expect("steps");
        assert!(
            sim.log.last().is_some_and(|l| l.converged),
            "{RED}: the first cycle did not converge"
        );
        for i in &d.draw.items {
            first.insert(i.mesh);
            first.extend(i.drapes.iter().map(|x| x.resource));
        }
    }
    let mut starts = Vec::new();
    for k in 0..10 {
        sim.set_lens(if k % 2 == 0 {
            Lens::landscape()
        } else {
            Lens::portrait()
        });
        for (_, d) in record(&mut sim, 2.0, home()) {
            starts.extend(d.start.iter().copied());
        }
    }
    (sim, first, starts)
}

/// P5.4: once a first cycle has converged in both orientations, ten
/// rotations start no network load: a local store holds what either
/// orientation needed.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn ten_rotations_fetch_nothing_after_the_first() {
    for (profile, seed) in [(Profile::handheld(), 33), (Profile::handheld_low(), 34)] {
        let name = profile.name;
        let (sim, first, starts) = ten_rotations_starts(profile, seed);
        // What either orientation needed in the first cycle never goes to
        // the network again; what neither did (a level the relaxation
        // reaches only later) may.
        let again: Vec<_> = starts
            .iter()
            .filter(|s| s.source == Source::Network && first.contains(&s.resource))
            .collect();
        assert!(!first.is_empty(), "{RED}");
        let _ = sim;
        assert!(
            again.is_empty(),
            "P5.4, {name}: {} network loads of what the first cycle needed",
            again.len()
        );
    }
}

/// P5.4 with no local store: the handheld holds both orientations, so the
/// rotations thrash nothing (no network fetch of what it evicted less than
/// ten seconds before).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn ten_rotations_without_a_store_thrash_nothing() {
    let (sim, first, from) = ten_rotations(Profile::handheld(), 35, false);
    let thrash: u32 = sim
        .log
        .iter()
        .filter(|l| l.now >= from)
        .map(|l| l.stats.thrash)
        .sum();
    assert!(!first.is_empty(), "{RED}");
    assert_eq!(
        thrash, 0,
        "P5.4: {thrash} refetched within 10 s of their eviction"
    );
}

/// P5.2: a resize decides on the new viewport from the step it lands: the
/// need of that step is the new lens's — narrower, nothing outside the new
/// frustum; larger at the same angles, finer.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_resize_decides_on_the_new_viewport_the_same_step() {
    let mut sim = Sim::new(quiet(Profile::desktop()), 36);
    settle(&mut sim, 6.0);
    let (wide, _) = sim.step(home());
    sim.set_lens(Lens::desktop().narrowed(1.0, 2.0));
    let (narrow, _) = sim.step(home());
    for (t, _) in &narrow.drawn {
        assert!(
            seen(&sim.last_views, *t, CULL_MARGIN),
            "P5.2: {t:?} wanted outside the new frustum"
        );
    }
    let gone = listed(&wide).difference(&listed(&narrow)).count();
    assert!(
        gone > 0,
        "P5.2: the narrower view wants all the wider one did"
    );
    sim.run(6.0, &|_| home());
    // 3840 × 2160 at scale 1: the same angles, twice the focal.
    let (before, _) = sim.step(home());
    sim.set_lens(Lens::horizontal(3840.0, 2160.0, 1.0, 60.0));
    let (after, _) = sim.step(home());
    // Finer: more of the ground refined, each texel wanted twice as sharp.
    let sharpest = |n: &NeedSet| {
        n.drawn
            .iter()
            .filter_map(|(_, x)| x.imagery.as_ref().map(|i| i.footprint.ratio()))
            .fold(0.0, f64::max)
    };
    assert!(
        after.drawn.len() > before.drawn.len(),
        "P5.2: twice the focal, no more tiles wanted ({} → {})",
        before.drawn.len(),
        after.drawn.len()
    );
    assert!(
        sharpest(&after) > 1.9 * sharpest(&before),
        "P5.2: the footprint"
    );
}

fn resize_starts(profile: Profile, seed: u64, lens: Lens) {
    let within = profile.converge_within;
    let mut sim = Sim::new(quiet(profile), seed);
    settle(&mut sim, 8.0);
    sim.set_lens(lens);
    let mut started: BTreeMap<ResourceId, u32> = BTreeMap::new();
    let steps = (within.0 / sim.profile.dt.0) as usize;
    for _ in 0..steps {
        let (need, d) = sim.step(home());
        let wanted = listed(&need);
        for s in &d.start {
            assert!(
                wanted.contains(&s.resource.tile()),
                "P5.3: {:?} started outside the new need",
                s.resource
            );
            *started.entry(s.resource).or_default() += 1;
        }
    }
    let failed: u32 = sim
        .log
        .iter()
        .rev()
        .take(steps)
        .map(|l| l.stats.failed + l.stats.cancelled)
        .sum();
    let twice = started.values().filter(|n| **n > 1).count() as u32;
    assert!(
        twice <= failed,
        "P5.3: {twice} resources started twice, {failed} failures or cancels"
    );
    assert!(!started.is_empty(), "{RED}: the change asked for nothing");
}

/// P5.3: after a resize, every start is in the new need and none is
/// repeated without a failure in between; I14, nothing held fetched again,
/// is `Sim::check`'s.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_resize_starts_nothing_outside_the_new_need() {
    resize_starts(
        Profile::desktop(),
        37,
        Lens::horizontal(3840.0, 2160.0, 1.0, 60.0),
    );
    resize_starts(Profile::handheld(), 38, Lens::landscape());
}

/// P5.6, P3.2: a split screen (full width, then a half, then a third)
/// brings every class under its target within the bound, and under
/// `max(target + overflow, protected)` before, without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_split_screen_shrink_comes_back_under_target_without_a_hole() {
    let profile = quiet(Profile::handheld());
    let within = profile.converge_within;
    let mut sim = Sim::new(profile, 39);
    settle(&mut sim, 8.0);
    for den in [2.0, 3.0] {
        sim.set_lens(Lens::portrait().narrowed(1.0, den));
        let change = sim.now;
        sim.run(8.0, &|_| home());
        for l in sim.log.iter().filter(|l| l.now >= change) {
            let s = &l.stats;
            for c in Class::ALL {
                let bound = if l.now >= change.plus(within) {
                    s.target[c].max(s.protected[c])
                } else {
                    s.target[c].plus(s.overflow[c]).max(s.protected[c])
                };
                assert!(
                    s.gpu[c] <= bound,
                    "P3.2 after a split to 1/{den}: {c:?} at {} MiB, bound {} MiB",
                    s.gpu[c].0 >> 20,
                    bound.0 >> 20
                );
            }
        }
        converged_within(&sim, change);
    }
}

fn scale_digests(change: bool, seed: u64) -> Vec<u64> {
    let mut sim = Sim::new(quiet(Profile::desktop()), seed);
    sim.run(6.0, &approach);
    if change {
        sim.set_lens(Lens::desktop().at_scale(2.0));
    }
    sim.run(3.0, &|_| home());
    if change {
        sim.set_lens(Lens::desktop());
    }
    sim.run(3.0, &|_| home());
    sim.log.iter().map(|l| l.digest).collect()
}

/// P5.5, D-L1: a scale change at constant logical size — a window moved
/// to a 2× display and back — decides nothing new: the decisions are the
/// same, step for step, bit for bit.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_scale_change_alone_decides_nothing_new() {
    let still = scale_digests(false, 40);
    let moved = scale_digests(true, 40);
    assert_eq!(still.len(), moved.len());
    let first = still.iter().zip(&moved).position(|(a, b)| a != b);
    assert_eq!(first, None, "P5.5: the decisions moved at step {first:?}");
    assert!(Lens::desktop().at_scale(2.0) != Lens::desktop(), "{RED}");
}

fn cold_start_bare(profile: Profile, seed: u64) -> u32 {
    let mut sim = Sim::new(quiet(profile), seed);
    sim.run(6.0, &|_| home());
    sim.log.iter().map(|l| l.stats.bare).sum()
}

/// I1 (D-A1), P1.1: a cold start never draws a mesh without the drapes it
/// needs — not one item, not one step, from the first (the architecture
/// experiment counted 4 to 8 bare tile-steps at start-up, every variant).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_cold_start_draws_no_bare_ground() {
    for (profile, seed) in [
        (Profile::desktop(), 41),
        (Profile::handheld(), 42),
        (Profile::handheld_low(), 43),
        (Profile::headset(90), 44),
    ] {
        let name = profile.name;
        let bare = cold_start_bare(profile, seed);
        assert_eq!(bare, 0, "{name}: {bare} bare item-steps at start-up");
    }
}

// P6 — stereo, and the display's deadline.

const DEG: f64 = std::f64::consts::PI / 180.0;

fn sin(x: f64) -> f64 {
    super::detmath::sin_cos(x).0
}

/// A head held still, with sensor jitter: ±1 mm, ±0.05°.
pub fn jitter(t: f64) -> Camera {
    let tau = std::f64::consts::TAU;
    Camera {
        x: home().x + 0.001 * sin(tau * 7.3 * t),
        y: home().y + 0.001 * sin(tau * 5.1 * t),
        heading: home().heading + 0.05 * DEG * sin(tau * 3.7 * t),
        pitch: home().pitch + 0.05 * DEG * sin(tau * 6.1 * t),
        ..home()
    }
}

/// Looking around: yaw ±30° at 0.5 Hz.
pub fn look_around(t: f64) -> Camera {
    Camera {
        heading: 30.0 * DEG * sin(std::f64::consts::TAU * 0.5 * t),
        ..home()
    }
}

/// Flicks at head rate: 90° in 0.3 s (300°/s), a second's hold, and back.
pub fn flicks(t: f64) -> Camera {
    let u = t % 2.6;
    let yaw = if u < 0.3 {
        300.0 * u
    } else if u < 1.3 {
        90.0
    } else if u < 1.6 {
        90.0 - 300.0 * (u - 1.3)
    } else {
        0.0
    };
    Camera {
        heading: yaw * DEG,
        ..home()
    }
}

/// Walking at 0.5 m/s, two metres above the ground, looking 30° down.
pub fn walking(t: f64) -> Camera {
    Camera {
        x: home().x + 0.5 * t,
        h: 2.0,
        ..home()
    }
}

/// A headset session: jitter, looking around, flicks, then a walk.
fn head(profile: Profile, seed: u64) -> Sim {
    let mut sim = Sim::new(quiet(profile), seed);
    sim.run(4.0, &jitter);
    sim.run(4.0, &look_around);
    sim.run(3.0, &flicks);
    sim.run(4.0, &walking);
    sim
}

/// I17, P6.2: a stereo head — still, looking around, flicking at 300°/s,
/// walking — never shows a hole in either eye, at 72, 90 and 120 Hz. Each
/// eye's frustum is checked against the one draw set every step, and every
/// eye draws the step's draw set (`Sim::check`, `Sim::execute`).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_stereo_head_never_shows_a_hole_in_either_eye() {
    for (hz, seed) in [(72, 51), (90, 52), (120, 53)] {
        let sim = head(Profile::headset(hz), seed);
        assert!(
            sim.floor_at.is_some(),
            "{RED}: {hz} Hz, the floor never came in"
        );
        assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    }
}

fn session_bytes(profile: Profile, seed: u64, script: &dyn Fn(f64) -> Camera) -> (Vec<u64>, u64) {
    let mut sim = Sim::new(quiet(profile), seed);
    sim.run(6.0, script);
    (
        sim.log.iter().map(|l| l.digest).collect(),
        sim.net.network_bytes,
    )
}

/// I16, P6.1: a stereo pair costs its union. Two eyes at the same place
/// through the same lens decide what one eye does, step for step, bit for
/// bit; a 63 mm pair costs little more than either of its eyes alone, and
/// never as much as both; and every step of every run the budget charges
/// what the executor holds, each resource once (`Sim::check_executor`).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_stereo_pair_costs_its_union() {
    use super::server::Eye;
    let with = |eyes: Vec<Eye>| {
        let mut p = Profile::headset(90);
        p.eyes = eyes;
        p
    };
    let lens = Lens::headset_left();
    let twin = Eye {
        offset_m: 0.0,
        lens,
    };
    let (one, _) = session_bytes(with(vec![twin]), 54, &look_around);
    let (two, _) = session_bytes(with(vec![twin, twin]), 54, &look_around);
    let first = one.iter().zip(&two).position(|(a, b)| a != b);
    assert_eq!(
        first, None,
        "I16: two identical eyes decided otherwise at step {first:?}"
    );
    let (_, pair) = session_bytes(Profile::headset(90), 55, &look_around);
    let (_, left) = session_bytes(with(Eye::mono(lens)), 55, &look_around);
    let (_, right) = session_bytes(with(Eye::mono(lens.mirrored())), 55, &look_around);
    assert!(left > 0 && right > 0, "{RED}");
    assert!(
        pair < left + right,
        "I16: the pair fetched {pair} bytes, its eyes alone {left} and {right}"
    );
    assert!(
        pair * 4 <= 5 * left.max(right),
        "I16: the pair fetched {pair} bytes, more than a quarter over its larger eye ({})",
        left.max(right)
    );
}

/// I16, I17, P6.1, P6.2 on a desktop with two eyes far apart
/// (hyperstereo: 6.3 m, then 630 m): the union is charged once, never as
/// much as both eyes alone, and covers both eyes every step.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn hyperstereo_costs_its_union_and_covers_both_eyes() {
    use super::server::Eye;
    for (ipd, seed) in [(6.3, 56), (630.0, 57)] {
        let mut pair = Profile::desktop();
        pair.eyes = Eye::pair(ipd, Lens::desktop());
        let (_, both) = session_bytes(pair, seed, &approach);
        let alone = |offset_m: f64| {
            let mut p = Profile::desktop();
            p.eyes = vec![Eye {
                offset_m,
                lens: Lens::desktop(),
            }];
            session_bytes(p, seed, &approach).1
        };
        let (left, right) = (alone(-ipd / 2.0), alone(ipd / 2.0));
        assert!(
            both < left + right,
            "I16 at {ipd} m: the pair fetched {both} bytes, its eyes alone {left} and {right}"
        );
    }
}

/// I18, P6.3: at 72, 90 and 120 Hz every render frame uploads at most its
/// deadline, but for a lone first item (`Sim::execute`, every frame); the
/// deadline does bind (frames defer part of the plan); and the ground stays
/// covered whatever is deferred.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn uploads_fit_the_refresh_deadline_at_72_90_and_120_hz() {
    for (hz, seed) in [(72, 58), (90, 59), (120, 60)] {
        let mut sim = Sim::new(quiet(Profile::headset(hz)), seed);
        sim.run(6.0, &look_around);
        assert!(
            sim.gpu.deferred > 0,
            "{hz} Hz: the deadline never bound, the test proves nothing"
        );
        assert!(sim.floor_at.is_some(), "{RED}: {hz} Hz");
    }
}

/// P6.5: a stereo head converges within the headset's bound (2 s) of
/// stopping after looking around.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_stereo_head_converges_after_it_stops() {
    let profile = quiet(Profile::headset(90));
    let within = profile.converge_within;
    let mut sim = Sim::new(profile, 61);
    settle(&mut sim, 6.0);
    sim.run(4.0, &look_around);
    let stop = sim.now;
    sim.run(6.0, &|_| home());
    let took = converged_within(&sim, stop);
    assert!(
        took <= within,
        "P6.5: converged {took:?} after the head stopped"
    );
}

/// I13, I16: a stereo film (two eyes 63 mm apart) is exact in both eyes:
/// each frame drawn only complete, every item dressed, both eyes covered
/// (`Sim::check`).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_stereo_film_is_exact_in_both_eyes() {
    use super::server::Eye;
    let mut p = Profile::film();
    p.eyes = Eye::pair(0.063, Lens::desktop());
    let mut sim = Sim::new(p, 62);
    for f in 0..60 {
        let (drawn, _) = sim.film_frame(super::scenarios::film_cam(f), 2_000);
        assert!(!drawn.is_empty(), "{RED}");
        assert!(
            drawn.iter().all(|(_, drape)| drape.is_some()),
            "frame {f} bare"
        );
    }
}

/// P1.3 on the web: the budget a message away from its executor converges
/// within the desktop's bound plus one step.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_web_cold_start_converges_one_message_late() {
    super::scenarios::converges(Profile::web());
}

/// P5.2 and P5.5 on the web: a resize decides on the new canvas the same
/// step, and a `devicePixelRatio` change alone decides nothing new, with
/// the executor a step behind.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_web_resize_and_a_device_pixel_ratio_change() {
    let mut sim = Sim::new(quiet(Profile::web()), 63);
    settle(&mut sim, 6.0);
    let (wide, _) = sim.step(home());
    sim.set_lens(Lens::horizontal(2560.0, 1440.0, 2.0, 60.0).narrowed(1.0, 2.0));
    let (narrow, _) = sim.step(home());
    for (t, _) in &narrow.drawn {
        assert!(seen(&sim.last_views, *t, CULL_MARGIN), "P5.2: {t:?}");
    }
    assert!(
        listed(&wide).difference(&listed(&narrow)).count() > 0,
        "P5.2"
    );
    let digests = |change: bool| {
        let mut sim = Sim::new(quiet(Profile::web()), 64);
        sim.run(6.0, &approach);
        if change {
            sim.set_lens(Lens::horizontal(2560.0, 1440.0, 2.0, 60.0).at_scale(1.0));
        }
        sim.run(3.0, &|_| home());
        sim.log.iter().map(|l| l.digest).collect::<Vec<_>>()
    };
    assert_eq!(digests(false), digests(true), "P5.5 on the web");
}
