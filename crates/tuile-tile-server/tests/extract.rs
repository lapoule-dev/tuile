// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Extracts: each tile asked for comes from the newest live archive that
//! holds it, as a reader of the zone gets it, and its neighbours in that
//! archive come with it in the same request.

mod common;

use std::time::Duration;

use common::*;
use tuile_tile_server::{DiskCacheConfig, ExtractAsk, TileStore};

/// Tiles `from..to` of the test zone, written as one archive at `version`.
async fn archive(
    objects: std::sync::Arc<dyn object_store::ObjectStore>,
    clock: &TestClock,
    from: u32,
    to: u32,
    version: u32,
) {
    clock.advance(Duration::from_secs(10));
    let w = store_on(objects, clock, eager());
    for i in from..to {
        let (x, y) = in_zone(i);
        w.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, version))
            .await
            .expect("put");
    }
    w.flush_all().await.expect("flush");
}

/// Three archives of one zone, each newer one overlapping the last:
/// tiles 0..32 at v1, 16..48 at v2, 40..64 at v3.
async fn three_archives(objects: std::sync::Arc<dyn object_store::ObjectStore>, clock: &TestClock) {
    archive(objects.clone(), clock, 0, 32, 1).await;
    archive(objects.clone(), clock, 16, 48, 2).await;
    archive(objects, clock, 40, 64, 3).await;
}

/// Asks for tiles `0..n` of the test zone.
fn ask(n: u32) -> Vec<ExtractAsk> {
    vec![ExtractAsk {
        layer: IMAGERY.into(),
        tiles: (0..n)
            .map(|i| {
                let (x, y) = in_zone(i);
                (LEVEL, x, y)
            })
            .collect(),
        ..Default::default()
    }]
}

/// The tiles of an extract, by their ids in it.
async fn tiles_of(file: &std::path::Path) -> Vec<(u64, bytes::Bytes)> {
    let reader = std::sync::Arc::new(
        tuile_tile_server::archive::open_local(file)
            .await
            .expect("the extract"),
    );
    let mut out = Vec::new();
    for id in tuile_tile_server::archive::ids(reader.clone())
        .await
        .expect("ids")
    {
        let tile = reader
            .get_tile(pmtiles::TileId::new(id).expect("id"))
            .await
            .expect("tile")
            .expect("a tile listed is there");
        out.push((id, tile));
    }
    out
}

#[tokio::test]
async fn each_tile_comes_from_the_newest_archive_that_holds_it() {
    let clock = TestClock::new();
    let objects = memory();
    three_archives(objects.clone(), &clock).await;
    let s = store_on(objects, &clock, eager());
    // 64 held, and one more that no archive holds.
    let (file, report) = s.extract(&ask(65)).await.expect("extract");
    assert_eq!((report.tiles, report.absent), (64, 1));
    let grid = imagery().grid;
    let mut want = Vec::new();
    for i in 0..64 {
        let (x, y) = in_zone(i);
        let version = if i >= 40 {
            3
        } else if i >= 16 {
            2
        } else {
            1
        };
        want.push((
            grid.to_id(LEVEL, x, y).expect("id"),
            body(LEVEL, x, y, version),
        ));
    }
    want.sort_by_key(|(id, _)| *id);
    assert_eq!(tiles_of(file.path()).await, want);
}

#[tokio::test]
async fn neighbouring_tiles_of_an_archive_come_in_one_request() {
    let clock = TestClock::new();
    let objects = memory();
    three_archives(objects.clone(), &clock).await;
    let counted = Counting::over(objects);
    let s = store_on(counted.clone(), &clock, eager());
    let (_, report) = s.extract(&ask(64)).await.expect("extract");
    assert_eq!(report.tiles, 64);
    // Each archive: its directories, then its tiles' bytes, together.
    let archive_gets = counted.archive_gets();
    assert!(
        archive_gets <= 3 * 2,
        "{archive_gets} archive requests for 64 tiles of 3 archives"
    );
    assert_eq!(report.requests, counted.gets(), "the report counts them");
}

#[tokio::test]
async fn a_second_extract_lists_no_archive_again() {
    let clock = TestClock::new();
    let objects = memory();
    three_archives(objects.clone(), &clock).await;
    let counted = Counting::over(objects);
    let s = store_on(counted.clone(), &clock, eager());
    let (first, _) = s.extract(&ask(64)).await.expect("extract");
    let before = counted.archive_gets();
    let (second, _) = s.extract(&ask(64)).await.expect("extract");
    // The tiles' bytes again, one request an archive; never its directories.
    assert_eq!(counted.archive_gets() - before, 3);
    assert_eq!(
        std::fs::read(first.path()).expect("first"),
        std::fs::read(second.path()).expect("second")
    );
}

#[tokio::test]
async fn archives_copied_to_this_disk_are_read_from_it() {
    let clock = TestClock::new();
    let objects = memory();
    three_archives(objects.clone(), &clock).await;
    let dir = tempfile::tempdir().expect("dir");
    let counted = Counting::over(objects.clone());
    let s = store_on(counted.clone(), &clock, eager())
        .with_disk_cache(DiskCacheConfig::new(dir.path()))
        .await
        .expect("disk cache");
    // A read of the zone copies its archives here.
    let (x, y) = in_zone(0);
    s.get(IMAGERY, LEVEL, x, y).await.expect("get");
    s.settle().await;
    let before = counted.archive_gets();
    let (file, report) = s.extract(&ask(64)).await.expect("extract");
    assert_eq!(report.tiles, 64);
    assert_eq!(
        counted.archive_gets(),
        before,
        "no archive asked of the bucket"
    );
    let plain = store_on(objects, &clock, eager());
    let (bucket, _) = plain.extract(&ask(64)).await.expect("extract");
    assert_eq!(
        std::fs::read(file.path()).expect("local"),
        std::fs::read(bucket.path()).expect("bucket")
    );
}

#[tokio::test]
async fn an_archive_gone_since_it_was_listed_leaves_its_tiles_to_the_older_ones() {
    let clock = TestClock::new();
    let objects = memory();
    three_archives(objects.clone(), &clock).await;
    let s: TileStore = store_on(objects.clone(), &clock, eager());
    s.extract(&ask(64)).await.expect("listed once");
    // The newest archive's object deleted, its manifest entry left: as a
    // compaction that removed it under a reader's manifest.
    let keys = archives(objects.as_ref(), "").await;
    let newest = keys
        .iter()
        .max_by_key(|k| {
            let name = k.rsplit('/').next().unwrap_or(k);
            name.rsplit('-').nth(1).map(str::to_string)
        })
        .expect("an archive")
        .clone();
    object_store::ObjectStoreExt::delete(
        objects.as_ref(),
        &object_store::path::Path::from(newest.as_str()),
    )
    .await
    .expect("delete");
    let (file, report) = s.extract(&ask(64)).await.expect("extract");
    // 40..48 fall back to v2; 48..64 were only in the deleted one.
    assert_eq!((report.tiles, report.absent), (48, 16));
    let grid = imagery().grid;
    let (x, y) = in_zone(40);
    let id = grid.to_id(LEVEL, x, y).expect("id");
    let got = tiles_of(file.path()).await;
    assert_eq!(
        got.iter().find(|(i, _)| *i == id).map(|(_, b)| b.clone()),
        Some(body(LEVEL, x, y, 2))
    );
}
