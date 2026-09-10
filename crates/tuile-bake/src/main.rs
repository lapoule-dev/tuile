// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bakes a scene's frame range into a pack. Job A of a two-job pipeline.
//!
//! ```text
//! TUILE_ION_TOKEN=… tuile-bake --tape traj.mcap --frames 1:48 --out shot.tuilepack
//! ```
//!
//! # Why this is a separate job
//!
//! Frame 1 of a farm render costs about 500 seconds and is **89 % of a
//! 48-frame job**. All of it is CPU: fetching from ion and Bing, decoding
//! quantized-mesh, resampling, draping, baking mosaics, encoding PNG. Sixteen
//! processes on one pod each paid it from zero, on a machine rented for its
//! four GPUs — of which the render was using one.
//!
//! So the two halves separate along the line that was always there. This one
//! wants cores, bandwidth and patience, and has no use for a GPU; it does not
//! belong on a rented GPU pod at all. What it leaves behind is a file, and the
//! render job opens it.
//!
//! The second effect is the one worth having. A render fed from a pack has no
//! network, no access token and no traversal: it is reproducible **by
//! construction**, not because a convergence race was stabilised but because
//! there is no race left to lose.
//!
//! # What a pack freezes
//!
//! The selection. Ground this bake did not select is bare in every render made
//! from the pack, for ever, and no count of tiles will report it — a counter is
//! blind to ground nobody selected. That is why the cull setting is written
//! into the pack rather than merely applied: see `PackWriter::culling`.

use std::sync::Arc;
use std::time::Instant;

use tuile_pack::{BakedTile, PackWriter, TextureFormat};

/// What one run was asked to do.
enum Job {
    Bake(Args),
    /// Open a pack and say what is in it. The operator's half of the split
    /// pipeline: a pack is opaque, and "36 MB" is not an answer to "is this
    /// the right bake, and does it cover the shot".
    Inspect(std::path::PathBuf),
    /// Compare two packs and say what they disagree about.
    Diff(std::path::PathBuf, std::path::PathBuf),
    /// Bake the same scene live, again, and compare it against a pack tile
    /// for tile and byte for byte.
    Verify {
        pack: std::path::PathBuf,
        tape: std::path::PathBuf,
        viewport: (f64, f64),
    },
}

struct Args {
    tape: std::path::PathBuf,
    out: std::path::PathBuf,
    first: u32,
    last: u32,
    viewport: (f64, f64),
}

const USAGE: &str = "\
usage: tuile-bake --tape <path.mcap> --frames <first>:<last> --out <path.tuilepack>
                  [--viewport <w>x<h>]
       tuile-bake --inspect <path.tuilepack>
       tuile-bake --diff <a.tuilepack> <b.tuilepack>
       tuile-bake --verify <path.tuilepack> --tape <path.mcap> [--viewport <w>x<h>]

environment:
  TUILE_ION_TOKEN    required
  TUILE_CACHE_DIR    where to keep the tile cache (strongly advised: a bake is
                     the one process that pays for a cold one)
  TUILE_*            every traversal knob the render honours is honoured here,
                     because they are read by the same code — and a pack is the
                     bake of the settings it was made with, not of the defaults
";

fn parse_args() -> Result<Job, String> {
    let mut tape = None;
    let mut out = None;
    let mut frames = None;
    let mut viewport = (1280.0, 960.0);
    let mut verify: Option<std::path::PathBuf> = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--inspect" => return Ok(Job::Inspect(value()?.into())),
            "--verify" => verify = Some(std::path::PathBuf::from(value()?)),
            "--diff" => {
                let a = std::path::PathBuf::from(value()?);
                let b = std::path::PathBuf::from(value()?);
                return Ok(Job::Diff(a, b));
            }
            "--tape" => tape = Some(std::path::PathBuf::from(value()?)),
            "--out" => out = Some(std::path::PathBuf::from(value()?)),
            "--frames" => frames = Some(value()?),
            "--viewport" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--viewport wants <w>x<h>")?;
                viewport = (
                    w.parse().map_err(|_| "--viewport width")?,
                    h.parse().map_err(|_| "--viewport height")?,
                );
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other}\n\n{USAGE}")),
        }
    }
    if let Some(pack) = verify {
        return Ok(Job::Verify {
            pack,
            // Required, and this is the point of the mode. Comparing a pack
            // against itself proves nothing: the first version of this read
            // the pack twice — once through the render's session, once
            // through the reader — and both agreed perfectly about a byte
            // that had been flipped in the file. A pack is only shown to be a
            // substitute by comparing it against the thing it substitutes.
            tape: tape.ok_or(
                "--verify needs --tape: a pack compared against itself agrees \
                 with itself, including about its own corruption",
            )?,
            viewport,
        });
    }
    let frames = frames.ok_or("--frames is required")?;
    let (first, last) = frames.split_once(':').ok_or("--frames wants <first>:<last>")?;
    let first: u32 = first.parse().map_err(|_| "--frames first")?;
    let last: u32 = last.parse().map_err(|_| "--frames last")?;
    if last < first {
        return Err(format!("--frames {first}:{last} runs backwards"));
    }
    Ok(Job::Bake(Args {
        tape: tape.ok_or("--tape is required")?,
        out: out.ok_or("--out is required")?,
        first,
        last,
        viewport,
    }))
}

/// Little-endian bytes, explicitly, rather than a view of this machine's
/// memory.
///
/// A pack is a file that another machine reads. Casting a `Vec<[f32; 3]>` to
/// bytes is one `unsafe` and encodes whatever this process happens to lay out;
/// spelling the encoding out costs a loop on a machine chosen for having time,
/// and makes the format a thing the file says rather than a thing the writer
/// remembers.
fn f32x3_le(values: &[[f32; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 12);
    for v in values {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    out
}

fn f32x2_le(values: &[[f32; 2]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 8);
    for v in values {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    out
}

fn u32_le(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// The name of the scene this pack is the bake of.
///
/// Everything that changes what a frame contains goes in, and nothing else. In
/// particular the frame range does **not**: two shards of one shot bake
/// different ranges of the same scene, and they must agree on the name of the
/// scene or nothing can tell a mismatched pack from a neighbouring one. The
/// range is in the object key beside the digest, which is where it belongs.
fn digest_of_scene(tape_bytes: &[u8], viewport: (f64, f64), traversal: &str) -> String {
    tuile_pack::scene_digest(&[
        tape_bytes,
        &viewport.0.to_le_bytes(),
        &viewport.1.to_le_bytes(),
        traversal.as_bytes(),
    ])
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TUILE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            // Loud and on stderr: a bake that half-worked and exited zero would
            // hand the render job a pack with a hole in it.
            eprintln!("BAKE-FAIL {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    match parse_args()? {
        Job::Inspect(path) => inspect(&path),
        Job::Diff(a, b) => diff(&a, &b),
        Job::Verify {
            pack,
            tape,
            viewport,
        } => verify(&pack, &tape, viewport),
        Job::Bake(args) => bake(args),
    }
}

/// Reads a pack back and reports what a render would get from it.
///
/// Deliberately reads the payloads rather than only the table: a pack whose
/// blocks do not decompress is a pack that fails on a farm node at 3 a.m., and
/// the cost of finding that out here is seconds.
fn inspect(path: &std::path::Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let pack = tuile_pack::Pack::open(&bytes).map_err(|e| e.to_string())?;
    let (first, last) = pack.frame_range();
    println!("scene    {}", pack.scene_digest());
    println!("culling  {}", pack.culling());
    println!("frames   {first}..={last}");
    println!("origin   {:?}", pack.render_origin());
    println!("tiles    {} distinct (id, drape)", pack.tile_count());
    println!("bytes    {}", bytes.len());

    let mut total_selected = 0usize;
    let mut payload_bytes = 0usize;
    for frame in first..=last {
        let tiles = pack.frame(frame).map_err(|e| e.to_string())?;
        total_selected += tiles.len();
        let mut textured = 0usize;
        for tile in &tiles {
            for (block, what) in [
                (tile.positions(), "positions"),
                (tile.normals(), "normals"),
                (tile.uvs(), "uvs"),
                (tile.indices(), "indices"),
                (tile.texture(), "texture"),
            ] {
                if let Some(block) = block {
                    payload_bytes += pack
                        .payload(block, what)
                        .map_err(|e| format!("frame {frame}: {e}"))?
                        .len();
                    if what == "texture" {
                        textured += 1;
                    }
                }
            }
        }
        let view = pack.view_of(frame).map_err(|e| e.to_string())?;
        println!(
            "  frame {frame:>5}: {:>5} tiles, {textured} textured, eye {:?}",
            tiles.len(),
            view.position
        );
    }
    // The number the whole container exists for. A shot whose frames reuse
    // their ground stores it once; one that does not is a shot the pack cannot
    // help, and that is worth knowing before a farm waits on it.
    println!(
        "reuse    {total_selected} selections over {} stored tiles ({:.1}x)",
        pack.tile_count(),
        total_selected as f64 / pack.tile_count().max(1) as f64
    );
    println!(
        "payloads {payload_bytes} bytes raw, {:.0}% of it stored",
        100.0 * bytes.len() as f64 / payload_bytes.max(1) as f64
    );
    Ok(())
}

fn bake(args: Args) -> Result<(), String> {
    let token = std::env::var("TUILE_ION_TOKEN")
        .map_err(|_| "TUILE_ION_TOKEN is not set; a bake is the one job that needs it")?;

    // The tape is read twice on purpose: once as bytes, which is what names the
    // scene, and once as frames. Naming the scene from the file rather than
    // from the poses re-derived out of it means a re-recorded tape that lands
    // one bit differently is a different scene, which is the safe direction.
    let tape_bytes = std::fs::read(&args.tape)
        .map_err(|e| format!("reading {}: {e}", args.tape.display()))?;

    let mut tape = tuile_tape::Tape::replaying(&args.tape)
        .map_err(|e| format!("opening {}: {e}", args.tape.display()))?;
    let mut poses = Vec::new();
    while let Some(frame) = tape.next_frame() {
        poses.push(frame);
    }
    if poses.is_empty() {
        return Err(format!("{} holds no camera frames", args.tape.display()));
    }
    let wanted = (args.last as usize).min(poses.len());
    if args.first as usize > wanted {
        return Err(format!(
            "--frames {}:{} but the tape holds {} frames",
            args.first,
            args.last,
            poses.len()
        ));
    }

    let mut config = tuile_hydra::GlobeConfig::new(&token);
    if let Ok(dir) = std::env::var("TUILE_CACHE_DIR") {
        config.cache_dir = Some(dir.into());
    }
    // The settings a pack is the bake of. **Resolved** first: every TUILE_*
    // knob the session honours is read by `exact_traversal`, and a digest
    // taken before that would give two packs baked at different screen-space
    // errors the same name — after which they answer for each other.
    let resolved = tuile_hydra::exact_traversal(config.session.traversal.clone());
    let scene = digest_of_scene(&tape_bytes, args.viewport, &format!("{resolved:?}"));
    let culling = if resolved.cull {
        "full"
    } else {
        "disabled"
    };
    tracing::info!(
        scene,
        culling,
        frames = format!("{}:{}", args.first, wanted),
        viewport = format!("{}x{}", args.viewport.0, args.viewport.1),
        "BAKE-BEGIN"
    );

    let began = Instant::now();
    let mut session = tuile_hydra::Session::globe(config)
        .map_err(|e| format!("opening the globe: {e}"))?;

    // The render origin the pack's positions are relative to. The pack stores
    // each tile's own ECEF origin, so this is carried for the consumer that
    // rebases, not used to move anything here.
    let origin = poses[args.first.max(1) as usize - 1].position;
    let mut writer = PackWriter::new(&scene, origin).culling(culling);

    for number in args.first..=(wanted as u32) {
        let pose = &poses[number as usize - 1];
        let view = tuile_core::traversal::ViewStateParams {
            position: glam::DVec3::from_array(pose.position),
            direction: glam::DVec3::from_array(pose.direction),
            up: glam::DVec3::from_array(pose.up),
            viewport_px: glam::dvec2(args.viewport.0, args.viewport.1),
            fovy_rad: pose.fovy,
        };
        let at = Instant::now();
        let frame = session
            .frame(vec![view])
            .map_err(|e| format!("frame {number}: {e}"))?;

        let mut tiles = Vec::with_capacity(frame.tiles.len());
        for (index, tile) in frame.tiles.iter().enumerate() {
            tiles.push(baked_tile(&frame, index, tile)?);
        }
        let selected = tiles.len();
        // The camera goes in beside the tiles, because that is what a render
        // will address this frame by: a Hydra host cooks at a timecode and
        // hands the session a camera, never a frame number.
        writer.frame(
            number,
            tuile_pack::BakedView {
                position: pose.position,
                direction: pose.direction,
                up: pose.up,
                viewport_px: [args.viewport.0, args.viewport.1],
                fovy_rad: pose.fovy,
            },
            tiles,
        );
        tracing::info!(
            frame = number,
            selected,
            seconds = at.elapsed().as_secs_f64(),
            "BAKE-FRAME"
        );
    }

    let bytes = writer.finish();
    std::fs::write(&args.out, &bytes)
        .map_err(|e| format!("writing {}: {e}", args.out.display()))?;
    tracing::info!(
        scene,
        culling,
        path = %args.out.display(),
        bytes = bytes.len(),
        seconds = began.elapsed().as_secs_f64(),
        "BAKE-DONE"
    );
    // On stdout, and parseable, because the launcher turns it into an object
    // key: `packs/<scene>/<first>-<last>.tuilepack`.
    println!("BAKE-KEY packs/{scene}/{}-{wanted}.tuilepack", args.first);
    Ok(())
}

/// Says what two packs disagree about, and at what level.
///
/// Built the moment two bakes of one scene, with one warm cache and one set of
/// settings, came out the same size and different bytes. "Different" is not a
/// finding; *which frames, which tiles, and whether it is the ground or the
/// imagery* is. The three questions are asked separately because they have
/// separate causes: a selection that differs is the traversal, a draping that
/// differs is the imagery level, and geometry that differs under an identical
/// id and draping is decoding.
fn diff(a_path: &std::path::Path, b_path: &std::path::Path) -> Result<(), String> {
    let a_bytes = std::fs::read(a_path).map_err(|e| format!("{}: {e}", a_path.display()))?;
    let b_bytes = std::fs::read(b_path).map_err(|e| format!("{}: {e}", b_path.display()))?;
    let a = tuile_pack::Pack::open(&a_bytes).map_err(|e| e.to_string())?;
    let b = tuile_pack::Pack::open(&b_bytes).map_err(|e| e.to_string())?;

    if a.scene_digest() != b.scene_digest() {
        println!(
            "scene    {} vs {} — these are bakes of different scenes",
            a.scene_digest(),
            b.scene_digest()
        );
        return Ok(());
    }
    println!("scene    {} (both)", a.scene_digest());
    if a.frame_range() != b.frame_range() {
        println!("frames   {:?} vs {:?}", a.frame_range(), b.frame_range());
    }

    let (first, last) = a.frame_range();
    let mut same = 0usize;
    for number in first..=last {
        let (Ok(ta), Ok(tb)) = (a.frame(number), b.frame(number)) else {
            println!("  frame {number}: present in only one of the two");
            continue;
        };
        let ids_a: Vec<u64> = ta.iter().map(|t| t.id()).collect();
        let ids_b: Vec<u64> = tb.iter().map(|t| t.id()).collect();
        let drapes_a: Vec<(u64, u64)> = ta.iter().map(|t| (t.id(), t.drape())).collect();
        let drapes_b: Vec<(u64, u64)> = tb.iter().map(|t| (t.id(), t.drape())).collect();

        if ids_a != ids_b {
            let set_a: std::collections::BTreeSet<_> = ids_a.iter().collect();
            let set_b: std::collections::BTreeSet<_> = ids_b.iter().collect();
            println!(
                "  frame {number}: SELECTION differs — {} vs {} tiles, {} only in \
                 the first, {} only in the second",
                ids_a.len(),
                ids_b.len(),
                set_a.difference(&set_b).count(),
                set_b.difference(&set_a).count()
            );
            continue;
        }
        let redraped = drapes_a
            .iter()
            .zip(drapes_b.iter())
            .filter(|(x, y)| x.1 != y.1)
            .count();
        if redraped > 0 {
            println!(
                "  frame {number}: same {} tiles, but {redraped} are draped \
                 differently — the imagery level, not the ground",
                ids_a.len()
            );
            continue;
        }
        // Same ground, same draping: anything left is the bytes themselves.
        let mut bytes_differ = 0usize;
        for (x, y) in ta.iter().zip(tb.iter()) {
            if a.baked(x).map_err(|e| e.to_string())? != b.baked(y).map_err(|e| e.to_string())? {
                bytes_differ += 1;
            }
        }
        if bytes_differ > 0 {
            println!(
                "  frame {number}: same {} tiles and drapings, {bytes_differ} \
                 differ in their bytes",
                ids_a.len()
            );
        } else {
            same += 1;
        }
    }
    println!(
        "identical {same} of {} frames",
        usize::try_from(last - first + 1).unwrap_or(0)
    );
    Ok(())
}

/// Bakes the scene again, live, and compares it against the pack.
///
/// This is the test the split pipeline stands on, and the only shape of it
/// that proves anything. A pack is a substitute for a live session; a
/// substitute that hands back *nearly* the same geometry is not a substitute,
/// it is a second implementation, and the difference appears as a render that
/// does not reproduce the one it was meant to.
///
/// It reads the pack through `Session::from_pack` — the render's own path,
/// including the little-endian decode, the texture URI and the vertex counts —
/// and it resolves the same cameras through a live session. Both sides then go
/// through `baked_tile`, so what is compared is what a renderer is handed.
///
/// What it will also catch, and this is worth knowing: a selection that is not
/// reproducible. If the live session picks different ground this time, the
/// comparison fails, and it should — a pack of a selection that cannot be
/// reproduced is a pack nobody can check.
fn verify(
    path: &std::path::Path,
    tape_path: &std::path::Path,
    viewport: (f64, f64),
) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let pack = tuile_pack::Pack::open(&bytes).map_err(|e| e.to_string())?;
    let (first, last) = pack.frame_range();

    let token = std::env::var("TUILE_ION_TOKEN")
        .map_err(|_| "TUILE_ION_TOKEN is not set; --verify bakes the scene again")?;
    let tape_bytes = std::fs::read(tape_path)
        .map_err(|e| format!("reading {}: {e}", tape_path.display()))?;
    let mut config = tuile_hydra::GlobeConfig::new(&token);
    if let Ok(dir) = std::env::var("TUILE_CACHE_DIR") {
        config.cache_dir = Some(dir.into());
    }
    let resolved = tuile_hydra::exact_traversal(config.session.traversal.clone());
    let scene = digest_of_scene(&tape_bytes, viewport, &format!("{resolved:?}"));
    if scene != pack.scene_digest() {
        return Err(format!(
            "this pack is a bake of scene {}, and the tape and settings given \
             here are scene {scene}",
            pack.scene_digest()
        ));
    }

    let mut live = tuile_hydra::Session::globe(config)
        .map_err(|e| format!("opening the globe: {e}"))?;
    let mut packed = tuile_hydra::Session::from_pack(path, Some(&scene))
        .map_err(|e| e.to_string())?;

    let mut checked = 0usize;
    for number in first..=last {
        let view = pack.view_of(number).map_err(|e| e.to_string())?;
        let views = vec![tuile_core::traversal::ViewStateParams {
            position: glam::DVec3::from_array(view.position),
            direction: glam::DVec3::from_array(view.direction),
            up: glam::DVec3::from_array(view.up),
            viewport_px: glam::dvec2(view.viewport_px[0], view.viewport_px[1]),
            fovy_rad: view.fovy_rad,
        }];
        let from_pack = packed
            .frame(views.clone())
            .map_err(|e| format!("frame {number} from the pack: {e}"))?;
        let from_network = live
            .frame(views)
            .map_err(|e| format!("frame {number} from the network: {e}"))?;

        if from_pack.tiles.len() != from_network.tiles.len() {
            return Err(format!(
                "frame {number}: the pack holds {} tiles, a live session \
                 selects {}",
                from_pack.tiles.len(),
                from_network.tiles.len()
            ));
        }
        for (index, (packed_tile, live_tile)) in from_pack
            .tiles
            .iter()
            .zip(from_network.tiles.iter())
            .enumerate()
        {
            let a = baked_tile(&from_pack, index, packed_tile)?;
            let b = baked_tile(&from_network, index, live_tile)?;
            if a != b {
                // Which field, not merely "different". A comparison that says
                // two tiles disagree without saying how is half an instrument,
                // and the half it is missing is the one you need at the moment
                // it fires.
                return Err(format!(
                    "frame {number}, tile {} (live: {}): {}",
                    a.id,
                    b.id,
                    first_difference(&a, &b)
                ));
            }
            checked += 1;
        }
        println!("VERIFY-FRAME {number} {} tiles identical", from_pack.tiles.len());
    }
    println!(
        "VERIFY-OK {checked} tiles over frames {first}..={last} are byte for \
byte what a live session produces"
    );
    Ok(())
}

/// Names the first field on which two tiles disagree, with sizes rather than
/// contents — a buffer printed in full explains nothing.
fn first_difference(a: &BakedTile, b: &BakedTile) -> String {
    let buffers = [
        ("positions", &a.positions, &b.positions),
        ("normals", &a.normals, &b.normals),
        ("uvs", &a.uvs, &b.uvs),
        ("indices", &a.indices, &b.indices),
    ];
    if a.drape != b.drape {
        return format!(
            "the imagery was composed differently: drape {:016x} baked, \
             {:016x} live",
            a.drape, b.drape
        );
    }
    if a.origin_ecef != b.origin_ecef {
        return format!("origin {:?} baked, {:?} live", a.origin_ecef, b.origin_ecef);
    }
    for (what, x, y) in buffers {
        if x != y {
            let at = x.iter().zip(y.iter()).position(|(p, q)| p != q);
            return format!(
                "{what}: {} bytes baked, {} live, first difference at {at:?}",
                x.len(),
                y.len()
            );
        }
    }
    match (&a.texture, &b.texture) {
        (Some(x), Some(y)) if x != y => {
            let at = x.iter().zip(y.iter()).position(|(p, q)| p != q);
            format!(
                "texture: {} bytes baked, {} live, first difference at {at:?}",
                x.len(),
                y.len()
            )
        }
        (Some(x), None) => format!("textured in the pack ({} bytes), bare live", x.len()),
        (None, Some(y)) => format!("bare in the pack, textured live ({} bytes)", y.len()),
        _ => format!(
            "counts or material: {}/{} vertices, {}/{} indices, factor {:?}/{:?}",
            a.vertex_count, b.vertex_count, a.index_count, b.index_count,
            a.base_color_factor, b.base_color_factor
        ),
    }
}

/// One tile, in the shape the pack stores and the ABI hands out.
///
/// The mapping is deliberately the same one `tuile_frame_tile` makes, field for
/// field: a pack is a substitute for a live session, and a substitute that
/// reshapes the data is a second implementation.
fn baked_tile(
    frame: &tuile_hydra::Frame,
    index: usize,
    tile: &Arc<tuile_hydra::TileGeometry>,
) -> Result<BakedTile, String> {
    // One prim per tile, as the consumer expects; terrain produces one mesh.
    let mesh = tile
        .content
        .meshes
        .first()
        .ok_or_else(|| format!("tile {} carries no mesh", tile.tile.0))?;

    // Index 0 only: a terrain tile carries one draped mosaic. A tile with
    // several would need the pack to hold several, and nothing produces one —
    // so this fails loudly rather than silently baking the first of many.
    let texture = frame
        .texture_png(index, 0)
        .map_err(|e| format!("tile {}: {e}", tile.tile.0))?;
    if frame
        .texture_png(index, 1)
        .map_err(|e| format!("tile {}: {e}", tile.tile.0))?
        .is_some()
    {
        return Err(format!(
            "tile {} carries more than one texture; the pack holds one",
            tile.tile.0
        ));
    }

    Ok(BakedTile {
        id: tile.tile.0,
        drape: tile.drape(),
        origin_ecef: tile.origin_ecef.to_array(),
        positions: f32x3_le(&mesh.positions),
        normals: mesh.normals.as_ref().map(|n| f32x3_le(n)).unwrap_or_default(),
        uvs: mesh.uvs.as_ref().map(|u| f32x2_le(u)).unwrap_or_default(),
        indices: u32_le(&mesh.indices),
        vertex_count: mesh.positions.len() as u32,
        index_count: mesh.indices.len() as u32,
        base_color_factor: mesh.material.base_color_factor,
        texture_format: if texture.is_some() {
            TextureFormat::Png
        } else {
            TextureFormat::None
        },
        texture: texture.map(|t| t.png.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoding is the file's, not this machine's.
    #[test]
    fn vertices_are_written_little_endian_whatever_the_host_is() {
        assert_eq!(f32x3_le(&[[1.0, -2.0, 0.5]]), {
            let mut want = Vec::new();
            want.extend_from_slice(&1.0f32.to_le_bytes());
            want.extend_from_slice(&(-2.0f32).to_le_bytes());
            want.extend_from_slice(&0.5f32.to_le_bytes());
            want
        });
        assert_eq!(u32_le(&[1, 0x0102_0304]), vec![1, 0, 0, 0, 4, 3, 2, 1]);
        assert_eq!(f32x2_le(&[[0.0, 1.0]]).len(), 8);
    }

    /// Two shards of one shot must agree on the name of the scene.
    ///
    /// The range belongs in the object key, never in the digest: a pack of
    /// frames 49–96 that called itself a different scene from the pack of 1–48
    /// would make a mismatched pack indistinguishable from a neighbouring one.
    #[test]
    fn the_scene_name_does_not_depend_on_which_frames_were_baked() {
        let tape = b"a tape";
        let a = digest_of_scene(tape, (1280.0, 960.0), "cfg");
        let b = digest_of_scene(tape, (1280.0, 960.0), "cfg");
        assert_eq!(a, b);
        // …but everything that changes what a frame contains does.
        assert_ne!(a, digest_of_scene(b"another tape", (1280.0, 960.0), "cfg"));
        assert_ne!(a, digest_of_scene(tape, (1920.0, 960.0), "cfg"));
        assert_ne!(a, digest_of_scene(tape, (1280.0, 960.0), "other cfg"));
    }
}
