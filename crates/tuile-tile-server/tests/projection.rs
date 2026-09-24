// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Projections: a scene's slice of the store, fetched once into local
//! archives, read before the bucket.

mod common;

use common::*;
use tuile_tile_server::projection::{bounds, DEFAULT_TILE_FACTOR};
use tuile_tile_server::{Eye, Footprint, Grid, TileStore};

/// The centre of a Web Mercator tile, as an eye position at `height`.
fn eye_over(level: u8, x: u32, y: u32, height: f64) -> Eye {
    let [w, s, e, n] = bounds(Grid::WebMercator, level, x, y);
    Eye { lon: (w + e) / 2.0, lat: (s + n) / 2.0, height }
}

/// A zone filled at `LEVEL` (every tile) and a coarse tile in `top`,
/// published, then a store opened fresh on the same bucket.
async fn published() -> (std::sync::Arc<dyn object_store::ObjectStore>, TestClock) {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 0)).await.expect("put");
    }
    s.put(IMAGERY, 3, 4, 2, body(3, 4, 2, 0)).await.expect("put");
    s.put(IMAGERY, 3, 0, 7, body(3, 0, 7, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    (objects, clock)
}

#[tokio::test]
async fn a_projection_keeps_what_the_scene_can_see_and_reads_it_locally() {
    let (objects, clock) = published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    // An eye 1 km above the ground, over the zone's first tile.
    let eye = eye_over(LEVEL, X0, Y0, 5_800.0);
    let fp = Footprint::from_eyes([eye], DEFAULT_TILE_FACTOR);
    let report = s.project(&fp, dir.path()).await.expect("project");
    assert!(report.zones_projected >= 2, "the cell and the top zone: {report:?}");
    assert!(report.tiles_kept > 0 && report.tiles_kept < report.tiles_listed, "filtered: {report:?}");
    assert_eq!(report.requests as usize, report.zones_seen, "one request per archive, one archive per zone here: {report:?}");

    // The tile under the eye: from the projection, bytes identical.
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 0)));
    let stats = s.stats();
    assert_eq!((stats.projection, stats.remote), (1, 0), "{stats:?}");

    // The coarse tile over Europe is kept; the one over the Pacific is not,
    // and is still served — from the bucket, counted.
    assert_eq!(s.get(IMAGERY, 3, 4, 2).await.expect("get"), Some(body(3, 4, 2, 0)));
    assert_eq!(s.get(IMAGERY, 3, 0, 7).await.expect("get"), Some(body(3, 0, 7, 0)));
    let stats = s.stats();
    assert_eq!(stats.projection, 2, "{stats:?}");
    assert_eq!((stats.projection_misses, stats.remote), (1, 1), "{stats:?}");
}

#[tokio::test]
async fn every_tile_of_every_projection_is_the_bucket_s_tile() {
    let (objects, clock) = published().await;
    let reference = store_on(objects.clone(), &clock, eager());
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    let fp = Footprint::from_eyes([eye_over(LEVEL, X0 + 8, Y0 + 8, 20_000.0)], DEFAULT_TILE_FACTOR);
    s.project(&fp, dir.path()).await.expect("project");
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), reference.get(IMAGERY, LEVEL, x, y).await.expect("get"));
    }
    assert!(s.stats().projection > 0);
}

#[tokio::test]
async fn the_newest_archive_wins_in_a_projection_too() {
    let (objects, clock) = published().await;
    let w = store_on(objects.clone(), &clock, eager());
    w.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 9)).await.expect("put");
    w.flush_all().await.expect("flush");
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    s.project(&Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], DEFAULT_TILE_FACTOR), dir.path())
        .await
        .expect("project");
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 9)));
    assert_eq!(s.stats().projection, 1);
}

#[tokio::test]
async fn projections_are_plain_archives_on_disk() {
    let (objects, clock) = published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    s.project(&Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], DEFAULT_TILE_FACTOR), dir.path())
        .await
        .expect("project");
    let local = tuile_tile_server::projection::projection_path(dir.path(), &common::imagery(), zone());
    let reader = tuile_tile_server::archive::open_local(&local).await.expect("a readable local archive");
    let tile = reader
        .get_tile(pmtiles::TileId::new(Grid::WebMercator.to_id(LEVEL, X0, Y0).expect("id")).expect("id"))
        .await
        .expect("get");
    assert_eq!(tile, Some(body(LEVEL, X0, Y0, 0)));
}

#[tokio::test]
async fn a_scene_far_away_projects_nothing_of_the_zone() {
    let (objects, clock) = published().await;
    let s: TileStore = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    // Low over the Pacific.
    let fp = Footprint::from_eyes([Eye { lon: -150.0, lat: -20.0, height: 1_000.0 }], DEFAULT_TILE_FACTOR);
    let report = s.project(&fp, dir.path()).await.expect("project");
    assert!(!tuile_tile_server::projection::projection_path(dir.path(), &common::imagery(), zone()).exists());
    // The top zone still projects the coarse tile over the Pacific.
    assert!(report.zones_projected <= 1, "{report:?}");
}
