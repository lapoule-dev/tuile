// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The field of gains a tile is corrected by, and its application.

use crate::stats::{linear_of, BLOCKS};
use crate::transfer::{Params, Transfer};

/// Nodes along a side: one at every corner of the sixteen blocks, the outer
/// ones on the tile's own edges — so a value on an edge is read off the same
/// node by the tile on either side of a quadrant boundary.
pub const LATTICE: usize = BLOCKS + 1;

/// The weight of a tile's overall gain at each node, against the blocks
/// around it: a twentieth of one fully believed block.
const PRIOR: f32 = 0.05;

/// A stored unit is a 1024th of a stop: fixed point, so a field is the same
/// bytes wherever it was computed, and finer than any byte of colour.
pub const STOPS_PER_UNIT: f32 = 1.0 / 1024.0;

/// A field of gains over a tile: `LATTICE × LATTICE` nodes of three channels,
/// in stops, interpolated between. All zero is "leave the tile as it is".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GainField {
    /// Row-major; node `(0, 0)` is the tile's top-left corner.
    pub nodes: Vec<[i16; 3]>,
}

impl GainField {
    /// The field that changes nothing.
    pub fn identity() -> Self {
        Self {
            nodes: vec![[0; 3]; LATTICE * LATTICE],
        }
    }

    pub fn is_identity(&self) -> bool {
        self.nodes.iter().all(|n| *n == [0; 3])
    }

    /// The field that brings a tile's tone to its parent's: the transfer's
    /// gains carried to the lattice, relaxed toward their neighbours where
    /// they are little believed, and bounded.
    pub fn toward_parent(transfer: &Transfer, params: &Params) -> Self {
        if transfer.same_source {
            return Self::identity();
        }
        // A node is the believed mean of the blocks around it.
        let mut value = vec![[0.0f32; 3]; LATTICE * LATTICE];
        let mut trust = vec![0.0f32; LATTICE * LATTICE];
        for j in 0..LATTICE {
            for i in 0..LATTICE {
                let (mut sum, mut weight) = ([0.0f32; 3], 0.0f32);
                for (bj, bi) in [
                    (j.wrapping_sub(1), i.wrapping_sub(1)),
                    (j.wrapping_sub(1), i),
                    (j, i.wrapping_sub(1)),
                    (j, i),
                ] {
                    if bj >= BLOCKS || bi >= BLOCKS {
                        continue;
                    }
                    let w = transfer.confidence[bj * BLOCKS + bi];
                    for c in 0..3 {
                        sum[c] += transfer.gain[bj * BLOCKS + bi][c] * w;
                    }
                    weight += w;
                }
                let node = j * LATTICE + i;
                trust[node] = weight.min(1.0);
                // The tile's own gain is a prior of small weight: a node
                // among believed blocks is theirs, and a node among blocks
                // nobody believes starts from the tile's gain rather than
                // from what those blocks say.
                for c in 0..3 {
                    value[node][c] = (sum[c] + transfer.overall[c] * PRIOR) / (weight + PRIOR);
                }
            }
        }
        // Relaxation: a node keeps its own value as far as it is trusted and
        // takes its neighbours' mean for the rest. A patch of changed ground
        // is filled in from around it; a believed gradient stays.
        for _ in 0..params.smoothing_passes {
            let before = value.clone();
            for j in 0..LATTICE {
                for i in 0..LATTICE {
                    let (mut sum, mut count) = ([0.0f32; 3], 0.0f32);
                    for (dj, di) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                        let (nj, ni) = (j as i32 + dj, i as i32 + di);
                        if nj < 0 || ni < 0 || nj >= LATTICE as i32 || ni >= LATTICE as i32 {
                            continue;
                        }
                        let n = before[nj as usize * LATTICE + ni as usize];
                        for c in 0..3 {
                            sum[c] += n[c];
                        }
                        count += 1.0;
                    }
                    let node = j * LATTICE + i;
                    // Never wholly its own: a field is smooth by construction.
                    let own = trust[node] * 0.75;
                    for c in 0..3 {
                        value[node][c] = own * before[node][c] + (1.0 - own) * sum[c] / count;
                    }
                }
            }
        }
        Self {
            nodes: value
                .iter()
                .map(|v| {
                    v.map(|stops| quantised(stops.clamp(-params.clamp_stops, params.clamp_stops)))
                })
                .collect(),
        }
    }

    /// This field — a tile's toward its parent — followed by the parent's
    /// own: the tile's field toward the anchor. `quadrant` is where the tile
    /// sits in its parent, `(0, 0)` top-left.
    pub fn chained(&self, parent: &GainField, quadrant: (u32, u32), params: &Params) -> Self {
        if parent.is_identity() {
            return self.clone();
        }
        let limit = quantised(params.clamp_stops);
        let mut nodes = Vec::with_capacity(LATTICE * LATTICE);
        for j in 0..LATTICE {
            for i in 0..LATTICE {
                // The node's place in the parent: half a tile, in the
                // quadrant.
                let u = (quadrant.0 as f32 + i as f32 / BLOCKS as f32) / 2.0;
                let v = (quadrant.1 as f32 + j as f32 / BLOCKS as f32) / 2.0;
                let under = parent.at(u, v);
                let own = self.nodes[j * LATTICE + i];
                let mut sum = [0i16; 3];
                for c in 0..3 {
                    sum[c] = (i32::from(own[c]) + i32::from(quantised(under[c])))
                        .clamp(-i32::from(limit), i32::from(limit))
                        as i16;
                }
                nodes.push(sum);
            }
        }
        Self { nodes }
    }

    /// The gain at a point of the tile, in stops; `(0, 0)` is its top-left
    /// corner and `(1, 1)` its bottom-right.
    pub fn at(&self, u: f32, v: f32) -> [f32; 3] {
        let x = u.clamp(0.0, 1.0) * BLOCKS as f32;
        let y = v.clamp(0.0, 1.0) * BLOCKS as f32;
        let (i, j) = ((x as usize).min(BLOCKS - 1), (y as usize).min(BLOCKS - 1));
        let (fx, fy) = (x - i as f32, y - j as f32);
        let node =
            |j: usize, i: usize| self.nodes[j * LATTICE + i].map(|n| f32::from(n) * STOPS_PER_UNIT);
        let (a, b, c, d) = (
            node(j, i),
            node(j, i + 1),
            node(j + 1, i),
            node(j + 1, i + 1),
        );
        let mut out = [0.0f32; 3];
        for k in 0..3 {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bottom = c[k] + (d[k] - c[k]) * fx;
            out[k] = top + (bottom - top) * fy;
        }
        out
    }

    /// The field as it is stored: its nodes, little-endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.nodes
            .iter()
            .flatten()
            .flat_map(|n| n.to_le_bytes())
            .collect()
    }

    /// A stored field, or `None` if these are not the bytes of one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != LATTICE * LATTICE * 6 {
            return None;
        }
        Some(Self {
            nodes: bytes
                .chunks_exact(6)
                .map(|n| {
                    [
                        i16::from_le_bytes([n[0], n[1]]),
                        i16::from_le_bytes([n[2], n[3]]),
                        i16::from_le_bytes([n[4], n[5]]),
                    ]
                })
                .collect(),
        })
    }
}

fn quantised(stops: f32) -> i16 {
    (stops / STOPS_PER_UNIT)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// The sRGB byte a linear value is stored as.
fn byte_of(linear: f32) -> u8 {
    let c = linear.clamp(0.0, 1.0);
    let encoded = if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5) as u8
}

/// Applies a field to an image of tightly packed RGBA8, in place: each texel
/// multiplied, in linear light, by the gain the field holds there, taken at
/// `strength` (0: nothing; 1: all of it). Alpha is not touched.
///
/// A field that is the identity, or a strength of zero, leaves every byte as
/// it was — not nearly, exactly: nothing is decoded and encoded again.
pub fn apply(rgba: &mut [u8], width: u32, height: u32, field: &GainField, strength: f32) {
    if field.is_identity() || strength <= 0.0 {
        return;
    }
    for y in 0..height {
        let v = (y as f32 + 0.5) / height as f32;
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            let stops = field.at(u, v);
            let i = ((y * width + x) * 4) as usize;
            let Some(texel) = rgba.get_mut(i..i + 3) else {
                return;
            };
            for c in 0..3 {
                let gain = (stops[c] * strength).exp2();
                texel[c] = byte_of(linear_of(texel[c]) * gain);
            }
        }
    }
}
