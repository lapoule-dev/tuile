// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One run of the budget headless over a [`Scenario`], and what it scored
//! (spec §1, §9.3): the objective `J` — the time-integrated deficit, in
//! level·seconds (the flouz of a reference pixel), plus the congestion
//! penalty — with **holes** counted by a tile-level coverage check of the
//! view's geometric footprint, independent of the selection (law 2b): a
//! footprint tile is covered when it is drawn, or a drawn ancestor covers
//! it, or drawn descendants partition it. A run with a hole once the floor
//! is in scores `J = ∞`.

use tuile_budget::{Decisions, Micros, Specification, TileId};

use crate::catalogue::{profile, Device, Network, Scenario, REST_SECONDS};
use crate::server::Sim;
use crate::world::{children, has_children, parent, sight, tile, View};

/// The footprint is checked at this level: tiles of a few kilometres, fine
/// enough that a gap at the edge of the view shows, coarse enough to walk
/// every step.
pub const FOOTPRINT_LEVEL: u32 = 8;

/// The congestion penalty: level·seconds per queued want·second.
pub const QUEUE_PENALTY_PERMILLE: u64 = 10;

/// What one phase of a run scored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhaseOutcome {
    /// The phase's name (the scenario's, when it has none).
    pub phase: &'static str,
    /// Footprint tiles uncovered, summed over the phase's steps.
    pub hole_tile_steps: u64,
    /// The worst step's uncovered footprint tiles.
    pub holes_max: u32,
    /// The deficit integrated over the phase, level·seconds.
    pub deficit_level_seconds: f64,
    /// The most GPU bytes held.
    pub bytes_max: u64,
    /// Loads started, and started for nothing (out of the need before
    /// they arrived).
    pub starts: u64,
    pub wasted_starts: u64,
}

/// What a run scored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// The scenario, the device, the network, the specification's kind.
    pub scenario: &'static str,
    pub device: &'static str,
    pub network: &'static str,
    pub spec: &'static str,
    /// The specification's query, for the record.
    pub query: String,
    /// The catalogue's version.
    pub catalogue: u32,
    /// Steps run.
    pub steps: u64,
    /// The whole run's phases (one, when the scenario has none).
    pub phases: Vec<PhaseOutcome>,
    /// The deficit integrated over the run, level·seconds.
    pub deficit_level_seconds: f64,
    /// Queued wants integrated over the run, want·seconds.
    pub queue_seconds: f64,
    /// Footprint tiles uncovered, summed over every step once the floor
    /// was in (must be 0).
    pub hole_tile_steps: u64,
    /// After the motion of a scenario that rests: seconds until no tile is
    /// under its need, and stays so (`None`: not within the rest).
    pub settle_seconds: Option<f64>,
    /// Network refetches of what was evicted less than 10 s before.
    pub thrash: u64,
    /// The objective: deficit + congestion, ∞ with a hole.
    pub j: f64,
    /// The run panicked (an invariant of the simulation broke): its
    /// message.
    pub broke: Option<String>,
}

/// Whether `t` is covered by the draw set for `views`: drawn, under a
/// drawn ancestor, or its visible part partitioned by drawn descendants
/// (`depth` levels down at most) — a child no eye sees needs nothing.
pub fn covered(d: &Decisions, views: &[View], t: TileId, depth: u32) -> bool {
    let mut a = Some(t);
    while let Some(x) = a {
        if d.draw.draws(x) {
            return true;
        }
        a = parent(x);
    }
    down(d, views, t, depth)
}

fn down(d: &Decisions, views: &[View], t: TileId, depth: u32) -> bool {
    if d.draw.draws(t) {
        return true;
    }
    depth > 0
        && has_children(t)
        && children(t)
            .iter()
            .filter(|c| views.iter().any(|v| sight(v, **c, 0.0).visible))
            .all(|c| down(d, views, *c, depth - 1))
}

/// The footprint tiles of `views` — every tile of [`FOOTPRINT_LEVEL`] (or a
/// leaf above it) an eye sees — from geometry alone, not from the need.
pub fn footprint(views: &[View]) -> Vec<TileId> {
    let mut out = Vec::new();
    let mut stack = vec![tile(0, 0, 0)];
    while let Some(t) = stack.pop() {
        if !views.iter().any(|v| sight(v, t, 0.0).visible) {
            continue;
        }
        if t.terrain_coord().0 >= FOOTPRINT_LEVEL || !has_children(t) {
            out.push(t);
        } else {
            stack.extend(children(t));
        }
    }
    out.sort_unstable();
    out
}

/// The uncovered footprint tiles of one step.
pub fn holes(d: &Decisions, views: &[View]) -> u32 {
    let n = footprint(views)
        .into_iter()
        .filter(|t| !covered(d, views, *t, 16))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Runs `scenario` on `device` and `network` under `spec`, seeded by
/// `seed`, and scores it. The holes are measured against the present
/// camera with the draw set of `lag` steps before: an asynchronous
/// geometry server's selection arrives late, and what the executor draws is
/// the last one published (0: the step's own). A broken invariant of the
/// simulation is caught and scored `J = ∞`.
pub fn run(
    scenario: &Scenario,
    device: Device,
    network: Network,
    spec: &Specification,
    seed: u64,
    lag: usize,
) -> Outcome {
    let mut out = Outcome {
        scenario: scenario.name,
        device: device.label(),
        network: network.label(),
        spec: spec.kind.label(),
        query: spec.to_query(),
        catalogue: crate::catalogue::CATALOGUE_VERSION,
        ..Outcome::default()
    };
    let s = *scenario;
    let spec = spec.clone();
    let result = std::panic::catch_unwind(move || score(&s, device, network, &spec, seed, lag));
    match result {
        Ok(scored) => Outcome {
            scenario: out.scenario,
            device: out.device,
            network: out.network,
            spec: out.spec,
            query: std::mem::take(&mut out.query),
            catalogue: out.catalogue,
            ..scored
        },
        Err(e) => {
            out.broke = Some(
                e.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "panicked".to_owned()),
            );
            out.j = f64::INFINITY;
            out
        }
    }
}

fn score(
    scenario: &Scenario,
    device: Device,
    network: Network,
    spec: &Specification,
    seed: u64,
    lag: usize,
) -> Outcome {
    let mut drawn: std::collections::VecDeque<Decisions> = std::collections::VecDeque::new();
    let mut sim = Sim::unchecked(profile(device, network, scenario.headset), seed);
    sim.city = scenario.city;
    sim.budget
        .specify(spec.clone())
        .expect("a valid specification");
    let dt = sim.profile.dt;
    let end_move = Micros((scenario.seconds * 1e6) as u64);
    let end = if scenario.rests {
        end_move.plus(Micros((REST_SECONDS * 1e6) as u64))
    } else {
        end_move
    };
    let mut o = Outcome::default();
    let phases: Vec<(&'static str, f64)> = if scenario.phases.is_empty() {
        vec![(scenario.name, f64::INFINITY)]
    } else {
        scenario.phases.to_vec()
    };
    o.phases = phases
        .iter()
        .map(|(name, _)| PhaseOutcome {
            phase: name,
            ..PhaseOutcome::default()
        })
        .collect();
    let mut floor_in = false;
    let mut settled_at: Option<Micros> = None;
    let secs = dt.0 as f64 / 1e6;
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        let cam = (scenario.path)(t.min(scenario.seconds));
        let views = sim.views(cam);
        let (_, d) = sim.step(cam);
        o.steps += 1;
        drawn.push_back(d.clone());
        if drawn.len() > lag + 1 {
            drawn.pop_front();
        }
        let shown = drawn.front().unwrap_or(&d);
        let st = &d.stats;
        floor_in |= st.floor_in;
        let under = st.deficit.under_levels();
        let h = if floor_in { holes(shown, &views) } else { 0 };
        let k = phases
            .iter()
            .position(|(_, until)| t < *until)
            .unwrap_or(phases.len() - 1);
        let p = &mut o.phases[k];
        p.hole_tile_steps += u64::from(h);
        p.holes_max = p.holes_max.max(h);
        p.deficit_level_seconds += f64::from(under) * secs;
        p.bytes_max = p
            .bytes_max
            .max(st.gpu.iter().map(|(_, b)| b.0).sum::<u64>());
        p.starts += u64::from(st.started_network + st.started_local);
        p.wasted_starts += st.wasted_starts.iter().map(|(_, n)| u64::from(n)).sum::<u64>();
        o.hole_tile_steps += u64::from(h);
        o.deficit_level_seconds += f64::from(under) * secs;
        o.queue_seconds += f64::from(st.heap_len) * secs;
        o.thrash += u64::from(st.thrash);
        if sim.now >= end_move {
            match (st.deficit.under_tiles() == 0, settled_at) {
                (true, None) => settled_at = Some(sim.now),
                (false, Some(_)) => settled_at = None,
                _ => {}
            }
        }
    }
    o.settle_seconds = settled_at.map(|at| at.since(end_move).0 as f64 / 1e6);
    o.j = if o.hole_tile_steps > 0 {
        f64::INFINITY
    } else {
        o.deficit_level_seconds + o.queue_seconds * QUEUE_PENALTY_PERMILLE as f64 / 1_000.0
    };
    o
}
