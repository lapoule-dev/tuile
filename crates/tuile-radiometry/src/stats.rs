// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A tile's tone: the mean of its texels, block by block, in linear light.

/// Blocks along a side. A 256-texel tile is cut into 16-texel blocks, which
/// its parent — covering the same ground with half the texels — answers with
/// 8-texel blocks of the matching quadrant.
pub const BLOCKS: usize = 16;

/// A texel this dark or this bright says little of the ground under it: it
/// is shadow, water, or a sensor at its limit, and a ratio taken on it is
/// noise. Left out of the means.
const TOO_DARK: u8 = 6;
const TOO_BRIGHT: u8 = 250;

/// The value an sRGB byte stands for, in linear light.
///
/// By the curve, not by a table of somebody's rounding: 256 values, computed
/// once, the same wherever this is compiled to.
pub fn linear_of(byte: u8) -> f32 {
    static TABLE: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0.0f32; 256];
        for (value, slot) in table.iter_mut().enumerate() {
            let c = value as f64 / 255.0;
            *slot = if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            } as f32;
        }
        table
    })[byte as usize]
}

/// The tone of a tile: per block, the mean linear colour of the texels that
/// count, and how many of the block's texels did.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockStats {
    /// Row-major, `BLOCKS × BLOCKS`.
    pub mean: Vec<[f32; 3]>,
    /// The share of each block's texels that went into its mean, 0 to 1.
    pub valid: Vec<f32>,
}

impl BlockStats {
    /// The tone of a whole image of tightly packed RGBA8.
    pub fn of(rgba: &[u8], width: u32, height: u32) -> Self {
        Self::of_region(rgba, width, (0, 0), (width, height))
    }

    /// The tone of one quadrant of an image: `(0, 0)` is its top-left
    /// quarter. What a parent has to say of the ground one child covers.
    pub fn of_quadrant(rgba: &[u8], width: u32, height: u32, quadrant: (u32, u32)) -> Self {
        let (w, h) = (width / 2, height / 2);
        Self::of_region(rgba, width, (quadrant.0 * w, quadrant.1 * h), (w, h))
    }

    fn of_region(rgba: &[u8], stride: u32, origin: (u32, u32), size: (u32, u32)) -> Self {
        let mut mean = vec![[0.0f32; 3]; BLOCKS * BLOCKS];
        let mut valid = vec![0.0f32; BLOCKS * BLOCKS];
        for by in 0..BLOCKS as u32 {
            for bx in 0..BLOCKS as u32 {
                // Block edges by proportion, so any size divides into the
                // same sixteen.
                let x0 = origin.0 + bx * size.0 / BLOCKS as u32;
                let x1 = origin.0 + (bx + 1) * size.0 / BLOCKS as u32;
                let y0 = origin.1 + by * size.1 / BLOCKS as u32;
                let y1 = origin.1 + (by + 1) * size.1 / BLOCKS as u32;
                let (mut sum, mut counted) = ([0.0f64; 3], 0u32);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = ((y * stride + x) * 4) as usize;
                        let Some(texel) = rgba.get(i..i + 3) else {
                            continue;
                        };
                        let (brightest, darkest) = (
                            texel.iter().copied().max().unwrap_or(0),
                            texel.iter().copied().min().unwrap_or(0),
                        );
                        if brightest <= TOO_DARK || darkest >= TOO_BRIGHT {
                            continue;
                        }
                        for c in 0..3 {
                            sum[c] += f64::from(linear_of(texel[c]));
                        }
                        counted += 1;
                    }
                }
                let block = (by as usize) * BLOCKS + bx as usize;
                let texels = ((x1 - x0) * (y1 - y0)).max(1);
                valid[block] = counted as f32 / texels as f32;
                if counted > 0 {
                    for c in 0..3 {
                        mean[block][c] = (sum[c] / f64::from(counted)) as f32;
                    }
                }
            }
        }
        Self { mean, valid }
    }
}
