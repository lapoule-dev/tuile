// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile handle, [`TileId`]: an opaque `u64` the budget keys its
//! resources by and tuile-core's tile sources interpret.
//!
//! It lives here, at the bottom of the graph, so the budget depends on
//! nothing above it; tuile-core re-exports it from its `source` module, where
//! the layout below is described for the traversal and the loaders.
//!
//! - bits 60..64 — a **source tag** (0..[`MAX_SOURCES`]), routing between the
//!   sub-sources of a composite source (0 for a standalone source);
//! - bits 0..60 — the source-private payload (an arena index for a 3D Tiles
//!   tileset, an encoded `(level, x, y)` for a terrain quadtree).

const TAG_SHIFT: u32 = 60;
const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;
/// Maximum number of sub-sources a composite source (tuile-core's
/// `CompositeTileTree`) can combine.
pub const MAX_SOURCES: usize = 1 << (64 - TAG_SHIFT);

// Terrain payload layout within the 60 payload bits.
const T_LEVEL_SHIFT: u32 = 54;
const T_X_SHIFT: u32 = 27;
const T_COORD_MASK: u64 = (1 << 27) - 1;

/// Opaque tile handle. The payload is interpreted by the owning `TileTree`;
/// the tag routes between sub-sources in a `CompositeTileTree` (both in
/// tuile-core's `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId(pub u64);

impl TileId {
    /// Encodes a terrain quadtree coordinate `(level, x, y)` (level < 64,
    /// x/y < 2^27 — well beyond CWT's max zoom).
    pub fn from_terrain(level: u32, x: u64, y: u64) -> Self {
        debug_assert!(level < 64 && x <= T_COORD_MASK && y <= T_COORD_MASK);
        TileId(
            ((level as u64) << T_LEVEL_SHIFT)
                | ((x & T_COORD_MASK) << T_X_SHIFT)
                | (y & T_COORD_MASK),
        )
    }

    /// Decodes a terrain payload back to `(level, x, y)` (tag ignored).
    // Moved verbatim from tuile-core's `source`, outside the budget's
    // determinism lints when it was written: no behaviour change here.
    #[allow(clippy::cast_possible_truncation)]
    pub fn terrain_coord(self) -> (u32, u64, u64) {
        let p = self.0 & PAYLOAD_MASK;
        (
            (p >> T_LEVEL_SHIFT) as u32,
            (p >> T_X_SHIFT) & T_COORD_MASK,
            p & T_COORD_MASK,
        )
    }

    /// As an arena index (for `Tileset`-backed sources). Ignores the tag.
    // Moved verbatim from tuile-core's `source`, outside the budget's
    // determinism lints when it was written: no behaviour change here.
    #[allow(clippy::cast_possible_truncation)]
    pub fn index(self) -> usize {
        (self.0 & PAYLOAD_MASK) as usize
    }

    /// The source tag (0..[`MAX_SOURCES`]).
    // Moved verbatim from tuile-core's `source`, outside the budget's
    // determinism lints when it was written: no behaviour change here.
    #[allow(clippy::cast_possible_truncation)]
    pub fn tag(self) -> u8 {
        (self.0 >> TAG_SHIFT) as u8
    }

    /// The payload with the tag cleared (what a sub-source sees).
    pub fn payload(self) -> TileId {
        TileId(self.0 & PAYLOAD_MASK)
    }

    /// This handle re-tagged for source `tag`.
    pub fn with_tag(self, tag: u8) -> TileId {
        debug_assert!((tag as usize) < MAX_SOURCES);
        TileId((self.0 & PAYLOAD_MASK) | ((tag as u64) << TAG_SHIFT))
    }
}
