// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! From a tile's tone to its parent's: a gain per block, and how far each
//! is to be believed.

use crate::stats::{BlockStats, BLOCKS};

/// What decides a transfer. Part of what a field is the field *of*: a field
/// computed under other parameters is another field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Bumped when the computation changes in a way that changes fields.
    pub version: u32,
    /// Under this many stops of gain, on every channel…
    pub dead_zone_stops: f32,
    /// …and over this agreement of structure, two levels are one source
    /// resampled, and the transfer is nothing at all.
    pub dead_zone_agreement: f32,
    /// No field moves a texel by more than this many stops.
    pub clamp_stops: f32,
    /// A block counts only if this share of its texels did, on both sides.
    pub min_valid: f32,
    /// How many times the field is relaxed toward its neighbours.
    pub smoothing_passes: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            version: 1,
            dead_zone_stops: 0.06,
            dead_zone_agreement: 0.98,
            clamp_stops: 1.5,
            min_valid: 0.5,
            smoothing_passes: 6,
        }
    }
}

/// The gain that brings a tile's tone to its parent's, block by block.
#[derive(Debug, Clone, PartialEq)]
pub struct Transfer {
    /// `log2(parent / child)` per block and channel, in stops. Row-major.
    pub gain: Vec<[f32; 3]>,
    /// How far each block's gain is believed, 0 to 1: nothing where too few
    /// texels counted, little where the block disagrees with the tile — the
    /// ground changed there, not the light.
    pub confidence: Vec<f32>,
    /// The tile's gain as a whole: the median of its blocks'.
    pub overall: [f32; 3],
    /// How alike the two tones are in shape — the correlation of their
    /// blocks' luminances — from −1 to 1. Near 1: the same picture.
    pub agreement: f32,
    /// Whether the two are one source resampled. The transfer is then
    /// nothing, exactly, so that a chain of them adds up to nothing.
    pub same_source: bool,
}

fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The transfer from `child` to `parent`: the parent's tone over the ground
/// the child covers ([`BlockStats::of_quadrant`]).
pub fn transfer(child: &BlockStats, parent: &BlockStats, params: &Params) -> Transfer {
    let blocks = BLOCKS * BLOCKS;
    let usable: Vec<bool> = (0..blocks)
        .map(|b| {
            child.valid[b] >= params.min_valid
                && parent.valid[b] >= params.min_valid
                && child.mean[b].iter().all(|c| *c > 0.0)
                && parent.mean[b].iter().all(|c| *c > 0.0)
        })
        .collect();
    let mut gain = vec![[0.0f32; 3]; blocks];
    for b in (0..blocks).filter(|b| usable[*b]) {
        for c in 0..3 {
            gain[b][c] = (parent.mean[b][c] / child.mean[b][c]).log2();
        }
    }
    let mut overall = [0.0f32; 3];
    for c in 0..3 {
        let mut values: Vec<f32> = (0..blocks)
            .filter(|b| usable[*b])
            .map(|b| gain[b][c])
            .collect();
        overall[c] = median(&mut values);
    }

    // How far each block is from the tile's gain, against how far blocks
    // usually are: a block far out is ground that changed.
    let off: Vec<f32> = (0..blocks)
        .map(|b| {
            (0..3)
                .map(|c| (gain[b][c] - overall[c]).abs())
                .fold(0.0, f32::max)
        })
        .collect();
    let mut spread: Vec<f32> = (0..blocks).filter(|b| usable[*b]).map(|b| off[b]).collect();
    let scale = (median(&mut spread) * 2.5).max(0.05);
    let confidence: Vec<f32> = (0..blocks)
        .map(|b| {
            if !usable[b] {
                return 0.0;
            }
            let far = off[b] / scale;
            child.valid[b].min(parent.valid[b]) / (1.0 + far * far)
        })
        .collect();

    // Agreement of shape: do the two tones rise and fall together.
    let pairs: Vec<(f32, f32)> = (0..blocks)
        .filter(|b| usable[*b])
        .map(|b| {
            (
                luminance(child.mean[b]).ln(),
                luminance(parent.mean[b]).ln(),
            )
        })
        .collect();
    let agreement = if pairs.len() < 8 {
        0.0
    } else {
        let n = pairs.len() as f32;
        let (ma, mb) = (
            pairs.iter().map(|p| p.0).sum::<f32>() / n,
            pairs.iter().map(|p| p.1).sum::<f32>() / n,
        );
        let (mut ab, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
        for (a, b) in &pairs {
            ab += (a - ma) * (b - mb);
            aa += (a - ma) * (a - ma);
            bb += (b - mb) * (b - mb);
        }
        if aa <= f32::EPSILON || bb <= f32::EPSILON {
            // A flat tile agrees with a flat tile.
            1.0
        } else {
            ab / (aa * bb).sqrt()
        }
    };

    let same_source = overall.iter().all(|g| g.abs() < params.dead_zone_stops)
        && agreement >= params.dead_zone_agreement
        && spread.last().copied().unwrap_or(0.0) < params.dead_zone_stops * 2.0;
    Transfer {
        gain,
        confidence,
        overall,
        agreement,
        same_source,
    }
}
