// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A film built again from the tile store is closed ground too.**
//!
//! A pack of references holds no mesh: `from_store::terrain_mesh` builds
//! each tile's from the store's bytes at render time. It decides nothing
//! about seams — it calls the engine's one way of turning a terrain tile
//! into ground — and this holds it to that: between the meshes it builds,
//! whatever their levels, no ray passes. A pack baked before the engine
//! closed its seams is built by the same call, so it is closed as well,
//! without being baked again.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use glam::{DVec3, Mat4};
use support::{along, encoded, measured, meeting, tile_at, LAT, LON};
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::seam::{seam, Side};
use tuile_core::source::TileId;
use tuile_film::from_store::terrain_mesh;
use tuile_film::{StoreTile, TileRefs};
use tuile_terrain::TileCoord;

/// The mesh of `tile` as a film is handed it, from the bytes of the terrain
/// tile `from`, read back as content a ray can be cast at.
fn built(tile: TileCoord, from: TileCoord, bytes: &[u8]) -> DecodedTileContent {
    let refs = TileRefs {
        terrain: StoreTile {
            level: from.level as u8,
            x: from.x as u32,
            y: from.y as u32,
            digest: 0,
        },
        // What a pack baked before this was fixed says: the shallow depth
        // of the tile's own level. It is not what is hung.
        skirt_height: 0.75,
        imagery: Vec::new(),
        composed_side: 0,
    };
    let id = TileId::from_terrain(tile.level, tile.x, tile.y).0;
    let mesh = terrain_mesh(id, &refs, bytes, [1.0; 4]).expect("mesh");
    let f = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    DecodedTileContent {
        withheld_drape: None,
        meshes: vec![DecodedMesh {
            positions: mesh
                .positions
                .chunks_exact(12)
                .map(|b| [f(&b[0..4]), f(&b[4..8]), f(&b[8..12])])
                .collect(),
            normals: None,
            uvs: Some(
                mesh.uvs
                    .chunks_exact(8)
                    .map(|b| [f(&b[0..4]), f(&b[4..8])])
                    .collect(),
            ),
            indices: mesh
                .indices
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
            material: MaterialDesc::default(),
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::from_array(mesh.origin_ecef),
        transform_local: Mat4::IDENTITY,
    }
}

fn rays(
    west: &[(TileCoord, DecodedTileContent)],
    east: &[(TileCoord, DecodedTileContent)],
) -> (usize, usize, f64) {
    let (mut cast, mut through, mut step) = (0, 0, 0.0f64);
    for w in west {
        for e in east {
            if let Some((a, b)) = meeting((w.0, &w.1), (e.0, &e.1)) {
                let found = seam(a, b, 48);
                cast += found.rays;
                through += found.through;
                step = step.max(found.open);
            }
        }
    }
    (cast, through, step)
}

#[test]
fn meshes_built_from_two_terrain_tiles_of_the_store_are_closed() {
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    let (bytes_a, bytes_b) = (encoded(&measured(a, 32)), encoded(&measured(b, 20)));
    let west: Vec<_> = along(a, 16, Side::East)
        .into_iter()
        .map(|t| (t, built(t, a, &bytes_a)))
        .collect();
    let east: Vec<_> = along(b, 14, Side::West)
        .into_iter()
        .map(|t| (t, built(t, b, &bytes_b)))
        .collect();
    let (cast, through, step) = rays(&west, &east);
    assert!(cast > 500, "the scene has few steps to look into: {cast}");
    assert_eq!(
        through, 0,
        "{through} of {cast} rays pass, through steps of up to {step:.2} m"
    );
}

#[test]
fn a_mesh_of_the_store_is_closed_against_one_several_levels_coarser() {
    for coarse_level in [11, 9, 7] {
        let coarse = tile_at(coarse_level, LON, LAT);
        let down = 13 - coarse_level;
        let fine = TileCoord::new(
            13,
            (coarse.x << down) - 1,
            (coarse.y << down) + (1 << (down - 1)),
        );
        let (bytes_fine, bytes_coarse) =
            (encoded(&measured(fine, 32)), encoded(&measured(coarse, 16)));
        let west: Vec<_> = along(fine, 17, Side::East)
            .into_iter()
            .map(|t| (t, built(t, fine, &bytes_fine)))
            .collect();
        let east = vec![(coarse, built(coarse, coarse, &bytes_coarse))];
        let (cast, through, step) = rays(&west, &east);
        assert!(
            cast > 500,
            "level {coarse_level}: few steps to look into: {cast}"
        );
        assert_eq!(
            through, 0,
            "level 17 against level {coarse_level}: {through} of {cast} rays pass, through \
             steps of up to {step:.1} m"
        );
    }
}
