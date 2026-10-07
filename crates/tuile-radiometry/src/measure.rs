// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How a tile is measured, kept apart from how a film's tiles are brought
//! together.
//!
//! There are two things, and they are not the same thing:
//!
//! - **a way of measuring** ([`Measure`]) says how a tile stands against
//!   the reference over the same ground — as a handful of numbers — and
//!   what a correction of those numbers does to a texel ([`Local`]);
//! - **a way of fitting** — a gain a block ([`crate::TileGains`]), a
//!   continuous field ([`crate::CornerField`]) — is handed those numbers
//!   and their bounds, and finds a correction for every tile. It does not
//!   know what the numbers are.
//!
//! So a fit is the same whatever the measure, and a measure can be changed
//! without touching a fit. What a fit may rely on is written here, once:
//!
//! 1. a measure is a list of numbers, always as many, in an order that
//!    does not change ([`Measure::names`]). One that could not be taken is
//!    not a number (`NaN`), and asks for nothing;
//! 2. **the first three are the tile's tone** against the reference, a
//!    channel each, in stops — what a seam between two captures is read
//!    from, whatever else a measure carries;
//! 3. a correction is a list of the same shape: what is *added* to a
//!    tile's measure to bring it where it is wanted;
//! 4. each number has a bound ([`Measure::within`]), and is kept to a
//!    fixed step ([`Measure::keep`]) so that it is the same wherever it
//!    was found.

use crate::tiles::{light_of, luma, Observed, TileAt, FLOOR};

/// What a correction may not go past, whatever is measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Tone, on light, in stops either way.
    pub light_stops: f32,
    /// A channel's tone against that, in stops either way: the cast.
    pub tint_stops: f32,
    /// Anything that is a ratio — contrast, saturation — in stops either
    /// way.
    pub shape_stops: f32,
    /// A point of a transfer curve, in stops either way of where the tone
    /// alone puts it.
    pub curve_stops: f32,
    /// A black point, in linear light either way.
    pub black: f32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            light_stops: 1.5,
            tint_stops: 0.6,
            shape_stops: 0.4,
            curve_stops: 1.0,
            black: 0.01,
        }
    }
}

/// The lights a transfer curve is given at, as stops under white: a point
/// a stop, from deep shadow to just under white.
pub const KNOTS: [f32; 8] = [-8.0, -7.0, -6.0, -5.0, -4.0, -3.0, -2.0, -1.0];

/// A transfer curve a channel: by how many stops a texel is lifted, given
/// the light it has. A value at each of [`KNOTS`], and between two of them
/// the line from one to the other; beyond the ends, the end's value.
pub type Curves = [[f32; KNOTS.len()]; 3];

/// A curve at a light, both in stops.
fn along(curve: &[f32; KNOTS.len()], stops: f32) -> f32 {
    let last = KNOTS.len() - 1;
    if stops <= KNOTS[0] {
        return curve[0];
    }
    if stops >= KNOTS[last] {
        return curve[last];
    }
    let at = (stops - KNOTS[0]) / (KNOTS[1] - KNOTS[0]);
    let below = (at.floor() as usize).min(last - 1);
    curve[below] + (curve[below + 1] - curve[below]) * (at - below as f32)
}

/// What is done to a texel of a tile, in linear light and in this order:
/// a colour taken away; a gain a channel; a transfer curve a channel, read
/// at the light the texel came in with; a power on luminance about a
/// pivot; a factor on what is not luminance.
///
/// The widest of what any measure asks for: a measure with no curve leaves
/// it out, one with no contrast leaves that at one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Local {
    /// A colour matrix the texel goes through first, where in the tile it
    /// is having been taken into it already ([`crate::MatrixField`]).
    pub matrix: Option<crate::Affine>,
    pub black: [f32; 3],
    pub gain: [f32; 3],
    /// Stops added to each channel, by the light it has: see [`Curves`].
    pub curve: Option<Curves>,
    /// Luminance raised to this about `pivot`: contrast.
    pub contrast: f32,
    pub pivot: f32,
    pub saturation: f32,
}

impl Local {
    /// Changes nothing.
    pub const IDENTITY: Self = Self {
        matrix: None,
        black: [0.0; 3],
        gain: [1.0; 3],
        curve: None,
        contrast: 1.0,
        pivot: 0.18,
        saturation: 1.0,
    };

    pub fn is_identity(&self) -> bool {
        self.matrix.is_none_or(|m| m == crate::SAME)
            && self.black == [0.0; 3]
            && self.gain == [1.0; 3]
            && self.curve.is_none_or(|c| c == [[0.0; KNOTS.len()]; 3])
            && self.contrast == 1.0
            && self.saturation == 1.0
    }

    /// One linear colour, corrected. Not clamped above.
    pub fn apply(&self, colour: [f32; 3]) -> [f32; 3] {
        let colour = match &self.matrix {
            Some(matrix) => crate::through(matrix, colour).map(|v| v.max(0.0)),
            None => colour,
        };
        let mut c = [0.0f32; 3];
        for i in 0..3 {
            let came = (colour[i] - self.black[i]).max(0.0);
            c[i] = came * self.gain[i];
            if let Some(curve) = &self.curve {
                c[i] *= along(&curve[i], came.max(1e-9).log2()).exp2();
            }
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

    /// As a grade, for whoever applies only that: the same, less the
    /// transfer curve, which a grade has not.
    pub fn grade(&self) -> crate::Grade {
        crate::Grade {
            black: self.black,
            gain: self.gain,
            contrast: self.contrast,
            pivot: self.pivot,
            saturation: self.saturation,
        }
    }
}

/// A way of measuring a tile against the reference: see the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// By moments, from the tile's ground in cells: tone a channel, then
    /// contrast (how far its light swings), saturation (how far its
    /// colours stand from grey) and black point (its darkest).
    Moments,
    /// By its **transfer curve**, from how the tile's texels are spread:
    /// tone a channel, then for each channel the curve that lays the
    /// tile's spread of light on the reference's — at each of [`KNOTS`],
    /// by how many stops the tile stands above where its tone alone would
    /// put it.
    ///
    /// The curve is the whole of what a colourist turns, a channel at a
    /// time: its low end is the black point, its slope the contrast, and
    /// the three of them against one another the saturation. So it carries
    /// none of those as numbers of their own.
    Curves,
    /// By the **line** that lays the tile on a reference picture of the
    /// same ground, a band at a time ([`crate::Line`]): its gain, as the
    /// tone, then its bias, as how far above the reference's the tile's
    /// black stands. The model a mosaic is commonly normalised by.
    ///
    /// The three lines against one another are the contrast and the
    /// saturation, so it carries neither as a number of its own. The
    /// reference is not a tile of the film: it is what each tile was set
    /// against when it was seen ([`crate::TileSeen::set_against`]).
    Linear,
}

const CURVE_NAMES: [&str; 3 + 3 * KNOTS.len()] = [
    "tone_r", "tone_g", "tone_b", "r_m8", "r_m7", "r_m6", "r_m5", "r_m4", "r_m3", "r_m2", "r_m1",
    "g_m8", "g_m7", "g_m6", "g_m5", "g_m4", "g_m3", "g_m2", "g_m1", "b_m8", "b_m7", "b_m6", "b_m5",
    "b_m4", "b_m3", "b_m2", "b_m1",
];

impl Measure {
    /// What each number is, in the order they come.
    pub fn names(&self) -> &'static [&'static str] {
        match self {
            Self::Moments => &[
                "tone_r",
                "tone_g",
                "tone_b",
                "contrast",
                "saturation",
                "black",
            ],
            Self::Curves => &CURVE_NAMES,
            Self::Linear => &[
                "tone_r", "tone_g", "tone_b", "black_r", "black_g", "black_b",
            ],
        }
    }

    pub fn len(&self) -> usize {
        self.names().len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// A name for a file or a column.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Moments => "moments",
            Self::Curves => "curves",
            Self::Linear => "linear",
        }
    }

    /// How a tile stands against the reference over the same ground. `None`
    /// for a tile that cannot be set against it at all; a number that
    /// could not be taken is `NaN`.
    pub fn of(&self, observed: &Observed, at: TileAt, reference_level: u8) -> Option<Vec<f32>> {
        if *self == Self::Linear {
            // Against what the tile was set against, whatever the level.
            let line = observed.tiles.get(&at)?.paired.as_deref()?.line()?;
            return Some(vec![
                -line.gain[0].log2(),
                -line.gain[1].log2(),
                -line.gain[2].log2(),
                -line.bias[0],
                -line.bias[1],
                -line.bias[2],
            ]);
        }
        let tone = observed.offset(at, reference_level)?;
        match self {
            Self::Linear => None,
            Self::Moments => {
                let apart = observed.apart(at, reference_level);
                Some(vec![
                    tone[0],
                    tone[1],
                    tone[2],
                    apart.map_or(f32::NAN, |a| a.contrast),
                    apart.map_or(f32::NAN, |a| a.saturation),
                    apart.map_or(f32::NAN, |a| a.black),
                ])
            }
            Self::Curves => {
                let mut found = vec![tone[0], tone[1], tone[2]];
                match transfer(observed, at, reference_level) {
                    Some(curves) => {
                        for (c, curve) in curves.iter().enumerate() {
                            // Against the tone: what is left is shape.
                            found.extend(curve.iter().map(|v| v - tone[c]));
                        }
                    }
                    None => found.extend([f32::NAN; 3 * KNOTS.len()]),
                }
                Some(found)
            }
        }
    }

    /// The tone a measure carries: its first three numbers.
    pub fn tone(values: &[f32]) -> [f32; 3] {
        [values[0], values[1], values[2]]
    }

    /// A correction held within its bounds.
    pub fn within(&self, values: &mut [f32], limits: &Limits) {
        // Tone: on light, then each channel against it — and holding a
        // channel back moves the light of the three, so the cast is taken
        // against it again until it has settled.
        let g = Self::tone(values);
        let light = light_of(g);
        let lifted = light.clamp(-limits.light_stops, limits.light_stops);
        let mut tint = g.map(|c| c - light);
        for _ in 0..6 {
            let together = light_of(tint);
            tint = tint.map(|t| (t - together).clamp(-limits.tint_stops, limits.tint_stops));
        }
        for c in 0..3 {
            values[c] = lifted + tint[c];
        }
        match self {
            Self::Moments => {
                values[3] = values[3].clamp(-limits.shape_stops, limits.shape_stops);
                values[4] = values[4].clamp(-limits.shape_stops, limits.shape_stops);
                values[5] = values[5].clamp(-limits.black, limits.black);
            }
            Self::Linear => {
                for black in &mut values[3..] {
                    *black = black.clamp(-limits.black, limits.black);
                }
            }
            Self::Curves => {
                let knots = KNOTS.len();
                for channel in values[3..].chunks_exact_mut(knots) {
                    for v in channel.iter_mut() {
                        *v = v.clamp(-limits.curve_stops, limits.curve_stops);
                    }
                    // A transfer curve rises: lighter in, lighter out. From
                    // one point to the next, a stop apart, it may give back
                    // three quarters of that stop and no more.
                    for j in 1..knots {
                        channel[j] = channel[j].max(channel[j - 1] - 0.75);
                    }
                }
            }
        }
    }

    /// A correction kept to a fixed step: stops to a 64th, a black point —
    /// far smaller — to a 65536th.
    pub fn keep(&self, values: &mut [f32]) {
        for (value, name) in values.iter_mut().zip(self.names()) {
            let steps = if name.starts_with("black") {
                65536.0
            } else {
                64.0
            };
            *value = (*value * steps).round() / steps;
        }
    }

    /// A correction as it is blended: every number of it such that mixing
    /// two of these is the same as mixing the two corrections. `pivot` is
    /// the light a contrast turns about, as its stops under white: the
    /// tile's own tone once the gain is on.
    pub fn blended(&self, given: &[f32], pivot: f32) -> Blended {
        let gain_stops = Self::tone(given);
        match self {
            // A measure says how far above the film's own a black point
            // stands, so what is added to it is what is taken away.
            Self::Moments => Blended {
                gain_stops,
                black: [-given[5]; 3],
                contrast_stops: given[3],
                pivot_stops: pivot,
                saturation_stops: given[4],
                curve: [[0.0; KNOTS.len()]; 3],
            },
            // The same, a band at a time: with all of what was measured
            // given back, a texel `t` comes out `gain × t + bias`.
            Self::Linear => Blended {
                gain_stops,
                black: [-given[3], -given[4], -given[5]],
                contrast_stops: 0.0,
                pivot_stops: pivot,
                saturation_stops: 0.0,
                curve: [[0.0; KNOTS.len()]; 3],
            },
            Self::Curves => {
                let knots = KNOTS.len();
                Blended {
                    gain_stops,
                    black: [0.0; 3],
                    contrast_stops: 0.0,
                    pivot_stops: pivot,
                    saturation_stops: 0.0,
                    curve: std::array::from_fn(|c| {
                        std::array::from_fn(|j| given[3 + c * knots + j])
                    }),
                }
            }
        }
    }

    /// What a correction does to a texel: see [`Self::blended`].
    pub fn local(&self, given: &[f32], pivot: f32) -> Local {
        self.blended(given, pivot).local()
    }
}

/// A correction in the form it is blended in, between the corners of a
/// tile: stops stay stops until the blend is done. What a renderer is
/// handed, a corner at a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Blended {
    /// The gain, in stops a channel.
    pub gain_stops: [f32; 3],
    /// A colour taken away, in the light the gain leaves.
    pub black: [f32; 3],
    pub contrast_stops: f32,
    pub pivot_stops: f32,
    pub saturation_stops: f32,
    pub curve: Curves,
}

impl Blended {
    /// Four corners at a place in a tile, `u` across and `v` down.
    pub fn mix(corners: &[Blended; 4], u: f32, v: f32) -> Self {
        let blend = |of: &dyn Fn(&Blended) -> f32| {
            (of(&corners[0]) * (1.0 - u) + of(&corners[1]) * u) * (1.0 - v)
                + (of(&corners[2]) * (1.0 - u) + of(&corners[3]) * u) * v
        };
        Self {
            gain_stops: std::array::from_fn(|c| blend(&|b| b.gain_stops[c])),
            black: std::array::from_fn(|c| blend(&|b| b.black[c])),
            contrast_stops: blend(&|b| b.contrast_stops),
            pivot_stops: blend(&|b| b.pivot_stops),
            saturation_stops: blend(&|b| b.saturation_stops),
            curve: std::array::from_fn(|c| std::array::from_fn(|j| blend(&|b| b.curve[c][j]))),
        }
    }

    /// What it does to a texel.
    pub fn local(&self) -> Local {
        let gain = self.gain_stops.map(f32::exp2);
        Local {
            matrix: None,
            // Found in the light the gain leaves, where a texel loses it
            // before the gain.
            black: std::array::from_fn(|c| self.black[c] / gain[c].max(1e-6)),
            gain,
            curve: (self.curve != [[0.0; KNOTS.len()]; 3]).then_some(self.curve),
            contrast: self.contrast_stops.exp2(),
            pivot: self.pivot_stops.exp2(),
            saturation: self.saturation_stops.exp2(),
        }
    }
}

/// Shares of a tile's texels between which a point of its transfer curve
/// can be read: outside them a light is one the tile hardly has, and what
/// the reference has at the same share says nothing of it.
const READABLE: (f32, f32) = (0.03, 0.97);

/// A tile's transfer curve against the reference, a channel at a time: at
/// each of [`KNOTS`], how many stops above the reference the tile is.
///
/// Read off the two spreads of light, share for share: of the tile's
/// texels a share lie under a given light; the light under which the same
/// share of the reference's lie, over the same ground, is what that light
/// stands for there. A point the tile has no texels about is not a number.
///
/// Nothing is smoothed along the curve: a curve smoothed here is a curve
/// under-corrected, by as much — measured, a third of it was left. What
/// noise there is from one tile to the next is the fit's to deal with.
fn transfer(observed: &Observed, at: TileAt, reference_level: u8) -> Option<Curves> {
    let (level, x, y) = at;
    // A tile is set against the quarter of the reference that is its own
    // ground: a level up. Further up, a quarter is more ground than the
    // tile, and a spread of light over other ground says nothing of it.
    if level.checked_sub(reference_level)? != 1 {
        return None;
    }
    let mine = observed.tiles.get(&at)?.tones.as_deref()?;
    let theirs = observed
        .tiles
        .get(&(reference_level, x >> 1, y >> 1))?
        .tones
        .as_deref()?;
    let quarter = 1 + (x & 1) as usize + 2 * (y & 1) as usize;
    let mut curves = [[f32::NAN; KNOTS.len()]; 3];
    for (c, curve) in curves.iter_mut().enumerate() {
        for (j, knot) in KNOTS.iter().enumerate() {
            let share = mine.share(0, c, *knot);
            if (READABLE.0..=READABLE.1).contains(&share) {
                curve[j] = knot - theirs.quantile(quarter, c, share);
            }
        }
    }
    Some(curves)
}

/// A tile's own tone, as its stops under white.
pub(crate) fn own_tone(observed: &Observed, at: TileAt) -> Option<f32> {
    let tile = observed.tiles.get(&at)?;
    Some((luma(tile.mean()) + FLOOR).log2())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::PAIRS;
    use crate::tiles::TileSeen;

    const SIDE: usize = 64;

    /// A tile whose texels are the ground's, each through `through`.
    fn tile(seed: u32, through: impl Fn([f32; 3]) -> [f32; 3]) -> TileSeen {
        let mut state = seed | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        let texels: Vec<[f32; 3]> = (0..SIDE * SIDE)
            .map(|_| {
                // Six stops of light, some colour.
                let y = 0.004 * (6.0 * next()).exp2();
                through([y * (0.8 + 0.4 * next()), y, y * (0.6 + 0.4 * next())])
            })
            .collect();
        let mut seen = TileSeen::of_linear(&texels, SIDE).expect("a tile");
        seen.usage = 1.0;
        seen
    }

    /// A film of one tile over a reference: the tile's texels are the
    /// reference's, each through `through`.
    fn film(through: impl Fn([f32; 3]) -> [f32; 3]) -> Observed {
        let mut observed = Observed::default();
        observed.tiles.insert((13, 100, 200), tile(5, through));
        // The reference's quarter over that tile is the same ground: the
        // whole reference tile is made of it, so each quarter is too.
        observed.tiles.insert((12, 50, 100), tile(5, |c| c));
        observed
    }

    #[test]
    fn every_measure_begins_with_the_tone_and_is_as_long_as_its_names() {
        let observed = film(|c| c.map(|v| v * 0.5));
        for measure in [Measure::Moments, Measure::Curves] {
            let found = measure.of(&observed, (13, 100, 200), 12).expect("measured");
            assert_eq!(found.len(), measure.len());
            assert_eq!(&measure.names()[..3], ["tone_r", "tone_g", "tone_b"]);
            // Half the light: a stop under the reference, on every channel.
            for c in Measure::tone(&found) {
                assert!((c + 1.0).abs() < 0.05, "{measure:?}: {found:?}");
            }
        }
        assert_eq!(Measure::Moments.of(&observed, (13, 1, 1), 12), None);
    }

    #[test]
    fn the_transfer_curve_is_read_off_the_two_spreads_of_light() {
        // The tile is the reference through a curve of its own on each
        // channel: flatter on red, as it is on green, steeper on blue.
        let pivot = 0.05f32;
        let powers = [0.7f32, 1.0, 1.3];
        let observed = film(|c| [0, 1, 2].map(|k| pivot * (c[k] / pivot).powf(powers[k])));
        let found = Measure::Curves
            .of(&observed, (13, 100, 200), 12)
            .expect("measured");
        assert_eq!(found.len(), 3 + 3 * KNOTS.len());
        let curve = |c: usize| &found[3 + c * KNOTS.len()..3 + (c + 1) * KNOTS.len()];
        // Green is the reference's own: its curve is flat.
        for v in curve(1).iter().filter(|v| v.is_finite()) {
            assert!(v.abs() < 0.15, "{:?}", curve(1));
        }
        // Red is flatter than the reference: its shadows stand above where
        // its tone puts them and its highlights below — the curve falls.
        // Blue, steeper, rises. A stop of light in is `power` stops out.
        let slope = |c: usize| {
            let read: Vec<(f32, f32)> = KNOTS
                .iter()
                .zip(curve(c))
                .filter(|(_, v)| v.is_finite())
                .map(|(k, v)| (*k, *v))
                .collect();
            assert!(read.len() >= 4, "channel {c}: {:?}", curve(c));
            let (first, last) = (read[0], read[read.len() - 1]);
            (last.1 - first.1) / (last.0 - first.0)
        };
        // The tile's light against the reference's: x = power · y, so the
        // tile stands (1 − 1/power) stops above per stop of its own light.
        for c in [0, 2] {
            let expected = 1.0 - 1.0 / powers[c];
            assert!(
                (slope(c) - expected).abs() < 0.12,
                "channel {c}: {} for {expected}",
                slope(c)
            );
        }
        // A light the tile has no texels about is not read.
        assert!(curve(0)[0].is_nan(), "{:?}", curve(0));
    }

    #[test]
    fn what_a_measure_finds_its_correction_undoes() {
        // Measure a tile, ask for the correction that brings it to the
        // reference, apply it to the texels, measure again: what was found
        // is gone, by the same measure — and by the other.
        let pivot = 0.05f32;
        let through = |c: [f32; 3]| {
            [0, 1, 2].map(|k| 0.6 * pivot * (c[k] / pivot).powf([0.8f32, 0.9, 1.15][k]))
        };
        let observed = film(through);
        let at = (13, 100, 200);
        for measure in [Measure::Curves, Measure::Moments] {
            let found = measure.of(&observed, at, 12).expect("measured");
            // What could not be read asks for nothing.
            let mut given: Vec<f32> = found
                .iter()
                .map(|v| if v.is_finite() { -v } else { 0.0 })
                .collect();
            measure.within(
                &mut given,
                &Limits {
                    shape_stops: 1.0,
                    ..Limits::default()
                },
            );
            measure.keep(&mut given);
            let tone = own_tone(&observed, at).expect("a tone") + light_of(Measure::tone(&given));
            let local = measure.local(&given, tone);
            // The tile again, texel by texel, corrected.
            let mut state = 5u32 | 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 8) as f32 / (1u32 << 24) as f32
            };
            let texels: Vec<[f32; 3]> = (0..SIDE * SIDE)
                .map(|_| {
                    let y = 0.004 * (6.0 * next()).exp2();
                    local.apply(through([
                        y * (0.8 + 0.4 * next()),
                        y,
                        y * (0.6 + 0.4 * next()),
                    ]))
                })
                .collect();
            let mut after = observed.clone();
            after
                .tiles
                .insert(at, TileSeen::of_linear(&texels, SIDE).expect("a tile"));
            let left = measure.of(&after, at, 12).expect("measured");
            // Tone. The curve brings it to the reference, to a tenth of a
            // stop from most of one. The moments only bring it closer: they
            // turn contrast by one power on luminance, a power moves the
            // mean of what it is applied to, and over ground of six stops
            // that leaves half a stop of tone behind. Stated here because
            // it is the difference between the two.
            let close = if measure == Measure::Curves { 0.1 } else { 0.7 };
            for c in 0..3 {
                assert!(
                    left[c].abs() < close && left[c].abs() < found[c].abs(),
                    "{measure:?} tone: {found:?} → {left:?}"
                );
            }
            if measure == Measure::Curves {
                // The curve: what was read of it is brought to a third.
                let apart = |v: &[f32]| {
                    let read: Vec<f32> = v[3..].iter().copied().filter(|x| x.is_finite()).collect();
                    read.iter().map(|x| x.abs()).sum::<f32>() / read.len().max(1) as f32
                };
                assert!(apart(&found) > 0.1, "{found:?}");
                assert!(apart(&left) < 0.35 * apart(&found), "{found:?} → {left:?}");
            }
        }
    }

    /// A tile that is the ground through the line `ground = GAIN × tile +
    /// BIAS` the other way, set against that ground.
    fn against_the_ground() -> Observed {
        let mut seen = tile(5, |c| [0, 1, 2].map(|k| (c[k] - BIAS[k]) / GAIN[k]));
        let ground = tile(5, |c| c).paired.expect("its places").tile;
        seen.set_against(|u, v, _, _| {
            Some(ground[(v * PAIRS as f32) as usize * PAIRS + (u * PAIRS as f32) as usize])
        });
        let mut observed = Observed::default();
        observed.tiles.insert((13, 100, 200), seen);
        observed
    }

    const GAIN: [f32; 3] = [1.5, 1.2, 0.8];
    const BIAS: [f32; 3] = [0.003, 0.001, 0.002];

    #[test]
    fn a_tile_is_measured_by_its_line_against_what_it_was_set_against() {
        let observed = against_the_ground();
        // Whatever the level asked for: the reference is not a tile.
        let found = Measure::Linear
            .of(&observed, (13, 100, 200), 0)
            .expect("measured");
        assert_eq!(found.len(), Measure::Linear.len());
        assert_eq!(
            &Measure::Linear.names()[..3],
            ["tone_r", "tone_g", "tone_b"]
        );
        for k in 0..3 {
            assert!((found[k] + GAIN[k].log2()).abs() < 0.03, "{found:?}");
            assert!((found[3 + k] + BIAS[k]).abs() < 0.001, "{found:?}");
        }
        // A tile that was set against nothing is not measured.
        let mut alone = Observed::default();
        alone.tiles.insert((13, 1, 1), tile(5, |c| c));
        assert_eq!(Measure::Linear.of(&alone, (13, 1, 1), 0), None);
    }

    #[test]
    fn the_correction_of_a_line_lays_the_tile_on_its_reference() {
        let observed = against_the_ground();
        let found = Measure::Linear
            .of(&observed, (13, 100, 200), 0)
            .expect("measured");
        let given: Vec<f32> = found.iter().map(|v| -v).collect();
        let local = Measure::Linear.local(&given, -3.0);
        for ground in [[0.02f32, 0.03, 0.025], [0.2, 0.15, 0.1], [0.08, 0.1, 0.05]] {
            let stored = [0, 1, 2].map(|k| (ground[k] - BIAS[k]) / GAIN[k]);
            let shown = local.apply(stored);
            for k in 0..3 {
                assert!(
                    (shown[k] - ground[k]).abs() < 0.02 * ground[k] + 0.0005,
                    "{stored:?} → {shown:?}, wanted {ground:?}"
                );
            }
        }
    }

    #[test]
    fn a_correction_is_held_within_its_bounds_and_kept_to_its_step() {
        let limits = Limits::default();
        for measure in [Measure::Moments, Measure::Curves, Measure::Linear] {
            let mut given = vec![9.0f32; measure.len()];
            given[2] = -9.0;
            measure.within(&mut given, &limits);
            measure.keep(&mut given);
            let light = light_of(Measure::tone(&given));
            assert!(light.abs() <= limits.light_stops + 1.0 / 32.0, "{given:?}");
            for (value, name) in given.iter().zip(measure.names()).skip(3) {
                let bound = match *name {
                    "black" | "black_r" | "black_g" | "black_b" => limits.black,
                    "contrast" | "saturation" => limits.shape_stops,
                    _ => limits.curve_stops,
                };
                assert!(value.abs() <= bound + 1.0 / 64.0, "{name}: {value}");
            }
            // Nothing asked for is nothing done.
            assert!(measure.local(&vec![0.0; measure.len()], -3.0).is_identity());
        }
    }
}
