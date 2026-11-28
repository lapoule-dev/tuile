// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The regulator's measurements on Laurent's acceptance sequence
//! (`docs/explorations/budget-regulator.md`): the SASO criteria — overshoot,
//! settling in frames, the eviction-rate peak, the upload peak per frame —
//! and how often a mesh is drawn or held without its own drape (the mesh +
//! drape law), per profile.
//!
//! The measurement runs print a table (`--nocapture`); the gates assert the
//! bounds the regulator must hold.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;

use tuile_budget::{Micros, ResourceId, Role, Specification};
use tuile_budget_sim::catalogue::{named, REST_SECONDS};
use tuile_budget_sim::{Profile, Sim};

/// What one run measured.
#[derive(Clone, Debug, Default)]
pub struct Saso {
    pub profile: &'static str,
    pub steps: u64,
    /// The worst step's GPU bytes over its target, ‰ of the target (0: never
    /// over).
    pub overshoot_permille: u64,
    /// The worst step's GPU bytes over the hard limit, bytes (must be 0).
    pub over_hard: u64,
    /// Steps after the motion ends until nothing is started, evicted or
    /// delivered and no tile is under its need, for good (`None`: not within
    /// the rest).
    pub settling_steps: Option<u64>,
    /// The most evictions in one step, and in any window of 10 steps.
    pub evict_peak: u32,
    pub evict_peak_10: u32,
    /// Evictions over the run.
    pub evicted: u64,
    /// The most uploads planned in one step, items and bytes.
    pub upload_peak_items: usize,
    pub upload_peak_bytes: u64,
    /// The most loads started in one step.
    pub start_peak: usize,
    /// Network refetches within the thrash window.
    pub thrash: u64,
    /// Starts whose result left the need before it arrived.
    pub wasted: u64,
    /// The deficit's tile·steps.
    pub under_tile_steps: u64,
    /// `decide`'s work counter: the worst step, and the mean.
    pub work_max: u32,
    pub work_mean: u64,
    /// Drawn ground items, summed over steps, by what their base drape is:
    /// their own within one level of the need, their own coarser, an
    /// ancestor's (a stand-in), none.
    pub drawn_own: u64,
    pub drawn_own_coarse: u64,
    pub drawn_ancestor: u64,
    pub drawn_bare: u64,
    /// Of the stand-ins, the worst gap between the level asked and the
    /// level worn, and how many item·steps wear one 3 levels or more under.
    pub ancestor_gap_max: u32,
    pub ancestor_gap_3: u64,
    /// Ancestor-dressed item·steps by gap: 0, 1, 2, 3 or more.
    pub ancestor_gaps: [u64; 4],
    /// Meshes on the GPU with no drape of their own tile on the GPU, summed
    /// over steps: drawn, and not drawn (a preload held bare).
    pub resident_bare_drawn: u64,
    pub resident_bare_undrawn: u64,
    /// Meshes on the GPU not drawn whose finest drape on the GPU, of
    /// their own tile or an ancestor, is 3 levels or more under the mesh's
    /// level: drawn now, they would wear a smear.
    pub resident_smear_undrawn: u64,
    /// Meshes on the GPU, summed over steps.
    pub resident_meshes: u64,
    /// At the last step (after 4 s at rest): drawn items wearing an
    /// ancestor's drape, drawn items, and meshes on the GPU without a drape
    /// of their own, of meshes on the GPU.
    pub last_ancestor: u64,
    pub last_drawn: u64,
    pub last_resident_bare: u64,
    pub last_resident: u64,
    /// The present view: footprint holes once the floor is in, tiles drawn
    /// exact (summed over steps), and the p95 of the wait from wanted to
    /// drawable of the present need's meshes, ms.
    pub holes: u64,
    pub exact: u64,
    pub wait_p95_ms: f64,
    /// The run broke an invariant of the simulation: its message.
    pub broke: Option<String>,
}

impl Saso {
    pub fn line(&self) -> String {
        format!(
            "{:<13} steps {:>5} | holes {} exact {} wait p95 {:.0} ms | over {:>4}‰ hard+{} settle {:>5} | evict pk {:>4} pk10 {:>5} tot {:>6} | upl pk {:>3} {:>6} KiB | start pk {:>3} | thrash {:>4} waste {:>5} under {:>7} | work max {:>6} mean {:>5} | drawn own {:>6} coarse {:>5} anc {:>5} (gaps {:?} max {} ≥3 {}) bare {} | resident bare drawn {} undrawn {} (smear {}) of {} | last: anc {}/{} drawn, bare {}/{} resident{}",
            self.profile,
            self.steps,
            self.holes,
            self.exact,
            self.wait_p95_ms,
            self.overshoot_permille,
            self.over_hard,
            self.settling_steps
                .map_or("never".to_owned(), |s| s.to_string()),
            self.evict_peak,
            self.evict_peak_10,
            self.evicted,
            self.upload_peak_items,
            self.upload_peak_bytes >> 10,
            self.start_peak,
            self.thrash,
            self.wasted,
            self.under_tile_steps,
            self.work_max,
            self.work_mean,
            self.drawn_own,
            self.drawn_own_coarse,
            self.drawn_ancestor,
            self.ancestor_gaps,
            self.ancestor_gap_max,
            self.ancestor_gap_3,
            self.drawn_bare,
            self.resident_bare_drawn,
            self.resident_bare_undrawn,
            self.resident_smear_undrawn,
            self.resident_meshes,
            self.last_ancestor,
            self.last_drawn,
            self.last_resident_bare,
            self.last_resident,
            self.broke
                .as_ref()
                .map_or(String::new(), |b| format!(" | BROKE: {}", first_line(b))),
        )
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Runs the acceptance sequence on `profile` and measures it.
pub fn measure(profile: Profile) -> Saso {
    measure_with(profile, false)
}

/// [`measure`], every preload candidate dropped when `no_preloads`.
pub fn measure_with(profile: Profile, no_preloads: bool) -> Saso {
    let name = profile.name;
    let run = std::panic::catch_unwind(move || measure_inner(profile, no_preloads));
    match run {
        Ok(s) => s,
        Err(e) => Saso {
            profile: name,
            broke: Some(
                e.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "panicked".to_owned()),
            ),
            ..Saso::default()
        },
    }
}

fn measure_inner(profile: Profile, no_preloads: bool) -> Saso {
    let s = named("aggressive-sequence").expect("in the catalogue");
    let mut o = Saso {
        profile: profile.name,
        ..Saso::default()
    };
    let mut sim = Sim::unchecked(profile, 101);
    sim.no_preloads = no_preloads;
    sim.budget
        .specify(Specification::interactive())
        .expect("a valid specification");
    let end_move = Micros((s.seconds * 1e6) as u64);
    let end = end_move.plus(Micros((REST_SECONDS * 1e6) as u64));
    let mut window: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
    let mut quiet_since: Option<u64> = None;
    let mut work_sum = 0u64;
    let mut step_at_end_move: Option<u64> = None;
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        let cam = (s.path)(t.min(s.seconds));
        let views = sim.views(cam);
        let (_, d) = sim.step(cam);
        o.steps += 1;
        if sim.floor_at.is_some() {
            o.holes += u64::from(tuile_budget_sim::objective::holes(&d, &views));
        }
        o.exact += u64::from(d.stats.exact);
        let st = &d.stats;
        // Bytes against the target and the hard limit, over the classes.
        let gpu: u64 = st.gpu.iter().map(|(_, b)| b.0).sum();
        let target: u64 = st.target.iter().map(|(_, b)| b.0).sum();
        let hard: u64 = st.hard_limit.iter().map(|(_, b)| b.0).sum();
        if target > 0 && gpu > target {
            o.overshoot_permille = o.overshoot_permille.max((gpu - target) * 1_000 / target);
        }
        o.over_hard = o.over_hard.max(gpu.saturating_sub(hard));
        let ev = u32::try_from(d.evict.len()).unwrap_or(u32::MAX);
        o.evict_peak = o.evict_peak.max(ev);
        o.evicted += u64::from(ev);
        window.push_back(ev);
        if window.len() > 10 {
            window.pop_front();
        }
        o.evict_peak_10 = o.evict_peak_10.max(window.iter().sum());
        o.upload_peak_items = o.upload_peak_items.max(d.upload_plan.items.len());
        o.upload_peak_bytes = o.upload_peak_bytes.max(d.upload_plan.bytes.0);
        o.start_peak = o.start_peak.max(d.start.len());
        o.thrash += u64::from(st.thrash);
        o.wasted += st.wasted_starts.iter().map(|(_, n)| u64::from(n)).sum::<u64>();
        o.under_tile_steps += u64::from(st.deficit.under_tiles());
        o.work_max = o.work_max.max(st.work);
        work_sum += u64::from(st.work);
        // Settling: after the motion, the first step from which nothing
        // moves and nothing is under its need until the end.
        if sim.now >= end_move {
            let at = *step_at_end_move.get_or_insert(o.steps);
            let quiet = d.start.is_empty()
                && d.evict.is_empty()
                && d.deliver.is_empty()
                && st.deficit.under_tiles() == 0;
            match (quiet, quiet_since) {
                (true, None) => quiet_since = Some(o.steps - at),
                (false, Some(_)) => quiet_since = None,
                _ => {}
            }
        }
        let (anc0, res0, bare0) = (
            o.drawn_ancestor,
            o.resident_meshes,
            o.resident_bare_drawn + o.resident_bare_undrawn,
        );
        o.last_drawn = d.draw.items.len() as u64;
        // The mesh + drape law, as drawn.
        for item in &d.draw.items {
            let tile = item.mesh.tile();
            let base = item.drapes.iter().min_by_key(|c| c.layer);
            match base {
                None => o.drawn_bare += 1,
                Some(c) if c.resource.tile() != tile => {
                    o.drawn_ancestor += 1;
                    let gap = c.need.saturating_sub(c.level);
                    o.ancestor_gap_max = o.ancestor_gap_max.max(gap);
                    o.ancestor_gaps[gap.min(3) as usize] += 1;
                    if gap >= 3 {
                        o.ancestor_gap_3 += 1;
                    }
                }
                Some(c) if item.role == Role::Exact && c.need.saturating_sub(c.level) >= 2 => {
                    o.drawn_own_coarse += 1
                }
                Some(_) => o.drawn_own += 1,
            }
        }
        // As held: meshes on the GPU without a drape of their own tile.
        let drawn: BTreeSet<_> = d.draw.items.iter().map(|i| i.mesh).collect();
        let draped: BTreeSet<_> = sim
            .gpu
            .drawable
            .keys()
            .filter(|r| matches!(r, ResourceId::Drape { .. }))
            .map(|r| r.tile())
            .collect();
        // The finest drape level held per tile.
        let mut finest: std::collections::BTreeMap<tuile_budget::TileId, u32> =
            std::collections::BTreeMap::new();
        for r in sim.gpu.drawable.keys() {
            if let ResourceId::Drape { tile, level, .. } = r {
                let e = finest.entry(*tile).or_insert(0);
                *e = (*e).max(*level);
            }
        }
        for r in sim.gpu.drawable.keys() {
            if let ResourceId::Mesh(t) = r {
                o.resident_meshes += 1;
                if !draped.contains(t) {
                    if drawn.contains(r) {
                        o.resident_bare_drawn += 1;
                    } else {
                        o.resident_bare_undrawn += 1;
                    }
                }
                if !drawn.contains(r) {
                    let (l, mut x, mut y) = t.terrain_coord();
                    let mut best = finest.get(t).copied().unwrap_or(0);
                    let mut up = l;
                    while up > 0 {
                        up -= 1;
                        x /= 2;
                        y /= 2;
                        let a = tuile_budget::TileId::from_terrain(up, x, y).with_tag(t.tag());
                        best = best.max(finest.get(&a).copied().unwrap_or(0));
                    }
                    if l.saturating_sub(best) >= 3 {
                        o.resident_smear_undrawn += 1;
                    }
                }
            }
        }
        o.last_ancestor = o.drawn_ancestor - anc0;
        o.last_resident = o.resident_meshes - res0;
        o.last_resident_bare = o.resident_bare_drawn + o.resident_bare_undrawn - bare0;
    }
    o.settling_steps = quiet_since;
    let mut w: Vec<f64> = sim.waits.iter().map(|x| x.1).collect();
    w.sort_by(f64::total_cmp);
    o.wait_p95_ms = w
        .get(((w.len().max(1) - 1) as f64 * 0.95).round() as usize)
        .copied()
        .unwrap_or(0.0);
    o.work_mean = work_sum / o.steps.max(1);
    o
}

/// The profiles of the before/after table: desktop, handheld, web, and the
/// handheld at its low clamp — the sequence under pressure.
pub fn profiles() -> Vec<Profile> {
    vec![
        Profile::desktop(),
        Profile::handheld(),
        Profile::web(),
        Profile::handheld_low(),
    ]
}

/// The table, printed: `cargo test --release -p tuile-budget-sim --test
/// regulator -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement: prints the SASO table of the acceptance sequence"]
fn saso_of_the_acceptance_sequence() {
    for p in profiles() {
        eprintln!("{}", measure(p).line());
    }
}

/// T5: the present view with and without preloading (the neighbourhood's
/// preload candidates dropped), printed side by side.
#[test]
#[ignore = "a measurement: prints the present view with and without preloads"]
fn the_present_view_with_and_without_preloads() {
    for p in profiles() {
        eprintln!("with    {}", measure(p.clone()).line());
        eprintln!("without {}", measure_with(p, true).line());
    }
}

/// T5, the gate (Laurent, 2026-11-28: a preload never affects the main
/// view): with preloads, the present view is the same or better than
/// without — no more holes, no more tile·steps under their need, no longer
/// a p95 wait for the present need's meshes.
#[test]
#[ignore = "red: on main the preloads take the present need's room (handheld, web, handheld-low)"]
fn preloads_never_worsen_the_present_view() {
    let mut worse = Vec::new();
    for p in profiles() {
        let with = measure(p.clone());
        let without = measure_with(p, true);
        assert!(with.broke.is_none(), "{}", with.line());
        assert!(without.broke.is_none(), "{}", without.line());
        if with.holes > without.holes
            || with.under_tile_steps > without.under_tile_steps
            || with.wait_p95_ms > without.wait_p95_ms
        {
            worse.push(format!(
                "{}: holes {} vs {}, under {} vs {}, wait p95 {:.0} vs {:.0} ms",
                with.profile,
                with.holes,
                without.holes,
                with.under_tile_steps,
                without.under_tile_steps,
                with.wait_p95_ms,
                without.wait_p95_ms
            ));
        }
    }
    assert!(worse.is_empty(), "preloads worsen the present view:\n{}", worse.join("\n"));
}
