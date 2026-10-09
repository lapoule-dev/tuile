// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A tile server's latency on a flight, against a real bucket, read-only.
//!
//! Replays what a globe viewer asks for along a flight over the Pyrenees —
//! the coarse pyramid first, then at every camera position a square window of
//! imagery tiles at each level from z10 to z17 — through [`TileService`], as
//! an HTTP adapter would, `CONCURRENCY` requests at a time (a browser's
//! per-host budget). Each scenario prints p50/p95/p99/max latency, overall
//! and split into hits (served without the source) and misses, the upstream
//! calls, and where tiles came from.
//!
//! Scenario S replays only tiles the bucket holds: the store's own latency.
//! Scenarios A and B are what a viewer asks for, holes included.
//!
//! The upstream is simulated (a fixed delay plus jitter): the benchmark
//! measures the store, and must not spend the source's quota. The bucket is
//! wrapped **read-only** and nothing is ever flushed, so nothing is written.
//!
//! ```text
//! TUILE_BENCH_BUCKET=stl-tiles CLOUDFLARE_ACCOUNT_ID=… R2_ACCESS_KEY_ID=… \
//!   R2_SECRET_ACCESS_KEY=… cargo run --release -p tuile-tile-server --example flight_bench
//! ```

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::ObjectStore;
use tuile_tile_server::{StoreConfig, TileService, TileStore, Upstream, UpstreamError};

/// The imagery layer of the bucket.
const LAYER: &str = "asset-2";
/// Requests in flight at once: a browser's budget for one host.
const CONCURRENCY: usize = 18;
/// The simulated source: this long, plus up to `UPSTREAM_JITTER_MS`.
const UPSTREAM_MS: u64 = 120;
const UPSTREAM_JITTER_MS: u64 = 80;
/// Camera positions along the flight.
const STEPS: usize = 60;
/// Pau to the Vignemale.
const FROM: (f64, f64) = (-0.37, 43.30);
const TO: (f64, f64) = (-0.14, 42.78);
/// The parallel flight, 1.5 km east.
const EAST: f64 = 0.02;

type Tile = (u8, u32, u32);

/// The tile of a point at a level, Web Mercator.
fn tile_of(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n) as u32;
    let lr = lat.to_radians();
    let y = ((1.0 - (lr.tan() + 1.0 / lr.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
    (x, y)
}

/// What a viewer asks for, in order, along a straight flight from `a` to `b`,
/// with a `(2r + 1)²` window at each level.
fn flight(a: (f64, f64), b: (f64, f64), r: i64) -> Vec<Tile> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |t: Tile, out: &mut Vec<Tile>| {
        if seen.insert(t) {
            out.push(t);
        }
    };
    // The coarse pyramid over the start, as a globe loads it.
    for z in 0..10u8 {
        let (cx, cy) = tile_of(a.0, a.1, z);
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                let n = 1i64 << z;
                let (x, y) = (i64::from(cx) + dx, i64::from(cy) + dy);
                if (0..n).contains(&x) && (0..n).contains(&y) {
                    push((z, x as u32, y as u32), &mut out);
                }
            }
        }
    }
    for s in 0..STEPS {
        let f = s as f64 / (STEPS - 1) as f64;
        let (lon, lat) = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
        for z in 10..=17u8 {
            let (cx, cy) = tile_of(lon, lat, z);
            for dy in -r..=r {
                for dx in -r..=r {
                    push((z, (i64::from(cx) + dx) as u32, (i64::from(cy) + dy) as u32), &mut out);
                }
            }
        }
    }
    out
}

/// A slow source that has every tile.
struct Simulated {
    calls: AtomicU64,
}

#[async_trait]
impl Upstream for Simulated {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let jitter = (u64::from(x) * 31 + u64::from(y) * 17 + u64::from(level)) % UPSTREAM_JITTER_MS;
        tokio::time::sleep(Duration::from_millis(UPSTREAM_MS + jitter)).await;
        Ok(Some(Bytes::from(format!("simulated {level}/{x}/{y}"))))
    }
}

/// The bucket, refusing every write.
#[derive(Debug)]
struct ReadOnly(Arc<dyn ObjectStore>);

impl std::fmt::Display for ReadOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReadOnly({})", self.0)
    }
}

fn refused() -> object_store::Error {
    object_store::Error::NotSupported { source: "the benchmark is read-only".into() }
}

type OsStream<T> = futures_util::stream::BoxStream<'static, object_store::Result<T>>;

#[async_trait]
impl ObjectStore for ReadOnly {
    async fn put_opts(
        &self,
        _: &Path,
        _: object_store::PutPayload,
        _: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        Err(refused())
    }
    async fn put_multipart_opts(
        &self,
        _: &Path,
        _: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        Err(refused())
    }
    async fn get_opts(&self, location: &Path, options: object_store::GetOptions) -> object_store::Result<object_store::GetResult> {
        self.0.get_opts(location, options).await
    }
    async fn get_ranges(&self, location: &Path, ranges: &[std::ops::Range<u64>]) -> object_store::Result<Vec<Bytes>> {
        self.0.get_ranges(location, ranges).await
    }
    fn delete_stream(&self, locations: OsStream<Path>) -> OsStream<Path> {
        locations.map(|_| Err(refused())).boxed()
    }
    fn list(&self, prefix: Option<&Path>) -> OsStream<object_store::ObjectMeta> {
        self.0.list(prefix)
    }
    fn list_with_offset(&self, prefix: Option<&Path>, offset: &Path) -> OsStream<object_store::ObjectMeta> {
        self.0.list_with_offset(prefix, offset)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, _: &Path, _: &Path, _: object_store::CopyOptions) -> object_store::Result<()> {
        Err(refused())
    }
}

fn bucket() -> Arc<dyn ObjectStore> {
    let var = |n: &str| {
        std::env::var(n).unwrap_or_else(|_| {
            eprintln!("{n} is not set");
            std::process::exit(2)
        })
    };
    let s3 = AmazonS3Builder::new()
        .with_bucket_name(var("TUILE_BENCH_BUCKET"))
        .with_endpoint(format!("https://{}.r2.cloudflarestorage.com", var("CLOUDFLARE_ACCOUNT_ID")))
        .with_region("auto")
        .with_access_key_id(var("R2_ACCESS_KEY_ID"))
        .with_secret_access_key(var("R2_SECRET_ACCESS_KEY"))
        .build()
        .expect("bucket client");
    Arc::new(ReadOnly(Arc::new(s3)))
}

/// Never flushes: everything the simulated source delivers stays in memory.
fn read_only_config() -> StoreConfig {
    StoreConfig {
        flush_bytes: usize::MAX,
        max_buffered_bytes: usize::MAX,
        flush_age: Duration::MAX,
        ..StoreConfig::default()
    }
}

// BEGIN-SERVICE (the one part that differs before and after)
async fn service(objects: Arc<dyn ObjectStore>, disk: &std::path::Path, up: Arc<Simulated>) -> TileService {
    let store = TileStore::open(objects, read_only_config()).await.expect("open");
    let mut ups: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    ups.insert(LAYER.into(), up);
    // TUILE_BENCH_NO_DISK: memory only, to tell the disk cache's share.
    if std::env::var("TUILE_BENCH_NO_DISK").is_ok() {
        return TileService::new(Arc::new(store), ups);
    }
    let mut disk_cfg = tuile_tile_server::DiskCacheConfig::new(disk);
    disk_cfg.prefetch_neighbours = std::env::var("TUILE_BENCH_NEIGHBOURS").is_ok();
    if let Some(b) = std::env::var("TUILE_BENCH_AWAIT_COPY_BYTES").ok().and_then(|v| v.parse().ok()) {
        disk_cfg.await_copy_bytes = b;
    }
    let store = store.with_disk_cache(disk_cfg).await.expect("disk cache");
    TileService::new(Arc::new(store), ups)
}

fn describe(svc: &TileService) -> String {
    format!("{:?} {:?} disk={}MB", svc.stats(), svc.store().stats(), svc.store().disk_bytes() >> 20)
}

async fn settle(svc: &TileService) {
    svc.store().settle().await;
}
// END-SERVICE

fn row(name: &str, lat: &mut [Duration], upstream: u64, wall: Duration) {
    if lat.is_empty() {
        println!("| {name} | 0 | | | | | {upstream} | |");
        return;
    }
    lat.sort();
    let q = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)].as_secs_f64() * 1000.0;
    println!(
        "| {name} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {:.1} s |",
        lat.len(),
        q(0.50),
        q(0.95),
        q(0.99),
        lat.last().map_or(0.0, |d| d.as_secs_f64() * 1000.0),
        upstream,
        wall.as_secs_f64()
    );
}

async fn run(name: &str, svc: &TileService, up: &Simulated, tiles: &[Tile]) {
    let calls0 = up.calls.load(Ordering::Relaxed);
    let began = Instant::now();
    // (latency, served without the source)
    let all: Vec<(Duration, bool)> = futures_util::stream::iter(tiles.iter().copied())
        .map(|(z, x, y)| async move {
            let t = Instant::now();
            let hit = match svc.tile(LAYER, z, x, y).await {
                Ok(Some(r)) => r.hit,
                Ok(None) => true,
                Err(e) => {
                    eprintln!("{z}/{x}/{y}: {e}");
                    false
                }
            };
            (t.elapsed(), hit)
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    let wall = began.elapsed();
    let upstream = up.calls.load(Ordering::Relaxed) - calls0;
    let mut lat: Vec<Duration> = all.iter().map(|a| a.0).collect();
    let mut hits: Vec<Duration> = all.iter().filter(|a| a.1).map(|a| a.0).collect();
    let mut misses: Vec<Duration> = all.iter().filter(|a| !a.1).map(|a| a.0).collect();
    row(name, &mut lat, upstream, wall);
    row(&format!("{name}, hits"), &mut hits, 0, wall);
    row(&format!("{name}, misses"), &mut misses, upstream, wall);
    println!("    {}", describe(svc));
}

/// Every tile the bucket holds in the zones these tiles fall in, found with a
/// separate client before anything is timed.
async fn stored(objects: Arc<dyn ObjectStore>, tiles: &[Tile]) -> HashSet<Tile> {
    let mut prefixes: HashSet<String> = HashSet::new();
    for &(z, x, y) in tiles {
        if z < 10 {
            prefixes.insert(format!("{LAYER}/top"));
        } else {
            let s = z - 10;
            prefixes.insert(format!("{LAYER}/zones/z10/{}/{}", x >> s, y >> s));
        }
    }
    let mut out = HashSet::new();
    for prefix in prefixes {
        let Ok(m) = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await else { continue };
        for a in &m.manifest.archives {
            let Ok(r) = tuile_tile_server::archive::open_remote(objects.clone(), &a.key).await else { continue };
            let Ok(ids) = tuile_tile_server::archive::ids(Arc::new(r)).await else { continue };
            for id in ids {
                if let Ok(t) = pmtiles::TileId::new(id) {
                    let c = pmtiles::TileCoord::from(t);
                    out.insert((c.z(), c.x(), c.y()));
                }
            }
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let a = flight(FROM, TO, 1);
    let b = flight((FROM.0 + EAST, FROM.1), (TO.0 + EAST, TO.1), 1);
    // Only tiles the bucket holds, in a wider window.
    let wide = flight(FROM, TO, 3);
    let held = stored(bucket(), &wide).await;
    let s: Vec<Tile> = wide.into_iter().filter(|t| held.contains(t)).collect();

    let root: PathBuf = std::env::var("TUILE_BENCH_DISK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("tuile-flight-bench"));
    let _ = std::fs::remove_dir_all(&root);
    let objects = bucket();
    println!("flight A: {} tiles, flight B: {} tiles, stored-only S: {} tiles", a.len(), b.len(), s.len());
    println!("| scenario | requests | p50 ms | p95 ms | p99 ms | max ms | upstream | wall |");
    println!("|---|---|---|---|---|---|---|---|");

    // S: hits only, on its own disk. Cold, then a second instance.
    {
        let disk = root.join("s");
        let up = Arc::new(Simulated { calls: AtomicU64::new(0) });
        let svc = service(objects.clone(), &disk, up.clone()).await;
        run("S cold", &svc, &up, &s).await;
        settle(&svc).await;
        drop(svc);
        let svc = service(objects.clone(), &disk, up.clone()).await;
        run("S, second instance", &svc, &up, &s).await;
    }

    let disk = root.join("a");
    let up = Arc::new(Simulated { calls: AtomicU64::new(0) });
    let svc = service(objects.clone(), &disk, up.clone()).await;
    run("A cold", &svc, &up, &a).await;
    settle(&svc).await;
    run("A again, same process", &svc, &up, &a).await;
    run("B, parallel flight, same process", &svc, &up, &b).await;
    drop(svc);

    // A second instance, or a restart: nothing in memory.
    let up2 = Arc::new(Simulated { calls: AtomicU64::new(0) });
    let svc2 = service(objects, &disk, up2.clone()).await;
    run("B, second instance", &svc2, &up2, &b).await;
}
