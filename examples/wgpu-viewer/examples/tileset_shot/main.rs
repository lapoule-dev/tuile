// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One picture of the viewer's own frame, headless, with the host's 3D Tiles
//! over the globe — and the same picture without them, to compare.
//!
//! ```text
//! CESIUM_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example tileset_shot -- \
//!     --tileset 2275207 --lon 2.3065 --lat 48.8505 --altitude 700 --heading 315 --pitch 20 \
//!     --at 2026-10-31T12:00:00Z --out videos/paris
//! ```
//!
//! Writes `<out>/globe.png` (no tileset), `<out>/tileset.png` (with it) and
//! prints what each frame's scan found. The host is the application's own
//! (`tuile_wgpu_viewer::Public`): the token is read from `CESIUM_ION_TOKEN`
//! and never printed. `--seconds` is how long the tileset is given to load
//! after the globe has settled (the settling watches the globe's own pump).

use std::path::PathBuf;
use std::sync::Arc;

use tuile_atmosphere::clock::parse_rfc3339;
use tuile_viewer::control::{Action, Headless, Start, View};

struct Asked {
    view: View,
    tilesets: Vec<i64>,
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
    let tilesets = flags
        .get("--tileset")
        .ok_or("--tileset names the asset to draw")?
        .split(',')
        .map(|a| {
            a.trim()
                .parse::<i64>()
                .map_err(|_| format!("--tileset wants asset numbers, got {a:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Asked {
        view: View {
            lon: number("--lon", 2.3065)?,
            lat: number("--lat", 48.8505)?,
            altitude: number("--altitude", 700.0)?,
            heading: number("--heading", 315.0)?,
            pitch: number("--pitch", 20.0)?,
        },
        tilesets,
        at: parse_rfc3339(flags.get("--at").copied().unwrap_or("2026-10-31T12:00:00Z"))?,
        out: PathBuf::from(flags.get("--out").copied().unwrap_or("videos/tileset-shot")),
        size: (
            number("--width", 1280.0)? as u32,
            number("--height", 800.0)? as u32,
        ),
        seconds: number("--seconds", 40.0)?,
    })
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

fn shoot(asked: &Asked, tilesets: Vec<i64>, name: &str) -> Result<(), String> {
    let dir = tempfile::Builder::new()
        .prefix("tuile-tileset-shot-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let with = !tilesets.is_empty();
    let mut run = Headless::start(Start {
        view: asked.view,
        size: asked.size,
        scale: 1.0,
        lit_at: asked.at,
        tilesets,
        ..Start::new(
            Arc::new(tuile_wgpu_viewer::Public),
            dir.path().join("tiles"),
        )
    })
    .map_err(|e| e.to_string())?;
    // Placed again once the ground under it is known: at the start the
    // floor is the coarsest relief's, which can stand kilometres above the
    // real ground and lifts a low eye with it.
    run.settle(120.0);
    run.act(&Action::Go(asked.view));
    let settled = run.settle(120.0);
    let _ = run.wait_until(60.0, tuile_viewer::control::State::idle);
    if with {
        run.run_for(asked.seconds);
    }
    let (pixels, (w, h)) = run.picture(0).ok_or("the eye drew nothing")?;
    let path = asked.out.join(format!("{name}.png"));
    image::save_buffer(&path, pixels, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!(
        "{}: settled {} in {:.1} s, {} black pixels, eye {}",
        path.display(),
        settled.settled,
        settled.seconds,
        run.state().black(),
        run.state().to_json()["view"]
    );
    run.finish().map(|_| ())
}

fn run(asked: &Asked) -> Result<(), String> {
    std::fs::create_dir_all(&asked.out).map_err(|e| e.to_string())?;
    shoot(asked, Vec::new(), "globe")?;
    shoot(asked, asked.tilesets.clone(), "tileset")
}
