// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The store on one writer: reading through buffer, deltas and base; misses
//! that stay misses; compaction that changes nothing a reader sees.

mod common;

use std::time::Duration;

use common::*;
use object_store::ObjectStoreExt;
use tuile_tile_server::{Compaction, Zone};

#[tokio::test]
async fn a_tile_is_readable_the_moment_it_is_put() {
    let clock = TestClock::new();
    let s = store_on(memory(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(s.get(IMAGERY, LEVEL, X0, 5969).await.expect("get"), None);
}

#[tokio::test]
async fn the_newest_archive_wins_over_older_ones() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.put(IMAGERY, LEVEL, 8353, Y0, body(LEVEL, 8353, Y0, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 2)).await.expect("put");
    s.flush_all().await.expect("flush");

    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 2)));
    assert_eq!(fresh.get(IMAGERY, LEVEL, 8353, Y0).await.expect("get"), Some(body(LEVEL, 8353, Y0, 1)));
}

#[tokio::test]
async fn a_vanished_archive_is_a_miss_never_an_error() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    for key in archives(objects.as_ref(), IMAGERY).await {
        objects.delete(&key.as_str().into()).await.expect("delete");
    }
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("a miss, not an error"), None);
}

#[tokio::test]
async fn a_full_buffer_freezes_into_a_delta_by_itself() {
    let clock = TestClock::new();
    let objects = memory();
    let mut cfg = eager();
    cfg.flush_bytes = 4096;
    let s = store_on(objects.clone(), &clock, cfg);
    // All in one zone, so one buffer fills up.
    let mut i = 0u32;
    while archives(objects.as_ref(), IMAGERY).await.is_empty() {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1)).await.expect("put");
        i += 1;
        assert!(i < ZONE_SIDE * ZONE_SIDE, "the buffer never froze");
    }
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
}

#[tokio::test]
async fn many_small_zones_past_the_global_cap_flush_the_largest() {
    let clock = TestClock::new();
    let objects = memory();
    let mut cfg = eager();
    cfg.max_buffered_bytes = 2048;
    let s = store_on(objects.clone(), &clock, cfg);
    // One tile per zone, across a row of zones: no zone ever fills up.
    let mut zone_x = ZONE_X;
    while archives(objects.as_ref(), IMAGERY).await.is_empty() {
        let x = zone_x * ZONE_SIDE;
        s.put(IMAGERY, LEVEL, x, Y0, body(LEVEL, x, Y0, 1)).await.expect("put");
        zone_x += 1;
        assert!(zone_x < ZONE_X + 200, "the global cap never flushed anything");
    }
}

#[tokio::test]
async fn an_old_buffer_is_flushed_by_flush_due() {
    let clock = TestClock::new();
    let objects = memory();
    let mut cfg = eager();
    cfg.flush_age = Duration::from_millis(20);
    let s = store_on(objects.clone(), &clock, cfg);
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    assert_eq!(s.flush_due().await.expect("flush_due"), 0, "not old yet");
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(s.flush_due().await.expect("flush_due"), 1);
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), 1);
}

#[tokio::test]
async fn compaction_changes_no_byte_a_reader_sees() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    let mut expected = std::collections::BTreeMap::new();
    for round in 0..5u32 {
        for i in 0..200u32 {
            // A different subset of the zone every round, overlapping.
            let (x, y) = in_zone(i * 7 + round);
            let b = body(LEVEL, x, y, round);
            s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
            expected.insert((x, y), b);
        }
        s.flush_all().await.expect("flush");
    }
    let zone = zone();
    let before = archives(objects.as_ref(), IMAGERY).await.len();
    assert!(before >= 5);
    let outcome = s.compact(IMAGERY, zone).await.expect("compact");
    assert!(matches!(outcome, Compaction::Merged { merged: 5, .. }), "{outcome:?}");

    let fresh = store_on(objects.clone(), &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "{x}/{y}");
    }
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone))
        .await
        .expect("manifest")
        .manifest;
    assert_eq!(m.archives.len(), 1, "five deltas became one archive");
    assert_eq!(m.retired.len(), 5, "the inputs wait out their grace");
}

#[tokio::test]
async fn compaction_never_merges_two_epochs() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    for v in 0..2 {
        s.put(IMAGERY, LEVEL, X0 + v, Y0, body(LEVEL, X0 + v, Y0, v)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    clock.advance(days(31)); // next month
    for v in 2..4 {
        s.put(IMAGERY, LEVEL, X0 + v, Y0, body(LEVEL, X0 + v, Y0, v)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    let zone = zone();
    assert!(matches!(s.compact(IMAGERY, zone).await.expect("compact"), Compaction::Merged { merged: 2, .. }));
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone))
        .await
        .expect("manifest")
        .manifest;
    let epochs: Vec<&str> = m.archives.iter().map(|a| a.epoch.as_str()).collect();
    assert_eq!(epochs, ["202609", "202609", "202610"], "last month's two archives are untouched");
}

#[tokio::test]
async fn tiles_past_their_expiry_are_no_longer_served() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.put(TERRAIN, 12, 4100, 2900, body(12, 4100, 2900, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    clock.advance(days(89));
    let fresh = store_on(objects.clone(), &clock, eager());
    assert!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get").is_some(), "still within 90 days of the month's end");
    clock.advance(days(40));
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None, "expired");
    assert!(fresh.get(TERRAIN, 12, 4100, 2900).await.expect("get").is_some(), "a durable layer keeps its tiles");
}

#[tokio::test]
async fn coarse_tiles_share_the_top_zone_and_fine_ones_their_cell() {
    let l = common::imagery();
    // One level above the zone level: coarser than any zone.
    assert_eq!(l.zone_of(ZONE_LEVEL - 1, ZONE_X / 2, ZONE_Y / 2).expect("zone"), Zone::Top);
    assert_eq!(l.zone_of(ZONE_LEVEL, ZONE_X, ZONE_Y).expect("zone"), zone());
    // Seven levels down, the last row of the zone is still the zone.
    let side = 1u32 << 7;
    assert_eq!(l.zone_of(ZONE_LEVEL + 7, ZONE_X * side + 5, ZONE_Y * side + side - 1).expect("zone"), zone());
    assert!(l.zone_of(3, 1 << 3, 0).is_err(), "x one past the edge of z3");

    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, 2, 1, 1, body(2, 1, 1, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    assert_eq!(archives(objects.as_ref(), "imagery/top").await.len(), 1);
}

#[tokio::test]
async fn tiered_compaction_leaves_a_large_base_alone_while_small_deltas_pile_up() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    let prefix = common::imagery().zone_prefix(zone());
    let mut expected = std::collections::BTreeMap::new();

    // A base holding the whole zone.
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        let b = body(LEVEL, x, y, 0);
        s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
        expected.insert((x, y), b);
    }
    s.flush_all().await.expect("flush");
    let base = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest.archives[0]
        .key
        .clone();

    // Then many small deltas, each followed by a maintenance pass.
    const DELTAS: u32 = 24;
    const DELTA_TILES: u32 = 3;
    for d in 0..DELTAS {
        for t in 0..DELTA_TILES {
            let (x, y) = in_zone(d * DELTA_TILES + t);
            let b = body(LEVEL, x, y, d + 1);
            s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
            expected.insert((x, y), b);
        }
        s.flush_all().await.expect("flush");
        s.compact_due().await.expect("compact_due");
        let m = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest;
        let sizes: Vec<u64> = m.archives.iter().map(|a| a.bytes).collect();
        assert_eq!(m.archives[0].key, base, "the base was rewritten after delta {d}: sizes {sizes:?}");
        assert!(m.archives.len() <= s.object_store_config().tiering.max_archives, "{} archives", m.archives.len());
    }
    let fresh = store_on(objects, &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "{x}/{y}");
    }
}

#[tokio::test]
async fn a_maintenance_pass_finds_every_zone_and_drops_expired_archives() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    // Three zones: the test zone, its neighbour, and the top zone.
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 0)).await.expect("put");
    s.put(IMAGERY, LEVEL, X0 + ZONE_SIDE, Y0, body(LEVEL, X0 + ZONE_SIDE, Y0, 0)).await.expect("put");
    s.put(IMAGERY, 2, 1, 1, body(2, 1, 1, 0)).await.expect("put");
    s.put(TERRAIN, 12, 4100, 2900, body(12, 4100, 2900, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    let mut zones = s.zones().await.expect("zones");
    zones.sort();
    assert_eq!(zones.len(), 4, "{zones:?}");

    clock.advance(days(130));
    s.compact_due().await.expect("compact_due");
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    assert!(m.archives.is_empty(), "expired imagery retired");
    assert_eq!(m.retired.len(), 1);
    let terrain = tuile_tile_server::manifest::read(objects.as_ref(), &common::terrain().zone_prefix(tuile_tile_server::Zone::Cell { x: 4100 >> 3, y: 2900 >> 3 }))
        .await
        .expect("manifest")
        .manifest;
    assert_eq!(terrain.archives.len(), 1, "durable terrain kept");
}
