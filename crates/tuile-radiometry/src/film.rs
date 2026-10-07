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
//!   - *its tiles brought to one another* — a gain a tile of imagery
//!     ([`TileGains`]): tiles that already meet are kept exactly as they
//!     are to one another, what is most of the film is not touched, and
//!     the rest is brought to it. This part makes tiles agree;
//!   - *the film brought to the target* — one exposure, one contrast, one
//!     saturation for every picture of it. No colour: one gain for the
//!     three channels, so a grey stays grey.
//!
//! Every number of a film's grade has a floor and a ceiling ([`Bounds`],
//! [`TileBounds`]). A fit that wants more than it may have is given the
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

use crate::tiles::{Observed, TileBounds, TileGains, TileReport};

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
/// ([`TileBounds`]).
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
    pub tiles: TileGains,
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
    /// What the tiles' gains were found from, and what they leave.
    pub fitted: TileReport,
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
fn measure(lit: &[([f32; 3], f32)], stops: f32, contrast: f32, saturation: f32) -> Pictured {
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
            tiles: TileGains::default(),
            exposure_ev: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            light: 1.0,
            before: [0.0; 3],
            after: [0.0; 3],
            fitted: TileReport::default(),
            limited: Vec::new(),
        }
    }

    /// Fits a film's grade on its imagery as it was seen. `light` is a
    /// picture's linear value for an imagery value of one.
    pub fn fit(
        observed: &Observed,
        light: f32,
        target: &LookTarget,
        bounds: &Bounds,
        tile_bounds: &TileBounds,
    ) -> Self {
        if observed.tiles.is_empty() {
            return Self::none();
        }
        let mut limited = Vec::new();

        // The tiles brought to one another.
        let (tiles, fitted) = TileGains::solve(observed, tile_bounds);

        // The film as it will be lit: every cell of every tile, counted
        // for the tiles of the film it is draped on — a part of it if it
        // is large.
        let cells: usize = observed.tiles.values().map(|t| t.cells.len()).sum();
        let every = cells.div_ceil(MEASURED).max(1);
        let pieces = |graded: bool| -> Vec<([f32; 3], f32)> {
            observed
                .tiles
                .iter()
                .flat_map(|(at, tile)| {
                    let grade = if graded {
                        tiles.of(*at)
                    } else {
                        crate::grade::Grade::IDENTITY
                    };
                    let weight = tile.usage / tile.cells.len() as f32;
                    tile.cells
                        .iter()
                        .map(move |c| (grade.apply(*c).map(|v| v * light), weight))
                })
                .step_by(every)
                .filter(|(_, weight)| *weight > 0.0)
                .collect()
        };
        let (lit, raw) = (pieces(true), pieces(false));
        let before = measure(&raw, 0.0, 1.0, 1.0);

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
                |e| measure(&lit, e, contrast, saturation)[0],
            )
        };
        let exposing = far(
            measure(&lit, 0.0, 1.0, 1.0)[0],
            target.lightness,
            target.within[0],
        );
        if exposing {
            stops = expose(1.0, 1.0);
        }
        if far(
            measure(&lit, stops, 1.0, 1.0)[1],
            target.contrast,
            target.within[1],
        ) {
            let (least, most) = bounds.film_contrast;
            contrast = where_it_is(least, most, target.contrast, |k| {
                measure(&lit, stops, k, 1.0)[1]
            });
            if exposing {
                stops = expose(contrast, 1.0);
            }
        }
        if far(
            measure(&lit, stops, contrast, 1.0)[2],
            target.chroma,
            target.within[2],
        ) {
            let (least, most) = bounds.film_saturation;
            saturation = where_it_is(least, most, target.chroma, |s| {
                measure(&lit, stops, contrast, s)[2]
            });
        }
        // Kept to a 4096th, and still within the bounds after that.
        let within = |value: f32, (least, most): (f32, f32)| kept(value).clamp(least, most);
        let stops = within(stops, (-bounds.film_stops, bounds.film_stops));
        let contrast = within(contrast, bounds.film_contrast);
        let saturation = within(saturation, bounds.film_saturation);
        let after = measure(&lit, stops, contrast, saturation);
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
            tiles,
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
    /// a tile two packs drape has one gain. `None` of nothing.
    pub fn of_packs(
        parts: &[(&FilmGrade, &Observed)],
        target: &LookTarget,
        bounds: &Bounds,
        tile_bounds: &TileBounds,
    ) -> Option<Self> {
        match parts {
            [] => None,
            [(only, _)] => Some((*only).clone()),
            several => {
                let observed = Observed::merged(several.iter().map(|(_, seen)| *seen));
                let light =
                    several.iter().map(|(g, _)| g.light).sum::<f32>() / several.len() as f32;
                Some(Self::fit(&observed, light, target, bounds, tile_bounds))
            }
        }
    }

    /// The grade as it is kept beside a pack: a small JSON object. A
    /// tile's gain is in 64ths of a stop.
    pub fn to_json(&self) -> String {
        let three = |v: Pictured| format!("[{:.2},{:.2},{:.2}]", v[0], v[1], v[2]);
        let pair = |v: (f32, f32)| format!("[{:.3},{:.3}]", v.0, v.1);
        // Every tile given anything: a gain, or only the rest.
        let mut given = self.tiles.stops.clone();
        for at in self.tiles.rest.keys() {
            given.entry(*at).or_insert([0.0; 3]);
        }
        let tiles: Vec<String> = given
            .iter()
            .map(|((level, x, y), g)| {
                // A gain; then, for a tile given more than a gain, every
                // other number of the measure in its order, and last the
                // pivot. Stops in 64ths, a black point in 65536ths.
                let at = (*level, *x, *y);
                let mut n: Vec<i32> = g.iter().map(|v| (v * 64.0).round() as i32).collect();
                if let Some(rest) = self.tiles.rest.get(&at) {
                    for (value, name) in rest.iter().zip(&self.tiles.measure.names()[3..]) {
                        let steps = if *name == "black" { 65536.0 } else { 64.0 };
                        n.push((value * steps).round() as i32);
                    }
                    let pivot = self.tiles.pivot.get(&at).copied().unwrap_or(-2.5);
                    n.push((pivot * 64.0).round() as i32);
                }
                let list: Vec<String> = n.iter().map(i32::to_string).collect();
                format!("\"{level}/{x}/{y}\":[{}]", list.join(","))
            })
            .collect();
        let limited: Vec<String> = self
            .limited
            .iter()
            .map(|what| format!("\"{}\"", what.replace(['"', '\\', ']'], "")))
            .collect();
        let r = &self.fitted;
        format!(
            "{{\"film_grade\":2,\"measure\":\"{}\",\"film\":{{\"exposure_ev\":{},\"contrast\":{},\"saturation\":{},\"light\":{}}},\"before\":{},\"after\":{},\"limited\":[{}],\"fitted\":{{\"tiles\":{},\"measured\":{},\"blocks\":{},\"untouched\":{},\"edges\":{},\"borders\":{},\"accorded\":{},\"accord_broken\":{},\"held\":{},\"apart_before\":{},\"apart_after\":{}}},\"gain_unit\":\"64ths of a stop\",\"tiles\":{{{}}}}}",
            self.tiles.measure.name(),
            self.exposure_ev,
            self.contrast,
            self.saturation,
            self.light,
            three(self.before),
            three(self.after),
            limited.join(","),
            r.tiles,
            r.measured,
            r.blocks,
            r.untouched,
            r.edges,
            r.borders,
            r.accorded,
            r.accord_broken,
            r.held,
            pair(r.apart_before),
            pair(r.apart_after),
            tiles.join(",")
        )
    }

    /// Reads [`Self::to_json`] back. `None` if this is not one.
    pub fn from_json(text: &str) -> Option<Self> {
        if !text.contains("\"film_grade\":2") {
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
        // The tiles come last: `"<level>/<x>/<y>":[r,g,b]`, one after the
        // other.
        let body = text.split("\"tiles\":{").nth(1)?;
        let measure = if text.contains("\"measure\":\"curves\"") {
            crate::Measure::Curves
        } else {
            crate::Measure::Moments
        };
        let (mut stops, mut rest, mut pivot) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        for entry in body.split(']') {
            let Some((at, gains)) = entry.split_once(":[") else {
                continue;
            };
            let mut at = at
                .trim_matches(|c: char| !c.is_ascii_digit())
                .split('/')
                .map(str::parse::<u32>);
            let (level, x, y) = (at.next()?.ok()?, at.next()?.ok()?, at.next()?.ok()?);
            let at = (u8::try_from(level).ok()?, x, y);
            let n: Vec<f32> = gains
                .split(',')
                .filter_map(|n| n.trim().parse::<i32>().ok())
                .map(|n| n as f32)
                .collect();
            if n.len() != 3 && n.len() != measure.len() + 1 {
                return None;
            }
            let gain = [n[0] / 64.0, n[1] / 64.0, n[2] / 64.0];
            if gain != [0.0; 3] {
                stops.insert(at, gain);
            }
            if n.len() > 3 {
                let values: Vec<f32> = n[3..n.len() - 1]
                    .iter()
                    .zip(&measure.names()[3..])
                    .map(|(v, name)| v / if *name == "black" { 65536.0 } else { 64.0 })
                    .collect();
                rest.insert(at, values);
                pivot.insert(at, n[n.len() - 1] / 64.0);
            }
        }
        Some(Self {
            tiles: TileGains {
                measure,
                stops,
                rest,
                pivot,
            },
            exposure_ev: number(&film, "exposure_ev")?,
            contrast: number(&film, "contrast")?,
            saturation: number(&film, "saturation")?,
            light: number(&film, "light")?,
            before: three("before")?,
            after: three("after")?,
            fitted: TileReport {
                tiles: count("tiles")?,
                measured: count("measured")?,
                blocks: count("blocks")?,
                untouched: count("untouched")?,
                edges: count("edges")?,
                borders: count("borders")?,
                accorded: count("accorded")?,
                accord_broken: count("accord_broken")?,
                held: count("held")?,
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

    /// A value that is the same for the same seed, 0 to 1.
    fn noise(seed: u32) -> f32 {
        let mut s = seed.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
        s ^= s >> 15;
        s = s.wrapping_mul(0x2C1B_3C6D);
        s ^= s >> 12;
        (s >> 8) as f32 / (1u32 << 24) as f32
    }

    /// A film of `wide` × 4 tiles of level 13 about `tone`, of varied tone
    /// and colour, whose first `dark` columns are a capture `stops` darker
    /// — over a reference level that shows the same ground as one picture.
    fn film(tone: f32, wide: u32, dark: u32, stops: f32) -> Observed {
        let mut observed = Observed::default();
        let ground = |x: u32, y: u32, k: usize| {
            let seed = (y * 1000 + x) * 64 + k as u32;
            let light = tone * (0.6 + 0.8 * noise(seed));
            [
                light * (0.8 + 0.4 * noise(seed + 9000)),
                light * (0.9 + 0.4 * noise(seed + 18000)),
                light * (0.6 + 0.4 * noise(seed + 27000)),
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
            1.0,
            target,
            &Bounds::default(),
            &TileBounds::default(),
        )
    }

    #[test]
    fn a_films_tiles_are_brought_together_before_the_film_is_measured() {
        // A dark film, a third of it a capture a stop darker still.
        let grade = fit(&film(0.05, 6, 2, 1.0), &LookTarget::default());
        assert_eq!(grade.fitted.blocks, 2, "{:?}", grade.fitted);
        // The larger capture is the film's own; the other is brought up.
        assert!(grade.tiles.of((13, 55, 70)).is_identity());
        let lifted = grade.tiles.of((13, 50, 70)).gain[1].log2();
        assert!((lifted - 1.0).abs() < 0.05, "{lifted}");
        // The film is then exposed as the tiles will be drawn, not as the
        // imagery lies: as much as the same film of one capture, and less
        // than its darker third would have asked for.
        let one_capture = fit(&film(0.05, 6, 0, 0.0), &LookTarget::default());
        assert!(one_capture.exposure_ev > 0.5);
        assert!(
            (grade.exposure_ev - one_capture.exposure_ev).abs() < 0.05,
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
        // One capture: no tile is given a gain, and the film's part is
        // three numbers with no colour in them.
        assert!(grade.tiles.stops.is_empty());
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
        assert!(grade.tiles.of((13, 1, 1)).is_identity());
        let (target, bounds, tiles) = (
            LookTarget::default(),
            Bounds::default(),
            TileBounds::default(),
        );
        assert_eq!(FilmGrade::of_packs(&[], &target, &bounds, &tiles), None);
    }

    #[test]
    fn a_film_of_several_packs_is_fitted_as_one() {
        let whole = film(0.05, 8, 3, 1.0);
        let part = |from: u32, to: u32| Observed {
            tiles: whole
                .tiles
                .iter()
                .filter(|(at, _)| at.0 == 12 || (50 + from..50 + to).contains(&at.1))
                .map(|(at, tile)| (*at, tile.clone()))
                .collect(),
        };
        let (west, east) = (part(0, 4), part(4, 8));
        let (target, bounds, tiles) = (
            LookTarget::default(),
            Bounds::default(),
            TileBounds::default(),
        );
        let (g_west, g_east) = (fit(&west, &target), fit(&east, &target));
        // Each pack alone: the west holds its dark capture as its own.
        assert!(g_west.tiles.of((13, 50, 70)).is_identity());
        let film = FilmGrade::of_packs(
            &[(&g_west, &west), (&g_east, &east)],
            &target,
            &bounds,
            &tiles,
        )
        .expect("a grade");
        assert_eq!(film, fit(&whole, &target));
        assert!(!film.tiles.of((13, 50, 70)).is_identity());
        // One pack alone is its own grade, exactly.
        assert_eq!(
            FilmGrade::of_packs(&[(&g_west, &west)], &target, &bounds, &tiles).as_ref(),
            Some(&g_west)
        );
    }

    #[test]
    fn a_grade_is_read_back_as_it_was_written() {
        let grade = fit(&film(0.004, 6, 2, 0.75), &LookTarget::default());
        assert!(!grade.limited.is_empty() && !grade.tiles.stops.is_empty());
        let read = FilmGrade::from_json(&grade.to_json()).expect("read back");
        assert_eq!(read.tiles, grade.tiles);
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
        assert_eq!(read.fitted.blocks, grade.fitted.blocks);
        assert_eq!(read.fitted.borders, grade.fitted.borders);
        assert_eq!(read.fitted.accord_broken, 0);
        assert!(close(
            read.fitted.apart_before.1,
            grade.fitted.apart_before.1
        ));
        assert_eq!(FilmGrade::from_json("{}"), None);
    }
}
