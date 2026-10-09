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
use tuile_tile_server::{
    Emitted, FreshTiles, ServiceConfig, SharedTile, Source, TileService, TileStore, Upstream, UpstreamError,
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
