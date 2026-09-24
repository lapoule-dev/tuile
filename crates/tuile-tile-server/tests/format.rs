// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The archive format and the grids: what goes in comes out, byte for byte,
//! and what others wrote reads as they wrote it.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use pmtiles::{TileCoord, TileId};
use rand::{Rng, SeedableRng};
use tuile_tile_server::archive;
use tuile_tile_server::{Grid, OutOfGrid};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

async fn read_all(path: &std::path::Path) -> Vec<(u64, Bytes)> {
    let r = Arc::new(archive::open_local(path).await.expect("open"));
    let ids = archive::ids(r.clone()).await.expect("ids");
    let mut out = Vec::new();
    for id in ids {
        let b = r.get_tile(TileId::new(id).expect("id")).await.expect("get").expect("listed tile present");
        out.push((id, b));
    }
    out
}

fn write_tiles(
    layer: &tuile_tile_server::Layer,
    epoch: &str,
    tiles: Vec<(u64, Bytes)>,
) -> Result<(tempfile::NamedTempFile, u64), tuile_tile_server::StoreError> {
    let zooms = archive::zoom_range(tiles.iter().map(|t| &t.0));
    archive::write(layer, epoch, zooms, tiles.into_iter().map(Ok))
}

fn random_tiles(n: usize, seed: u64) -> Vec<(u64, Bytes)> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut ids = std::collections::BTreeSet::new();
    while ids.len() < n {
        let z = rng.random_range(0..=16u8);
        let side = 1u32 << z;
        let (x, y) = (rng.random_range(0..side), rng.random_range(0..side));
        ids.insert(TileId::from(TileCoord::new(z, x, y).expect("coord")).value());
    }
    ids.into_iter()
        .map(|id| {
            let len = rng.random_range(1..4096);
            let bytes: Vec<u8> = (0..len).map(|_| rng.random()).collect();
            (id, Bytes::from(bytes))
        })
        .collect()
}

#[tokio::test]
async fn thousands_of_random_tiles_come_back_byte_for_byte() {
    let tiles = random_tiles(5000, 7);
    let (file, count) =
        write_tiles(&common::imagery(), "202609", tiles.clone()).expect("write");
    assert_eq!(count, 5000);
    assert_eq!(read_all(file.path()).await, tiles);
}

#[tokio::test]
async fn enough_tiles_to_need_leaf_directories_still_read_back() {
    // Well past what a 16 KiB root directory can index.
    let tiles = random_tiles(40_000, 11);
    let (file, _) = write_tiles(&common::imagery(), "202609", tiles.clone()).expect("write");
    // Leaf directories length: bytes 48..56 of the v3 header, little-endian.
    let raw = std::fs::read(file.path()).expect("read");
    let leaf_length = u64::from_le_bytes(raw[48..56].try_into().expect("8 bytes"));
    assert!(leaf_length > 0, "this archive was supposed to need leaf directories");
    assert_eq!(read_all(file.path()).await, tiles);
}

#[tokio::test]
async fn an_empty_archive_and_a_single_tile_archive_are_valid() {
    let (empty, n) = write_tiles(&common::imagery(), "202609", Vec::new()).expect("write empty");
    assert_eq!(n, 0);
    assert!(read_all(empty.path()).await.is_empty());

    let one = vec![(TileId::from(TileCoord::new(3, 4, 5).expect("coord")).value(), Bytes::from_static(b"only"))];
    let (file, _) = write_tiles(&common::imagery(), "202609", one.clone()).expect("write one");
    assert_eq!(read_all(file.path()).await, one);
}

#[tokio::test]
async fn the_header_says_what_the_tiles_are_and_the_metadata_says_where_they_belong() {
    let layer = common::terrain();
    let tiles = vec![(Grid::Geographic.to_id(0, 1, 0).expect("id"), Bytes::from_static(b"\x1f\x8bterrain"))];
    let (file, _) = write_tiles(&layer, "d", tiles).expect("write");
    let r = archive::open_local(file.path()).await.expect("open");
    let h = r.get_header();
    assert_eq!(h.tile_type, pmtiles::TileType::Unknown);
    assert_eq!(h.tile_compression, pmtiles::Compression::Gzip);
    let meta: serde_json::Value = serde_json::from_str(&r.get_metadata().await.expect("metadata")).expect("json");
    assert_eq!(meta["tuile:grid"], "geographic");
    assert_eq!(meta["tuile:layer"], "terrain");
    assert_eq!(meta["tuile:epoch"], "d");
}

#[test]
fn tiles_out_of_order_are_refused_not_silently_written() {
    let a = TileId::from(TileCoord::new(5, 1, 1).expect("coord")).value();
    let b = TileId::from(TileCoord::new(5, 0, 0).expect("coord")).value();
    let tiles = vec![(a.max(b), Bytes::from_static(b"x")), (a.min(b), Bytes::from_static(b"y"))];
    assert!(archive::write(&common::imagery(), "202609", Some((5, 5)), tiles.into_iter().map(Ok)).is_err());
}

#[tokio::test]
async fn archives_written_by_others_read_as_they_were_written() {
    // Leaf directories, from the reference fixtures.
    let leaf = read_all(&fixture("leaf.pmtiles")).await;
    assert!(!leaf.is_empty());

    // A raster pyramid z0–z3: every tile is a PNG, z0 is the one tile at id 0.
    let raster = read_all(&fixture("raster-z3.pmtiles")).await;
    assert_eq!(raster.first().map(|t| t.0), Some(0));
    assert!(raster.iter().all(|(_, b)| b.starts_with(b"\x89PNG")), "every tile a PNG");
    let full_pyramid: usize = (0..=3).map(|z| 1usize << (2 * z)).sum();
    assert!(raster.len() <= full_pyramid);
}

#[test]
fn web_mercator_addresses_are_the_archive_s_own() {
    for z in [0u8, 1, 5, 18, 31] {
        let last = (1u32 << z) - 1;
        for (x, y) in [(0, 0), (last, 0), (0, last), (last, last)] {
            let c = Grid::WebMercator.to_archive(z, x, y).expect("in grid");
            assert_eq!((c.z(), c.x(), c.y()), (z, x, y));
            assert_eq!(Grid::WebMercator.from_archive(c), Some((z, x, y)));
        }
        if z < 31 {
            assert!(Grid::WebMercator.to_archive(z, last + 1, 0).is_err(), "x past the edge at z{z}");
            assert!(Grid::WebMercator.to_archive(z, 0, last + 1).is_err(), "y past the edge at z{z}");
        }
    }
    assert!(Grid::WebMercator.to_archive(32, 0, 0).is_err());
}

#[test]
fn geographic_level_l_is_stored_at_z_l_plus_one_in_the_upper_half() {
    // The two tiles of level 0.
    for x in 0..2 {
        let c = Grid::Geographic.to_archive(0, x, 0).expect("level 0");
        assert_eq!((c.z(), c.x(), c.y()), (1, x, 0));
        assert_eq!(Grid::Geographic.from_archive(c), Some((0, x, 0)));
    }
    // Level L is 2^(L+1) × 2^L.
    for l in [1u8, 9, 17, 30] {
        let (w, h) = (1u32 << (l + 1), 1u32 << l);
        let c = Grid::Geographic.to_archive(l, w - 1, h - 1).expect("corner");
        assert_eq!((c.z(), c.x(), c.y()), (l + 1, w - 1, h - 1));
        let beyond = Grid::Geographic.to_archive(l, 0, h);
        assert_eq!(beyond, Err(OutOfGrid { grid: Grid::Geographic, level: l, x: 0, y: h }));
        assert!(Grid::Geographic.to_archive(l, w, 0).is_err());
    }
    assert!(Grid::Geographic.to_archive(31, 0, 0).is_err(), "no room one level down");
    // The half the geographic grid never uses holds nothing.
    let lower = TileCoord::new(3, 0, 5).expect("coord");
    assert_eq!(Grid::Geographic.from_archive(lower), None);
    assert_eq!(Grid::Geographic.from_archive(TileCoord::new(0, 0, 0).expect("z0")), None);
}

/// Writes sample archives where `TUILE_PMTILES_SAMPLES` points, for the
/// reference command-line tool: `pmtiles verify` and `pmtiles show`.
#[tokio::test]
#[ignore = "writes files for an external tool; run with --ignored"]
async fn writes_samples_for_the_reference_tool() {
    let Ok(dir) = std::env::var("TUILE_PMTILES_SAMPLES") else { return };
    let dir = PathBuf::from(dir);
    let tiles = random_tiles(40_000, 11);
    let (file, _) = write_tiles(&common::imagery(), "202609", tiles).expect("write");
    std::fs::copy(file.path(), dir.join("imagery-leaves.pmtiles")).expect("copy");
    let terrain: Vec<_> = (0..2u32)
        .map(|x| (Grid::Geographic.to_id(0, x, 0).expect("id"), Bytes::from(format!("level 0 tile {x}"))))
        .collect();
    let (file, _) = write_tiles(&common::terrain(), "d", terrain).expect("write");
    std::fs::copy(file.path(), dir.join("terrain-level0.pmtiles")).expect("copy");
    let (file, _) = write_tiles(&common::imagery(), "202609", Vec::new()).expect("write");
    std::fs::copy(file.path(), dir.join("empty.pmtiles")).expect("copy");

    // An archive produced by a compaction of three deltas at z13–z15.
    use object_store::ObjectStoreExt;
    let clock = common::TestClock::new();
    let objects = common::memory();
    let s = common::store_on(objects.clone(), &clock, common::eager());
    // A square of tiles at one level above, at, and one level below the
    // test level, all inside the test zone.
    const SQUARE: u32 = 8;
    let at = |v: u32, z: u8| if z >= common::LEVEL { v << (z - common::LEVEL) } else { v >> (common::LEVEL - z) };
    for z in [common::LEVEL - 1, common::LEVEL, common::LEVEL + 1] {
        for i in 0..SQUARE * SQUARE {
            let (x, y) = (at(common::X0, z) + i % SQUARE, at(common::Y0, z) + i / SQUARE);
            s.put(common::IMAGERY, z, x, y, common::body(z, x, y, 0)).await.expect("put");
        }
        s.flush_all().await.expect("flush");
    }
    let zone = common::zone();
    s.compact(common::IMAGERY, zone).await.expect("compact");
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone))
        .await
        .expect("manifest")
        .manifest;
    let merged = objects.get(&m.archives[0].key.as_str().into()).await.expect("get").bytes().await.expect("bytes");
    std::fs::write(dir.join("compacted.pmtiles"), merged).expect("write");
}
