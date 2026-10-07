// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's own grade, against one target for every film.
//!
//! Two things are set, and they are not the same thing:
//!
//! - **the target** ([`LookTarget`]) is absolute and everyone's: how light,
//!   how contrasted and how coloured a picture of ground from the air is.
//!   It is not fitted; it is what every film is brought to.
//! - **a film's grade** ([`FilmGrade`]) is that film's, and relative. It
//!   has two parts:
//!   - *its tiles brought to one another* — a continuous field carried by
//!     the corners of its imagery tiles ([`CornerField`]): two neighbours
//!     are given the same correction along the edge they share, so what
//!     met still meets, and the field brings every tile to the film's own
//!     tone. This part makes tiles agree;
//!   - *the film brought to the target* — one exposure, one contrast, one
//!     saturation for every picture of it. No colour: one gain for the
//!     three channels, so a grey stays grey.
//!
//! Every number of a film's grade has a floor and a ceiling ([`Bounds`],
//! [`FieldBounds`]). A fit that wants more than it may have is given the
//! bound and says so: a film that cannot be brought to the target within
//! the bounds is left short of it, never pushed past them.
//!
//! Nothing here is borrowed: a film's grade is fitted on that film's
//! imagery alone. A film without one is drawn with the target's own
//! settings and nothing of another film's.
//!
//! The film is brought to the target without being drawn. What a picture
//! will measure is foretold from the imagery it is made of — every tile,
//! counted for the tiles of the film it is draped on — and from how much
//! light the renderer puts on ground (`light`: a picture's linear value for
//! an imagery value of one, lighting and exposure together).

use std::collections::BTreeMap;

use crate::corners::{CornerField, CornerReport, FieldBounds};
use crate::measure::Measure;
use crate::tiles::Observed;

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn luma(c: [f32; 3]) -> f32 {
    LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2]
}

/// Kept to a 4096th: the same numbers wherever this ran.
fn kept(value: f32) -> f32 {
    (value * 4096.0).round() / 4096.0
}

/// What every film is brought to, in CIELAB over its pictures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookTarget {
    /// Mean L*.
    pub lightness: f32,
    /// L* at the 95th centile less L* at the 5th.
    pub contrast: f32,
    /// Mean C*.
    pub chroma: f32,
    /// How far from each a film may be and be left alone: lightness,
    /// contrast, chroma.
    pub within: [f32; 3],
}

impl Default for LookTarget {
    /// The middle of 216 photographs of ground taken from the air: L* 47
    /// (38 to 53 from quartile to quartile of what was measured), contrast
    /// 58 (42 to 67), C* 21 (14 to 27).
    fn default() -> Self {
        Self {
            lightness: 47.0,
            contrast: 58.0,
            chroma: 21.0,
            within: [3.0, 6.0, 3.0],
        }
    }
}

/// What the film's part of a grade may not go past. A tile's has its own
/// ([`FieldBounds`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// The film against the target, in stops either way.
    pub film_stops: f32,
    /// The film's contrast and saturation, least and most.
    pub film_contrast: (f32, f32),
    pub film_saturation: (f32, f32),
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            film_stops: 2.0,
            film_contrast: (0.8, 1.3),
            film_saturation: (0.8, 1.3),
        }
    }
}

/// What pictures measure: mean L*, contrast, mean C*.
pub type Pictured = [f32; 3];

/// A film's grade: see the module.
#[derive(Debug, Clone, PartialEq)]
pub struct FilmGrade {
    /// The film's tiles brought to one another.
    pub field: CornerField,
    /// The film brought to the target: stops added to the renderer's own
    /// exposure, then its contrast and saturation multiplied by these.
    pub exposure_ev: f32,
    pub contrast: f32,
    pub saturation: f32,
    /// The light the renderer was taken to put on ground.
    pub light: f32,
    /// What the film's pictures were foretold to measure with nothing
    /// done, and with this grade.
    pub before: Pictured,
    pub after: Pictured,
    /// What the field was found from, and what it leaves.
    pub fitted: CornerReport,
    /// What the film's part wanted more of than it may have.
    pub limited: Vec<String>,
}

/// CIELAB of a linear sRGB colour, D65.
fn lab(c: [f32; 3]) -> [f32; 3] {
    let f = |t: f32| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let x = f((0.4124 * c[0] + 0.3576 * c[1] + 0.1805 * c[2]) / 0.9505);
    let y = f(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]);
    let z = f((0.0193 * c[0] + 0.1192 * c[1] + 0.9505 * c[2]) / 1.089);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

/// Where the roll-off of highlights begins: the renderer's own.
const KNEE: f32 = 0.5;

/// What the renderer's last step makes of a lit colour: contrast about
/// middle grey, saturation, highlights rolled off, then cut to what a
/// display shows. The same arithmetic as the renderer's, so that what is
/// foretold here is what it draws.
fn shown(lit: [f32; 3], contrast: f32, saturation: f32) -> [f32; 3] {
    let mut c = lit;
    if contrast != 1.0 {
        let by = (luma(c).max(1e-5) / 0.18).powf(contrast - 1.0);
        c = c.map(|v| v * by);
    }
    if saturation != 1.0 {
        let y = luma(c);
        c = c.map(|v| (y + (v - y) * saturation).max(0.0));
    }
    let y = luma(c);
    if y > KNEE {
        let bent = KNEE + (1.0 - KNEE) * ((y - KNEE) / (1.0 - KNEE)).tanh();
        c = c.map(|v| v * bent / y);
    }
    c.map(|v| v.clamp(0.0, 1.0))
}

/// Samples a measure is taken over at most: more says nothing more.
const MEASURED: usize = 40_000;

/// What pictures made of these lit colours measure.
fn pictured(lit: &[([f32; 3], f32)], stops: f32, contrast: f32, saturation: f32) -> Pictured {
    let gain = stops.exp2();
    let mut lightness: Vec<(f32, f32)> = Vec::with_capacity(lit.len());
    let (mut sum_l, mut sum_c, mut sum_w) = (0.0f64, 0.0f64, 0.0f64);
    for (colour, weight) in lit {
        let [l, a, b] = lab(shown(colour.map(|v| v * gain), contrast, saturation));
        lightness.push((l, *weight));
        sum_l += f64::from(l * weight);
        sum_c += f64::from(a.hypot(b) * weight);
        sum_w += f64::from(*weight);
    }
    if sum_w <= 0.0 {
        return [0.0; 3];
    }
    lightness.sort_by(|a, b| a.0.total_cmp(&b.0));
    let at = |q: f64| {
        let (mut seen, wanted) = (0.0f64, sum_w * q);
        for (l, w) in &lightness {
            seen += f64::from(*w);
            if seen >= wanted {
                return *l;
            }
        }
        lightness.last().map_or(0.0, |l| l.0)
    };
    [
        (sum_l / sum_w) as f32,
        at(0.95) - at(0.05),
        (sum_c / sum_w) as f32,
    ]
}

/// The value between `low` and `high` at which `of`, which rises with it,
/// is `wanted` — or the end it comes nearest at.
fn where_it_is(low: f32, high: f32, wanted: f32, of: impl Fn(f32) -> f32) -> f32 {
    if of(low) >= wanted {
        return low;
    }
    if of(high) <= wanted {
        return high;
    }
    let (mut low, mut high) = (low, high);
    for _ in 0..24 {
        let middle = 0.5 * (low + high);
        if of(middle) < wanted {
            low = middle;
        } else {
            high = middle;
        }
    }
    0.5 * (low + high)
}

impl FilmGrade {
    /// The grade of a film that has none: nothing is done to its imagery,
    /// and it is drawn with the renderer's own settings.
    pub fn none() -> Self {
        Self {
            field: CornerField::default(),
            exposure_ev: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            light: 1.0,
            before: [0.0; 3],
            after: [0.0; 3],
            fitted: CornerReport::default(),
            limited: Vec::new(),
        }
    }

    /// Fits a film's grade on its imagery as it was seen, its tiles
    /// measured as `measure` says. `light` is a picture's linear value for
    /// an imagery value of one.
    pub fn fit(
        observed: &Observed,
        measure: Measure,
        light: f32,
        target: &LookTarget,
        bounds: &Bounds,
        field_bounds: &FieldBounds,
    ) -> Self {
        if observed.tiles.is_empty() {
            return Self {
                field: CornerField {
                    measure,
                    ..CornerField::default()
                },
                ..Self::none()
            };
        }
        let mut limited = Vec::new();

        // The tiles brought to one another.
        let (field, fitted) = CornerField::solve_by(observed, measure, field_bounds);

        // The film as it will be lit: every cell of every tile, counted
        // for the tiles of the film it is draped on — a part of it if it
        // is large. A cell is given what the field gives its middle.
        let grid = crate::tiles::GRID;
        let cells: usize = observed.tiles.values().map(|t| t.cells.len()).sum();
        let every = cells.div_ceil(MEASURED).max(1);
        let pieces = |graded: bool| -> Vec<([f32; 3], f32)> {
            observed
                .tiles
                .iter()
                .flat_map(|(at, tile)| {
                    let corners = if graded { field.corners(*at) } else { None };
                    let weight = tile.usage / tile.cells.len() as f32;
                    tile.cells.iter().enumerate().map(move |(k, c)| {
                        let lit = match &corners {
                            Some(corners) => crate::measure::Blended::mix(
                                corners,
                                ((k % grid) as f32 + 0.5) / grid as f32,
                                ((k / grid) as f32 + 0.5) / grid as f32,
                            )
                            .local()
                            .apply(*c),
                            None => *c,
                        };
                        (lit.map(|v| v * light), weight)
                    })
                })
                .step_by(every)
                .filter(|(_, weight)| *weight > 0.0)
                .collect()
        };
        let (lit, raw) = (pieces(true), pieces(false));
        let before = pictured(&raw, 0.0, 1.0, 1.0);

        // The film brought to the target, one thing at a time and each
        // only if it is further from the target than a film may be: light
        // first, since contrast and colour are read off a picture that is
        // rightly exposed; then light once more, since contrast moves it.
        let (mut stops, mut contrast, mut saturation) = (0.0f32, 1.0f32, 1.0f32);
        let far = |measured: f32, wanted: f32, within: f32| (measured - wanted).abs() > within;
        let expose = |contrast: f32, saturation: f32| {
            where_it_is(
                -bounds.film_stops,
                bounds.film_stops,
                target.lightness,
                |e| pictured(&lit, e, contrast, saturation)[0],
            )
        };
        let exposing = far(
            pictured(&lit, 0.0, 1.0, 1.0)[0],
            target.lightness,
            target.within[0],
        );
        if exposing {
            stops = expose(1.0, 1.0);
        }
        if far(
            pictured(&lit, stops, 1.0, 1.0)[1],
            target.contrast,
            target.within[1],
        ) {
            let (least, most) = bounds.film_contrast;
            contrast = where_it_is(least, most, target.contrast, |k| {
                pictured(&lit, stops, k, 1.0)[1]
            });
            if exposing {
                stops = expose(contrast, 1.0);
            }
        }
        if far(
            pictured(&lit, stops, contrast, 1.0)[2],
            target.chroma,
            target.within[2],
        ) {
            let (least, most) = bounds.film_saturation;
            saturation = where_it_is(least, most, target.chroma, |s| {
                pictured(&lit, stops, contrast, s)[2]
            });
        }
        // Kept to a 4096th, and still within the bounds after that.
        let within = |value: f32, (least, most): (f32, f32)| kept(value).clamp(least, most);
        let stops = within(stops, (-bounds.film_stops, bounds.film_stops));
        let contrast = within(contrast, bounds.film_contrast);
        let saturation = within(saturation, bounds.film_saturation);
        let after = pictured(&lit, stops, contrast, saturation);
        let names = ["lightness", "contrast", "chroma"];
        let wanted = [target.lightness, target.contrast, target.chroma];
        for i in 0..3 {
            if far(after[i], wanted[i], target.within[i]) {
                limited.push(format!(
                    "film: {} {:.1} for {:.1}",
                    names[i], after[i], wanted[i]
                ));
            }
        }
        Self {
            field,
            exposure_ev: stops,
            contrast,
            saturation,
            light,
            before,
            after,
            fitted,
            limited,
        }
    }

    /// The grade of a film made of several packs, from the grade and what
    /// was seen of each. One pack's grade is the film's. Several are not
    /// averaged: what they saw is put together and the film is fitted
    /// anew, so that it is one grade from its first frame to its last and
    /// a tile two packs drape is given one thing. `None` of nothing.
    pub fn of_packs(
        parts: &[(&FilmGrade, &Observed)],
        target: &LookTarget,
        bounds: &Bounds,
        field_bounds: &FieldBounds,
    ) -> Option<Self> {
        match parts {
            [] => None,
            [(only, _)] => Some((*only).clone()),
            several => {
                let observed = Observed::merged(several.iter().map(|(_, seen)| *seen));
                let light =
                    several.iter().map(|(g, _)| g.light).sum::<f32>() / several.len() as f32;
                Some(Self::fit(
                    &observed,
                    several[0].0.field.measure,
                    light,
                    target,
                    bounds,
                    field_bounds,
                ))
            }
        }
    }

    /// The grade as it is kept beside a pack: a small JSON object.
    ///
    /// The field is kept by its corners. A corner is shared by the tiles
    /// that meet at it, so it is written once — every number of the
    /// measure, then the pivot, stops in 64ths and a black point in
    /// 65536ths — and a tile names its four: top-left, top-right,
    /// bottom-left, bottom-right.
    pub fn to_json(&self) -> String {
        let three = |v: Pictured| format!("[{:.2},{:.2},{:.2}]", v[0], v[1], v[2]);
        let pair = |v: (f32, f32)| format!("[{:.3},{:.3}]", v.0, v.1);
        let measure = self.field.measure;
        let mut corners: Vec<String> = Vec::new();
        let mut known: BTreeMap<String, usize> = BTreeMap::new();
        let mut tiles: Vec<String> = Vec::new();
        for ((level, x, y), given) in &self.field.given {
            let pivot = self.field.pivot.get(&(*level, *x, *y));
            let named: Vec<String> = (0..4)
                .map(|corner| {
                    let mut numbers: Vec<String> = given[corner]
                        .iter()
                        .zip(measure.names())
                        .map(|(value, name)| {
                            let steps = if *name == "black" { 65536.0 } else { 64.0 };
                            ((value * steps).round() as i32).to_string()
                        })
                        .collect();
                    let pivot = pivot.map_or(-2.5, |p| p[corner]);
                    numbers.push(((pivot * 64.0).round() as i32).to_string());
                    let text = numbers.join(",");
                    let next = known.len();
                    let at = *known.entry(text.clone()).or_insert_with(|| {
                        corners.push(format!("[{text}]"));
                        next
                    });
                    at.to_string()
                })
                .collect();
            tiles.push(format!("\"{level}/{x}/{y}\":[{}]", named.join(",")));
        }
        let limited: Vec<String> = self
            .limited
            .iter()
            .map(|what| format!("\"{}\"", what.replace(['"', '\\', ']'], "")))
            .collect();
        let r = &self.fitted;
        format!(
            "{{\"film_grade\":3,\"measure\":\"{}\",\"film\":{{\"exposure_ev\":{},\"contrast\":{},\"saturation\":{},\"light\":{}}},\"before\":{},\"after\":{},\"limited\":[{}],\"fitted\":{{\"tiles\":{},\"measured\":{},\"edges\":{},\"seam_edges\":{},\"seams\":{},\"steps_made\":{},\"untouched\":{},\"held\":{},\"widest_break\":{},\"seam_before\":{},\"seam_after\":{},\"apart_before\":{},\"apart_after\":{}}},\"unit\":\"64ths of a stop\",\"corners\":[{}],\"tiles\":{{{}}}}}",
            measure.name(),
            self.exposure_ev,
            self.contrast,
            self.saturation,
            self.light,
            three(self.before),
            three(self.after),
            limited.join(","),
            r.tiles,
            r.measured,
            r.edges,
            r.seam_edges,
            r.seams,
            r.steps_made,
            r.untouched,
            r.held,
            r.widest_break,
            pair(r.seam_before),
            pair(r.seam_after),
            pair(r.apart_before),
            pair(r.apart_after),
            corners.join(","),
            tiles.join(",")
        )
    }

    /// Reads [`Self::to_json`] back. `None` if this is not one.
    pub fn from_json(text: &str) -> Option<Self> {
        if !text.contains("\"film_grade\":3") {
            return None;
        }
        let inside = |text: &'_ str, open: &str, close: char| -> Option<String> {
            Some(text.split(open).nth(1)?.split(close).next()?.to_string())
        };
        let number = |text: &str, key: &str| -> Option<f32> {
            text.split(&format!("\"{key}\":"))
                .nth(1)?
                .split([',', '}'])
                .next()?
                .trim()
                .parse()
                .ok()
        };
        let list = |text: &str, key: &str| -> Option<Vec<f32>> {
            Some(
                inside(text, &format!("\"{key}\":["), ']')?
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect(),
            )
        };
        let three = |key: &str| -> Option<Pictured> {
            let v = list(text, key)?;
            (v.len() == 3).then(|| [v[0], v[1], v[2]])
        };
        let film = inside(text, "\"film\":{", '}')?;
        let fitted = inside(text, "\"fitted\":{", '}')?;
        let count = |key: &str| number(&fitted, key).map(|v| v as usize);
        let pair = |key: &str| -> Option<(f32, f32)> {
            let v = list(&fitted, key)?;
            (v.len() == 2).then(|| (v[0], v[1]))
        };
        let limited = inside(text, "\"limited\":[", ']')?
            .split("\",\"")
            .map(|what| what.trim_matches('"').to_string())
            .filter(|what| !what.is_empty())
            .collect();
        let measure = match inside(text, "\"measure\":\"", '"')?.as_str() {
            "moments" => Measure::Moments,
            "curves" => Measure::Curves,
            _ => return None,
        };
        // The corners, each its numbers and then its pivot…
        let found = measure.len();
        let mut corners: Vec<(Vec<f32>, f32)> = Vec::new();
        let body = text.split("\"corners\":[").nth(1)?;
        let body = body.split("],\"tiles\":").next()?;
        for corner in body.split(']') {
            let numbers: Vec<f32> = corner
                .trim_start_matches([',', '['])
                .split(',')
                .filter_map(|n| n.trim().parse::<i32>().ok())
                .map(|n| n as f32)
                .collect();
            if numbers.is_empty() {
                continue;
            }
            if numbers.len() != found + 1 {
                return None;
            }
            let given = numbers[..found]
                .iter()
                .zip(measure.names())
                .map(|(v, name)| v / if *name == "black" { 65536.0 } else { 64.0 })
                .collect();
            corners.push((given, numbers[found] / 64.0));
        }
        // …and the tiles, each naming its four.
        let (mut given, mut pivot) = (BTreeMap::new(), BTreeMap::new());
        let body = text.split("\"tiles\":{").nth(1)?;
        for entry in body.split(']') {
            let Some((at, named)) = entry.split_once(":[") else {
                continue;
            };
            let mut at = at
                .trim_matches(|c: char| !c.is_ascii_digit())
                .split('/')
                .map(str::parse::<u32>);
            let (level, x, y) = (at.next()?.ok()?, at.next()?.ok()?, at.next()?.ok()?);
            let at = (u8::try_from(level).ok()?, x, y);
            let named: Vec<usize> = named
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            if named.len() != 4 || named.iter().any(|n| *n >= corners.len()) {
                return None;
            }
            let of: [Vec<f32>; 4] = std::array::from_fn(|c| corners[named[c]].0.clone());
            given.insert(at, of);
            pivot.insert(at, std::array::from_fn(|c| corners[named[c]].1));
        }
        Some(Self {
            field: CornerField {
                measure,
                given,
                pivot,
            },
            exposure_ev: number(&film, "exposure_ev")?,
            contrast: number(&film, "contrast")?,
            saturation: number(&film, "saturation")?,
            light: number(&film, "light")?,
            before: three("before")?,
            after: three("after")?,
            fitted: CornerReport {
                tiles: count("tiles")?,
                measured: count("measured")?,
                edges: count("edges")?,
                seam_edges: count("seam_edges")?,
                seams: count("seams")?,
                steps_made: count("steps_made")?,
                untouched: count("untouched")?,
                held: count("held")?,
                widest_break: number(&fitted, "widest_break")?,
                seam_before: pair("seam_before")?,
                seam_after: pair("seam_after")?,
                apart_before: pair("apart_before")?,
                apart_after: pair("apart_after")?,
            },
            limited,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{TileSeen, GRID};
    use crate::{FieldBounds, Measure};

    /// A film of `wide` × 4 tiles of level 13 about `tone`, of varied tone
    /// and colour, whose first `dark` columns are a capture `stops` darker
    /// — over a reference level that shows the same ground as one picture.
    fn film(tone: f32, wide: u32, dark: u32, stops: f32) -> Observed {
        let mut observed = Observed::default();
        // Ground that goes on from one tile to the next, as ground does:
        // light and colour that vary smoothly with where a cell is, not
        // cell by cell.
        let ground = |x: u32, y: u32, k: usize| {
            let (gx, gy) = (
                (x as usize * GRID + k % GRID) as f32,
                (y as usize * GRID + k / GRID) as f32,
            );
            let light = tone * (1.0 + 0.3 * (gx * 0.37).sin() * (gy * 0.29).cos());
            [
                light * (1.0 + 0.15 * (gx * 0.11).sin()),
                light * (1.1 + 0.15 * (gy * 0.13).cos()),
                light * (0.8 + 0.15 * (gx * 0.07 + gy * 0.05).sin()),
            ]
        };
        for y in 0..4u32 {
            for x in 0..wide {
                let by = if x < dark { (-stops).exp2() } else { 1.0 };
                let cells: [[f32; 3]; GRID * GRID] =
                    std::array::from_fn(|k| ground(x, y, k).map(|v| v * by));
                observed.tiles.insert(
                    (13, 50 + x, 70 + y),
                    TileSeen {
                        cells,
                        edges: [[[tone * by; 3]; GRID]; 4],
                        usage: 1.0,
                        tones: None,
                    },
                );
                // The reference under it: the same ground, a level up, as
                // it is — each of its cells four of the tile's.
                let under = observed
                    .tiles
                    .entry((12, (50 + x) / 2, (70 + y) / 2))
                    .or_insert(TileSeen {
                        cells: [[0.0; 3]; GRID * GRID],
                        edges: [[[tone; 3]; GRID]; 4],
                        usage: 0.0,
                        tones: None,
                    });
                let (i0, j0) = (((50 + x) % 2) as usize * 4, ((70 + y) % 2) as usize * 4);
                for cj in 0..4 {
                    for ci in 0..4 {
                        let mut sum = [0.0f32; 3];
                        for (dj, di) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            let c = ground(x, y, (cj * 2 + dj) * GRID + ci * 2 + di);
                            for i in 0..3 {
                                sum[i] += c[i] / 4.0;
                            }
                        }
                        under.cells[(j0 + cj) * GRID + i0 + ci] = sum;
                    }
                }
            }
        }
        observed
    }

    fn fit(observed: &Observed, target: &LookTarget) -> FilmGrade {
        FilmGrade::fit(
            observed,
            Measure::Moments,
            1.0,
            target,
            &Bounds::default(),
            &FieldBounds::default(),
        )
    }

    /// The gain on light the field gives the middle of a tile, in stops.
    fn lift(grade: &FilmGrade, at: (u8, u32, u32)) -> f32 {
        grade.field.at(at, 0.5, 0.5).gain[1].log2()
    }

    #[test]
    fn a_films_tiles_are_brought_together_before_the_film_is_measured() {
        // A dark film, a third of it a capture a stop darker still.
        let grade = fit(&film(0.05, 12, 4, 1.0), &LookTarget::default());
        assert_eq!(grade.fitted.seams, 1, "{:?}", grade.fitted);
        // The larger capture is the film's own; the other is brought up.
        assert!(lift(&grade, (13, 59, 71)).abs() < 0.02);
        assert!((lift(&grade, (13, 51, 71)) - 1.0).abs() < 0.06);
        // The film is then exposed as the tiles will be drawn, not as the
        // imagery lies: as much as the same film of one capture, and less
        // than its darker third would have asked for.
        let one_capture = fit(&film(0.05, 12, 0, 0.0), &LookTarget::default());
        assert!(one_capture.exposure_ev > 0.5);
        assert!(
            (grade.exposure_ev - one_capture.exposure_ev).abs() < 0.06,
            "{} against {}",
            grade.exposure_ev,
            one_capture.exposure_ev
        );
    }

    #[test]
    fn the_films_part_stays_within_its_bounds_and_says_when_it_is_short() {
        // Far too dark to be brought to the target in two stops.
        let bounds = Bounds::default();
        let grade = fit(&film(0.004, 4, 0, 0.0), &LookTarget::default());
        assert_eq!(grade.exposure_ev, bounds.film_stops);
        assert!(
            grade.contrast >= bounds.film_contrast.0 && grade.contrast <= bounds.film_contrast.1
        );
        assert!(
            grade.saturation >= bounds.film_saturation.0
                && grade.saturation <= bounds.film_saturation.1
        );
        assert!(
            grade
                .limited
                .iter()
                .any(|w| w.starts_with("film: lightness")),
            "{:?}",
            grade.limited
        );
    }

    #[test]
    fn a_dark_film_is_lifted_to_the_target_with_no_colour() {
        let target = LookTarget::default();
        let grade = fit(&film(0.05, 4, 0, 0.0), &target);
        assert!(
            grade.before[0] < target.lightness - 10.0,
            "{:?}",
            grade.before
        );
        assert!(grade.exposure_ev > 0.5, "{grade:?}");
        assert!(
            (grade.after[0] - target.lightness).abs() <= target.within[0],
            "{:?}",
            grade.after
        );
        // One capture, nothing to bring together: the field gives no tile
        // a gain to speak of, and the film's part is three numbers with no
        // colour in them.
        assert!(lift(&grade, (13, 51, 71)).abs() < 0.05);
    }

    #[test]
    fn a_film_already_at_the_target_is_not_touched() {
        let observed = film(0.18, 4, 0, 0.0);
        let mut target = LookTarget::default();
        let as_it_is = fit(&observed, &target);
        target.lightness = as_it_is.before[0];
        target.contrast = as_it_is.before[1];
        target.chroma = as_it_is.before[2];
        let grade = fit(&observed, &target);
        assert_eq!(
            (grade.exposure_ev, grade.contrast, grade.saturation),
            (0.0, 1.0, 1.0)
        );
        assert!(grade.limited.is_empty(), "{:?}", grade.limited);
        // A little off is still left alone; far off is not.
        target.lightness += 0.9 * target.within[0];
        assert_eq!(fit(&observed, &target).exposure_ev, 0.0);
        target.lightness += 3.0 * target.within[0];
        assert!(fit(&observed, &target).exposure_ev > 0.0);
    }

    #[test]
    fn a_film_of_nothing_has_no_grade_and_borrows_none() {
        let grade = fit(&Observed::default(), &LookTarget::default());
        assert_eq!(grade, FilmGrade::none());
        assert!(grade.field.at((13, 1, 1), 0.5, 0.5).is_identity());
        let (target, bounds, tiles) = (
            LookTarget::default(),
            Bounds::default(),
            FieldBounds::default(),
        );
        assert_eq!(FilmGrade::of_packs(&[], &target, &bounds, &tiles), None);
    }

    #[test]
    fn a_film_of_several_packs_is_fitted_as_one() {
        let whole = film(0.05, 12, 4, 1.0);
        let part = |from: u32, to: u32| Observed {
            tiles: whole
                .tiles
                .iter()
                .filter(|(at, _)| at.0 == 12 || (50 + from..50 + to).contains(&at.1))
                .map(|(at, tile)| (*at, tile.clone()))
                .collect(),
        };
        // Cut where the capture changes: each pack alone is one capture.
        let (west, east) = (part(0, 4), part(4, 12));
        let (target, bounds, field) = (
            LookTarget::default(),
            Bounds::default(),
            FieldBounds::default(),
        );
        let (g_west, g_east) = (fit(&west, &target), fit(&east, &target));
        assert!(lift(&g_west, (13, 51, 71)).abs() < 0.05);
        let film = FilmGrade::of_packs(
            &[(&g_west, &west), (&g_east, &east)],
            &target,
            &bounds,
            &field,
        )
        .expect("a grade");
        // Together they are two captures, and the film is what the whole
        // of it fitted at once is.
        assert_eq!(film, fit(&whole, &target));
        assert!((lift(&film, (13, 51, 71)) - 1.0).abs() < 0.06);
        // One pack alone is its own grade, exactly.
        assert_eq!(
            FilmGrade::of_packs(&[(&g_west, &west)], &target, &bounds, &field).as_ref(),
            Some(&g_west)
        );
    }

    #[test]
    fn a_grade_is_read_back_as_it_was_written() {
        for measure in [Measure::Moments, Measure::Curves] {
            let grade = FilmGrade::fit(
                &film(0.004, 12, 4, 0.75),
                measure,
                1.0,
                &LookTarget::default(),
                &Bounds::default(),
                &FieldBounds::default(),
            );
            assert!(!grade.limited.is_empty() && !grade.field.given.is_empty());
            let text = grade.to_json();
            let read = FilmGrade::from_json(&text).expect("read back");
            // The field, corner for corner and to the bit: what is kept is
            // what was found.
            assert_eq!(read.field, grade.field, "{measure:?}");
            assert_eq!(read.limited, grade.limited);
            assert_eq!(
                (read.exposure_ev, read.contrast, read.saturation, read.light),
                (
                    grade.exposure_ev,
                    grade.contrast,
                    grade.saturation,
                    grade.light
                )
            );
            let close = |a: f32, b: f32| (a - b).abs() < 0.01;
            for i in 0..3 {
                assert!(close(read.before[i], grade.before[i]));
                assert!(close(read.after[i], grade.after[i]));
            }
            assert_eq!(read.fitted.seams, grade.fitted.seams);
            assert_eq!(read.fitted.seam_edges, grade.fitted.seam_edges);
            assert_eq!(read.fitted.steps_made, 0);
            assert!(close(
                read.fitted.apart_before.1,
                grade.fitted.apart_before.1
            ));
            // A corner is written once, however many tiles meet at it.
            let corners = text.split("\"corners\":[").nth(1).expect("corners");
            let written = corners
                .split("],\"tiles\"")
                .next()
                .expect("corners")
                .matches('[')
                .count();
            assert!(written < 2 * grade.field.given.len(), "{written} corners");
        }
        assert_eq!(FilmGrade::from_json("{}"), None);
    }
}
