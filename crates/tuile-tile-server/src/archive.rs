// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One immutable archive: written once from sorted tiles, read by ranges.
//!
//! A thin layer over the `pmtiles` crate. Tiles go in as the source delivered
//! them (`add_raw_tile`, never recompressed) in increasing tile id, so an
//! archive is clustered and its neighbours on the ground are neighbours in the
//! file. The metadata names the grid, the layer and the epoch, so an archive
//! found alone in a bucket still says what it holds.

use std::path::Path as FsPath;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::path::Path;
use object_store::ObjectStore;
use pmtiles::{AsyncPmTilesReader, HashMapCache, MmapBackend, ObjectStoreBackend, PmTilesWriter, TileCoord, TileId};
use serde_json::json;

use crate::layer::Layer;
use crate::StoreError;

/// An archive read from the object store, one range at a time.
pub type RemoteReader = AsyncPmTilesReader<ObjectStoreBackend, HashMapCache>;
/// An archive read from a local file.
pub type LocalReader = AsyncPmTilesReader<MmapBackend, HashMapCache>;

/// Writes tiles, **sorted by tile id and without duplicates**, to a new
/// temporary file. Returns the file and the number of tiles written.
///
/// The tiles come as results so a producer on another thread (a compaction
/// reading its inputs) can stream them through a bounded channel and still
/// report its own failure; the first error stops the write.
///
/// `zooms` is the lowest and highest archive zoom among the tiles, which the
/// header must state exactly; `None` for an empty archive.
pub fn write<I>(
    layer: &Layer,
    epoch: &str,
    zooms: Option<(u8, u8)>,
    tiles: I,
) -> Result<(tempfile::NamedTempFile, u64), StoreError>
where
    I: IntoIterator<Item = Result<(u64, Bytes), StoreError>>,
{
    let file = tempfile::NamedTempFile::new()?;
    let out = file.reopen()?;
    let metadata = json!({
        "name": layer.name,
        "tuile:grid": layer.grid,
        "tuile:layer": layer.name,
        "tuile:epoch": epoch,
    })
    .to_string();

    let mut builder = PmTilesWriter::new(layer.tile_type).tile_compression(layer.tile_compression).metadata(&metadata);
    if let Some((min, max)) = zooms {
        builder = builder.min_zoom(min).max_zoom(max).center_zoom(min);
    }
    let mut writer = builder.create(out)?;
    let mut count = 0u64;
    let mut last: Option<u64> = None;
    for tile in tiles {
        let (id, bytes) = tile?;
        if let Some(prev) = last {
            if id <= prev {
                return Err(StoreError::Corrupt(format!("tile ids out of order: {id} after {prev}")));
            }
        }
        last = Some(id);
        let coord = TileCoord::from(TileId::new(id)?);
        writer.add_raw_tile(coord, &bytes)?;
        count += 1;
    }
    writer.finalize()?;
    fix_empty_leaf_offset(file.path())?;
    Ok((file, count))
}

/// The lowest and highest archive zoom of a set of tile ids.
pub fn zoom_range<'a>(ids: impl IntoIterator<Item = &'a u64>) -> Option<(u8, u8)> {
    ids.into_iter().filter_map(|id| TileId::new(*id).ok()).map(|t| TileCoord::from(t).z()).fold(None, |acc, z| {
        Some(acc.map_or((z, z), |(lo, hi): (u8, u8)| (lo.min(z), hi.max(z))))
    })
}

/// Header layout, from the v3 specification: 127 bytes, little-endian u64
/// fields for the section offsets and lengths.
pub const HEADER_LEN: usize = 127;
pub const LEAF_OFFSET: usize = 40;
pub const LEAF_LENGTH: usize = 48;
pub const DATA_OFFSET: usize = 56;
const U64_LEN: usize = 8;

/// An archive without leaf directories must still say where that (empty)
/// section is: the reference verifier refuses a leaf offset of 0. The `pmtiles`
/// crate (0.24) writes 0 in that case; point it at the tile data, where an
/// empty section between the two ends — the same bytes the reference writer
/// produces. Nothing else in the file moves.
fn fix_empty_leaf_offset(path: &FsPath) -> Result<(), StoreError> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)?;
    let field = |at: usize| {
        let mut b = [0u8; U64_LEN];
        b.copy_from_slice(&header[at..at + U64_LEN]);
        u64::from_le_bytes(b)
    };
    if field(LEAF_LENGTH) == 0 && field(LEAF_OFFSET) == 0 {
        f.seek(SeekFrom::Start(LEAF_OFFSET as u64))?;
        f.write_all(&field(DATA_OFFSET).to_le_bytes())?;
        f.flush()?;
    }
    Ok(())
}

/// Opens an archive in the object store.
pub async fn open_remote(store: Arc<dyn ObjectStore>, key: &str) -> Result<RemoteReader, StoreError> {
    let backend = ObjectStoreBackend::new(Box::new(store), Path::from(key));
    Ok(AsyncPmTilesReader::try_from_cached_source(backend, HashMapCache::default()).await?)
}

/// Opens an archive from a local file.
pub async fn open_local(path: &FsPath) -> Result<LocalReader, StoreError> {
    Ok(AsyncPmTilesReader::new_with_cached_path(HashMapCache::default(), path).await?)
}

/// Every tile id an archive holds, in increasing order, runs of identical
/// tiles expanded.
pub async fn ids<B, C>(reader: Arc<AsyncPmTilesReader<B, C>>) -> Result<Vec<u64>, StoreError>
where
    B: pmtiles::AsyncBackend + Sync + Send + 'static,
    C: pmtiles::DirectoryCache + Sync + Send + 'static,
{
    let mut out = Vec::new();
    let mut entries = reader.entries();
    while let Some(entry) = entries.try_next().await? {
        out.extend(entry.iter_coords().map(TileId::value));
    }
    Ok(out)
}
