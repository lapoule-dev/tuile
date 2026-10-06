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

/// A centre as a key: the same bits, or not the same mesh.
fn bits(centre: [f64; 3]) -> [u64; 3] {
    centre.map(f64::to_bits)
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
    /// By the tile **and the centre of its mesh**. A tile can be built
    /// twice in one session, from two places — cut from an ancestor while
    /// the source's own tile was not known to exist, decoded from that tile
    /// later — and the two meshes are both out there. The centre is the
    /// source tile's own, carried through every cut, so it says which.
    terrain: Mutex<HashMap<(TileCoord, [u64; 3]), (TileCoord, u64)>>,
    imagery: Mutex<HashMap<ImageryCoord, u64>>,
}

impl Provenance {
    /// Where the mesh of `tile` whose origin is `centre` came from, once the
    /// loader has built it.
    pub fn terrain(&self, tile: TileCoord, centre: [f64; 3]) -> Option<TerrainOrigin> {
        let (source, digest) = *self.terrain.lock().ok()?.get(&(tile, bits(centre)))?;
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

    // What follows is the loader's side: what it notes as it fetches. Public
    // because a loader need not be this crate's.

    /// `tile` was decoded from its own bytes, which digest to `digest`, into
    /// a mesh centred on `centre`.
    pub fn fetched_terrain(&self, tile: TileCoord, centre: [f64; 3], digest: u64) {
        if let Ok(mut terrain) = self.terrain.lock() {
            terrain.insert((tile, bits(centre)), (tile, digest));
        }
    }

    /// `tile` was cut from the mesh of `parent` centred on `centre` — a cut
    /// keeps its ancestor's centre — so it comes from wherever that did.
    pub fn upsampled_terrain(&self, tile: TileCoord, parent: TileCoord, centre: [f64; 3]) {
        if let Ok(mut terrain) = self.terrain.lock() {
            if let Some(origin) = terrain.get(&(parent, bits(centre))).copied() {
                terrain.insert((tile, bits(centre)), origin);
            }
        }
    }

    pub fn fetched_imagery(&self, tile: ImageryCoord, bytes: &[u8]) {
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
        let centre = [1.0, 2.0, 3.0];
        assert_eq!(noted.terrain(root, centre), None);
        noted.fetched_terrain(root, centre, digest(b"bytes of the root"));
        noted.upsampled_terrain(child, root, centre);
        noted.upsampled_terrain(grandchild, child, centre);
        let origin = noted.terrain(grandchild, centre).expect("noted");
        assert_eq!(origin.source, root);
        assert_eq!(origin.digest, digest(b"bytes of the root"));
        // The skirt is the drawn tile's, not its ancestor's: it hangs from
        // the edges of the tile that is drawn.
        assert!(origin.skirt_height < noted.terrain(root, centre).expect("noted").skirt_height);
        // A tile cut from a parent nobody noted comes from nowhere known.
        noted.upsampled_terrain(TileCoord::new(9, 1, 1), TileCoord::new(8, 0, 0), centre);
        assert_eq!(noted.terrain(TileCoord::new(9, 1, 1), centre), None);
    }

    /// One tile, built twice in a session: first cut from its parent, then
    /// decoded from its own bytes once the source turned out to have them.
    /// Both meshes exist, and each says where it came from.
    #[test]
    fn a_tile_built_twice_keeps_both_origins() {
        let noted = Provenance::default();
        let (parent, tile) = (TileCoord::new(7, 20, 9), TileCoord::new(8, 41, 18));
        let (of_parent, of_tile) = ([10.0, 0.0, 0.0], [10.5, 0.0, 0.0]);
        noted.fetched_terrain(parent, of_parent, 1);
        noted.upsampled_terrain(tile, parent, of_parent);
        noted.fetched_terrain(tile, of_tile, 2);
        assert_eq!(noted.terrain(tile, of_parent).expect("cut").source, parent);
        assert_eq!(noted.terrain(tile, of_tile).expect("own").source, tile);
    }

    #[test]
    fn the_digest_is_fnv_1a() {
        // The published test vectors of FNV-1a, 64 bits.
        assert_eq!(digest(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(digest(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(digest(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
