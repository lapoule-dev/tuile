// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A gain a tile, found over a whole film.
//!
//! A level of imagery is not one picture. It is a patchwork of captures —
//! this block in winter, that one in summer, a third overexposed — and
//! where two of them meet, two tiles of the same level meet in two colours.
//! One grade a level cannot mend that: it gives both tiles the same curve.
//!
//! So every imagery tile of a film is given a gain of its own, and all of
//! them are found together.
//!
//! # The problem, by its name
//!
//! Balancing the colour of a mosaic: *relative radiometric normalisation*.
//! Where pictures overlap it is gain compensation, a least squares over
//! what they share. Tiles do not overlap, they abut — so each is normalised
//! against a **reference**: the same ground in a coarser level that is one
//! homogeneous picture. And since what differs is the capture, the gains
//! wanted are **constant by pieces**: few blocks, a step only where two
//! captures meet. That is a Potts segmentation of the tiles' graph — the
//! least number of borders that explains the offsets.
//!
//! # What is measured
//!
//! **One number a tile and a channel**: the whole tile's tone against the
//! tone of the same ground in the reference level, in stops. Relief
//! cancels, since it is the same ground; what is left is the capture's own
//! offset. Not one tile against its neighbour: along a border between
//! captures the outermost texels are blended by whoever made the mosaic —
//! the edges meet while the tiles are a stop and a half apart — and a
//! little further in, relief steps as much as a seam does.
//!
//! # How the blocks are found
//!
//! By **fusion of regions**, which solves Potts greedily and exactly
//! piecewise: every tile begins as a region; the two neighbouring regions
//! that cost least to make one are made one, again and again, until every
//! border left costs more to remove than it is allowed to
//! ([`TileBounds::fusion`]). The cost of a border is what removing it
//! would add to the misfit, for each edge of it: `w₁w₂/(w₁+w₂) · Δ² / edges`
//! — so one odd tile is absorbed, and a long border between two large
//! blocks stands on a small step.
//!
//! # What is already right is kept — always
//!
//! **Two neighbours that are in accord are never given two gains.** This
//! is how the gains are built, not what they aim at:
//!
//! 1. pairs measured in accord are one region before anything is fused;
//! 2. fusion only ever joins;
//! 3. a border that is left stands only if it is borne out twice: by the
//!    offsets on its two sides, and **by the tiles themselves**, whose
//!    facing ground, taken all along the border, steps the same way. The
//!    reference is another picture of the ground, of another season; where
//!    it differs from a level by what grows there, and not by a capture,
//!    the tiles themselves show no step, and no border is drawn;
//! 4. a block has **one gain**. Inside it nothing moves against anything.
//!
//! A border that is missed leaves a seam as it was; it cannot make one.
//! The block that is most of the film is given no gain at all, and the
//! others are brought to it: the film stays what it mostly is.

use std::collections::BTreeMap;

use crate::grade::Grade;
use crate::linear::{Paired, PAIRS};
use crate::measure::{Limits, Local, Measure};

/// Level, column, row.
pub type TileAt = (u8, u32, u32);

/// Cells along a side of what is kept of a tile.
pub const GRID: usize = 8;

pub(crate) const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];
/// Under this, a linear value is the dark, where a ratio means little:
/// three steps of a stored byte. No higher — imagery is dark, a tenth of
/// this film's ground lies under 0.02, and a floor that weighs against it
/// reads a stop there as nine tenths of one.
pub(crate) const FLOOR: f32 = 0.001;
pub(crate) fn luma(c: [f32; 3]) -> f32 {
    LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2]
}

pub(crate) fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

/// The step from `b` to `a`, in stops, channel by channel.
fn step(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| ((a[i] + FLOOR) / (b[i] + FLOOR)).log2())
}

/// Which edge of a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

/// What is kept of an imagery tile: its ground in [`GRID`]² cells, and its
/// four edges — the outermost strip of each, in [`GRID`] cells along it.
/// Linear light.
#[derive(Debug, Clone, PartialEq)]
pub struct TileSeen {
    pub cells: [[f32; 3]; GRID * GRID],
    pub edges: [[[f32; 3]; GRID]; 4],
    /// Tiles of the film this one is draped on.
    pub usage: f32,
    /// How its texels are spread, a channel at a time: what a curve is
    /// fitted on. `None` for a tile seen without them.
    pub tones: Option<Box<Tones>>,
    /// Its ground place for place, and the reference under it once it has
    /// been set against one ([`Self::set_against`]): what a line is fitted
    /// on. `None` for a tile seen without it.
    pub paired: Option<Box<Paired>>,
}

/// Bins a channel's texels are counted in: an eighth of a stop each, from
/// white down to a 4096th of it.
pub const BINS: usize = 96;
const DEPTH: f32 = 12.0;

/// How a tile's texels are spread in light, a channel at a time, over the
/// whole tile and over each of its quarters — so that a tile a level down
/// is set against the quarter of this one that is its own ground. Counts,
/// scaled to 65535 for the fullest bin of each.
#[derive(Debug, Clone, PartialEq)]
pub struct Tones {
    /// The whole tile, then its quarters: top-left, top-right, bottom-left,
    /// bottom-right. A channel each.
    pub spread: [[[u16; BINS]; 3]; 5],
}

impl Tones {
    fn bin(value: f32) -> usize {
        let stops = (value.max(1e-9)).log2().clamp(-DEPTH, 0.0);
        (((stops + DEPTH) / DEPTH * BINS as f32) as usize).min(BINS - 1)
    }

    /// Counts texels of linear light: `at` gives the texel at a place, or
    /// `None` for one that is not there.
    fn count(side: (usize, usize), at: impl Fn(usize, usize) -> Option<[f32; 3]>) -> Self {
        let (w, h) = side;
        let mut counts = [[[0u32; BINS]; 3]; 5];
        for y in 0..h {
            for x in 0..w {
                let Some(c) = at(x, y) else {
                    continue;
                };
                let quarter = 1 + usize::from(x * 2 >= w) + 2 * usize::from(y * 2 >= h);
                for k in 0..3 {
                    counts[0][k][Self::bin(c[k])] += 1;
                    counts[quarter][k][Self::bin(c[k])] += 1;
                }
            }
        }
        let mut spread = [[[0u16; BINS]; 3]; 5];
        for (region, kept) in counts.iter().zip(&mut spread) {
            for (channel, kept) in region.iter().zip(kept) {
                let most = channel.iter().copied().max().unwrap_or(0).max(1);
                for (count, kept) in channel.iter().zip(kept) {
                    *kept = (u64::from(*count) * 65535 / u64::from(most)) as u16;
                }
            }
        }
        Self { spread }
    }

    /// The share of a region's texels that lie under a light, given as its
    /// stops under white, for a channel: [`Self::quantile`] the other way.
    pub fn share(&self, region: usize, channel: usize, stops: f32) -> f32 {
        let bins = &self.spread[region][channel];
        let total: f32 = bins.iter().map(|b| f32::from(*b)).sum();
        if total <= 0.0 {
            return 0.0;
        }
        let at = ((stops + DEPTH) / DEPTH * BINS as f32).clamp(0.0, BINS as f32);
        let whole = at.floor() as usize;
        let below: f32 = bins[..whole.min(BINS)].iter().map(|b| f32::from(*b)).sum();
        let within = bins.get(whole).map_or(0.0, |b| f32::from(*b) * at.fract());
        (below + within) / total
    }

    /// The light under which `share` of a region's texels lie, for a
    /// channel, as its stops under white. `region` 0 is the whole tile.
    pub fn quantile(&self, region: usize, channel: usize, share: f32) -> f32 {
        let bins = &self.spread[region][channel];
        let total: f32 = bins.iter().map(|b| f32::from(*b)).sum();
        if total <= 0.0 {
            return -DEPTH;
        }
        let (mut below, wanted) = (0.0f32, share.clamp(0.0, 1.0) * total);
        for (i, count) in bins.iter().enumerate() {
            let count = f32::from(*count);
            if below + count >= wanted && count > 0.0 {
                // Within the bin, as if its texels were spread evenly.
                let into = (wanted - below) / count;
                return (i as f32 + into) / BINS as f32 * DEPTH - DEPTH;
            }
            below += count;
        }
        0.0
    }
}

impl TileSeen {
    /// A tile from its texels: tightly packed RGBA8, sRGB-encoded. `None`
    /// for one too small to say anything, or mostly transparent.
    pub fn of_rgba8(rgba: &[u8], width: u32, height: u32) -> Option<Self> {
        let (w, h) = (width as usize, height as usize);
        if w < GRID || h < GRID || rgba.len() < w * h * 4 {
            return None;
        }
        let table: [f32; 256] = std::array::from_fn(|v| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        let opaque = rgba[..w * h * 4]
            .chunks_exact(4)
            .filter(|p| p[3] >= 128)
            .count();
        if opaque * 2 < w * h {
            return None;
        }
        // The mean of a rectangle of texels, the transparent left out.
        let mean = |x0: usize, x1: usize, y0: usize, y1: usize| {
            let (mut sum, mut n) = ([0.0f32; 3], 0usize);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &rgba[(y * w + x) * 4..][..4];
                    if p[3] < 128 {
                        continue;
                    }
                    for i in 0..3 {
                        sum[i] += table[p[i] as usize];
                    }
                    n += 1;
                }
            }
            sum.map(|v| v / n.max(1) as f32)
        };
        let cells = std::array::from_fn(|k| {
            let (i, j) = (k % GRID, k / GRID);
            mean(
                i * w / GRID,
                (i + 1) * w / GRID,
                j * h / GRID,
                (j + 1) * h / GRID,
            )
        });
        // A sixty-fourth of the tile deep: the ground on either side of an
        // edge is then the same ground, and a step there is the capture's.
        let (dx, dy) = ((w / 64).max(1), (h / 64).max(1));
        let along_x = |i: usize| (i * w / GRID, (i + 1) * w / GRID);
        let along_y = |i: usize| (i * h / GRID, (i + 1) * h / GRID);
        let edges = [
            std::array::from_fn(|i| mean(0, dx, along_y(i).0, along_y(i).1)),
            std::array::from_fn(|i| mean(w - dx, w, along_y(i).0, along_y(i).1)),
            std::array::from_fn(|i| mean(along_x(i).0, along_x(i).1, 0, dy)),
            std::array::from_fn(|i| mean(along_x(i).0, along_x(i).1, h - dy, h)),
        ];
        let tones = Tones::count((w, h), |x, y| {
            let p = &rgba[(y * w + x) * 4..][..4];
            (p[3] >= 128).then(|| {
                [
                    table[p[0] as usize],
                    table[p[1] as usize],
                    table[p[2] as usize],
                ]
            })
        });
        let places = std::array::from_fn(|k| {
            let (i, j) = (k % PAIRS, k / PAIRS);
            let (x0, x1) = (i * w / PAIRS, ((i + 1) * w / PAIRS).max(i * w / PAIRS + 1));
            let (y0, y1) = (j * h / PAIRS, ((j + 1) * h / PAIRS).max(j * h / PAIRS + 1));
            let seen = (y0..y1).any(|y| (x0..x1).any(|x| rgba[(y * w + x) * 4 + 3] >= 128));
            if seen {
                mean(x0, x1.min(w), y0, y1.min(h))
            } else {
                [f32::NAN; 3]
            }
        });
        Some(Self {
            cells,
            edges,
            usage: 0.0,
            tones: Some(Box::new(tones)),
            paired: Some(Box::new(Paired::alone(places))),
        })
    }

    /// A tile from texels of linear light, `side` of them a side, row by
    /// row: what a tile is once a correction has been applied to it, to be
    /// measured again as any other.
    pub fn of_linear(texels: &[[f32; 3]], side: usize) -> Option<Self> {
        if side < GRID || texels.len() < side * side {
            return None;
        }
        let mean = |x0: usize, x1: usize, y0: usize, y1: usize| {
            let mut sum = [0.0f32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    for k in 0..3 {
                        sum[k] += texels[y * side + x][k];
                    }
                }
            }
            sum.map(|v| v / ((x1 - x0) * (y1 - y0)).max(1) as f32)
        };
        let cut = |i: usize| (i * side / GRID, (i + 1) * side / GRID);
        let cells = std::array::from_fn(|k| {
            let ((x0, x1), (y0, y1)) = (cut(k % GRID), cut(k / GRID));
            mean(x0, x1, y0, y1)
        });
        let d = (side / 64).max(1);
        let edges = [
            std::array::from_fn(|i| mean(0, d, cut(i).0, cut(i).1)),
            std::array::from_fn(|i| mean(side - d, side, cut(i).0, cut(i).1)),
            std::array::from_fn(|i| mean(cut(i).0, cut(i).1, 0, d)),
            std::array::from_fn(|i| mean(cut(i).0, cut(i).1, side - d, side)),
        ];
        Some(Self {
            cells,
            edges,
            usage: 0.0,
            tones: Some(Box::new(Tones::count((side, side), |x, y| {
                Some(texels[y * side + x])
            }))),
            paired: Some(Box::new(Paired::alone(std::array::from_fn(|k| {
                let (i, j) = (k % PAIRS, k / PAIRS);
                let cut = |i: usize| {
                    (
                        i * side / PAIRS,
                        ((i + 1) * side / PAIRS).max(i * side / PAIRS + 1),
                    )
                };
                mean(cut(i).0, cut(i).1.min(side), cut(j).0, cut(j).1.min(side))
            })))),
        })
    }

    /// Sets the tile against a reference picture of the same ground:
    /// `under` is asked for the reference over a rectangle of the tile —
    /// from `(u0, v0)` to `(u1, v1)`, across and down, 0 to 1 — in linear
    /// light, and answers `None` where it has none.
    pub fn set_against(&mut self, mut under: impl FnMut(f32, f32, f32, f32) -> Option<[f32; 3]>) {
        let Some(paired) = self.paired.as_deref_mut() else {
            return;
        };
        let side = PAIRS as f32;
        for (k, place) in paired.reference.iter_mut().enumerate() {
            let (i, j) = ((k % PAIRS) as f32, (k / PAIRS) as f32);
            *place = under(i / side, j / side, (i + 1.0) / side, (j + 1.0) / side)
                .unwrap_or([f32::NAN; 3]);
        }
    }

    /// Sets the tile against the reference another sight of the same tile
    /// was set against: for a tile seen again once corrected.
    pub fn set_against_as(&mut self, other: &TileSeen) {
        if let (Some(mine), Some(theirs)) = (self.paired.as_deref_mut(), other.paired.as_deref()) {
            mine.reference = theirs.reference;
        }
    }

    pub(crate) fn mean(&self) -> [f32; 3] {
        let mut sum = [0.0f32; 3];
        for c in &self.cells {
            for i in 0..3 {
                sum[i] += c[i];
            }
        }
        sum.map(|v| v / (GRID * GRID) as f32)
    }

    /// The mean of a square of cells.
    fn block(&self, i0: usize, j0: usize, side: usize) -> [f32; 3] {
        let mut sum = [0.0f32; 3];
        for j in j0..j0 + side {
            for i in i0..i0 + side {
                for k in 0..3 {
                    sum[k] += self.cells[j * GRID + i][k];
                }
            }
        }
        sum.map(|v| v / (side * side) as f32)
    }
}

/// The imagery of a film, as it was seen: every tile it drapes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observed {
    pub tiles: BTreeMap<TileAt, TileSeen>,
}

/// A value kept in sixteen bits: its stops under white, a 4096th of a stop
/// apart, down to sixteen stops.
fn packed(value: f32) -> u16 {
    let stops = (value.max(0.0) + 1.0 / 65536.0).log2().clamp(-16.0, 0.0);
    ((stops + 16.0) * 4095.0).round() as u16
}

fn unpacked(kept: u16) -> f32 {
    ((f32::from(kept) / 4095.0 - 16.0).exp2() - 1.0 / 65536.0).max(0.0)
}

const MAGIC: &[u8; 8] = b"TLOBS\x03\0\0";
/// What a place that is not a number is kept as: past every light there is.
const NOTHING: u16 = u16::MAX;
/// Bytes a tile is kept in: where it is, its usage, its cells and edges,
/// how its texels are spread, and its places against the reference.
const KEPT: usize =
    1 + 4 + 4 + 4 + (GRID * GRID + 4 * GRID) * 3 * 2 + 1 + 5 * 3 * BINS * 2 + PLACES;
const PLACES: usize = 1 + 2 * PAIRS * PAIRS * 3 * 2;

impl Observed {
    /// Sees a tile, or sees it draped once more.
    pub fn see(&mut self, at: TileAt, tile: impl FnOnce() -> Option<TileSeen>, drapes: f32) {
        if let Some(seen) = self.tiles.get_mut(&at) {
            seen.usage += drapes;
        } else if let Some(mut seen) = tile() {
            seen.usage = drapes;
            self.tiles.insert(at, seen);
        }
    }

    /// What several packs of a film saw, together: a tile two of them
    /// drape is the same tile, draped by both.
    pub fn merged<'a>(parts: impl IntoIterator<Item = &'a Observed>) -> Self {
        let mut all = Self::default();
        for part in parts {
            for (at, tile) in &part.tiles {
                all.see(*at, || Some(tile.clone()), tile.usage);
            }
        }
        all
    }

    /// As it is kept beside a pack.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MAGIC.len() + self.tiles.len() * KEPT);
        out.extend_from_slice(MAGIC);
        for ((level, x, y), tile) in &self.tiles {
            out.push(*level);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&tile.usage.to_le_bytes());
            for c in tile.cells.iter().chain(tile.edges.iter().flatten()) {
                for v in c {
                    out.extend_from_slice(&packed(*v).to_le_bytes());
                }
            }
            // Then how its texels are spread, if that was seen.
            out.push(u8::from(tile.tones.is_some()));
            for i in 0..5 * 3 * BINS {
                let count = tile
                    .tones
                    .as_ref()
                    .map_or(0, |t| t.spread[i / (3 * BINS)][i / BINS % 3][i % BINS]);
                out.extend_from_slice(&count.to_le_bytes());
            }
            // Then its places, and the reference under them.
            out.push(u8::from(tile.paired.is_some()));
            for i in 0..2 * PAIRS * PAIRS * 3 {
                let value = tile.paired.as_ref().map_or(f32::NAN, |p| {
                    let of = if i < PAIRS * PAIRS * 3 {
                        &p.tile
                    } else {
                        &p.reference
                    };
                    of[i / 3 % (PAIRS * PAIRS)][i % 3]
                });
                let kept = if value.is_finite() {
                    packed(value)
                } else {
                    NOTHING
                };
                out.extend_from_slice(&kept.to_le_bytes());
            }
        }
        out
    }

    /// Reads [`Self::to_bytes`] back. `None` if this is not one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let body = bytes.strip_prefix(MAGIC)?;
        if body.len() % KEPT != 0 {
            return None;
        }
        let mut tiles = BTreeMap::new();
        for kept in body.chunks_exact(KEPT) {
            let four = |at: usize| [kept[at], kept[at + 1], kept[at + 2], kept[at + 3]];
            let seen = (GRID * GRID + 4 * GRID) * 3 * 2;
            let mut values = kept[13..13 + seen]
                .chunks_exact(2)
                .map(|b| unpacked(u16::from_le_bytes([b[0], b[1]])));
            let (spread, places) = kept[13 + seen..].split_at(KEPT - PLACES - 13 - seen);
            let paired = (places[0] == 1).then(|| {
                let mut paired = Paired::alone([[f32::NAN; 3]; PAIRS * PAIRS]);
                for (i, b) in places[1..].chunks_exact(2).enumerate() {
                    let kept = u16::from_le_bytes([b[0], b[1]]);
                    let of = if i < PAIRS * PAIRS * 3 {
                        &mut paired.tile
                    } else {
                        &mut paired.reference
                    };
                    of[i / 3 % (PAIRS * PAIRS)][i % 3] = if kept == NOTHING {
                        f32::NAN
                    } else {
                        unpacked(kept)
                    };
                }
                Box::new(paired)
            });
            let tones = (spread[0] == 1).then(|| {
                let mut tones = Tones {
                    spread: [[[0; BINS]; 3]; 5],
                };
                for (i, b) in spread[1..].chunks_exact(2).enumerate() {
                    tones.spread[i / (3 * BINS)][i / BINS % 3][i % BINS] =
                        u16::from_le_bytes([b[0], b[1]]);
                }
                Box::new(tones)
            });
            let mut colour = || Some([values.next()?, values.next()?, values.next()?]);
            let mut cells = [[0.0f32; 3]; GRID * GRID];
            for c in &mut cells {
                *c = colour()?;
            }
            let mut edges = [[[0.0f32; 3]; GRID]; 4];
            for c in edges.iter_mut().flatten() {
                *c = colour()?;
            }
            tiles.insert(
                (
                    kept[0],
                    u32::from_le_bytes(four(1)),
                    u32::from_le_bytes(four(5)),
                ),
                TileSeen {
                    cells,
                    edges,
                    usage: f32::from_le_bytes(four(9)),
                    tones,
                    paired,
                },
            );
        }
        Some(Self { tiles })
    }
}

/// What a tile's gain may not go past, and how the fit is told things.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBounds {
    /// The reference level: the finest level at which the imagery is one
    /// homogeneous picture. Tiles finer than it are measured against it.
    pub reference_level: u8,
    /// A tile's gain on light, in stops either way.
    pub light_stops: f32,
    /// A channel against that, in stops either way: the cast.
    pub tint_stops: f32,
    /// A tile's contrast and saturation, in stops either way: a ratio of
    /// two to the power of this at most.
    pub contrast_stops: f32,
    pub saturation_stops: f32,
    /// A tile's black point, in linear light either way.
    pub black: f32,
    /// Two neighbours whose offsets differ by less than this on every
    /// channel are in accord: they are one block, whatever else is found.
    pub accord_stops: f32,
    /// What a border must cost to stand, an edge of it: regions are fused
    /// while the cheapest border costs less. In stops squared — see the
    /// module for what a border costs.
    pub fusion: f32,
    /// A border between blocks is one only where the tiles themselves,
    /// all along it, step by more than this.
    pub border_stops: f32,
}

impl TileBounds {
    /// The same bounds as a measure is told them.
    pub fn limits(&self) -> Limits {
        Limits {
            light_stops: self.light_stops,
            tint_stops: self.tint_stops,
            shape_stops: self.contrast_stops.max(self.saturation_stops),
            black: self.black,
            ..Limits::default()
        }
    }
}

impl Default for TileBounds {
    fn default() -> Self {
        Self {
            reference_level: 12,
            light_stops: 1.5,
            tint_stops: 0.6,
            contrast_stops: 0.4,
            saturation_stops: 0.4,
            black: 0.01,
            accord_stops: 0.05,
            fusion: 0.3,
            border_stops: 0.15,
        }
    }
}

/// What a fit was made of and what it left: steps in stops of light, as
/// the median and the 95th centile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TileReport {
    /// Tiles finer than the reference level, and of those the ones that
    /// could be measured against it.
    pub tiles: usize,
    pub measured: usize,
    /// Blocks: tiles given one gain together.
    pub blocks: usize,
    /// Tiles in the block left as it is.
    pub untouched: usize,
    /// Edges two tiles of a level share, and those a border between two
    /// blocks runs along.
    pub edges: usize,
    pub borders: usize,
    /// Pairs of neighbours in accord — and of those, the pairs given two
    /// gains. The second is nought, by construction.
    pub accorded: usize,
    pub accord_broken: usize,
    /// How far tiles are from the film's own block against the reference,
    /// with nothing done and with the gains.
    pub apart_before: (f32, f32),
    pub apart_after: (f32, f32),
    /// Blocks whose gain was held at a bound.
    pub held: usize,
}

/// What a tile is given: a gain, in stops a channel (`stops`), and — in
/// `rest` — the correction of every other number of the measure the gains
/// were fitted by, in that measure's order, with in `pivot` the light a
/// contrast turns about (its stops under white). A tile that is in none of
/// them has nothing done to it. Every tile of a block has the same.
#[derive(Debug, Clone, PartialEq)]
pub struct TileGains {
    pub measure: Measure,
    pub stops: BTreeMap<TileAt, [f32; 3]>,
    pub rest: BTreeMap<TileAt, Vec<f32>>,
    pub pivot: BTreeMap<TileAt, f32>,
}

impl Default for TileGains {
    fn default() -> Self {
        Self {
            measure: Measure::Moments,
            stops: BTreeMap::new(),
            rest: BTreeMap::new(),
            pivot: BTreeMap::new(),
        }
    }
}

/// Kept to a 64th of a stop: the same numbers wherever this ran, and two
/// tiles of a block the same to the bit.
pub(crate) fn kept(stops: f32) -> f32 {
    (stops * 64.0).round() / 64.0
}

pub(crate) fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

pub(crate) fn shown(steps: &mut [f32]) -> (f32, f32) {
    if steps.is_empty() {
        return (0.0, 0.0);
    }
    steps.sort_by(f32::total_cmp);
    (
        steps[steps.len() / 2],
        steps[((steps.len() - 1) as f32 * 0.95) as usize],
    )
}

/// The value at the middle of `values`' weight.
pub(crate) fn weighted_median(values: &mut [(f32, f32)]) -> f32 {
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f32 = values.iter().map(|v| v.1).sum();
    let mut seen = 0.0;
    for (value, weight) in values.iter() {
        seen += weight;
        if seen >= 0.5 * total {
            return *value;
        }
    }
    values.last().map_or(0.0, |v| v.0)
}

pub(crate) fn light_of(stops: [f32; 3]) -> f32 {
    LUMA[0] * stops[0] + LUMA[1] * stops[1] + LUMA[2] * stops[2]
}

impl Observed {
    /// How far a tile is from the reference level over the same ground,
    /// in stops a channel: the median over the cells the two share. `None`
    /// for a tile no finer than the reference, or whose ground the
    /// reference was not seen over.
    pub fn offset(&self, at: TileAt, reference_level: u8) -> Option<[f32; 3]> {
        let (level, x, y) = at;
        let up = level.checked_sub(reference_level).filter(|up| *up > 0)?;
        let tile = self.tiles.get(&at)?;
        let under = self.tiles.get(&(
            reference_level,
            x.checked_shr(up.into())?,
            y.checked_shr(up.into())?,
        ))?;
        let span = 1u32 << up;
        // The reference over the tile's own ground: the cells of it the
        // tile covers, or — the tile being less than a cell — the cell it
        // lies in.
        let ground = if u32::from(up) <= GRID.trailing_zeros() {
            let across = GRID >> up;
            under.block(
                (x & (span - 1)) as usize * across,
                (y & (span - 1)) as usize * across,
                across,
            )
        } else {
            let cell = |v: u32| {
                (((u64::from(v & (span - 1)) * 2 + 1) * GRID as u64 / (u64::from(span) * 2))
                    as usize)
                    .min(GRID - 1)
            };
            under.cells[cell(y) * GRID + cell(x)]
        };
        Some(step(tile.mean(), ground))
    }
}

/// Everything a fit worked out on the way, for whoever wants to see why
/// it found what it found: one table a kind of thing, as CSV.
///
/// The fit does not read it and is the same with or without it. Regions
/// are named by the index of one of their tiles in `tiles.csv`.
#[derive(Debug, Clone, Default)]
pub struct TileTrace {
    head: String,
    tiles: Vec<String>,
    edges: Vec<String>,
    fusions: Vec<String>,
    borders: Vec<String>,
    rounds: Vec<String>,
}

impl TileTrace {
    /// The tables: a file name and its content.
    pub fn tables(&self) -> [(&'static str, String); 5] {
        let table = |head: &str, rows: &[String]| {
            let mut text = format!("{head}\n");
            for row in rows {
                text += row;
                text.push('\n');
            }
            text
        };
        [
            (
                "tiles.csv",
                table(
                    &self.head,
                    &self.tiles,
                ),
            ),
            (
                "edges.csv",
                table(
                    "edge,a,b,upright,direct,d_tone_light,d_offset_r,d_offset_g,d_offset_b,d_offset_light,accorded,joined_by,joined_round,joined_step,gain_step_light",
                    &self.edges,
                ),
            ),
            (
                "fusions.csv",
                table(
                    "step,round,region_a,region_b,tiles_a,tiles_b,edges,offset_light_a,offset_light_b,apart,cost,regions_left",
                    &self.fusions,
                ),
            ),
            (
                "borders.csv",
                table(
                    "round,region_a,region_b,tiles_a,tiles_b,edges,by_reference,by_tiles,by_tiles_p25,by_tiles_p75,cost,borne",
                    &self.borders,
                ),
            ),
            (
                "rounds.csv",
                table(
                    "round,regions_at_start,regions_after_fusion,borders_checked,borders_removed",
                    &self.rounds,
                ),
            ),
        ]
    }
}

/// How a tile stands against the reference over the same ground, beyond its
/// tone: what a gain does not mend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Apart {
    /// Its contrast against the reference's, in stops: the spread of its
    /// light over the ground, as a ratio. Nought: as contrasted.
    pub contrast: f32,
    /// Its saturation against the reference's, in stops: how far its
    /// colours stand from grey, as a ratio.
    pub saturation: f32,
    /// Its black point less the reference's: the darkest of its ground, in
    /// linear light, once the two are brought to one tone.
    pub black: f32,
}

impl Observed {
    /// What a gain does not say of a tile against the reference: contrast,
    /// saturation and black point, measured over the same ground at the
    /// same sampling — the cells of the reference the tile covers, and the
    /// tile's own brought to them. `None` where [`Self::offset`] is, and
    /// for a tile too far below the reference to share cells with it.
    pub fn apart(&self, at: TileAt, reference_level: u8) -> Option<Apart> {
        let (level, x, y) = at;
        let up = level.checked_sub(reference_level).filter(|up| *up > 0)?;
        if u32::from(up) >= GRID.trailing_zeros() {
            return None;
        }
        let tile = self.tiles.get(&at)?;
        let under = self.tiles.get(&(reference_level, x >> up, y >> up))?;
        let span = 1u32 << up;
        let across = GRID >> up;
        let (i0, j0) = (
            (x & (span - 1)) as usize * across,
            (y & (span - 1)) as usize * across,
        );
        let (mut mine, mut theirs) = (Vec::new(), Vec::new());
        for cj in 0..across {
            for ci in 0..across {
                mine.push(tile.block(ci << up, cj << up, 1 << up));
                theirs.push(under.cells[(j0 + cj) * GRID + i0 + ci]);
            }
        }
        // The two brought to one tone first: what is compared is shape.
        let tone =
            |cells: &[[f32; 3]]| cells.iter().map(|c| luma(*c)).sum::<f32>() / cells.len() as f32;
        let to = tone(&theirs) / tone(&mine).max(1e-6);
        let mine: Vec<[f32; 3]> = mine.iter().map(|c| c.map(|v| v * to)).collect();
        // The spread of light over the ground, in stops.
        let spread = |cells: &[[f32; 3]]| {
            let light: Vec<f32> = cells.iter().map(|c| (luma(*c) + FLOOR).log2()).collect();
            let mean = light.iter().sum::<f32>() / light.len() as f32;
            (light.iter().map(|l| (l - mean) * (l - mean)).sum::<f32>() / light.len() as f32).sqrt()
        };
        // How far colours stand from grey, against how light they are.
        let colour = |cells: &[[f32; 3]]| {
            cells
                .iter()
                .map(|c| {
                    let y = luma(*c);
                    (0..3).map(|k| (c[k] - y).abs()).sum::<f32>() / (y + FLOOR)
                })
                .sum::<f32>()
                / cells.len() as f32
        };
        let contrast = (spread(&mine) + 0.02) / (spread(&theirs) + 0.02);
        // The black point is what is left in the darkest of the ground once
        // tone and contrast are the reference's: a flatter picture has
        // lighter shadows without any veil over them, and that is contrast,
        // not black.
        let middle = tone(&mine).max(1e-6);
        let darkest = |cells: &[[f32; 3]], power: f32| {
            cells
                .iter()
                .map(|c| middle * (luma(*c).max(1e-6) / middle).powf(power))
                .fold(f32::MAX, f32::min)
        };
        Some(Apart {
            contrast: contrast.log2(),
            saturation: ((colour(&mine) + 0.02) / (colour(&theirs) + 0.02)).log2(),
            black: darkest(&mine, 1.0 / contrast) - darkest(&theirs, 1.0),
        })
    }
}

/// Whether a cell is snow, or cloud, or anything as light and as
/// colourless: not ground to be matched. Snow on one side of an edge and
/// none on the other is the ground as it was that day — it is not a seam
/// to be mended, and a tile is not to be darkened for having it.
pub(crate) fn snow(c: [f32; 3]) -> bool {
    let y = luma(c);
    let (most, least) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
    y > 0.45 && most - least < 0.2 * y
}

/// How two neighbours of a level meet along the edge they share.
///
/// Measured **at the edge, in the gradient domain, from the two tiles
/// alone**: a seam is an edge that is in neither picture. Across the edge
/// the two tiles' facing cells step by some amount; just inside each tile,
/// the ground steps from one cell to the next by some amount too — its own
/// slope there, relief and all. What the edge steps by *beyond the mean of
/// those two slopes* is what neither tile has: the seam.
///
/// Nothing else is asked. No reference: a reference is another picture of
/// the ground, of another season, and whatever grows or melts between the
/// two would be read as a seam. And not how far apart the two tiles are as
/// wholes: snow on one and forest on the other are apart and meet
/// perfectly well. Both were tried on a real film, and both drew seams
/// where the tiles met.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Junction {
    /// The step from the second tile to the first across the edge that is
    /// in neither tile, in stops of light: the median over the cells along
    /// the edge.
    pub step: f32,
    /// The share of those cells that step the way the median does. A seam
    /// steps them all; ground steps some up and some down.
    pub coherence: f32,
    /// What the facing cells step by, before the ground's own slope is
    /// taken away.
    pub tiles: f32,
}

impl Junction {
    /// Of the cells along an edge, as many as must step the same way for
    /// the edge to be a seam: all but one.
    pub const COHERENT: f32 = 0.87;

    /// How far the two tiles are from meeting: the step, if it runs all
    /// along the edge; nothing if it does not.
    pub fn apart(&self) -> f32 {
        if self.coherence >= Self::COHERENT {
            self.step.abs()
        } else {
            0.0
        }
    }
}

impl Observed {
    /// How tile `a` and its neighbour `b` — to its right if `upright`,
    /// below it otherwise — meet along their edge: see [`Junction`].
    /// Cells under snow are left out of it. `None` if either was not seen.
    pub fn junction(&self, a: TileAt, b: TileAt, upright: bool) -> Option<Junction> {
        let (mine, theirs) = (self.tiles.get(&a)?, self.tiles.get(&b)?);
        // A cell of a tile, `depth` cells in from the shared edge, at
        // `k` along it: an eighth of a tile each, so that the first is
        // already past the texels a mosaic blends at a seam.
        let cell = |tile: &TileSeen, from_end: bool, depth: usize, k: usize| {
            let at = if from_end { GRID - 1 - depth } else { depth };
            let c = if upright {
                tile.cells[k * GRID + at]
            } else {
                tile.cells[at * GRID + k]
            };
            (luma(c) + FLOOR).log2()
        };
        let snowy = |tile: &TileSeen, from_end: bool, depth: usize, k: usize| {
            let at = if from_end { GRID - 1 - depth } else { depth };
            snow(if upright {
                tile.cells[k * GRID + at]
            } else {
                tile.cells[at * GRID + k]
            })
        };
        let (mut across, mut beyond) = (Vec::with_capacity(GRID), Vec::with_capacity(GRID));
        for k in 0..GRID {
            // a's last two cells towards the edge, b's first two away
            // from it: four cells in a line across the edge.
            if [(mine, true), (theirs, false)]
                .iter()
                .any(|(tile, end)| (0..2).any(|depth| snowy(tile, *end, depth, k)))
            {
                continue;
            }
            let (a1, a0) = (cell(mine, true, 1, k), cell(mine, true, 0, k));
            let (b0, b1) = (cell(theirs, false, 0, k), cell(theirs, false, 1, k));
            let step = a0 - b0;
            // The ground's own slope, on either side, in the same sense.
            let slope = 0.5 * ((a1 - a0) + (b0 - b1));
            across.push(step);
            beyond.push(step - slope);
        }
        // Under snow for more than half its length, an edge says nothing.
        if beyond.len() * 2 < GRID {
            return Some(Junction {
                step: 0.0,
                coherence: 0.0,
                tiles: 0.0,
            });
        }
        let cells = beyond.clone();
        let step = median(&mut beyond);
        let coherence = cells.iter().filter(|c| c.signum() == step.signum()).count() as f32
            / cells.len() as f32;
        Some(Junction {
            step,
            coherence,
            tiles: median(&mut across),
        })
    }
}

/// Places from a tile's edge over which a mosaic blends one capture into
/// the next, of [`PAIRS`] a side.
const BLENDED: usize = 2;

impl Observed {
    /// How tile `a` and its neighbour `b` meet along their edge, **read
    /// against the reference under them**: at each place along the edge,
    /// the step from one tile's last place to the other's first, less the
    /// step the reference shows between the same two places. What is left
    /// is in the film and not in the ground — whatever the season the
    /// film was taken in, and whatever the relief does there, since the
    /// reference is one picture across the edge.
    ///
    /// `None` if either tile was not set against a reference; an edge with
    /// fewer than half its places to compare says nothing.
    pub fn junction_against(&self, a: TileAt, b: TileAt, upright: bool) -> Option<Junction> {
        let (mine, theirs) = (
            self.tiles.get(&a)?.paired.as_deref()?,
            self.tiles.get(&b)?.paired.as_deref()?,
        );
        if !mine.reference.iter().any(|c| c[0].is_finite())
            || !theirs.reference.iter().any(|c| c[0].is_finite())
        {
            return None;
        }
        // Places at `k` along the edge, `depth` in from it on a's side or
        // on b's. A mosaic blends two captures over the places nearest
        // their seam — measured, two of them: a step of more than a stop
        // shows as a quarter of one between the places that face each
        // other. So each side is read past the blend, over the two places
        // after it; the reference taken away, the ground between places
        // that far apart costs nothing.
        let place = |last: bool, depth: usize, k: usize| {
            let at = if last { PAIRS - 1 - depth } else { depth };
            if upright {
                k * PAIRS + at
            } else {
                at * PAIRS + k
            }
        };
        let stops = |c: [f32; 3]| (luma(c) + FLOOR).log2();
        // A side's film and reference past the blend, in stops; `None` if
        // either of its places cannot be compared.
        let side = |of: &Paired, last: bool, k: usize| {
            let (mut film, mut under) = (0.0f32, 0.0f32);
            for depth in BLENDED..BLENDED + 2 {
                let at = place(last, depth, k);
                if !crate::linear::ground(of.tile[at]) || !crate::linear::ground(of.reference[at]) {
                    return None;
                }
                film += stops(of.tile[at]) / 2.0;
                under += stops(of.reference[at]) / 2.0;
            }
            Some((film, under))
        };
        let (mut across, mut beyond) = (Vec::with_capacity(PAIRS), Vec::with_capacity(PAIRS));
        for k in 0..PAIRS {
            let (Some(a), Some(b)) = (side(mine, true, k), side(theirs, false, k)) else {
                continue;
            };
            let step = a.0 - b.0;
            across.push(step);
            beyond.push(step - (a.1 - b.1));
        }
        if beyond.len() * 2 < PAIRS {
            return Some(Junction {
                step: 0.0,
                coherence: 0.0,
                tiles: 0.0,
            });
        }
        let places = beyond.clone();
        let step = median(&mut beyond);
        let coherence = places
            .iter()
            .filter(|c| c.signum() == step.signum())
            .count() as f32
            / places.len() as f32;
        Some(Junction {
            step,
            coherence,
            tiles: median(&mut across),
        })
    }
}

impl TileGains {
    /// What is done to a tile: its correction, as the measure it was
    /// fitted by makes of it. Nothing for a tile that was not fitted.
    pub fn local(&self, at: TileAt) -> Local {
        let (stops, rest) = (self.stops.get(&at), self.rest.get(&at));
        if stops.is_none() && rest.is_none() {
            return Local::IDENTITY;
        }
        let mut given = stops.map_or(vec![0.0; 3], |s| s.to_vec());
        match rest {
            Some(rest) => given.extend(rest),
            None => given.resize(self.measure.len(), 0.0),
        }
        self.measure
            .local(&given, self.pivot.get(&at).copied().unwrap_or(-2.5))
    }

    /// The same as a grade, for whoever applies only that: black point,
    /// gain, contrast about a pivot, saturation.
    pub fn of(&self, at: TileAt) -> Grade {
        self.local(at).grade()
    }

    /// Finds a gain for every tile of a film, the tiles measured by their
    /// moments: see the module.
    pub fn solve(observed: &Observed, bounds: &TileBounds) -> (Self, TileReport) {
        Self::solve_with(observed, Measure::Moments, bounds, None)
    }

    /// Finds a gain for every tile of a film, the tiles measured as
    /// `measure` says. The blocks are found from the tone alone — the
    /// first three numbers of any measure — so they are the same blocks
    /// whatever the measure; what each block is given is every number of
    /// it.
    pub fn solve_by(
        observed: &Observed,
        measure: Measure,
        bounds: &TileBounds,
    ) -> (Self, TileReport) {
        Self::solve_with(observed, measure, bounds, None)
    }

    /// [`Self::solve_by`], and everything it worked out on the way.
    pub fn solve_traced(
        observed: &Observed,
        measure: Measure,
        bounds: &TileBounds,
    ) -> (Self, TileReport, TileTrace) {
        let mut trace = TileTrace::default();
        let (gains, report) = Self::solve_with(observed, measure, bounds, Some(&mut trace));
        (gains, report, trace)
    }

    fn solve_with(
        observed: &Observed,
        measure: Measure,
        bounds: &TileBounds,
        mut trace: Option<&mut TileTrace>,
    ) -> (Self, TileReport) {
        let nothing = || Self {
            measure,
            ..Self::default()
        };
        let reference = bounds.reference_level;
        // The tiles finer than the reference: the ones a gain is found
        // for. The reference's own, and coarser, are one picture already.
        let at: Vec<TileAt> = observed
            .tiles
            .keys()
            .filter(|a| a.0 > reference)
            .copied()
            .collect();
        let n = at.len();
        let index: BTreeMap<TileAt, usize> = at.iter().enumerate().map(|(i, a)| (*a, i)).collect();
        let usage: Vec<f32> = at
            .iter()
            .map(|a| observed.tiles[a].usage.max(0.0))
            .collect();
        // What each tile measures against the reference: all this fit
        // knows of it. The first three numbers are its tone, and the blocks
        // are found from that.
        let measures: Vec<Option<Vec<f32>>> = at
            .iter()
            .map(|a| measure.of(observed, *a, reference))
            .collect();
        let offset: Vec<Option<[f32; 3]>> = measures
            .iter()
            .map(|m| m.as_ref().map(|m| Measure::tone(m)))
            .collect();
        let mut report = TileReport {
            tiles: n,
            measured: offset.iter().flatten().count(),
            ..TileReport::default()
        };
        if report.measured == 0 {
            return (nothing(), report);
        }

        // Neighbours: the tile to the right and the tile below, of the
        // same level.
        let mut edges: Vec<(usize, usize)> = Vec::new();
        let mut uprights: Vec<bool> = Vec::new();
        // What the two tiles themselves show across each edge: the step,
        // in stops of light, from the second to the first, between their
        // facing outer cells — past the texels a mosaic blends — as the
        // median along the edge. Relief makes it rough edge by edge; all
        // along a border it tells.
        let mut direct: Vec<f32> = Vec::new();
        for (i, (level, x, y)) in at.iter().enumerate() {
            for (next, upright) in [((*level, x + 1, *y), true), ((*level, *x, y + 1), false)] {
                let Some(j) = index.get(&next) else {
                    continue;
                };
                let (mine, theirs) = (&observed.tiles[&at[i]], &observed.tiles[&at[*j]]);
                let mut along: Vec<f32> = (0..GRID)
                    .map(|k| {
                        let (p, q) = if upright {
                            (mine.cells[k * GRID + GRID - 1], theirs.cells[k * GRID])
                        } else {
                            (mine.cells[(GRID - 1) * GRID + k], theirs.cells[k])
                        };
                        ((luma(p) + FLOOR) / (luma(q) + FLOOR)).log2()
                    })
                    .collect();
                edges.push((i, *j));
                uprights.push(upright);
                direct.push(median(&mut along));
            }
        }
        report.edges = edges.len();
        let apart = |a: [f32; 3], b: [f32; 3]| {
            (0..3).map(|c| (a[c] - b[c]).abs()).fold(
                light_of([a[0] - b[0], a[1] - b[1], a[2] - b[2]]).abs(),
                f32::max,
            )
        };
        let in_accord = |a: usize, b: usize| match (offset[a], offset[b]) {
            (Some(p), Some(q)) => apart(p, q) < bounds.accord_stops,
            _ => false,
        };
        let mut parent: Vec<usize> = (0..n).collect();
        let join = |parent: &mut Vec<usize>, a: usize, b: usize| {
            let (ra, rb) = (find(parent, a), find(parent, b));
            if ra != rb {
                parent[ra] = rb;
            }
        };
        // 1. Pairs in accord are one region before anything else is done:
        // this is the line that keeps what is right as it is. A tile that
        // could not be measured goes with its neighbours.
        // For the trace: what first made the two tiles of an edge one
        // region, in which round and at which fusion.
        let mut fate: Vec<Option<(&'static str, usize, usize)>> = vec![None; edges.len()];
        let tracing = trace.is_some();
        let settle = |fate: &mut Vec<Option<(&'static str, usize, usize)>>,
                      parent: &mut Vec<usize>,
                      by: &'static str,
                      round: usize,
                      step: usize| {
            for (k, (a, b)) in edges.iter().enumerate() {
                if fate[k].is_none() && find(parent, *a) == find(parent, *b) {
                    fate[k] = Some((by, round, step));
                }
            }
        };
        for (a, b) in &edges {
            let accorded = in_accord(*a, *b);
            report.accorded += usize::from(accorded);
            if accorded || offset[*a].is_none() || offset[*b].is_none() {
                join(&mut parent, *a, *b);
            }
        }
        let after_accord: Vec<usize> = if tracing {
            settle(&mut fate, &mut parent, "accord", 0, 0);
            (0..n).map(|i| find(&mut parent, i)).collect()
        } else {
            Vec::new()
        };
        let (mut round, mut step, mut at_start) = (0usize, 0usize, 0usize);

        // 2. Regions are fused, the cheapest border first, and then every
        // border left is put to the tiles themselves; one they do not bear
        // out is removed, and fusion goes on from there.
        loop {
            // The regions as they stand: how many measured tiles each has,
            // and the sum of their offsets.
            let root: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
            let mut region: BTreeMap<usize, (f32, [f32; 3])> = BTreeMap::new();
            let mut tiles_in: BTreeMap<usize, usize> = BTreeMap::new();
            for i in 0..n {
                *tiles_in.entry(root[i]).or_default() += 1;
                let entry = region.entry(root[i]).or_insert((0.0, [0.0; 3]));
                if let Some(o) = offset[i] {
                    entry.0 += 1.0;
                    for c in 0..3 {
                        entry.1[c] += o[c];
                    }
                }
            }
            // The borders: how many edges two regions share, and what the
            // tiles themselves show across them, the lower against the
            // higher.
            let mut border: BTreeMap<(usize, usize), Vec<f32>> = BTreeMap::new();
            for (k, (a, b)) in edges.iter().enumerate() {
                let (ra, rb) = (root[*a], root[*b]);
                if ra < rb {
                    border.entry((ra, rb)).or_default().push(direct[k]);
                } else if rb < ra {
                    border.entry((rb, ra)).or_default().push(-direct[k]);
                }
            }
            let mean = |r: &(f32, [f32; 3])| r.1.map(|v| v / r.0.max(1.0));
            // What removing a border would add to the misfit, an edge.
            let cost = |a: &(f32, [f32; 3]), b: &(f32, [f32; 3]), edges: usize| {
                if a.0 == 0.0 || b.0 == 0.0 {
                    return 0.0;
                }
                let (p, q) = (mean(a), mean(b));
                let apart: f32 = (0..3).map(|c| (p[c] - q[c]) * (p[c] - q[c])).sum();
                a.0 * b.0 / (a.0 + b.0) * apart / edges as f32
            };
            // The cheapest border, if it costs less than a border must.
            let cheapest = border
                .iter()
                .map(|((a, b), across)| ((*a, *b), cost(&region[a], &region[b], across.len())))
                .min_by(|x, y| x.1.total_cmp(&y.1));
            if at_start == 0 {
                at_start = region.len();
            }
            if let Some(((a, b), least)) = cheapest {
                if least < bounds.fusion {
                    join(&mut parent, a, b);
                    step += 1;
                    if let Some(trace) = trace.as_deref_mut() {
                        let (p, q) = (mean(&region[&a]), mean(&region[&b]));
                        trace.fusions.push(format!(
                            "{step},{round},{a},{b},{},{},{},{:.4},{:.4},{:.4},{least:.5},{}",
                            tiles_in[&a],
                            tiles_in[&b],
                            border[&(a, b)].len(),
                            light_of(p),
                            light_of(q),
                            apart(p, q),
                            region.len() - 1,
                        ));
                        settle(&mut fate, &mut parent, "fusion", round, step);
                    }
                    continue;
                }
            }
            // Every border left is dear by the reference. Is it borne out
            // by the tiles?
            let (mut removed, checked) = (0usize, border.len());
            for ((a, b), across) in &mut border {
                let (p, q) = (mean(&region[a]), mean(&region[b]));
                let by_reference = light_of([p[0] - q[0], p[1] - q[1], p[2] - q[2]]);
                let by_tiles = median(across);
                let borne = by_tiles.abs() > bounds.border_stops
                    && (by_reference == 0.0 || by_tiles.signum() == by_reference.signum());
                if let Some(trace) = trace.as_deref_mut() {
                    // `across` was sorted by the median.
                    let at = |q: f32| across[((across.len() - 1) as f32 * q) as usize];
                    trace.borders.push(format!(
                        "{round},{a},{b},{},{},{},{by_reference:.4},{by_tiles:.4},{:.4},{:.4},{:.5},{}",
                        tiles_in[a],
                        tiles_in[b],
                        across.len(),
                        at(0.25),
                        at(0.75),
                        cost(&region[a], &region[b], across.len()),
                        u8::from(borne),
                    ));
                }
                if !borne {
                    join(&mut parent, *a, *b);
                    removed += 1;
                }
            }
            if let Some(trace) = trace.as_deref_mut() {
                trace.rounds.push(format!(
                    "{round},{at_start},{},{checked},{removed}",
                    region.len()
                ));
                settle(&mut fate, &mut parent, "unborne", round, step);
            }
            round += 1;
            at_start = 0;
            if removed == 0 {
                break;
            }
        }
        let block_offset = |members: &[usize]| -> Option<[f32; 3]> {
            let measured: Vec<usize> = members
                .iter()
                .copied()
                .filter(|i| offset[*i].is_some())
                .collect();
            (!measured.is_empty()).then(|| {
                [0, 1, 2].map(|c| {
                    let mut values: Vec<(f32, f32)> = measured
                        .iter()
                        .map(|i| (offset[*i].map_or(0.0, |o| o[c]), usage[*i].max(1e-3)))
                        .collect();
                    weighted_median(&mut values)
                })
            })
        };

        // 3. One gain a block: what brings it to the block that is most of
        // the film, which is left as it is.
        let root: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, r) in root.iter().enumerate() {
            members.entry(*r).or_default().push(i);
        }
        report.blocks = members.len();
        let weight = |m: &[usize]| m.iter().map(|i| usage[*i].max(1e-3)).sum::<f32>();
        let own = members
            .iter()
            .filter(|(_, m)| block_offset(m).is_some())
            .max_by(|a, b| weight(a.1).total_cmp(&weight(b.1)))
            .map(|(r, _)| *r);
        let Some(own) = own else {
            return (nothing(), report);
        };
        let held_to = block_offset(&members[&own]).unwrap_or([0.0; 3]);
        report.untouched = members[&own].len();
        // Held within the bounds: the gain on light, then each channel
        // against it.
        let within = |g: [f32; 3]| {
            let light = light_of(g);
            let lifted = light.clamp(-bounds.light_stops, bounds.light_stops);
            // Holding a channel back moves the light of the three, so the
            // cast is taken against it again until it has settled.
            let mut tint = g.map(|c| c - light);
            for _ in 0..6 {
                let together = light_of(tint);
                tint = tint.map(|t| (t - together).clamp(-bounds.tint_stops, bounds.tint_stops));
            }
            tint.map(|t| lifted + t)
        };
        let mut gain: BTreeMap<usize, [f32; 3]> = BTreeMap::new();
        for (block, m) in &members {
            let wanted = match block_offset(m) {
                Some(o) if *block != own => [0, 1, 2].map(|c| held_to[c] - o[c]),
                _ => [0.0; 3],
            };
            let given = within(wanted).map(kept);
            report.held += usize::from((0..3).any(|c| (given[c] - wanted[c]).abs() > 1.0 / 32.0));
            gain.insert(*block, given);
        }

        // …and every other number of the measure, a block at a time and
        // the same way: the block's own middle, brought to that of the
        // film's own block, within its bound.
        let found = measure.len();
        let limits = bounds.limits();
        let block_rest = |members: &[usize]| -> Vec<f32> {
            (3..found)
                .map(|c| {
                    let mut values: Vec<(f32, f32)> = members
                        .iter()
                        .filter_map(|i| measures[*i].as_ref().map(|m| (m[c], usage[*i].max(1e-3))))
                        .filter(|(v, _)| v.is_finite())
                        .collect();
                    if values.is_empty() {
                        f32::NAN
                    } else {
                        weighted_median(&mut values)
                    }
                })
                .collect()
        };
        let own_rest = block_rest(&members[&own]);
        let mut rest: BTreeMap<usize, Vec<f32>> = BTreeMap::new();
        let mut pivot: BTreeMap<usize, f32> = BTreeMap::new();
        for (block, m) in &members {
            if *block == own {
                continue;
            }
            // The whole correction, so that the measure holds it within
            // its bounds as one; the gain was held already and is not
            // moved by it.
            let mut given = gain[block].to_vec();
            given.extend(block_rest(m).iter().zip(&own_rest).map(|(mine, theirs)| {
                if mine.is_finite() && theirs.is_finite() {
                    theirs - mine
                } else {
                    0.0
                }
            }));
            measure.within(&mut given, &limits);
            measure.keep(&mut given);
            if given[3..].iter().all(|v| *v == 0.0) {
                continue;
            }
            // A contrast turns the block about its own light, once the
            // gain is on.
            let mut tones: Vec<(f32, f32)> = m
                .iter()
                .map(|i| {
                    (
                        (luma(observed.tiles[&at[*i]].mean()) + FLOOR).log2()
                            + light_of(gain[block]),
                        usage[*i].max(1e-3),
                    )
                })
                .collect();
            pivot.insert(*block, kept(weighted_median(&mut tones)));
            rest.insert(*block, given[3..].to_vec());
        }

        for (a, b) in &edges {
            let split = gain[&root[*a]] != gain[&root[*b]];
            report.borders += usize::from(root[*a] != root[*b]);
            report.accord_broken += usize::from(split && in_accord(*a, *b));
        }
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for i in 0..n {
            if let Some(o) = offset[i] {
                let g = gain[&root[i]];
                let left = [0, 1, 2].map(|c| o[c] - held_to[c]);
                before.push(light_of(left).abs());
                after.push(light_of([0, 1, 2].map(|c| left[c] + g[c])).abs());
            }
        }
        report.apart_before = shown(&mut before);
        report.apart_after = shown(&mut after);

        if let Some(trace) = trace {
            let number = |v: Option<f32>| v.map_or(String::new(), |v| format!("{v:.4}"));
            let mut head = String::from("tile,level,x,y,usage,measured,tone_r,tone_g,tone_b,offset_r,offset_g,offset_b,offset_light,region_after_accord,block,gain_r,gain_g,gain_b");
            for prefix in ["m_", "given_"] {
                for name in measure.names() {
                    head += &format!(",{prefix}{name}");
                }
            }
            trace.head = head;
            for (i, (level, x, y)) in at.iter().enumerate() {
                let tone = observed.tiles[&at[i]].mean().map(|v| (v + FLOOR).log2());
                let o = offset[i];
                let g = gain[&root[i]];
                let mut row = format!(
                    "{i},{level},{x},{y},{:.1},{},{:.4},{:.4},{:.4},{},{},{},{},{},{},{:.4},{:.4},{:.4}",
                    usage[i],
                    u8::from(o.is_some()),
                    tone[0],
                    tone[1],
                    tone[2],
                    number(o.map(|o| o[0])),
                    number(o.map(|o| o[1])),
                    number(o.map(|o| o[2])),
                    number(o.map(light_of)),
                    after_accord[i],
                    root[i],
                    g[0],
                    g[1],
                    g[2],
                );
                // Then every number of the measure by its name: what the
                // tile measured (`m_`), and what it is given (`given_`).
                for c in 0..found {
                    row.push(',');
                    row += &number(measures[i].as_ref().map(|m| m[c]).filter(|v| v.is_finite()));
                }
                for c in 0..found {
                    let given = if c < 3 {
                        g[c]
                    } else {
                        rest.get(&root[i]).map_or(0.0, |r| r[c - 3])
                    };
                    row += &format!(",{given:.5}");
                }
                trace.tiles.push(row);
            }
            for (k, (a, b)) in edges.iter().enumerate() {
                let tone = |i: usize| (luma(observed.tiles[&at[i]].mean()) + FLOOR).log2();
                let d = match (offset[*a], offset[*b]) {
                    (Some(p), Some(q)) => Some([p[0] - q[0], p[1] - q[1], p[2] - q[2]]),
                    _ => None,
                };
                let (by, in_round, at_step) = fate[k].unwrap_or(("border", 0, 0));
                let (ga, gb) = (gain[&root[*a]], gain[&root[*b]]);
                trace.edges.push(format!(
                    "{k},{a},{b},{},{:.4},{:.4},{},{},{},{},{},{by},{in_round},{at_step},{:.4}",
                    u8::from(uprights[k]),
                    direct[k],
                    tone(*a) - tone(*b),
                    number(d.map(|d| d[0])),
                    number(d.map(|d| d[1])),
                    number(d.map(|d| d[2])),
                    number(d.map(light_of)),
                    u8::from(in_accord(*a, *b)),
                    light_of([ga[0] - gb[0], ga[1] - gb[1], ga[2] - gb[2]]),
                ));
            }
        }

        let stops = at
            .iter()
            .enumerate()
            .filter(|(i, _)| gain[&root[*i]] != [0.0; 3])
            .map(|(i, a)| (*a, gain[&root[i]]))
            .collect();
        let rest_of = at
            .iter()
            .enumerate()
            .filter_map(|(i, a)| rest.get(&root[i]).map(|r| (*a, r.clone())))
            .collect();
        let pivot_of = at
            .iter()
            .enumerate()
            .filter_map(|(i, a)| pivot.get(&root[i]).map(|p| (*a, *p)))
            .collect();
        (
            Self {
                measure,
                stops,
                rest: rest_of,
                pivot: pivot_of,
            },
            report,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value that is the same for the same place and seed, 0 to 1.
    fn noise(seed: u32, x: u32, y: u32) -> f32 {
        let mut s = seed
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(x.wrapping_mul(0x85EB_CA6B))
            .wrapping_add(y.wrapping_mul(0xC2B2_AE35));
        s ^= s >> 15;
        s = s.wrapping_mul(0x2C1B_3C6D);
        s ^= s >> 12;
        (s >> 8) as f32 / (1u32 << 24) as f32
    }

    const SIDE: u32 = 64;
    /// The level the test's ground is defined at.
    const FINEST: u8 = 18;

    /// The ground at a texel of the world at `level`: smooth over a few
    /// tiles, the same whatever tile of whatever level reads it.
    fn ground(level: u8, wx: u32, wy: u32) -> [f32; 3] {
        let scale = (1u64 << (FINEST - level)) as f32;
        // In texels of level 13, a few tiles to a bump.
        let to13 = (1u64 << (FINEST - 13)) as f32;
        let (fx, fy) = (
            (wx as f32 + 0.5) * scale / to13 / 160.0,
            (wy as f32 + 0.5) * scale / to13 / 160.0,
        );
        let (x0, y0) = (fx.floor() as u32, fy.floor() as u32);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let at = |c: u32| {
            let v = |x, y| 0.05 + 0.25 * noise(7 + c, x, y);
            let (top, bottom) = (
                v(x0, y0) * (1.0 - tx) + v(x0 + 1, y0) * tx,
                v(x0, y0 + 1) * (1.0 - tx) + v(x0 + 1, y0 + 1) * tx,
            );
            top * (1.0 - ty) + bottom * ty
        };
        [at(0), at(1), at(2)]
    }

    /// A tile of that ground, its capture `stops` lighter a channel.
    fn tile(at: TileAt, stops: [f32; 3]) -> TileSeen {
        let stored = |v: f32| {
            let v = v.clamp(0.0, 1.0);
            let s = if v <= 0.003_130_8 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (s * 255.0).round() as u8
        };
        let mut rgba = Vec::with_capacity((SIDE * SIDE * 4) as usize);
        for y in 0..SIDE {
            for x in 0..SIDE {
                let c = ground(at.0, at.1 * SIDE + x, at.2 * SIDE + y);
                for i in 0..3 {
                    rgba.push(stored(c[i] * stops[i].exp2()));
                }
                rgba.push(255);
            }
        }
        let mut seen = TileSeen::of_rgba8(&rgba, SIDE, SIDE).expect("a tile");
        seen.usage = 1.0;
        seen
    }

    const REFERENCE: u8 = 12;

    /// The reference tiles under every finer tile seen, as one picture —
    /// or with `of` more light where it says, for a reference that shows
    /// the ground otherwise.
    fn refer(observed: &mut Observed, of: impl Fn(u32, u32) -> f32) {
        let fine: Vec<TileAt> = observed.tiles.keys().copied().collect();
        for (level, x, y) in fine {
            let up = level - REFERENCE;
            let at = (REFERENCE, x >> up, y >> up);
            let mut under = tile(at, [of(at.1, at.2); 3]);
            under.usage = 0.0;
            observed.tiles.entry(at).or_insert(under);
        }
    }

    /// A film of `wide` × `high` tiles of `level`, each capture as `of`
    /// says, over a reference that is one picture.
    fn film_at(level: u8, wide: u32, high: u32, of: impl Fn(u32, u32) -> [f32; 3]) -> Observed {
        let up = level - REFERENCE;
        let mut observed = Observed::default();
        for y in 0..high {
            for x in 0..wide {
                let at = (level, (50 << up) + x, (100 << up) + y);
                observed.tiles.insert(at, tile(at, of(x, y)));
            }
        }
        refer(&mut observed, |_, _| 0.0);
        observed
    }

    fn film(wide: u32, high: u32, of: impl Fn(u32, u32) -> [f32; 3]) -> Observed {
        film_at(13, wide, high, of)
    }

    fn solved(observed: &Observed) -> (TileGains, TileReport) {
        TileGains::solve(observed, &TileBounds::default())
    }

    fn stops_of(gains: &TileGains, x: u32, y: u32) -> [f32; 3] {
        gains
            .stops
            .get(&(13, 100 + x, 200 + y))
            .copied()
            .unwrap_or([0.0; 3])
    }

    #[test]
    fn two_captures_of_a_level_are_brought_together_and_each_is_kept_whole() {
        // The left third a stop darker and bluer than the rest.
        let off = [-1.0, -1.0, -0.7];
        let observed = film(12, 6, |x, _| if x < 4 { off } else { [0.0; 3] });
        let (gains, report) = solved(&observed);
        assert_eq!(report.blocks, 2, "{report:?}");
        assert_eq!((report.tiles, report.measured), (72, 72));
        assert_eq!(report.borders, 6);
        // What is most of the film is not touched at all…
        assert_eq!(report.untouched, 48);
        for x in 4..12 {
            assert_eq!(stops_of(&gains, x, 1), [0.0; 3]);
        }
        // …and the other capture is brought to it, as one: every tile of
        // it the same gain, to the bit.
        let first = stops_of(&gains, 0, 0);
        for i in 0..3 {
            assert!((first[i] + off[i]).abs() < 0.06, "{first:?}");
        }
        for y in 0..6 {
            for x in 0..4 {
                assert_eq!(stops_of(&gains, x, y), first);
            }
        }
        assert!(report.apart_before.1 > 0.9, "{report:?}");
        assert!(report.apart_after.1 < 0.06, "{report:?}");
        assert_eq!(report.accord_broken, 0);
    }

    #[test]
    fn tiles_that_already_meet_are_left_exactly_as_they_are() {
        let (gains, report) = solved(&film(8, 6, |_, _| [0.0; 3]));
        assert!(gains.stops.is_empty(), "{gains:?}");
        assert_eq!((report.blocks, report.borders), (1, 0));
        assert_eq!(report.untouched, 48);
        assert_eq!(report.accorded, report.edges);
    }

    #[test]
    fn a_reference_that_shows_the_ground_otherwise_draws_no_border() {
        // One capture throughout: every tile meets its neighbours. But the
        // reference is another season, and shows the left of the film a
        // stop lighter than the tiles do. Against it the film is two
        // regions a stop apart — and the tiles themselves show no step
        // where the two meet. Nothing is touched.
        let mut observed = Observed::default();
        for y in 0..6u32 {
            for x in 0..12u32 {
                let at = (13, 100 + x, 200 + y);
                observed.tiles.insert(at, tile(at, [0.0; 3]));
            }
        }
        refer(&mut observed, |x, _| if x < 52 { 1.0 } else { 0.0 });
        let offsets = |x: u32| {
            observed
                .offset((13, 100 + x, 201), REFERENCE)
                .expect("measured")
        };
        assert!(offsets(0)[1] < -0.9 && offsets(8)[1].abs() < 0.1);
        let (gains, report) = solved(&observed);
        assert!(gains.stops.is_empty(), "{:?}", gains.stops);
        assert_eq!(
            (report.blocks, report.borders, report.accord_broken),
            (1, 0, 0)
        );
    }

    #[test]
    fn no_pair_in_accord_is_given_two_gains() {
        // Captures of every size, some a tile wide, offsets from nothing
        // to a stop and a half, over rough ground: whatever is found, a
        // pair measured in accord has one gain.
        for seed in 0..6u32 {
            let observed = film(14, 8, |x, y| {
                let block = (x / (1 + seed % 4), y / (2 + seed % 3));
                let stops = (noise(seed, block.0, block.1) * 6.0).floor() * 0.3 - 0.9;
                [
                    stops,
                    stops,
                    stops + 0.2 * noise(seed + 50, block.0, block.1),
                ]
            });
            let bounds = TileBounds::default();
            let (gains, report) = TileGains::solve(&observed, &bounds);
            assert_eq!(report.accord_broken, 0, "seed {seed}: {report:?}");
            // And counted here again, from the gains alone.
            let (mut accorded, mut broken) = (0, 0);
            for (at, _) in observed.tiles.iter().filter(|(at, _)| at.0 == 13) {
                for next in [(13, at.1 + 1, at.2), (13, at.1, at.2 + 1)] {
                    let (Some(p), Some(q)) = (
                        observed.offset(*at, REFERENCE),
                        observed.offset(next, REFERENCE),
                    ) else {
                        continue;
                    };
                    let light = light_of([p[0] - q[0], p[1] - q[1], p[2] - q[2]]).abs();
                    let apart = (0..3).map(|c| (p[c] - q[c]).abs()).fold(light, f32::max);
                    if apart < bounds.accord_stops {
                        accorded += 1;
                        broken += usize::from(gains.stops.get(at) != gains.stops.get(&next));
                    }
                }
            }
            assert_eq!(accorded, report.accorded);
            assert_eq!(broken, 0, "seed {seed}");
        }
    }

    #[test]
    fn a_tile_in_accord_with_one_neighbour_stays_with_it_whatever_surrounds_it() {
        // A dark capture, and one tile of the light capture reaching into
        // it: dark on three sides, in accord with the light tile on the
        // fourth. A fit that smooths takes it for the dark capture's.
        let observed = film(12, 7, |x, y| {
            if x < 4 || (x == 4 && y != 3) {
                [-1.0; 3]
            } else {
                [0.0; 3]
            }
        });
        let (gains, report) = solved(&observed);
        assert_eq!(report.accord_broken, 0, "{report:?}");
        // It keeps the gain of the one it is in accord with — none…
        assert_eq!(stops_of(&gains, 4, 3), stops_of(&gains, 5, 3));
        assert_eq!(stops_of(&gains, 4, 3), [0.0; 3]);
        // …and the dark capture around it is still brought up.
        assert!((stops_of(&gains, 4, 2)[1] - 1.0).abs() < 0.06, "{gains:?}");
        assert!((stops_of(&gains, 0, 0)[1] - 1.0).abs() < 0.06);
    }

    #[test]
    fn the_film_is_brought_to_its_own_block_not_to_the_reference() {
        // The reference shows all of it half a stop lighter than the film
        // does. The film's larger capture is still what is left alone.
        let mut observed = Observed::default();
        for y in 0..6u32 {
            for x in 0..12u32 {
                let at = (13, 100 + x, 200 + y);
                let stops = if x < 4 { [-1.0; 3] } else { [0.0; 3] };
                observed.tiles.insert(at, tile(at, stops));
            }
        }
        refer(&mut observed, |_, _| 0.5);
        let (gains, report) = solved(&observed);
        assert_eq!(report.untouched, 48, "{report:?}");
        assert_eq!(stops_of(&gains, 8, 2), [0.0; 3]);
        assert!((stops_of(&gains, 0, 2)[1] - 1.0).abs() < 0.06, "{gains:?}");
    }

    #[test]
    fn a_fit_that_is_traced_is_the_same_fit_and_says_what_it_did() {
        let observed = film(
            12,
            6,
            |x, _| if x < 4 { [-1.0, -1.0, -0.7] } else { [0.0; 3] },
        );
        let bounds = TileBounds::default();
        let (gains, report) = TileGains::solve(&observed, &bounds);
        let (traced, traced_report, trace) =
            TileGains::solve_traced(&observed, Measure::Moments, &bounds);
        assert_eq!((&traced, &traced_report), (&gains, &report));

        let tables = trace.tables();
        let rows = |name: &str| -> Vec<Vec<String>> {
            let (_, text) = tables.iter().find(|(n, _)| *n == name).expect("a table");
            let mut lines = text.lines();
            let columns = lines.next().expect("a head").split(',').count();
            lines
                .map(|l| l.split(',').map(str::to_string).collect::<Vec<_>>())
                .inspect(|row| assert_eq!(row.len(), columns, "{name}: {row:?}"))
                .collect()
        };
        // A row a tile and an edge, and the tiles' gains are the fit's.
        let tiles = rows("tiles.csv");
        assert_eq!(tiles.len(), report.tiles);
        let moved = tiles.iter().filter(|t| t[15] != "0.0000").count();
        assert_eq!(moved, gains.stops.len());
        let edges = rows("edges.csv");
        assert_eq!(edges.len(), report.edges);
        // Every edge says what became of it: the borders left are the
        // report's, and the rest were joined by something.
        let by = |what: &str| edges.iter().filter(|e| e[11] == what).count();
        assert_eq!(by("border"), report.borders);
        assert_eq!(
            by("accord") + by("fusion") + by("unborne") + by("border"),
            edges.len()
        );
        assert!(by("accord") >= report.accorded);
        // Every fusion is there, each leaving one region fewer, and the
        // last round is the one that removed nothing.
        let fusions = rows("fusions.csv");
        for pair in fusions.windows(2).filter(|p| p[0][1] == p[1][1]) {
            let left = |row: &Vec<String>| row[11].parse::<usize>().expect("a count");
            assert_eq!(left(&pair[1]) + 1, left(&pair[0]));
        }
        let rounds = rows("rounds.csv");
        assert_eq!(rounds.last().expect("a round")[4], "0");
        // The border kept was checked and borne out.
        let kept: Vec<_> = rows("borders.csv")
            .into_iter()
            .filter(|b| b[11] == "1")
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0][5], "6");
    }

    #[test]
    fn a_block_is_given_contrast_saturation_and_black_point_as_well_as_a_gain() {
        // The left third is another capture: a stop darker — and flatter,
        // duller, under a veil.
        let mut observed = film(12, 6, |x, _| if x < 4 { [-1.0; 3] } else { [0.0; 3] });
        for (at, seen) in observed
            .tiles
            .iter_mut()
            .filter(|(at, _)| at.0 == 13 && at.1 < 104)
        {
            let _ = at;
            let middle = luma(seen.mean());
            for c in seen.cells.iter_mut().chain(seen.edges.iter_mut().flatten()) {
                let y = luma(*c).max(1e-6);
                // Half the swing about the middle, half the colour, a veil.
                let flat = middle * (y / middle).powf(0.5);
                *c = c.map(|v| flat + (v * flat / y - flat) * 0.5 + 0.002);
            }
        }
        let bounds = TileBounds::default();
        let (gains, report) = TileGains::solve(&observed, &bounds);
        assert_eq!(report.blocks, 2, "{report:?}");
        let dull = gains.of((13, 101, 202));
        assert!(dull.gain[1] > 1.6, "{dull:?}");
        assert!(
            dull.contrast > 1.15 && dull.contrast <= bounds.contrast_stops.exp2() + 0.01,
            "{dull:?}"
        );
        assert!(dull.saturation > 1.15 && dull.saturation <= bounds.saturation_stops.exp2() + 0.01);
        assert!(
            dull.black[1] > 0.0 && dull.black[1] * dull.gain[1] <= bounds.black + 1e-4,
            "{dull:?}"
        );
        // The film's own block is given nothing, and every tile of the
        // other the same grade, to the bit.
        assert!(gains.of((13, 108, 202)).is_identity());
        for y in 0..6 {
            for x in 0..4 {
                assert_eq!(gains.of((13, 100 + x, 200 + y)), dull);
            }
        }
        // Graded, a cell of it swings more about its middle than it did.
        let seen = &observed.tiles[&(13, 101, 202)];
        let swing = |grade: &Grade| {
            let light: Vec<f32> = seen
                .cells
                .iter()
                .map(|c| luma(grade.apply(*c)).log2())
                .collect();
            light.iter().copied().fold(f32::MIN, f32::max)
                - light.iter().copied().fold(f32::MAX, f32::min)
        };
        assert!(
            swing(&dull) > 1.1 * swing(&Grade::IDENTITY),
            "{}",
            swing(&dull)
        );
    }

    #[test]
    fn the_blocks_are_the_same_whatever_the_tiles_are_measured_by() {
        // The same film, its tiles measured by their moments and by their
        // transfer curves: the fit knows of a measure only its tone, so it
        // finds the same blocks and gives them the same gains.
        let observed = film(
            12,
            6,
            |x, _| if x < 4 { [-1.0, -1.0, -0.7] } else { [0.0; 3] },
        );
        let bounds = TileBounds::default();
        let (by_moments, report) = TileGains::solve_by(&observed, Measure::Moments, &bounds);
        let (by_curves, of_curves) = TileGains::solve_by(&observed, Measure::Curves, &bounds);
        assert_eq!(of_curves, report);
        assert_eq!(by_curves.stops, by_moments.stops);
        assert!(!by_curves.stops.is_empty());
        // What differs is the rest of what each block is given: as many
        // numbers as its measure has.
        for (gains, measure) in [
            (&by_moments, Measure::Moments),
            (&by_curves, Measure::Curves),
        ] {
            assert_eq!(gains.measure, measure);
            for rest in gains.rest.values() {
                assert_eq!(rest.len(), measure.len() - 3);
            }
        }
    }

    /// Two tiles side by side, each cell as `light` says for where it is
    /// across the pair (0 to 15) and down (0 to 7), in stops; the second
    /// tile's capture `seam` stops lighter.
    fn pair(light: impl Fn(usize, usize) -> f32, seam: f32) -> Observed {
        let mut observed = Observed::default();
        for (n, gain) in [(0usize, 0.0f32), (1, seam)] {
            let mut cells = [[0.0f32; 3]; GRID * GRID];
            for (k, c) in cells.iter_mut().enumerate() {
                // Ground has a colour: earth, here.
                let v = (light(n * GRID + k % GRID, k / GRID) + gain).exp2();
                *c = [v, 0.8 * v, 0.5 * v];
            }
            observed.tiles.insert(
                (13, 10 + n as u32, 20),
                TileSeen {
                    cells,
                    edges: [[[0.1; 3]; GRID]; 4],
                    usage: 1.0,
                    tones: None,
                    paired: None,
                },
            );
        }
        observed
    }

    fn met(observed: &Observed) -> Junction {
        observed
            .junction((13, 10, 20), (13, 11, 20), true)
            .expect("two tiles")
    }

    #[test]
    fn a_slope_of_the_ground_is_not_a_seam() {
        // Ground that darkens steadily from one tile into the next, a
        // tenth of a stop a cell: the two tiles are most of a stop apart as
        // wholes, their facing cells a tenth of a stop apart — and they
        // meet perfectly, because that is what the ground does there.
        let observed = pair(|x, _| -2.0 - 0.1 * x as f32, 0.0);
        let junction = met(&observed);
        assert!((junction.tiles - 0.1).abs() < 0.01, "{junction:?}");
        assert!(junction.step.abs() < 0.01, "{junction:?}");
        assert!(junction.apart() < 0.01);
    }

    #[test]
    fn a_step_that_is_in_neither_tile_is_a_seam_whatever_the_ground_does() {
        // The same slope, and the second tile a capture a stop lighter:
        // the edge steps by a stop more than the ground slopes.
        let observed = pair(|x, _| -2.0 - 0.1 * x as f32, 1.0);
        let junction = met(&observed);
        assert!((junction.step + 1.0).abs() < 0.02, "{junction:?}");
        assert_eq!(junction.coherence, 1.0);
        assert!((junction.apart() - 1.0).abs() < 0.02);
        // And on ground that is rough along the edge too: every cell still
        // steps the same way.
        let rough = pair(
            |x, y| -3.0 - 0.1 * x as f32 + 0.3 * ((y * 5) % 3) as f32,
            0.6,
        );
        assert!(
            (met(&rough).apart() - 0.6).abs() < 0.02,
            "{:?}",
            met(&rough)
        );
    }

    #[test]
    fn ground_that_steps_here_up_and_there_down_is_not_a_seam() {
        // Relief across the edge: lit slopes in some rows, shaded ones in
        // others. Large steps, and no one way to them.
        let observed = pair(
            |x, y| {
                if (x < GRID) == (y % 2 == 0) {
                    -2.0
                } else {
                    -3.0
                }
            },
            0.0,
        );
        let junction = met(&observed);
        assert!(junction.coherence < Junction::COHERENT, "{junction:?}");
        assert_eq!(junction.apart(), 0.0);
    }

    #[test]
    fn snow_on_one_side_of_an_edge_is_not_a_seam() {
        // One tile's last cells are under snow along the whole edge; the
        // other's are bare ground. Two stops between them, all the same
        // way — and it is the ground as it was, not a seam.
        let mut observed = pair(|_, _| -3.0, 0.0);
        let west = observed.tiles.get_mut(&(13, 10, 20)).expect("a tile");
        for k in 0..GRID {
            west.cells[k * GRID + GRID - 1] = [0.8, 0.8, 0.82];
        }
        let junction = met(&observed);
        assert_eq!(junction.apart(), 0.0, "{junction:?}");
        // The same two stops of bare, coloured ground would be one.
        let mut observed = pair(|_, _| -3.0, 0.0);
        let west = observed.tiles.get_mut(&(13, 10, 20)).expect("a tile");
        for k in 0..GRID {
            for depth in 0..2 {
                west.cells[k * GRID + GRID - 1 - depth] = [0.5, 0.4, 0.25];
            }
        }
        assert!(met(&observed).apart() > 1.5, "{:?}", met(&observed));
        // And snow along a part of the edge leaves the rest to speak.
        let mut observed = pair(|_, _| -3.0, 0.7);
        let west = observed.tiles.get_mut(&(13, 10, 20)).expect("a tile");
        for k in 0..3 {
            west.cells[k * GRID + GRID - 1] = [0.8, 0.8, 0.82];
        }
        assert!(
            (met(&observed).apart() - 0.7).abs() < 0.02,
            "{:?}",
            met(&observed)
        );
    }

    #[test]
    fn no_gain_goes_past_its_bounds() {
        // Three stops and a strong cast: more than a tile may be given.
        let observed = film(
            12,
            4,
            |x, _| if x < 4 { [-3.0, -3.0, -1.5] } else { [0.0; 3] },
        );
        let bounds = TileBounds::default();
        let (gains, report) = solved(&observed);
        let given = stops_of(&gains, 0, 0);
        let light = light_of(given);
        assert!(light <= bounds.light_stops + 1.0 / 32.0, "{given:?}");
        assert!(light > bounds.light_stops - 1.0 / 16.0, "{given:?}");
        for c in given {
            assert!(
                (c - light).abs() <= bounds.tint_stops + 1.0 / 32.0,
                "{given:?}"
            );
        }
        assert_eq!(report.held, 1, "{report:?}");
    }

    #[test]
    fn a_tile_whose_reference_was_not_seen_goes_with_its_neighbours() {
        let mut observed = film(12, 6, |x, _| if x < 4 { [-1.0; 3] } else { [0.0; 3] });
        // The reference under four tiles in the middle of the dark capture
        // is not there.
        observed.tiles.remove(&(REFERENCE, 50, 101));
        let (gains, report) = solved(&observed);
        assert_eq!(report.measured, 68, "{report:?}");
        let block = stops_of(&gains, 3, 0);
        assert!((block[1] - 1.0).abs() < 0.06, "{block:?}");
        // They are given their capture's gain, not left behind in it.
        assert_eq!(stops_of(&gains, 0, 2), block);
        assert_eq!(stops_of(&gains, 1, 3), block);
    }

    #[test]
    fn a_film_far_finer_than_the_reference_is_measured_against_it_all_the_same() {
        // Five levels down: a tile is less than a cell of the reference.
        let observed = film_at(17, 16, 8, |x, _| if x < 8 { [-1.0; 3] } else { [0.0; 3] });
        let (gains, report) = solved(&observed);
        assert_eq!(report.measured, 128, "{report:?}");
        assert_eq!(report.blocks, 2, "{report:?}");
        let (dark, light) = (
            gains
                .stops
                .get(&(17, 50 << 5, 100 << 5))
                .copied()
                .unwrap_or([0.0; 3]),
            gains
                .stops
                .get(&(17, (50 << 5) + 15, 100 << 5))
                .copied()
                .unwrap_or([0.0; 3]),
        );
        // Rough — but a stop is found to a third, and one side is kept.
        assert!(
            (dark[1] - light[1] - 1.0).abs() < 0.35,
            "{dark:?} {light:?}"
        );
        assert!(dark == [0.0; 3] || light == [0.0; 3]);
    }

    #[test]
    fn two_packs_of_a_film_seen_together_are_the_film() {
        let whole = film(12, 4, |x, _| if x < 4 { [-1.0; 3] } else { [0.0; 3] });
        // Cut in two, a column shared; each has the reference under it.
        let part = |from: u32, to: u32| Observed {
            tiles: whole
                .tiles
                .iter()
                .filter(|(at, _)| at.0 == REFERENCE || (100 + from..100 + to).contains(&at.1))
                .map(|(at, tile)| (*at, tile.clone()))
                .collect(),
        };
        let (west, east) = (part(0, 7), part(6, 12));
        let together = Observed::merged([&west, &east]);
        assert_eq!(together.tiles.len(), whole.tiles.len());
        // The shared column is draped by both.
        assert_eq!(together.tiles[&(13, 106, 200)].usage, 2.0);
        assert_eq!(solved(&together).0, solved(&whole).0);
    }

    /// Two neighbours over ground that goes on from one to the other,
    /// `ground` saying its light by column and row of the two together —
    /// the reference shows it as it is, the tiles through `east` on the
    /// right-hand one.
    fn against(ground: impl Fn(usize, usize) -> f32, east: f32) -> Observed {
        let mut observed = Observed::default();
        for (n, by) in [(0usize, 1.0f32), (1, east)] {
            let places: [[f32; 3]; PAIRS * PAIRS] = std::array::from_fn(|k| {
                let light = ground(n * PAIRS + k % PAIRS, k / PAIRS);
                [light, light * 1.1, light * 0.8]
            });
            let texels = vec![[0.1f32; 3]; 64 * 64];
            let mut seen = TileSeen::of_linear(&texels, 64).expect("a tile");
            seen.paired = Some(Box::new(Paired {
                tile: places.map(|c| c.map(|v| v * by)),
                reference: places,
            }));
            observed.see((13, 10 + n as u32, 20), || Some(seen), 1.0);
        }
        observed
    }

    #[test]
    fn a_step_the_reference_does_not_show_is_a_seam_and_one_it_shows_is_not() {
        let rolling =
            |x: usize, y: usize| 0.08 + 0.03 * ((x as f32 * 0.7).sin() + (y as f32 * 0.5).cos());
        // One capture: nothing between the two but the ground.
        let met = |observed: &Observed| {
            observed
                .junction_against((13, 10, 20), (13, 11, 20), true)
                .expect("both set against it")
        };
        assert_eq!(met(&against(rolling, 1.0)).apart(), 0.0);
        // The right-hand tile a stop darker: a seam of a stop, all along.
        let seam = met(&against(rolling, 0.5));
        assert!(
            (seam.step - 1.0).abs() < 0.05 && seam.coherence == 1.0,
            "{seam:?}"
        );
        assert!((seam.apart() - 1.0).abs() < 0.05);
        // The ground itself a stop darker east of the edge — a forest's
        // edge, a shore — in the film and in the reference alike: the two
        // tiles step by a stop, and it is no seam.
        let shore = met(&against(
            |x, y| rolling(x, y) * if x < PAIRS { 1.0 } else { 0.5 },
            1.0,
        ));
        assert!(shore.tiles.abs() > 0.9, "{shore:?}");
        assert_eq!(shore.apart(), 0.0, "{shore:?}");
        // A seam blended over the places nearest the edge, as a mosaic
        // blends them: between the places that face each other hardly a
        // step, and a stop all the same.
        let mut blended = against(rolling, 0.5);
        let east = blended.tiles.get_mut(&(13, 11, 20)).expect("a tile");
        let paired = east.paired.as_deref_mut().expect("its places");
        for k in 0..PAIRS {
            paired.tile[k * PAIRS] = paired.tile[k * PAIRS].map(|v| v * 1.9);
            paired.tile[k * PAIRS + 1] = paired.tile[k * PAIRS + 1].map(|v| v * 1.3);
        }
        let seam = met(&blended);
        assert!((seam.apart() - 1.0).abs() < 0.05, "{seam:?}");
        // Tiles that were set against nothing cannot be read this way.
        let mut alone = against(rolling, 0.5);
        for tile in alone.tiles.values_mut() {
            tile.paired = None;
        }
        assert_eq!(
            alone.junction_against((13, 10, 20), (13, 11, 20), true),
            None
        );
    }

    #[test]
    fn a_tile_set_against_a_reference_is_read_back_with_it() {
        let texels: Vec<[f32; 3]> = (0..64 * 64)
            .map(|k| [0.05 + 0.1 * noise(1, k % 64, k / 64), 0.1, 0.07])
            .collect();
        let mut seen = TileSeen::of_linear(&texels, 64).expect("a tile");
        // A reference over the left half only, each place by where it is.
        seen.set_against(|u0, v0, u1, _| (u1 <= 0.5).then_some([u0 + 0.01, v0 + 0.01, 0.2]));
        let mut observed = Observed::default();
        observed.see((13, 4, 5), || Some(seen.clone()), 2.0);
        let read = Observed::from_bytes(&observed.to_bytes()).expect("read back");
        let (was, is) = (
            seen.paired.as_deref().expect("its places"),
            read.tiles[&(13, 4, 5)].paired.as_deref().expect("kept"),
        );
        for k in 0..PAIRS * PAIRS {
            for c in 0..3 {
                for (a, b) in [
                    (was.tile[k][c], is.tile[k][c]),
                    (was.reference[k][c], is.reference[k][c]),
                ] {
                    assert!(
                        a.is_nan() && b.is_nan() || (a / b).log2().abs() < 0.001,
                        "{k}: {a} {b}"
                    );
                }
            }
        }
        assert!(is.reference[3][0].is_finite() && is.reference[PAIRS - 1][0].is_nan());
        assert_eq!(
            is.reference[PAIRS + 2][1],
            unpacked(packed(1.0 / PAIRS as f32 + 0.01))
        );
    }

    #[test]
    fn what_was_seen_is_read_back_and_solves_the_same() {
        let observed = film(
            12,
            4,
            |x, _| if x < 4 { [-0.8, -0.6, -0.9] } else { [0.0; 3] },
        );
        let read = Observed::from_bytes(&observed.to_bytes()).expect("read back");
        assert_eq!(read.tiles.len(), observed.tiles.len());
        let (gains, _) = solved(&observed);
        assert!(!gains.stops.is_empty());
        assert_eq!(solved(&read).0, gains);
        assert_eq!(Observed::from_bytes(b"not one"), None);
        let mut cut = observed.to_bytes();
        cut.pop();
        assert_eq!(Observed::from_bytes(&cut), None);
    }
}
