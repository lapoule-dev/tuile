// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The service: a tile absent from the store is fetched once, however many
//! ask at the same moment, then served from the store.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use tuile_tile_server::{TileService, Upstream, UpstreamError};

struct Slow {
    calls: AtomicUsize,
}

#[async_trait]
impl Upstream for Slow {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        if x == 0 {
            return Ok(None);
        }
        Ok(Some(body(level, x, y, 0)))
    }
}

fn service(upstream: Arc<Slow>) -> TileService {
    let clock = TestClock::new();
    let store = Arc::new(store_on(memory(), &clock, eager()));
    let mut ups: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    ups.insert(IMAGERY.into(), upstream);
    TileService::new(store, ups)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_simultaneous_requests_for_a_missing_tile_make_one_upstream_call() {
    let up = Arc::new(Slow { calls: AtomicUsize::new(0) });
    let svc = Arc::new(service(up.clone()));
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let s = svc.clone();
        tasks.push(tokio::spawn(async move { s.tile(IMAGERY, LEVEL, X0, Y0).await }));
    }
    for t in tasks {
        let r = t.await.expect("task").expect("tile").expect("present");
        assert_eq!(r.bytes, body(LEVEL, X0, Y0, 0));
    }
    assert_eq!(up.calls.load(Ordering::SeqCst), 1);

    let again = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert!(again.hit, "served from the store the second time");
    assert_eq!(up.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_tile_the_source_does_not_have_is_none_and_the_etag_is_stable() {
    let up = Arc::new(Slow { calls: AtomicUsize::new(0) });
    let svc = service(up);
    assert!(svc.tile(IMAGERY, LEVEL, 0, Y0).await.expect("tile").is_none());
    let a = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    let b = svc.tile(IMAGERY, LEVEL, X0, Y0).await.expect("tile").expect("present");
    assert_eq!(a.etag, b.etag);
    assert_eq!(a.content_type, "image/jpeg");
    assert!(svc.tile("nope", 1, 0, 0).await.is_err());
    assert!(svc.tile(IMAGERY, 3, 99, 0).await.is_err(), "out of the grid");
}
