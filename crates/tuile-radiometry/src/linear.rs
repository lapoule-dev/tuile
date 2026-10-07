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
//! The gain carries tone and cast; the bias carries the black point; the
//! three lines against one another carry contrast and saturation.

use nalgebra::{Matrix2, Vector2};

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
}

/// Fewer places than this say nothing of a tile.
const LEAST: usize = 32;
/// Over this, a value is burnt out: what it was is not known.
const BURNT: f32 = 0.95;
/// Under this luminance, a place is the dark: deep water, deep shadow.
const DARK: f32 = 0.002;
/// How strongly a bias is held to nothing, against the places' own say:
/// only enough for a tile of one flat tone, where a line has no slope to
/// be read, to be given a gain alone.
const HELD: f64 = 0.01;
/// Rounds of weighing the places again.
const ROUNDS: usize = 12;

/// Whether a place can be compared at all.
fn ground(c: [f32; 3]) -> bool {
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

    /// The line, fitted robustly. `None` for a tile with too few places to
    /// say, or whose line comes out as no picture's could.
    pub fn line(&self) -> Option<Line> {
        let places: Vec<([f32; 3], [f32; 3])> = self.places().collect();
        if places.len() < LEAST {
            return None;
        }
        let (mut gain, mut bias) = ([1.0f32; 3], [0.0f32; 3]);
        for band in 0..3 {
            let pairs: Vec<(f64, f64)> = places
                .iter()
                .map(|(t, r)| (f64::from(t[band]), f64::from(r[band])))
                .collect();
            let (a, b) = fitted(&pairs)?;
            (gain[band], bias[band]) = (a as f32, b as f32);
        }
        Some(Line {
            gain,
            bias,
            places: places.len(),
        })
    }
}

/// `y = a·x + b` over pairs `(x, y)`, least squares weighed again each
/// round by how far a pair stands from the line (Cauchy, on the middle
/// residual): ground that changed between the two pictures counts for
/// less and less.
fn fitted(pairs: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = pairs.len() as f64;
    let mut weights = vec![1.0f64; pairs.len()];
    let (mut a, mut b) = (1.0f64, 0.0f64);
    for _ in 0..ROUNDS {
        let (mut sxx, mut sx, mut sw, mut sxy, mut sy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for ((x, y), w) in pairs.iter().zip(&weights) {
            sxx += w * x * x;
            sx += w * x;
            sw += w;
            sxy += w * x * y;
            sy += w * y;
        }
        // The bias is held to nothing by a little: see `HELD`.
        let normal = Matrix2::new(sxx, sx, sx, sw + HELD * n);
        let found = normal.cholesky()?.solve(&Vector2::new(sxy, sy));
        (a, b) = (found[0], found[1]);
        let mut apart: Vec<f64> = pairs.iter().map(|(x, y)| (y - a * x - b).abs()).collect();
        let residuals = apart.clone();
        apart.sort_by(f64::total_cmp);
        // Never so tight that the step of a stored byte is an outlier.
        let scale = (apart[apart.len() / 2] * 2.0).max(1e-3);
        for (w, r) in weights.iter_mut().zip(residuals) {
            *w = 1.0 / (1.0 + (r / scale) * (r / scale));
        }
    }
    // A picture is not laid on another by turning it over, nor by a gain
    // of sixteen.
    (a.is_finite() && b.is_finite() && (1.0 / 16.0..=16.0).contains(&a)).then_some((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: u32, k: usize) -> f32 {
        let mut h = seed ^ (k as u32).wrapping_mul(0x9E37_79B9);
        h ^= h >> 15;
        h = h.wrapping_mul(0x85EB_CA6B);
        h ^= h >> 13;
        (h & 0xFFFF) as f32 / 65535.0
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
}
