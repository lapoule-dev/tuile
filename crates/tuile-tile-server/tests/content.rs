// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The store as the engine's cache: the catalog in the bucket, the engine's
//! keys, absences remembered in a sibling layer.

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::*;
use tuile_core::storage::{ContentStore, ABSENT};
use tuile_tile_server::{catalog, StoreContent, TileStore};

#[tokio::test]
async fn a_store_opens_on_the_catalog_its_bucket_carries() {
    let objects = memory();
    assert!(TileStore::open(objects.clone(), eager()).await.is_err(), "no catalog, no store");
    catalog::write(objects.as_ref(), &common::catalog()).await.expect("write catalog");
    let opened = TileStore::open(objects.clone(), eager()).await.expect("open");
    let imagery = opened.layer(IMAGERY).expect("listed");
    assert_eq!(imagery.zone_level, common::imagery().zone_level);
    assert_eq!(imagery.expiry, common::imagery().expiry);
    assert!(opened.layer("imagery.absent").is_ok(), "each layer has its absence sibling");
    assert!(opened.layer("nope").is_err());
}

#[tokio::test]
async fn engine_keys_land_in_the_layers_and_come_back_from_another_process() {
    let objects = memory();
    catalog::write(objects.as_ref(), &common::catalog()).await.expect("catalog");
    let writer = Arc::new(TileStore::open(objects.clone(), eager()).await.expect("open"));
    let cache = StoreContent::new(writer.clone());
    let key = format!("img/{IMAGERY}/{LEVEL}/{X0}/{Y0}");
    cache.put(&key, body(LEVEL, X0, Y0, 1), None).await;
    cache.put(&format!("mesh/{TERRAIN}/0/1/0"), Bytes::from_static(b"mesh"), None).await;
    assert_eq!(cache.get(&key).await, Some(body(LEVEL, X0, Y0, 1)), "readable before any flush");
    writer.flush_all().await.expect("flush");

    let other = StoreContent::new(Arc::new(TileStore::open(objects, eager()).await.expect("open")));
    assert_eq!(other.get(&key).await, Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(other.get(&format!("mesh/{TERRAIN}/0/1/0")).await, Some(Bytes::from_static(b"mesh")));
    assert_eq!(other.get(&format!("img/{IMAGERY}/{LEVEL}/{X0}/{}", Y0 + 1)).await, None);
}

#[tokio::test]
async fn an_absence_is_remembered_without_touching_the_layer_s_archives() {
    let objects = memory();
    catalog::write(objects.as_ref(), &common::catalog()).await.expect("catalog");
    let store = Arc::new(TileStore::open(objects.clone(), eager()).await.expect("open"));
    let cache = StoreContent::new(store.clone());
    let key = format!("img/{IMAGERY}/2/1/1");
    cache.put(&key, ABSENT, None).await;
    store.flush_all().await.expect("flush");

    let fresh = StoreContent::new(Arc::new(TileStore::open(objects.clone(), eager()).await.expect("open")));
    let got = fresh.get(&key).await.expect("remembered");
    assert!(tuile_core::storage::is_absent(&got), "read back as the engine's absence");
    // The image layer itself holds nothing for it.
    assert!(archives(objects.as_ref(), &format!("{IMAGERY}/")).await.is_empty());
    assert_eq!(archives(objects.as_ref(), "imagery.absent/").await.len(), 1);
}

#[tokio::test]
async fn keys_the_store_does_not_know_are_misses_not_errors() {
    let objects = memory();
    catalog::write(objects.as_ref(), &common::catalog()).await.expect("catalog");
    let cache = StoreContent::new(Arc::new(TileStore::open(objects, eager()).await.expect("open")));
    assert_eq!(cache.get("img/unknown-layer/3/1/1").await, None);
    assert_eq!(cache.get("not-a-tile-key").await, None);
    assert_eq!(cache.get(&format!("img/{IMAGERY}/3/99/0")).await, None, "out of the grid");
    cache.put("img/unknown-layer/3/1/1", Bytes::from_static(b"x"), None).await; // declined, silently
}
