// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The deterministic cost estimator, shared by the budget and the consumer.
//!
//! A [`Cost`] has two parts: the bytes a resource keeps on the GPU (and, on
//! unified memory, the CPU copy kept beside it), and the transient staging
//! bytes its upload goes through, which count against the delivery credit.
//! The consumer reports the actual GPU bytes in its acknowledgement; a drift
//! above 5 % means this estimator is wrong.
//!
//! What it accounts for, as wgpu lays it out:
//! - texture copies with `bytes_per_row` aligned to 256
//!   (`COPY_BYTES_PER_ROW_ALIGNMENT`), mip by mip, in the staging bytes;
//! - the full mip chain, level by level (about 4/3 of the base);
//! - block-compressed formats by whole 4×4 blocks;
//! - buffer sizes aligned to 4 (`COPY_BUFFER_ALIGNMENT`), and the per-tile
//!   uniform to 256 (the minimum uniform offset alignment).

use super::types::Bytes;

/// The per-tile uniform block, before alignment.
pub const TILE_UNIFORM_BYTES: u64 = 8_272;
/// wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT`.
pub const ROW_ALIGNMENT: u64 = 256;
/// wgpu's `COPY_BUFFER_ALIGNMENT`.
pub const BUFFER_ALIGNMENT: u64 = 4;
/// The minimum uniform buffer offset alignment wgpu guarantees.
pub const UNIFORM_ALIGNMENT: u64 = 256;

/// What a resource costs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cost {
    /// Bytes held while resident (GPU, plus the CPU copy on unified memory).
    pub gpu: Bytes,
    /// Transient bytes of the upload (staging), against the delivery credit.
    pub staging: Bytes,
}

impl Cost {
    /// A cost whose staging equals its resident bytes.
    pub const fn flat(b: Bytes) -> Self {
        Cost { gpu: b, staging: b }
    }
}

const fn align(v: u64, to: u64) -> u64 {
    v.div_ceil(to).saturating_mul(to)
}

/// A mesh: vertex and index buffers and the per-tile uniform, aligned, plus
/// the CPU copy kept for stitching when `cpu_copy` (unified memory pays it).
pub fn mesh_cost(vertices: u64, stride: u64, indices: u64, cpu_copy: bool) -> Cost {
    let vb = align(vertices.saturating_mul(stride), BUFFER_ALIGNMENT);
    let ib = align(indices.saturating_mul(4), BUFFER_ALIGNMENT);
    let uniform = align(TILE_UNIFORM_BYTES, UNIFORM_ALIGNMENT);
    let staging = vb.saturating_add(ib).saturating_add(uniform);
    let copy = if cpu_copy { vb.saturating_add(ib) } else { 0 };
    Cost {
        gpu: Bytes(staging.saturating_add(copy)),
        staging: Bytes(staging),
    }
}

/// A texture's pixel format, for its byte cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TextureFormat {
    /// 4 bytes per texel.
    Rgba8,
    /// ASTC 4×4 or BC7: 16 bytes per 4×4 block.
    Block16,
    /// BC1: 8 bytes per 4×4 block.
    Block8,
}

/// A drape texture `w × h` with its full mip chain.
pub fn drape_cost(w: u32, h: u32, format: TextureFormat) -> Cost {
    let (mut w, mut h) = (u64::from(w.max(1)), u64::from(h.max(1)));
    let (mut gpu, mut staging) = (0u64, 0u64);
    loop {
        let (row, rows) = match format {
            TextureFormat::Rgba8 => (w * 4, h),
            TextureFormat::Block16 => (w.div_ceil(4) * 16, h.div_ceil(4)),
            TextureFormat::Block8 => (w.div_ceil(4) * 8, h.div_ceil(4)),
        };
        gpu = gpu.saturating_add(row.saturating_mul(rows));
        staging = staging.saturating_add(align(row, ROW_ALIGNMENT).saturating_mul(rows));
        if w == 1 && h == 1 {
            break;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    Cost {
        gpu: Bytes(gpu),
        staging: Bytes(staging),
    }
}

/// Whether `actual` is within 5 % of `estimate`.
pub fn within_drift(estimate: Bytes, actual: Bytes) -> bool {
    let (e, a) = (u128::from(estimate.0), u128::from(actual.0));
    e.abs_diff(a) * 100 <= e * 5
}
