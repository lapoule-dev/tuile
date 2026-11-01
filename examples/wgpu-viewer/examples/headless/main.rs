// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The motion patterns of the graphics budget, played on this application,
//! headless, on the real catalog, the real tile store and the real clock — on
//! demand. A runner, not a test: it needs the network, an ion token and a
//! GPU, and its numbers vary with all three.
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example headless -- run all
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example headless -- \
//!     run p2-fast --device phone --hz 120
//! cargo run --release -p tuile-wgpu-viewer --example headless -- list
//! ```
//!
//! Each run is the application a window runs — its session, its runtime and
//! its loads, its upload pacing, its tile store on disk in a fresh directory
//! of its own — asking Cesium World Terrain and the aerial imagery through a
//! catalog proxy served in this process, which alone holds the token (read
//! from `TUILE_ION_TOKEN`, never printed: see `catalog`). It draws into
//! offscreen eyes at the display's refresh in wall time, and every frame is
//! read back and scanned.
//!
//! Each pattern is held to the specification's obligations
//! (`tuile_viewer::control::verdict`): no ground left black in any eye, no
//! selected ground drawn by nothing, the server within the profile's budget,
//! the picture settled within the bound, and — on a warm relaunch — nothing
//! the store holds asked of the network again. The runner prints a line per
//! obligation, writes each run's session log (MCAP: the camera, a state a
//! frame, the pictures of the frames that broke an obligation) and a JSON
//! summary, and exits 1 when an obligation was violated.
//!
//! | flag | meaning |
//! |---|---|
//! | `--device D` | `desktop`, `phone`, `low-memory-phone`, `android` or `headset`, instead of the pattern's own |
//! | `--hz N` | the display's refresh, instead of the pattern's |
//! | `--memory-gib N` | a handheld's memory, half of it left to the process |
//! | `--watch S` | seconds the settling is watched for at most, instead of the pattern's |
//! | `--logs DIR` | where the session logs and `summary.json` go: `target/headless` unless said |
//! | `--json PATH` | where the summary goes, instead |
//! | `--imagery-compression C` | `off` (the default), `auto`, `bc7` or `astc`: how imagery is held on the GPU |
//! | `--pictures DIR` | where each run's last picture goes, once settled: `<pattern>.png`, eye 0 |
//! | `--idle S` | after settling, wait up to `S` seconds for every load and upload to land (drapes included, which the settling does not see), and say how long it took |
//! | `--hold S` | then keep the view still for `S` seconds more, logged: what a still view costs |
//!
//! A run's log flies again in the application: `TUILE_REPLAY=<log>.mcap`.

mod catalog;

use std::path::{Path, PathBuf};

use serde_json::json;
use tuile_viewer::control::patterns::{self, Pattern};
use tuile_viewer::control::{
    compression_named, Device, Headless, ImageryCompression, Obligations, Start, State, Verdict,
};

const USAGE: &str = "headless list
headless run <pattern|all> [--device D] [--hz N] [--memory-gib N] [--watch S] [--logs DIR] [--json PATH]
             [--imagery-compression off|auto|bc7|astc] [--pictures DIR] [--idle S] [--hold S]

The patterns ask the real catalog: TUILE_ION_TOKEN must be set. See the source's documentation.";

/// What the command line asked for.
struct Asked {
    patterns: Vec<Pattern>,
    logs: PathBuf,
    json: Option<PathBuf>,
    compression: ImageryCompression,
    pictures: Option<PathBuf>,
    idle: Option<f64>,
    hold: Option<f64>,
}

fn device(name: &str, memory_gib: Option<f64>) -> Result<Device, String> {
    let memory =
        |default: u64| memory_gib.map_or(default, |gib| (gib * f64::from(1u32 << 30)) as u64);
    Ok(match name {
        "desktop" => Device::Desktop,
        "phone" => Device::Handheld {
            memory: memory(6 << 30),
            available: Some(memory(6 << 30) / 2),
            cores: 6,
        },
        "low-memory-phone" => Device::LOW_MEMORY_PHONE,
        "android" => Device::Android {
            memory: memory(6 << 30),
            available: Some(memory(6 << 30) / 2),
            cores: 8,
        },
        "headset" => patterns::HEADSET,
        other => {
            return Err(format!(
                "no device {other:?}: desktop, phone, low-memory-phone, android or headset"
            ))
        }
    })
}

fn read(args: &[String]) -> Result<Option<Asked>, String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            for p in patterns::all() {
                println!("{:14} {}", p.key, p.name);
            }
            return Ok(None);
        }
        Some("run") => {}
        _ => return Err(USAGE.to_owned()),
    }
    let which = args.get(1).ok_or(USAGE)?;
    let mut flags = std::collections::BTreeMap::new();
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        flags.insert(flag.as_str(), value.as_str());
    }
    let number = |flag: &str| -> Result<Option<f64>, String> {
        flags
            .get(flag)
            .map(|v| {
                v.parse::<f64>()
                    .map_err(|_| format!("{flag} is a number, not {v:?}"))
            })
            .transpose()
    };
    let mut patterns = if which == "all" {
        patterns::all()
    } else {
        vec![patterns::named(which)
            .ok_or_else(|| format!("no pattern {which:?}: see `headless list`"))?]
    };
    let memory = number("--memory-gib")?;
    if let Some(name) = flags.get("--device") {
        let device = device(name, memory)?;
        patterns = patterns.into_iter().map(|p| p.on(device)).collect();
    }
    if let Some(hz) = number("--hz")? {
        patterns.iter_mut().for_each(|p| p.refresh_hz = hz);
    }
    if let Some(watch) = number("--watch")? {
        patterns.iter_mut().for_each(|p| p.watch = watch);
    }
    for key in flags.keys() {
        if ![
            "--device",
            "--hz",
            "--memory-gib",
            "--watch",
            "--logs",
            "--json",
            "--imagery-compression",
            "--pictures",
            "--idle",
            "--hold",
        ]
        .contains(key)
        {
            return Err(format!("unknown flag {key}\n\n{USAGE}"));
        }
    }
    Ok(Some(Asked {
        patterns,
        logs: flags
            .get("--logs")
            .map_or_else(|| PathBuf::from("target/headless"), PathBuf::from),
        json: flags.get("--json").map(PathBuf::from),
        compression: flags
            .get("--imagery-compression")
            .map_or(Ok(ImageryCompression::Off), |name| compression_named(name))?,
        pictures: flags.get("--pictures").map(PathBuf::from),
        idle: number("--idle")?,
        hold: number("--hold")?,
    }))
}

/// What one pattern came to.
struct Outcome {
    verdict: Verdict,
    /// Whether the run came up before its motion, within its time.
    came_up: bool,
    launch: f64,
    log: Option<PathBuf>,
}

/// A directory a run keeps its store and its HTTP cache in, removed after.
fn place() -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix("tuile-headless-")
        .tempdir()
        .map_err(|e| e.to_string())
}

fn start(
    pattern: &Pattern,
    proxy: &catalog::Proxy,
    place: &Path,
    log: Option<PathBuf>,
    compression: ImageryCompression,
) -> Start {
    Start {
        log,
        imagery_compression: compression,
        ..pattern.start(proxy.host(place.join("http")), place.join("tiles"))
    }
}

/// Plays `pattern`: up, the motion, the watch — or, for a relaunch, a first
/// run that fills the store and the relaunch over it.
fn play(
    pattern: &Pattern,
    proxy: &catalog::Proxy,
    logs: &Path,
    compression: ImageryCompression,
    pictures: Option<&Path>,
    idle: Option<f64>,
    hold: Option<f64>,
) -> Result<Outcome, String> {
    let dir = place()?;
    let log = logs.join(format!("{}.mcap", pattern.key));
    let mut run = if pattern.relaunch {
        let mut first = Headless::start(start(pattern, proxy, dir.path(), None, compression))
            .map_err(|e| e.to_string())?;
        first.settle(pattern.watch);
        // What it was still fetching lands before it is put away.
        let _ = first.wait_until(5.0, State::idle);
        first.relaunch(Some(log)).map_err(|e| e.to_string())?
    } else {
        Headless::start(start(pattern, proxy, dir.path(), Some(log), compression))
            .map_err(|e| e.to_string())?
    };
    let launch = run.launched();
    let came_up = pattern.warm == 0.0 || run.settle(pattern.warm).settled;
    let first = if pattern.warm == 0.0 {
        0
    } else {
        run.history().len()
    };
    run.play(&pattern.motion);
    let mut settled = run.settle(pattern.watch);
    if pattern.warm == 0.0 {
        // Coming up is the pattern: its launch is part of it.
        settled.seconds += launch;
    }
    let verdict = Verdict::of(
        &run.history()[first..],
        settled,
        Obligations {
            bound: pattern.settle,
            resident_budget: run.limits().resident_bytes,
            warm: pattern.relaunch,
        },
    );
    // Until every upload has landed: the drapes refine after the meshes
    // have settled, and a paced encoder lands them over more frames.
    if let Some(bound) = idle {
        match run.wait_until(bound, State::idle) {
            Ok(seconds) => println!("  idle {seconds:.1} s after settling"),
            Err(seconds) => println!("  NOT idle {seconds:.1} s after settling"),
        }
    }
    // A still view, for as long as asked: nothing it holds ends the wait.
    if let Some(bound) = hold {
        let held = run.wait_until(bound, |_| false).unwrap_or_else(|s| s);
        println!("  held still {held:.1} s");
    }
    // The settled picture, as the run's own offscreen eye drew it.
    if let Some(dir) = pictures {
        let path = dir.join(format!("{}.png", pattern.key));
        match run.picture(0) {
            Some((pixels, (width, height))) => {
                image::save_buffer(
                    &path,
                    pixels,
                    width,
                    height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| format!("{}: {e}", path.display()))?;
                println!("  picture: {}", path.display());
            }
            None => eprintln!("  no picture: the eye drew nothing"),
        }
    }
    let log = run.finish()?;
    Ok(Outcome {
        verdict,
        came_up,
        launch,
        log,
    })
}

fn device_name(device: Device) -> String {
    match device {
        Device::Desktop => "desktop".into(),
        d if d == patterns::HEADSET => "headset".into(),
        Device::Handheld { memory, .. } => format!("phone ({} GiB)", memory >> 30),
        Device::Android { memory, .. } => format!("android ({} GiB)", memory >> 30),
    }
}

fn main() {
    // The engine's own lines on stderr, errors only unless `RUST_LOG` says
    // more: the runner's lines are the obligations.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error,foyer_storage=off".into()),
        )
        .try_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let asked = match read(&args) {
        Ok(Some(asked)) => asked,
        Ok(None) => return,
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(2);
        }
    };
    let proxy = match catalog::Proxy::serve() {
        Ok(proxy) => proxy,
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(2);
        }
    };
    for dir in std::iter::once(&asked.logs).chain(asked.pictures.as_ref()) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("{}: {e}", dir.display());
            std::process::exit(2);
        }
    }
    let began = std::time::Instant::now();
    let (mut kept, mut summaries) = (true, Vec::new());
    for pattern in &asked.patterns {
        let header = format!(
            "{} [{}] {} {}×{} @{} {} Hz{}",
            pattern.name,
            pattern.key,
            device_name(pattern.device),
            pattern.size.0,
            pattern.size.1,
            pattern.scale,
            pattern.refresh_hz,
            if pattern.eyes.count() > 1 {
                ", two eyes"
            } else {
                ""
            },
        );
        let outcome = match play(
            pattern,
            &proxy,
            &asked.logs,
            asked.compression,
            asked.pictures.as_deref(),
            asked.idle,
            asked.hold,
        ) {
            Ok(outcome) => outcome,
            Err(why) => {
                eprintln!("{header}\n  could not run: {why}");
                kept = false;
                summaries.push(json!({"key": pattern.key, "name": pattern.name, "error": why}));
                continue;
            }
        };
        let ok = outcome.verdict.kept() && outcome.came_up;
        kept &= ok;
        println!("{header} — {}", if ok { "kept" } else { "VIOLATED" });
        if !outcome.came_up {
            println!(
                "VIOLATED came up: not settled within {:.0} s before the motion",
                pattern.warm
            );
        }
        for line in outcome.verdict.lines() {
            println!("  {line}");
        }
        if let Some(log) = &outcome.log {
            println!("  log: {}", log.display());
        }
        summaries.push(json!({
            "key": pattern.key,
            "name": pattern.name,
            "device": device_name(pattern.device),
            "size": [pattern.size.0, pattern.size.1],
            "scale": pattern.scale,
            "eyes": pattern.eyes.count(),
            "refresh_hz": pattern.refresh_hz,
            "came_up": outcome.came_up,
            "launch_seconds": outcome.launch,
            "log": outcome.log,
            "verdict": outcome.verdict.to_json(),
        }));
    }
    let (endpoints, metadata) = proxy.sessions();
    let seconds = began.elapsed().as_secs_f64();
    println!(
        "{} pattern(s) in {seconds:.0} s, {} — catalog sessions opened: {endpoints} asset \
         endpoints, {metadata} imagery metadata",
        asked.patterns.len(),
        if kept {
            "every obligation kept"
        } else {
            "obligations VIOLATED"
        }
    );
    let summary = json!({
        "version": 1,
        "kept": kept,
        "seconds": seconds,
        "sessions": {"asset_endpoints": endpoints, "imagery_metadata": metadata},
        "patterns": summaries,
    });
    let path = asked
        .json
        .unwrap_or_else(|| asked.logs.join("summary.json"));
    match std::fs::write(&path, format!("{summary:#}\n")) {
        Ok(()) => println!("summary: {}", path.display()),
        Err(e) => eprintln!("{}: {e}", path.display()),
    }
    std::process::exit(if kept { 0 } else { 1 });
}
