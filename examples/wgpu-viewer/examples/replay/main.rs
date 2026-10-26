// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A recorded session (`--record`), drawn again headless through the
//! viewer's own renderer — the one a window draws with — frame by frame:
//! each frame's camera, lit at its own scene instant from its own wall
//! instant, at the recorded viewport's size, drawn until complete and
//! nothing loading. Each picture is compared with the one recorded live,
//! and the replay is written as a session of its own, which
//! `tuile-record export` makes a film and a sidecar of like any other.
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example replay -- \
//!     session.mcap --out replay.mcap [--seek N] [--frames N] [--every N] \
//!     [--bound SECONDS] [--store DIR] [--scale S]
//! ```
//!
//! `--seek N` starts at frame N, `--frames` says how many, `--every` keeps
//! one frame in so many. `--scale` divides the picture as the recording's
//! pictures were divided (`--record-scale`); the default is the recorded
//! pictures' own size, so the two compare pixel for pixel. Each frame's
//! line says how far the replay's picture is from the recorded one (mean
//! absolute difference, 0–255) and a digest of the replay's pixels: two
//! runs that print the same digests drew the same pictures.
//!
//! What a replay cannot give back is what went wrong only live: the tiles
//! resident at that moment, the streaming's timing, the exposure's history.
//! The recorded pictures are the evidence; this is the comparison.

// The runner's proxy; what only the runner reads of it is unused here.
#[allow(dead_code)]
#[path = "../headless/catalog.rs"]
mod catalog;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tuile_tape::session::{self, Recorded};
use tuile_viewer::control::{Headless, Start, View};

struct Asked {
    session: PathBuf,
    out: PathBuf,
    seek: usize,
    frames: usize,
    every: usize,
    bound: f64,
    store: PathBuf,
    scale: Option<f64>,
}

fn read(args: &[String]) -> Result<Asked, String> {
    let mut rest = args.iter();
    let session = PathBuf::from(rest.next().ok_or("the recording to replay")?);
    let mut flags = BTreeMap::new();
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
        out: PathBuf::from(flags.get("--out").copied().unwrap_or("replay.mcap")),
        seek: number("--seek", 0.0)? as usize,
        frames: number("--frames", f64::from(u32::MAX))? as usize,
        every: (number("--every", 1.0)? as usize).max(1),
        bound: number("--bound", 30.0)?,
        store: PathBuf::from(flags.get("--store").copied().unwrap_or("target/replay-store")),
        scale: flags.contains_key("--scale").then(|| number("--scale", 1.0)).transpose()?,
        session,
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

/// The recorded pictures, decoded, by frame.
fn recorded_pictures(path: &std::path::Path) -> Result<BTreeMap<u64, (Vec<u8>, (u32, u32))>, String> {
    let mut out = BTreeMap::new();
    session::for_each_picture(path, |p| {
        let decoded = p.rgba()?;
        out.insert(p.index, decoded);
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    Ok(out)
}

/// `rgba` of `size`, averaged in blocks of `step` pixels a side.
fn shrink(rgba: &[u8], (w, h): (u32, u32), step: u32) -> (Vec<u8>, (u32, u32)) {
    if step <= 1 {
        return (rgba.to_vec(), (w, h));
    }
    let (sw, sh) = (w / step, h / step);
    let mut out = Vec::with_capacity((sw * sh * 4) as usize);
    let area = step * step;
    for y in 0..sh {
        for x in 0..sw {
            let mut sum = [0u32; 4];
            for dy in 0..step {
                for dx in 0..step {
                    let i = (((y * step + dy) * w + x * step + dx) * 4) as usize;
                    for (c, s) in sum.iter_mut().enumerate() {
                        *s += u32::from(rgba[i + c]);
                    }
                }
            }
            out.extend(sum.map(|s| ((s + area / 2) / area) as u8));
        }
    }
    (out, (sw, sh))
}

/// FNV-1a over the pixels: equal digests, equal pictures.
fn digest(rgba: &[u8]) -> u64 {
    rgba.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn run(asked: &Asked) -> Result<(), String> {
    let frames: Vec<Recorded> = session::read(&asked.session).map_err(|e| e.to_string())?;
    let recorded = recorded_pictures(&asked.session)?;
    let first = frames.get(asked.seek).ok_or("--seek is past the recording")?;
    let ([w, h], scale) = first
        .context
        .map_or(([1280, 800], 2.0), |c| (c.viewport, c.scale));
    // The pictures' own division of the frame, unless said.
    let step = match asked.scale {
        Some(s) => (1.0 / s).round().max(1.0) as u32,
        None => recorded
            .values()
            .next()
            .map_or(1, |(_, (pw, _))| (w / (*pw).max(1)).max(1)),
    };
    println!(
        "{} frames recorded, {} pictured; replaying at {w}x{h} (scale {scale}), pictures 1/{step}",
        frames.len(),
        recorded.len()
    );
    let proxy = catalog::Proxy::serve()?;
    std::fs::create_dir_all(&asked.store).map_err(|e| e.to_string())?;
    let camera = tuile_camera::GlobeCamera {
        position: glam::DVec3::from_array(first.camera.position),
        direction: glam::DVec3::from_array(first.camera.direction),
        up: glam::DVec3::from_array(first.camera.up),
        fovy: first.camera.fovy,
    };
    let eye = tuile_core::geo::ecef_to_geodetic(camera.position);
    let mut run = Headless::start(Start {
        view: View {
            lon: eye.lon.to_degrees(),
            lat: eye.lat.to_degrees(),
            altitude: eye.height.max(1.0),
            heading: camera.heading().to_degrees().rem_euclid(360.0),
            pitch: camera.pitch().to_degrees().clamp(0.0, 90.0),
        },
        size: (w, h),
        scale,
        ..Start::new(proxy.host(asked.store.join("http")), asked.store.join("tiles"))
    })
    .map_err(|e| e.to_string())?;
    let mut out = tuile_tape::Tape::recording(&asked.out).map_err(|e| e.to_string())?;
    let mut diffs = Vec::new();
    for frame in frames
        .iter()
        .skip(asked.seek)
        .step_by(asked.every)
        .take(asked.frames)
    {
        let replayed = run.replay(frame, asked.bound);
        let (pixels, size) = run.picture(0).ok_or("the eye drew nothing")?;
        let (small, small_size) = shrink(pixels, size, step);
        out.push(frame.camera);
        if let Some(context) = frame.context {
            out.push_context(&context);
        }
        if let Some(sky) = replayed.sky {
            out.push_sky(&sky);
        }
        let index = out.frames() - 1;
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            &small,
            small_size.0,
            small_size.1,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| e.to_string())?;
        out.push_picture(index, "png", &png);
        let diff = recorded.get(&frame.index).and_then(|(live, live_size)| {
            (*live_size == small_size).then(|| {
                let sum: u64 = live
                    .iter()
                    .zip(&small)
                    .enumerate()
                    .filter(|(i, _)| i % 4 != 3)
                    .map(|(_, (a, b))| u64::from(a.abs_diff(*b)))
                    .sum();
                sum as f64 / (live.len() / 4 * 3) as f64
            })
        });
        out.push_json(
            "/replay/diff",
            "tuile.ReplayDiff",
            r#"{"type":"object"}"#,
            &format!(
                r#"{{"version":1,"recorded_frame":{},"mean_abs_diff":{},"settled":{},"draws":{},"digest":"{:016x}"}}"#,
                frame.index,
                diff.map_or("null".to_owned(), |d| format!("{d}")),
                replayed.settled,
                replayed.draws,
                digest(&small)
            ),
        );
        if let Some(d) = diff {
            diffs.push(d);
        }
        println!(
            "frame {:>6}  {}  draws {:>3}  {}  diff {}  digest {:016x}",
            frame.index,
            tuile_atmosphere::clock::format_rfc3339(replayed.instant),
            replayed.draws,
            if replayed.settled { "settled" } else { "BOUND  " },
            diff.map_or("-".to_owned(), |d| format!("{d:.2}")),
            digest(&small)
        );
    }
    out.finish().map_err(|e| e.to_string())?;
    run.finish()?;
    if !diffs.is_empty() {
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let worst = diffs.iter().copied().fold(0.0, f64::max);
        println!("replay vs live: mean abs diff {mean:.2}, worst {worst:.2} (0-255)");
    }
    println!("replay written: {}", asked.out.display());
    Ok(())
}
