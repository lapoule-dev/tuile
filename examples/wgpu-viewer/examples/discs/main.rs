// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The discs' acceptance scenes in the viewer, headless, on the real terrain
//! and imagery (`tuile_atmosphere::scenes`): the viewer's own frame at the
//! scene's place and aim, its clock set to each of the scene's instants —
//! a PNG an instant and a contact sheet a scene. The film shows the same
//! scenes from their tapes (`tuile-film-scene`).
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example discs -- \
//!     all --out videos/discs-viewer
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example discs -- \
//!     full-moon-rise --out videos/discs-viewer
//! ```
//!
//! The viewer draws with one field of view (35°). A scene of a narrower
//! lens is drawn larger at that field — as many pixels a degree as the
//! scene's lens gives `--height` pixels — and its middle cut out: the same
//! picture, pixel for pixel, as a camera of that lens would draw.

// The runner's proxy; what only the runner reads of it is unused here.
#[allow(dead_code)]
#[path = "../headless/catalog.rs"]
mod catalog;

use std::path::{Path, PathBuf};

use tuile_atmosphere::clock::format_rfc3339;
use tuile_atmosphere::scenes::{disc_scene, disc_scenes, DiscScene};
use tuile_viewer::control::{Action, Headless, Start, View};

/// The viewer's own vertical field of view, degrees.
const VIEWER_FOVY_DEG: f64 = 35.0;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error,foyer_storage=off".into()),
        )
        .try_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((which, rest)) = args.split_first() else {
        eprintln!("usage: discs <scene|all> [--out DIR] [--width W] [--height H]");
        for s in disc_scenes() {
            eprintln!("  {}", s.name);
        }
        std::process::exit(2);
    };
    let mut out = PathBuf::from("videos/discs-viewer");
    let (mut width, mut height) = (960u32, 540u32);
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| {
            eprintln!("{flag} needs a value");
            std::process::exit(2);
        });
        match flag.as_str() {
            "--out" => out = PathBuf::from(value),
            "--width" => width = value.parse().unwrap_or(width),
            "--height" => height = value.parse().unwrap_or(height),
            other => {
                eprintln!("unknown flag {other}");
                std::process::exit(2);
            }
        }
    }
    let scenes: Vec<DiscScene> = if which == "all" {
        disc_scenes()
    } else {
        match disc_scene(which) {
            Some(s) => vec![s],
            None => {
                eprintln!("no scene {which:?}");
                std::process::exit(2);
            }
        }
    };
    for scene in &scenes {
        if let Err(why) = run(scene, &out.join(scene.name), (width, height)) {
            eprintln!("{}: {why}", scene.name);
            std::process::exit(1);
        }
    }
}

fn run(scene: &DiscScene, out: &Path, size: (u32, u32)) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    // Drawn at the viewer's field, as many pixels a degree as the scene's.
    let grow =
        (VIEWER_FOVY_DEG.to_radians() * 0.5).tan() / (scene.fovy_deg.to_radians() * 0.5).tan();
    let grow = grow.max(1.0);
    let drawn = (
        (f64::from(size.0) * grow).round() as u32,
        (f64::from(size.1) * grow).round() as u32,
    );
    let proxy = catalog::Proxy::serve()?;
    let dir = tempfile::Builder::new()
        .prefix("tuile-discs-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let times = scene.times();
    let mut run = Headless::start(Start {
        view: View {
            lon: scene.lon_deg,
            lat: scene.lat_deg,
            altitude: scene.height_m,
            heading: scene.heading_deg,
            // The viewer's pitch is under the horizon.
            pitch: -scene.pitch_deg,
        },
        size: drawn,
        scale: 1.0,
        lit_at: times[0],
        ..Start::new(
            proxy.host(dir.path().join("http")),
            dir.path().join("tiles"),
        )
    })
    .map_err(|e| e.to_string())?;
    let settled = run.settle(120.0);
    let _ = run.wait_until(60.0, tuile_viewer::control::State::idle);
    println!(
        "{}: settled {} in {:.1} s, drawn {}×{} for a {}° lens",
        scene.name, settled.settled, settled.seconds, drawn.0, drawn.1, scene.fovy_deg
    );
    let mut frames = Vec::new();
    for (n, &instant) in times.iter().enumerate() {
        run.act(&Action::Time { instant, rate: 0.0 });
        // A light worked out again takes a frame; its sky-view another.
        run.steps(4);
        let (pixels, (w, h)) = run.picture(0).ok_or("the eye drew nothing")?;
        let full = image::RgbaImage::from_raw(w, h, pixels.to_vec()).ok_or("a frame")?;
        let (cw, ch) = (size.0.min(w), size.1.min(h));
        let cut = image::imageops::crop_imm(&full, (w - cw) / 2, (h - ch) / 2, cw, ch).to_image();
        let path = out.join(format!("{n:03}.png"));
        cut.save(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let black = run.state().black();
        println!(
            "{} {n:03} {}{}",
            scene.name,
            format_rfc3339(instant),
            if black > 0 {
                format!(" — {black} black pixels")
            } else {
                String::new()
            }
        );
        frames.push(cut);
    }
    sheet(&frames, &out.join("sheet.png"))?;
    run.finish()?;
    Ok(())
}

/// Every frame, in reading order, at half size when there are several.
fn sheet(frames: &[image::RgbaImage], path: &Path) -> Result<(), String> {
    let Some(first) = frames.first() else {
        return Ok(());
    };
    let shrink = if frames.len() > 1 { 2 } else { 1 };
    let (w, h) = (first.width() / shrink, first.height() / shrink);
    let columns = (frames.len() as u32).min(4);
    let rows = (frames.len() as u32).div_ceil(columns);
    let mut out = image::RgbaImage::new(w * columns, h * rows);
    for (i, frame) in frames.iter().enumerate() {
        let small = image::imageops::resize(frame, w, h, image::imageops::FilterType::Triangle);
        let (x, y) = ((i as u32 % columns) * w, (i as u32 / columns) * h);
        image::imageops::overlay(&mut out, &small, i64::from(x), i64::from(y));
    }
    out.save(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("sheet: {}", path.display());
    Ok(())
}
