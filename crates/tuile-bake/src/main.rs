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
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--inspect" => return Ok(Job::Inspect(value()?.into())),
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
        println!("  frame {frame:>5}: {:>5} tiles, {textured} textured", tiles.len());
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
        writer.frame(number, tiles);
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
