// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile-source seam: the abstraction the SSE traversal consumes, so it
//! drives 3D Tiles and quantized-mesh terrain with the same code.
//!
//! [`TileId`] is an opaque `u64` handle whose meaning is private to each
//! source: an arena index for a [`crate::tileset::Tileset`], an encoded
//! `(level, x, y)` for a terrain quadtree. The two are never mixed (one
//! geometry server has one source), so no global tag is needed.
//!
//! [`TileTree`] is pure and synchronous — the traversal walks it every frame.
//! Loading (async I/O + decode) is a separate concern handled by the runtime.

use crate::math::BoundingVolume;
use crate::tileset::Refine;

/// Opaque tile handle. Interpreted by the owning [`TileTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId(pub u64);

impl TileId {
    /// Encodes a terrain quadtree coordinate `(level, x, y)` into a handle.
    /// Layout: level in bits 56.., x in bits 28..56, y in bits 0..28.
    pub fn from_terrain(level: u32, x: u64, y: u64) -> Self {
        debug_assert!(level < 256 && x < (1 << 28) && y < (1 << 28));
        TileId(((level as u64) << 56) | ((x & 0xFFF_FFFF) << 28) | (y & 0xFFF_FFFF))
    }

    /// Decodes a terrain handle back to `(level, x, y)`.
    pub fn terrain_coord(self) -> (u32, u64, u64) {
        (
            (self.0 >> 56) as u32,
            (self.0 >> 28) & 0xFFF_FFFF,
            self.0 & 0xFFF_FFFF,
        )
    }

    /// As an arena index (for [`Tileset`]-backed sources).
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Topological properties of a tile — everything the traversal needs to make
/// a refine/render/cull decision, without loading the tile.
#[derive(Debug, Clone, Copy)]
pub struct TileProperties {
    /// Bounding volume in world (ECEF) coordinates.
    pub bounding_volume: BoundingVolume,
    /// Geometric error in meters.
    pub geometric_error: f64,
    pub refine: Refine,
    /// Whether the tile has renderable content (vs a structural empty tile).
    pub has_content: bool,
}

/// The tile hierarchy as the traversal sees it: pure, synchronous, walked
/// every frame. Implemented by [`crate::tileset::Tileset`] (3D Tiles) and by
/// terrain quadtrees (`tuile-terrain`).
pub trait TileTree {
    /// Root tiles (one for a 3D Tiles tileset, several for a global terrain).
    fn roots(&self) -> Vec<TileId>;

    /// Children of a tile (already filtered by availability for terrain).
    fn children(&self, id: TileId) -> Vec<TileId>;

    /// Topological properties of a tile.
    fn properties(&self, id: TileId) -> TileProperties;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_handle_round_trips() {
        for (l, x, y) in [
            (0u32, 0u64, 0u64),
            (19, 541, 386),
            (12, 1083, 773),
            (255, 1, 1),
        ] {
            let id = TileId::from_terrain(l, x, y);
            assert_eq!(id.terrain_coord(), (l, x, y));
        }
    }

    #[test]
    fn arena_index_round_trips() {
        let id = TileId(12345);
        assert_eq!(id.index(), 12345);
    }
}
