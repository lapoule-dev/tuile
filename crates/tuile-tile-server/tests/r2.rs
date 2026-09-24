// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Against a real bucket: does it honour the conditional writes everything
//! else relies on, and do many writers still lose nothing there?
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
