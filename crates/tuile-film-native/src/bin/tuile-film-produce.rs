// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A whole film from a tape of cameras, in one command.
//!
//! ```text
//! tuile-film-produce flight.mcap
//! tuile-film-produce flight.mcap --resolution 3840x2160 --fps 30 --sse 3 --imagery 3954
//! ```
//!
//! The tape's frames are cut in packs and each pack in parts; each pack is
//! baked, each part rendered to a segment, several packs at once
//! (`--at-once`), and the segments are joined. All of it in this process —
//! the bake and the render are the engine's own, called — and all of it kept
//! in the run store: the packs under `packs/<scene>…/`, the parts and the
//! film under `films/<scene>…/<look>/`, where the scene names everything a
//! pack depends on and the look everything a picture does beyond its pack.
//!
//! So what was made is found: a pack already baked with these parameters is
//! not baked again, a part already rendered with them is not rendered
//! again, and the same command stopped and asked again goes on from where
//! it was. `--bake-again` and `--render-again` say otherwise. Changing a
//! parameter of the picture renders another film from the same packs;
//! changing one of the bake bakes other packs.
//!
//! The stores are the environment's:
//!
//! ```text
//! TUILE_STORE_ENDPOINT, TUILE_STORE_BUCKET,
//! TUILE_STORE_ACCESS_KEY_ID, TUILE_STORE_SECRET_ACCESS_KEY   the run store, a bucket
//! TUILE_STORE_DIR                                            …or a directory, instead
//! TUILE_TILES_REMOTE (+ _HEADER, _SECRET) or TUILE_TILES_BUCKET   the tile store
//! TUILE_ION_TOKEN                                            the bake's sources
//! ```
//!
//! Nothing is left on this machine: steps write to a scratch directory, and
//! what the render reads of the stores is cached under `--cache`, both gone
//! when the film is made unless `--cache` names a directory to keep.
//!
//! Exit codes: 0 the film is in the store (its key is printed, `FILM <key>`),
//! 1 it is not (the parts missing are named), 2 usage, 3 stopped — by an
//! object named `STOP` under the film's prefix, or by a step that did not
//! return within `--step-minutes`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tuile_bake::tape::TapeBake;
use tuile_farm::produce::{produce, Host, Journal, Order, ProduceError, Stretch};
use tuile_farm::{ObjectRunStore, RunStore};
use tuile_film_native::engine::{Engine, RenderSpec, Rendering, StereoPacking, StereoSpec};
use tuile_film_native::setup::tile_store;
use tuile_film_native::Error;

#[derive(Parser)]
#[command(
    name = "tuile-film-produce",
    about = "Bake, render in parts and join a whole film, kept in the run store"
)]
struct Args {
    /// The tape of cameras, one frame a camera: a file here, or
    /// `store:<key>`, an object of the run store.
    tape: String,
    /// First and last frame, `first:last`. Default: the whole tape.
    #[arg(long, value_parser = stretch)]
    frames: Option<Stretch>,

    // ── what is baked ──────────────────────────────────────────────────
    /// The picture's size, `<width>x<height>`.
    #[arg(long, default_value = "1920x1080", value_parser = resolution)]
    resolution: (u32, u32),
    /// The selection's largest screen-space error, in pixels. Default: the
    /// engine's.
    #[arg(long)]
    sse: Option<f64>,
    /// The imagery layer, as its source's asset; negative for none.
    /// Default: the engine's aerial imagery.
    #[arg(long, allow_negative_numbers = true)]
    imagery: Option<i64>,
    /// The terrain layer, as its source's asset. Default: the engine's.
    #[arg(long)]
    terrain: Option<i64>,
    /// Where the ground comes from: `ion` (its terrain and imagery assets,
    /// --terrain and --imagery; a token), `open-earth` (the ellipsoid and
    /// NASA's Blue Marble; no token) or `empty` (no ground: a sky, checked
    /// on its own; only with --atmosphere).
    #[arg(long, default_value = "ion", value_parser = tuile_bake::open::Ground::parse)]
    ground: tuile_bake::open::Ground,
    /// What a pack carries of its tiles: `references` into the tile store,
    /// the tiles themselves (`embedded`), or `both`.
    #[arg(long, default_value = "references", value_parser = content)]
    content: tuile_pack::Content,

    // ── how it is rendered ─────────────────────────────────────────────
    /// Frames a second.
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// The picture's size against the size baked for.
    #[arg(long, default_value_t = 1.0)]
    scale: f32,
    /// Samples a pixel, a side.
    #[arg(long, default_value_t = 2)]
    supersample: u32,
    /// The encoder: `h264` — the machine's own (NVENC on an NVIDIA card,
    /// VideoToolbox on a Mac) — or `nvenc-h264`; for a film of two eyes,
    /// `mv-hevc` (spatial video: AVFoundation on a Mac, NVENC on a card
    /// that encodes it). A film is joined from its parts, and the join
    /// reads H.264 and spatial films.
    #[arg(long, default_value = "h264", value_parser = ["h264", "nvenc-h264", "mv-hevc", "nvenc-mv-hevc"])]
    codec: String,
    /// Megabits a second.
    #[arg(long, default_value_t = 12.0)]
    mbps: f64,
    /// The sun, `<azimuth>,<elevation>` in degrees, where the film begins.
    #[arg(long, value_parser = pair, allow_hyphen_values = true)]
    sun: Option<(f64, f64)>,
    /// Stops of exposure.
    #[arg(long, allow_negative_numbers = true)]
    exposure: Option<f32>,
    #[arg(long)]
    contrast: Option<f32>,
    #[arg(long)]
    saturation: Option<f32>,
    /// How much of the sun a shadow takes, 0 to 1.
    #[arg(long)]
    shadow: Option<f32>,
    /// The air's extinction at the ground, per metre: asks for the air.
    #[arg(long)]
    haze_density: Option<f32>,
    /// The height over which the air thins by a factor of e, in metres.
    #[arg(long)]
    haze_height: Option<f32>,
    /// The air's colour at the horizon, `<r>,<g>,<b>` linear.
    #[arg(long)]
    haze_horizon: Option<String>,
    /// The sky's colour straight up, `<r>,<g>,<b>` linear.
    #[arg(long)]
    haze_zenith: Option<String>,
    /// The sun's light, `<r>,<g>,<b>` linear.
    #[arg(long)]
    sun_light: Option<String>,
    /// The sky's light, `<r>,<g>,<b>` linear.
    #[arg(long)]
    sky_light: Option<String>,
    /// The imagery as stored, without the film's tone correction.
    #[arg(long)]
    no_tone: bool,
    /// The physical atmosphere: sky, sun, Moon, air and light from the
    /// air (`tuile_film::Look::atmosphere`).
    #[arg(long)]
    atmosphere: bool,
    /// The atmosphere's turbidity (2 a clear day); implies --atmosphere.
    #[arg(long)]
    turbidity: Option<f32>,
    /// Each frame's instant, `<start>,<seconds a frame>`: every frame lit by
    /// its own sun, Moon and stars. Default: the instants the tape states
    /// (`tuile-film-timelapse` writes them into it).
    #[arg(long, allow_hyphen_values = true)]
    clock: Option<String>,
    /// Towns lit at night, under --atmosphere: NASA's Black Marble (public
    /// domain), fetched once and kept in the run store by its content.
    #[arg(long)]
    night_lights: bool,
    /// The sun's disc drawn this many times its size (1: as it is).
    #[arg(long)]
    sun_scale: Option<f32>,
    /// The Moon's disc drawn this many times its size (1: as it is).
    #[arg(long)]
    moon_scale: Option<f32>,
    /// The lens's glare about the sun and the Moon (1: a clean lens's, 0:
    /// none).
    #[arg(long)]
    glare: Option<f32>,
    /// For two eyes, packed into one picture: `side-by-side` or `top-bottom`.
    #[arg(long, value_parser = ["side-by-side", "top-bottom"])]
    stereo: Option<String>,
    /// Each eye squeezed into half the picture, for a 3D television.
    #[arg(long, requires = "stereo")]
    stereo_half: bool,
    /// How deep the furthest ground goes behind the screen, as a fraction
    /// of an eye's width.
    #[arg(long, requires = "stereo")]
    stereo_divergence: Option<f64>,
    /// The screen's distance from the eyes, metres; else where the camera's
    /// axis meets the ground.
    #[arg(long, requires = "stereo")]
    stereo_convergence: Option<f64>,
    /// The ground's height over the ellipsoid, metres, for the convergence.
    #[arg(long, requires = "stereo")]
    stereo_ground: Option<f64>,
    /// The eyes this many metres apart, whatever the distance (hyperstereo).
    #[arg(long, requires = "stereo")]
    stereo_baseline: Option<f64>,

    // ── how it is produced ─────────────────────────────────────────────
    /// Frames to a pack.
    #[arg(long, default_value_t = 450)]
    pack_frames: u32,
    /// Frames to a part: never more than a pack's. Each part pays its own
    /// start — its tiles read, its first frame — so few long parts are
    /// rendered sooner than many short ones.
    #[arg(long, default_value_t = 450)]
    part_frames: u32,
    /// Packs worked on at once, each on a thread: so many bakes or renders
    /// run together. A 4K render holds 4 to 6.5 GB.
    #[arg(long, default_value_t = 1)]
    at_once: usize,
    /// Times a bake or a render is tried.
    #[arg(long, default_value_t = 3)]
    tries: u32,
    /// Minutes a bake or a render may take before the production stops.
    #[arg(long)]
    step_minutes: Option<u64>,
    /// Bake the packs the parts to render need, even those already baked.
    #[arg(long)]
    bake_again: bool,
    /// Render every part, even those already rendered.
    #[arg(long)]
    render_again: bool,
    /// Where what the render reads of the stores is kept while it runs.
    /// Default: a scratch directory, gone at the end.
    #[arg(long)]
    cache: Option<PathBuf>,
    /// This process's share of a production several run at once,
    /// `<index>/<count>`: the packs `index`, `index + count`, … Default: what
    /// Cloud Run says (`CLOUD_RUN_TASK_INDEX`, `CLOUD_RUN_TASK_COUNT`), else
    /// every pack. Every task is given the same command; the last to finish
    /// joins the film.
    #[arg(long, value_parser = share)]
    task: Option<(u32, u32)>,
    /// Also bring the film here once it is in the store.
    #[arg(long)]
    download: Option<PathBuf>,
    /// Once the film is in the store, write at this key of the run store a
    /// receipt — `{"film": <key>, "frames": <n>}` — for whoever launched the
    /// production elsewhere to find it by (`tuile-launch-job film`).
    #[arg(long)]
    receipt: Option<String>,
}

fn stretch(text: &str) -> Result<Stretch, String> {
    let (first, last) = text.split_once(':').ok_or("expected <first>:<last>")?;
    let number = |n: &str| n.parse::<u32>().map_err(|e| format!("{n}: {e}"));
    let frames = Stretch {
        first: number(first)?,
        last: number(last)?,
    };
    if frames.first == 0 || frames.first > frames.last {
        return Err(format!(
            "{}:{} is not a range of frames",
            frames.first, frames.last
        ));
    }
    Ok(frames)
}

fn share(text: &str) -> Result<(u32, u32), String> {
    let (index, count) = text.split_once('/').ok_or("expected <index>/<count>")?;
    let number = |n: &str| n.parse::<u32>().map_err(|e| format!("{n}: {e}"));
    let (index, count) = (number(index)?, number(count)?);
    if index >= count {
        return Err(format!("task {index} of {count}"));
    }
    Ok((index, count))
}

/// This process's share: the one asked for, else the one Cloud Run gives a
/// task of a job, else none — every pack. A job of one task has no share.
fn task_of(
    asked: Option<(u32, u32)>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<(u32, u32)>, String> {
    if asked.is_some() {
        return Ok(asked);
    }
    match (env("CLOUD_RUN_TASK_INDEX"), env("CLOUD_RUN_TASK_COUNT")) {
        (Some(index), Some(count)) => {
            let (index, count) = share(&format!("{index}/{count}"))?;
            Ok((count > 1).then_some((index, count)))
        }
        _ => Ok(None),
    }
}

fn resolution(text: &str) -> Result<(u32, u32), String> {
    let (width, height) = text.split_once('x').ok_or("expected <width>x<height>")?;
    let side = |n: &str| match n.parse::<u32>() {
        Ok(side) if side > 0 => Ok(side),
        _ => Err(format!("{n} is not a number of pixels")),
    };
    Ok((side(width)?, side(height)?))
}

fn pair(text: &str) -> Result<(f64, f64), String> {
    let (a, b) = text
        .split_once(',')
        .ok_or("expected <azimuth>,<elevation>")?;
    let degrees = |n: &str| n.trim().parse::<f64>().map_err(|e| format!("{n}: {e}"));
    Ok((degrees(a)?, degrees(b)?))
}

fn content(text: &str) -> Result<tuile_pack::Content, String> {
    match text {
        "references" => Ok(tuile_pack::Content::References),
        "embedded" => Ok(tuile_pack::Content::Embedded),
        "both" => Ok(tuile_pack::Content::Both),
        other => Err(format!(
            "a pack carries references, embedded or both, not {other}"
        )),
    }
}

/// How the film is rendered, as the command line says.
/// The rendering `args` ask for; the tape's clock, if it states one, when
/// `--clock` does not give another.
fn rendering(
    args: &Args,
    tape_clock: Option<tuile_tape::Clock>,
    night_lights: Option<String>,
) -> Result<Rendering, Error> {
    RenderSpec {
        fps: args.fps,
        scale: args.scale,
        supersample: args.supersample,
        codec: args.codec.clone(),
        mbps: args.mbps,
        sun: args.sun,
        exposure: args.exposure,
        contrast: args.contrast,
        saturation: args.saturation,
        shadow: args.shadow,
        haze_density: args.haze_density,
        haze_height: args.haze_height,
        haze_horizon: args.haze_horizon.clone(),
        haze_zenith: args.haze_zenith.clone(),
        sun_light: args.sun_light.clone(),
        sky_light: args.sky_light.clone(),
        no_tone: args.no_tone,
        atmosphere: args.atmosphere,
        turbidity: args.turbidity,
        clock: args
            .clock
            .clone()
            .or_else(|| tape_clock.map(|c| c.to_arg())),
        night_lights: night_lights.clone(),
        sun_scale: args.sun_scale,
        moon_scale: args.moon_scale,
        glare: args.glare,
        stereo: args.stereo.as_deref().map(|packing| StereoSpec {
            packing: if packing == "top-bottom" {
                StereoPacking::TopBottom
            } else {
                StereoPacking::SideBySide
            },
            half: args.stereo_half,
            divergence: args
                .stereo_divergence
                .unwrap_or(tuile_film::DEFAULT_DIVERGENCE),
            convergence_m: args.stereo_convergence,
            ground_m: args.stereo_ground.unwrap_or(0.0),
            baseline_m: args.stereo_baseline,
        }),
        ..RenderSpec::default()
    }
    .rendering()
}

/// The run store, as a place a bake keeps what it fetched once.
struct Keep(Arc<ObjectRunStore>);

#[async_trait::async_trait]
impl tuile_bake::NightLightsStore for Keep {
    async fn keep(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        if self.0.exists(key).await.map_err(|e| e.to_string())? {
            return Ok(());
        }
        let file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        std::fs::write(file.path(), bytes).map_err(|e| e.to_string())?;
        self.0
            .put(file.path(), key)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// What it came to, as an exit code.
enum Ended {
    Film,
    NoFilm,
    Usage,
    Stopped,
    /// This task's share is done; another joins the film.
    Waiting,
}

async fn run(args: Args) -> Result<Ended, Error> {
    let task = task_of(args.task, |name| std::env::var(name).ok())?;
    // The run store: a bucket, unless the environment names a directory.
    let store = Arc::new(ObjectRunStore::from_env()?);
    // What the render reads is kept here while the film is made.
    let scratch = tempfile::tempdir()?;
    let tape = match args.tape.strip_prefix("store:") {
        Some(key) => {
            let here = scratch.path().join("tape.mcap");
            store.get(key, &here).await?;
            here
        }
        None => PathBuf::from(&args.tape),
    };
    let poses = TapeBake::poses_of(&tape)?;
    let whole = Stretch {
        first: 1,
        last: poses.len() as u32,
    };
    let frames = args.frames.unwrap_or(whole);
    if frames.last > whole.last {
        eprintln!(
            "tuile-film-produce: frames {}:{} of a tape that holds {}",
            frames.first, frames.last, whole.last
        );
        return Ok(Ended::Usage);
    }
    if args.ground == tuile_bake::open::Ground::Empty
        && !(args.atmosphere || args.turbidity.is_some())
    {
        eprintln!(
            "tuile-film-produce: --ground empty draws nothing but a sky: it wants --atmosphere"
        );
        return Ok(Ended::Usage);
    }
    let tape_clock = tuile_tape::Clock::of_tape(&tape).map_err(|e| e.to_string())?;
    // The night lights, kept in the run store once, by their content: the
    // render reads them from there and nowhere else.
    let night_lights = if args.night_lights {
        let keep = Keep(store.clone());
        let cache = std::env::var_os("TUILE_CACHE_DIR").map(Into::into);
        Some(tuile_bake::open::publish_night_lights(&keep, cache).await?)
    } else {
        None
    };
    let rendering = rendering(&args, tape_clock, night_lights)?;
    let bake = TapeBake {
        // Needed only if a pack is to be baked: a production whose packs are
        // there renders without it, and a bake without it says so.
        token: std::env::var("TUILE_ION_TOKEN").unwrap_or_default(),
        cache_dir: std::env::var("TUILE_CACHE_DIR").ok().map(Into::into),
        poses,
        viewport: (f64::from(args.resolution.0), f64::from(args.resolution.1)),
        sse: args.sse,
        terrain: args.terrain,
        imagery: args.imagery,
        content: args.content,
        ground: args.ground,
    };

    let cache = args
        .cache
        .clone()
        .unwrap_or_else(|| scratch.path().to_path_buf());
    let tiles = tile_store(&cache).await?;
    let engine = Engine::open(
        bake,
        rendering,
        frames.first,
        store.clone(),
        tiles.objects.clone(),
        &cache,
    )
    .await?;
    let host: Arc<dyn Host> = Arc::new(engine);

    let order = Order {
        pack_frames: args.pack_frames,
        part_frames: args.part_frames,
        at_once: args.at_once,
        tries: args.tries,
        step_timeout: args
            .step_minutes
            .map(|minutes| Duration::from_secs(minutes * 60)),
        fps: Some(f64::from(args.fps.max(1))),
        bake_again: args.bake_again,
        render_again: args.render_again,
        task,
        ..Order::of(frames)
    };
    let journal = Journal::default();
    let made = produce(&order, store.as_ref(), host, &journal).await;
    if let Some(blocks) = &tiles.block_store {
        // What it has not written by now is downloaded again next time.
        let _ = blocks.close().await;
    }
    match made {
        Ok(film) => {
            if let Some(dest) = &args.download {
                store.get(&film.key, dest).await?;
                println!("FILM-HERE {}", dest.display());
            }
            println!("FILM {} {} frames", film.key, film.frames);
            if let Some(key) = &args.receipt {
                let receipt = serde_json::json!({"film": film.key, "frames": film.frames});
                let here = scratch.path().join("receipt.json");
                std::fs::write(&here, receipt.to_string())?;
                store.put(&here, key).await?;
            }
            Ok(Ended::Film)
        }
        Err(ProduceError::Order(why)) => {
            eprintln!("tuile-film-produce: {why}");
            Ok(Ended::Usage)
        }
        Err(e @ ProduceError::Waiting { .. }) => {
            println!("FILM-WAITING {e}");
            Ok(Ended::Waiting)
        }
        Err(e @ (ProduceError::Stopped(_) | ProduceError::Stalled { .. })) => {
            println!("FILM-STOPPED {e}");
            Ok(Ended::Stopped)
        }
        Err(e) => {
            println!("FILM-FAILED {e}");
            Ok(Ended::NoFilm)
        }
    }
}

fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tuile-film-produce: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let ended = runtime.block_on(run(args));
    // A step that did not return holds its thread for ever: the runtime is
    // not waited for.
    runtime.shutdown_background();
    match ended {
        Ok(Ended::Film | Ended::Waiting) => ExitCode::SUCCESS,
        Ok(Ended::NoFilm) => ExitCode::FAILURE,
        Ok(Ended::Usage) => ExitCode::from(2),
        Ok(Ended::Stopped) => ExitCode::from(3),
        Err(e) => {
            eprintln!("tuile-film-produce: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use tuile_film::Haze;
    use tuile_film_native::Tone;

    fn parsed(line: &str) -> Result<Args, clap::Error> {
        Args::try_parse_from(line.split_whitespace())
    }

    #[test]
    fn the_command_line_is_one_clap_accepts() {
        Args::command().debug_assert();
    }

    #[test]
    fn a_tape_alone_is_a_film_with_every_default() {
        let args = parsed("tuile-film-produce flight.mcap").expect("parse");
        assert_eq!(args.frames, None);
        assert_eq!(args.resolution, (1920, 1080));
        assert_eq!(
            (
                args.fps,
                args.pack_frames,
                args.part_frames,
                args.at_once,
                args.tries
            ),
            (30, 450, 450, 1, 3)
        );
        assert_eq!(args.content, tuile_pack::Content::References);
        assert!(!args.bake_again && !args.render_again);
        assert!(args.imagery.is_none() && args.sse.is_none() && args.cache.is_none());
        // H.264, the one encoding whose parts join: the machine's own encoder
        // gives it, whichever the machine has.
        assert_eq!(args.codec, "h264");
        let rendering = rendering(&args, None, None).expect("rendering");
        assert_eq!(rendering.bitrate, 12_000_000);
        assert!(rendering.order.look.haze.is_none());
    }

    #[test]
    fn what_is_said_of_the_film_is_what_it_is_made_with() {
        let args = parsed(
            "tuile-film-produce flight.mcap --frames 12800:14599 --resolution 3840x2160 --fps 25 \
             --sse 3 --imagery 3954 --terrain 1 --content both --sun 255,10 --exposure -0.5 \
             --shadow 1 --haze-horizon 1.0,0.42,0.16 --sun-light 3.2,1.7,0.8 --mbps 45 \
             --codec h264 --no-tone --at-once 2 --render-again --step-minutes 30",
        )
        .expect("parse");
        assert_eq!(
            args.frames,
            Some(Stretch {
                first: 12800,
                last: 14599
            })
        );
        assert_eq!(args.resolution, (3840, 2160));
        assert_eq!(
            (args.sse, args.imagery, args.terrain),
            (Some(3.0), Some(3954), Some(1))
        );
        assert_eq!(args.content, tuile_pack::Content::Both);
        assert!(args.render_again && !args.bake_again);
        assert_eq!((args.at_once, args.step_minutes), (2, Some(30)));
        let rendering = rendering(&args, None, None).expect("rendering");
        assert_eq!(rendering.order.fps, 25);
        assert_eq!(rendering.sun, Some((255.0, 10.0)));
        assert_eq!(rendering.bitrate, 45_000_000);
        assert_eq!(rendering.codec.as_deref(), Some("h264"));
        assert!(matches!(rendering.order.tone, Tone::Off));
        let look = rendering.order.look;
        assert_eq!((look.exposure_ev, look.shadow), (-0.5, 1.0));
        assert_eq!(look.sun.to_array(), [3.2, 1.7, 0.8]);
        // Naming one end of the air asks for the air, the rest of it as it is.
        let haze = look.haze.expect("haze");
        assert_eq!(haze.horizon.to_array(), [1.0, 0.42, 0.16]);
        assert_eq!(haze.density, Haze::default().density);
        // No imagery at all is a negative asset.
        assert_eq!(
            parsed("tuile-film-produce t.mcap --imagery -1")
                .expect("parse")
                .imagery,
            Some(-1)
        );
    }

    #[test]
    fn a_task_s_share_is_the_one_asked_else_cloud_run_s_else_none() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let cloud = env(&[("CLOUD_RUN_TASK_INDEX", "3"), ("CLOUD_RUN_TASK_COUNT", "8")]);
        assert_eq!(task_of(None, cloud), Ok(Some((3, 8))));
        assert_eq!(task_of(Some((1, 2)), cloud), Ok(Some((1, 2))));
        assert_eq!(task_of(None, env(&[])), Ok(None));
        // A job of one task, or half of what Cloud Run says: every pack.
        let alone = env(&[("CLOUD_RUN_TASK_INDEX", "0"), ("CLOUD_RUN_TASK_COUNT", "1")]);
        assert_eq!(task_of(None, alone), Ok(None));
        assert_eq!(
            task_of(None, env(&[("CLOUD_RUN_TASK_INDEX", "2")])),
            Ok(None)
        );
        // What cannot be a share is refused, not taken for every pack.
        let wrong = env(&[("CLOUD_RUN_TASK_INDEX", "8"), ("CLOUD_RUN_TASK_COUNT", "8")]);
        assert!(task_of(None, wrong).is_err());
        assert_eq!(
            parsed("tuile-film-produce t.mcap --task 2/5")
                .expect("parse")
                .task,
            Some((2, 5))
        );
        assert!(parsed("tuile-film-produce t.mcap --task 5/5").is_err());
    }

    #[test]
    fn what_cannot_be_a_film_is_refused_at_the_command_line() {
        for line in [
            "tuile-film-produce",
            "tuile-film-produce t.mcap --frames 9:3",
            "tuile-film-produce t.mcap --frames 0:3",
            "tuile-film-produce t.mcap --resolution 1920",
            "tuile-film-produce t.mcap --resolution 0x1080",
            "tuile-film-produce t.mcap --content pictures",
            "tuile-film-produce t.mcap --sun 255",
            // Parts that cannot be joined make no film.
            "tuile-film-produce t.mcap --codec av1",
            "tuile-film-produce t.mcap --codec nvenc-av1",
        ] {
            assert!(parsed(line).is_err(), "{line}");
        }
        let args = parsed("tuile-film-produce t.mcap --sky-light 1,2").expect("parse");
        assert!(rendering(&args, None, None).is_err());
    }
}
