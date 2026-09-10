// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A session that reads a pre-baked pack instead of the network — job B.
//!
//! # What it removes
//!
//! Everything the render half was paying for and could not use. A live session
//! resolves ion and Bing, primes a coarse pyramid, traverses, fetches,
//! decodes, resamples, drapes, bakes mosaics and encodes PNG; frame 1 of a
//! farm job costs about 500 seconds of that and is 89 % of a 48-frame job.
//! Sixteen processes on one pod each paid it from zero, on a machine rented
//! for its GPUs.
//!
//! This opens a file and reads a table.
//!
//! The second effect is the one worth having. A packed session has no network,
//! no ion token and **no traversal**, so a frame is not reproducible because a
//! convergence race was stabilised — there is no race. The selection was
//! decided once, by the bake, and is recorded.
//!
//! # Addressed by camera, not by frame number
//!
//! A Hydra host cooks at a timecode and hands the session a camera; there is
//! no frame number anywhere on the ABI. Rather than add one — which would
//! change the header, the procedural, and every host that ever links this —
//! the pack records the camera each frame was baked for, and a lookup finds it.
//!
//! # Nothing is approximated
//!
//! Every way this can be handed the wrong data is an error, never a fallback:
//! a pack of another scene, a camera that matches no baked frame, a payload
//! that does not decompress. A live session that loses a tile draws its
//! ancestor and looks plausible; a packed one that guesses would render the
//! wrong ground and report success, which is the single failure mode the split
//! adds and the one thing it must not do.

use std::sync::Arc;

use glam::DVec3;
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::source::TileId;
use tuile_core::traversal::ViewStateParams;
use tuile_pack::{BakedView, Pack, PackError};

use crate::session::{EncodedTexture, Frame, FrameError, TileGeometry};

#[derive(Debug, thiserror::Error)]
pub enum PackedError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Pack(#[from] PackError),
}

/// A pack, held open for the life of the session.
pub struct Packed {
    /// The whole file. Read once; the table is walked in place inside it and
    /// payloads are decompressed on demand.
    ///
    /// Read rather than `mmap`ed, deliberately. On a farm every process opens
    /// the same file that the job just downloaded, so the page cache already
    /// holds it and the copy is a memcpy from RAM — while `mmap` would add a
    /// dependency and a class of failure (a truncated file becomes SIGBUS
    /// rather than an error) for a saving that does not exist here.
    bytes: Vec<u8>,
    dataset: Arc<str>,
}

impl Packed {
    pub fn open(path: &std::path::Path, scene: Option<&str>) -> Result<Self, PackedError> {
        let bytes = std::fs::read(path).map_err(|source| PackedError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let pack = Pack::open(&bytes)?;
        if let Some(scene) = scene {
            pack.expect_scene(scene)?;
        }
        let (first, last) = pack.frame_range();
        tracing::info!(
            scene = pack.scene_digest(),
            culling = pack.culling(),
            first,
            last,
            tiles = pack.tile_count(),
            bytes = bytes.len(),
            path = %path.display(),
            "reading a pre-baked scene: no network, no token, no traversal"
        );
        // Said out loud, every session, because it cannot be seen in the
        // output. A pack freezes its selection: ground the bake did not select
        // is bare in every frame this ever renders, and no count of tiles
        // reports ground nobody selected.
        if pack.culling() != "full" {
            tracing::warn!(
                culling = pack.culling(),
                "this pack was baked with culling {}: whatever the bake left \
                 unselected is frozen into every frame it answers",
                pack.culling()
            );
        }
        // The dataset scopes every texture URI, and both halves must agree on
        // it or a material resolves to nothing. The scene digest is the one
        // name that identifies exactly these bytes.
        let dataset = format!("pack-{}", pack.scene_digest()).into();
        Ok(Self { bytes, dataset })
    }

    /// The frame baked for this camera.
    pub fn frame(&self, views: &[ViewStateParams]) -> Result<Frame, FrameError> {
        // One camera. A packed frame is a lookup, and a union of two cameras
        // is a selection nobody baked — so a stereo pair needs a pack baked
        // for the pair, not two lookups blended here.
        let [view] = views else {
            return Err(FrameError::TilesFailed {
                count: views.len(),
                first: format!(
                    "a packed session answers one camera; {} were given",
                    views.len()
                ),
            });
        };
        let pack = Pack::open(&self.bytes).map_err(pack_failed)?;
        let wanted = BakedView {
            position: view.position.to_array(),
            direction: view.direction.to_array(),
            up: view.up.to_array(),
            viewport_px: view.viewport_px.to_array(),
            fovy_rad: view.fovy_rad,
        };
        let (number, tiles) = pack.frame_for_view(&wanted).map_err(pack_failed)?;

        let mut out = Vec::with_capacity(tiles.len());
        for tile in &tiles {
            out.push(self.geometry(&pack, tile)?);
        }
        tuile_core::det!(
            "packed",
            frame = number,
            selected = out.len(),
            sel_digest = tuile_core::determinism::digest(out.iter().map(|t| t.tile.0)),
        );
        tracing::info!(frame = number, tiles = out.len(), "frame read from the pack");
        Ok(Frame::new(Arc::clone(&self.dataset), out))
    }

    /// One tile, rebuilt in exactly the shape a live session would have handed
    /// over — same `TileGeometry`, same buffers, same texture URI.
    fn geometry(
        &self,
        pack: &Pack<'_>,
        tile: &tuile_pack::fb::Tile<'_>,
    ) -> Result<Arc<TileGeometry>, FrameError> {
        let baked = pack.baked(tile).map_err(pack_failed)?;
        let id = TileId(baked.id);
        let origin = DVec3::from_array(baked.origin_ecef);

        let textured = baked.texture.is_some();
        let mesh = DecodedMesh {
            positions: f32x3(&baked.positions),
            normals: (!baked.normals.is_empty()).then(|| f32x3(&baked.normals)),
            uvs: (!baked.uvs.is_empty()).then(|| f32x2(&baked.uvs)),
            indices: u32s(&baked.indices),
            material: MaterialDesc {
                base_color_factor: baked.base_color_factor,
                // Index 0 of the tile's own textures, which is where the ABI
                // looks and what makes a terrain tile report a texture at all.
                base_color_texture: textured.then_some(0),
            },
        };
        // What the bake counted, checked against what came back. A vertex
        // count that disagrees with the buffer means the file is not what it
        // says it is, and a renderer handed a short buffer draws a torn tile
        // rather than failing.
        if mesh.positions.len() != baked.vertex_count as usize
            || mesh.indices.len() != baked.index_count as usize
        {
            return Err(FrameError::TilesFailed {
                count: 1,
                first: format!(
                    "tile {}: the pack says {} vertices and {} indices, the \
                     buffers hold {} and {}",
                    baked.id,
                    baked.vertex_count,
                    baked.index_count,
                    mesh.positions.len(),
                    mesh.indices.len()
                ),
            });
        }

        let memoized = baked.texture.map(|png| {
            Arc::new(EncodedTexture {
                uri: Frame::texture_uri(&self.dataset, id, 0, baked.drape),
                png,
            })
        });
        Ok(Arc::new(TileGeometry::packed(
            id,
            origin,
            DecodedTileContent {
                meshes: vec![mesh],
                textures: Vec::new(),
                imagery: Vec::new(),
                local_origin_ecef: origin,
                transform_local: glam::Mat4::IDENTITY,
            },
            baked.drape,
            memoized,
        )))
    }
}

/// A pack that cannot be read is a failed frame, not a smaller one.
fn pack_failed(e: PackError) -> FrameError {
    FrameError::TilesFailed {
        count: 1,
        first: e.to_string(),
    }
}

fn f32x3(bytes: &[u8]) -> Vec<[f32; 3]> {
    bytes
        .chunks_exact(12)
        .map(|c| {
            [
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                f32::from_le_bytes([c[8], c[9], c[10], c[11]]),
            ]
        })
        .collect()
}

fn f32x2(bytes: &[u8]) -> Vec<[f32; 2]> {
    bytes
        .chunks_exact(8)
        .map(|c| {
            [
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            ]
        })
        .collect()
}

fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_decoders_are_the_inverse_of_the_bake_encoders() {
        let mut bytes = Vec::new();
        for c in [1.5f32, -2.0, 0.25] {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(f32x3(&bytes), vec![[1.5, -2.0, 0.25]]);
        assert_eq!(f32x2(&bytes[..8]), vec![[1.5, -2.0]]);
        assert_eq!(u32s(&[1, 0, 0, 0, 4, 3, 2, 1]), vec![1, 0x0102_0304]);
        // A trailing partial element is dropped rather than read past — the
        // vertex-count check above is what turns that into a reported failure.
        assert_eq!(f32x3(&bytes[..11]), Vec::<[f32; 3]>::new());
    }
}
