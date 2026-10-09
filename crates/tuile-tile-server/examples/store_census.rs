// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Counts what a tile store holds, zone by zone: tiles a reader can get,
//! copies kept, and tiles held by more than one archive.
//!
//! A tile in two archives is not an error — the newest wins — but it is
//! space, and the measure of how often two writers stored the same thing, or
//! of what a compaction has yet to merge. Reads only: nothing is written.
//!
//! ```text
//! TUILE_CENSUS_BUCKET=… CLOUDFLARE_ACCOUNT_ID=… R2_ACCESS_KEY_ID=… R2_SECRET_ACCESS_KEY=… \
//!   cargo run --release -p tuile-tile-server --example store_census [-- <layer>]
//! ```
//!
//! `TUILE_CENSUS_PREFIX` counts a store kept under a prefix of the bucket.

use std::sync::Arc;

use object_store::aws::AmazonS3Builder;
use object_store::prefix::PrefixStore;
use object_store::ObjectStore;
use tuile_tile_server::{Census, TileStore};

fn bucket() -> Arc<dyn ObjectStore> {
    let var = |n: &str| {
        std::env::var(n).unwrap_or_else(|_| {
            eprintln!("{n} is not set");
            std::process::exit(2)
        })
    };
    let s3 = AmazonS3Builder::new()
        .with_bucket_name(var("TUILE_CENSUS_BUCKET"))
        .with_endpoint(format!("https://{}.r2.cloudflarestorage.com", var("CLOUDFLARE_ACCOUNT_ID")))
        .with_region("auto")
        .with_access_key_id(var("R2_ACCESS_KEY_ID"))
        .with_secret_access_key(var("R2_SECRET_ACCESS_KEY"))
        .build()
        .expect("bucket client");
    let s3: Arc<dyn ObjectStore> = Arc::new(s3);
    match std::env::var("TUILE_CENSUS_PREFIX").ok().filter(|p| !p.is_empty()) {
        Some(prefix) => Arc::new(PrefixStore::new(s3, prefix)),
        None => s3,
    }
}

#[tokio::main]
async fn main() {
    let only = std::env::args().nth(1);
    let store = TileStore::open(bucket(), Default::default()).await.expect("opening the store");
    let mut zones = store.zones().await.expect("listing the zones");
    zones.sort();
    let mut by_layer: std::collections::BTreeMap<String, (usize, Census)> = std::collections::BTreeMap::new();
    let mut worst: Vec<(u64, String, Census)> = Vec::new();
    for (layer, zone) in zones {
        if only.as_deref().is_some_and(|l| l != layer) {
            continue;
        }
        let c = store.census(&layer, zone).await.expect("counting a zone");
        let (n, total) = by_layer.entry(layer.clone()).or_default();
        *n += 1;
        total.archives += c.archives;
        total.copies += c.copies;
        total.tiles += c.tiles;
        total.repeated += c.repeated;
        if c.repeated > 0 {
            worst.push((c.repeated, format!("{layer} {zone:?}"), c));
        }
    }
    println!("{:<24} {:>7} {:>9} {:>11} {:>11} {:>10} {:>7}", "layer", "zones", "archives", "tiles", "copies", "repeated", "share");
    for (layer, (n, c)) in &by_layer {
        let share = if c.tiles == 0 { 0.0 } else { 100.0 * c.repeated as f64 / c.tiles as f64 };
        println!("{layer:<24} {n:>7} {:>9} {:>11} {:>11} {:>10} {share:>6.1}%", c.archives, c.tiles, c.copies, c.repeated);
    }
    worst.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, zone, c) in worst.iter().take(10) {
        println!("  {zone}: {} of {} tiles held more than once, in {} archives", c.repeated, c.tiles, c.archives);
    }
}
