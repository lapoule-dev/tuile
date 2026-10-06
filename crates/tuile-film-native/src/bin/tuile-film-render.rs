// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Renders a film natively from its packs and the tile store.
//!
//! ```text
//! tuile-film-render <packs prefix> [options]
//!
//!   --out <film.mp4>      the film, in an mp4
//!   --codec <name>        av1: rav1e, in software, anywhere.
//!                         h264: the machine's own encoder — VideoToolbox
//!                         on macOS (feature `videotoolbox`), NVENC on
//!                         Linux (feature `nvenc`).
//!                         nvenc-av1, nvenc-h264: an NVIDIA card's encoder
//!                         (feature `nvenc`; card chosen by
//!                         TUILE_CUDA_DEVICE, default 0).
//!                         Default: the machine's own where one was built
//!                         in (AV1 on NVIDIA), rav1e otherwise.
//!   --pictures <dir>      one PNG a frame
//!   --meter <dir>         measure light going in and coming out; write
//!                         tiles.csv, frames.csv, tone.json, report.md,
//!                         and each place's table of grades under the key
//!                         a tile store keeps it by (<layer>/tone/…)
//!   --frames <a:b>        first and last frame (default: the whole film)
//!   --every <n>           one frame in n (default 1)
//!   --scale <s>           picture size against the bake's (default 1)
//!   --supersample <n>     default 2
//!   --fps <n>             default 30
//!   --mbps <n>            default 12
//!   --no-tone             imagery as stored: no tone correction
//!   --tone-table <file>   this tone table, not the one made from the
//!                         tables the store keeps for the film's places
//!   --tone <0..1>         how much of the correction (default 1)
//!   --imagery <decoded|stored>
//!                         how imagery's values are read before lighting:
//!                         decoded from sRGB as a photograph asks (the
//!                         default), or as they lie, washed, as one Cycles
//!                         film this was once calibrated on
//!   --exposure <stops>    the look's exposure
//!   --contrast <power>    the look's contrast, about middle grey
//!   --saturation <factor> the look's saturation
//!   --anchor <level>      the level --meter's own solve holds still (10)
//!   --cache <dir>         chunks of packs and archives (default
//!                         $TUILE_CACHE_DIR, else ./film-cache)
//! ```
//!
//! The packs are in the bucket `TUILE_STORE_BUCKET`, the tile store in
//! `TUILE_TILES_BUCKET`, both signed for by the `TUILE_STORE_*` variables.

use std::path::PathBuf;
use std::sync::Arc;

use tuile_farm::{BucketConfig, ObjectRunStore, Tuning};
use tuile_film_native::{
    render, Av1Film, Error, Film, LightMeter, Nothing, Observer, Order, Pictures, Sink, Sources,
    Tone,
};
use tuile_radiometry::LevelGrades;
use tuile_repository::Objects;

fn bucket(name: &str) -> Result<Arc<dyn Objects>, Error> {
    let config = BucketConfig {
        bucket: name.to_string(),
        ..BucketConfig::from_env()?
    };
    Ok(Arc::new(ObjectRunStore::bucket(
        &config,
        Tuning::from_env(),
    )?))
}

/// The pictures to several sinks at once.
struct Both(Vec<Box<dyn Sink>>);

impl Sink for Both {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        self.0
            .iter_mut()
            .try_for_each(|s| s.open(width, height, fps))
    }
    fn wants_i420(&self) -> bool {
        self.0.iter().any(|s| s.wants_i420())
    }
    fn picture(&mut self, index: u32, frame: u32, rgba: &[u8], i420: &[u8]) -> Result<(), Error> {
        self.0
            .iter_mut()
            .try_for_each(|s| s.picture(index, frame, rgba, i420))
    }
    fn close(&mut self) -> Result<(), Error> {
        self.0.iter_mut().try_for_each(|s| s.close())
    }
    fn spent(&self) -> Vec<(String, f64)> {
        self.0.iter().flat_map(|s| s.spent()).collect()
    }
}

/// The film's encoder: the one asked for, or the best this binary has —
/// the machine's own before software.
fn film_sink(path: String, bitrate: u32, codec: Option<&str>) -> Result<Box<dyn Sink>, Error> {
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    if matches!(codec, Some("h264") | None) {
        return Ok(Box::new(tuile_film_native::H264Film::at(path, bitrate)));
    }
    #[cfg(all(target_os = "linux", feature = "nvenc"))]
    {
        use tuile_film_native::{NvencCodec, NvencFilm};
        let device = std::env::var("TUILE_CUDA_DEVICE")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0);
        match codec {
            Some("nvenc-av1") | None => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::Av1).on_device(device),
                ))
            }
            Some("nvenc-h264") | Some("h264") => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::H264).on_device(device),
                ))
            }
            _ => {}
        }
    }
    match codec {
        Some("av1") | None => Ok(Box::new(Av1Film::at(path, bitrate))),
        Some(other) => Err(format!("no {other} encoder was built into this binary").into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);
    let prefix = args
        .first()
        .filter(|a| !a.starts_with("--"))
        .ok_or("usage: tuile-film-render <packs prefix> [--out film.mp4] [--meter dir] …")?;

    let mut order = Order::default();
    if let Some(frames) = value("--frames") {
        let (a, b) = frames.split_once(':').ok_or("--frames is <first>:<last>")?;
        order.frames = Some((a.parse()?, b.parse()?));
    }
    if let Some(n) = value("--every") {
        order.every = n.parse()?;
    }
    if let Some(s) = value("--scale") {
        order.scale = s.parse()?;
    }
    if let Some(n) = value("--supersample") {
        order.supersample = n.parse()?;
    }
    if let Some(n) = value("--fps") {
        order.fps = n.parse()?;
    }
    if let Some(t) = value("--tone") {
        order.tone_strength = t.parse()?;
    }
    if let Some(path) = value("--tone-table") {
        let text = std::fs::read_to_string(&path)?;
        order.tone = Tone::Table(
            LevelGrades::from_json(&text).ok_or_else(|| format!("{path} is not a tone table"))?,
        );
    }
    match value("--imagery").as_deref() {
        Some("stored") => order.look = tuile_film::Look::cycles_film(),
        Some("decoded") | None => {}
        Some(other) => return Err(format!("imagery is read stored or decoded, not {other}").into()),
    }
    if let Some(stops) = value("--exposure") {
        order.look.exposure_ev = stops.parse()?;
    }
    if let Some(power) = value("--contrast") {
        order.look.contrast = power.parse()?;
    }
    if let Some(factor) = value("--saturation") {
        order.look.saturation = factor.parse()?;
    }
    if flag("--no-tone") {
        order.tone = Tone::Off;
    }
    let mbps: f64 = value("--mbps").map_or(Ok(12.0), |m| m.parse())?;
    let anchor: u8 = value("--anchor").map_or(Ok(10), |a| a.parse())?;
    let cache = value("--cache")
        .or_else(|| std::env::var("TUILE_CACHE_DIR").ok())
        .unwrap_or_else(|| "film-cache".into());

    let sources = Sources::open(
        bucket(&std::env::var("TUILE_STORE_BUCKET")?)?,
        bucket(&std::env::var("TUILE_TILES_BUCKET")?)?,
        &PathBuf::from(cache),
    )
    .await?;
    let film = Film::open(sources.packs.objects.as_ref(), prefix).await?;
    let (first, last) = film.frames();
    println!(
        "{prefix}: {} packs, frames {first}–{last}",
        film.packs.len()
    );

    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    if let Some(path) = value("--out") {
        let bitrate = (mbps * 1e6) as u32;
        let codec = value("--codec");
        let film: Box<dyn Sink> = film_sink(path, bitrate, codec.as_deref())?;
        sinks.push(film);
    }
    if let Some(dir) = value("--pictures") {
        sinks.push(Box::new(Pictures::into(dir)));
    }
    if sinks.is_empty() {
        sinks.push(Box::new(Nothing));
    }
    let mut sink = Both(sinks);

    let mut meter = LightMeter::default();
    let metering = value("--meter");
    let mut nobody = ();
    let observer: &mut dyn Observer = if metering.is_some() {
        &mut meter
    } else {
        &mut nobody
    };

    let done = render(&sources, &film, &order, &mut sink, observer).await?;
    let megabytes = |b: u64| b as f64 / 1e6;
    println!(
        "{} frames at {}×{} in {:.1} s ({:.2} frames/s), {:.1} s of it before the first frame",
        done.frames,
        done.width,
        done.height,
        done.seconds,
        f64::from(done.frames) / done.seconds.max(1e-9),
        done.setup_seconds
    );
    // Where the time went: the frame that enters everything, then the rest.
    let after = f64::from(done.frames.saturating_sub(1)).max(1.0);
    println!(
        "\n{:<18} {:>14} {:>16} {:>12} {:>7}",
        "step", "opening frame", "frames after", "per frame", "share"
    );
    let whole = done.opening.total() + done.timings.total();
    for ((name, opening), (_, rest)) in done.opening.steps().iter().zip(done.timings.steps()) {
        println!(
            "{name:<18} {:>11.0} ms {:>13.1} s {:>9.1} ms {:>6.1}%",
            opening,
            rest / 1000.0,
            rest / after,
            (opening + rest) * 100.0 / whole.max(1e-9)
        );
    }
    println!(
        "{:<18} {:>11.0} ms {:>13.1} s {:>9.1} ms   ({} tiles entered by the opening frame)",
        "all steps",
        done.opening.total(),
        done.timings.total() / 1000.0,
        done.timings.total() / after,
        done.opening_tiles
    );
    for (name, seconds) in sink.spent() {
        println!(
            "{name}: {seconds:.1} s, {:.1} ms a frame",
            seconds * 1000.0 / f64::from(done.frames.max(1))
        );
    }
    println!();
    println!(
        "tiles: {} from the store, {} from the pack, {} source tiles renewed since the bake; tone: {}",
        done.tiles_from_store,
        done.tiles_from_pack,
        done.renewed,
        match (&done.tone, order.tone_strength) {
            (None, _) if done.tone_places.0 > 0 => format!(
                "none — the store keeps no table for any of the film's {} places, nor one for the layer",
                done.tone_places.0
            ),
            (None, _) => "none".to_string(),
            (Some(t), s) => format!(
                "one grade a level, anchor {}, strength {s}{}",
                t.anchor,
                match done.tone_places {
                    (0, ..) => String::new(),
                    (places, fitted, layer) => format!(
                        ", from the store: {fitted} of the film's {places} places have a table of their own{}",
                        if layer { ", the others the layer's" } else { ", the others none" }
                    ),
                }
            ),
        }
    );
    for (name, cache) in [("packs", &sources.packs), ("archives", &sources.archives)] {
        let (asked, fetched) = cache.reads();
        println!(
            "{name}: {} reads ({:.1} MB) asked, {} ({:.1} MB) fetched from the bucket",
            asked.reads,
            megabytes(asked.bytes),
            fetched.reads,
            megabytes(fetched.bytes)
        );
    }
    let live = sources.live.so_far();
    println!(
        "store catalog and manifests: {} reads ({:.2} MB), from the bucket each time",
        live.reads,
        megabytes(live.bytes)
    );
    if let Some(dir) = metering {
        let solved = meter.solve(anchor);
        let report = meter.write(&PathBuf::from(&dir), prefix, &solved)?;
        if let Some((_, layer)) = film
            .packs
            .first()
            .and_then(|p| tuile_film::Pack::open_table(&p.head).ok())
            .as_ref()
            .and_then(|p| p.store_layers())
        {
            let keys = meter.write_places(&PathBuf::from(&dir), layer, anchor)?;
            println!(
                "tables of {} places written under {dir}/{layer}/tone/",
                keys.len()
            );
        }
        println!("\n{report}");
    }
    Ok(())
}
