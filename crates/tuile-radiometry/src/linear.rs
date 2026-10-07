// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A tile set against a reference picture of the same ground, place for
//! place, and the line that lays one on the other.
//!
//! This is relative radiometric normalisation as a mosaic is commonly
//! given it: a homogeneous reference — a composite of many dates, one
//! rendering over the whole ground — and, a band at a time, the linear
//! model `reference = gain × tile + bias` fitted on the places where both
//! show ground that can be compared. Places that cannot are left out before
//! the fit (snow and cloud, burnt-out, near-black); places where the ground
//! has changed between the two pictures are weighed down by it, round after
//! round.
//!
//! Three things about the fit, each of which was got wrong once:
//!
//! - **the tile is no more exact than the reference.** Another season,
//!   another hour, a texel's misplacement: both pictures stand off the
//!   ground they share. Least squares of one on the other takes the first
//!   for exact, and its slope comes out too low by as much as the two fail
//!   to agree — two tiles of one capture are then given two gains, by how
//!   much texture each has. So the line is the one that lies closest to the
//!   places taking both as uncertain: the main axis of their scatter
//!   (orthogonal regression), once the tile is brought to the reference's
//!   light so that a distance means the same along both;
//! - **a slope is only read where there is one to read.** Where the two
//!   pictures hardly agree, the scatter has no axis worth the name, and the
//!   line is drawn back to the one through nothing: a gain alone, the ratio
//!   of the two lights. Either way the line goes through the middle of the
//!   places, so the tile's tone — its light against the reference's — is
//!   never the slope's to spoil;
//! - **a place is weighed once, for the three bands.** Ground that changed
//!   changed in all of them; a place kept in red and left out in green
//!   would pull the cast.
//!
//! The gain carries tone and cast; the bias carries the black point; the
//! three lines against one another carry contrast and saturation.

use nalgebra::{Matrix2, SymmetricEigen};

use crate::tiles::{luma, snow};

/// Places along a side of a tile at which it is set against the reference.
pub const PAIRS: usize = 16;

/// A tile and the reference under it: the same ground, in [`PAIRS`]² places
/// of linear light, row by row. A place either has none of is not a number.
#[derive(Debug, Clone, PartialEq)]
pub struct Paired {
    pub tile: [[f32; 3]; PAIRS * PAIRS],
    pub reference: [[f32; 3]; PAIRS * PAIRS],
}

/// The line that lays a tile on its reference, a band at a time:
/// `reference = gain × tile + bias`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    pub gain: [f32; 3],
    pub bias: [f32; 3],
    /// Places it was fitted on.
    pub places: usize,
    /// How well the two pictures agree over them, a band at a time: the
    /// correlation of tile and reference over all of them, each for one.
    pub agreement: [f32; 3],
    /// The tile's light over the reference's, a band at a time: the gain a
    /// line through nothing would have. The tile's tone.
    pub ratio: [f32; 3],
    /// How much of the slope read off the places is in the gain, 0 to 1:
    /// the rest is the ratio's.
    pub trust: [f32; 3],
}

/// A line and what it was fitted on.
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    pub line: Line,
    /// The places, tile then reference, and what each ended up weighing —
    /// one weight for its three bands.
    pub places: Vec<([f32; 3], [f32; 3], f32)>,
}

/// Fewer places than this say nothing of a tile.
const LEAST: usize = 32;
/// Over this, a value is burnt out: what it was is not known.
const BURNT: f32 = 0.95;
/// Under this luminance, a place is the dark: deep water, deep shadow.
const DARK: f32 = 0.002;
/// Rounds of weighing the places again.
const ROUNDS: usize = 12;
/// Under this agreement no slope is read at all; over [`AGREED`] it is read
/// whole; between, in part.
const UNREAD: f64 = 0.5;
/// A place this many middle distances from the line weighs half: the
/// usual width of a Cauchy weight, for distances spread as a bell.
const WIDE: f64 = 3.5;
const AGREED: f64 = 0.85;

/// Whether a place can be compared at all.
pub(crate) fn ground(c: [f32; 3]) -> bool {
    c.iter().all(|v| v.is_finite() && *v < BURNT) && luma(c) > DARK && !snow(c)
}

impl Paired {
    /// A tile's places, with no reference under them yet.
    pub fn alone(tile: [[f32; 3]; PAIRS * PAIRS]) -> Self {
        Self {
            tile,
            reference: [[f32::NAN; 3]; PAIRS * PAIRS],
        }
    }

    /// The places both pictures show ground that can be compared.
    pub fn places(&self) -> impl Iterator<Item = ([f32; 3], [f32; 3])> + '_ {
        self.tile
            .iter()
            .zip(&self.reference)
            .filter(|(t, r)| ground(**t) && ground(**r))
            .map(|(t, r)| (*t, *r))
    }

    /// The line: see the module. `None` for a tile with too few places to
    /// say, or whose line comes out as no picture's could.
    pub fn line(&self) -> Option<Line> {
        self.fit().map(|fit| fit.line)
    }

    /// [`Self::line`], and the places with what each weighed.
    pub fn fit(&self) -> Option<Fit> {
        let places: Vec<([f32; 3], [f32; 3])> = self.places().collect();
        if places.len() < LEAST {
            return None;
        }
        let mut weights = vec![1.0f64; places.len()];
        let mut bands = [Band::default(); 3];
        for round in 0..ROUNDS {
            for (band, found) in bands.iter_mut().enumerate() {
                // How well the two agree is said by all the places, each
                // for one: weighed, the places kept are those that agree.
                let agreement = (round > 0).then_some(found.agreement);
                *found = Band::of(
                    places
                        .iter()
                        .zip(&weights)
                        .map(|((t, r), w)| (f64::from(t[band]), f64::from(r[band]), *w)),
                    agreement,
                )?;
            }
            // How far each place stands from the three lines — across the
            // line, not down from it: a distance taken down from a line
            // keeps the places that flatter its slope. Each band by its own
            // middle distance, never so tight that the step of a stored
            // byte is an outlier; then one weight for the three.
            let apart = |band: usize, t: &[f32; 3], r: &[f32; 3]| {
                let b = &bands[band];
                let slope = b.gain / b.ratio;
                (f64::from(r[band]) - b.gain * f64::from(t[band]) - b.bias).abs()
                    / (1.0 + slope * slope).sqrt()
            };
            let scales: [f64; 3] = std::array::from_fn(|band| {
                let mut all: Vec<f64> = places.iter().map(|(t, r)| apart(band, t, r)).collect();
                all.sort_by(f64::total_cmp);
                (all[all.len() / 2] * WIDE).max(1e-3)
            });
            for (w, (t, r)) in weights.iter_mut().zip(&places) {
                let far: f64 = (0..3)
                    .map(|band| (apart(band, t, r) / scales[band]).powi(2))
                    .sum::<f64>()
                    / 3.0;
                *w = 1.0 / (1.0 + far);
            }
        }
        // A picture is not laid on another by turning it over, nor by a
        // gain of sixteen.
        if !bands.iter().all(|b| {
            b.gain.is_finite() && b.bias.is_finite() && (1.0 / 16.0..=16.0).contains(&b.gain)
        }) {
            return None;
        }
        Some(Fit {
            line: Line {
                gain: bands.map(|b| b.gain as f32),
                bias: bands.map(|b| b.bias as f32),
                places: places.len(),
                agreement: bands.map(|b| b.agreement as f32),
                ratio: bands.map(|b| b.ratio as f32),
                trust: bands.map(|b| b.trust as f32),
            },
            places: places
                .iter()
                .zip(&weights)
                .map(|((t, r), w)| (*t, *r, *w as f32))
                .collect(),
        })
    }
}

/// One band's line.
#[derive(Debug, Clone, Copy, Default)]
struct Band {
    gain: f64,
    bias: f64,
    agreement: f64,
    ratio: f64,
    trust: f64,
}

impl Band {
    /// `y = gain·x + bias` over weighed places `(x, y, weight)`: see the
    /// module. `agreement` is how well the two agree if that is known
    /// already; else it is read off these places as they are weighed.
    /// `None` if there is no light to speak of.
    fn of(
        places: impl Iterator<Item = (f64, f64, f64)> + Clone,
        agreement: Option<f64>,
    ) -> Option<Self> {
        let (mut sw, mut sx, mut sy) = (0.0, 0.0, 0.0);
        for (x, y, w) in places.clone() {
            sw += w;
            sx += w * x;
            sy += w * y;
        }
        let (mx, my) = (sx / sw, sy / sw);
        if !(mx > 0.0 && my > 0.0) {
            return None;
        }
        // The line through nothing: the reference's light over the tile's.
        let ratio = my / mx;
        // The scatter about its middle, the tile brought to the reference's
        // light: a distance is then the same along both, and the main axis
        // is the line that lies closest to the places taking both pictures
        // as uncertain.
        let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
        for (x, y, w) in places {
            let (dx, dy) = (x * ratio - my, y - my);
            sxx += w * dx * dx;
            syy += w * dy * dy;
            sxy += w * dx * dy;
        }
        let agreement = agreement.unwrap_or(if sxx > 0.0 && syy > 0.0 {
            sxy / (sxx * syy).sqrt()
        } else {
            0.0
        });
        let trust = ((agreement - UNREAD) / (AGREED - UNREAD)).clamp(0.0, 1.0);
        let trust = trust * trust * (3.0 - 2.0 * trust);
        let mut slope = 1.0f64;
        if trust > 0.0 {
            let axes = SymmetricEigen::new(Matrix2::new(sxx, sxy, sxy, syy));
            let main = if axes.eigenvalues[0] >= axes.eigenvalues[1] {
                0
            } else {
                1
            };
            let axis = axes.eigenvectors.column(main);
            if axis[0].abs() > 1e-9 && axis[1] / axis[0] > 0.0 {
                // In stops, so that half the trust is half the way.
                slope = (trust * (axis[1] / axis[0]).log2()).exp2();
            }
        }
        let gain = ratio * slope;
        Some(Self {
            gain,
            bias: my - gain * mx,
            agreement,
            ratio,
            trust,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noise from 0 to 1, a value a place: two seeds give two noises that
    /// have nothing to do with one another.
    fn noise(seed: u32, k: usize) -> f32 {
        let mut h = seed.wrapping_mul(0x9E37_79B9) ^ (k as u32).wrapping_mul(0x85EB_CA6B);
        for _ in 0..2 {
            h ^= h >> 16;
            h = h.wrapping_mul(0x7FEB_352D);
            h ^= h >> 15;
            h = h.wrapping_mul(0x846C_A68B);
            h ^= h >> 16;
        }
        (h >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Ground of mixed tones and colours, and over it the same ground as
    /// `through` makes it.
    fn paired(through: impl Fn(usize, [f32; 3]) -> [f32; 3]) -> Paired {
        let tile: [[f32; 3]; PAIRS * PAIRS] = std::array::from_fn(|k| {
            let light = 0.02 + 0.25 * noise(1, k);
            [
                light * (0.8 + 0.4 * noise(2, k)),
                light,
                light * (0.6 + 0.4 * noise(3, k)),
            ]
        });
        Paired {
            reference: std::array::from_fn(|k| through(k, tile[k])),
            tile,
        }
    }

    const GAIN: [f32; 3] = [1.6, 1.3, 0.8];
    const BIAS: [f32; 3] = [0.012, -0.004, 0.02];

    fn laid(c: [f32; 3]) -> [f32; 3] {
        std::array::from_fn(|i| GAIN[i] * c[i] + BIAS[i])
    }

    fn close(line: &Line, by: f32) {
        for band in 0..3 {
            assert!(
                (line.gain[band] - GAIN[band]).abs() < by * GAIN[band]
                    && (line.bias[band] - BIAS[band]).abs() < by * 0.1,
                "band {band}: {line:?}"
            );
        }
    }

    #[test]
    fn the_gain_and_the_bias_of_a_tile_are_found() {
        let line = paired(|_, c| laid(c)).line().expect("a line");
        close(&line, 0.02);
        assert_eq!(line.places, PAIRS * PAIRS);
        assert!(line.agreement.iter().all(|a| *a > 0.99), "{line:?}");
        assert!(line.trust.iter().all(|t| *t == 1.0), "{line:?}");
    }

    #[test]
    fn snow_on_the_tile_does_not_move_the_line() {
        // A third of the tile under snow, which the reference — a composite
        // of the whole year — shows as bare ground.
        let mut snowed = paired(|_, c| laid(c));
        for k in (0..PAIRS * PAIRS).filter(|k| k % 3 == 0) {
            snowed.tile[k] = [0.8, 0.8, 0.82];
        }
        let line = snowed.line().expect("a line");
        close(&line, 0.02);
        assert!(line.places < PAIRS * PAIRS * 3 / 4, "{}", line.places);
    }

    #[test]
    fn ground_that_changed_over_a_quarter_of_the_tile_does_not_move_the_line() {
        // A quarter of the tile was fields in another state: not snow, not
        // burnt — nothing a mask knows — and twice as light on the tile.
        let changed = paired(|k, c| {
            if k % PAIRS < PAIRS / 2 && k / PAIRS < PAIRS / 2 {
                laid(c.map(|v| v * 0.5))
            } else {
                laid(c)
            }
        });
        close(&changed.line().expect("a line"), 0.08);
    }

    #[test]
    fn a_tile_with_too_little_ground_to_compare_has_no_line() {
        let mut bare = paired(|_, c| laid(c));
        // Thirty-one places of two hundred and fifty-six.
        for k in 31..PAIRS * PAIRS {
            bare.reference[k] = [f32::NAN; 3];
        }
        assert_eq!(bare.line(), None);
    }

    #[test]
    fn a_tile_of_one_flat_tone_is_given_a_gain_and_no_bias() {
        // No slope to read a line from: all there is to say is how much
        // lighter the reference is.
        let flat = Paired {
            tile: [[0.1, 0.1, 0.1]; PAIRS * PAIRS],
            reference: [[0.15, 0.15, 0.15]; PAIRS * PAIRS],
        };
        let line = flat.line().expect("a line");
        for band in 0..3 {
            assert!((line.gain[band] - 1.5).abs() < 0.05, "{line:?}");
            assert!(line.bias[band].abs() < 0.005, "{line:?}");
        }
    }
    #[test]
    fn a_tile_as_uncertain_as_its_reference_is_not_given_too_low_a_gain() {
        // Both pictures stand off the ground they share by as much: the
        // tile is the ground with noise on it, the reference is the ground
        // through the line with noise of its own. Least squares of the one
        // on the other would give a slope a fifth too low.
        let ground = paired(|_, c| c).tile;
        let uncertain = Paired {
            tile: std::array::from_fn(|k| {
                std::array::from_fn(|i| ground[k][i] * (0.75 + 0.5 * noise(7 + i as u32, k)))
            }),
            reference: std::array::from_fn(|k| {
                let laid = laid(ground[k]);
                std::array::from_fn(|i| laid[i] * (0.75 + 0.5 * noise(17 + i as u32, k)))
            }),
        };
        let line = uncertain.line().expect("a line");
        for band in 0..3 {
            assert!(line.agreement[band] < 0.95, "{line:?}");
            assert!(
                (line.gain[band] / GAIN[band]).log2().abs() < 0.12,
                "band {band}: {line:?}"
            );
        }
    }

    #[test]
    fn where_the_two_pictures_do_not_agree_the_line_is_a_gain_alone() {
        // The reference shows other ground altogether: there is no slope
        // to read, and what is left is how much lighter it is.
        let unlike = Paired {
            tile: std::array::from_fn(|k| [0.05 + 0.1 * noise(1, k); 3]),
            reference: std::array::from_fn(|k| [0.1 + 0.2 * noise(2, k); 3]),
        };
        let line = unlike.line().expect("a line");
        for band in 0..3 {
            assert!(line.agreement[band].abs() < 0.3, "{line:?}");
            assert_eq!(line.trust[band], 0.0);
            assert!(line.bias[band].abs() < 1e-6, "{line:?}");
            assert!(
                (line.gain[band] - line.ratio[band]).abs() < 1e-6,
                "{line:?}"
            );
            assert!((line.gain[band] - 2.0).abs() < 0.2, "{line:?}");
        }
    }

    #[test]
    fn a_place_that_changed_in_one_band_weighs_less_in_all_three() {
        // One place in eight is far redder on the reference — not burnt,
        // nothing a mask knows — and as it was in green and blue. It is
        // weighed down as a place, not as a red: there is one weight for
        // the three.
        let fit = paired(|k, c| {
            let mut laid = laid(c);
            if k % 8 == 0 {
                laid[0] += 0.25;
            }
            laid
        })
        .fit()
        .expect("a fit");
        let mean = |keep: &dyn Fn(usize) -> bool| {
            let kept: Vec<f32> = fit
                .places
                .iter()
                .enumerate()
                .filter(|(k, _)| keep(*k))
                .map(|(_, p)| p.2)
                .collect();
            kept.iter().sum::<f32>() / kept.len() as f32
        };
        let (changed, others) = (mean(&|k| k % 8 == 0), mean(&|k| k % 8 != 0));
        assert!(changed < 0.5 * others, "{changed} against {others}");
        // Green, where nothing changed, is still found.
        assert!(
            (fit.line.gain[1] - GAIN[1]).abs() < 0.03 * GAIN[1],
            "{:?}",
            fit.line
        );
    }

    #[test]
    fn how_well_two_pictures_agree_is_said_by_all_their_places() {
        // Half the places are other ground on the reference. The half that
        // agree would, weighed alone, agree perfectly — and a slope read
        // off half a tile, picked for agreeing, is not the tile's.
        let line = paired(|k, c| {
            if k % 2 == 0 {
                laid(c)
            } else {
                [0.05 + 0.3 * noise(9, k); 3]
            }
        })
        .line()
        .expect("a line");
        for band in 0..3 {
            assert!(line.agreement[band] < 0.8, "{line:?}");
            assert!(line.trust[band] < 1.0, "{line:?}");
        }
    }
}
