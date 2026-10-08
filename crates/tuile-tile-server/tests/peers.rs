// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Several instances of a server over one bucket, sharing what they have
//! just fetched, the fetching of it, and the news of what they publish.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use object_store::ObjectStore;
use tuile_tile_server::catalog::layers;
use tuile_tile_server::peers::InMemory;
use tuile_tile_server::{
    Claim, ServiceConfig, SharedTile, SharedTiles, Source, StoreConfig, TileService, TileStore, Upstream,
    UpstreamError,
};

/// A source that takes its time, has every tile except those with `x == 0`,
/// and counts how often it is asked — by every instance together.
struct Slow {
    calls: AtomicUsize,
}

#[async_trait]
impl Upstream for Slow {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        Ok((x != 0).then(|| body(level, x, y, 0)))
    }
}

fn source() -> Arc<Slow> {
    Arc::new(Slow { calls: AtomicUsize::new(0) })
}

/// A store of its own over the bucket every instance shares.
fn store(objects: &Arc<dyn ObjectStore>, clock: &TestClock, cfg: StoreConfig) -> TileStore {
    TileStore::with_clock(objects.clone(), layers(&catalog()).expect("layers"), cfg, clock.clock())
}

/// An instance: its own store and its own memory, the source and the shared
/// buffer of all.
fn instance(
    objects: &Arc<dyn ObjectStore>,
    clock: &TestClock,
    source: &Arc<Slow>,
    shared: Option<Arc<dyn SharedTiles>>,
    cfg: ServiceConfig,
) -> TileService {
    let mut ups: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    ups.insert(IMAGERY.into(), source.clone());
    let service = TileService::with_config(Arc::new(store(objects, clock, eager())), ups, cfg);
    match shared {
        Some(shared) => service.with_shared(shared),
        None => service,
    }
}

fn quick() -> ServiceConfig {
    ServiceConfig { shared_poll: Duration::from_millis(5), ..ServiceConfig::default() }
}

#[tokio::test]
async fn an_instance_serves_what_another_has_fetched_and_not_yet_flushed() {
    let (clock, objects, source) = (TestClock::new(), memory(), source());
    let shared: Arc<dyn SharedTiles> = Arc::new(InMemory::default());
    let a = instance(&objects, &clock, &source, Some(shared.clone()), quick());
    let b = instance(&objects, &clock, &source, Some(shared), quick());

    let first = a.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!((first.source, first.hit), (Source::Upstream, false));
    // The tile is in A's buffer, not in the bucket: B's store has nothing.
    assert_eq!(b.store().get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None);

    let second = b.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!((second.source, second.hit), (Source::Shared, true));
    assert_eq!(second.bytes, first.bytes);
    assert_eq!(second.etag, first.etag);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1, "the source was asked once, by A");
    assert_eq!(b.stats().shared, 1);
    // B does not write it: publishing it is A's, who fetched it.
    assert_eq!(b.store().get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None);
    // And B's next answer is from its own memory.
    assert_eq!(b.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present").source, Source::Memory);
}

#[tokio::test]
async fn two_instances_asked_at_once_ask_the_source_once() {
    let (clock, objects, source) = (TestClock::new(), memory(), source());
    let shared: Arc<dyn SharedTiles> = Arc::new(InMemory::default());
    let a = instance(&objects, &clock, &source, Some(shared.clone()), quick());
    let b = instance(&objects, &clock, &source, Some(shared), quick());

    let (x, y) = in_zone(3);
    let (from_a, from_b) = tokio::join!(a.tile(IMAGERY, LEVEL, x, y), b.tile(IMAGERY, LEVEL, x, y));
    let (from_a, from_b) = (from_a.expect("tile").expect("present"), from_b.expect("tile").expect("present"));
    assert_eq!(source.calls.load(Ordering::SeqCst), 1, "one fetch for the two of them");
    assert_eq!(from_a.bytes, from_b.bytes);
    let mut sources = [from_a.source, from_b.source];
    sources.sort_by_key(|s| s.as_str());
    assert_eq!(sources, [Source::Shared, Source::Upstream], "one fetched, the other waited for it");
}

#[tokio::test]
async fn a_claim_never_given_back_does_not_stop_the_others_for_long() {
    let (clock, objects, source) = (TestClock::new(), memory(), source());
    let shared = Arc::new(InMemory::default());
    // An instance claimed the tile and died before fetching it.
    assert!(matches!(shared.claim(IMAGERY, LEVEL, X0, Y0).await, Claim::Mine(_)));
    assert_eq!(shared.claim(IMAGERY, LEVEL, X0, Y0).await, Claim::Theirs);

    let wait = Duration::from_millis(150);
    let cfg = ServiceConfig { shared_wait: wait, ..quick() };
    let b = instance(&objects, &clock, &source, Some(shared), cfg);
    let started = Instant::now();
    let tile = b.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    let took = started.elapsed();
    assert_eq!(tile.source, Source::Upstream, "it went and fetched the tile itself");
    assert!(took >= wait, "it waited for the other first: {took:?}");
    assert!(took < Duration::from_secs(2), "and not for ever: {took:?}");
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

/// What instances share, unreachable: nothing is ever there, nothing is
/// kept, and no claim can be told.
struct Unreachable;

#[async_trait]
impl SharedTiles for Unreachable {
    async fn get(&self, _: &str, _: u8, _: u32, _: u32) -> Option<SharedTile> {
        None
    }
    async fn put(&self, _: &str, _: u8, _: u32, _: u32, _: &SharedTile) {}
    async fn claim(&self, _: &str, _: u8, _: u32, _: u32) -> Claim {
        Claim::Unknown
    }
    async fn release(&self, _: &str, _: u8, _: u32, _: u32, _: &str) {}
}

#[tokio::test]
async fn with_nothing_shared_reachable_an_instance_is_as_one_that_shares_nothing() {
    let (clock, objects, source) = (TestClock::new(), memory(), source());
    let a = instance(&objects, &clock, &source, Some(Arc::new(Unreachable)), quick());
    let b = instance(&objects, &clock, &source, None, quick());

    let started = Instant::now();
    for service in [&a, &b] {
        let tile = service.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
        assert_eq!((tile.source, tile.hit), (Source::Upstream, false));
        assert_eq!(tile.bytes, body(LEVEL, X0, Y0, 0));
        assert_eq!(service.stats().shared, 0);
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 2, "each asked the source, as two instances always did");
    assert!(started.elapsed() < Duration::from_secs(1), "and neither waited on what cannot be reached");
    // A tile each holds is served from its own store, as ever.
    assert_eq!(a.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present").source, Source::Memory);
}

#[tokio::test]
async fn a_tile_the_source_does_not_have_is_shared_as_a_tile_is() {
    let (clock, objects, source) = (TestClock::new(), memory(), source());
    let shared: Arc<dyn SharedTiles> = Arc::new(InMemory::default());
    let a = instance(&objects, &clock, &source, Some(shared.clone()), quick());
    let b = instance(&objects, &clock, &source, Some(shared), quick());

    assert!(a.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    assert!(b.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1, "the hole was asked for once");
    assert_eq!(b.stats().shared, 1);
}

#[tokio::test]
async fn a_publication_that_is_announced_is_seen_at_once_and_one_that_is_not_waits() {
    let (clock, objects) = (TestClock::new(), memory());
    // Readers that trust a manifest for an hour.
    let trusting = StoreConfig { manifest_ttl: Duration::from_secs(3600), manifest_max_stale: Duration::ZERO, ..eager() };
    let bus = Arc::new(InMemory::default());
    let told = Arc::new(store(&objects, &clock, trusting.clone()));
    let untold = Arc::new(store(&objects, &clock, trusting));
    bus.listen(&told);
    // Both have read the zone, and found it empty.
    for reader in [&told, &untold] {
        assert_eq!(reader.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None);
    }

    // Another instance publishes a tile of it, and says so.
    let writer = store(&objects, &clock, eager()).with_announcer(bus.clone());
    writer.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    writer.flush_all().await.expect("flush");
    assert_eq!(bus.announced(), 1);

    assert_eq!(told.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(untold.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None, "still trusting what it read");
}

#[tokio::test]
async fn the_claim_is_given_back_once_the_tile_is_fetched() {
    let (clock, objects, source) = (TestClock::new(), memory(), source());
    let shared = Arc::new(InMemory::default());
    let a = instance(&objects, &clock, &source, Some(shared.clone()), quick());
    a.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert!(
        matches!(shared.claim(IMAGERY, LEVEL, X0, Y0).await, Claim::Mine(_)),
        "nothing is left claimed behind a fetch"
    );
}
