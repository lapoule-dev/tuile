// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! PROBE (exploration, not a feature): the seams of a frame, measured on
//! the meshes the frame draws.
//!
//! Two tiles that share a stretch of edge each state the ground along it,
//! from their own mesh. Where the two disagree in height there is a step,
//! and the step is closed only if the higher of the two hangs a skirt at
//! least as deep. This walks every such stretch of every frame — whatever
//! the two tiles' levels — and writes, a pair a row: how far the surfaces
//! part, how much of it no skirt closes, and what that opening is on the
//! picture, in pixels, from the frame's own camera.
//!
//! What it does not know: whether nearer ground hides an opening. Its
//! pixels are an upper bound; the picture's own count (`TUILE_PROBE_HOLES`)
//! is the truth they are set against.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use glam::{DVec3, Vec4};
use tuile_core::geo::ecef_to_geodetic;
use tuile_core::source::TileId;
use tuile_film::TileKey;
use tuile_terrain::{GeographicTilingScheme, TileCoord};

use crate::observe::{FrameOut, Observer, TileIn};

/// Tile coordinates are compared on the grid of this level.
const DEEP: u32 = 30;
/// West, south, east, north: the order a quantized mesh lists its edges in.
const SIDES: [&str; 4] = ["west", "south", "east", "north"];

#[derive(Clone, Copy)]
struct Post {
    /// Along the edge, 0 to 1 in the tile.
    t: f64,
    /// The surface there.
    top: DVec3,
    /// How far a skirt hangs below it; 0 for none.
    skirt: f64,
}

struct Held {
    coord: TileCoord,
    /// The level of the terrain tile the surface is from.
    source: u32,
    sides: [Vec<Post>; 4],
}

impl Held {
    /// The line a side lies on, and the stretch of it the side covers, on
    /// the deep grid.
    fn span(&self, side: usize) -> (u64, (u64, u64)) {
        let shift = DEEP - self.coord.level;
        let (x, y) = (self.coord.x << shift, self.coord.y << shift);
        let one = 1u64 << shift;
        let round = 2u64 << DEEP;
        match side {
            0 => (x, (y, y + one)),
            2 => ((x + one) % round, (y, y + one)),
            1 => (y, (x, x + one)),
            _ => (y + one, (x, x + one)),
        }
    }

    /// The surface and the skirt's depth at `t` along a side.
    fn at(&self, side: usize, t: f64) -> Option<(DVec3, f64)> {
        let posts = &self.sides[side];
        let (first, last) = (posts.first()?, posts.last()?);
        if t <= first.t {
            return Some((first.top, first.skirt));
        }
        if t >= last.t {
            return Some((last.top, last.skirt));
        }
        let next = posts.partition_point(|p| p.t <= t);
        let (a, b) = (posts[next - 1], posts[next]);
        let k = (t - a.t) / (b.t - a.t).max(f64::MIN_POSITIVE);
        Some((a.top.lerp(b.top, k), a.skirt + (b.skirt - a.skirt) * k))
    }
}

/// Watches seams. See the module.
pub struct SeamMeter {
    dir: PathBuf,
    held: HashMap<TileKey, Held>,
    rows: Option<std::fs::File>,
}

impl SeamMeter {
    pub fn into(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            held: HashMap::new(),
            rows: None,
        }
    }
}

fn posts_of(tile: &TileIn<'_>, coord: TileCoord) -> [Vec<Post>; 4] {
    let origin = DVec3::from_array(tile.mesh.origin_ecef);
    let f = |b: &[u8]| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let places: Vec<DVec3> = tile
        .mesh
        .positions
        .chunks_exact(12)
        .map(|b| origin + DVec3::new(f(&b[0..4]), f(&b[4..8]), f(&b[8..12])))
        .collect();
    let uvs: Vec<(f64, f64)> = tile
        .mesh
        .uvs
        .chunks_exact(8)
        // Texture space counts v down; the tile counts it northward.
        .map(|b| (f(&b[0..4]), 1.0 - f(&b[4..8])))
        .collect();
    let rect = GeographicTilingScheme::default().tile_rect(coord);
    // A skirt is told from the surface by hanging well below it.
    let hung = 0.2 * tuile_terrain::skirt_height(&rect);
    const ON: f64 = 1.0e-6;
    std::array::from_fn(|side| {
        let mut on: Vec<(f64, f64, DVec3)> = places
            .iter()
            .zip(&uvs)
            .filter_map(|(p, &(u, v))| {
                let (across, along) = match side {
                    0 => (u, v),
                    2 => (1.0 - u, v),
                    1 => (v, u),
                    _ => (1.0 - v, u),
                };
                (across.abs() < ON).then(|| (along, ecef_to_geodetic(*p).height, *p))
            })
            .collect();
        on.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut posts: Vec<Post> = Vec::new();
        let mut i = 0;
        while i < on.len() {
            let mut j = i;
            let (mut high, mut low) = (on[i], on[i]);
            while j < on.len() && on[j].0 - on[i].0 < ON {
                if on[j].1 > high.1 {
                    high = on[j];
                }
                if on[j].1 < low.1 {
                    low = on[j];
                }
                j += 1;
            }
            let depth = high.1 - low.1;
            posts.push(Post {
                t: on[i].0,
                top: high.2,
                skirt: if depth > hung { depth } else { 0.0 },
            });
            i = j;
        }
        posts
    })
}

impl Observer for SeamMeter {
    fn tile(&mut self, tile: &TileIn<'_>) {
        let (level, x, y) = TileId(tile.key.id).terrain_coord();
        if level > DEEP {
            return;
        }
        let coord = TileCoord::new(level, x, y);
        self.held.insert(
            tile.key,
            Held {
                coord,
                source: tile.terrain.map_or(level, |t| u32::from(t.0)),
                sides: posts_of(tile, coord),
            },
        );
    }

    fn frame(&mut self, frame: &FrameOut<'_>) {
        if self.rows.is_none() {
            let _ = std::fs::create_dir_all(&self.dir);
            let mut file = std::fs::File::create(self.dir.join("seams.csv")).expect("seams.csv");
            writeln!(
                file,
                "frame,axis,a_level,a_x,a_y,a_source,a_skirted,b_level,b_x,b_y,b_source,b_skirted,\
                 length_m,distance_m,step_max_m,step_mean_m,lateral_max_m,open_max_m,open_m2,\
                 open_px,open_px_facing,on_screen"
            )
            .expect("seams.csv");
            self.rows = Some(file);
        }
        let drawn: Vec<&Held> = frame
            .selection
            .iter()
            .filter_map(|key| self.held.get(key))
            .collect();
        // Sides by the line they lie on: an east side meets the west sides
        // on its line, a north side the south sides on its own.
        let mut lines: HashMap<(usize, u64), Vec<(usize, usize)>> = HashMap::new();
        for (n, held) in drawn.iter().enumerate() {
            for side in 0..4 {
                lines
                    .entry((side % 2, held.span(side).0))
                    .or_default()
                    .push((n, side));
            }
        }
        let camera = frame.camera;
        let matrix = camera.view_projection();
        let (wide, high) = (f64::from(frame.width), f64::from(frame.height));
        let pixel = |p: DVec3| -> Option<(f64, f64)> {
            let clip = matrix * Vec4::from(((p - camera.eye).as_vec3(), 1.0));
            (clip.w > 0.0).then(|| {
                (
                    (f64::from(clip.x / clip.w) + 1.0) * 0.5 * wide,
                    (1.0 - f64::from(clip.y / clip.w)) * 0.5 * high,
                )
            })
        };
        let mut marked = frame.rgba.to_vec();
        let mut mark = |x: f64, y: f64| {
            // A ring, so that what it is drawn round stays in view.
            for step in 0..96 {
                let turn = f64::from(step) / 96.0 * std::f64::consts::TAU;
                let (px, py) = (x + 9.0 * turn.cos(), y + 9.0 * turn.sin());
                if px >= 0.0 && py >= 0.0 && px < wide && py < high {
                    let at = (py as usize * frame.width as usize + px as usize) * 4;
                    marked[at..at + 4].copy_from_slice(&[255, 0, 255, 255]);
                }
            }
        };
        let rows = self.rows.as_mut().expect("opened above");
        for sides in lines.values() {
            for &(a, side_a) in sides.iter().filter(|s| s.1 >= 2) {
                for &(b, side_b) in sides.iter().filter(|s| s.1 + 2 == side_a) {
                    let (ta, tb) = (drawn[a], drawn[b]);
                    let ((_, (a0, a1)), (_, (b0, b1))) = (ta.span(side_a), tb.span(side_b));
                    let (g0, g1) = (a0.max(b0), a1.min(b1));
                    if g0 >= g1 {
                        continue;
                    }
                    let to = |g: f64, (s0, s1): (u64, u64)| (g - s0 as f64) / (s1 - s0) as f64;
                    let back = |t: f64, (s0, s1): (u64, u64)| s0 as f64 + t * (s1 - s0) as f64;
                    // Where either mesh has a vertex on the stretch, and
                    // between each two of those.
                    let mut stops: Vec<f64> = vec![g0 as f64, g1 as f64];
                    stops.extend(ta.sides[side_a].iter().map(|p| back(p.t, (a0, a1))));
                    stops.extend(tb.sides[side_b].iter().map(|p| back(p.t, (b0, b1))));
                    stops.retain(|g| *g >= g0 as f64 && *g <= g1 as f64);
                    stops.sort_by(f64::total_cmp);
                    stops.dedup();
                    let mids: Vec<f64> = stops.windows(2).map(|w| (w[0] + w[1]) / 2.0).collect();
                    stops.extend(mids);
                    stops.sort_by(f64::total_cmp);

                    struct At {
                        low: DVec3,
                        under: DVec3,
                        open: f64,
                        facing: bool,
                    }
                    let mut walk: Vec<At> = Vec::with_capacity(stops.len());
                    let (mut step_max, mut step_sum, mut lateral_max, mut open_max) =
                        (0.0f64, 0.0f64, 0.0f64, 0.0f64);
                    for g in &stops {
                        let (Some((pa, sa)), Some((pb, sb))) = (
                            ta.at(side_a, to(*g, (a0, a1))),
                            tb.at(side_b, to(*g, (b0, b1))),
                        ) else {
                            continue;
                        };
                        let up = pa.normalize();
                        let east = DVec3::new(-pa.y, pa.x, 0.0).normalize_or_zero();
                        // From a to b: east across a meridian, north across
                        // a parallel.
                        let across = if side_a == 2 { east } else { up.cross(east) };
                        let apart = pa - pb;
                        let step = apart.dot(up);
                        lateral_max = lateral_max.max((apart - step * up).length());
                        let (higher, lower, skirt) = if step > 0.0 {
                            (pa, pb, sa)
                        } else {
                            (pb, pa, sb)
                        };
                        let open = (step.abs() - skirt).max(0.0);
                        step_max = step_max.max(step.abs());
                        step_sum += step.abs();
                        open_max = open_max.max(open);
                        // An opening is looked into from the lower tile's
                        // side, by an eye above the ground there.
                        let eye = camera.eye - pa;
                        let from_b = eye.dot(across) > 0.0;
                        let facing = eye.dot(up) > 0.0 && (from_b == (step > 0.0));
                        walk.push(At {
                            low: lower,
                            under: if open > 0.0 {
                                higher - skirt * up
                            } else {
                                lower
                            },
                            open,
                            facing,
                        });
                    }
                    if walk.len() < 2 {
                        continue;
                    }
                    let length = (walk[0].low - walk[walk.len() - 1].low).length();
                    let distance =
                        ((walk[0].low + walk[walk.len() - 1].low) / 2.0 - camera.eye).length();
                    let (mut open_m2, mut open_px, mut open_px_facing) = (0.0f64, 0.0f64, 0.0f64);
                    let mut on_screen = false;
                    for pair in walk.windows(2) {
                        let (p, q) = (&pair[0], &pair[1]);
                        if p.open <= 0.0 && q.open <= 0.0 {
                            continue;
                        }
                        open_m2 += (p.open + q.open) / 2.0 * (p.low - q.low).length();
                        let corners = [p.low, q.low, q.under, p.under].map(pixel);
                        let [Some(c0), Some(c1), Some(c2), Some(c3)] = corners else {
                            continue;
                        };
                        let quad = [c0, c1, c2, c3];
                        let (cx, cy) = (
                            quad.iter().map(|c| c.0).sum::<f64>() / 4.0,
                            quad.iter().map(|c| c.1).sum::<f64>() / 4.0,
                        );
                        if cx < 0.0 || cy < 0.0 || cx >= wide || cy >= high {
                            continue;
                        }
                        on_screen = true;
                        let area = (0..4)
                            .map(|i| {
                                let (m, n) = (quad[i], quad[(i + 1) % 4]);
                                m.0 * n.1 - n.0 * m.1
                            })
                            .sum::<f64>()
                            .abs()
                            / 2.0;
                        open_px += area;
                        if p.facing || q.facing {
                            open_px_facing += area;
                            if area > 0.05 {
                                mark(cx, cy);
                            }
                        }
                    }
                    let count = walk.len() as f64;
                    let skirted =
                        |t: &Held, side: usize| t.sides[side].iter().any(|p| p.skirt > 0.0);
                    writeln!(
                        rows,
                        "{},{},{},{},{},{},{},{},{},{},{},{},{:.2},{:.0},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2},{:.2},{}",
                        frame.frame,
                        if side_a == 2 { "meridian" } else { "parallel" },
                        ta.coord.level, ta.coord.x, ta.coord.y, ta.source, u8::from(skirted(ta, side_a)),
                        tb.coord.level, tb.coord.x, tb.coord.y, tb.source, u8::from(skirted(tb, side_b)),
                        length, distance, step_max, step_sum / count, lateral_max, open_max,
                        open_m2, open_px, open_px_facing, u8::from(on_screen),
                    )
                    .expect("seams.csv");
                    let _ = SIDES;
                }
            }
        }
        // What the picture itself says, when the resolve was asked to paint
        // what a gap shows (TUILE_PROBE_HOLES): magenta, whole or in part.
        let (mut touched, mut whole) = (0u64, 0.0f64);
        for p in frame.rgba.chunks_exact(4) {
            let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
            if r - g > 24 && b - g > 24 && (r - b).abs() < 40 {
                touched += 1;
                whole += f64::from(r.min(b) - g) / 255.0;
            }
        }
        println!(
            "seams: frame {} — {} tiles; picture: {touched} pixels touched by magenta, {whole:.1} whole",
            frame.frame,
            drawn.len()
        );
        let path = self
            .dir
            .join(format!("seams-{:06}-marked.png", frame.frame));
        if let Err(e) = image::save_buffer(
            &path,
            &marked,
            frame.width,
            frame.height,
            image::ExtendedColorType::Rgba8,
        ) {
            eprintln!("seams: {}: {e}", path.display());
        }
    }
}
