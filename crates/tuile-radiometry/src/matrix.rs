// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A fitted function a tile: where in the tile, and what colour, in — the
//! colour to draw, out.
//!
//! ```text
//! f(u, v, colour) = M(u, v) · [r, g, b, 1]
//! ```
//!
//! `M` is a colour matrix — three rows of four, a channel out for each, the
//! last column what is added — and it is carried by the four corners of the
//! tile and blended between them. Two tiles that share an edge share the
//! two matrices along it, so the function is one function across the edge:
//! tiles that meet stay met, whatever the fit finds. Only across a seam —
//! an edge where the film steps and the reference does not — do the two
//! sides have corners of their own.
//!
//! The matrices of a whole film are found at once, by least squares over
//! every place of every tile ([`crate::Paired`]): what the function makes of
//! the film's colour there, against what is wanted there. Sparse — a place
//! touches four corners — and solved as such. Places are weighed again,
//! round after round, by how far the function leaves them: ground that
//! changed between the two pictures counts for less.
//!
//! **What is wanted** is not the reference's colour. A film keeps the look
//! of its own imagery; the reference is there for being one picture where
//! the film is many. So the reference is first given the film's look — one
//! matrix a zone for the whole film, ground and water, fitted the same way
//! ([`Look`]) — and that, with a dose of the reference's own left in, is
//! what every tile is brought to.

use std::collections::BTreeMap;

use nalgebra::DMatrix;
use nalgebra_sparse::factorization::CscCholesky;
use nalgebra_sparse::{CooMatrix, CscMatrix};
use petgraph::unionfind::UnionFind;

use crate::linear::{ground, water, Paired, PAIRS};
use crate::tiles::{luma, snow, Observed, TileAt};

/// A colour matrix: a row a channel out; red, green, blue in, then what is
/// added.
pub type Affine = [[f32; 4]; 3];

/// Changes nothing.
pub const SAME: Affine = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// A colour through a matrix. Not clamped.
pub fn through(matrix: &Affine, colour: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|o| {
        matrix[o][0] * colour[0]
            + matrix[o][1] * colour[1]
            + matrix[o][2] * colour[2]
            + matrix[o][3]
    })
}

/// How a field of matrices is fitted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatrixBounds {
    /// How much of the reference's own look is left in what the film is
    /// brought to, 0 to 1: at 0 the film keeps the look of its imagery
    /// whole, at 1 it takes the reference's. [`MatrixBounds::DOSE`] unless
    /// told.
    pub toward: f32,
    /// How strongly a corner's matrix is held to changing nothing, against
    /// a tile's places fully believed.
    pub held: f32,
    /// How strongly two corners of a tile are held to the same matrix.
    pub smooth: f32,
    /// A seam begins where the film steps across an edge by more than this
    /// and the reference does not…
    pub seam_stops: f32,
    /// …and goes on along edges that touch it and step by more than this.
    pub seam_linked_stops: f32,
    /// A seam of fewer edges than this is not one.
    pub least_seam: usize,
}

impl MatrixBounds {
    /// The dose of the reference's look a film takes when nothing says:
    /// three tenths, chosen by eye on a first film — more, and the film is
    /// the reference's green; less, and it is its imagery's pallor.
    pub const DOSE: f32 = 0.3;
}

impl Default for MatrixBounds {
    fn default() -> Self {
        Self {
            toward: Self::DOSE,
            held: 0.02,
            smooth: 0.5,
            seam_stops: 0.6,
            seam_linked_stops: 0.3,
            least_seam: 3,
        }
    }
}

/// The film's look, as what it makes of the reference: a matrix for ground
/// and one for water — a gain and what is added, a band at a time — each
/// from the tiles of the film that are that zone for the most part.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub ground: Affine,
    pub water: Affine,
}

impl Look {
    /// What a tile is brought to at a place whose reference is `colour`:
    /// the reference in the film's look, with `toward` of its own left in.
    pub fn wanted(&self, colour: [f32; 3], toward: f32) -> [f32; 3] {
        let zone = if water(colour) {
            &self.water
        } else {
            &self.ground
        };
        let looked = through(zone, colour);
        std::array::from_fn(|c| (toward * colour[c] + (1.0 - toward) * looked[c]).max(0.0))
    }
}

/// What a fit was made of and what it leaves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatrixReport {
    pub tiles: usize,
    /// Tiles with places to fit on.
    pub measured: usize,
    pub places: usize,
    pub corners: usize,
    pub edges: usize,
    pub seam_edges: usize,
    /// The seams: the tile on the near side of each edge, and whether the
    /// far side is to its right (else below it).
    pub seams: Vec<(TileAt, bool)>,
    /// How far the film stands from what is wanted, in stops of luminance,
    /// as the median and the 95th centile over its places: as it is, and
    /// through the field.
    pub before: (f32, f32),
    pub after: (f32, f32),
}

/// A field of matrices over a film's tiles: for each tile, the matrix at
/// each of its corners — top-left, top-right, bottom-left, bottom-right.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatrixField {
    pub given: BTreeMap<TileAt, [Affine; 4]>,
}

/// Rounds of weighing the places again.
const ROUNDS: usize = 4;

/// A tile's places that can be fitted on, each with where it is in the
/// tile, the film's colour and the reference's.
fn places_of(paired: &Paired) -> Vec<(f32, f32, [f32; 3], [f32; 3])> {
    // A reference may be as dark as a sea is; it may not be burnt, or
    // snow, or missing.
    let shown = |c: [f32; 3]| c.iter().all(|v| v.is_finite() && *v < 0.95) && !snow(c);
    (0..PAIRS * PAIRS)
        .filter(|k| ground(paired.tile[*k]) && shown(paired.reference[*k]))
        .map(|k| {
            (
                ((k % PAIRS) as f32 + 0.5) / PAIRS as f32,
                ((k / PAIRS) as f32 + 0.5) / PAIRS as f32,
                paired.tile[k],
                paired.reference[k],
            )
        })
        .collect()
}

fn centiles(values: &mut [f32]) -> (f32, f32) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    values.sort_by(f32::total_cmp);
    (
        values[values.len() / 2],
        values[(values.len() * 95 / 100).min(values.len() - 1)],
    )
}

impl MatrixField {
    /// A tile's matrix at a place in it, `u` across and `v` down, 0 to 1.
    /// The identity for a tile the field does nothing to.
    pub fn at(&self, tile: TileAt, u: f32, v: f32) -> Affine {
        let Some(corners) = self.given.get(&tile) else {
            return SAME;
        };
        let weights = [(1.0 - u) * (1.0 - v), u * (1.0 - v), (1.0 - u) * v, u * v];
        std::array::from_fn(|o| {
            std::array::from_fn(|i| (0..4).map(|q| weights[q] * corners[q][o][i]).sum())
        })
    }

    /// The film's look: see [`Look`].
    ///
    /// A band at a time, from the lines that lay each tile on the
    /// reference ([`Paired::fit`]): the middle of them over the tiles of a
    /// zone, turned round — what the reference is made into, to look as
    /// the film does. Not a matrix fitted of the film on the reference:
    /// least squares of one picture on another gives a slope too low by as
    /// much as the two fail to agree, and a look fitted so is the film
    /// with its contrast pressed out.
    pub fn look(observed: &Observed) -> Look {
        // Per zone and band: the stops of a line's gain, and its bias.
        let mut lines: [[(Vec<f32>, Vec<f32>); 3]; 2] = Default::default();
        for tile in observed.tiles.values() {
            let Some(line) = tile.paired.as_deref().and_then(Paired::line) else {
                continue;
            };
            for band in 0..3 {
                let of = &mut lines[usize::from(line.water)][band];
                of.0.push(line.gain[band].log2());
                of.1.push(line.bias[band]);
            }
        }
        let middle = |values: &mut Vec<f32>| {
            values.sort_by(f32::total_cmp);
            values.get(values.len() / 2).copied()
        };
        let mut zones = [SAME; 2];
        for (zone, of) in zones.iter_mut().zip(&mut lines) {
            for band in 0..3 {
                let (Some(gain), Some(bias)) = (middle(&mut of[band].0), middle(&mut of[band].1))
                else {
                    continue;
                };
                // reference = gain × film + bias, turned round.
                let gain = gain.exp2();
                zone[band][band] = 1.0 / gain;
                zone[band][3] = -bias / gain;
            }
        }
        Look {
            ground: zones[0],
            water: zones[1],
        }
    }

    /// Fits the field for a film: see the module.
    pub fn solve(observed: &Observed, bounds: &MatrixBounds) -> (Self, MatrixReport, Look) {
        let look = Self::look(observed);
        let at: Vec<TileAt> = observed.tiles.keys().copied().collect();
        let n = at.len();
        let index: BTreeMap<TileAt, usize> = at.iter().enumerate().map(|(i, a)| (*a, i)).collect();
        // Every tile's places: where, the film's colour, what is wanted.
        let places: Vec<Vec<(f32, f32, [f32; 3], [f32; 3])>> = at
            .iter()
            .map(|a| {
                observed.tiles[a].paired.as_deref().map_or(Vec::new(), |p| {
                    places_of(p)
                        .into_iter()
                        .map(|(u, v, film, reference)| {
                            (u, v, film, look.wanted(reference, bounds.toward))
                        })
                        .collect()
                })
            })
            .collect();
        let mut report = MatrixReport {
            tiles: n,
            measured: places.iter().filter(|p| !p.is_empty()).count(),
            places: places.iter().map(Vec::len).sum(),
            ..MatrixReport::default()
        };
        if report.places == 0 {
            return (Self::default(), report, look);
        }

        // Edges, and the seams among them: where the film steps across an
        // edge and the reference does not, all along it.
        struct Edge {
            a: usize,
            b: usize,
            upright: bool,
            apart: f32,
        }
        let mut edges = Vec::new();
        for (i, (level, x, y)) in at.iter().enumerate() {
            for (next, upright) in [((*level, x + 1, *y), true), ((*level, *x, y + 1), false)] {
                if let Some(j) = index.get(&next).copied() {
                    let apart = observed
                        .junction_against(at[i], at[j], upright)
                        .map_or(0.0, |met| met.apart());
                    edges.push(Edge {
                        a: i,
                        b: j,
                        upright,
                        apart,
                    });
                }
            }
        }
        report.edges = edges.len();
        // The two ends of an edge, as corners of its level's grid.
        let ends = |e: &Edge| {
            let (level, x, y) = at[e.a];
            if e.upright {
                [(level, x + 1, y), (level, x + 1, y + 1)]
            } else {
                [(level, x, y + 1), (level, x + 1, y + 1)]
            }
        };
        let mut meeting: BTreeMap<TileAt, Vec<usize>> = BTreeMap::new();
        for (k, e) in edges.iter().enumerate() {
            for end in ends(e) {
                meeting.entry(end).or_default().push(k);
            }
        }
        let mut seam: Vec<bool> = edges.iter().map(|e| e.apart > bounds.seam_stops).collect();
        let mut front: Vec<usize> = (0..edges.len()).filter(|k| seam[*k]).collect();
        while let Some(k) = front.pop() {
            for end in ends(&edges[k]) {
                for j in &meeting[&end] {
                    if !seam[*j] && edges[*j].apart > bounds.seam_linked_stops {
                        seam[*j] = true;
                        front.push(*j);
                    }
                }
            }
        }
        // Seam edges that touch are one seam; one of too few is none.
        let mut seams = UnionFind::<usize>::new(edges.len());
        for touching in meeting.values() {
            let cut: Vec<usize> = touching.iter().copied().filter(|k| seam[*k]).collect();
            for pair in cut.windows(2) {
                seams.union(pair[0], pair[1]);
            }
        }
        let mut length: BTreeMap<usize, usize> = BTreeMap::new();
        for k in (0..edges.len()).filter(|k| seam[*k]) {
            *length.entry(seams.find(k)).or_default() += 1;
        }
        for k in 0..edges.len() {
            if seam[k] && length[&seams.find(k)] < bounds.least_seam {
                seam[k] = false;
            }
        }
        report.seam_edges = seam.iter().filter(|s| **s).count();
        report.seams = edges
            .iter()
            .zip(&seam)
            .filter(|(_, s)| **s)
            .map(|(e, _)| (at[e.a], e.upright))
            .collect();

        // The unknowns: a tile's four corners, made one with its
        // neighbour's along every edge that is not a seam.
        let slot = |tile: usize, corner: usize| tile * 4 + corner;
        let mut same = UnionFind::<usize>::new(n * 4);
        for (k, e) in edges.iter().enumerate() {
            if seam[k] {
                continue;
            }
            if e.upright {
                same.union(slot(e.a, 1), slot(e.b, 0));
                same.union(slot(e.a, 3), slot(e.b, 2));
            } else {
                same.union(slot(e.a, 2), slot(e.b, 0));
                same.union(slot(e.a, 3), slot(e.b, 1));
            }
        }
        let mut node_of: BTreeMap<usize, usize> = BTreeMap::new();
        let node: Vec<usize> = (0..n * 4)
            .map(|s| {
                let next = node_of.len();
                *node_of.entry(same.find(s)).or_insert(next)
            })
            .collect();
        let nodes = node_of.len();
        report.corners = nodes;

        // A colour as the fit is given it: its three channels and a one,
        // the one brought to the size of the film's light so that what is
        // held and what is smoothed weigh the same on all four.
        let light = {
            let (mut sum, mut count) = (0.0f64, 0usize);
            for (_, _, film, _) in places.iter().flatten() {
                sum += f64::from(luma(*film));
                count += 1;
            }
            (sum / count.max(1) as f64).max(1e-3)
        };
        let seen = |film: [f32; 3]| {
            [
                f64::from(film[0]),
                f64::from(film[1]),
                f64::from(film[2]),
                light,
            ]
        };
        let blend = |u: f32, v: f32| {
            [
                f64::from((1.0 - u) * (1.0 - v)),
                f64::from(u * (1.0 - v)),
                f64::from((1.0 - u) * v),
                f64::from(u * v),
            ]
        };
        // What a tile fully believed weighs: its places, at the film's
        // light. What is held and what is smoothed are told against it.
        let whole = (PAIRS * PAIRS) as f64 * light * light;
        const SIDES: [(usize, usize); 4] = [(0, 1), (2, 3), (0, 2), (1, 3)];

        let mut weights: Vec<Vec<f64>> = places.iter().map(|p| vec![1.0; p.len()]).collect();
        let mut solved = DMatrix::<f64>::zeros(nodes * 4, 3);
        for i in 0..nodes {
            for c in 0..3 {
                solved[(i * 4 + c, c)] = 1.0;
            }
        }
        let made = |solved: &DMatrix<f64>, tile: usize, u: f32, v: f32, film: [f32; 3]| {
            let (b, f) = (blend(u, v), seen(film));
            let mut out = [0.0f64; 3];
            for q in 0..4 {
                let base = node[slot(tile, q)] * 4;
                for i in 0..4 {
                    for (o, value) in out.iter_mut().enumerate() {
                        *value += b[q] * f[i] * solved[(base + i, o)];
                    }
                }
            }
            out
        };
        for round in 0..ROUNDS {
            let mut normal = CooMatrix::<f64>::new(nodes * 4, nodes * 4);
            let mut right = DMatrix::<f64>::zeros(nodes * 4, 3);
            for tile in 0..n {
                // A tile's places, gathered before they are spread over
                // the whole: sixteen unknowns a tile, however many places.
                let mut block = [[0.0f64; 16]; 16];
                let mut side = [[0.0f64; 3]; 16];
                for ((u, v, film, wanted), w) in places[tile].iter().zip(&weights[tile]) {
                    let (b, f) = (blend(*u, *v), seen(*film));
                    let row: [f64; 16] = std::array::from_fn(|k| b[k / 4] * f[k % 4]);
                    for i in 0..16 {
                        for j in 0..16 {
                            block[i][j] += w * row[i] * row[j];
                        }
                        for o in 0..3 {
                            side[i][o] += w * row[i] * f64::from(wanted[o]);
                        }
                    }
                }
                let unknown = |k: usize| node[slot(tile, k / 4)] * 4 + k % 4;
                if !places[tile].is_empty() {
                    for i in 0..16 {
                        for j in 0..16 {
                            normal.push(unknown(i), unknown(j), block[i][j]);
                        }
                        for o in 0..3 {
                            right[(unknown(i), o)] += side[i][o];
                        }
                    }
                }
                // Held to changing nothing, a corner at a time; and two
                // corners of a tile held to the same.
                let hold = f64::from(bounds.held) * whole / 4.0;
                for k in 0..16 {
                    normal.push(unknown(k), unknown(k), hold);
                    if k % 4 < 3 {
                        right[(unknown(k), k % 4)] += hold;
                    }
                }
                let smooth = f64::from(bounds.smooth) * whole;
                for (p, q) in SIDES {
                    if node[slot(tile, p)] == node[slot(tile, q)] {
                        continue;
                    }
                    for i in 0..4 {
                        let (a, b) = (unknown(p * 4 + i), unknown(q * 4 + i));
                        normal.push(a, a, smooth);
                        normal.push(b, b, smooth);
                        normal.push(a, b, -smooth);
                        normal.push(b, a, -smooth);
                    }
                }
            }
            let Ok(factored) = CscCholesky::factor(&CscMatrix::from(&normal)) else {
                break;
            };
            solved = factored.solve(&right);
            if round + 1 == ROUNDS {
                break;
            }
            // Places weighed again: by how far the function leaves each,
            // against the middle of its own tile's — not of the film's. A
            // capture that stands far from the rest of the film is a
            // capture to be brought in, not a tile of outliers; what is
            // weighed down is a place that stands apart from its tile.
            for tile in 0..n {
                let apart: Vec<f64> = places[tile]
                    .iter()
                    .map(|(u, v, film, wanted)| {
                        let out = made(&solved, tile, *u, *v, *film);
                        (0..3)
                            .map(|c| (out[c] - f64::from(wanted[c])).abs())
                            .fold(0.0, f64::max)
                    })
                    .collect();
                if apart.is_empty() {
                    continue;
                }
                let mut sorted = apart.clone();
                sorted.sort_by(f64::total_cmp);
                let scale = (sorted[sorted.len() / 2] * 3.5).max(1e-3);
                for (w, r) in weights[tile].iter_mut().zip(apart) {
                    *w = 1.0 / (1.0 + (r / scale) * (r / scale));
                }
            }
        }

        // A corner's matrix, the one given back the size it was taken at.
        let corner = |tile: usize, q: usize| -> Affine {
            let base = node[slot(tile, q)] * 4;
            std::array::from_fn(|o| {
                std::array::from_fn(|i| {
                    let value = solved[(base + i, o)] * if i == 3 { light } else { 1.0 };
                    value as f32
                })
            })
        };
        let field = Self {
            given: (0..n)
                .map(|tile| (at[tile], std::array::from_fn(|q| corner(tile, q))))
                .collect(),
        };
        let stops = |c: [f32; 3]| (luma(c).max(0.0) + 0.001).log2();
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for tile in 0..n {
            for (u, v, film, wanted) in &places[tile] {
                before.push((stops(*film) - stops(*wanted)).abs());
                let made = through(&field.at(at[tile], *u, *v), *film);
                after.push((stops(made) - stops(*wanted)).abs());
            }
        }
        report.before = centiles(&mut before);
        report.after = centiles(&mut after);
        (field, report, look)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::TileSeen;

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

    /// Ground of mixed tones and colours at a place of the world.
    fn ground_at(x: u32, y: u32, k: usize) -> [f32; 3] {
        let seed = x * 131 + y * 17;
        let light = 0.03 + 0.2 * noise(seed, k);
        [
            light * (0.7 + 0.5 * noise(seed + 1, k)),
            light,
            // Never bluer than it is red: this is ground, not water.
            light * (0.3 + 0.3 * noise(seed + 2, k)),
        ]
    }

    /// A film `wide` tiles by three: the reference is the ground, a tile is
    /// the ground through `film` (told the tile's column and the place).
    fn seen(wide: u32, film: impl Fn(u32, usize, [f32; 3]) -> [f32; 3]) -> Observed {
        let mut observed = Observed::default();
        for y in 0..3u32 {
            for x in 0..wide {
                let reference: [[f32; 3]; PAIRS * PAIRS] =
                    std::array::from_fn(|k| ground_at(x, y, k));
                let texels = vec![[0.1f32; 3]; 64 * 64];
                let mut tile = TileSeen::of_linear(&texels, 64).expect("a tile");
                tile.paired = Some(Box::new(Paired {
                    tile: std::array::from_fn(|k| film(x, k, reference[k])),
                    reference,
                }));
                observed.see((13, 100 + x, 200 + y), || Some(tile), 1.0);
            }
        }
        observed
    }

    /// A capture: lighter, with a cast and a veil.
    const CAPTURE: Affine = [
        [1.5, 0.0, 0.0, 0.010],
        [0.0, 1.2, 0.0, 0.006],
        [0.0, 0.0, 0.9, 0.012],
    ];

    fn whole(toward: f32) -> MatrixBounds {
        MatrixBounds {
            toward,
            ..MatrixBounds::default()
        }
    }

    #[test]
    fn a_film_of_one_capture_keeps_its_look_and_takes_the_references_by_the_dose() {
        let observed = seen(4, |_, _, c| through(&CAPTURE, c));
        // Its look is what it makes of the reference: the capture.
        let look = MatrixField::look(&observed);
        for o in 0..3 {
            for i in 0..4 {
                assert!((look.ground[o][i] - CAPTURE[o][i]).abs() < 0.02, "{look:?}");
            }
        }
        // Brought to its own look, nothing is done to it…
        let (field, report, _) = MatrixField::solve(&observed, &whole(0.0));
        assert_eq!((report.measured, report.seam_edges), (12, 0));
        let colour = through(&CAPTURE, [0.08, 0.1, 0.06]);
        let made = through(&field.at((13, 101, 201), 0.4, 0.6), colour);
        for c in 0..3 {
            assert!(
                (made[c] - colour[c]).abs() < 0.02 * colour[c],
                "{made:?} for {colour:?}"
            );
        }
        // …and to the reference's, it is laid on the reference.
        let (field, report, _) = MatrixField::solve(&observed, &whole(1.0));
        let made = through(&field.at((13, 101, 201), 0.4, 0.6), colour);
        for (c, wanted) in [0.08f32, 0.1, 0.06].iter().enumerate() {
            // To within what holding a matrix to changing nothing costs.
            assert!((made[c] - wanted).abs() < 0.07 * wanted, "{made:?}");
        }
        assert!(report.after.0 < 0.03 && report.before.0 > 0.3, "{report:?}");
    }

    #[test]
    fn two_captures_are_brought_to_one_look_and_the_function_is_one_across_an_edge() {
        // Six columns of the capture, then two of another: a stop darker
        // and bluer. Most of the film is the first: that is its look.
        let other = |c: [f32; 3]| {
            let c = through(&CAPTURE, c);
            [c[0] * 0.4, c[1] * 0.5, c[2] * 0.7]
        };
        let observed = seen(8, |x, _, c| {
            if x < 6 {
                through(&CAPTURE, c)
            } else {
                other(c)
            }
        });
        let (field, report, _) = MatrixField::solve(&observed, &whole(0.0));
        // The edge between them is a seam, all three tiles of it.
        assert_eq!(report.seam_edges, 3, "{report:?}");
        // The second capture is given the look of the first.
        let reference = [0.08f32, 0.1, 0.06];
        let made = through(&field.at((13, 107, 201), 0.5, 0.5), other(reference));
        let wanted = through(&CAPTURE, reference);
        for c in 0..3 {
            assert!(
                (made[c] - wanted[c]).abs() < 0.1 * wanted[c],
                "{made:?} for {wanted:?}"
            );
        }
        // Across an edge that is no seam, the same matrix on either side,
        // to the bit: two tiles that meet stay met.
        for (x, v) in [(100u32, 0.3f32), (102, 0.8), (106, 0.5)] {
            let (mine, theirs) = (
                field.at((13, x, 201), 1.0, v),
                field.at((13, x + 1, 201), 0.0, v),
            );
            if x == 105 {
                continue;
            }
            assert_eq!(mine, theirs, "between columns {x} and {}", x + 1);
        }
        // And across the seam, not the same.
        assert_ne!(
            field.at((13, 105, 201), 1.0, 0.5),
            field.at((13, 106, 201), 0.0, 0.5)
        );
    }

    #[test]
    fn a_tile_half_on_water_is_given_what_each_half_wants() {
        // The reference: ground on the left of every tile, a dark sea on
        // the right. The film shows its ground through the capture and its
        // sea light and blue, as a film of one capture does: its look for
        // water is not its look for ground.
        // The last column of tiles is open sea: where the film's look for
        // water is read.
        let mut observed = seen(4, |_, _, c| through(&CAPTURE, c));
        for (at, tile) in &mut observed.tiles {
            let sea = |k: usize| at.1 == 103 || k % PAIRS >= PAIRS / 2;
            let paired = tile.paired.as_deref_mut().expect("its places");
            for k in (0..PAIRS * PAIRS).filter(|k| sea(*k)) {
                let swell = 1.0 + 0.4 * noise(77, k);
                paired.reference[k] = [0.002, 0.012, 0.02].map(|v| v * swell);
                paired.tile[k] = [0.02, 0.07, 0.15].map(|v| v * swell);
            }
        }
        let look = MatrixField::look(&observed);
        // A sea of the reference, in the film's look, is the film's sea.
        let looked = look.wanted([0.002, 0.012, 0.02], 0.0);
        for (c, wanted) in [0.02f32, 0.07, 0.15].iter().enumerate() {
            assert!((looked[c] - wanted).abs() < 0.15 * wanted, "{looked:?}");
        }
        // With all of the reference wanted, the function darkens the sea
        // where the sea is and lays the ground where the ground is — by
        // where in the tile and by what colour, both.
        let (field, _, _) = MatrixField::solve(&observed, &whole(1.0));
        let on_sea = through(&field.at((13, 101, 201), 0.85, 0.5), [0.02, 0.07, 0.15]);
        assert!(on_sea[2] < 0.06, "the sea stayed light: {on_sea:?}");
        let on_ground = through(
            &field.at((13, 101, 201), 0.15, 0.5),
            through(&CAPTURE, [0.08, 0.1, 0.06]),
        );
        assert!((on_ground[1] - 0.1).abs() < 0.03, "{on_ground:?}");
    }
}
