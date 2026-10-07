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
//!   - *its levels brought to one another* — a [`Grade`] per level of
//!     imagery, with the level the film draws most left exactly as it is.
//!     The others are brought to it. This part makes tiles agree and
//!     leaves the film as light as it was;
//!   - *the film brought to the target* — one exposure, one contrast, one
//!     saturation for every picture of it. No colour: one gain for the
//!     three channels, so a grey stays grey.
//!
//! Every number of a film's grade has a floor and a ceiling ([`Bounds`]).
//! A fit that wants more than it may have is given the bound and says so
//! ([`FilmGrade::limited`]): a film that cannot be brought to the target
//! within the bounds is left short of it, never pushed past them.
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

use crate::grade::{Grade, LevelGrades, Seen};
use crate::levels::LevelParams;

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

/// What a film's grade may not go past.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// A level against the film's own, in stops either way.
    pub level_stops: f32,
    /// A channel of a level against that level's gain on light, in stops
    /// either way: how much cast a level may be corrected by.
    pub level_tint_stops: f32,
    /// A level's contrast and saturation, least and most.
    pub level_contrast: (f32, f32),
    pub level_saturation: (f32, f32),
    /// The film against the target, in stops either way.
    pub film_stops: f32,
    /// The film's contrast and saturation, least and most.
    pub film_contrast: (f32, f32),
    pub film_saturation: (f32, f32),
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            level_stops: 1.0,
            level_tint_stops: 0.15,
            level_contrast: (0.85, 1.2),
            level_saturation: (0.85, 1.2),
            film_stops: 2.0,
            film_contrast: (0.8, 1.3),
            film_saturation: (0.8, 1.3),
        }
    }
}

/// A piece of the film's imagery: one cell of one tile, in linear light,
/// counted for the tiles of the film it is draped on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub level: u8,
    pub colour: [f32; 3],
    pub weight: f32,
}

/// What pictures measure: mean L*, contrast, mean C*.
pub type Measure = [f32; 3];

/// A film's grade: see the module.
#[derive(Debug, Clone, PartialEq)]
pub struct FilmGrade {
    /// The film's levels brought to the one it draws most.
    pub levels: LevelGrades,
    /// The film brought to the target: stops added to the renderer's own
    /// exposure, then its contrast and saturation multiplied by these.
    pub exposure_ev: f32,
    pub contrast: f32,
    pub saturation: f32,
    /// How much of the film each level is: tiles draped, by level.
    pub usage: BTreeMap<u8, f32>,
    /// What the film's pictures were foretold to measure with nothing
    /// done, and with this grade.
    pub before: Measure,
    pub after: Measure,
    /// What the fit wanted more of than it may have.
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
fn measure(lit: &[([f32; 3], f32)], stops: f32, contrast: f32, saturation: f32) -> Measure {
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

/// A level's grade held within its bounds, and what was held back.
fn bounded(level: u8, grade: Grade, bounds: &Bounds, limited: &mut Vec<String>) -> Grade {
    if grade.is_identity() {
        return grade;
    }
    let mut held = |what: &str| limited.push(format!("level {level}: {what}"));
    // The gain on light, then each channel against it.
    let light = luma(grade.gain).max(1e-6).log2();
    let lifted = light.clamp(-bounds.level_stops, bounds.level_stops);
    if lifted != light {
        held("gain");
    }
    // Each channel against the light of the three together — and that
    // light is itself moved by holding a channel back, so it is taken
    // again until it has settled.
    let wanted = grade.gain.map(|g| g.max(1e-6).log2() - light);
    let mut tint = wanted;
    for _ in 0..8 {
        let together = luma(tint.map(f32::exp2)).max(1e-6).log2();
        tint =
            tint.map(|t| (t - together).clamp(-bounds.level_tint_stops, bounds.level_tint_stops));
    }
    if (0..3).any(|i| (tint[i] - wanted[i]).abs() > 0.01) {
        held("cast");
    }
    let gain = tint.map(|t| kept(lifted + t).exp2());
    let mut between = |value: f32, (least, most): (f32, f32), what: &str| {
        let kept_value = value.clamp(least, most);
        if kept_value != value {
            limited.push(format!("level {level}: {what}"));
        }
        kept_value
    };
    Grade {
        black: grade.black,
        gain,
        contrast: between(grade.contrast, bounds.level_contrast, "contrast"),
        pivot: grade.pivot,
        saturation: between(grade.saturation, bounds.level_saturation, "saturation"),
    }
}

impl FilmGrade {
    /// The grade of a film that has none: nothing is done to its imagery,
    /// and it is drawn with the renderer's own settings.
    pub fn none() -> Self {
        Self {
            levels: LevelGrades {
                anchor: 0,
                grades: BTreeMap::new(),
                sources: Vec::new(),
            },
            exposure_ev: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            usage: BTreeMap::new(),
            before: [0.0; 3],
            after: [0.0; 3],
            limited: Vec::new(),
        }
    }

    /// Fits a film's grade on its imagery: `seen`, its tiles against their
    /// ancestors over the same ground; `samples`, what it is made of;
    /// `light`, a picture's linear value for an imagery value of one.
    pub fn fit(
        seen: &[Seen],
        samples: &[Sample],
        light: f32,
        target: &LookTarget,
        bounds: &Bounds,
    ) -> Self {
        let mut usage: BTreeMap<u8, f32> = BTreeMap::new();
        for s in samples {
            *usage.entry(s.level).or_default() += s.weight;
        }
        let Some(own) = usage
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(level, _)| *level)
        else {
            return Self::none();
        };
        let mut limited = Vec::new();

        // The levels brought to the film's own.
        // A film of one level has nothing to be brought to anything.
        let mut levels = if seen.is_empty() {
            LevelGrades {
                anchor: own,
                grades: BTreeMap::new(),
                sources: Vec::new(),
            }
        } else {
            LevelGrades::solve(
                seen,
                &LevelParams {
                    anchor: own,
                    // Wider than a level may be given: what the fit wanted is
                    // then known, and what is held back of it can be said.
                    clamp_stops: 3.0 * bounds.level_stops,
                    ..LevelParams::default()
                },
            )
        };
        // The solve holds the level it was told to, or — if nothing of it
        // was seen against another — the nearest that was. Whichever it
        // held, the film's own is brought back to nothing and the others
        // with it, so that the film is as light as it was.
        let held = levels.of(own);
        if !held.is_identity() {
            let back = luma(held.gain).max(1e-6);
            for grade in levels.grades.values_mut() {
                grade.gain = grade.gain.map(|g| g / back);
            }
            levels.grades.insert(own, Grade::IDENTITY);
        }
        for (level, grade) in &mut levels.grades {
            *grade = bounded(*level, *grade, bounds, &mut limited);
        }

        // The film as it will be lit, a part of it if it is large.
        let every = samples.len().div_ceil(MEASURED).max(1);
        let lit: Vec<([f32; 3], f32)> = samples
            .iter()
            .step_by(every)
            .map(|s| {
                (
                    levels.of(s.level).apply(s.colour).map(|v| v * light),
                    s.weight,
                )
            })
            .collect();
        let raw: Vec<([f32; 3], f32)> = samples
            .iter()
            .step_by(every)
            .map(|s| (s.colour.map(|v| v * light), s.weight))
            .collect();
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
            levels,
            exposure_ev: stops,
            contrast,
            saturation,
            usage,
            before,
            after,
            limited,
        }
    }

    /// One grade from several, each counting for how much imagery it was
    /// fitted on: the grades of a film's packs, made the film's. One grade
    /// from the first frame to the last is what keeps a film from changing
    /// where one pack ends and the next begins. `None` of nothing.
    ///
    /// A mean, not a fit over the packs together: packs whose own levels
    /// differ are brought to one another only as far as their means are.
    pub fn merged(parts: &[&FilmGrade]) -> Option<Self> {
        let weighed: Vec<(&FilmGrade, f32)> = parts
            .iter()
            .map(|p| (*p, p.usage.values().sum::<f32>()))
            .filter(|(_, weight)| *weight > 0.0)
            .collect();
        let total: f32 = weighed.iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            return None;
        }
        if let [(only, _)] = weighed[..] {
            return Some(only.clone());
        }
        let tables: Vec<(&LevelGrades, f32)> =
            weighed.iter().map(|(p, w)| (&p.levels, *w)).collect();
        let mean = |of: &dyn Fn(&FilmGrade) -> f32| {
            weighed.iter().map(|(p, w)| of(p) * w).sum::<f32>() / total
        };
        let both =
            |of: &dyn Fn(&FilmGrade) -> Measure| [0, 1, 2].map(|i| mean(&|p: &FilmGrade| of(p)[i]));
        let mut usage: BTreeMap<u8, f32> = BTreeMap::new();
        let mut limited: Vec<String> = Vec::new();
        for (part, _) in &weighed {
            for (level, tiles) in &part.usage {
                *usage.entry(*level).or_default() += tiles;
            }
            for what in &part.limited {
                if !limited.contains(what) {
                    limited.push(what.clone());
                }
            }
        }
        Some(Self {
            levels: LevelGrades::merged(&tables)?,
            exposure_ev: kept(mean(&|p| p.exposure_ev)),
            contrast: kept(mean(&|p| p.contrast.ln()).exp()),
            saturation: kept(mean(&|p| p.saturation.ln()).exp()),
            usage,
            before: both(&|p| p.before),
            after: both(&|p| p.after),
            limited,
        })
    }

    /// The grade as it is kept beside a pack: a small JSON object.
    pub fn to_json(&self) -> String {
        let three = |v: Measure| format!("[{:.2},{:.2},{:.2}]", v[0], v[1], v[2]);
        let usage: Vec<String> = self
            .usage
            .iter()
            .map(|(level, tiles)| format!("\"{level}\":{tiles:.1}"))
            .collect();
        let limited: Vec<String> = self
            .limited
            .iter()
            .map(|what| format!("\"{}\"", what.replace(['"', '\\', ']'], "")))
            .collect();
        format!(
            "{{\"film_grade\":1,\"film\":{{\"exposure_ev\":{},\"contrast\":{},\"saturation\":{}}},\"usage\":{{{}}},\"before\":{},\"after\":{},\"limited\":[{}],\"table\":{}}}",
            self.exposure_ev,
            self.contrast,
            self.saturation,
            usage.join(","),
            three(self.before),
            three(self.after),
            limited.join(","),
            self.levels.to_json()
        )
    }

    /// Reads [`Self::to_json`] back. `None` if this is not one.
    pub fn from_json(text: &str) -> Option<Self> {
        if !text.contains("\"film_grade\":1") {
            return None;
        }
        let inside = |open: &str, close: char| -> Option<&str> {
            text.split(open).nth(1)?.split(close).next()
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
        let three = |key: &str| -> Option<Measure> {
            let v: Vec<f32> = inside(&format!("\"{key}\":["), ']')?
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            (v.len() == 3).then(|| [v[0], v[1], v[2]])
        };
        let film = inside("\"film\":{", '}')?;
        let usage = inside("\"usage\":{", '}')?
            .split(',')
            .filter_map(|entry| {
                let (level, tiles) = entry.split_once(':')?;
                Some((
                    level.trim().trim_matches('"').parse().ok()?,
                    tiles.trim().parse().ok()?,
                ))
            })
            .collect();
        let limited = inside("\"limited\":[", ']')?
            .split("\",\"")
            .map(|what| what.trim_matches('"').to_string())
            .filter(|what| !what.is_empty())
            .collect();
        Some(Self {
            levels: LevelGrades::from_json(text.split("\"table\":").nth(1)?)?,
            exposure_ev: number(film, "exposure_ev")?,
            contrast: number(film, "contrast")?,
            saturation: number(film, "saturation")?,
            usage,
            before: three("before")?,
            after: three("after")?,
            limited,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground of varied tone and colour, the same for the same seed, about
    /// `tone` in linear light.
    fn ground(seed: u32, cells: usize, tone: f32) -> Vec<[f32; 3]> {
        let mut state = seed | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        (0..cells)
            .map(|_| {
                let light = tone * (0.25 + 1.5 * next());
                [
                    light * (0.8 + 0.4 * next()),
                    light * (0.9 + 0.4 * next()),
                    light * (0.6 + 0.4 * next()),
                ]
            })
            .collect()
    }

    /// A film of two levels over the same ground: `fine` drawn `share` of
    /// the time and darker than `coarse` by `stops`, with `cast` more of it
    /// on blue.
    fn film(tone: f32, stops: f32, cast: f32, share: f32) -> (Vec<Seen>, Vec<Sample>) {
        let (coarse, fine) = (12u8, 14u8);
        let mut seen = Vec::new();
        let mut samples = Vec::new();
        for tile in 0..40u32 {
            let under = ground(tile + 1, 64, tone);
            let over: Vec<[f32; 3]> = under
                .iter()
                .map(|c| {
                    let by = (-stops).exp2();
                    [c[0] * by, c[1] * by, c[2] * by * (-cast).exp2()]
                })
                .collect();
            for c in &under {
                samples.push(Sample {
                    level: coarse,
                    colour: *c,
                    weight: 1.0 - share,
                });
            }
            for c in &over {
                samples.push(Sample {
                    level: fine,
                    colour: *c,
                    weight: share,
                });
            }
            seen.push(Seen {
                level: fine,
                ancestor: coarse,
                tile: over,
                under,
            });
        }
        (seen, samples)
    }

    fn fit(seen: &[Seen], samples: &[Sample], bounds: &Bounds) -> FilmGrade {
        FilmGrade::fit(seen, samples, 1.0, &LookTarget::default(), bounds)
    }

    #[test]
    fn the_level_a_film_draws_most_is_left_as_it_is() {
        // Fine tiles half a stop darker, and most of the film.
        let (seen, samples) = film(0.18, 0.5, 0.0, 0.8);
        let grade = fit(&seen, &samples, &Bounds::default());
        assert!(grade.levels.of(14).is_identity(), "{:?}", grade.levels);
        // The other is brought to it: half a stop down.
        let coarse = luma(grade.levels.of(12).gain).log2();
        assert!((coarse + 0.5).abs() < 0.05, "{coarse}");

        // The same film drawn mostly from the coarse level holds that one.
        let (seen, samples) = film(0.18, 0.5, 0.0, 0.2);
        let grade = fit(&seen, &samples, &Bounds::default());
        assert!(grade.levels.of(12).is_identity(), "{:?}", grade.levels);
        let fine = luma(grade.levels.of(14).gain).log2();
        assert!((fine - 0.5).abs() < 0.05, "{fine}");
    }

    #[test]
    fn nothing_goes_past_its_bounds_and_what_is_held_back_is_said() {
        // Two stops between the levels, and a strong cast: more than a
        // level may be given of either.
        let (seen, samples) = film(0.18, 2.0, 0.8, 0.2);
        let bounds = Bounds::default();
        let grade = fit(&seen, &samples, &bounds);
        let fine = grade.levels.of(14);
        let light = luma(fine.gain).log2();
        assert!(light <= bounds.level_stops + 1e-3, "{light}");
        for g in fine.gain {
            assert!(
                (g.log2() - light).abs() <= bounds.level_tint_stops + 1e-3,
                "{fine:?}"
            );
        }
        assert!(
            fine.contrast >= bounds.level_contrast.0 && fine.contrast <= bounds.level_contrast.1
        );
        assert!(
            fine.saturation >= bounds.level_saturation.0
                && fine.saturation <= bounds.level_saturation.1
        );
        assert!(
            grade.limited.iter().any(|w| w == "level 14: gain"),
            "{:?}",
            grade.limited
        );
        assert!(grade.limited.iter().any(|w| w == "level 14: cast"));

        // And the film: far too dark to be brought to the target in two
        // stops. It is given two, and said to be short.
        let (seen, samples) = film(0.004, 0.0, 0.0, 0.5);
        let grade = fit(&seen, &samples, &bounds);
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
    fn a_dark_film_is_lifted_to_the_target_and_a_grey_stays_grey() {
        let (seen, samples) = film(0.05, 0.0, 0.0, 0.5);
        let target = LookTarget::default();
        let grade = FilmGrade::fit(&seen, &samples, 1.0, &target, &Bounds::default());
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
        // The film's part is three numbers and no colour: nothing in it
        // can turn a grey.
        for grade in grade.levels.grades.values() {
            assert!(grade.is_identity());
        }
    }

    #[test]
    fn a_film_already_at_the_target_is_not_touched() {
        // Found by the fit itself: the film as lit that measures the
        // target's lightness.
        let (seen, samples) = film(0.18, 0.0, 0.0, 0.5);
        let mut target = LookTarget::default();
        let as_it_is = FilmGrade::fit(&seen, &samples, 1.0, &target, &Bounds::default());
        target.lightness = as_it_is.before[0];
        target.contrast = as_it_is.before[1];
        target.chroma = as_it_is.before[2];
        let grade = FilmGrade::fit(&seen, &samples, 1.0, &target, &Bounds::default());
        assert_eq!(
            (grade.exposure_ev, grade.contrast, grade.saturation),
            (0.0, 1.0, 1.0)
        );
        assert!(grade.limited.is_empty(), "{:?}", grade.limited);
        // A little off is still left alone; far off is not.
        target.lightness += 0.9 * target.within[0];
        let near = FilmGrade::fit(&seen, &samples, 1.0, &target, &Bounds::default());
        assert_eq!(near.exposure_ev, 0.0);
        target.lightness += 3.0 * target.within[0];
        let far = FilmGrade::fit(&seen, &samples, 1.0, &target, &Bounds::default());
        assert!(far.exposure_ev > 0.0);
    }

    #[test]
    fn a_film_of_nothing_has_no_grade_and_borrows_none() {
        let grade = fit(&[], &[], &Bounds::default());
        assert_eq!(grade, FilmGrade::none());
        assert!(grade.levels.of(13).is_identity());
        assert_eq!(FilmGrade::merged(&[&grade]), None);

        // A film of one level, never seen against another: its levels are
        // left alone, and it is still brought to the target.
        let one: Vec<Sample> = ground(7, 400, 0.03)
            .into_iter()
            .map(|colour| Sample {
                level: 15,
                colour,
                weight: 1.0,
            })
            .collect();
        let grade = fit(&[], &one, &Bounds::default());
        assert!(grade.levels.of(15).is_identity());
        assert!(grade.exposure_ev > 0.0, "{grade:?}");
    }

    #[test]
    fn the_grades_of_a_films_packs_are_made_one() {
        let (seen, samples) = film(0.05, 0.5, 0.0, 0.8);
        let one = fit(&seen, &samples, &Bounds::default());
        let (seen, samples) = film(0.10, 0.5, 0.0, 0.8);
        let other = fit(&seen, &samples, &Bounds::default());
        assert!(one.exposure_ev > other.exposure_ev);
        let film = FilmGrade::merged(&[&one, &other]).expect("a grade");
        // Equal parts: half-way between the two.
        let middle = 0.5 * (one.exposure_ev + other.exposure_ev);
        assert!((film.exposure_ev - middle).abs() < 1e-3, "{film:?}");
        assert_eq!(film.usage[&14], one.usage[&14] + other.usage[&14]);
        // One pack alone is its own grade, exactly.
        assert_eq!(FilmGrade::merged(&[&one]).as_ref(), Some(&one));
    }

    #[test]
    fn a_grade_is_read_back_as_it_was_written() {
        let (seen, samples) = film(0.004, 3.0, 0.8, 0.2);
        let mut grade = fit(&seen, &samples, &Bounds::default());
        assert!(!grade.limited.is_empty());
        let read = FilmGrade::from_json(&grade.to_json()).expect("read back");
        // What a fit was found from is not kept.
        grade.levels.sources.clear();
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert_eq!(read.limited, grade.limited);
        assert_eq!(
            read.usage.keys().collect::<Vec<_>>(),
            grade.usage.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            (read.exposure_ev, read.contrast, read.saturation),
            (grade.exposure_ev, grade.contrast, grade.saturation)
        );
        for i in 0..3 {
            assert!(close(read.before[i], grade.before[i]));
            assert!(close(read.after[i], grade.after[i]));
        }
        assert_eq!(read.levels.anchor, grade.levels.anchor);
        for (level, g) in &grade.levels.grades {
            let r = read.levels.of(*level);
            assert!(close(r.contrast, g.contrast) && close(r.saturation, g.saturation));
            for i in 0..3 {
                assert!(close(r.gain[i].log2(), g.gain[i].log2()), "{r:?} {g:?}");
            }
        }
        assert_eq!(FilmGrade::from_json("{}"), None);
        assert_eq!(FilmGrade::from_json(&grade.levels.to_json()), None);
    }
}
