// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A light meter on a render.
//!
//! Going in: the tone of every imagery tile as the store holds it, and —
//! tile against ancestor, over the same ground — the step a joint between
//! the two would show, which is what a grade per level is fitted on and
//! judged by, over the whole film and region by region. Coming out: the
//! luminance of every picture.
//!
//! It is an [`Observer`]: the render does not know it is there.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use tuile_radiometry::{
    region_key, tone_of, Bounds, FilmGrade, Grade, LevelGrades, LevelParams, LookTarget, Sample,
    Seen, REGION_LEVEL,
};

use crate::observe::{FrameOut, ImageryIn, Observer, TileIn};
use crate::Error;

type Coord = (u8, u32, u32);

/// The side of what is kept of each imagery tile.
const KEPT: u32 = 64;
/// A tile and an ancestor are compared over this many cells a side: coarse,
/// because a joint is seen as a step in tone over tens of texels.
const CELLS: u32 = 8;
/// How far up a tile looks for an ancestor: past this the ground a tile
/// covers is less than a cell of what was kept of the ancestor.
const REACH: u8 = 3;
/// The level whose tiles are the places a change of source is broken down
/// by, and grades kept by.
const REGION: u8 = REGION_LEVEL;
/// Under this, a linear value is the dark, where a ratio means little.
const FLOOR: f32 = 0.004;

fn linear(stored: f32) -> f32 {
    let v = stored / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Relative luminance of a linear colour, in stops under white.
fn stops(rgb: [f32; 3]) -> f32 {
    (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2])
        .max(1e-6)
        .log2()
}

fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

struct TileLight {
    /// Its tone as stored, linear.
    tone: [f32; 3],
    /// The tile reduced to `KEPT` a side, as linear RGB.
    kept: Vec<[f32; 3]>,
    grade: Grade,
    renewed: bool,
    /// Tiles of the film whose drape it is in.
    drapes: u32,
}

/// The mean of a square of what was kept of a tile.
fn cell(kept: &[[f32; 3]], x: u32, y: u32, side: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    for row in y..y + side {
        for col in x..x + side {
            let c = kept[(row * KEPT + col) as usize];
            for i in 0..3 {
                sum[i] += c[i];
            }
        }
    }
    sum.map(|v| v / (side * side) as f32)
}

/// The step between two colours, in stops, worst channel.
fn step(a: [f32; 3], b: [f32; 3]) -> f32 {
    (0..3)
        .map(|i| ((a[i] + FLOOR) / (b[i] + FLOOR)).log2().abs())
        .fold(0.0, f32::max)
}

/// What a picture measures, in stops.
#[derive(Debug, Clone, Copy)]
pub struct Light {
    pub frame: u32,
    pub mean: f32,
    pub p5: f32,
    pub p50: f32,
    pub p95: f32,
    /// Top, middle and bottom thirds of the picture.
    pub bands: [f32; 3],
    /// Red and blue against green: the cast.
    pub red: f32,
    pub blue: f32,
}

/// Stored values as linear light, once.
fn linear_table() -> &'static [f32; 256] {
    static TABLE: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::array::from_fn(|v| linear(v as f32)))
}

/// Bins of luminance a picture's percentiles are read from: a 4096th of
/// white each, which is finer than a stored value anywhere but the dark.
const BINS: usize = 4096;

fn light_of(frame: &FrameOut<'_>) -> Light {
    let (width, height) = (frame.width as usize, frame.height as usize);
    let table = linear_table();
    let mut sum = [0.0f64; 3];
    let mut bands = [0.0f64; 3];
    let mut bins = vec![0u32; BINS + 1];
    let third = (height / 3).max(1);
    for (row, texels) in frame.rgba.chunks_exact(width * 4).enumerate() {
        let (mut r, mut g, mut b) = (0.0f32, 0.0f32, 0.0f32);
        for p in texels.chunks_exact(4) {
            let rgb = [
                table[p[0] as usize],
                table[p[1] as usize],
                table[p[2] as usize],
            ];
            r += rgb[0];
            g += rgb[1];
            b += rgb[2];
            let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
            bins[(y * BINS as f32) as usize] += 1;
        }
        sum[0] += f64::from(r);
        sum[1] += f64::from(g);
        sum[2] += f64::from(b);
        bands[(row / third).min(2)] += f64::from(0.2126 * r + 0.7152 * g + 0.0722 * b);
    }
    let n = (width * height).max(1) as f64;
    let at = |q: f64| {
        let (mut seen, wanted) = (0u64, (n * q) as u64);
        for (bin, count) in bins.iter().enumerate() {
            seen += u64::from(*count);
            if seen > wanted {
                return ((bin as f32 + 0.5) / BINS as f32).log2();
            }
        }
        0.0
    };
    let rows_in = |band: usize| match band {
        2 => height - 2 * third.min(height / 2),
        _ => third,
    };
    let mean = sum.map(|s| (s / n) as f32);
    Light {
        frame: frame.frame,
        mean: stops(mean),
        p5: at(0.05),
        p50: at(0.5),
        p95: at(0.95),
        bands: std::array::from_fn(|band| {
            ((bands[band] / (rows_in(band) * width).max(1) as f64) as f32)
                .max(1e-6)
                .log2()
        }),
        red: (mean[0] / mean[1].max(1e-6)).max(1e-6).log2(),
        blue: (mean[2] / mean[1].max(1e-6)).max(1e-6).log2(),
    }
}

/// Measures the light of a render: see the module.
#[derive(Default)]
pub struct LightMeter {
    tiles: HashMap<Coord, TileLight>,
    frames: Vec<(Light, usize, usize, Vec<(u8, u32)>, f64)>,
    undecodable: u32,
}

impl Observer for LightMeter {
    fn imagery(&mut self, tile: &ImageryIn<'_>) {
        let Ok(decoded) = image::load_from_memory(tile.bytes) else {
            self.undecodable += 1;
            return;
        };
        let rgba = decoded.to_rgba8();
        let (tone, counted) = tone_of(&rgba, rgba.width(), rgba.height(), (0.0, 0.0, 1.0, 1.0));
        if counted < 0.5 || tone.iter().any(|c| *c <= 0.0) {
            return;
        }
        let kept =
            image::imageops::resize(&rgba, KEPT, KEPT, image::imageops::FilterType::Triangle)
                .pixels()
                .map(|p| {
                    [
                        linear(p[0].into()),
                        linear(p[1].into()),
                        linear(p[2].into()),
                    ]
                })
                .collect();
        self.tiles.insert(
            (tile.level, tile.x, tile.y),
            TileLight {
                tone,
                kept,
                grade: tile.grade,
                renewed: tile.renewed,
                drapes: 0,
            },
        );
    }

    fn tile(&mut self, tile: &TileIn<'_>) {
        for at in tile.imagery {
            if let Some(light) = self.tiles.get_mut(at) {
                light.drapes += 1;
            }
        }
    }

    fn frame(&mut self, frame: &FrameOut<'_>) {
        self.frames.push((
            light_of(frame),
            frame.tiles,
            frame.entered,
            frame.layers.to_vec(),
            frame.timings.total(),
        ));
    }
}

impl LightMeter {
    /// The pictures' light, in the order they came out.
    pub fn pictures(&self) -> Vec<Light> {
        self.frames.iter().map(|f| f.0).collect()
    }

    /// Every tile against its two nearest ancestors among the tiles seen,
    /// cell for cell over the same ground, and the region each is in.
    fn seen(&self) -> Vec<(Seen, (u32, u32))> {
        let mut out = Vec::new();
        for (coord, light) in &self.tiles {
            let mut found = 0;
            for up in 1..=REACH.min(coord.0) {
                let Some(ancestor) = self
                    .tiles
                    .get(&(coord.0 - up, coord.1 >> up, coord.2 >> up))
                else {
                    continue;
                };
                // Where the tile lies in what was kept of the ancestor.
                let across = KEPT >> up;
                let (x0, y0) = (
                    (coord.1 & ((1 << up) - 1)) * across,
                    (coord.2 & ((1 << up) - 1)) * across,
                );
                let (mut tile, mut under) = (Vec::new(), Vec::new());
                for j in 0..CELLS {
                    for i in 0..CELLS {
                        let side = KEPT / CELLS;
                        tile.push(cell(&light.kept, i * side, j * side, side));
                        let side = across / CELLS;
                        under.push(cell(&ancestor.kept, x0 + i * side, y0 + j * side, side));
                    }
                }
                let region = if coord.0 >= REGION {
                    let down = coord.0 - REGION;
                    (coord.1 >> down, coord.2 >> down)
                } else {
                    (u32::MAX, u32::MAX)
                };
                out.push((
                    Seen {
                        level: coord.0,
                        ancestor: coord.0 - up,
                        tile,
                        under,
                    },
                    region,
                ));
                found += 1;
                if found == 2 {
                    break;
                }
            }
        }
        out
    }

    /// Where two tiles of one level meet, and where a tile lies on a far
    /// coarser one: what a grade a level cannot mend, measured.
    ///
    /// A grade a level gives every tile of a level the same curve, so two
    /// neighbours of a level meet after it as they met before. If they did
    /// not meet before — two captures within one level — they still do
    /// not. The step across the edge two neighbours share is compared with
    /// the step across a line drawn inside a tile, which is what ground
    /// alone does over the same distance.
    pub fn joints(&self) -> String {
        const STRIP: u32 = 4;
        const SEAM: f32 = 0.15;
        let side = KEPT / CELLS;
        // The step, in stops of luminance and of blue against green,
        // between two strips of a tile's kept texels, cell by cell along
        // the edge: the median cell.
        let across = |a: &[[f32; 3]], ax: u32, b: &[[f32; 3]], bx: u32, upright: bool| {
            let (mut light, mut cast) = (Vec::new(), Vec::new());
            for i in 0..CELLS {
                let strip = |kept: &[[f32; 3]], at: u32| {
                    let mut sum = [0.0f32; 3];
                    for along in i * side..(i + 1) * side {
                        for off in 0..STRIP {
                            let (x, y) = if upright {
                                (at + off, along)
                            } else {
                                (along, at + off)
                            };
                            let c = kept[(y * KEPT + x) as usize];
                            for k in 0..3 {
                                sum[k] += c[k];
                            }
                        }
                    }
                    sum.map(|v| v / (side * STRIP) as f32)
                };
                let (p, q) = (strip(a, ax), strip(b, bx));
                let y = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                light.push(((y(p) + FLOOR) / (y(q) + FLOOR)).log2());
                cast.push(
                    ((p[2] + FLOOR) / (p[1] + FLOOR)).log2()
                        - ((q[2] + FLOOR) / (q[1] + FLOOR)).log2(),
                );
            }
            // A change of capture steps the whole edge one way; ground
            // steps it here up and there down.
            let one_way = light.iter().all(|v| *v > 0.0) || light.iter().all(|v| *v < 0.0);
            (median(&mut light), median(&mut cast), one_way)
        };
        let mut out = String::from("## Joints between tiles of one level\n\n");
        out += "The step across the edge two neighbours of a level share, against the step across a line inside a tile (ground alone), in stops of luminance; `cast` is blue against green.\n\n";
        out += "| level | tiles | edges | inside: median / p95 | at edges: median / p95 | edges past the inside p95 | past 0.15 | past 0.30 | cast at edges p95 | seams: edges / lines inside |\n|---|---|---|---|---|---|---|---|---|---|\n";
        out += "\nA seam is a step of more than 0.15 stops that goes the same way all along the edge; the last column counts them at edges and, for comparison, along lines inside tiles.\n\n";
        let mut levels: BTreeMap<u8, Vec<&Coord>> = BTreeMap::new();
        for coord in self.tiles.keys() {
            levels.entry(coord.0).or_default().push(coord);
        }
        let quantile = |values: &mut Vec<f32>, q: f32| {
            if values.is_empty() {
                return f32::NAN;
            }
            values.sort_by(f32::total_cmp);
            values[((values.len() - 1) as f32 * q) as usize]
        };
        let mut worst: Vec<(f32, f32, Coord, Coord)> = Vec::new();
        for (level, coords) in &levels {
            let (mut inside, mut edges, mut casts) = (Vec::new(), Vec::new(), Vec::new());
            let (mut seams, mut seams_inside) = (0usize, 0usize);
            for coord in coords {
                let mine = &self.tiles[*coord].kept;
                let middle = KEPT / 2;
                for upright in [true, false] {
                    let (light, _, one_way) = across(mine, middle - STRIP, mine, middle, upright);
                    inside.push(light.abs());
                    seams_inside += usize::from(one_way && light.abs() > SEAM);
                }
                for (next, upright) in [
                    ((coord.0, coord.1 + 1, coord.2), true),
                    ((coord.0, coord.1, coord.2 + 1), false),
                ] {
                    let Some(other) = self.tiles.get(&next) else {
                        continue;
                    };
                    let (light, cast, one_way) =
                        across(mine, KEPT - STRIP, &other.kept, 0, upright);
                    edges.push(light.abs());
                    casts.push(cast.abs());
                    if one_way && light.abs() > SEAM {
                        seams += 1;
                        worst.push((light, cast, **coord, next));
                    }
                }
            }
            if edges.is_empty() {
                continue;
            }
            let bound = quantile(&mut inside, 0.95);
            let past = |limit: f32| {
                edges.iter().filter(|e| **e > limit).count() as f32 * 100.0 / edges.len() as f32
            };
            let (p_bound, p15, p30) = (past(bound), past(0.15), past(0.30));
            let _ = writeln!(
                out,
                "| {level} | {} | {} | {:.3} / {:.3} | {:.3} / {:.3} | {:.0}% | {:.0}% | {:.0}% | {:.3} | {seams} ({:.0}%) / {seams_inside} ({:.0}%) |",
                coords.len(),
                edges.len(),
                quantile(&mut inside, 0.5),
                bound,
                quantile(&mut edges, 0.5),
                quantile(&mut edges, 0.95),
                p_bound,
                p15,
                p30,
                quantile(&mut casts, 0.95),
                seams as f32 * 100.0 / edges.len() as f32,
                seams_inside as f32 * 100.0 / inside.len().max(1) as f32,
            );
        }
        worst.sort_by(|a, b| b.0.abs().total_cmp(&a.0.abs()));
        out += "\nThe widest steps (luminance, cast, the two tiles as level/x/y, and how many tiles of the film each is draped on):\n\n";
        for (light, cast, a, b) in worst.iter().take(20) {
            let _ = writeln!(
                out,
                "- {light:+.2} stops, cast {cast:+.2}: {}/{}/{} ({}) | {}/{}/{} ({})",
                a.0, a.1, a.2, self.tiles[a].drapes, b.0, b.1, b.2, self.tiles[b].drapes
            );
        }

        // A tile on an ancestor however far: the tile's whole tone against
        // the texel of the ancestor it lies in. Rough tile by tile, telling
        // over many.
        out += "\n## A level against every coarser level the film also reads\n\n";
        out += "Median over tiles of the tile's tone against the same ground in the coarser tile, in stops of luminance (positive: the finer is lighter), and cast (blue against green).\n\n| level | against | tiles | luminance median / p10 / p90 | cast median |\n|---|---|---|---|---|\n";
        let mut pairs: BTreeMap<(u8, u8), (Vec<f32>, Vec<f32>)> = BTreeMap::new();
        for (coord, light) in &self.tiles {
            for up in 1..=coord.0 {
                let Some(ancestor) = self
                    .tiles
                    .get(&(coord.0 - up, coord.1 >> up, coord.2 >> up))
                else {
                    continue;
                };
                // Where the tile's middle falls in what was kept of it.
                let span = 1u64 << up;
                let at = |v: u32| ((u64::from(v) % span) * 2 + 1) * u64::from(KEPT) / (span * 2);
                let (x, y) = (at(coord.1) as u32, at(coord.2) as u32);
                let under = ancestor.kept[(y.min(KEPT - 1) * KEPT + x.min(KEPT - 1)) as usize];
                let mut mine = [0.0f32; 3];
                for c in &light.kept {
                    for k in 0..3 {
                        mine[k] += c[k];
                    }
                }
                let mine = mine.map(|v| v / light.kept.len() as f32);
                let y_of = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                let entry = pairs.entry((coord.0, coord.0 - up)).or_default();
                entry
                    .0
                    .push(((y_of(mine) + FLOOR) / (y_of(under) + FLOOR)).log2());
                entry.1.push(
                    ((mine[2] + FLOOR) / (mine[1] + FLOOR)).log2()
                        - ((under[2] + FLOOR) / (under[1] + FLOOR)).log2(),
                );
            }
        }
        for ((level, against), (mut light, mut cast)) in pairs {
            let _ = writeln!(
                out,
                "| {level} | {against} | {} | {:+.2} / {:+.2} / {:+.2} | {:+.2} |",
                light.len(),
                quantile(&mut light, 0.5),
                quantile(&mut light, 0.1),
                quantile(&mut light, 0.9),
                quantile(&mut cast, 0.5),
            );
        }
        out
    }

    /// A picture of one level's tiles as the store holds them, each where
    /// it lies, with the seams between neighbours drawn in red: a step of
    /// more than 0.15 stops of luminance between the two tiles' facing
    /// edges. What is uniform within a patch and steps at its border is a
    /// capture; what the red encloses is its extent.
    pub fn seams_picture(&self, level: u8, path: &Path) -> Result<Option<(u32, u32)>, Error> {
        const SIDE: u32 = 16;
        let tiles: Vec<(&Coord, &TileLight)> =
            self.tiles.iter().filter(|(c, _)| c.0 == level).collect();
        let (Some(x0), Some(x1), Some(y0), Some(y1)) = (
            tiles.iter().map(|(c, _)| c.1).min(),
            tiles.iter().map(|(c, _)| c.1).max(),
            tiles.iter().map(|(c, _)| c.2).min(),
            tiles.iter().map(|(c, _)| c.2).max(),
        ) else {
            return Ok(None);
        };
        let (wide, high) = ((x1 - x0 + 1) * SIDE, (y1 - y0 + 1) * SIDE);
        if wide > 8192 || high > 8192 {
            return Ok(None);
        }
        let mut picture = image::RgbImage::from_pixel(wide, high, image::Rgb([40, 40, 40]));
        let stored = |v: f32| {
            let v = v.clamp(0.0, 1.0);
            let s = if v <= 0.003_130_8 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (s * 255.0).round() as u8
        };
        let step = KEPT / SIDE;
        for (coord, light) in &tiles {
            for j in 0..SIDE {
                for i in 0..SIDE {
                    // Lifted two stops, as a render would show it.
                    let c = cell(&light.kept, i * step, j * step, step).map(|v| stored(v * 4.0));
                    picture.put_pixel(
                        (coord.1 - x0) * SIDE + i,
                        (coord.2 - y0) * SIDE + j,
                        image::Rgb(c),
                    );
                }
            }
        }
        let luminance = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        for (coord, light) in &tiles {
            for (next, upright) in [
                ((coord.0, coord.1 + 1, coord.2), true),
                ((coord.0, coord.1, coord.2 + 1), false),
            ] {
                let Some(other) = self.tiles.get(&next) else {
                    continue;
                };
                // The whole facing strips, a sixteenth of a tile deep.
                let strip = |kept: &[[f32; 3]], at: u32| {
                    let mut sum = 0.0f32;
                    for along in 0..KEPT {
                        for off in 0..4 {
                            let (x, y) = if upright {
                                (at + off, along)
                            } else {
                                (along, at + off)
                            };
                            sum += luminance(kept[(y * KEPT + x) as usize]);
                        }
                    }
                    sum / (KEPT * 4) as f32
                };
                let apart = ((strip(&light.kept, KEPT - 4) + FLOOR)
                    / (strip(&other.kept, 0) + FLOOR))
                    .log2()
                    .abs();
                if apart <= 0.15 {
                    continue;
                }
                let shade = if apart > 0.5 {
                    [255, 0, 0]
                } else {
                    [255, 160, 0]
                };
                for along in 0..SIDE {
                    let (x, y) = if upright {
                        ((next.1 - x0) * SIDE, (coord.2 - y0) * SIDE + along)
                    } else {
                        ((coord.1 - x0) * SIDE + along, (next.2 - y0) * SIDE)
                    };
                    picture.put_pixel(x, y, image::Rgb(shade));
                }
            }
        }
        picture.save(path)?;
        Ok(Some((wide, high)))
    }

    /// What the film is made of: every imagery tile draped on a tile of
    /// it, cell by cell, counted for the tiles it is draped on.
    fn samples(&self) -> Vec<Sample> {
        let side = KEPT / CELLS;
        let mut out = Vec::new();
        for (coord, light) in &self.tiles {
            if light.drapes == 0 {
                continue;
            }
            let weight = light.drapes as f32 / (CELLS * CELLS) as f32;
            for j in 0..CELLS {
                for i in 0..CELLS {
                    out.push(Sample {
                        level: coord.0,
                        colour: cell(&light.kept, i * side, j * side, side),
                        weight,
                    });
                }
            }
        }
        out
    }

    /// How much light this render put on ground: its pictures' mean linear
    /// luminance for an imagery luminance of one — lighting and exposure
    /// together, as [`FilmGrade::fit`] wants it. Read off a render made
    /// with no grade. `None` before a picture came out.
    pub fn light(&self) -> Option<f32> {
        let luminance = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let (mut imagery, mut weight) = (0.0f64, 0.0f64);
        for s in self.samples() {
            imagery += f64::from(luminance(s.colour) * s.weight);
            weight += f64::from(s.weight);
        }
        let pictures: f64 = self.frames.iter().map(|f| f64::from(f.0.mean.exp2())).sum();
        (weight > 0.0 && imagery > 0.0 && !self.frames.is_empty())
            .then(|| (pictures / self.frames.len() as f64 / (imagery / weight)) as f32)
    }

    /// The film's own grade, fitted on the imagery this render read: its
    /// levels brought to the one it draws most, and the film brought to
    /// `target`, within `bounds`. `light` is [`Self::light`]'s, or the
    /// renderer's known one.
    pub fn film_grade(&self, light: f32, target: &LookTarget, bounds: &Bounds) -> FilmGrade {
        let seen: Vec<Seen> = self.seen().into_iter().map(|s| s.0).collect();
        FilmGrade::fit(&seen, &self.samples(), light, target, bounds)
    }

    /// One grade a level, fitted on the tiles this render read: the one
    /// that leaves the least step at the joints between sources.
    pub fn solve(&self, anchor: u8) -> LevelGrades {
        let seen: Vec<Seen> = self.seen().into_iter().map(|s| s.0).collect();
        LevelGrades::solve(
            &seen,
            &LevelParams {
                anchor,
                clamp_stops: 3.0,
                ..LevelParams::default()
            },
        )
    }

    /// One table a place: the grades fitted on the tiles of each region
    /// this render read, with how many tiles each was fitted on. Tiles
    /// coarser than a region are everywhere, and count in every one.
    pub fn solve_places(&self, anchor: u8) -> Vec<((u32, u32), LevelGrades, usize)> {
        let seen = self.seen();
        let places: std::collections::BTreeSet<(u32, u32)> = seen
            .iter()
            .map(|s| s.1)
            .filter(|place| place.0 != u32::MAX)
            .collect();
        places
            .into_iter()
            .map(|place| {
                let here: Vec<Seen> = seen
                    .iter()
                    .filter(|s| s.1 == place || s.1 .0 == u32::MAX)
                    .map(|s| s.0.clone())
                    .collect();
                let tiles = seen.iter().filter(|s| s.1 == place).count();
                let params = LevelParams {
                    anchor,
                    clamp_stops: 3.0,
                    ..LevelParams::default()
                };
                (place, LevelGrades::solve(&here, &params), tiles)
            })
            .collect()
    }

    /// Writes each place's table under `dir` at the key a tile store keeps
    /// it by, beside the imagery layer `layer` — ready to be put there —
    /// and returns the keys.
    pub fn write_places(&self, dir: &Path, layer: &str, anchor: u8) -> Result<Vec<String>, Error> {
        let mut keys = Vec::new();
        for ((x, y), grades, _) in self.solve_places(anchor) {
            let key = region_key(layer, x, y);
            let path = dir.join(&key);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, grades.to_json())?;
            keys.push(key);
        }
        Ok(keys)
    }

    /// Writes `tiles.csv`, `frames.csv`, `tone.json` and `report.md` into
    /// `dir`, and returns the report. `title` heads it; `solved` is
    /// [`Self::solve`]'s.
    pub fn write(&self, dir: &Path, title: &str, solved: &LevelGrades) -> Result<String, Error> {
        std::fs::create_dir_all(dir)?;
        let mut report = format!("# {title}\n");
        let said = |g: &Grade| {
            if g.is_identity() {
                "—".to_string()
            } else {
                let stops = g.gain.map(f32::log2);
                format!(
                    "black {:+.3} {:+.3} {:+.3}, gain {:+.2} {:+.2} {:+.2} stops, contrast {:.2} about {:.2}, saturation {:.2}",
                    g.black[0], g.black[1], g.black[2], stops[0], stops[1], stops[2],
                    g.contrast, g.pivot, g.saturation
                )
            }
        };

        writeln!(report, "\n## Imagery going in, by level\n")?;
        writeln!(report, "Luminance in stops under white, as stored.\n")?;
        writeln!(report, "| level | tiles | median | darkest | brightest | p10–p90 | grade fitted | grade applied |")?;
        writeln!(report, "|---|---|---|---|---|---|---|---|")?;
        let mut by_level: BTreeMap<u8, Vec<&TileLight>> = BTreeMap::new();
        for (coord, light) in &self.tiles {
            by_level.entry(coord.0).or_default().push(light);
        }
        for (level, lights) in &by_level {
            let mut values: Vec<f32> = lights.iter().map(|l| stops(l.tone)).collect();
            values.sort_by(f32::total_cmp);
            let at = |q: f32| values[((values.len() - 1) as f32 * q) as usize];
            writeln!(
                report,
                "| {level} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {} |",
                values.len(),
                at(0.5),
                at(0.0),
                at(1.0),
                at(0.9) - at(0.1),
                said(&solved.of(*level)),
                said(&lights[0].grade)
            )?;
        }

        writeln!(report, "\n## The step at a joint between sources\n")?;
        writeln!(report, "A tile against an ancestor of another source, over the same ground, in stops: the median cell and the 90th centile.\n")?;
        writeln!(
            report,
            "| source (levels) | tiles | cells | nothing done | best gain alone | the grade |"
        )?;
        writeln!(report, "|---|---|---|---|---|---|")?;
        for r in &solved.sources {
            let levels: Vec<String> = r.levels.iter().map(u8::to_string).collect();
            writeln!(
                report,
                "| {} | {} | {} | {:.2} / {:.2} | {:.2} / {:.2} | {:.2} / {:.2} |",
                levels.join(" "),
                r.observations,
                r.cells,
                r.before.0,
                r.before.1,
                r.gain_alone.0,
                r.gain_alone.1,
                r.after.0,
                r.after.1
            )?;
        }

        // The same step, region by region: with nothing done, as this
        // render composed it, and under the grade fitted.
        writeln!(
            report,
            "\n## The same step, region by region (level {REGION} tiles)\n"
        )?;
        writeln!(report, "Median cell, worst channel, in stops.\n")?;
        writeln!(
            report,
            "| region x/y | tiles | nothing done | as applied in this render | the grade fitted |"
        )?;
        writeln!(report, "|---|---|---|---|---|")?;
        let applied: BTreeMap<u8, Grade> = self.tiles.iter().map(|(c, l)| (c.0, l.grade)).collect();
        let applied = |level: u8| applied.get(&level).copied().unwrap_or(Grade::IDENTITY);
        let mut by_region: BTreeMap<(u32, u32), (usize, [Vec<f32>; 3])> = BTreeMap::new();
        for (s, region) in self.seen() {
            if solved.of(s.level) == solved.of(s.ancestor) {
                continue;
            }
            let entry = by_region.entry(region).or_default();
            entry.0 += 1;
            let ways = [
                (Grade::IDENTITY, Grade::IDENTITY),
                (applied(s.level), applied(s.ancestor)),
                (solved.of(s.level), solved.of(s.ancestor)),
            ];
            for (way, (mine, theirs)) in ways.iter().enumerate() {
                for (t, u) in s.tile.iter().zip(&s.under) {
                    entry.1[way].push(step(mine.apply(*t), theirs.apply(*u)));
                }
            }
        }
        for (region, (tiles, mut ways)) in by_region {
            writeln!(
                report,
                "| {}/{} | {tiles} | {:.2} | {:.2} | {:.2} |",
                region.0,
                region.1,
                median(&mut ways[0]),
                median(&mut ways[1]),
                median(&mut ways[2])
            )?;
        }

        let lights = self.pictures();
        writeln!(report, "\n## Pictures coming out ({})\n", lights.len())?;
        writeln!(report, "| measure, in stops | value |")?;
        writeln!(report, "|---|---|")?;
        let column = |of: &dyn Fn(&Light) -> f32| lights.iter().map(of).collect::<Vec<f32>>();
        let range = |mut v: Vec<f32>| {
            v.sort_by(f32::total_cmp);
            v.last().copied().unwrap_or(0.0) - v.first().copied().unwrap_or(0.0)
        };
        let jump = |v: Vec<f32>| {
            v.windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0, f32::max)
        };
        let worst = |v: Vec<f32>| v.into_iter().map(f32::abs).fold(0.0, f32::max);
        for (name, value) in [
            (
                "mean luminance, median picture",
                median(&mut column(&|l| l.mean)),
            ),
            (
                "mean luminance, range over the film",
                range(column(&|l| l.mean)),
            ),
            (
                "largest step from one picture to the next",
                jump(column(&|l| l.mean)),
            ),
            (
                "contrast within a picture, p95 − p5, median",
                median(&mut column(&|l| l.p95 - l.p5)),
            ),
            (
                "top third − bottom third, median picture",
                median(&mut column(&|l| l.bands[0] - l.bands[2])),
            ),
            (
                "top third − bottom third, worst picture",
                worst(column(&|l| l.bands[0] - l.bands[2])),
            ),
            ("red against green, median", median(&mut column(&|l| l.red))),
            (
                "blue against green, median",
                median(&mut column(&|l| l.blue)),
            ),
        ] {
            writeln!(report, "| {name} | {value:+.2} |")?;
        }
        if self.undecodable > 0 {
            writeln!(
                report,
                "\n{} imagery tiles could not be decoded for measuring.",
                self.undecodable
            )?;
        }

        let mut csv =
            String::from("level,x,y,drapes,renewed,red,green,blue,stops,stops_as_composed\n");
        let mut coords: Vec<&Coord> = self.tiles.keys().collect();
        coords.sort();
        for coord in coords {
            let t = &self.tiles[coord];
            writeln!(
                csv,
                "{},{},{},{},{},{:.4},{:.4},{:.4},{:.3},{:.3}",
                coord.0,
                coord.1,
                coord.2,
                t.drapes,
                u8::from(t.renewed),
                t.tone[0],
                t.tone[1],
                t.tone[2],
                stops(t.tone),
                stops(t.grade.apply(t.tone))
            )?;
        }
        std::fs::write(dir.join("tiles.csv"), csv)?;

        let mut csv = String::from(
            "frame,tiles,entered,layers_by_level,mean,p5,p50,p95,top,middle,bottom,red,blue,render_ms\n",
        );
        for (l, tiles, entered, layers, t) in &self.frames {
            let levels: Vec<String> = layers.iter().map(|(l, n)| format!("{l}:{n}")).collect();
            writeln!(
                csv,
                "{},{tiles},{entered},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{t:.1}",
                l.frame, levels.join(" "), l.mean, l.p5, l.p50, l.p95,
                l.bands[0], l.bands[1], l.bands[2], l.red, l.blue
            )?;
        }
        std::fs::write(dir.join("frames.csv"), csv)?;
        std::fs::write(dir.join("tone.json"), solved.to_json())?;
        std::fs::write(dir.join("report.md"), &report)?;
        Ok(report)
    }
}
