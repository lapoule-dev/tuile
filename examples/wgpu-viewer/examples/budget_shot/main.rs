// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Pictures of the viewer's own frame, headless, with the budget's line over
//! it — the specification in force and the tiles under their need
//! (`docs/explorations/budget-eviction-prefetch-spec.md` §7.3) — under the
//! context's specification, then under the FPV one forced as a diagnostic.
//!
//! ```text
//! cargo run --release -p tuile-wgpu-viewer --example budget_shot -- \
//!     --lon 6.86 --lat 45.83 --altitude 9000 --heading 120 --pitch 25 --out videos/budget-shot
//! ```
//!
//! Writes `<out>/interactive.png` (settled, the context's specification),
//! `<out>/fpv.png` (the FPV specification forced, `tuile://budget?spec=fpv`)
//! and `<out>/approach.png` (the eye a fifth lower, then still for
//! `--seconds`), and prints the budget's line at each. The credential comes
//! as for the window (`CESIUM_ION_TOKEN`, the token file, …); give it in
//! the environment to keep a build outside the signed bundle away from the
//! keychain.

use std::path::PathBuf;
use std::sync::Arc;

use tuile_atmosphere::clock::parse_rfc3339;
use tuile_viewer::control::{Action, Headless, Start, View};

struct Asked {
    view: View,
    at: f64,
    out: PathBuf,
    size: (u32, u32),
    seconds: f64,
}

fn read(args: &[String]) -> Result<Asked, String> {
    let mut flags = std::collections::BTreeMap::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        flags.insert(flag.as_str(), value.as_str());
    }
    let number = |flag: &str, or: f64| -> Result<f64, String> {
        flags.get(flag).map_or(Ok(or), |v| {
            v.parse()
                .map_err(|_| format!("{flag} wants a number, got {v:?}"))
        })
    };
    Ok(Asked {
        view: View {
            lon: number("--lon", 6.86)?,
            lat: number("--lat", 45.83)?,
            altitude: number("--altitude", 9000.0)?,
            heading: number("--heading", 120.0)?,
            pitch: number("--pitch", 25.0)?,
        },
        at: parse_rfc3339(flags.get("--at").copied().unwrap_or("2026-11-20T12:00:00Z"))?,
        out: PathBuf::from(flags.get("--out").copied().unwrap_or("videos/budget-shot")),
        size: (
            number("--width", 1280.0)? as u32,
            number("--height", 800.0)? as u32,
        ),
        seconds: number("--seconds", 4.0)?,
    })
}

/// The budget's line as the viewer shows it, from the signals published.
fn line() -> String {
    match tuile_core::metrics::budget() {
        None => "budget: not yet".to_owned(),
        Some(s) => format!(
            "budget {}{} · under need {} mesh, {} imagery ({} levels) · scale {:.2}",
            s.spec.label(),
            if s.spec_overridden { " (diagnostic)" } else { "" },
            s.under_need_tiles[0],
            s.under_need_tiles[1],
            s.deficit_levels[0] + s.deficit_levels[1],
            f64::from(s.sse_scale_milli) / 1000.0,
        ),
    }
}

/// Saves the eye's picture as `<out>/<name>.png` and prints the line.
fn save(run: &Headless, asked: &Asked, name: &str) -> Result<(), String> {
    let (pixels, (w, h)) = run.picture(0).ok_or("the eye drew nothing")?;
    let path = asked.out.join(format!("{name}.png"));
    image::save_buffer(&path, pixels, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!(
        "{}: {}, {} black pixels",
        path.display(),
        line(),
        run.state().black()
    );
    Ok(())
}

fn run(asked: &Asked) -> Result<(), String> {
    std::fs::create_dir_all(&asked.out).map_err(|e| e.to_string())?;
    let dir = tempfile::Builder::new()
        .prefix("tuile-budget-shot-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let mut run = Headless::start(Start {
        view: asked.view,
        size: asked.size,
        scale: 1.0,
        lit_at: asked.at,
        ..Start::new(
            Arc::new(tuile_wgpu_viewer::Public),
            dir.path().join("tiles"),
        )
    })
    .map_err(|e| e.to_string())?;
    run.act(&Action::Url("tuile://budget?line=on".to_owned()));
    run.settle(120.0);
    run.act(&Action::Go(asked.view));
    run.settle(120.0);
    let _ = run.wait_until(60.0, tuile_viewer::control::State::idle);
    run.run_for(asked.seconds);
    save(&run, asked, "interactive")?;
    run.act(&Action::Url("tuile://budget?spec=fpv".to_owned()));
    run.run_for(asked.seconds);
    save(&run, asked, "fpv")?;
    run.act(&Action::Url("tuile://budget?spec=auto".to_owned()));
    let closer = View {
        altitude: asked.view.altitude * 0.8,
        ..asked.view
    };
    run.act(&Action::Go(closer));
    run.run_for(asked.seconds);
    save(&run, asked, "approach")?;
    run.finish().map(|_| ())
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error,foyer_storage=off".into()),
        )
        .try_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let asked = read(&args).unwrap_or_else(|why| {
        eprintln!("{why}");
        std::process::exit(2);
    });
    if let Err(why) = run(&asked) {
        eprintln!("{why}");
        std::process::exit(1);
    }
}
