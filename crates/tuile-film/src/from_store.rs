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
    // The mesh says how far below its source it was cut, and the skirt is
    // the engine's to size by that: nothing of a seam is decided here.
    let content = tuile_terrain::to_ground(&mesh, &rect);
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
            source: 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A terrain tile's bytes as a source serves them: a 5 × 5 grid with its
    /// edges and the oct-encoded normals extension, numbered in the order
    /// its triangles first use its vertices.
    fn lit_tile(coord: TileCoord) -> Vec<u8> {
        let n = 5usize;
        let (mut u, mut v, mut height, mut normals) = (vec![], vec![], vec![], vec![]);
        for j in 0..n {
            for i in 0..n {
                let (fu, fv) = (i as f64 / 4.0, j as f64 / 4.0);
                u.push(fu);
                v.push(fv);
                height.push(0.5 + 0.4 * (3.0 * fu).sin() * (2.0 * fv).cos());
                let lean = (0.3 * fu - 0.1) as f32;
                let len = (lean * lean + 1.0).sqrt();
                normals.push([lean / len, 0.0, 1.0 / len]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..n - 1 {
            for i in 0..n - 1 {
                let a = (j * n + i) as u32;
                let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
                indices.extend_from_slice(&[a, b, c, c, b, d]);
            }
        }
        // Renumbered into first use, as the format codes its indices.
        let mut renumbered = vec![u32::MAX; n * n];
        let mut order = Vec::new();
        for &i in &indices {
            if renumbered[i as usize] == u32::MAX {
                renumbered[i as usize] = order.len() as u32;
                order.push(i as usize);
            }
        }
        let pick = |values: &[f64]| order.iter().map(|&i| values[i]).collect::<Vec<_>>();
        let normals: Vec<[f32; 3]> = order.iter().map(|&i| normals[i]).collect();
        let indices: Vec<u32> = indices.iter().map(|&i| renumbered[i as usize]).collect();
        let line = |f: &dyn Fn(usize) -> usize| -> Vec<u32> {
            (0..n).map(|k| renumbered[f(k)]).collect()
        };
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let (lon, lat) = rect.center();
        let centre = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
            lon,
            lat,
            height: 300.0,
        });
        tuile_terrain::decode::encode(
            &tuile_terrain::Header {
                center: centre.to_array(),
                min_height: 120.0,
                max_height: 480.0,
                bounding_sphere_center: centre.to_array(),
                bounding_sphere_radius: 1.0e5,
                horizon_occlusion: [0.0; 3],
            },
            &pick(&u),
            &pick(&v),
            &pick(&height),
            &indices,
            Some(&normals),
            &[
                line(&|k| k * n),
                line(&|k| k),
                line(&|k| k * n + n - 1),
                line(&|k| (n - 1) * n + k),
            ],
        )
    }

    /// A bake that names its tiles decodes none of their normals; the render
    /// builds each tile again from the store's own bytes, and those carry
    /// them. So a tile built from the store has its normals, one a vertex,
    /// skirts included — and as many vertices and indices as the bake
    /// counted without them, which is what its pack records.
    #[test]
    fn a_tile_built_from_the_store_has_the_normals_the_bake_never_decoded() {
        let source = TileCoord::new(9, 507, 385);
        let bytes = lit_tile(source);
        let store_tile = StoreTile {
            level: 9,
            x: 507,
            y: 385,
            digest: digest(&bytes),
        };
        // The tile itself, and one cut out of it two levels down.
        for (level, x, y) in [(9, 507, 385), (11, 2029, 1541)] {
            let refs = TileRefs {
                terrain: store_tile,
                skirt_height: 0.0,
                imagery: Vec::new(),
                composed_side: 0,
            };
            let id = TileId::from_terrain(level, x, y).0;
            let mesh = terrain_mesh(id, &refs, &bytes, [1.0; 4]).expect("built");
            let vertices = mesh.positions.len() / 12;
            assert!(vertices > 25, "{level}: the skirts are there too");
            assert_eq!(
                mesh.normals.len(),
                mesh.positions.len(),
                "{level}: a normal a vertex"
            );

            // What the bake counted, without normals.
            let mut quantized = tuile_terrain::decode(&bytes).expect("decoded");
            quantized.normals = None;
            let mut at = source;
            while at.level < level {
                let shift = level - at.level - 1;
                let child = TileCoord::new(at.level + 1, x >> shift, y >> shift);
                quantized = tuile_terrain::upsample(&quantized, at, child).expect("cut");
                at = child;
            }
            let rect = GeographicTilingScheme::default().tile_rect(at);
            let counted = tuile_terrain::to_ground_with(&quantized, &rect, false);
            let counted = &counted.meshes[0];
            assert!(counted.normals.is_none());
            assert_eq!(vertices, counted.positions.len(), "{level}: vertices");
            assert_eq!(
                mesh.index_count as usize,
                counted.indices.len(),
                "{level}: indices"
            );
        }
    }
}
