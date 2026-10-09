// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Against a real bucket: does it honour the conditional writes everything
//! else relies on, and do many writers still lose nothing there? And does a
//! zone's whole life — deltas of several months written by two instances, a
//! tile rewritten in each, a compaction pass by a third process, expiry,
//! removal — go there as it does in memory?
//!
//! Opt-in: set `TUILE_TILES_TEST_BUCKET`, with `CLOUDFLARE_ACCOUNT_ID`,
//! `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`. Everything happens under a
//! fresh `_tests/tuile-tile-server/<run>/` prefix, removed at the end.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use common::*;
use futures_util::TryStreamExt;
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::path::Path;
use object_store::prefix::PrefixStore;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use rand::Rng;

/// The raw bucket, the same scoped to this run's prefix, and that prefix.
struct Scratch {
    raw: Arc<dyn ObjectStore>,
    scoped: Arc<dyn ObjectStore>,
    prefix: String,
}

fn bucket() -> Option<Scratch> {
    let name = std::env::var("TUILE_TILES_TEST_BUCKET").ok()?;
    let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").ok()?;
    let raw = AmazonS3Builder::new()
        .with_bucket_name(name)
        .with_endpoint(format!("https://{account}.r2.cloudflarestorage.com"))
        .with_region("auto")
        .with_access_key_id(std::env::var("R2_ACCESS_KEY_ID").ok()?)
        .with_secret_access_key(std::env::var("R2_SECRET_ACCESS_KEY").ok()?)
        .with_conditional_put(S3ConditionalPut::ETagMatch)
        .build()
        .expect("bucket client");
    let raw: Arc<dyn ObjectStore> = Arc::new(raw);
    let run: u64 = rand::rng().random();
    let prefix = format!("_tests/tuile-tile-server/{run:016x}");
    let scoped: Arc<dyn ObjectStore> = Arc::new(PrefixStore::new(raw.clone(), prefix.as_str()));
    Some(Scratch { raw, scoped, prefix })
}

async fn remove_prefix(raw: &dyn ObjectStore, prefix: &str) {
    let listed: Vec<object_store::ObjectMeta> =
        raw.list(Some(&Path::from(prefix))).try_collect().await.expect("list");
    for m in listed {
        raw.delete(&m.location).await.expect("delete");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "talks to a real bucket; set TUILE_TILES_TEST_BUCKET and run with --ignored"]
async fn the_bucket_honours_conditional_writes_and_many_writers_lose_nothing() {
    let Some(Scratch { raw, scoped: objects, prefix }) = bucket() else {
        eprintln!("TUILE_TILES_TEST_BUCKET not set: skipped");
        return;
    };

    // 1. Two updates derived from the same version: exactly one is accepted.
    let key = Path::from("cas/object");
    let first = objects.put(&key, PutPayload::from_static(b"v0")).await.expect("put v0");
    let from = UpdateVersion { e_tag: first.e_tag.clone(), version: first.version.clone() };
    let opts = || PutOptions { mode: PutMode::Update(from.clone()), ..Default::default() };
    let a = objects.put_opts(&key, PutPayload::from_static(b"a"), opts()).await;
    let b = objects.put_opts(&key, PutPayload::from_static(b"b"), opts()).await;
    assert!(a.is_ok(), "the first conditional update must pass: {a:?}");
    assert!(matches!(b, Err(object_store::Error::Precondition { .. })), "the second must be refused: {b:?}");

    // 2. Exclusive creation of an existing object is refused.
    let c = objects
        .put_opts(&key, PutPayload::from_static(b"c"), PutOptions { mode: PutMode::Create, ..Default::default() })
        .await;
    assert!(matches!(c, Err(object_store::Error::AlreadyExists { .. })), "create over an object: {c:?}");

    // 3. Sixteen writers on one zone, on the real bucket.
    let clock = TestClock::new();
    let mut expected = BTreeMap::new();
    let mut tasks = Vec::new();
    for w in 0..16u32 {
        let s = Arc::new(store_on(objects.clone(), &clock, eager()));
        let tiles: Vec<(u32, u32, Bytes)> = (0..8u32)
            .map(|i| {
                let n = w * 8 + i;
                let (x, y) = in_zone(n);
                let b = body(LEVEL, x, y, w);
                expected.insert((x, y), b.clone());
                (x, y, b)
            })
            .collect();
        tasks.push(tokio::spawn(async move {
            for (x, y, b) in tiles {
                s.put(IMAGERY, LEVEL, x, y, b).await.expect("put");
            }
            s.flush_all().await.expect("flush");
        }));
    }
    for t in tasks {
        t.await.expect("writer");
    }
    let fresh = store_on(objects.clone(), &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "lost {x}/{y}");
    }
    let outcome = fresh.compact(IMAGERY, zone()).await;
    assert!(outcome.is_ok(), "{outcome:?}");
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "after compaction {x}/{y}");
    }

    remove_prefix(raw.as_ref(), &prefix).await;
}

fn archives_on_disk(dir: &std::path::Path) -> usize {
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

async fn epochs(objects: &dyn ObjectStore) -> Vec<String> {
    let m = tuile_tile_server::manifest::read(objects, &imagery().zone_prefix(zone())).await.expect("manifest").manifest;
    m.archives.iter().map(|a| a.epoch.clone()).collect()
}

/// The life of a zone, on whatever holds the objects. The months are the
/// test clock's: the archives are written today and named for the month the
/// clock says, which is all that ages them.
async fn the_life_of_a_zone(objects: Arc<dyn ObjectStore>) {
    use std::time::Duration;
    use tuile_tile_server::peers::InMemory;
    use tuile_tile_server::{Compaction, DiskCacheConfig, StoreConfig};

    let clock = TestClock::new();
    let fanout = eager().tiering.fanout as u32;
    let minute = Duration::from_secs(60);

    // 1. A month's deltas, by two instances in turn: one tile rewritten in
    // each, one tile of its own in each.
    let writers = [store_on(objects.clone(), &clock, eager()), store_on(objects.clone(), &clock, eager())];
    let delta = |v: u32| {
        let w = &writers[(v % 2) as usize];
        async move {
            let (x, y) = in_zone(v + 1);
            w.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, v)).await.expect("put");
            w.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, v)).await.expect("put");
            w.flush_all().await.expect("flush");
        }
    };
    for v in 0..fanout {
        delta(v).await;
        clock.advance(minute);
    }
    // 2. The month after: two more, too few for a run of their own.
    clock.advance(days(31));
    for v in fanout..fanout + 2 {
        delta(v).await;
        clock.advance(minute);
    }
    let last = fanout + 1;
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), fanout as usize + 2);

    // 3. A server that trusts a manifest for an hour, the zone on its disk.
    let trusting = StoreConfig { manifest_ttl: Duration::from_secs(3600), manifest_max_stale: Duration::ZERO, ..eager() };
    let dir = tempfile::tempdir().expect("dir");
    let server = Arc::new(
        store_on(objects.clone(), &clock, trusting).with_disk_cache(DiskCacheConfig::new(dir.path())).await.expect("disk"),
    );
    let bus = Arc::new(InMemory::default());
    bus.listen(&server);
    let every_tile = |s: Arc<tuile_tile_server::TileStore>, from: u32| async move {
        assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, last)), "the last written wins");
        for v in from..=last {
            let (x, y) = in_zone(v + 1);
            assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, v)), "delta {v}");
        }
    };
    every_tile(server.clone(), 0).await;
    server.settle().await;
    assert_eq!(archives_on_disk(dir.path()), fanout as usize + 2);

    // 4. A pass by another process: the past month's deltas become one,
    // never with the next month's; the server reads the zone as it now is.
    let job = store_on(objects.clone(), &clock, eager()).with_announcer(bus.clone());
    let done = job.compact_due().await.expect("compact_due");
    assert!(done.iter().any(|(_, _, c)| matches!(c, Compaction::Merged { .. })), "{done:?}");
    assert_eq!(epochs(objects.as_ref()).await, ["202609", "202610", "202610"]);
    every_tile(server.clone(), 0).await;
    server.settle().await;
    assert_eq!(archives_on_disk(dir.path()), 3, "the merged archive and the two it was not merged with");
    // What was merged is retired, not removed: a reader still on the older
    // manifest reads it.
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), fanout as usize + 3);

    // 5. The first month runs out, the second has not.
    clock.advance(days(80));
    job.compact_due().await.expect("compact_due");
    assert_eq!(epochs(objects.as_ref()).await, ["202610", "202610"]);
    every_tile(server.clone(), fanout).await;
    let (x, y) = in_zone(1);
    assert_eq!(server.get(IMAGERY, LEVEL, x, y).await.expect("get"), None, "a tile of the month that ran out");

    // 6. Past the grace, what was retired is gone from the bucket itself.
    clock.advance(Duration::from_secs(601));
    job.compact_due().await.expect("compact_due");
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), 2);
    every_tile(Arc::new(store_on(objects, &clock, eager())), fanout).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_life_of_a_zone_in_memory() {
    the_life_of_a_zone(memory()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "talks to a real bucket; set TUILE_TILES_TEST_BUCKET and run with --ignored"]
async fn the_life_of_a_zone_on_the_bucket() {
    let Some(Scratch { raw, scoped, prefix }) = bucket() else {
        eprintln!("TUILE_TILES_TEST_BUCKET not set: skipped");
        return;
    };
    eprintln!("under {prefix}/");
    the_life_of_a_zone(scoped).await;
    remove_prefix(raw.as_ref(), &prefix).await;
}
