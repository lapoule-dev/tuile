// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use tuile_pack::{fb, Pack, PackError, TextureFormat};

/// One tile's mesh out of a pack, decompressed, without its texture.
///
/// The texture is left where it lies: a browser hands those bytes to its own
/// image decoder, a binary to its own, and neither wants a copy made first.
#[derive(Debug, Clone, PartialEq)]
pub struct Mesh {
    pub origin_ecef: [f64; 3],
    pub positions: Vec<u8>,
    pub normals: Vec<u8>,
    pub uvs: Vec<u8>,
    pub indices: Vec<u8>,
    pub index_count: u32,
    pub base_color_factor: [f32; 4],
}

impl Mesh {
    pub fn of(pack: &Pack<'_>, tile: &fb::Tile<'_>) -> Result<Self, PackError> {
        Self::read(tile, |b, what| pack.payload(b, what))
    }

    /// The mesh out of a fetched span of the blob region: `bytes` start at
    /// blob offset `at` (see `Pack::span_of`).
    pub fn of_span(
        pack: &Pack<'_>,
        tile: &fb::Tile<'_>,
        at: u64,
        bytes: &[u8],
    ) -> Result<Self, PackError> {
        Self::read(tile, |b, what| pack.payload_in(b, at, bytes, what))
    }

    fn read(
        tile: &fb::Tile<'_>,
        payload: impl Fn(&fb::Block, &'static str) -> Result<Vec<u8>, PackError>,
    ) -> Result<Self, PackError> {
        let block = |b: Option<&fb::Block>, what| match b {
            Some(b) => payload(b, what),
            None => Ok(Vec::new()),
        };
        let origin_ecef = match tile.origin_ecef() {
            Some(v) if v.len() == 3 => [v.get(0), v.get(1), v.get(2)],
            _ => return Err(PackError::Malformed("a tile with no origin".into())),
        };
        let base_color_factor = match tile.base_color_factor() {
            Some(v) if v.len() == 4 => [v.get(0), v.get(1), v.get(2), v.get(3)],
            _ => [1.0; 4],
        };
        Ok(Self {
            origin_ecef,
            positions: block(tile.positions(), "positions")?,
            normals: block(tile.normals(), "normals")?,
            uvs: block(tile.uvs(), "uvs")?,
            indices: block(tile.indices(), "indices")?,
            index_count: tile.index_count(),
            base_color_factor,
        })
    }
}

/// [`texture`], out of a fetched span of the blob region.
pub fn texture_of_span(
    pack: &Pack<'_>,
    tile: &fb::Tile<'_>,
    at: u64,
    bytes: &[u8],
) -> Result<Option<Vec<u8>>, PackError> {
    if tile.texture_format() == TextureFormat::None {
        return Ok(None);
    }
    let bytes = pack.texture_in(tile, at, bytes)?;
    Ok((!bytes.is_empty()).then_some(bytes))
}

/// The tile's encoded texture (a PNG), or `None` when it has none.
pub fn texture(pack: &Pack<'_>, tile: &fb::Tile<'_>) -> Result<Option<Vec<u8>>, PackError> {
    if tile.texture_format() == TextureFormat::None {
        return Ok(None);
    }
    let bytes = pack.texture(tile)?;
    Ok((!bytes.is_empty()).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::tests::{three_frames, tile};

    #[test]
    fn a_mesh_comes_back_as_it_was_baked() {
        let bytes = three_frames();
        let pack = Pack::open(&bytes).expect("open");
        let tiles = pack.frame(10).expect("frame");
        let mesh = Mesh::of(&pack, &tiles[0]).expect("mesh");
        let baked = tile(1, 0);
        assert_eq!(mesh.positions, baked.positions);
        assert_eq!(mesh.indices, baked.indices);
        assert_eq!(mesh.index_count, 3);
        assert_eq!(texture(&pack, &tiles[0]).expect("texture"), None);
    }
}
