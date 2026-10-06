// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One grade per level: black point, gain, contrast and saturation.
//!
//! A gain brings a level's mean tone to another's and nothing else. Two
//! sources differ by more than that: one has a veil over its shadows that
//! the other has not, one is flatter, one is duller. A [`Grade`] is the
//! four things a colourist would turn to bring the one to the other, in
//! the order they are applied, in linear light:
//!
//! 1. **black point** — a colour taken away, the veil;
//! 2. **gain** — a multiplier per channel, the exposure and the cast;
//! 3. **contrast** — a power on luminance about a pivot;
//! 4. **saturation** — a factor on what is not luminance.
//!
//! As with a gain, the grade is the level's and every tile of a level is
//! given the same one, so two neighbours of a level meet after it as they
//! met before it.
//!
//! The four are found together, by minimising what shows: the step at a
//! joint. Where a tile of one source is drawn beside a tile of another,
//! the two meet along a line of ground both sources have a picture of —
//! an ancestor covers all of its descendants. So the step a viewer would
//! see at that joint is, over that ground, the graded tile against the
//! graded ancestor; and every tile seen against an ancestor of another
//! source, brought to the same sampling, is a sample of every joint the
//! film can draw between the two. The grade is the one that makes those
//! differences least, in stops, cell for cell — a least-squares fit
//! (Levenberg–Marquardt on the eight numbers) with a loss that does not
//! let changed ground, a cloud or a harvested field, pull it.
//!
//! Cells are coarse on purpose. A joint is seen as a step in tone over
//! tens of texels, not as a disagreement of texels; and two pictures of
//! one ground taken years apart agree on its regions long after they have
//! stopped agreeing on its fields.

use std::collections::{BTreeMap, BTreeSet};

use crate::levels::{LevelGains, LevelParams, Observation};

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn luma(c: [f32; 3]) -> f32 {
    LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2]
}

fn linear_of_stored(value: f32) -> f32 {
    let v = (value / 255.0).clamp(0.0, 1.0);
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn stored_of_linear(value: f32) -> u8 {
    let v = value.clamp(0.0, 1.0);
    let stored = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (stored * 255.0).round() as u8
}

/// What a level's colour goes through to be the anchor's: see the module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grade {
    /// Taken away first, linear RGB. Negative adds a veil.
    pub black: [f32; 3],
    /// Then multiplied by, per channel.
    pub gain: [f32; 3],
    /// Then luminance is raised to this power about `pivot`: above one,
    /// more contrast.
    pub contrast: f32,
    /// The luminance contrast leaves where it is.
    pub pivot: f32,
    /// Then what is not luminance is multiplied by this.
    pub saturation: f32,
}

impl Default for Grade {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Grade {
    /// Changes nothing.
    pub const IDENTITY: Self = Self {
        black: [0.0; 3],
        gain: [1.0; 3],
        contrast: 1.0,
        pivot: 0.18,
        saturation: 1.0,
    };

    /// Whether this changes nothing: stored bytes then go through
    /// untouched, exactly.
    pub fn is_identity(&self) -> bool {
        self.black == [0.0; 3]
            && self.gain == [1.0; 3]
            && self.contrast == 1.0
            && self.saturation == 1.0
    }

    /// The grade taken at `strength`: 0 is the identity, 1 all of it.
    #[must_use]
    pub fn at(&self, strength: f32) -> Self {
        if strength >= 1.0 {
            return *self;
        }
        if strength <= 0.0 {
            return Self::IDENTITY;
        }
        Self {
            black: self.black.map(|b| b * strength),
            gain: self.gain.map(|g| g.powf(strength)),
            contrast: self.contrast.powf(strength),
            pivot: self.pivot,
            saturation: self.saturation.powf(strength),
        }
    }

    /// One linear colour, graded. Not clamped above: a caller storing it
    /// clips.
    pub fn apply(&self, colour: [f32; 3]) -> [f32; 3] {
        let mut c = [0.0f32; 3];
        for i in 0..3 {
            c[i] = (colour[i] - self.black[i]).max(0.0) * self.gain[i];
        }
        if self.contrast != 1.0 {
            let by = (luma(c).max(1e-5) / self.pivot.max(1e-5)).powf(self.contrast - 1.0);
            c = c.map(|v| v * by);
        }
        if self.saturation != 1.0 {
            let y = luma(c);
            c = c.map(|v| (y + (v - y) * self.saturation).max(0.0));
        }
        c
    }

    /// An image of tightly packed RGBA8, sRGB-encoded, graded in place;
    /// alpha is left alone. What a renderer's composition does to a layer,
    /// for the one that composes without a GPU.
    pub fn apply_rgba8(&self, rgba: &mut [u8]) {
        if self.is_identity() {
            return;
        }
        let table: [f32; 256] = std::array::from_fn(|v| linear_of_stored(v as f32));
        for texel in rgba.chunks_exact_mut(4) {
            let graded = self.apply([
                table[texel[0] as usize],
                table[texel[1] as usize],
                table[texel[2] as usize],
            ]);
            for c in 0..3 {
                texel[c] = stored_of_linear(graded[c]);
            }
        }
    }
}

/// One tile seen against one of its ancestors, both brought to the same
/// sampling of the same ground: linear RGB, cell for cell.
#[derive(Debug, Clone)]
pub struct Seen {
    pub level: u8,
    pub ancestor: u8,
    pub tile: Vec<[f32; 3]>,
    pub under: Vec<[f32; 3]>,
}

/// What a source's grade was found from, and what it leaves at a joint.
#[derive(Debug, Clone, PartialEq)]
pub struct GradeReport {
    /// The source: its coarsest level.
    pub source: u8,
    /// The levels it is.
    pub levels: Vec<u8>,
    /// Tiles seen against an ancestor of a source already graded, and the
    /// cells they were compared over.
    pub observations: usize,
    pub cells: usize,
    /// The step at a joint, in stops, as the median over cells and
    /// channels and as the 90th centile: with nothing done…
    pub before: (f32, f32),
    /// …with the best gain alone…
    pub gain_alone: (f32, f32),
    /// …and with the grade.
    pub after: (f32, f32),
}

/// A grade per level, to bring every level to the anchor's.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelGrades {
    pub anchor: u8,
    pub grades: BTreeMap<u8, Grade>,
    pub sources: Vec<GradeReport>,
}

/// Cells a tile and its ancestor must share before they say anything.
const CELLS: usize = 12;
/// Cells a source is fitted on at most: more says nothing more.
const FITTED: usize = 24_000;
/// Under this, a linear value is the dark, where a ratio means little.
const FLOOR: f32 = 0.004;
/// A difference of about this, in natural-log units (half a stop), is
/// where ground begins to be taken for ground that changed.
const CHANGED: f32 = 0.35;

fn quantile(values: &mut [f32], q: f32) -> f32 {
    values.sort_by(f32::total_cmp);
    values[((values.len() - 1) as f32 * q).round() as usize]
}

/// Kept to a 4096th: the same numbers wherever this ran.
fn kept(value: f32) -> f32 {
    (value * 4096.0).round() / 4096.0
}

/// The grade a vector of the fit's eight unknowns stands for: black point
/// in hundredths, then gain, contrast and saturation as natural logs.
fn grade_of(theta: &[f32; 8], pivot: f32, clamp_stops: f32) -> Grade {
    let stops = clamp_stops * std::f32::consts::LN_2;
    Grade {
        black: [0, 1, 2].map(|i| (theta[i] * 0.01).clamp(-0.1, 0.1)),
        gain: [3, 4, 5].map(|i| theta[i].clamp(-stops, stops).exp()),
        contrast: theta[6].clamp(-0.7, 0.7).exp(),
        pivot,
        saturation: theta[7].clamp(-0.7, 0.7).exp(),
    }
}

/// The step at every cell and channel between a graded tile and what is
/// under it, in natural-log units.
fn steps(grade: &Grade, cells: &[([f32; 3], [f32; 3])], out: &mut Vec<f32>) {
    out.clear();
    for (tile, under) in cells {
        let graded = grade.apply(*tile);
        for i in 0..3 {
            out.push(((graded[i] + FLOOR) / (under[i] + FLOOR)).ln());
        }
    }
}

/// What a set of steps costs. A small step counts by its square; a large
/// one counts for less and less the larger it is, so that ground which is
/// simply no longer the same — a fifth of a tile under snow — is left out
/// of the fit rather than averaged into it.
fn cost(steps: &[f32]) -> f32 {
    steps
        .iter()
        .map(|r| 0.5 * CHANGED * CHANGED * (1.0 + (r / CHANGED) * (r / CHANGED)).ln())
        .sum()
}

/// How much a step counts in the fit: the weight that cost stands for.
fn counted(step: f32) -> f32 {
    1.0 / (1.0 + (step / CHANGED) * (step / CHANGED))
}

/// The median and the 90th centile of the steps, in stops.
fn shown(steps: &[f32]) -> (f32, f32) {
    if steps.is_empty() {
        return (0.0, 0.0);
    }
    let mut size: Vec<f32> = steps
        .iter()
        .map(|r| r.abs() / std::f32::consts::LN_2)
        .collect();
    (quantile(&mut size, 0.5), quantile(&mut size, 0.9))
}

/// Solves `a · x = b` for eight unknowns, in place. `None` if it cannot.
fn solved(mut a: [[f32; 8]; 8], mut b: [f32; 8]) -> Option<[f32; 8]> {
    for col in 0..8 {
        let pivot = (col..8).max_by(|x, y| a[*x][col].abs().total_cmp(&a[*y][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..8 {
            let by = a[row][col] / a[col][col];
            for k in col..8 {
                a[row][k] -= by * a[col][k];
            }
            b[row] -= by * b[col];
        }
    }
    let mut x = [0.0f32; 8];
    for row in (0..8).rev() {
        let rest: f32 = (row + 1..8).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - rest) / a[row][row];
    }
    Some(x)
}

/// The grade that makes the steps over `cells` least: Levenberg–Marquardt
/// from `start`, with the weights of the loss recomputed at each step.
fn fitted(
    cells: &[([f32; 3], [f32; 3])],
    start: [f32; 8],
    free: [bool; 8],
    pivot: f32,
    clamp_stops: f32,
) -> [f32; 8] {
    let mut theta = start;
    let (mut at, mut moved) = (Vec::new(), Vec::new());
    steps(&grade_of(&theta, pivot, clamp_stops), cells, &mut at);
    let mut best = cost(&at);
    let mut damping = 1e-2f32;
    let mut columns: Vec<Vec<f32>> = vec![Vec::new(); 8];
    for _ in 0..60 {
        // How each step moves with each unknown, by trying it.
        for j in 0..8 {
            columns[j].clear();
            if !free[j] {
                continue;
            }
            let mut nudged = theta;
            nudged[j] += 1e-3;
            steps(&grade_of(&nudged, pivot, clamp_stops), cells, &mut moved);
            columns[j].extend(moved.iter().zip(&at).map(|(m, r)| (m - r) / 1e-3));
        }
        let weights: Vec<f32> = at.iter().map(|r| counted(*r)).collect();
        let (mut normal, mut slope) = ([[0.0f32; 8]; 8], [0.0f32; 8]);
        for j in 0..8 {
            if !free[j] {
                normal[j][j] = 1.0;
                continue;
            }
            for k in j..8 {
                if !free[k] {
                    continue;
                }
                let sum: f32 = (0..at.len())
                    .map(|n| weights[n] * columns[j][n] * columns[k][n])
                    .sum();
                normal[j][k] = sum;
                normal[k][j] = sum;
            }
            slope[j] = -(0..at.len())
                .map(|n| weights[n] * columns[j][n] * at[n])
                .sum::<f32>();
        }
        let mut better = false;
        for _ in 0..8 {
            let mut damped = normal;
            for j in 0..8 {
                if free[j] {
                    damped[j][j] *= 1.0 + damping;
                    damped[j][j] += 1e-9;
                }
            }
            let Some(step) = solved(damped, slope) else {
                damping *= 10.0;
                continue;
            };
            let mut tried = theta;
            for j in 0..8 {
                tried[j] += step[j];
            }
            steps(&grade_of(&tried, pivot, clamp_stops), cells, &mut moved);
            let costs = cost(&moved);
            if costs < best {
                let gained = best - costs;
                theta = tried;
                best = costs;
                std::mem::swap(&mut at, &mut moved);
                damping = (damping * 0.3).max(1e-6);
                better = gained > best * 1e-6;
                break;
            }
            damping *= 10.0;
        }
        if !better {
            break;
        }
    }
    theta
}

impl LevelGrades {
    /// The grade of a level; the identity for one that was never seen.
    pub fn of(&self, level: u8) -> Grade {
        self.grades.get(&level).copied().unwrap_or(Grade::IDENTITY)
    }

    /// Solves what was seen for one grade a level.
    ///
    /// Which levels are one source is found first, from tone alone
    /// ([`LevelGains`]): levels of one source are given one grade, exactly,
    /// and the anchor's source the identity. Then each other source, the
    /// best observed first, is fitted against the ancestors it was seen
    /// over that are already graded — graded themselves, so that a third
    /// source is brought to the anchor and not to the second.
    pub fn solve(seen: &[Seen], params: &LevelParams) -> Self {
        let usable: Vec<&Seen> = seen
            .iter()
            .filter(|s| s.tile.len() == s.under.len() && s.tile.len() >= CELLS)
            .collect();
        let mean = |cells: &[[f32; 3]]| {
            let mut sum = [0.0f32; 3];
            for c in cells {
                for i in 0..3 {
                    sum[i] += c[i];
                }
            }
            sum.map(|s| (s / cells.len() as f32).max(1e-6))
        };
        let observations: Vec<Observation> = usable
            .iter()
            .map(|s| {
                let (tile, under) = (mean(&s.tile), mean(&s.under));
                Observation {
                    level: s.level,
                    ancestor: s.ancestor,
                    gain: [0, 1, 2].map(|i| (under[i] / tile[i]).log2()),
                    weight: 1.0,
                }
            })
            .collect();
        let tones = LevelGains::solve(&observations, params);
        let anchor = tones.anchor;
        let source_of = |level: u8| tones.sources.get(&level).copied().unwrap_or(level);

        let mut of_source: BTreeMap<u8, Grade> = BTreeMap::new();
        of_source.insert(source_of(anchor), Grade::IDENTITY);
        let all: BTreeSet<u8> = tones.sources.values().copied().collect();
        let mut reports = Vec::new();
        loop {
            // The ungraded source with the most to go on against what is
            // graded.
            let graded: BTreeSet<u8> = of_source.keys().copied().collect();
            let against = |source: u8| -> Vec<&Seen> {
                usable
                    .iter()
                    .filter(|s| {
                        source_of(s.level) == source && graded.contains(&source_of(s.ancestor))
                    })
                    .copied()
                    .collect()
            };
            let next = all
                .iter()
                .filter(|s| !graded.contains(s))
                .map(|s| (*s, against(*s).len()))
                .filter(|(_, count)| *count > 0)
                .max_by_key(|(_, count)| *count);
            let Some((source, observations)) = next else {
                break;
            };

            // Every cell of every such tile against what is under it — the
            // ancestor as it will be drawn, graded.
            let mut cells: Vec<([f32; 3], [f32; 3])> = Vec::new();
            for s in against(source) {
                let grade = of_source[&source_of(s.ancestor)];
                cells.extend(
                    s.tile
                        .iter()
                        .zip(&s.under)
                        .map(|(t, u)| (*t, grade.apply(*u))),
                );
            }
            if cells.len() > FITTED {
                let every = cells.len().div_ceil(FITTED);
                cells = cells.into_iter().step_by(every).collect();
            }
            let mut under: Vec<f32> = cells.iter().map(|c| luma(c.1).max(1e-5)).collect();
            let pivot = kept(quantile(&mut under, 0.5).clamp(0.01, 1.0));

            // The best gain alone first — where the fit starts from, and
            // what the grade is to be judged against.
            let gain_only = [false, false, false, true, true, true, false, false];
            let fit = |cells: &[([f32; 3], [f32; 3])], from: [f32; 8], free: [bool; 8]| {
                fitted(cells, from, free, pivot, params.clamp_stops)
            };
            // Ground that changed is put aside, not weighed: the cells
            // whose step under the fit so far is far past what the others
            // show are not the joint, they are another picture.
            let kept_of = |under: &[f32; 8], floor: f32| -> Vec<([f32; 3], [f32; 3])> {
                let grade = grade_of(under, pivot, params.clamp_stops);
                let mut at = Vec::new();
                steps(&grade, &cells, &mut at);
                let worst: Vec<f32> = at
                    .chunks_exact(3)
                    .map(|c| c.iter().fold(0.0f32, |m, r| m.max(r.abs())))
                    .collect();
                let mut sorted = worst.clone();
                let bound = (3.0 * quantile(&mut sorted, 0.5)).max(floor);
                cells
                    .iter()
                    .zip(&worst)
                    .filter(|(_, w)| **w <= bound)
                    .map(|(c, _)| *c)
                    .collect()
            };
            let rough = fit(&cells, [0.0; 8], gain_only);
            let same = kept_of(&rough, CHANGED);
            let alone = fit(&same, rough, gain_only);
            let first = fit(&same, alone, [true; 8]);
            let same = kept_of(&first, 0.5 * CHANGED);
            let whole = fit(&same, first, [true; 8]);
            let mut grade = grade_of(&whole, pivot, params.clamp_stops);
            grade.black = grade.black.map(kept);
            grade.gain = grade.gain.map(|g| kept(g.log2()).exp2());
            grade.contrast = kept(grade.contrast);
            grade.saturation = kept(grade.saturation);

            let mut at = Vec::new();
            let mut left = |grade: &Grade| {
                steps(grade, &cells, &mut at);
                shown(&at)
            };
            reports.push(GradeReport {
                source,
                levels: tones
                    .sources
                    .iter()
                    .filter(|(_, s)| **s == source)
                    .map(|(l, _)| *l)
                    .collect(),
                observations,
                cells: cells.len(),
                before: left(&Grade::IDENTITY),
                gain_alone: left(&grade_of(&alone, pivot, params.clamp_stops)),
                after: left(&grade),
            });
            of_source.insert(source, grade);
        }

        let grades = tones
            .sources
            .iter()
            .map(|(level, source)| (*level, of_source.get(source).copied().unwrap_or_default()))
            .collect();
        Self {
            anchor,
            grades,
            sources: reports,
        }
    }

    /// The grades as a reader of the store finds them: a small JSON object.
    /// Gains are in stops, the rest as it is applied.
    pub fn to_json(&self) -> String {
        let three = |v: [f32; 3]| format!("[{:.4},{:.4},{:.4}]", v[0], v[1], v[2]);
        let levels: Vec<String> = self
            .grades
            .iter()
            .map(|(level, g)| {
                format!(
                    "\"{level}\":{{\"black\":{},\"gain\":{},\"contrast\":{:.4},\"pivot\":{:.4},\"saturation\":{:.4}}}",
                    three(g.black),
                    three(g.gain.map(f32::log2)),
                    g.contrast,
                    g.pivot,
                    g.saturation
                )
            })
            .collect();
        format!(
            "{{\"version\":2,\"anchor\":{},\"gain_unit\":\"stops\",\"levels\":{{{}}}}}",
            self.anchor,
            levels.join(",")
        )
    }

    /// Reads [`Self::to_json`] back — or the table of gains alone that came
    /// before it, read as grades that are gains alone. `None` if this is
    /// neither.
    pub fn from_json(text: &str) -> Option<Self> {
        if !text.contains("\"version\":2") {
            let gains = LevelGains::from_json(text)?;
            return Some(Self {
                anchor: gains.anchor,
                grades: gains
                    .gains
                    .iter()
                    .map(|(level, stops)| {
                        let gain = stops.map(|s| if s == 0.0 { 1.0 } else { s.exp2() });
                        (
                            *level,
                            Grade {
                                gain,
                                ..Grade::IDENTITY
                            },
                        )
                    })
                    .collect(),
                sources: Vec::new(),
            });
        }
        let number = |text: &str, key: &str| -> Option<f32> {
            text.split(&format!("\"{key}\":"))
                .nth(1)?
                .split([',', '}'])
                .next()?
                .trim()
                .parse()
                .ok()
        };
        let three = |text: &str, key: &str| -> Option<[f32; 3]> {
            let inside = text
                .split(&format!("\"{key}\":["))
                .nth(1)?
                .split(']')
                .next()?;
            let v: Vec<f32> = inside
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            (v.len() == 3).then(|| [v[0], v[1], v[2]])
        };
        let anchor = number(text, "anchor")? as u8;
        let body = text.split("\"levels\":{").nth(1)?;
        let mut grades = BTreeMap::new();
        // One level ends where its saturation does.
        for entry in body.split_inclusive('}') {
            let Some((level, rest)) = entry.split_once(":{") else {
                continue;
            };
            let level: u8 = level
                .trim_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()?;
            grades.insert(
                level,
                Grade {
                    black: three(rest, "black")?,
                    gain: three(rest, "gain")?.map(|s| if s == 0.0 { 1.0 } else { s.exp2() }),
                    contrast: number(rest, "contrast")?,
                    pivot: number(rest, "pivot")?,
                    saturation: number(rest, "saturation")?,
                },
            );
        }
        Some(Self {
            anchor,
            grades,
            sources: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A patch of ground: cells of varied tone and colour, the same every
    /// time for the same seed.
    fn ground(seed: u32, cells: usize) -> Vec<[f32; 3]> {
        let mut state = seed | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        (0..cells)
            .map(|_| {
                let y = 0.03 + 0.4 * next();
                [
                    y * (0.7 + 0.6 * next()),
                    y * (0.8 + 0.5 * next()),
                    y * (0.5 + 0.7 * next()),
                ]
            })
            .collect()
    }

    /// What `grade` undoes: the picture a source would have made of `truth`
    /// if `grade` is what brings it back.
    fn spoiled(truth: &[[f32; 3]], grade: &Grade) -> Vec<[f32; 3]> {
        truth
            .iter()
            .map(|c| {
                let y = luma(*c);
                let c = c.map(|v| y + (v - y) / grade.saturation);
                let y = luma(c).max(1e-5);
                // Contrast undone: the luminance whose power about the
                // pivot is this one.
                let was = grade.pivot * (y / grade.pivot).powf(1.0 / grade.contrast);
                let c = c.map(|v| v * was / y);
                [0, 1, 2].map(|i| c[i] / grade.gain[i] + grade.black[i])
            })
            .collect()
    }

    #[test]
    fn a_grade_is_recovered_whole_and_levels_of_one_source_share_it() {
        // 10 to 12 are the anchor's source. 13 and 14 are another: veiled,
        // darker, flatter and duller, all at once.
        let truth_of_13 = Grade {
            black: [0.012, 0.010, 0.016],
            gain: [2.6, 2.4, 1.9],
            contrast: 1.25,
            pivot: 0.15,
            saturation: 1.3,
        };
        let mut seen = Vec::new();
        for tile in 0..24u32 {
            let truth = ground(100 + tile, 256);
            seen.push(Seen {
                level: 11,
                ancestor: 10,
                tile: truth.clone(),
                under: truth.clone(),
            });
            seen.push(Seen {
                level: 12,
                ancestor: 11,
                tile: truth.clone(),
                under: truth.clone(),
            });
            let other = spoiled(&truth, &truth_of_13);
            seen.push(Seen {
                level: 13,
                ancestor: 12,
                tile: other.clone(),
                under: truth.clone(),
            });
            seen.push(Seen {
                level: 14,
                ancestor: 13,
                tile: other.clone(),
                under: other,
            });
        }
        let solved = LevelGrades::solve(
            &seen,
            &LevelParams {
                clamp_stops: 3.0,
                ..Default::default()
            },
        );
        assert_eq!(solved.anchor, 10);
        for level in [10, 11, 12] {
            assert!(
                solved.of(level).is_identity(),
                "level {level}: {:?}",
                solved.of(level)
            );
        }
        let found = solved.of(13);
        assert_eq!(found, solved.of(14));
        // Gain and pivot are two ways of saying one thing, so the numbers
        // that must come back as they were are the others.
        for i in 0..3 {
            assert!(
                (found.black[i] - truth_of_13.black[i]).abs() < 0.002,
                "black {found:?}"
            );
        }
        assert!(
            (found.contrast - truth_of_13.contrast).abs() < 0.02,
            "contrast {found:?}"
        );
        assert!(
            (found.saturation - truth_of_13.saturation).abs() < 0.02,
            "saturation {found:?}"
        );

        // And graded, the other source is the anchor's over the same ground.
        let truth = ground(7, 256);
        let back: Vec<[f32; 3]> = spoiled(&truth, &truth_of_13)
            .iter()
            .map(|c| found.apply(*c))
            .collect();
        let off = truth
            .iter()
            .zip(&back)
            .map(|(a, b)| (luma(*b).max(1e-5) / luma(*a).max(1e-5)).log2().abs())
            .fold(0.0, f32::max);
        assert!(off < 0.03, "worst cell {off} stops from the truth");
    }

    #[test]
    fn ground_that_changed_does_not_pull_the_grade() {
        let truth_of_13 = Grade {
            black: [0.008, 0.008, 0.012],
            gain: [2.2, 2.2, 1.7],
            contrast: 1.15,
            pivot: 0.15,
            saturation: 1.2,
        };
        let mut seen = Vec::new();
        for tile in 0..30u32 {
            let truth = ground(500 + tile, 64);
            let mut other = spoiled(&truth, &truth_of_13);
            // A fifth of the ground is not what it was: snow, cloud, crops.
            for (n, cell) in other.iter_mut().enumerate() {
                if n % 5 == 0 {
                    *cell = [0.5, 0.5, 0.55];
                }
            }
            seen.push(Seen {
                level: 13,
                ancestor: 12,
                tile: other,
                under: truth,
            });
        }
        let solved = LevelGrades::solve(
            &seen,
            &LevelParams {
                anchor: 12,
                clamp_stops: 3.0,
                ..Default::default()
            },
        );
        let found = solved.of(13);
        assert!(
            (found.contrast - truth_of_13.contrast).abs() < 0.1,
            "{found:?}"
        );
        assert!(
            (found.saturation - truth_of_13.saturation).abs() < 0.12,
            "{found:?}"
        );
        // Over ground that did not change, graded is the anchor's.
        let truth = ground(9, 256);
        let off = spoiled(&truth, &truth_of_13)
            .iter()
            .zip(&truth)
            .map(|(c, t)| {
                (luma(found.apply(*c)).max(1e-5) / luma(*t).max(1e-5))
                    .log2()
                    .abs()
            })
            .fold(0.0, f32::max);
        assert!(off < 0.15, "worst cell {off} stops from the truth");
    }

    #[test]
    fn a_gain_alone_cannot_do_it_which_is_why_there_is_a_grade() {
        // The same spoiling, corrected by its gain alone: the mean comes
        // back and the spread does not.
        let grade = Grade {
            black: [0.012, 0.010, 0.016],
            gain: [2.6, 2.4, 1.9],
            contrast: 1.25,
            pivot: 0.15,
            saturation: 1.3,
        };
        let truth = ground(3, 512);
        let other = spoiled(&truth, &grade);
        let spread = |cells: &[[f32; 3]]| {
            let mut y: Vec<f32> = cells.iter().map(|c| luma(*c).log2()).collect();
            quantile(&mut y, 0.9) - quantile(&mut y, 0.1)
        };
        let gained: Vec<[f32; 3]> = other
            .iter()
            .map(|c| [0, 1, 2].map(|i| c[i] * grade.gain[i]))
            .collect();
        let graded: Vec<[f32; 3]> = other.iter().map(|c| grade.apply(*c)).collect();
        assert!(spread(&gained) < spread(&truth) * 0.85);
        assert!((spread(&graded) - spread(&truth)).abs() < 0.02);
    }

    #[test]
    fn the_identity_touches_nothing_and_a_grade_is_its_json() {
        let mut texels = [10u8, 120, 250, 77, 0, 255, 33, 9];
        let before = texels;
        Grade::IDENTITY.apply_rgba8(&mut texels);
        assert_eq!(texels, before);

        let grade = Grade {
            black: [0.01, -0.02, 0.0],
            gain: [2.0, 1.0, 0.5],
            contrast: 1.2,
            pivot: 0.2,
            saturation: 0.8,
        };
        grade.apply_rgba8(&mut texels);
        assert_ne!(texels[..3], before[..3]);
        assert_eq!((texels[3], texels[7]), (77, 9), "alpha was touched");
        assert!(grade.at(0.0).is_identity());
        assert_eq!(grade.at(1.0), grade);

        let grades = LevelGrades {
            anchor: 10,
            grades: BTreeMap::from([(10, Grade::IDENTITY), (13, grade)]),
            sources: Vec::new(),
        };
        let read = LevelGrades::from_json(&grades.to_json()).expect("read back");
        assert_eq!(read.anchor, 10);
        assert!(read.of(10).is_identity());
        let back = read.of(13);
        for i in 0..3 {
            assert!((back.gain[i] - grade.gain[i]).abs() < 1e-3);
            assert!((back.black[i] - grade.black[i]).abs() < 1e-4);
        }
        assert!((back.contrast - 1.2).abs() < 1e-4 && (back.saturation - 0.8).abs() < 1e-4);
        // The table of gains that came before reads as gains alone.
        let old = "{\"version\":1,\"anchor\":10,\"unit\":\"stops\",\"levels\":{\"10\":[0.0000,0.0000,0.0000],\"13\":[1.0000,0.0000,-1.0000]}}";
        let read = LevelGrades::from_json(old).expect("the old table");
        assert!(read.of(10).is_identity());
        assert_eq!(read.of(13).gain, [2.0, 1.0, 0.5]);
        assert_eq!(read.of(13).contrast, 1.0);
        assert_eq!(LevelGrades::from_json("{}"), None);
    }
}
