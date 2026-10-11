// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What is left between the tiles of each frame, logged.
//!
//! Two tiles that share a stretch of edge should state one line. Where they
//! do not — a step between two levels of a source's data, a vertex off its
//! neighbour's segment, an edge with nothing drawn across it — there is a
//! seam, and whether it shows depends on what happens to be hung across it.
//! This says it whatever shows: an [`Observer`] that hands every frame's
//! tiles to `tuile_core::seam::residuals` and writes what comes back.
//!
//! **It measures what the renderer is handed.** Each mesh is kept as the
//! GPU gets it — its origin in `f64` and its positions already narrowed to
//! `f32` about it — so a mismatch made late, by the narrowing or by two
//! tiles narrowed about two origins, is in the number.
//!
//! **And as the renderer displaces them.** A render that stitches moves
//! edges in its shaders; this plans the same frame through the same
//! `tuile_core::stitch::plan` and measures the meshes `stitch::displaced`
//! gives — the shaders' arithmetic on the CPU, held to the GPU's by a test
//! of `tuile-film-gpu`.
//!
//! In `<dir>`:
//!
//! - `seams.csv`: a row a frame — shared edges, how many are over the
//!   tolerance and how many of those are in the picture, the largest, median
//!   and 99th-centile residual in metres and in pixels, same-level pairs and
//!   their largest, edges with nothing drawn across them and how many of
//!   those are in the picture (the rest is where the selection ends, out of
//!   view: a film draws what its camera sees);
//! - `seam-pairs.csv`: the stretches over the tolerance, the worst first —
//!   the two tiles, the terrain level each surface is from, the residual
//!   and its parts, and where it is on the picture.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use glam::{DVec3, Mat4, Vec4};
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::seam::{residuals, Drawn, Eye, Residuals, Tolerance};
use tuile_core::source::TileId;
use tuile_core::stitch;
use tuile_film::TileKey;

use crate::observe::{FrameOut, Observer, TileIn};
use crate::Error;

struct Held {
    at: (u32, u64, u64),
    /// The level of the terrain tile the surface is from.
    source: u32,
    content: DecodedTileContent,
    edges: stitch::Edges,
}

/// Logs seams. See the module.
pub struct SeamLog {
    dir: PathBuf,
    tolerance: Tolerance,
    /// Whether the render stitches: the meshes are then measured displaced.
    stitched: bool,
    held: HashMap<TileKey, Held>,
    frames: Option<std::fs::File>,
    pairs: Option<std::fs::File>,
    /// Over the frames seen: frames, shared edges, edges over the
    /// tolerance, frames with any, the worst residual in pixels and metres
    /// and its frame, uncovered edges, and the time spent measuring.
    seen: u32,
    shared: u64,
    over: u64,
    /// …and of those, and of the uncovered edges, the ones in the picture.
    over_in_picture: u64,
    uncovered_in_picture: u64,
    frames_over: u32,
    worst: (f64, f64, u32),
    uncovered: u64,
    spent: std::time::Duration,
    failed: Option<String>,
}

/// Rows of `seam-pairs.csv` a frame may add.
const WORST_KEPT: usize = 64;

impl SeamLog {
    /// Into `dir`, against `tolerance`.
    pub fn into(dir: impl Into<PathBuf>, tolerance: Tolerance) -> Self {
        Self {
            dir: dir.into(),
            tolerance,
            stitched: false,
            held: HashMap::new(),
            frames: None,
            pairs: None,
            seen: 0,
            shared: 0,
            over: 0,
            over_in_picture: 0,
            uncovered_in_picture: 0,
            frames_over: 0,
            worst: (0.0, 0.0, 0),
            uncovered: 0,
            spent: std::time::Duration::ZERO,
            failed: None,
        }
    }

    /// Measures the tiles as a render that stitches draws them.
    pub fn stitched(mut self, stitched: bool) -> Self {
        self.stitched = stitched;
        self
    }

    /// Shared edges over the tolerance, over every frame seen.
    pub fn over(&self) -> u64 {
        self.over
    }

    /// Those of them that are in the picture, and the edges in the picture
    /// with nothing drawn across them.
    pub fn in_picture(&self) -> (u64, u64) {
        (self.over_in_picture, self.uncovered_in_picture)
    }

    /// What was found, for a person: a few lines.
    pub fn summary(&self) -> String {
        let frames = self.seen.max(1);
        format!(
            "seams: {} frames, {} shared edges, {} over the tolerance ({:?}) — {} of them in \
             the picture — in {} frames; worst {:.2} px ({:.2} m) at frame {}; {} edges with \
             nothing across them, {} in the picture; {:.1} ms a frame measuring",
            self.seen,
            self.shared,
            self.over,
            self.tolerance,
            self.over_in_picture,
            self.frames_over,
            self.worst.0,
            self.worst.1,
            self.worst.2,
            self.uncovered,
            self.uncovered_in_picture,
            self.spent.as_secs_f64() * 1000.0 / f64::from(frames),
        )
    }

    /// The first thing that kept the log from being written, if anything.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn write(
        &mut self,
        frame: &FrameOut<'_>,
        found: &Residuals,
        sources: &HashMap<(u32, u64, u64), u32>,
    ) -> Result<(), Error> {
        if self.frames.is_none() {
            std::fs::create_dir_all(&self.dir)?;
            let mut frames = std::fs::File::create(self.dir.join("seams.csv"))?;
            writeln!(
                frames,
                "frame,tiles,shared,over,over_in_picture,max_m,p50_m,p99_m,max_px,p50_px,p99_px,\
                 same_level,same_level_max_m,uncovered,uncovered_in_picture,uncovered_m"
            )?;
            let mut pairs = std::fs::File::create(self.dir.join("seam-pairs.csv"))?;
            writeln!(
                pairs,
                "frame,across,a_level,a_x,a_y,a_source,b_level,b_x,b_y,b_source,length_m,\
                 residual_m,height_m,horizontal_m,residual_px,picture_x,picture_y,in_picture"
            )?;
            (self.frames, self.pairs) = (Some(frames), Some(pairs));
        }
        let matrix = frame.camera.view_projection();
        let tolerance = self.tolerance;
        let over = |p: &&tuile_core::seam::Residual| match tolerance {
            Tolerance::Metres(most) => p.metres > most,
            Tolerance::Pixels(most) => p.pixels > most,
        };
        // In the picture: before the eye, inside the frame, and on the near
        // side of the Earth.
        let eye = frame.camera.eye;
        let (wide, high) = (frame.width as f32, frame.height as f32);
        let shown = |at: DVec3| {
            let clip = frame.camera.view_projection() * Vec4::from(((at - eye).as_vec3(), 1.0));
            let (x, y) = (
                (clip.x / clip.w + 1.0) * 0.5 * wide,
                (1.0 - clip.y / clip.w) * 0.5 * high,
            );
            clip.w > 0.0
                && (0.0..wide).contains(&x)
                && (0.0..high).contains(&y)
                && (eye - at).dot(at.normalize()) > 0.0
        };
        let over_shown = found
            .pairs
            .iter()
            .filter(over)
            .filter(|p| shown(p.at))
            .count();
        let bare_shown = found.bare.iter().filter(|b| shown(b.at)).count();
        self.over_in_picture += over_shown as u64;
        self.uncovered_in_picture += bare_shown as u64;
        let (Some(frames), Some(pairs)) = (self.frames.as_mut(), self.pairs.as_mut()) else {
            return Ok(());
        };
        writeln!(
            frames,
            "{},{},{},{},{},{:.4},{:.4},{:.4},{:.3},{:.3},{:.3},{},{:.4},{},{},{:.1}",
            frame.frame,
            frame.selection.len(),
            found.shared,
            found.over,
            over_shown,
            found.metres[0],
            found.metres[1],
            found.metres[2],
            found.pixels[0],
            found.pixels[1],
            found.pixels[2],
            found.same_level,
            found.same_level_max,
            found.uncovered,
            bare_shown,
            found.uncovered_metres,
        )?;
        for pair in found.pairs.iter().filter(over).take(WORST_KEPT) {
            let clip = matrix * Vec4::from(((pair.at - frame.camera.eye).as_vec3(), 1.0));
            let (x, y) = if clip.w > 0.0 {
                (
                    (clip.x / clip.w + 1.0) * 0.5 * frame.width as f32,
                    (1.0 - clip.y / clip.w) * 0.5 * frame.height as f32,
                )
            } else {
                (f32::NAN, f32::NAN)
            };
            let source = |at: &(u32, u64, u64)| sources.get(at).copied().unwrap_or(at.0);
            writeln!(
                pairs,
                "{},{},{},{},{},{},{},{},{},{},{:.2},{:.4},{:.4},{:.4},{:.3},{:.1},{:.1},{}",
                frame.frame,
                if pair.meridian {
                    "meridian"
                } else {
                    "parallel"
                },
                pair.a.0,
                pair.a.1,
                pair.a.2,
                source(&pair.a),
                pair.b.0,
                pair.b.1,
                pair.b.2,
                source(&pair.b),
                pair.length,
                pair.metres,
                pair.height,
                pair.horizontal,
                pair.pixels,
                x,
                y,
                u8::from(shown(pair.at)),
            )?;
        }
        Ok(())
    }
}

/// A film's mesh as content: the same numbers, in the shape the engine's
/// measure reads.
fn content_of(tile: &TileIn<'_>) -> DecodedTileContent {
    let f = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    DecodedTileContent {
        withheld_drape: None,
        meshes: vec![DecodedMesh {
            positions: tile
                .mesh
                .positions
                .chunks_exact(12)
                .map(|b| [f(&b[0..4]), f(&b[4..8]), f(&b[8..12])])
                .collect(),
            normals: None,
            uvs: (!tile.mesh.uvs.is_empty()).then(|| {
                tile.mesh
                    .uvs
                    .chunks_exact(8)
                    .map(|b| [f(&b[0..4]), f(&b[4..8])])
                    .collect()
            }),
            indices: tile
                .mesh
                .indices
                .chunks_exact(4)
                .take(tile.mesh.index_count as usize)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
            material: MaterialDesc::default(),
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::from_array(tile.mesh.origin_ecef),
        transform_local: Mat4::IDENTITY,
    }
}

impl Observer for SeamLog {
    fn tile(&mut self, tile: &TileIn<'_>) {
        let at = TileId(tile.key.id).terrain_coord();
        let content = content_of(tile);
        self.held.insert(
            tile.key,
            Held {
                at,
                source: tile.terrain.map_or(at.0, |t| u32::from(t.0)),
                edges: stitch::Edges::of(&content),
                content,
            },
        );
    }

    fn frame(&mut self, frame: &FrameOut<'_>) {
        let began = std::time::Instant::now();
        // What the frame no longer draws is let go, as the renderer lets go.
        let drawn: std::collections::HashSet<TileKey> = frame.selection.iter().copied().collect();
        self.held.retain(|key, _| drawn.contains(key));
        let held: Vec<&Held> = frame
            .selection
            .iter()
            .filter_map(|key| self.held.get(key))
            .collect();
        let displaced: Vec<DecodedTileContent> = if self.stitched {
            let tiles: Vec<stitch::Tile<'_>> = held
                .iter()
                .map(|held| stitch::Tile {
                    level: held.at.0,
                    x: held.at.1,
                    y: held.at.2,
                    source: held.source,
                    edges: &held.edges,
                })
                .collect();
            held.iter()
                .zip(stitch::plan(&tiles, (2, 1), stitch::BAND))
                .map(|(held, plan)| stitch::displaced(&held.content, &plan, stitch::BAND, 1.0))
                .collect()
        } else {
            Vec::new()
        };
        let tiles: Vec<Drawn<'_>> = held
            .iter()
            .enumerate()
            .map(|(n, held)| Drawn {
                level: held.at.0,
                x: held.at.1,
                y: held.at.2,
                content: displaced.get(n).unwrap_or(&held.content),
            })
            .collect();
        let eye = Eye {
            position: frame.camera.eye,
            // The projection's y scale is 1 / tan(fovy / 2).
            pixels_per_radian: f64::from(frame.height) / 2.0
                * f64::from(frame.camera.projection.y_axis.y),
        };
        let found = residuals(&tiles, (2, 1), Some(eye), self.tolerance);
        let sources: HashMap<(u32, u64, u64), u32> =
            self.held.values().map(|h| (h.at, h.source)).collect();
        self.seen += 1;
        self.shared += found.shared as u64;
        self.over += found.over as u64;
        self.frames_over += u32::from(found.over > 0);
        self.uncovered += found.uncovered as u64;
        if found.pixels[0] > self.worst.0 {
            self.worst = (
                found.pixels[0],
                found.pairs.first().map_or(0.0, |p| p.metres),
                frame.frame,
            );
        }
        if let Err(e) = self.write(frame, &found, &sources) {
            self.failed.get_or_insert_with(|| e.to_string());
        }
        self.spent += began.elapsed();
    }
}
