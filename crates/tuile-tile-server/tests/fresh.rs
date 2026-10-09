// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A serving instance that hands its fresh tiles on instead of writing them:
//! what it emits, what it no longer does, and what another process makes of
//! a batch.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use futures_util::TryStreamExt;
use object_store::ObjectStore;
use tuile_tile_server::catalog::layers;
use tuile_tile_server::catalog::{ABSENT_MARKER, ABSENT_SUFFIX};
use tuile_tile_server::{
    Census, Compaction, Emitted, Fresh, FreshTiles, ServiceConfig, SharedTile, Source, TileService, TileStore, Upstream,
    UpstreamError,
};

/// A source that has every tile except those with `x == 0`.
struct Counted(AtomicUsize);

#[async_trait]
impl Upstream for Counted {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok((x != 0).then(|| body(level, x, y, 0)))
    }
}

/// A sink that keeps what it is given, or refuses everything.
struct Sink {
    emitted: Mutex<Vec<(String, u8, u32, u32, SharedTile)>>,
    keeps: bool,
}

impl Sink {
    fn new(keeps: bool) -> Arc<Self> {
        Arc::new(Self { emitted: Mutex::new(Vec::new()), keeps })
    }
    fn emitted(&self) -> Vec<(String, u8, u32, u32, SharedTile)> {
        self.emitted.lock().expect("lock").clone()
    }
}

#[async_trait]
impl FreshTiles for Sink {
    async fn emit(&self, layer: &str, level: u8, x: u32, y: u32, tile: &SharedTile) -> Emitted {
        self.emitted.lock().expect("lock").push((layer.to_string(), level, x, y, tile.clone()));
        if self.keeps {
            Emitted::Kept
        } else {
            Emitted::NotKept
        }
    }
}

fn store(objects: &Arc<dyn ObjectStore>, clock: &TestClock) -> Arc<TileStore> {
    Arc::new(TileStore::with_clock(objects.clone(), layers(&catalog()).expect("layers"), eager(), clock.clock()))
}

fn service(store: Arc<TileStore>, source: &Arc<Counted>) -> TileService {
    let mut ups: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    ups.insert(IMAGERY.into(), source.clone());
    TileService::with_config(store, ups, ServiceConfig::default())
}

async fn objects_in(objects: &dyn ObjectStore) -> usize {
    let listed: Vec<object_store::ObjectMeta> = objects.list(None).try_collect().await.expect("list");
    listed.len()
}

#[tokio::test]
async fn with_a_sink_a_service_emits_each_fresh_tile_once_and_writes_nothing() {
    let (clock, objects) = (TestClock::new(), memory());
    let source = Arc::new(Counted(AtomicUsize::new(0)));
    let sink = Sink::new(true);
    let st = store(&objects, &clock);
    let s = service(st.clone(), &source).with_sink(sink.clone());

    let tile = s.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!((tile.source, tile.bytes.clone()), (Source::Upstream, body(LEVEL, X0, Y0, 0)));
    assert!(s.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none(), "the source has no such tile");
    // Asked again: from this instance's memory, and nothing more is emitted.
    assert_eq!(s.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present").source, Source::Memory);
    assert!(s.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    assert_eq!(source.0.load(Ordering::SeqCst), 2);

    let emitted = sink.emitted();
    assert_eq!(emitted.len(), 2, "the tile and the absence, once each: {emitted:?}");
    assert_eq!((emitted[0].0.as_str(), emitted[0].1, emitted[0].2, emitted[0].3), (IMAGERY, LEVEL, X0, Y0));
    assert_eq!(emitted[0].4.bytes, Some(body(LEVEL, X0, Y0, 0)));
    assert!(emitted[0].4.fetched_ms > 0);
    // The absence under the layer's own name, as a tile without bytes.
    assert_eq!((emitted[1].0.as_str(), emitted[1].2, emitted[1].4.bytes.clone()), (IMAGERY, 0, None));

    // Nothing in this instance's store, and nothing it could flush.
    assert_eq!(st.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None);
    assert_eq!(st.flush_all().await.expect("flush"), 0);
    assert_eq!(objects_in(objects.as_ref()).await, 0, "the bucket was not written to");
}

#[tokio::test]
async fn a_tile_the_sink_did_not_keep_is_served_all_the_same() {
    let (clock, objects) = (TestClock::new(), memory());
    let source = Arc::new(Counted(AtomicUsize::new(0)));
    let sink = Sink::new(false);
    let s = service(store(&objects, &clock), &source).with_sink(sink.clone());
    let tile = s.tile(IMAGERY, LEVEL, X0, Y0).await.expect("no error").expect("present");
    assert_eq!(tile.bytes, body(LEVEL, X0, Y0, 0));
    assert_eq!(sink.emitted().len(), 1);
}

#[tokio::test]
async fn a_tile_learnt_from_another_is_served_from_memory_without_the_source() {
    let (clock, objects) = (TestClock::new(), memory());
    let source = Arc::new(Counted(AtomicUsize::new(0)));
    let s = service(store(&objects, &clock), &source);
    s.learn(IMAGERY, LEVEL, X0, Y0, SharedTile { bytes: Some(body(LEVEL, X0, Y0, 7)), fetched_ms: 1 });
    s.learn(IMAGERY, LEVEL, X0 + 1, Y0, SharedTile { bytes: None, fetched_ms: 1 });

    let tile = s.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!((tile.source, tile.bytes), (Source::Memory, body(LEVEL, X0, Y0, 7)));
    assert!(s.tile(IMAGERY, LEVEL, X0 + 1, Y0).await.expect("tile").is_none(), "a learnt absence is an absence");
    assert_eq!(source.0.load(Ordering::SeqCst), 0);
    assert_eq!(s.stats().learned, 2);
    assert_eq!(objects_in(objects.as_ref()).await, 0);
}

// ── What another process makes of a batch ────────────────────────────────

fn fresh(i: u32, version: u32, fetched_ms: u64) -> Fresh {
    let (x, y) = in_zone(i);
    Fresh { level: LEVEL, x, y, bytes: body(LEVEL, x, y, version), fetched_ms }
}

#[tokio::test]
async fn a_fresh_tile_is_placed_in_its_layer_and_zone_and_an_absence_in_the_sibling() {
    let (clock, objects) = (TestClock::new(), memory());
    let with_siblings = store(&objects, &clock);
    assert_eq!(with_siblings.place(IMAGERY, LEVEL, X0, Y0, false).expect("place"), Some((IMAGERY.to_string(), zone())));
    assert_eq!(
        with_siblings.place(IMAGERY, LEVEL, X0, Y0, true).expect("place"),
        Some((format!("{IMAGERY}{ABSENT_SUFFIX}"), zone()))
    );
    // A store whose layers keep no absences: nowhere, and not an error.
    let without = store_on(objects, &clock, eager());
    assert_eq!(without.place(IMAGERY, LEVEL, X0, Y0, true).expect("place"), None);
    // A tile nobody could address is refused either way.
    assert!(without.place(IMAGERY, 3, 99, 0, true).is_err());
    assert!(without.place("no-such-layer", LEVEL, X0, Y0, false).is_err());
}

#[tokio::test]
async fn a_batch_published_with_no_buffer_gives_the_store_what_a_flush_gives() {
    let clock = TestClock::new();
    // The way it was: tiles put, then flushed.
    let flushed = memory();
    let a = store(&flushed, &clock);
    for i in 0..5 {
        let (x, y) = in_zone(i);
        a.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1)).await.expect("put");
    }
    a.put(&format!("{IMAGERY}{ABSENT_SUFFIX}"), LEVEL, X0 + 9, Y0, Bytes::from_static(ABSENT_MARKER)).await.expect("put");
    a.flush_all().await.expect("flush");

    // The same tiles, by a process that never buffered them.
    let batched = memory();
    let b = store(&batched, &clock);
    let wrote = b.publish_batch(IMAGERY, zone(), (0..5).map(|i| fresh(i, 1, 10)).collect()).await.expect("batch");
    assert_eq!(wrote, 5);
    let absence = Fresh { level: LEVEL, x: X0 + 9, y: Y0, bytes: Bytes::from_static(ABSENT_MARKER), fetched_ms: 10 };
    let (layer, at) = b.place(IMAGERY, LEVEL, X0 + 9, Y0, true).expect("place").expect("a sibling");
    assert_eq!(b.publish_batch(&layer, at, vec![absence]).await.expect("batch"), 1);
    assert_eq!(b.flush_all().await.expect("flush"), 0, "nothing was ever buffered");

    for objects in [&flushed, &batched] {
        let reader = store(objects, &clock);
        for i in 0..5 {
            let (x, y) = in_zone(i);
            assert_eq!(reader.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 1)));
        }
        assert!(reader.get(&format!("{IMAGERY}{ABSENT_SUFFIX}"), LEVEL, X0 + 9, Y0).await.expect("get").is_some());
        assert_eq!(
            reader.census(IMAGERY, zone()).await.expect("census"),
            Census { archives: 1, copies: 5, tiles: 5, repeated: 0 }
        );
    }
}

#[tokio::test]
async fn the_same_batch_from_two_processes_is_stored_once() {
    let (clock, objects) = (TestClock::new(), memory());
    let (a, b) = (store(&objects, &clock), store(&objects, &clock));
    let batch = || (0..6).map(|i| fresh(i, 1, 10)).collect::<Vec<_>>();
    assert_eq!(a.publish_batch(IMAGERY, zone(), batch()).await.expect("batch"), 6);
    assert_eq!(b.publish_batch(IMAGERY, zone(), batch()).await.expect("batch"), 0, "all of it is there already");
    assert_eq!(b.census(IMAGERY, zone()).await.expect("census"), Census { archives: 1, copies: 6, tiles: 6, repeated: 0 });

    // A tile fetched again, changed: written, and the census says it is held twice.
    assert_eq!(b.publish_batch(IMAGERY, zone(), vec![fresh(0, 2, 20)]).await.expect("batch"), 1);
    assert_eq!(a.census(IMAGERY, zone()).await.expect("census"), Census { archives: 2, copies: 7, tiles: 6, repeated: 1 });
    let (x, y) = in_zone(0);
    assert_eq!(store(&objects, &clock).get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 2)));
}

#[tokio::test]
async fn a_tile_twice_in_a_batch_is_the_one_fetched_last_and_a_stray_tile_fails_the_batch() {
    let (clock, objects) = (TestClock::new(), memory());
    let s = store(&objects, &clock);
    // Whatever the order they come in: the later fetch first, or last.
    let batch = vec![fresh(0, 2, 20), fresh(0, 1, 10), fresh(2, 1, 10), fresh(2, 2, 20)];
    assert_eq!(s.publish_batch(IMAGERY, zone(), batch).await.expect("batch"), 2);
    for i in [0, 2] {
        let (x, y) = in_zone(i);
        assert_eq!(store(&objects, &clock).get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 2)));
    }

    // A tile of the zone east of it, offered for this one: nothing is written.
    let before = objects_in(objects.as_ref()).await;
    let stray = Fresh { level: LEVEL, x: X0 + ZONE_SIDE, y: Y0, bytes: body(LEVEL, X0 + ZONE_SIDE, Y0, 1), fetched_ms: 30 };
    assert!(s.publish_batch(IMAGERY, zone(), vec![fresh(1, 1, 30), stray]).await.is_err());
    assert_eq!(objects_in(objects.as_ref()).await, before);
}

#[tokio::test]
async fn one_zone_is_maintained_by_itself() {
    let (clock, objects) = (TestClock::new(), memory());
    let s = store(&objects, &clock);
    let fanout = eager().tiering.fanout as u32;
    for i in 0..fanout {
        assert_eq!(s.publish_batch(IMAGERY, zone(), vec![fresh(i, 1, 10)]).await.expect("batch"), 1);
    }
    assert_eq!(s.census(IMAGERY, zone()).await.expect("census").archives, fanout as usize);

    let done = store(&objects, &clock).maintain(IMAGERY, zone()).await.expect("maintain");
    assert!(matches!(done.as_slice(), [Compaction::Merged { .. }]), "{done:?}");
    assert_eq!(
        s.census(IMAGERY, zone()).await.expect("census"),
        Census { archives: 1, copies: u64::from(fanout), tiles: u64::from(fanout), repeated: 0 }
    );
    // Nothing more is due.
    assert!(store(&objects, &clock).maintain(IMAGERY, zone()).await.expect("maintain").is_empty());
}
