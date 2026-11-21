// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The parameter search over a specification on the scenario catalogue
//! (`docs/explorations/budget-eviction-prefetch-spec.md` §9.3).
//!
//! ```text
//! cargo run --release -p tuile-budget-sim --bin tuile-budget-tune -- \
//!     --spec interactive --out videos/budget-tune [--quick] [--scenario aggressive-sequence] [--lag 30]
//! ```
//!
//! Runs the default specification across the catalogue × devices × networks,
//! then a coordinate search over its margin, horizon and ring probability
//! (each tried on a grid, the best kept, one coordinate at a time), scoring
//! each candidate by the mean of `J` over the scenarios, each normalised by
//! the default's `J` on it, a hole on any scenario making it `∞`. Writes
//! `<out>/runs.parquet` (every run, a row per phase) and `<out>/report.md`.
//! Deterministic: fixed seeds, the simulation's determinism.

use std::path::PathBuf;

use tuile_budget::{Micros, SpecKind, Specification};
use tuile_budget_sim::catalogue::{named, scenarios};
use tuile_budget_sim::{run, table, Device, Network, Outcome, Scenario};

const SEED: u64 = 101;

struct Asked {
    spec: SpecKind,
    out: PathBuf,
    quick: bool,
    only: Option<String>,
    lag: usize,
}

fn read() -> Result<Asked, String> {
    let mut a = Asked {
        spec: SpecKind::Interactive,
        out: PathBuf::from("videos/budget-tune"),
        quick: false,
        only: None,
        lag: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--quick" => a.quick = true,
            "--spec" => {
                let v = it.next().ok_or("--spec needs a value")?;
                a.spec = SpecKind::from_label(&v).ok_or(format!("unknown spec {v}"))?;
            }
            "--out" => a.out = PathBuf::from(it.next().ok_or("--out needs a value")?),
            "--scenario" => a.only = Some(it.next().ok_or("--scenario needs a value")?),
            "--lag" => {
                a.lag = it
                    .next()
                    .ok_or("--lag needs a value")?
                    .parse()
                    .map_err(|_| "--lag is a number of steps")?
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(a)
}

/// The runs of `spec` over the scenarios and axes.
fn sweep(
    spec: &Specification,
    set: &[Scenario],
    axes: &[(Device, Network)],
    lag: usize,
) -> Vec<Outcome> {
    let mut out = Vec::new();
    for s in set {
        for (d, n) in axes {
            out.push(run(s, *d, *n, spec, SEED, lag));
        }
    }
    out
}

/// The mean over runs of `J / J_default`, ∞ with a hole or a break.
fn normalised(runs: &[Outcome], base: &[Outcome]) -> f64 {
    let mut sum = 0.0;
    for (r, b) in runs.iter().zip(base) {
        if !r.j.is_finite() {
            return f64::INFINITY;
        }
        sum += if b.j > 0.0 && b.j.is_finite() { r.j / b.j } else { 1.0 };
    }
    sum / runs.len().max(1) as f64
}

fn main() {
    let asked = read().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    std::fs::create_dir_all(&asked.out).expect("the output directory");
    let set: Vec<Scenario> = match &asked.only {
        Some(n) => vec![named(n).unwrap_or_else(|| {
            eprintln!("no scenario {n}");
            std::process::exit(2);
        })],
        None if asked.quick => scenarios().into_iter().take(4).collect(),
        None => scenarios(),
    };
    let axes: Vec<(Device, Network)> = if asked.quick {
        vec![(Device::Desktop, Network::Fast), (Device::Phone, Network::Slow)]
    } else {
        Device::ALL
            .iter()
            .flat_map(|d| Network::ALL.iter().map(move |n| (*d, *n)))
            .collect()
    };
    let default = Specification::of_kind(asked.spec);
    let base = sweep(&default, &set, &axes, asked.lag);
    let mut all = base.clone();
    let mut best = (default.clone(), 1.0_f64);
    // One coordinate at a time, over a grid around the default.
    type Set = fn(&mut Specification, u32);
    let coordinates: [(&str, &[u32], Set); 3] = [
        ("margin", &[100, 250, 500, 1_000], |s, v| s.params.margin_permille = v),
        ("horizon_ms", &[500, 1_000, 2_000], |s, v| {
            s.params.horizon = Micros::from_millis(u64::from(v))
        }),
        ("p_ring", &[250, 500, 750, 1_000], |s, v| {
            s.params.anticipation.ring_permille = u16::try_from(v).unwrap_or(1_000)
        }),
    ];
    let mut lines = Vec::new();
    for (name, grid, set_to) in coordinates {
        for &v in grid {
            let mut cand = best.0.clone();
            set_to(&mut cand, v);
            if cand == best.0 {
                continue;
            }
            let runs = sweep(&cand, &set, &axes, asked.lag);
            let score = normalised(&runs, &base);
            lines.push(format!("| {name} = {v} | {score:.4} |"));
            all.extend(runs);
            if score < best.1 {
                best = (cand, score);
            }
        }
    }
    table::write(&asked.out.join("runs.parquet"), &all).expect("the runs table");
    let holes: u64 = base.iter().map(|o| o.hole_tile_steps).sum();
    let broke = base.iter().filter(|o| o.broke.is_some()).count();
    let mut report = format!(
        "# Parameter search, {} specification\n\nCatalogue version {}, {} scenarios × {} axes, seed {SEED}, selection lag {} steps.\n\n\
         Default: holes {holes} (tile·steps), {broke} broken runs.\n\n\
         | Candidate | mean J / J_default |\n|---|---|\n",
        asked.spec.label(),
        tuile_budget_sim::CATALOGUE_VERSION,
        set.len(),
        axes.len(),
        asked.lag
    );
    for l in &lines {
        report.push_str(l);
        report.push('\n');
    }
    report.push_str(&format!(
        "\nBest: `{}` at {:.4} of the default.\n\n## Default, per scenario and phase\n\n\
         | scenario | device | network | phase | holes | deficit (lvl·s) | bytes max | starts | wasted | settle (s) |\n|---|---|---|---|---|---|---|---|---|---|\n",
        best.0.to_query(),
        best.1
    ));
    for o in &base {
        for p in &o.phases {
            report.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.1} | {} | {} | {} | {} |\n",
                o.scenario,
                o.device,
                o.network,
                p.phase,
                p.hole_tile_steps,
                p.deficit_level_seconds,
                p.bytes_max,
                p.starts,
                p.wasted_starts,
                o.settle_seconds
                    .map_or("—".to_owned(), |s| format!("{s:.2}")),
            ));
        }
    }
    std::fs::write(asked.out.join("report.md"), report).expect("the report");
    println!("{}", asked.out.join("report.md").display());
}
