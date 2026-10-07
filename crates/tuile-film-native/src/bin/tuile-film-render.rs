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
//!   --no-tone             no grade: imagery as stored, the look as it is
//!   --tone-table <file>   this grade, not the film's own: a film's grade
//!                         as --calibrate writes it
//!   --tone <0..1>         how much of the grade (default 1)
//!   --calibrate <dir>     fit the film's own grade on its imagery — its
//!                         tiles brought to one another, those that
//!                         already meet kept as they are, then the film to
//!                         the look's target, all of it bounded —
//!                         and write it under <dir> at the key it is kept
//!                         by, beside each pack — with, under <dir>, the
//!                         mosaic of each level before and after, and
//!                         trace/: every computation of the fit as CSV
//!                         tables. Renders with no grade; --every and
//!                         --scale make it quick.
//!   --reference-level <n> with --calibrate: the finest level at which the
//!                         imagery is one homogeneous picture, which the
//!                         finer tiles are measured against (default 12)
//!   --put                 with --calibrate: also write it to the bucket
//!   --light <x>           with --calibrate: the light the renderer puts
//!                         on ground, instead of the one this render shows
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

use tuile_farm::{BucketConfig, ObjectRunStore, RunStore, Tuning};
use tuile_film_native::{
    render, Av1Film, Error, Film, LightMeter, Nothing, Observer, Order, Pictures, Sink, Sources,
    Tone,
};
use tuile_radiometry::{
    Bounds, CornerField, FieldBounds, FilmGrade, Local, LookTarget, Measure, TileBounds, TileGains,
    TileSeen,
};
use tuile_repository::tone::{pack_seen_key, pack_tone_key};
use tuile_repository::Objects;
use tuile_repository::TileRepository;

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
        let grade =
            FilmGrade::from_json(&text).ok_or_else(|| format!("{path} is not a film's grade"))?;
        order.tone = Tone::Grade(grade);
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
    let calibrating = value("--calibrate");
    if flag("--no-tone") || calibrating.is_some() {
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
    let observer: &mut dyn Observer = if metering.is_some() || calibrating.is_some() {
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
        "tiles: {} from the store, {} from the pack, {} source tiles renewed since the bake",
        done.tiles_from_store, done.tiles_from_pack, done.renewed,
    );
    match &done.grade {
        None => println!("grade: none — the film has none of its own, and borrows none"),
        Some(grade) => println!(
            "grade: the film's own, strength {}\n{}",
            order.tone_strength,
            said(grade)
        ),
    }
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
    let (asked, kept) = (sources.live.so_far(), sources.revalidations());
    println!(
        "store catalog, manifests and tables: {} asked of the bucket — {} unchanged (no body sent), {} downloaded ({:.3} MB), {} not there; {} more answered from what was kept without asking",
        asked.reads,
        kept.unchanged,
        kept.fetched,
        megabytes(kept.fetched_bytes),
        kept.absent,
        kept.kept
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
        for level in 0..=22u8 {
            let path = PathBuf::from(&dir).join(format!("seams-{level}.png"));
            if let Some((wide, high)) = meter.seams_picture(level, None, &path)? {
                println!("seams of level {level}: {} ({wide}×{high})", path.display());
            }
        }
        let joints = meter.joints();
        std::fs::write(PathBuf::from(&dir).join("joints.md"), &joints)?;
        println!("\n{report}\n{joints}");
    }
    if let Some(dir) = calibrating {
        let layer = film
            .packs
            .first()
            .and_then(|p| tuile_film::Pack::open_table(&p.head).ok())
            .as_ref()
            .and_then(|p| p.store_layers())
            .map(|(_, imagery)| imagery.to_string())
            .ok_or("the film's packs name no imagery layer: nothing to fit a grade on")?;
        if !sources.store.is_graded(&layer).await? {
            return Err(format!(
                "{layer} is not a graded layer: its store keeps no {layer}/tone.json"
            )
            .into());
        }
        let shown = meter.light().ok_or("no picture came out")?;
        let light = value("--light").map_or(Ok(shown), |l| l.parse())?;
        // What the render saw, and under it the reference level the tiles
        // are measured against — which a film need not draw, and is read
        // from the store here.
        let mut tile_bounds = TileBounds::default();
        if let Some(level) = value("--reference-level") {
            tile_bounds.reference_level = level.parse()?;
        }
        let mut observed = meter.observed();
        let reference = tile_bounds.reference_level;
        let wanted: std::collections::BTreeSet<(u32, u32)> = observed
            .tiles
            .keys()
            .filter(|at| at.0 > reference)
            .map(|at| (at.1 >> (at.0 - reference), at.2 >> (at.0 - reference)))
            .filter(|at| !observed.tiles.contains_key(&(reference, at.0, at.1)))
            .collect();
        let mut missing = 0usize;
        for (x, y) in &wanted {
            let seen = match sources.store.tiles.tile(&layer, reference, *x, *y).await? {
                Some(tile) => image::load_from_memory(&tile.bytes)
                    .ok()
                    .and_then(|decoded| {
                        let rgba = decoded.to_rgba8();
                        TileSeen::of_rgba8(&rgba, rgba.width(), rgba.height())
                    }),
                None => None,
            };
            missing += usize::from(seen.is_none());
            observed.see((reference, *x, *y), || seen, 0.0);
        }
        println!(
            "\nreference level {reference}: {} tiles read from the store for it, {missing} not there",
            wanted.len()
        );
        let grade = FilmGrade::fit(
            &observed,
            light,
            &LookTarget::default(),
            &Bounds::default(),
            &tile_bounds,
        );
        // What was seen, for a film of several packs to be fitted as one;
        // and the seams of each level, before and after.
        let seen = observed.to_bytes();
        println!(
            "\nlight on ground: {shown:.3} shown by this render, {light:.3} used\n{}",
            said(&grade)
        );
        // Two ways of fitting, each with two ways of measuring, on the same
        // observations: a gain a block and a continuous field, the tiles
        // measured by their moments and by their transfer curves. Each is
        // traced and drawn, and what each leaves is measured again — the
        // corrected tiles seen anew, by both measures — so that the four,
        // and the film as it is, are judged by the same rule. The grade
        // written below is still the blocks', by moments.
        let lift = order.look.exposure_ev + grade.exposure_ev;
        let field_bounds = FieldBounds {
            reference_level: reference,
            ..FieldBounds::default()
        };
        let out = PathBuf::from(&dir);
        let write = |sub: &str, name: &str, text: String| -> Result<(), Error> {
            let path = out.join(sub);
            std::fs::create_dir_all(&path)?;
            std::fs::write(path.join(name), text)?;
            Ok(())
        };
        // The reference tiles, to set corrected tiles against.
        let under: Vec<_> = observed
            .tiles
            .iter()
            .filter(|(at, _)| at.0 <= reference)
            .map(|(at, tile)| (*at, tile.clone()))
            .collect();
        let judge = |version: &str,
                     correction: &dyn Fn((u8, u32, u32), f32, f32) -> Local|
         -> Result<(), Error> {
            let mut seen = meter.corrected(correction);
            for (at, tile) in &under {
                seen.tiles.entry(*at).or_insert_with(|| tile.clone());
            }
            for measure in [Measure::Moments, Measure::Curves] {
                let mut text = format!("level,x,y,usage,{}\n", measure.names().join(","));
                for (at, tile) in seen.tiles.iter().filter(|(at, _)| at.0 > reference) {
                    let Some(found) = measure.of(&seen, *at, reference) else {
                        continue;
                    };
                    let numbers: Vec<String> = found
                        .iter()
                        .map(|v| {
                            if v.is_finite() {
                                format!("{v:.5}")
                            } else {
                                String::new()
                            }
                        })
                        .collect();
                    text += &format!(
                        "{},{},{},{:.1},{}\n",
                        at.0,
                        at.1,
                        at.2,
                        tile.usage,
                        numbers.join(",")
                    );
                }
                write(
                    &format!("eval/{version}"),
                    &format!("{}.csv", measure.name()),
                    text,
                )?;
            }
            for level in reference + 1..=22u8 {
                let path = out.join(format!("mosaic-{level}-{version}.png"));
                if let Some((wide, high)) = meter.mosaic_with(level, correction, lift, &path)? {
                    println!(
                        "  mosaic of level {level}: {} ({wide}×{high})",
                        path.display()
                    );
                }
            }
            Ok(())
        };
        println!("\nas it is:");
        judge("as-it-is", &|_, _, _| Local::IDENTITY)?;
        for measure in [Measure::Moments, Measure::Curves] {
            let version = format!("blocks-{}", measure.name());
            let (gains, of_blocks, trace) =
                TileGains::solve_traced(&observed, measure, &tile_bounds);
            for (name, table) in trace.tables() {
                write(&format!("trace/{version}"), name, table)?;
            }
            println!(
                "\n{version}: {} blocks, {} of {} edges a border; {} tiles untouched, {} given a gain; pairs in accord given two gains: {}",
                of_blocks.blocks,
                of_blocks.borders,
                of_blocks.edges,
                of_blocks.untouched,
                gains.stops.len(),
                of_blocks.accord_broken,
            );
            judge(&version, &|at, _, _| gains.local(at))?;

            let version = format!("field-{}", measure.name());
            let (field, of_field, trace) =
                CornerField::solve_traced(&observed, measure, &field_bounds);
            for (name, table) in trace.tables() {
                write(&format!("trace/{version}"), name, table)?;
            }
            println!(
                "\n{version}: {} seams of {} edges in all, of {}; widest break along an edge that is not a seam: {}; steps made where the tiles show none: {}\n  the tiles' step across the seams: {:.2} / {:.2} stops → {:.2} / {:.2}; apart from the film's own tone: {:.2} / {:.2} → {:.2} / {:.2}",
                of_field.seams,
                of_field.seam_edges,
                of_field.edges,
                of_field.widest_break,
                of_field.steps_made,
                of_field.seam_before.0,
                of_field.seam_before.1,
                of_field.seam_after.0,
                of_field.seam_after.1,
                of_field.apart_before.0,
                of_field.apart_before.1,
                of_field.apart_after.0,
                of_field.apart_after.1,
            );
            judge(&version, &|at, u, v| field.at(at, u, v))?;
        }
        let putting = if flag("--put") {
            let config = BucketConfig {
                bucket: std::env::var("TUILE_STORE_BUCKET")?,
                ..BucketConfig::from_env()?
            };
            Some(ObjectRunStore::bucket(&config, Tuning::from_env())?)
        } else {
            None
        };
        for pack in &film.packs {
            for (key, bytes) in [
                (pack_tone_key(&pack.key), grade.to_json().into_bytes()),
                (pack_seen_key(&pack.key), seen.clone()),
            ] {
                let path = PathBuf::from(&dir).join(&key);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, bytes)?;
                match &putting {
                    Some(store) => {
                        store.put(&path, &key).await?;
                        println!("written to the bucket: {key}");
                    }
                    None => println!("written: {}", path.display()),
                }
            }
        }
    }
    sources.close().await?;
    Ok(())
}

/// A film's grade, in words.
fn said(grade: &FilmGrade) -> String {
    let mut out = format!(
        "  the film to the target: exposure {:+.2} stops, contrast ×{:.2}, saturation ×{:.2}\n  foretold L* {:.1} → {:.1}, contrast {:.1} → {:.1}, C* {:.1} → {:.1}\n",
        grade.exposure_ev,
        grade.contrast,
        grade.saturation,
        grade.before[0],
        grade.after[0],
        grade.before[1],
        grade.after[1],
        grade.before[2],
        grade.after[2],
    );
    let r = &grade.fitted;
    out += &format!(
        "  tiles: {} finer than the reference, {} measured against it; {} blocks, {} edges of {} a border between two\n  the film's own block: {} tiles untouched; {} tiles given a gain; {} blocks held at a bound\n  pairs of neighbours in accord: {} — given two gains: {}\n  apart from the film's own block: {:.2} / {:.2} stops → {:.2} / {:.2} (median / p95)\n",
        r.tiles,
        r.measured,
        r.blocks,
        r.borders,
        r.edges,
        r.untouched,
        grade.tiles.stops.len(),
        r.held,
        r.accorded,
        r.accord_broken,
        r.apart_before.0,
        r.apart_before.1,
        r.apart_after.0,
        r.apart_after.1,
    );
    if !grade.limited.is_empty() {
        out += &format!("  held back at a bound: {}\n", grade.limited.join("; "));
    }
    out
}
