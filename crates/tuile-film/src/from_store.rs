// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A tile, built again from the tile store's own tiles.
//!
//! A pack of references says where each tile comes from: the terrain tile
//! its mesh was built from, and the imagery tiles of its drape with where
//! each lies. This is the building — the same decoding, the same cutting of
//! a mesh out of an ancestor's, the same composition of a drape, by the same
//! functions a bake calls — given the bytes. Nothing here fetches: the bytes
//! are handed in, by whoever reads the store.
//!
//! Pure functions of bytes and references, so that a pack that carries both
//! its tiles and its references can be checked against itself.

use std::sync::Arc;

use tuile_core::content::DecodedTexture;
use tuile_core::raster::{self, ImageryCoord, ImageryLayer, TilingScheme};
use tuile_core::source::TileId;
use tuile_pack::{StoreTile, TileRefs};
use tuile_terrain::{GeographicTilingScheme, TileCoord};

use crate::Mesh;

#[derive(Debug, thiserror::Error)]
pub enum FromStoreError {
    #[error("terrain {level}/{x}/{y}: {why}")]
    Terrain {
        level: u8,
        x: u32,
        y: u32,
        why: String,
    },
    #[error("imagery {level}/{x}/{y}: {why}")]
    Imagery {
        level: u8,
        x: u32,
        y: u32,
        why: String,
    },
    /// The store no longer holds the bytes the pack was baked from.
    #[error("{what} {level}/{x}/{y} is not the tile that was baked: digest {found:016x}, baked {baked:016x}")]
    Renewed {
        what: &'static str,
        level: u8,
        x: u32,
        y: u32,
        found: u64,
        baked: u64,
    },
}

/// FNV-1a, 64 bits: what a reference's digest is of.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut state = tuile_pack::Fnv1a::default();
    state.update(bytes);
    state.finish()
}

/// Whether `bytes` are the bytes `tile` was baked from.
pub fn is_baked(tile: &StoreTile, bytes: &[u8]) -> bool {
    digest(bytes) == tile.digest
}

fn terrain_error(tile: &StoreTile, why: impl std::fmt::Display) -> FromStoreError {
    FromStoreError::Terrain {
        level: tile.level,
        x: tile.x,
        y: tile.y,
        why: why.to_string(),
    }
}

/// The mesh of tile `id`, from the bytes of the terrain tile it was built
/// from.
///
/// `refs.terrain` is that tile: `id` itself, or an ancestor — in which case
/// the mesh is cut out of the ancestor's, a level at a time, as the loader
/// cut it. `base_color_factor` is the pack's: it is a decision of the drape,
/// not something the terrain says.
pub fn terrain_mesh(
    id: u64,
    refs: &TileRefs,
    bytes: &[u8],
    base_color_factor: [f32; 4],
) -> Result<Mesh, FromStoreError> {
    let source = &refs.terrain;
    let (level, x, y) = TileId(id).terrain_coord();
    let target = TileCoord::new(level, x, y);
    let from = TileCoord::new(
        u32::from(source.level),
        u64::from(source.x),
        u64::from(source.y),
    );
    if from.level > target.level {
        return Err(terrain_error(
            source,
            "is below the tile it is said to make",
        ));
    }
    let mut mesh = tuile_terrain::decode(bytes).map_err(|e| terrain_error(source, e))?;
    // Down from the ancestor to the tile, one level at a time: each rung is
    // the ancestor of the tile at that level.
    let mut at = from;
    while at.level < target.level {
        let shift = target.level - at.level - 1;
        let child = TileCoord::new(at.level + 1, target.x >> shift, target.y >> shift);
        mesh = tuile_terrain::upsample(&mesh, at, child)
            .ok_or_else(|| terrain_error(source, "nothing of it covers the tile"))?;
        at = child;
    }
    if at != target {
        return Err(terrain_error(source, "is not an ancestor of the tile"));
    }
    let rect = GeographicTilingScheme::default().tile_rect(target);
    // PROBE (exploration): a skirt as deep as the level the surface is
    // from, not the level the tile is cut at.
    let deep = if std::env::var_os("TUILE_PROBE_SKIRT_OF_SOURCE").is_some() {
        tuile_terrain::skirt_height(&GeographicTilingScheme::default().tile_rect(from))
    } else {
        tuile_terrain::skirt_height(&rect)
    };
    let content = tuile_terrain::to_decoded(&mesh, &rect, deep);
    let built = content
        .meshes
        .first()
        .ok_or_else(|| terrain_error(source, "decodes to no mesh"))?;
    let le3 = |values: &[[f32; 3]]| {
        values
            .iter()
            .flatten()
            .flat_map(|c| c.to_le_bytes())
            .collect()
    };
    Ok(Mesh {
        origin_ecef: content.local_origin_ecef.to_array(),
        positions: le3(&built.positions),
        normals: built.normals.as_deref().map(le3).unwrap_or_default(),
        uvs: built
            .uvs
            .as_deref()
            .map(|uvs| uvs.iter().flatten().flat_map(|c| c.to_le_bytes()).collect())
            .unwrap_or_default(),
        indices: built.indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
        index_count: built.indices.len() as u32,
        base_color_factor,
    })
}

fn coord_of(tile: &StoreTile) -> ImageryCoord {
    ImageryCoord {
        level: u32::from(tile.level),
        x: u64::from(tile.x),
        y: u64::from(tile.y),
    }
}

/// One imagery tile, decoded and laid on geographic spacing: what a drape
/// is composed of. Shared by every terrain tile it lies on, so made once.
pub fn imagery_texture(
    tile: &StoreTile,
    scheme: &TilingScheme,
    bytes: &[u8],
) -> Result<DecodedTexture, FromStoreError> {
    raster::decode_and_reproject(bytes, scheme, coord_of(tile)).map_err(|e| {
        FromStoreError::Imagery {
            level: tile.level,
            x: tile.x,
            y: tile.y,
            why: e.to_string(),
        }
    })
}

/// The drape of a tile, composed from its imagery tiles as the bake composed
/// it: same layers, same order, same size. `None` for a tile with no
/// imagery. `texture` gives each layer's tile, as [`imagery_texture`] made
/// it.
pub fn compose(
    refs: &TileRefs,
    base_color_factor: [f32; 4],
    texture: impl Fn(&StoreTile) -> Arc<DecodedTexture>,
) -> Option<DecodedTexture> {
    if refs.imagery.is_empty() {
        return None;
    }
    let layers: Vec<ImageryLayer> = refs
        .imagery
        .iter()
        .map(|placed| ImageryLayer {
            coord: coord_of(&placed.tile),
            texture: texture(&placed.tile),
            coverage: placed.coverage,
            translation: placed.translation,
            scale: placed.scale,
        })
        .collect();
    let side = refs.composed_side.max(1);
    Some(raster::bake_layers(
        &layers,
        base_color_factor,
        (side, side),
    ))
}
