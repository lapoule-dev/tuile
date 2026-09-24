// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Serving with low latency: hot tiles and absences in memory, whole zones on
//! local disk, manifests refreshed behind the reader, and publications that
//! never duplicate what the store already holds — the newest fetch winning.

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
use tuile_tile_server::{
    DiskCacheConfig, ServiceConfig, Source, StoreConfig, TileService, TileStore, Upstream, UpstreamError,
};

/// A source that has every tile except those with `x == 0`, and counts calls.
struct Source0 {
    calls: AtomicUsize,
}

#[async_trait]
impl Upstream for Source0 {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if x == 0 {
            return Ok(None);
        }
        Ok(Some(body(level, x, y, 0)))
    }
}

/// A store over `objects` with the catalog's layers, absence siblings included.
fn cataloged(objects: Arc<dyn ObjectStore>, clock: &TestClock, cfg: StoreConfig) -> TileStore {
    TileStore::with_clock(objects, layers(&catalog()).expect("layers"), cfg, clock.clock())
}

fn service_over(store: Arc<TileStore>, up: Arc<Source0>, cfg: ServiceConfig) -> TileService {
    let mut ups: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    ups.insert(IMAGERY.into(), up);
    TileService::with_config(store, ups, cfg)
}

// ── Hot tiles ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_hot_tile_is_served_from_memory_without_touching_the_bucket() {
    let clock = TestClock::new();
    let objects = memory();
    let writer = store_on(objects.clone(), &clock, eager());
    writer.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    writer.flush_all().await.expect("flush");

    let counted = Counting::over(objects);
    let store = Arc::new(store_on(counted.clone(), &clock, eager()));
    let up = Arc::new(Source0 { calls: AtomicUsize::new(0) });
    let svc = service_over(store, up.clone(), ServiceConfig::default());

    let first = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!(first.source, Source::Store);
    let reads = counted.gets();
    assert!(reads > 0);
    let second = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!(second.source, Source::Memory);
    assert_eq!(second.bytes, body(LEVEL, X0, Y0, 1));
    assert_eq!(second.etag, first.etag, "the validator is the tile's, wherever it is served from");
    assert_eq!(counted.gets(), reads, "no bucket read for a hot tile");
    assert_eq!(up.calls.load(Ordering::SeqCst), 0);
    assert_eq!(svc.stats().memory, 1);
}

#[tokio::test]
async fn a_hot_tile_past_its_ttl_is_read_from_the_store_again() {
    let clock = TestClock::new();
    let store = Arc::new(store_on(memory(), &clock, eager()));
    store.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    let up = Arc::new(Source0 { calls: AtomicUsize::new(0) });
    let svc = service_over(store, up, ServiceConfig { hot_ttl: Duration::ZERO, ..ServiceConfig::default() });
    svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile");
    let again = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!(again.source, Source::Store);
}

#[tokio::test]
async fn the_hot_budget_is_kept() {
    let clock = TestClock::new();
    let store = Arc::new(store_on(memory(), &clock, eager()));
    let up = Arc::new(Source0 { calls: AtomicUsize::new(0) });
    // Room for either tile, not both: the second evicts the first.
    let (a, b) = (in_zone(1), in_zone(2));
    let weight = |(x, y): (u32, u32)| body(LEVEL, x, y, 0).len() as u64 + 64;
    let budget = weight(a) + weight(b) - 1;
    let svc = service_over(store, up, ServiceConfig { hot_bytes: budget, ..ServiceConfig::default() });
    svc.tile(IMAGERY, LEVEL, a.0, a.1).await.expect("tile");
    svc.tile(IMAGERY, LEVEL, b.0, b.1).await.expect("tile");
    let again = svc.tile(IMAGERY, LEVEL, a.0, a.1).await.expect("tile").expect("present");
    assert_eq!(again.source, Source::Store, "evicted from memory, still in the store");
}

#[tokio::test]
async fn if_none_match_is_matched_strongly_and_weakly() {
    let clock = TestClock::new();
    let store = Arc::new(store_on(memory(), &clock, eager()));
    let up = Arc::new(Source0 { calls: AtomicUsize::new(0) });
    let svc = service_over(store, up, ServiceConfig::default());
    let t = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert!(t.matches(&t.etag));
    assert!(t.matches(&format!("\"other\", W/{}", t.etag)));
    assert!(!t.matches("\"other\""));
}

// ── Absences ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_source_absence_is_asked_once_then_remembered_in_memory_and_in_the_store() {
    let clock = TestClock::new();
    let objects = memory();
    let store = Arc::new(cataloged(objects.clone(), &clock, eager()));
    let up = Arc::new(Source0 { calls: AtomicUsize::new(0) });
    let svc = service_over(store.clone(), up.clone(), ServiceConfig::default());

    assert!(svc.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    assert!(svc.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    assert_eq!(up.calls.load(Ordering::SeqCst), 1, "remembered in memory");
    store.flush_all().await.expect("flush");

    // Another instance, cold: the absence comes from the store.
    let up2 = Arc::new(Source0 { calls: AtomicUsize::new(0) });
    let other = service_over(Arc::new(cataloged(objects, &clock, eager())), up2.clone(), ServiceConfig::default());
    assert!(other.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    assert_eq!(up2.calls.load(Ordering::SeqCst), 0, "remembered in the store");
    assert_eq!(other.stats().absent, 1);
}

// ── Manifests refreshed behind the reader ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zone_s_first_readers_share_one_manifest_read() {
    let clock = TestClock::new();
    let counted = Counting::over(memory());
    let store = Arc::new(store_on(counted.clone(), &clock, StoreConfig::default()));
    let mut tasks = Vec::new();
    for i in 0..20 {
        let s = store.clone();
        let (x, y) = in_zone(i);
        tasks.push(tokio::spawn(async move { s.get(IMAGERY, LEVEL, x, y).await }));
    }
    for t in tasks {
        t.await.expect("task").expect("get");
    }
    assert_eq!(counted.manifest_gets(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_manifest_is_served_while_a_fresh_one_is_read_behind() {
    let clock = TestClock::new();
    let objects = memory();
    let counted = Counting::over(objects.clone());
    let cfg = StoreConfig {
        manifest_ttl: Duration::ZERO,
        manifest_max_stale: Duration::from_secs(3600),
        ..eager()
    };
    let reader = store_on(counted.clone(), &clock, cfg);
    assert_eq!(reader.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None);
    let blocking_reads = counted.manifest_gets();

    // Another instance publishes the tile.
    let writer = store_on(objects, &clock, eager());
    writer.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    writer.flush_all().await.expect("flush");

    // The reader answers at once from the list it has, and refreshes it.
    assert_eq!(reader.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None, "stale, not waited on");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if reader.get(IMAGERY, LEVEL, X0, Y0).await.expect("get").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the background refresh never landed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(counted.manifest_gets() > blocking_reads, "refreshed in the background");
}

// ── Whole zones on local disk ────────────────────────────────────────────

/// Writes `n` tiles of the test zone and of the zone east of it, and
/// publishes them as one archive per zone.
async fn two_zones(objects: Arc<dyn ObjectStore>, clock: &TestClock, n: u32) {
    let w = store_on(objects, clock, eager());
    for i in 0..n {
        let (x, y) = in_zone(i);
        w.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1)).await.expect("put");
        w.put(IMAGERY, LEVEL, x + ZONE_SIDE, y, body(LEVEL, x + ZONE_SIDE, y, 1)).await.expect("put");
    }
    w.flush_all().await.expect("flush");
}

async fn with_disk(objects: Arc<dyn ObjectStore>, clock: &TestClock, disk: DiskCacheConfig) -> TileStore {
    store_on(objects, clock, eager()).with_disk_cache(disk).await.expect("disk cache")
}

#[tokio::test]
async fn a_zone_s_second_tile_is_read_from_local_disk() {
    let clock = TestClock::new();
    let objects = memory();
    two_zones(objects.clone(), &clock, 16).await;
    let dir = tempfile::tempdir().expect("dir");
    let counted = Counting::over(objects);
    let s = with_disk(counted.clone(), &clock, DiskCacheConfig::new(dir.path())).await;

    let (x, y) = in_zone(0);
    assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 1)));
    assert_eq!(s.stats().remote, 1, "the first read goes to the bucket");
    s.settle().await;
    assert!(s.disk_bytes() > 0, "the zone was copied");

    let archive_reads = || counted.gets() - counted.manifest_gets();
    let reads = archive_reads();
    for i in 1..16 {
        let (x, y) = in_zone(i);
        assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 1)));
    }
    assert_eq!(s.stats().disk, 15);
    assert_eq!(s.stats().remote, 1);
    assert_eq!(archive_reads(), reads, "no bucket read of an archive once it is local");
    // The zone east was never touched, so never copied.
    let (x, y) = in_zone(0);
    s.get(IMAGERY, LEVEL, x + ZONE_SIDE, y).await.expect("get");
    assert_eq!(s.stats().remote, 2);
}

#[tokio::test]
async fn the_disk_budget_evicts_the_least_recently_read_zone() {
    let clock = TestClock::new();
    let objects = memory();
    two_zones(objects.clone(), &clock, 16).await;
    let dir = tempfile::tempdir().expect("dir");
    // Measure one zone's archive, then allow a little more than one.
    let probe_dir = tempfile::tempdir().expect("dir");
    let probe = with_disk(objects.clone(), &clock, DiskCacheConfig::new(probe_dir.path())).await;
    let (x, y) = in_zone(0);
    probe.get(IMAGERY, LEVEL, x, y).await.expect("get");
    probe.settle().await;
    let one = probe.disk_bytes();
    assert!(one > 0);

    let s = with_disk(objects, &clock, DiskCacheConfig { budget_bytes: one + one / 2, ..DiskCacheConfig::new(dir.path()) }).await;
    s.get(IMAGERY, LEVEL, x, y).await.expect("get");
    s.settle().await;
    s.get(IMAGERY, LEVEL, x + ZONE_SIDE, y).await.expect("get");
    s.settle().await;
    assert!(s.disk_bytes() <= one + one / 2, "within budget");
    let files = count_files(dir.path());
    assert_eq!(files, 1, "the first zone's archive was removed from disk");

    let before = s.stats();
    s.get(IMAGERY, LEVEL, x + ZONE_SIDE + 1, y).await.expect("get");
    assert_eq!(s.stats().disk, before.disk + 1, "the zone read last is local");
    s.get(IMAGERY, LEVEL, x + 1, y).await.expect("get");
    assert_eq!(s.stats().remote, before.remote + 1, "the evicted zone is remote again");
}

fn count_files(dir: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).expect("read_dir") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "pmtiles") {
                n += 1;
            }
        }
    }
    n
}

#[tokio::test]
async fn a_restarted_store_starts_warm_from_its_disk() {
    let clock = TestClock::new();
    let objects = memory();
    two_zones(objects.clone(), &clock, 4).await;
    let dir = tempfile::tempdir().expect("dir");
    let (x, y) = in_zone(0);
    {
        let s = with_disk(objects.clone(), &clock, DiskCacheConfig::new(dir.path())).await;
        s.get(IMAGERY, LEVEL, x, y).await.expect("get");
        s.settle().await;
    }
    let s = with_disk(objects, &clock, DiskCacheConfig::new(dir.path())).await;
    assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 1)));
    assert_eq!(s.stats().disk, 1);
    assert_eq!(s.stats().remote, 0);
}

#[tokio::test]
async fn a_newer_archive_still_wins_over_a_local_copy() {
    let clock = TestClock::new();
    let objects = memory();
    two_zones(objects.clone(), &clock, 4).await;
    let dir = tempfile::tempdir().expect("dir");
    let s = with_disk(objects.clone(), &clock, DiskCacheConfig::new(dir.path())).await;
    let (x, y) = in_zone(0);
    s.get(IMAGERY, LEVEL, x, y).await.expect("get");
    s.settle().await;

    clock.advance(Duration::from_secs(10));
    let w = store_on(objects, &clock, eager());
    w.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 2)).await.expect("put");
    w.flush_all().await.expect("flush");
    assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 2)));
}

#[tokio::test]
async fn neighbour_zones_are_copied_when_asked() {
    let clock = TestClock::new();
    let objects = memory();
    two_zones(objects.clone(), &clock, 4).await;
    let (x, y) = in_zone(0);

    for prefetch in [false, true] {
        let dir = tempfile::tempdir().expect("dir");
        let cfg = DiskCacheConfig { prefetch_neighbours: prefetch, ..DiskCacheConfig::new(dir.path()) };
        let s = with_disk(objects.clone(), &clock, cfg).await;
        s.get(IMAGERY, LEVEL, x, y).await.expect("get");
        s.settle().await;
        // The prefetch reads the neighbour's manifest before scheduling its
        // copy: wait for that too.
        let deadline = Instant::now() + Duration::from_secs(5);
        while prefetch && count_files(dir.path()) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
            s.settle().await;
        }
        s.get(IMAGERY, LEVEL, x + ZONE_SIDE, y).await.expect("get");
        let st = s.stats();
        if prefetch {
            assert_eq!((st.disk, st.remote), (1, 1), "the zone east was copied before it was read");
        } else {
            assert_eq!((st.disk, st.remote), (0, 2), "without prefetch the zone east is remote");
        }
    }
}

// ── No duplicate publication, and the newest fetch wins ──────────────────

async fn archive_count(objects: &dyn ObjectStore) -> usize {
    archives(objects, IMAGERY).await.len()
}

#[tokio::test]
async fn a_tile_the_store_already_holds_as_is_is_not_published_again() {
    let clock = TestClock::new();
    let objects = memory();
    let a = store_on(objects.clone(), &clock, eager());
    let b = store_on(objects.clone(), &clock, eager());
    a.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    a.flush_all().await.expect("flush");
    let before = archive_count(objects.as_ref()).await;

    // b fetched the very same bytes (its read raced a's publication).
    b.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    b.flush_all().await.expect("flush");
    assert_eq!(archive_count(objects.as_ref()).await, before, "no delta for a duplicate");
    assert_eq!(b.stats().dedup_identical, 1);
}

#[tokio::test]
async fn only_the_new_tiles_of_a_mixed_buffer_are_published() {
    let clock = TestClock::new();
    let objects = memory();
    let a = store_on(objects.clone(), &clock, eager());
    a.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    a.flush_all().await.expect("flush");

    let b = store_on(objects.clone(), &clock, eager());
    b.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    let (x, y) = in_zone(1);
    b.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1)).await.expect("put");
    b.flush_all().await.expect("flush");

    let keys = archives(objects.as_ref(), IMAGERY).await;
    assert_eq!(keys.len(), 2);
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, 1)));
    assert_eq!(b.stats().dedup_identical, 1);
}

#[tokio::test]
async fn a_stored_copy_fetched_after_ours_wins_and_ours_is_dropped() {
    let clock = TestClock::new();
    let objects = memory();
    let t0 = clock.now();
    let late = store_on(objects.clone(), &clock, eager());
    let early = store_on(objects.clone(), &clock, eager());

    // `early` fetched v1 at t0 but publishes last; `late` fetched v2 ten
    // seconds after and published first.
    early.put_fetched(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1), t0).await.expect("put");
    late.put_fetched(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 2), t0 + Duration::from_secs(10)).await.expect("put");
    late.flush_all().await.expect("flush");
    early.flush_all().await.expect("flush");

    assert_eq!(early.stats().dedup_newer, 1);
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 2)), "the newest fetch wins");
}

#[tokio::test]
async fn a_stored_copy_fetched_before_ours_is_superseded() {
    let clock = TestClock::new();
    let objects = memory();
    let t0 = clock.now();
    let a = store_on(objects.clone(), &clock, eager());
    let b = store_on(objects.clone(), &clock, eager());
    a.put_fetched(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1), t0).await.expect("put");
    a.flush_all().await.expect("flush");
    b.put_fetched(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 2), t0 + Duration::from_secs(10)).await.expect("put");
    b.flush_all().await.expect("flush");

    assert_eq!((b.stats().dedup_identical, b.stats().dedup_newer), (0, 0));
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 2)));
}

#[tokio::test]
async fn a_compaction_keeps_the_fetch_times_of_what_it_merges() {
    let clock = TestClock::new();
    let objects = memory();
    let t0 = clock.now();
    let a = store_on(objects.clone(), &clock, eager());
    a.put_fetched(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1), t0 + Duration::from_secs(20)).await.expect("put");
    a.flush_all().await.expect("flush");
    let (x, y) = in_zone(1);
    a.put_fetched(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1), t0 + Duration::from_secs(30)).await.expect("put");
    a.flush_all().await.expect("flush");
    assert!(matches!(a.compact(IMAGERY, zone()).await.expect("compact"), tuile_tile_server::Compaction::Merged { .. }));

    // A copy fetched before everything the merged archive holds is dropped.
    let b = store_on(objects.clone(), &clock, eager());
    b.put_fetched(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 9), t0 + Duration::from_secs(10)).await.expect("put");
    b.flush_all().await.expect("flush");
    assert_eq!(b.stats().dedup_newer, 1);
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
}
