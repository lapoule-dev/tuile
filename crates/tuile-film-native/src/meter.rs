// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A light meter on a render.
//!
//! Going in: the tone of every imagery tile as the store holds it, and —
//! tile against ancestor, over the same ground — what one level is from
//! another, over the whole film and region by region. Coming out: the
//! luminance of every picture.
//!
//! It is an [`Observer`]: the render does not know it is there.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use tuile_radiometry::{tone_of, LevelGains, LevelParams, Observation};

use crate::observe::{FrameOut, ImageryIn, Observer, TileIn};
use crate::Error;

type Coord = (u8, u32, u32);

/// The side of what is kept of each imagery tile: enough to find, in an
/// ancestor five levels up, the ground a tile covers.
const KEPT: u32 = 32;
/// How far up a tile looks for an ancestor to be seen against.
const REACH: u8 = 5;
/// The level whose tiles are the regions a change of source is broken down
/// by.
const REGION: u8 = 9;

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
    /// The tile reduced to `KEPT` a side, stored values averaged.
    kept: Vec<u8>,
    gain: [f32; 3],
    renewed: bool,
    /// Tiles of the film whose drape it is in.
    drapes: u32,
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

fn light_of(frame: &FrameOut<'_>) -> Light {
    let (width, height) = (frame.width, frame.height);
    let mut sum = [0.0f64; 3];
    let mut bands = [[0.0f64; 2]; 3];
    let mut each = Vec::with_capacity((width * height) as usize);
    for (i, p) in frame.rgba.chunks_exact(4).enumerate() {
        let rgb = [
            linear(p[0].into()),
            linear(p[1].into()),
            linear(p[2].into()),
        ];
        for c in 0..3 {
            sum[c] += f64::from(rgb[c]);
        }
        let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
        let band = ((i as u32 / width) * 3 / height).min(2) as usize;
        bands[band][0] += f64::from(y);
        bands[band][1] += 1.0;
        each.push(y);
    }
    each.sort_by(f32::total_cmp);
    let at = |q: f32| {
        each.get(((each.len().max(1) - 1) as f32 * q) as usize)
            .copied()
            .unwrap_or(0.0)
            .max(1e-6)
            .log2()
    };
    let n = f64::from(width * height).max(1.0);
    let mean = sum.map(|s| (s / n) as f32);
    Light {
        frame: frame.frame,
        mean: stops(mean),
        p5: at(0.05),
        p50: at(0.5),
        p95: at(0.95),
        bands: bands.map(|b| ((b[0] / b[1].max(1.0)) as f32).max(1e-6).log2()),
        red: (mean[0] / mean[1].max(1e-6)).max(1e-6).log2(),
        blue: (mean[2] / mean[1].max(1e-6)).max(1e-6).log2(),
    }
}

/// Measures the light of a render: see the module.
#[derive(Default)]
pub struct LightMeter {
    tiles: HashMap<Coord, TileLight>,
    frames: Vec<(Light, usize, usize, Vec<(u8, u32)>, [f64; 3])>,
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
                .into_raw();
        self.tiles.insert(
            (tile.level, tile.x, tile.y),
            TileLight {
                tone,
                kept,
                gain: tile.gain,
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
            [frame.timings.read, frame.timings.build, frame.timings.gpu],
        ));
    }
}

impl LightMeter {
    /// The pictures' light, in the order they came out.
    pub fn pictures(&self) -> Vec<Light> {
        self.frames.iter().map(|f| f.0).collect()
    }

    /// Every tile against its two nearest ancestors among the tiles seen,
    /// and the region each observation is in.
    fn observations(&self) -> Vec<(Observation, (u32, u32))> {
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
                let span = (1u32 << up) as f32;
                let (x, y) = (
                    (coord.1 & ((1 << up) - 1)) as f32 / span,
                    (coord.2 & ((1 << up) - 1)) as f32 / span,
                );
                let (under, counted) = tone_of(
                    &ancestor.kept,
                    KEPT,
                    KEPT,
                    (x, y, x + 1.0 / span, y + 1.0 / span),
                );
                if counted < 0.5 || under.iter().any(|c| *c <= 0.0) {
                    continue;
                }
                let mut gain = [0.0f32; 3];
                for c in 0..3 {
                    gain[c] = (under[c] / light.tone[c]).log2();
                }
                let region = if coord.0 >= REGION {
                    let down = coord.0 - REGION;
                    (coord.1 >> down, coord.2 >> down)
                } else {
                    (u32::MAX, u32::MAX)
                };
                out.push((
                    Observation {
                        level: coord.0,
                        ancestor: coord.0 - up,
                        gain,
                        // An ancestor far up shows this ground in a handful
                        // of what was kept of it.
                        weight: (KEPT as f32 / span / 4.0).min(1.0),
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

    /// One gain a level, solved from the tiles this render read.
    pub fn solve(&self, anchor: u8) -> LevelGains {
        let seen: Vec<Observation> = self.observations().iter().map(|o| o.0).collect();
        LevelGains::solve(
            &seen,
            &LevelParams {
                anchor,
                ..LevelParams::default()
            },
        )
    }

    /// Writes `tiles.csv`, `frames.csv`, `tone.json` and `report.md` into
    /// `dir`, and returns the report. `title` heads it; `solved` is
    /// [`Self::solve`]'s.
    pub fn write(&self, dir: &Path, title: &str, solved: &LevelGains) -> Result<String, Error> {
        std::fs::create_dir_all(dir)?;
        let mut report = format!("# {title}\n");

        writeln!(report, "\n## Imagery going in, by level\n")?;
        writeln!(report, "Luminance in stops under white, as stored. The gain is the one this render's tiles solve to, in stops, R G B.\n")?;
        writeln!(report, "| level | tiles | median | darkest | brightest | p10–p90 | gain solved | gain applied |")?;
        writeln!(report, "|---|---|---|---|---|---|---|---|")?;
        let mut by_level: BTreeMap<u8, Vec<&TileLight>> = BTreeMap::new();
        for (coord, light) in &self.tiles {
            by_level.entry(coord.0).or_default().push(light);
        }
        for (level, lights) in &by_level {
            let mut values: Vec<f32> = lights.iter().map(|l| stops(l.tone)).collect();
            values.sort_by(f32::total_cmp);
            let at = |q: f32| values[((values.len() - 1) as f32 * q) as usize];
            let (g, applied) = (solved.of(*level), lights[0].gain.map(f32::log2));
            writeln!(
                report,
                "| {level} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:+.2} {:+.2} {:+.2} | {:+.2} {:+.2} {:+.2} |",
                values.len(), at(0.5), at(0.0), at(1.0), at(0.9) - at(0.1),
                g[0], g[1], g[2], applied[0], applied[1], applied[2]
            )?;
        }

        writeln!(report, "\n## Level against level, over the same ground\n")?;
        writeln!(report, "| level → ancestor | tiles | gain R G B (median) | spread among tiles | left by one gain a level |")?;
        writeln!(report, "|---|---|---|---|---|")?;
        for p in &solved.pairs {
            writeln!(
                report,
                "| {} → {} | {} | {:+.2} {:+.2} {:+.2} | {:.2} | {:+.2} {:+.2} {:+.2} |",
                p.level,
                p.ancestor,
                p.observations,
                p.gain[0],
                p.gain[1],
                p.gain[2],
                p.spread,
                p.residual[0],
                p.residual[1],
                p.residual[2]
            )?;
        }

        writeln!(
            report,
            "\n## A change of source, region by region (level {REGION} tiles)\n"
        )?;
        writeln!(
            report,
            "| region x/y | tiles | step R G B (median) | spread | left by one gain a level |"
        )?;
        writeln!(report, "|---|---|---|---|---|")?;
        let observations = self.observations();
        let mut by_region: BTreeMap<(u32, u32), Vec<&Observation>> = BTreeMap::new();
        for (o, region) in &observations {
            if solved.of(o.level) != solved.of(o.ancestor) {
                by_region.entry(*region).or_default().push(o);
            }
        }
        for (region, seen) in &by_region {
            let (mut step, mut left, mut spread) = ([0.0f32; 3], [0.0f32; 3], 0.0f32);
            for c in 0..3 {
                let mut values: Vec<f32> = seen.iter().map(|o| o.gain[c]).collect();
                step[c] = median(&mut values);
                let mut off: Vec<f32> = values.iter().map(|v| (v - step[c]).abs()).collect();
                spread = spread.max(median(&mut off));
                left[c] = step[c] - (solved.of(seen[0].level)[c] - solved.of(seen[0].ancestor)[c]);
            }
            writeln!(
                report,
                "| {}/{} | {} | {:+.2} {:+.2} {:+.2} | {:.2} | {:+.2} {:+.2} {:+.2} |",
                region.0,
                region.1,
                seen.len(),
                step[0],
                step[1],
                step[2],
                spread,
                left[0],
                left[1],
                left[2]
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

        let mut csv = String::from(
            "level,x,y,drapes,renewed,red,green,blue,stops,gain_r,gain_g,gain_b,stops_as_composed\n",
        );
        let mut coords: Vec<&Coord> = self.tiles.keys().collect();
        coords.sort();
        for coord in coords {
            let t = &self.tiles[coord];
            let after = [
                t.tone[0] * t.gain[0],
                t.tone[1] * t.gain[1],
                t.tone[2] * t.gain[2],
            ];
            writeln!(
                csv,
                "{},{},{},{},{},{:.4},{:.4},{:.4},{:.3},{:.3},{:.3},{:.3},{:.3}",
                coord.0,
                coord.1,
                coord.2,
                t.drapes,
                u8::from(t.renewed),
                t.tone[0],
                t.tone[1],
                t.tone[2],
                stops(t.tone),
                t.gain[0].log2(),
                t.gain[1].log2(),
                t.gain[2].log2(),
                stops(after)
            )?;
        }
        std::fs::write(dir.join("tiles.csv"), csv)?;

        let mut csv = String::from(
            "frame,tiles,entered,layers_by_level,mean,p5,p50,p95,top,middle,bottom,red,blue,read_ms,build_ms,gpu_ms\n",
        );
        for (l, tiles, entered, layers, t) in &self.frames {
            let levels: Vec<String> = layers.iter().map(|(l, n)| format!("{l}:{n}")).collect();
            writeln!(
                csv,
                "{},{tiles},{entered},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.1},{:.1},{:.1}",
                l.frame, levels.join(" "), l.mean, l.p5, l.p50, l.p95,
                l.bands[0], l.bands[1], l.bands[2], l.red, l.blue, t[0], t[1], t[2]
            )?;
        }
        std::fs::write(dir.join("frames.csv"), csv)?;
        std::fs::write(dir.join("tone.json"), solved.to_json())?;
        std::fs::write(dir.join("report.md"), &report)?;
        Ok(report)
    }
}
