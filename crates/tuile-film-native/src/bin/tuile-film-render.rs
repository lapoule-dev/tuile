// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Renders a film natively from its packs and the tile store.
//!
//! ```text
//! tuile-film-render <packs prefix | one pack's key> [options]
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
//!   --calibrate <dir>     fit the film's own grade on its imagery — a
//!                         continuous field that brings its tiles to one
//!                         another and gives two neighbours the same along
//!                         the edge they share, then the film to the look's
//!                         target, all of it bounded —
//!                         and write it under <dir> at the key it is kept
//!                         by, beside each pack — with, under <dir>, the
//!                         mosaic of each level before and after, and
//!                         trace/: every computation of the fit as CSV
//!                         tables. Renders with no grade; --every and
//!                         --scale make it quick.
//!   --reference-level <n> with --calibrate: the finest level at which the
//!                         imagery is one homogeneous picture, which the
//!                         finer tiles are measured against (default 12)
//!   --measure <moments|curves|linear>
//!                         with --calibrate: what a tile is measured by —
//!                         its moments, its transfer curve a channel (the
//!                         default), or the line that lays it on the
//!                         reference layer, a band at a time
//!   --reference-layer <name>
//!                         with --calibrate: a layer of the store that is
//!                         one homogeneous picture of the ground, which
//!                         every tile is set against place for place — what
//!                         the linear measure is taken against
//!   --reference-cap <n>   the film's level past which that layer gets no
//!                         finer (default 14)
//!   --toward <0..1>       with --measure linear or matrix: the dose of the
//!                         reference's look the film takes, from none (0:
//!                         its tiles are brought together, in the look of
//!                         its own imagery) to all of it (1). Unless given:
//!                         TUILE_REFERENCE_DOSE from the environment, else
//!                         three tenths
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
//!   --sun <azimuth,elevation>
//!                         where the look's sun stands, in degrees, as seen
//!                         from the first frame rendered: clockwise from
//!                         north, and over the horizon. Unless given, the
//!                         look's own: along the Earth's axis
//!   --shadow <0..1>       how much of the sun ground loses where other
//!                         ground stands before it (default 0: no shadows)
//!   --haze                air between the eye and the ground, and a sky
//!                         from horizon to zenith: the look's clear day
//!   --haze-density <per m>, --haze-height <m>
//!                         that air's extinction at the ellipsoid, and the
//!                         height over which it thins by e
//!   --anchor <level>      the level --meter's own solve holds still (10)
//!   --cache <dir>         chunks of packs and archives (default
//!                         $TUILE_CACHE_DIR, else ./film-cache)
//!   --ribbon <points>     an overlay to see the overlay layer by, not a
//!                         feature: a ribbon through these points, each
//!                         `longitude,latitude,height` in degrees and
//!                         metres over the ellipsoid, a space between two
//!   --ribbon-width <m>    its width (default 30)
//!   --ribbon-depth <terrain|always>
//!                         hidden by nearer ground (the default), or never
//!   --marker <point>      another: a marker standing at this point, never
//!                         hidden
//!   --marker-size <m>     its height (default 80)
//! ```
//!
//! A first argument ending in `.tuilepack` is one pack, opened alone: a
//! part of a film whose packs lie beside the other parts'.
//!
//! The packs are in the bucket `TUILE_STORE_BUCKET`, the tile store in
//! `TUILE_TILES_BUCKET`, both signed for by the `TUILE_STORE_*` variables.

use std::path::PathBuf;
use std::sync::Arc;

use glam::DVec3;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::{BakedView, OverlayDepth, OverlayMesh, Overlays};

use tuile_farm::{BucketConfig, ObjectRunStore, RunStore, Tuning};
use tuile_film_native::{
    render, Av1Film, Error, Film, LightMeter, Nothing, Observer, Order, Pictures, Reference, Sink,
    Sources, Tone,
};
use tuile_radiometry::{
    Bounds, CornerField, FieldBounds, FilmGrade, Local, LookTarget, MatrixBounds, MatrixField,
    Measure, TileBounds, TileGains, TileSeen,
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

/// The same shapes in every frame: `--ribbon` and `--marker`.
struct Shapes(Vec<OverlayMesh>);

/// `longitude,latitude,height`, in degrees and metres over the ellipsoid:
/// where it is, and up there.
fn place(point: &str) -> Result<(DVec3, DVec3), Error> {
    let at: Vec<f64> = point.split(',').map(str::parse).collect::<Result<_, _>>()?;
    let [lon, lat, height] = at[..] else {
        return Err(format!("{point} is not longitude,latitude,height").into());
    };
    let at = Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height,
    };
    Ok((geodetic_to_ecef(at), enu_frame(at).z_axis))
}

/// A band `width` metres across through `points`, level across at each: a
/// path in the air, climbing and diving as its points do.
fn ribbon(points: &str, width: f64, depth: OverlayDepth) -> Result<OverlayMesh, Error> {
    let path: Vec<(DVec3, DVec3)> = points
        .split_whitespace()
        .map(place)
        .collect::<Result<_, _>>()?;
    if path.len() < 2 {
        return Err("a ribbon goes through two points at least".into());
    }
    let origin = path[0].0;
    let mut mesh = OverlayMesh {
        origin_ecef: origin.to_array(),
        depth,
        ..OverlayMesh::default()
    };
    // Display-linear, opaque: an orange no ground is, one edge deeper than
    // the other so that a turn reads as one.
    let edges = [[0.72, 0.1, 0.0, 1.0], [1.0, 0.3, 0.03, 1.0]];
    for (i, (at, up)) in path.iter().enumerate() {
        let along = path[(i + 1).min(path.len() - 1)].0 - path[i.saturating_sub(1)].0;
        let across = along.cross(*up).normalize_or_zero() * (width / 2.0);
        for (edge, color) in [*at - across, *at + across].into_iter().zip(edges) {
            mesh.positions.push((edge - origin).as_vec3().to_array());
            mesh.colors.push(color);
        }
        if i > 0 {
            let v = 2 * i as u32;
            mesh.indices
                .extend_from_slice(&[v - 2, v - 1, v, v, v - 1, v + 1]);
        }
    }
    Ok(mesh)
}

/// A marker `size` metres tall standing on its point at `point`, tested
/// against nothing: it shows through whatever stands before it.
fn marker(point: &str, size: f64) -> Result<OverlayMesh, Error> {
    let (at, up) = place(point)?;
    let east = DVec3::Z.cross(up).normalize_or_zero();
    let north = up.cross(east);
    let (waist, half) = (up * (size * 0.6), size * 0.3);
    let mut mesh = OverlayMesh {
        origin_ecef: at.to_array(),
        positions: vec![[0.0; 3], (up * size).as_vec3().to_array()],
        // Its point deep, its top white, its waist between.
        colors: vec![[0.0, 0.1, 0.5, 1.0], [1.0, 1.0, 1.0, 1.0]],
        depth: OverlayDepth::Always,
        ..OverlayMesh::default()
    };
    for corner in [east, north, -east, -north] {
        mesh.positions
            .push((waist + corner * half).as_vec3().to_array());
        mesh.colors.push([0.0, 0.75, 1.0, 1.0]);
    }
    for side in 0..4u32 {
        let (a, b) = (2 + side, 2 + (side + 1) % 4);
        mesh.indices.extend_from_slice(&[0, a, b, 1, b, a]);
    }
    Ok(mesh)
}

impl Overlays for Shapes {
    fn frame(&mut self, _: u32, _: &BakedView, out: &mut Vec<OverlayMesh>) {
        out.extend(self.0.iter().cloned());
    }
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
    if let Some(strength) = value("--shadow") {
        order.look.shadow = strength.parse()?;
    }
    if flag("--haze") || value("--haze-density").is_some() || value("--haze-height").is_some() {
        let mut haze = tuile_film::Haze::default();
        if let Some(density) = value("--haze-density") {
            haze.density = density.parse()?;
        }
        if let Some(height) = value("--haze-height") {
            haze.scale_height = height.parse()?;
        }
        order.look.haze = Some(haze);
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

    // The packs: a directory (`TUILE_STORE_DIR`) or a bucket. The tile store:
    // somebody else's server of the store routes (`TUILE_TILES_REMOTE`, see
    // `tuile_repository::HttpGet::from_env`) or a bucket.
    let runs: Arc<dyn Objects> = match std::env::var("TUILE_STORE_DIR") {
        Ok(dir) if !dir.is_empty() => Arc::new(ObjectRunStore::local(
            std::path::Path::new(&dir),
            Tuning::from_env(),
        )?),
        _ => bucket(&std::env::var("TUILE_STORE_BUCKET")?)?,
    };
    // The blocks of a remote store, counted where they cross the network.
    let mut remote_blocks: Option<Arc<tuile_repository::Kept<tuile_repository::HttpGet>>> = None;
    // Their store, to be closed on the way out: what it has not written by
    // then is lost, and downloaded again by the next render.
    let mut block_store: Option<tuile_storage_foyer::FoyerStore> = None;
    let tiles: Arc<dyn Objects> = match tuile_repository::HttpGet::from_env()? {
        Some(get) => {
            println!("tile store: {}/store, by its routes", get.root());
            // A block is downloaded once: kept in a store of its own beside
            // the film's other caches — memory over disk — for this render
            // and the next. Sized well above a film's blocks: what the store
            // gives up is downloaded again.
            let blocks =
                tuile_storage_foyer::FoyerStore::keeping(tuile_storage_foyer::StoreConfig {
                    dir: PathBuf::from(&cache).join("store-blocks"),
                    memory_bytes: 256 << 20,
                    disk_bytes: 32 << 30,
                    default_ttl: None,
                })
                .await?;
            block_store = Some(blocks.clone());
            let kept = Arc::new(tuile_repository::Kept::new(get, Arc::new(blocks)));
            remote_blocks = Some(kept.clone());
            Arc::new(tuile_repository::RemoteStore::new(kept, "store"))
        }
        None => bucket(&std::env::var("TUILE_TILES_BUCKET")?)?,
    };
    let sources = Sources::open(runs, tiles, &PathBuf::from(cache)).await?;
    let film = if prefix.ends_with(".tuilepack") {
        Film::of(sources.packs.objects.as_ref(), &[prefix.as_str()]).await?
    } else {
        Film::open(sources.packs.objects.as_ref(), prefix).await?
    };
    let (first, last) = film.frames();
    println!(
        "{prefix}: {} packs, frames {first}–{last}",
        film.packs.len()
    );

    if let Some(sun) = value("--sun") {
        let (azimuth, elevation) = sun
            .split_once(',')
            .ok_or("--sun is <azimuth>,<elevation>")?;
        let (azimuth, elevation) = (
            azimuth.parse::<f64>()?.to_radians(),
            elevation.parse::<f64>()?.to_radians(),
        );
        // East, north and up where the first frame rendered is seen from.
        let from = order.frames.map_or(first, |f| f.0);
        let pack = film
            .packs
            .iter()
            .find(|p| (p.first..=p.last).contains(&from))
            .ok_or("no pack holds the first frame")?;
        let eye = tuile_film::Pack::open_table(&pack.head)?
            .view_of(from)?
            .position;
        let local = enu_frame(tuile_core::geo::ecef_to_geodetic(DVec3::from_array(eye)));
        let level = local.x_axis * azimuth.sin() + local.y_axis * azimuth.cos();
        order.look.to_sun = (level * elevation.cos() + local.z_axis * elevation.sin()).as_vec3();
    }

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

    let mut shapes = Shapes(Vec::new());
    if let Some(points) = value("--ribbon") {
        shapes.0.push(ribbon(
            &points,
            value("--ribbon-width").map_or(Ok(30.0), |w| w.parse())?,
            match value("--ribbon-depth").as_deref() {
                Some("always") => OverlayDepth::Always,
                Some("terrain") | None => OverlayDepth::Terrain,
                Some(other) => {
                    return Err(format!("a ribbon is tested terrain or always, not {other}").into())
                }
            },
        )?);
    }
    if let Some(point) = value("--marker") {
        let size = value("--marker-size").map_or(Ok(80.0), |s| s.parse())?;
        shapes.0.push(marker(&point, size)?);
    }

    let done = render(&sources, &film, &order, &mut shapes, &mut sink, observer).await?;
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
    if let Some((fine, coarse)) = done.shadow_texel {
        println!(
            "shadows: a texel of the sun's map is {fine:.1} to {coarse:.1} m of ground, frame by frame"
        );
    }
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
    if let Some(blocks) = &remote_blocks {
        let (downloaded, confirmed, held) = blocks.blocks();
        println!("store blocks: {downloaded} downloaded from the store's server, {confirmed} confirmed unchanged by it (no body sent), {held} answered from those kept here without asking");
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
        // A reference picture from another layer, under every tile of the
        // film whatever its level: what a line is fitted against.
        let reference_layer = value("--reference-layer");
        if let Some(under) = &reference_layer {
            if !sources
                .store
                .tiles
                .layers()
                .iter()
                .any(|l| l.name == *under)
            {
                return Err(format!("the store has no layer {under}").into());
            }
            let cap = value("--reference-cap").map_or(Ok(14), |c| c.parse())?;
            let mut pictured = Reference::new(under.clone(), sources.store.scheme_of(under), cap);
            let set = pictured
                .set_under(
                    &sources.store.tiles,
                    &sources.store.scheme_of(&layer),
                    &mut observed,
                )
                .await?;
            println!(
                "reference layer {under}: {} tiles of it read, {} not there; of the film's {} tiles, {} have it under all of them, {} under none",
                set.read, set.absent, set.tiles, set.whole, set.bare
            );
            // Read as any layer of the store is: its catalog, manifests and
            // tables kept with their validators, its archives by chunks.
            let (asked, kept, chunks) = (
                sources.live.so_far(),
                sources.revalidations(),
                sources.archives.reads(),
            );
            println!(
                "  the store's small objects since the render began: {} asked of the bucket — {} unchanged (no body sent), {} downloaded ({:.3} MB); its archives: {} reads asked of the cache, {} of them gone to the bucket for ({:.1} MB)",
                asked.reads,
                kept.unchanged,
                kept.fetched,
                megabytes(kept.fetched_bytes),
                chunks.0.reads,
                chunks.1.reads,
                megabytes(chunks.1.bytes),
            );
            // Every tile's line as it was fitted, and what the lines leave:
            // how far the reference stands from what a line says, by the
            // light it says — were that to bend, a line would not be enough.
            let trace = PathBuf::from(&dir).join("trace/linear");
            std::fs::create_dir_all(&trace)?;
            let mut lines = String::from(
                "level,x,y,usage,places,gain_r,gain_g,gain_b,bias_r,bias_g,bias_b,ratio_r,ratio_g,ratio_b,agreement_r,agreement_g,agreement_b,trust_r,trust_g,trust_b,weight\n",
            );
            // Half a stop a bin, from nine stops under white.
            const BINS: usize = 18;
            let mut left = [[(0.0f64, 0.0f64, 0.0f64); BINS]; 3];
            for (at, tile) in &observed.tiles {
                let Some(fit) = tile.paired.as_deref().and_then(|p| p.fit()) else {
                    continue;
                };
                let line = &fit.line;
                let three = |v: [f32; 3]| format!("{:.6},{:.6},{:.6}", v[0], v[1], v[2]);
                let weight = fit.places.iter().map(|p| p.2).sum::<f32>() / fit.places.len() as f32;
                lines += &format!(
                    "{},{},{},{:.1},{},{},{},{},{},{},{weight:.4}\n",
                    at.0,
                    at.1,
                    at.2,
                    tile.usage,
                    line.places,
                    three(line.gain),
                    three(line.bias),
                    three(line.ratio),
                    three(line.agreement),
                    three(line.trust),
                );
                for (stored, shown, weight) in &fit.places {
                    for band in 0..3 {
                        let said = line.gain[band] * stored[band] + line.bias[band];
                        if said <= 0.0 || shown[band] <= 0.0 {
                            continue;
                        }
                        let bin = ((said.log2() + 9.0) * 2.0).floor();
                        if (0.0..BINS as f32).contains(&bin) {
                            let (off, w) =
                                (f64::from((shown[band] / said).log2()), f64::from(*weight));
                            let b = &mut left[band][bin as usize];
                            *b = (b.0 + w * off, b.1 + w, b.2 + w * off * off);
                        }
                    }
                }
            }
            let mut residuals = String::from("band,stops,weight,mean_stops,spread_stops\n");
            for (band, bins) in left.iter().enumerate() {
                for (bin, (sum, weight, squares)) in bins.iter().enumerate() {
                    if *weight > 0.0 {
                        let mean = sum / weight;
                        residuals += &format!(
                            "{},{:.2},{weight:.1},{mean:.5},{:.5}\n",
                            ["r", "g", "b"][band],
                            bin as f32 / 2.0 - 9.0 + 0.25,
                            (squares / weight - mean * mean).max(0.0).sqrt(),
                        );
                    }
                }
            }
            std::fs::write(trace.join("lines.csv"), lines)?;
            std::fs::write(trace.join("residuals.csv"), residuals)?;
            std::fs::create_dir_all(&dir)?;
            for level in 1..=22u8 {
                let path = PathBuf::from(&dir).join(format!("reference-{level}.png"));
                if let Some((wide, high)) = tuile_film_native::reference::picture(
                    &observed,
                    level,
                    order.look.exposure_ev,
                    &path,
                )? {
                    println!(
                        "  level {level} and the reference under it: {} ({wide}×{high})",
                        path.display()
                    );
                }
            }
        }
        // The grade written beside the pack: the continuous field, the
        // tiles measured by their transfer curves unless told otherwise.
        let measure = match value("--measure").as_deref() {
            Some("moments") => Measure::Moments,
            Some("curves") | None => Measure::Curves,
            Some("linear" | "matrix") if reference_layer.is_some() => Measure::Linear,
            Some("linear" | "matrix") => {
                return Err("a line is fitted against a reference: --reference-layer".into())
            }
            Some(other) => {
                return Err(
                    format!("a tile is measured by moments, curves or linear, not {other}").into(),
                )
            }
        };
        // A line is against a reference that is no tile of the film: every
        // tile is measured, whatever its level.
        let toward: f32 = match (measure, value("--toward")) {
            (Measure::Linear, Some(toward)) => toward.parse()?,
            // Else what the environment says, else the dose chosen by eye.
            (Measure::Linear, None) => match std::env::var("TUILE_REFERENCE_DOSE") {
                Ok(dose) => dose.parse()?,
                Err(_) => MatrixBounds::DOSE,
            },
            _ => 0.0,
        };
        let field_bounds = FieldBounds {
            reference_level: if measure == Measure::Linear {
                0
            } else {
                reference
            },
            toward,
            ..FieldBounds::default()
        };
        // The grade written beside the pack: with a function a tile in the
        // field's place, if that is what was asked for.
        let with_matrices = value("--measure").as_deref() == Some("matrix");
        let grade = if with_matrices {
            FilmGrade::fit_matrix(
                &observed,
                light,
                &LookTarget::default(),
                &Bounds::default(),
                &MatrixBounds {
                    toward,
                    ..MatrixBounds::default()
                },
            )
        } else {
            FilmGrade::fit(
                &observed,
                measure,
                light,
                &LookTarget::default(),
                &Bounds::default(),
                &field_bounds,
            )
        };
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
        // and the film as it is, are judged by the same rule.
        let lift = order.look.exposure_ev + grade.exposure_ev;
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
            // Against the same reference as before it was corrected.
            for (at, tile) in &mut seen.tiles {
                if let Some(was) = observed.tiles.get(at) {
                    tile.set_against_as(was);
                }
            }
            let measures = [Measure::Moments, Measure::Curves, Measure::Linear];
            let measures = &measures[..if reference_layer.is_some() { 3 } else { 2 }];
            for measure in measures.iter().copied() {
                let mut text = format!("level,x,y,usage,{}\n", measure.names().join(","));
                let above = if measure == Measure::Linear {
                    0
                } else {
                    reference
                };
                for (at, tile) in seen.tiles.iter().filter(|(at, _)| at.0 > above) {
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
        // The earlier fits, against a level of the film's own imagery, for
        // comparison — not when the grade is fitted on lines: they are a
        // third of a gigabyte of mosaics that say nothing of it.
        let earlier: &[Measure] = if measure == Measure::Linear {
            &[]
        } else {
            &[Measure::Moments, Measure::Curves]
        };
        for measure in earlier.iter().copied() {
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
            // Against the level of the film's own imagery, whatever the
            // grade written is fitted against.
            let against_level = FieldBounds {
                reference_level: reference,
                ..FieldBounds::default()
            };
            let (field, of_field, trace) =
                CornerField::solve_traced(&observed, measure, &against_level);
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
        if reference_layer.is_some() {
            let (field, of_field, trace) =
                CornerField::solve_traced(&observed, Measure::Linear, &field_bounds);
            for (name, table) in trace.tables() {
                write("trace/field-linear", name, table)?;
            }
            println!(
                "\nfield-linear: {} of {} tiles measured; {} seams of {} edges in all, of {}; widest break along an edge that is not a seam: {}; steps made where the tiles show none: {}\n  the tiles' step across the seams: {:.2} / {:.2} stops → {:.2} / {:.2}; apart from the film's own tone: {:.2} / {:.2} → {:.2} / {:.2}; {} corners held at a bound",
                of_field.measured,
                of_field.tiles,
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
                of_field.held,
            );
            judge("field-linear", &|at, u, v| field.at(at, u, v))?;
            // A fitted function a tile — where in it and what colour in,
            // the colour out: traced, and drawn in the field's place.
            if value("--measure").as_deref() == Some("matrix") {
                let (matrices, of_matrices, look) = MatrixField::solve(
                    &observed,
                    &MatrixBounds {
                        toward,
                        ..MatrixBounds::default()
                    },
                );
                println!(
                    "\nmatrix: {} of {} tiles with places to fit on, {} places, {} corners; {} seam edges of {}\n  from what is wanted, in stops of luminance (median / p95): {:.3} / {:.3} → {:.3} / {:.3}\n  the film's look on ground {:?}\n  and on water {:?}",
                    of_matrices.measured,
                    of_matrices.tiles,
                    of_matrices.places,
                    of_matrices.corners,
                    of_matrices.seam_edges,
                    of_matrices.edges,
                    of_matrices.before.0,
                    of_matrices.before.1,
                    of_matrices.after.0,
                    of_matrices.after.1,
                    look.ground,
                    look.water,
                );
                let numbers = |m: &[[f32; 4]; 3]| {
                    m.iter()
                        .flatten()
                        .map(|v| format!("{v:.6}"))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let mut corners = String::from("level,x,y,corner,m\n");
                for (at, four) in &matrices.given {
                    for (corner, matrix) in four.iter().enumerate() {
                        corners +=
                            &format!("{},{},{},{corner},{}\n", at.0, at.1, at.2, numbers(matrix));
                    }
                }
                write(
                    "trace/matrix",
                    "corners.csv",
                    corners.replace("corner,m\n", "corner,rr,rg,rb,r1,gr,gg,gb,g1,br,bg,bb,b1\n"),
                )?;
                write(
                    "trace/matrix",
                    "look.csv",
                    format!(
                        "zone,rr,rg,rb,r1,gr,gg,gb,g1,br,bg,bb,b1,toward\nground,{},{toward}\nwater,{},{toward}\n",
                        numbers(&look.ground),
                        numbers(&look.water)
                    ),
                )?;
                let mut seams = String::from("level,x,y,upright\n");
                for (at, upright) in &of_matrices.seams {
                    seams += &format!("{},{},{},{}\n", at.0, at.1, at.2, u8::from(*upright));
                }
                write("trace/matrix", "seams.csv", seams)?;
                judge("matrix", &|at, u, v| Local {
                    matrix: Some(matrices.at(at, u, v)),
                    ..Local::IDENTITY
                })?;
            }
            // The same field at several doses of the reference, one above
            // the other from none to all of it: the tiles are brought
            // together the same in each, and only what the film is given
            // back of its own standing against the reference changes. For
            // choosing the dose by eye.
            const DOSES: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
            for level in 1..=22u8 {
                // The field's doses, that is: not drawn for a function a
                // tile, which has its own.
                if meter.extent(level).is_none() || level <= 12 || with_matrices {
                    continue;
                }
                let mut rows = Vec::new();
                for dose in DOSES {
                    let bounds = FieldBounds {
                        toward: dose,
                        ..field_bounds
                    };
                    // Each with the exposure the film would be given at
                    // that dose: a film brought to a darker reference is
                    // lifted by more.
                    let dosed = FilmGrade::fit(
                        &observed,
                        Measure::Linear,
                        light,
                        &LookTarget::default(),
                        &Bounds::default(),
                        &bounds,
                    );
                    let lifted = order.look.exposure_ev + dosed.exposure_ev;
                    let dosed = dosed.field;
                    let path = out.join(format!("dose-{level}.tmp.png"));
                    if meter
                        .mosaic_with(level, |at, u, v| dosed.at(at, u, v), lifted, &path)?
                        .is_none()
                    {
                        continue;
                    }
                    let full = image::open(&path)?.to_rgb8();
                    std::fs::remove_file(&path)?;
                    rows.push(image::imageops::resize(
                        &full,
                        full.width() / 4,
                        full.height() / 4,
                        image::imageops::FilterType::Triangle,
                    ));
                }
                let Some(first) = rows.first() else { continue };
                let (wide, high) = first.dimensions();
                let mut sheet = image::RgbImage::new(wide, (high + 8) * rows.len() as u32);
                for (row, picture) in rows.iter().enumerate() {
                    image::imageops::replace(
                        &mut sheet,
                        picture,
                        0,
                        (row as u32 * (high + 8)).into(),
                    );
                }
                let path = out.join(format!("doses-{level}.png"));
                sheet.save(&path)?;
                println!(
                    "  the field at doses {DOSES:?} of the reference, top to bottom: {}",
                    path.display()
                );
            }
            // The three side by side — one above the other, a film being
            // wider than high: the reference, the film as the store holds
            // it, the film as the field leaves it. Half the mosaics' size.
            if let Some(under) = &reference_layer {
                let cap = value("--reference-cap").map_or(Ok(14), |c| c.parse())?;
                let mut pictured =
                    Reference::new(under.clone(), sources.store.scheme_of(under), cap);
                for level in 1..=22u8 {
                    let Some((tiles, extent)) = meter.extent(level) else {
                        continue;
                    };
                    let (was, is) = (
                        out.join(format!("mosaic-{level}-as-it-is.png")),
                        out.join(format!(
                            "mosaic-{level}-{}.png",
                            if value("--measure").as_deref() == Some("matrix") {
                                "matrix"
                            } else {
                                "field-linear"
                            }
                        )),
                    );
                    if !(was.exists() && is.exists()) {
                        continue;
                    }
                    const SIDE: u32 = 32;
                    let above = pictured
                        .mosaic(
                            &sources.store.tiles,
                            &sources.store.scheme_of(&layer),
                            &tiles,
                            extent,
                            SIDE,
                            lift,
                        )
                        .await?;
                    let (wide, high) = above.dimensions();
                    let mut three = image::RgbImage::new(wide, high * 3 + 16);
                    image::imageops::replace(&mut three, &above, 0, 0);
                    for (row, path) in [(1u32, &was), (2, &is)] {
                        let full = image::open(path)?.to_rgb8();
                        let half = image::imageops::resize(
                            &full,
                            wide,
                            high,
                            image::imageops::FilterType::Triangle,
                        );
                        image::imageops::replace(&mut three, &half, 0, i64::from(row * (high + 8)));
                    }
                    let path = out.join(format!("triptych-{level}.png"));
                    three.save(&path)?;
                    println!(
                        "  the reference, the film as it is, the film corrected: {} ({wide}×{})",
                        path.display(),
                        high * 3 + 16
                    );
                }
            }
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
    if let Some(blocks) = &block_store {
        blocks.close().await?;
    }
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
        "  the tiles to one another: a continuous field, the tiles measured by their {}\n  {} tiles finer than the reference, {} measured against it; {} seams of {} edges in all, of {}\n  widest break along an edge that is not a seam: {} stops; steps made where the tiles show none: {}\n  the tiles' step across the seams: {:.2} / {:.2} stops → {:.2} / {:.2} (median / p95)\n  apart from the film's own tone: {:.2} / {:.2} → {:.2} / {:.2}; {} tiles untouched, {} corners held at a bound\n",
        grade.field.measure.name(),
        r.tiles,
        r.measured,
        r.seams,
        r.seam_edges,
        r.edges,
        r.widest_break,
        r.steps_made,
        r.seam_before.0,
        r.seam_before.1,
        r.seam_after.0,
        r.seam_after.1,
        r.apart_before.0,
        r.apart_before.1,
        r.apart_after.0,
        r.apart_after.1,
        r.untouched,
        r.held,
    );
    if !grade.limited.is_empty() {
        out += &format!("  held back at a bound: {}\n", grade.limited.join("; "));
    }
    out
}
