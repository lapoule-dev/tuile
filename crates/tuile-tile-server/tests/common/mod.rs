// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use object_store::memory::InMemory;
use object_store::ObjectStore;
use tuile_tile_server::{Clock, Compression, Grid, Layer, StoreConfig, TileStore, TileType};

pub const IMAGERY: &str = "imagery";
pub const TERRAIN: &str = "terrain";

/// The imagery layer's zone level, and the terrain layer's.
pub const ZONE_LEVEL: u8 = 10;
pub const TERRAIN_ZONE_LEVEL: u8 = 9;
/// The level most tests write at, a few levels below the zone level.
pub const LEVEL: u8 = 14;
/// Tiles per side of one zone at [`LEVEL`].
pub const ZONE_SIDE: u32 = 1 << (LEVEL - ZONE_LEVEL);
/// The zone the tests write into (any would do; this one is over the Pyrenees).
pub const ZONE_X: u32 = 522;
pub const ZONE_Y: u32 = 373;
/// Its first tile at [`LEVEL`].
pub const X0: u32 = ZONE_X * ZONE_SIDE;
pub const Y0: u32 = ZONE_Y * ZONE_SIDE;

pub fn zone() -> tuile_tile_server::Zone {
    tuile_tile_server::Zone::Cell { x: ZONE_X, y: ZONE_Y }
}

/// The `i`-th tile of the test zone at [`LEVEL`], row by row, wrapping.
pub fn in_zone(i: u32) -> (u32, u32) {
    (X0 + i % ZONE_SIDE, Y0 + (i / ZONE_SIDE) % ZONE_SIDE)
}

pub const fn days(n: u64) -> Duration {
    Duration::from_secs(n * 86_400)
}

/// The imagery layer's lifetime.
pub const IMAGERY_EXPIRY: Duration = days(90);

/// An expiring Web Mercator layer, zones at z10.
pub fn imagery() -> Layer {
    Layer {
        name: IMAGERY.into(),
        grid: Grid::WebMercator,
        tile_type: TileType::Jpeg,
        tile_compression: Compression::None,
        zone_level: ZONE_LEVEL,
        expiry: Some(IMAGERY_EXPIRY),
        content_type: "image/jpeg".into(),
    }
}

/// A durable geographic layer, zones at level 9.
pub fn terrain() -> Layer {
    Layer {
        name: TERRAIN.into(),
        grid: Grid::Geographic,
        tile_type: TileType::Unknown,
        tile_compression: Compression::Gzip,
        zone_level: TERRAIN_ZONE_LEVEL,
        expiry: None,
        content_type: "application/vnd.quantized-mesh".into(),
    }
}

/// 2026-09-15T00:00:00Z: mid-month, so a month later is another epoch.
pub const START: u64 = 1_789_430_400;

/// A clock the test moves by hand, from [`START`].
#[derive(Clone)]
pub struct TestClock(pub Arc<AtomicU64>);

impl TestClock {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(START)))
    }
    pub fn clock(&self) -> Clock {
        let t = self.0.clone();
        Arc::new(move || UNIX_EPOCH + Duration::from_secs(t.load(Ordering::SeqCst)))
    }
    pub fn advance(&self, d: Duration) {
        self.0.fetch_add(d.as_secs(), Ordering::SeqCst);
    }
    pub fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.0.load(Ordering::SeqCst))
    }
}

/// Settings that make every publication visible at once and never flush on
/// their own, so a test decides when things happen. (Nothing compacts on its
/// own either way: only `compact*` calls do.) Graces keep their defaults.
pub fn eager() -> StoreConfig {
    StoreConfig {
        flush_bytes: usize::MAX,
        max_buffered_bytes: usize::MAX,
        flush_age: Duration::MAX,
        manifest_ttl: Duration::ZERO,
        // Many writers on one zone in the tests: let them all get through.
        publish_attempts: 1000,
        ..StoreConfig::default()
    }
}

pub fn memory() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

pub fn store_on(objects: Arc<dyn ObjectStore>, clock: &TestClock, cfg: StoreConfig) -> TileStore {
    TileStore::with_clock(objects, vec![imagery(), terrain()], cfg, clock.clock())
}

/// Distinct, recognisable bytes for a tile and a version.
pub fn body(level: u8, x: u32, y: u32, version: u32) -> Bytes {
    let mut v = format!("tile {level}/{x}/{y} v{version} ").into_bytes();
    // Vary the length, so no two tiles are accidentally the same size.
    const LENGTH_SPREAD: u32 = 97;
    v.extend(std::iter::repeat_n(b'.', ((x ^ y) % LENGTH_SPREAD) as usize));
    Bytes::from(v)
}

/// Every archive object under a prefix.
pub async fn archives(objects: &dyn ObjectStore, prefix: &str) -> Vec<String> {
    use futures_util::TryStreamExt;
    let listed: Vec<object_store::ObjectMeta> =
        objects.list(Some(&object_store::path::Path::from(prefix))).try_collect().await.expect("list");
    let mut keys: Vec<String> =
        listed.into_iter().map(|m| m.location.to_string()).filter(|k| k.ends_with(".pmtiles")).collect();
    keys.sort();
    keys
}

/// The catalog the test layers come from.
pub fn catalog() -> tuile_tile_server::Catalog {
    use tuile_tile_server::LayerDef;
    tuile_tile_server::Catalog {
        layers: vec![
            LayerDef {
                name: IMAGERY.into(),
                grid: Grid::WebMercator,
                tile_type: "jpeg".into(),
                tile_compression: "none".into(),
                zone_level: ZONE_LEVEL,
                expiry_days: Some(IMAGERY_EXPIRY.as_secs() / 86_400),
                content_type: "image/jpeg".into(),
            },
            LayerDef {
                name: TERRAIN.into(),
                grid: Grid::Geographic,
                tile_type: "other".into(),
                tile_compression: "gzip".into(),
                zone_level: TERRAIN_ZONE_LEVEL,
                expiry_days: None,
                content_type: "application/vnd.quantized-mesh".into(),
            },
        ],
    }
}
