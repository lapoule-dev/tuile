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
use tuile_camera::predict::{MotionModel, PredictConfig};

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

/// The views ahead a run sends (`ClientMessage::Ahead`).
#[derive(Clone, Debug, PartialEq)]
pub enum Ahead {
    /// None: the need without a prediction.
    Nothing,
    /// The scenario's own path at the configuration's horizons, as a glide
    /// or a coast is known: exact, its probability decaying as a hand may
    /// interrupt it.
    Known(PredictConfig),
    /// The camera's motion model (`tuile_camera::predict`), fed with the
    /// poses of the steps so far: an interactive hand.
    Model(PredictConfig),
}

/// The motion model's error at one horizon over a run: the predicted pose
/// against the scenario's at that time, while the camera moves.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HorizonError {
    pub horizon_s: f64,
    /// Predictions scored.
    pub n: usize,
    /// Ground distance, km: median and 90th percentile.
    pub ground_km: [f64; 2],
    /// Heading, degrees.
    pub heading_deg: [f64; 2],
    /// Pitch, degrees.
    pub pitch_deg: [f64; 2],
    /// `max(a/b, b/a)` of the heights.
    pub height_ratio: [f64; 2],
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
    /// Under-need tile·steps: the deficit's tiles summed over every step.
    pub under_tile_steps: u64,
    /// The motion model's error per horizon ([`Ahead::Model`] only).
    pub ahead_errors: Vec<HorizonError>,
    /// The budget's work, summed over the steps and at its peak.
    pub work_sum: u64,
    pub work_max: u32,
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
    run_ahead(scenario, device, network, spec, seed, lag, &Ahead::Nothing)
}

/// [`run`], with the views ahead `ahead` sent before each step.
pub fn run_ahead(
    scenario: &Scenario,
    device: Device,
    network: Network,
    spec: &Specification,
    seed: u64,
    lag: usize,
    ahead: &Ahead,
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
    let ahead = ahead.clone();
    let result =
        std::panic::catch_unwind(move || score(&s, device, network, &spec, seed, lag, &ahead));
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
    ahead: &Ahead,
) -> Outcome {
    let mut model = match ahead {
        Ahead::Model(c) => Some(MotionModel::new(c.clone())),
        _ => None,
    };
    let mut errors: Vec<Vec<[f64; 4]>> = match ahead {
        Ahead::Model(c) => vec![Vec::new(); c.horizons_s.len()],
        _ => Vec::new(),
    };
    let mut heading_unwrapped: Option<(f64, f64)> = None;
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
        let path = |at: f64| (scenario.path)(at.min(scenario.seconds));
        sim.ahead = match ahead {
            Ahead::Nothing => Vec::new(),
            Ahead::Known(c) => c
                .horizons_s
                .iter()
                .map(|tau| (path(t + tau), micros(*tau), permille(*tau, c.interrupt_s)))
                .collect(),
            Ahead::Model(c) => {
                let m = model.as_mut().expect("a model");
                // The heading unwrapped, as the model wants it.
                let h = match heading_unwrapped {
                    None => cam.heading,
                    Some((raw, un)) => un + wrap_pi(cam.heading - raw),
                };
                heading_unwrapped = Some((cam.heading, h));
                m.observe_state(t, [cam.x, cam.y, cam.h.max(1.0).ln(), h, cam.pitch]);
                let mut out = Vec::new();
                if m.moving() {
                    for (k, tau) in c.horizons_s.iter().enumerate() {
                        let Some((z, _)) = m.predict_state(*tau) else {
                            continue;
                        };
                        let predicted = crate::world::Camera {
                            x: z[0],
                            y: z[1],
                            h: z[2].exp(),
                            heading: z[3],
                            pitch: z[4],
                        };
                        if t + tau <= scenario.seconds {
                            let truth = path(t + tau);
                            errors[k].push([
                                (predicted.x - truth.x).hypot(predicted.y - truth.y) / 1000.0,
                                wrap_pi(predicted.heading - truth.heading)
                                    .abs()
                                    .to_degrees(),
                                (predicted.pitch - truth.pitch).abs().to_degrees(),
                                (predicted.h.max(1.0) / truth.h.max(1.0)).ln().abs().exp(),
                            ]);
                        }
                        out.push((predicted, micros(*tau), permille(*tau, c.laplace_s)));
                    }
                }
                out
            }
        };
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
        o.under_tile_steps += u64::from(st.deficit.under_tiles());
        o.work_sum += u64::from(st.work);
        o.work_max = o.work_max.max(st.work);
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
        p.wasted_starts += st
            .wasted_starts
            .iter()
            .map(|(_, n)| u64::from(n))
            .sum::<u64>();
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
    if let Ahead::Model(c) = ahead {
        o.ahead_errors = c
            .horizons_s
            .iter()
            .zip(&mut errors)
            .map(|(h, e)| {
                let q = |e: &mut Vec<[f64; 4]>, i: usize| -> [f64; 2] {
                    if e.is_empty() {
                        return [f64::NAN; 2];
                    }
                    let mut v: Vec<f64> = e.iter().map(|x| x[i]).collect();
                    v.sort_by(f64::total_cmp);
                    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
                    [at(0.5), at(0.9)]
                };
                HorizonError {
                    horizon_s: *h,
                    n: e.len(),
                    ground_km: q(e, 0),
                    heading_deg: q(e, 1),
                    pitch_deg: q(e, 2),
                    height_ratio: q(e, 3),
                }
            })
            .collect();
    }
    o.settle_seconds = settled_at.map(|at| at.since(end_move).0 as f64 / 1e6);
    o.j = if o.hole_tile_steps > 0 {
        f64::INFINITY
    } else {
        o.deficit_level_seconds + o.queue_seconds * QUEUE_PENALTY_PERMILLE as f64 / 1_000.0
    };
    o
}

/// Seconds as microseconds.
fn micros(s: f64) -> Micros {
    Micros((s * 1e6).round() as u64)
}

/// `e^(−τ/scale)` in thousandths.
fn permille(tau: f64, scale: f64) -> u16 {
    if scale <= 0.0 {
        return 0;
    }
    ((-tau / scale).exp() * 1000.0).round().clamp(0.0, 1000.0) as u16
}

/// An angle folded into `[−π, π]`.
fn wrap_pi(a: f64) -> f64 {
    (a + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
}
