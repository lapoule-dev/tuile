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

use tuile_radiometry::{region_key, tone_of, Grade, LevelGrades, LevelParams, Seen, REGION_LEVEL};

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
