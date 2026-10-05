// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where what was loaded came from.
//!
//! A loader hands back finished things: a mesh, a stack of imagery layers.
//! What they were made from is gone by then — the mesh of a tile the source
//! does not have was cut from an ancestor's, and nothing in it says which —
//! and a consumer that wants to build the same things again from the same
//! source tiles has to be told. This is the telling: noted by the loader as
//! it fetches, read by whoever asks.
//!
//! It records identities and digests, never bytes, and it only grows: a tile
//! evicted from a cache is still a tile that was built from what it was
//! built from.

use std::collections::HashMap;
use std::sync::Mutex;

use tuile_core::raster::ImageryCoord;
use tuile_terrain::{GeographicTilingScheme, TileCoord};

/// FNV-1a, 64 bits, over a tile's bytes as its source served them.
///
/// The digest a reference to a source tile carries: cheap, and enough to say
/// "these are not the bytes that were here" — which is all it is for.
pub fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The terrain tile a mesh was built from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainOrigin {
    /// The tile whose bytes were decoded: the tile itself, or the nearest
    /// ancestor the source has, from which the mesh was cut.
    pub source: TileCoord,
    /// [`digest`] of those bytes.
    pub digest: u64,
    /// How deep the mesh's skirts hang, in metres.
    pub skirt_height: f64,
}

/// What a loader noted of its sources. Shared with the loader; see
/// [`crate::globe_with_provenance`].
#[derive(Default)]
pub struct Provenance {
    terrain: Mutex<HashMap<TileCoord, (TileCoord, u64)>>,
    imagery: Mutex<HashMap<ImageryCoord, u64>>,
}

impl Provenance {
    /// Where the mesh of `tile` came from, once the loader has built it.
    pub fn terrain(&self, tile: TileCoord) -> Option<TerrainOrigin> {
        let (source, digest) = *self.terrain.lock().ok()?.get(&tile)?;
        let rect = GeographicTilingScheme::default().tile_rect(tile);
        Some(TerrainOrigin {
            source,
            digest,
            skirt_height: tuile_terrain::skirt_height(&rect),
        })
    }

    /// The digest of an imagery tile's bytes, once the loader has fetched it.
    pub fn imagery(&self, tile: ImageryCoord) -> Option<u64> {
        self.imagery.lock().ok()?.get(&tile).copied()
    }

    /// `tile` was decoded from its own bytes.
    pub(crate) fn fetched_terrain(&self, tile: TileCoord, bytes: &[u8]) {
        if let Ok(mut terrain) = self.terrain.lock() {
            terrain.insert(tile, (tile, digest(bytes)));
        }
    }

    /// `tile` was cut from `parent`'s mesh, so it comes from wherever that
    /// did.
    pub(crate) fn upsampled_terrain(&self, tile: TileCoord, parent: TileCoord) {
        if let Ok(mut terrain) = self.terrain.lock() {
            if let Some(origin) = terrain.get(&parent).copied() {
                terrain.insert(tile, origin);
            }
        }
    }

    pub(crate) fn fetched_imagery(&self, tile: ImageryCoord, bytes: &[u8]) {
        if let Ok(mut imagery) = self.imagery.lock() {
            imagery.insert(tile, digest(bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_cut_from_an_ancestor_names_the_ancestor() {
        let noted = Provenance::default();
        let (root, child, grandchild) = (
            TileCoord::new(3, 5, 2),
            TileCoord::new(4, 10, 4),
            TileCoord::new(5, 21, 9),
        );
        assert_eq!(noted.terrain(root), None);
        noted.fetched_terrain(root, b"bytes of the root");
        noted.upsampled_terrain(child, root);
        noted.upsampled_terrain(grandchild, child);
        let origin = noted.terrain(grandchild).expect("noted");
        assert_eq!(origin.source, root);
        assert_eq!(origin.digest, digest(b"bytes of the root"));
        // The skirt is the drawn tile's, not its ancestor's: it hangs from
        // the edges of the tile that is drawn.
        assert!(origin.skirt_height < noted.terrain(root).expect("noted").skirt_height);
        // A tile cut from a parent nobody noted comes from nowhere known.
        noted.upsampled_terrain(TileCoord::new(9, 1, 1), TileCoord::new(8, 0, 0));
        assert_eq!(noted.terrain(TileCoord::new(9, 1, 1)), None);
    }

    #[test]
    fn the_digest_is_fnv_1a() {
        // The published test vectors of FNV-1a, 64 bits.
        assert_eq!(digest(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(digest(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(digest(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
