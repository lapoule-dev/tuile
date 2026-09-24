// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Real-time writing and many writers: several stores on one bucket, as
//! several replicas of a server would be, writing, flushing and compacting the
//! same zone at the same time. Nothing written may be lost or come back wrong.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use rand::{Rng, SeedableRng};
use tuile_tile_server::store::Fault;
use tuile_tile_server::{Compaction, TileStore};

#[tokio::test]
async fn another_instance_sees_a_published_tile_within_the_manifest_ttl() {
    let clock = TestClock::new();
    let objects = memory();
    let writer = store_on(objects.clone(), &clock, eager());
    let mut cfg = eager();
    cfg.manifest_ttl = Duration::from_millis(50);
    let reader = store_on(objects, &clock, cfg);

    assert_eq!(reader.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None);
    writer.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    writer.flush_all().await.expect("flush");

    let published = std::time::Instant::now();
    loop {
        if reader.get(IMAGERY, LEVEL, X0, Y0).await.expect("get").is_some() {
            break;
        }
        assert!(published.elapsed() < Duration::from_millis(500), "never became visible");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(published.elapsed() <= Duration::from_millis(200), "visible later than the TTL allows");
}

async fn write_all(store: Arc<TileStore>, tiles: Vec<(u32, u32, Bytes)>, flush_every: usize) {
    for (n, (x, y, b)) in tiles.into_iter().enumerate() {
        store.put(IMAGERY, LEVEL, x, y, b).await.expect("put");
        if n % flush_every == flush_every - 1 {
            store.flush_all().await.expect("flush");
        }
    }
    store.flush_all().await.expect("final flush");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixteen_writers_on_one_zone_lose_nothing() {
    let clock = TestClock::new();
    let objects = memory();
    let mut tasks = Vec::new();
    let mut expected = BTreeMap::new();
    const WRITERS: u32 = 16;
    // Together they fill the zone exactly, each its own tiles.
    const EACH: u32 = ZONE_SIDE * ZONE_SIDE / WRITERS;
    for w in 0..WRITERS {
        let s = Arc::new(store_on(objects.clone(), &clock, eager()));
        let tiles: Vec<_> = (0..EACH)
            .map(|i| {
                let (x, y) = in_zone(w * EACH + i);
                let b = body(LEVEL, x, y, w);
                expected.insert((x, y), b.clone());
                (x, y, b)
            })
            .collect();
        tasks.push(tokio::spawn(write_all(s, tiles, 4)));
    }
    for t in tasks {
        t.await.expect("writer");
    }
    let fresh = store_on(objects, &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "lost {x}/{y}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn overlapping_writers_leave_one_of_the_written_versions_for_every_tile() {
    let clock = TestClock::new();
    let objects = memory();
    let mut tasks = Vec::new();
    for w in 0..8u32 {
        let s = Arc::new(store_on(objects.clone(), &clock, eager()));
        // Every writer writes the same 32 tiles, each with its own version.
        let tiles: Vec<_> = (0..32u32).map(in_zone).map(|(x, y)| (x, y, body(LEVEL, x, y, w))).collect();
        tasks.push(tokio::spawn(write_all(s, tiles, 8)));
    }
    for t in tasks {
        t.await.expect("writer");
    }
    let fresh = store_on(objects, &clock, eager());
    for i in 0..32u32 {
        let (x, y) = in_zone(i);
        let got = fresh.get(IMAGERY, LEVEL, x, y).await.expect("get").expect("present");
        assert!((0..8).any(|w| got == body(LEVEL, x, y, w)), "{x}/{y} holds bytes nobody wrote");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writers_during_a_compaction_lose_nothing() {
    let clock = TestClock::new();
    let objects = memory();
    let seed = store_on(objects.clone(), &clock, eager());
    let mut expected = BTreeMap::new();
    for i in 0..64u32 {
        let (x, y) = in_zone(i);
        seed.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 0)).await.expect("put");
        expected.insert((x, y), body(LEVEL, x, y, 0));
        if i % 8 == 7 {
            seed.flush_all().await.expect("flush");
        }
    }

    let compactor = Arc::new(store_on(objects.clone(), &clock, eager()));
    let c = compactor.clone();
    let compacting = tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for _ in 0..5 {
            outcomes.push(c.compact(IMAGERY, zone()).await.expect("compact"));
        }
        outcomes
    });
    let mut writers = Vec::new();
    for w in 1..5u32 {
        let s = Arc::new(store_on(objects.clone(), &clock, eager()));
        let tiles: Vec<_> = (0..64u32)
            .filter(|i| i % 4 == w - 1)
            .map(|i| {
                let (x, y) = in_zone(i);
                let b = body(LEVEL, x, y, w);
                expected.insert((x, y), b.clone());
                (x, y, b)
            })
            .collect();
        writers.push(tokio::spawn(write_all(s, tiles, 2)));
    }
    for t in writers {
        t.await.expect("writer");
    }
    let outcomes = compacting.await.expect("compactor");
    assert!(outcomes.iter().any(|o| matches!(o, Compaction::Merged { .. })), "{outcomes:?}");

    let fresh = store_on(objects, &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "{x}/{y}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_simultaneous_compactions_at_most_one_publishes() {
    let clock = TestClock::new();
    let objects = memory();
    let seed = store_on(objects.clone(), &clock, eager());
    for i in 0..6u32 {
        let (x, y) = in_zone(i);
        seed.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, i)).await.expect("put");
        seed.flush_all().await.expect("flush");
    }
    let a = Arc::new(store_on(objects.clone(), &clock, eager()));
    let b = Arc::new(store_on(objects.clone(), &clock, eager()));
    let (ra, rb) = tokio::join!(a.compact(IMAGERY, zone()), b.compact(IMAGERY, zone()));
    let outcomes = [ra.expect("a"), rb.expect("b")];
    let merged = outcomes.iter().filter(|o| matches!(o, Compaction::Merged { merged: 6, .. })).count();
    assert_eq!(merged, 1, "{outcomes:?}");

    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    assert_eq!(m.archives.len(), 1);
    // The loser removed its upload: one live archive and six retired ones.
    assert_eq!(archives(objects.as_ref(), &common::imagery().zone_prefix(zone())).await.len(), 7);
    let fresh = store_on(objects, &clock, eager());
    for i in 0..6u32 {
        let (x, y) = in_zone(i);
        assert_eq!(fresh.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(body(LEVEL, x, y, i)));
    }
}

#[tokio::test]
async fn a_crash_between_upload_and_publication_leaves_a_valid_zone_and_an_orphan_that_is_cleaned() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.flush_all().await.expect("flush");

    s.inject_fault(Fault::CrashAfterUpload);
    s.put(IMAGERY, LEVEL, 8353, Y0, body(LEVEL, 8353, Y0, 1)).await.expect("put");
    assert!(s.flush_all().await.is_err(), "the injected crash must surface");

    let prefix = common::imagery().zone_prefix(zone());
    assert_eq!(archives(objects.as_ref(), &prefix).await.len(), 2, "the orphan was uploaded");
    let fresh = store_on(objects.clone(), &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(fresh.get(IMAGERY, LEVEL, 8353, Y0).await.expect("get"), None, "never published, never seen");

    // Too young to be told from an upload still in flight: kept.
    assert_eq!(fresh.cleanup(&common::imagery(), zone()).await.expect("cleanup"), 0);
    assert_eq!(archives(objects.as_ref(), &prefix).await.len(), 2);
    // The in-memory store dates objects by the wall clock: move the store's
    // clock well past the real upload time plus the orphan grace.
    let real = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_secs();
    let past_grace = real + 2 * tuile_tile_server::store::DEFAULT_ORPHAN_GRACE.as_secs();
    clock.0.store(past_grace, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(fresh.cleanup(&common::imagery(), zone()).await.expect("cleanup"), 1);
    assert_eq!(archives(objects.as_ref(), &prefix).await.len(), 1);
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
}

#[tokio::test]
async fn a_failed_publication_puts_the_tiles_back_and_a_later_flush_delivers_them() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.inject_fault(Fault::FailAfterUpload);
    assert!(s.flush_all().await.is_err());
    // Still readable by the writer, from memory.
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(s.flush_all().await.expect("flush"), 1);
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_during_a_failing_publication_survives_the_put_back() {
    let clock = TestClock::new();
    let objects = memory();
    let s = Arc::new(store_on(objects.clone(), &clock, eager()));
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    let gate = s.pause_next_publication();
    s.inject_fault(Fault::FailAfterUpload);
    let flushing = {
        let s = s.clone();
        tokio::spawn(async move { s.flush_all().await })
    };
    gate.reached.notified().await;
    // Frozen and uploaded, not yet published: still readable.
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
    // A newer write lands while the publication is under way…
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 2)).await.expect("put");
    gate.go.notify_one();
    assert!(flushing.await.expect("task").is_err());
    // …and the put-back of the failed publication does not undo it.
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 2)));
    s.flush_all().await.expect("flush");
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 2)));
}

#[tokio::test]
async fn retired_archives_stay_through_their_grace_then_go() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    for i in 0..3u32 {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, i)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    s.compact(IMAGERY, zone()).await.expect("compact");
    let prefix = common::imagery().zone_prefix(zone());
    assert_eq!(archives(objects.as_ref(), &prefix).await.len(), 4, "inputs kept for older readers");
    clock.advance(Duration::from_secs(601));
    assert_eq!(s.cleanup(&common::imagery(), zone()).await.expect("cleanup"), 3);
    assert_eq!(archives(objects.as_ref(), &prefix).await.len(), 1);
}

/// Random writes, flushes, compactions and crashes from several stores, with a
/// model of what must be readable checked after every step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_sequences_of_everything_always_match_the_model() {
    for seed in 0..8u64 {
        let clock = TestClock::new();
        let objects = memory();
        let stores: Vec<Arc<TileStore>> =
            (0..3).map(|_| Arc::new(store_on(objects.clone(), &clock, eager()))).collect();
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        // What a fresh reader must see once everything is flushed.
        let mut published: BTreeMap<(u32, u32), Bytes> = BTreeMap::new();
        // Written to a store's buffer, not yet flushed: (store, tile) → bytes.
        let mut buffered: BTreeMap<(usize, u32, u32), Bytes> = BTreeMap::new();

        for step in 0..120u32 {
            let who = rng.random_range(0..stores.len());
            let s = &stores[who];
            match rng.random_range(0..10) {
                0..=5 => {
                    let (x, y) = in_zone(rng.random_range(0..48));
                    let b = body(LEVEL, x, y, step);
                    s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
                    buffered.insert((who, x, y), b);
                    // A store reads its own writes at once.
                    assert_eq!(s.get(IMAGERY, LEVEL, x, y).await.expect("get"), Some(buffered[&(who, x, y)].clone()));
                }
                6 | 7 => {
                    s.flush_all().await.expect("flush");
                    let mine: Vec<_> = buffered.keys().filter(|k| k.0 == who).cloned().collect();
                    for k in mine {
                        if let Some(b) = buffered.remove(&k) {
                            published.insert((k.1, k.2), b);
                        }
                    }
                }
                8 => {
                    s.compact(IMAGERY, zone()).await.expect("compact");
                }
                _ => {
                    // A crash: this store's unflushed tiles are gone.
                    s.inject_fault(Fault::CrashAfterUpload);
                    let had = buffered.keys().any(|k| k.0 == who);
                    let r = s.flush_all().await;
                    assert_eq!(r.is_err(), had, "seed {seed} step {step}");
                    buffered.retain(|k, _| k.0 != who);
                    s.inject_fault(Fault::None);
                }
            }
            // Flushes from different stores race only in time, never in this
            // loop, so the latest flush of a tile is the one that must win.
            let fresh = store_on(objects.clone(), &clock, eager());
            for ((x, y), b) in &published {
                let got = fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get");
                assert_eq!(got.as_ref(), Some(b), "seed {seed} step {step}: {x}/{y}");
            }
        }
    }
}
