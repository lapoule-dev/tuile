// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A zone projected whole, made once for every machine behind the same
//! tiers: kept there as one object under a key derived from its archives,
//! and read back in one request instead of the zone's archives.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::*;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use tuile_tile_server::projection::{
    bounds, projection_path, shared_projection_key, DEFAULT_TILE_FACTOR,
};
use tuile_tile_server::tiered::is_archive_key;
use tuile_tile_server::{Eye, Footprint, Grid, Tier, Tiered, TileStore, Zone};

/// An object store that counts the reads it answers, by key, heads apart.
#[derive(Debug)]
struct Counted {
    inner: Arc<dyn ObjectStore>,
    gets: Mutex<HashMap<String, u64>>,
}

impl Counted {
    fn over(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gets: Mutex::default(),
        })
    }

    fn snapshot(&self) -> HashMap<String, u64> {
        self.gets.lock().expect("gets").clone()
    }

    /// Reads since `before` of the keys `keep` keeps.
    fn since(&self, before: &HashMap<String, u64>, keep: impl Fn(&str) -> bool) -> u64 {
        self.snapshot()
            .iter()
            .filter(|(k, _)| keep(k))
            .map(|(k, n)| n - before.get(k).copied().unwrap_or(0))
            .sum()
    }
}

impl std::fmt::Display for Counted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Counted({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Counted {
    async fn put_opts(
        &self,
        l: &Path,
        p: object_store::PutPayload,
        o: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(
        &self,
        l: &Path,
        o: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(
        &self,
        l: &Path,
        o: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if !o.head {
            *self
                .gets
                .lock()
                .expect("gets")
                .entry(l.to_string())
                .or_default() += 1;
        }
        self.inner.get_opts(l, o).await
    }
    fn delete_stream(
        &self,
        l: futures_util::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(l)
    }
    fn list(
        &self,
        p: Option<&Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(p)
    }
    async fn list_with_delimiter(
        &self,
        p: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(
        &self,
        f: &Path,
        t: &Path,
        o: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(f, t, o).await
    }
}

/// The centre of a Web Mercator tile, as an eye position at `height`.
fn eye_over(level: u8, x: u32, y: u32, height: f64) -> Eye {
    let [w, s, e, n] = bounds(Grid::WebMercator, level, x, y);
    Eye {
        lon: (w + e) / 2.0,
        lat: (s + n) / 2.0,
        height,
    }
}

/// The coarse tiles of `top`, over the whole globe.
const TOP: [(u8, u32, u32); 4] = [(3, 4, 2), (3, 0, 7), (5, 16, 11), (2, 1, 1)];

/// The origin: a cell zone of imagery in one archive, and `top` in three —
/// one flush each, the last one rewriting a tile of the first.
async fn origin(clock: &TestClock) -> Arc<Counted> {
    let objects = memory();
    let s = store_on(objects.clone(), clock, eager());
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 0))
            .await
            .expect("put");
    }
    s.flush_all().await.expect("flush");
    for (i, (l, x, y)) in TOP.iter().enumerate() {
        s.put(IMAGERY, *l, *x, *y, body(*l, *x, *y, 0))
            .await
            .expect("put");
        if i % 2 == 1 {
            s.flush_all().await.expect("flush");
        }
    }
    let (l, x, y) = TOP[0];
    s.put(IMAGERY, l, x, y, body(l, x, y, 1))
        .await
        .expect("put");
    s.flush_all().await.expect("flush");
    assert_eq!(
        archives(objects.as_ref(), "imagery/top").await.len(),
        3,
        "top in three archives"
    );
    Counted::over(objects)
}

/// One machine: a store over the origin behind `global`, sharing its whole
/// projections there.
fn machine(origin: &Arc<Counted>, global: &Arc<Counted>, clock: &TestClock) -> TileStore {
    let tiered = Tiered::new(
        origin.clone(),
        vec![Tier::new("global", global.clone() as Arc<dyn ObjectStore>)],
    );
    TileStore::with_clock(
        Arc::new(tiered.clone()),
        vec![imagery(), terrain()],
        eager(),
        clock.clock(),
    )
    .with_shared_projections(tiered)
}

fn footprint() -> Footprint {
    Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], DEFAULT_TILE_FACTOR)
}

/// The keys of the projections kept in `global`.
async fn shared_keys(global: &dyn ObjectStore) -> Vec<String> {
    use futures_util::TryStreamExt;
    let mut keys: Vec<String> = global
        .list(Some(&Path::from("projections")))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .expect("list");
    keys.sort();
    keys
}

/// Every tile the test wrote, as `store` reads it.
async fn every_tile(store: &TileStore) -> Vec<Option<bytes::Bytes>> {
    let mut out = Vec::new();
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        out.push(store.get(IMAGERY, LEVEL, x, y).await.expect("get"));
    }
    for (l, x, y) in TOP {
        out.push(store.get(IMAGERY, l, x, y).await.expect("get"));
    }
    out
}

#[tokio::test]
async fn another_machine_reads_each_whole_zone_as_one_object() {
    let clock = TestClock::new();
    let origin = origin(&clock).await;
    let global = Counted::over(memory());

    // The first machine makes both zones, and keeps each in the tier.
    let here = machine(&origin, &global, &clock);
    let a = tempfile::tempdir().expect("dir");
    let made = here.project(&footprint(), a.path()).await.expect("project");
    assert_eq!(made.zones_projected, 2, "{made:?}");
    assert_eq!(
        (made.zones_shared, made.zones_shared_written),
        (0, 2),
        "{made:?}"
    );
    let keys = shared_keys(global.as_ref()).await;
    assert_eq!(keys.len(), 2, "{keys:?}");

    // Another machine: one request per zone, to the tier; not one archive
    // read, from the tier or from the origin.
    let there = machine(&origin, &global, &clock);
    let b = tempfile::tempdir().expect("dir");
    let (o, g) = (origin.snapshot(), global.snapshot());
    let read = there
        .project(&footprint(), b.path())
        .await
        .expect("project");
    assert_eq!(
        (
            read.zones_projected,
            read.zones_shared,
            read.zones_shared_written
        ),
        (2, 2, 0),
        "{read:?}"
    );
    assert_eq!(read.requests, 2, "{read:?}");
    assert_eq!(
        origin.since(&o, is_archive_key),
        0,
        "no archive from the origin"
    );
    assert_eq!(
        global.since(&g, is_archive_key),
        0,
        "no archive from the tier"
    );
    for key in &keys {
        assert_eq!(global.since(&g, |k| k == key), 1, "{key} read once");
    }

    // The same files, and the same tiles as a store without tiers reads —
    // the newest copy of the tile rewritten included.
    for zone in [zone(), Zone::Top] {
        assert!(
            std::fs::read(projection_path(a.path(), &imagery(), zone)).expect("a")
                == std::fs::read(projection_path(b.path(), &imagery(), zone)).expect("b"),
            "{zone:?}: the projection read is the one made"
        );
    }
    let reference = store_on(origin.clone(), &clock, eager());
    let tiles = every_tile(&there).await;
    assert_eq!(tiles, every_tile(&reference).await);
    let (l, x, y) = TOP[0];
    assert!(tiles.contains(&Some(body(l, x, y, 1))));
    let stats = there.stats();
    assert_eq!(
        (stats.projection_misses, stats.remote),
        (0, 0),
        "every tile from the projections: {stats:?}"
    );
}

#[tokio::test]
async fn a_projection_read_from_the_tier_is_the_one_made_without_tiers() {
    let clock = TestClock::new();
    let origin = origin(&clock).await;
    let global = Counted::over(memory());
    let here = machine(&origin, &global, &clock);
    here.project(&footprint(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");

    // As before this change: no tiers at all.
    let alone = store_on(origin.clone(), &clock, eager());
    let a = tempfile::tempdir().expect("dir");
    alone
        .project(&footprint(), a.path())
        .await
        .expect("project");
    let there = machine(&origin, &global, &clock);
    let b = tempfile::tempdir().expect("dir");
    let read = there
        .project(&footprint(), b.path())
        .await
        .expect("project");
    assert_eq!(read.zones_shared, 2, "{read:?}");
    for zone in [zone(), Zone::Top] {
        assert!(
            std::fs::read(projection_path(a.path(), &imagery(), zone)).expect("a")
                == std::fs::read(projection_path(b.path(), &imagery(), zone)).expect("b"),
            "{zone:?}: byte for byte"
        );
    }
}

#[tokio::test]
async fn bakes_of_one_machine_at_once_read_the_shared_projection_once() {
    let clock = TestClock::new();
    let origin = origin(&clock).await;
    let global = Counted::over(memory());
    machine(&origin, &global, &clock)
        .project(&footprint(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    let keys = shared_keys(global.as_ref()).await;

    let there = machine(&origin, &global, &clock);
    let dir = tempfile::tempdir().expect("dir");
    let g = global.snapshot();
    let o = origin.snapshot();
    // Six bakes interleaved on one runtime, as the six of an instance are.
    let fp = footprint();
    let reports: Vec<_> = futures_util::future::join_all(
        (0..6).map(|_| async { there.project(&fp, dir.path()).await.expect("project") }),
    )
    .await;
    for key in &keys {
        assert_eq!(global.since(&g, |k| k == key), 1, "{key}: {reports:?}");
    }
    assert_eq!(global.since(&g, is_archive_key), 0);
    assert_eq!(origin.since(&o, is_archive_key), 0);
    assert_eq!(
        reports.iter().map(|r| r.zones_shared).sum::<usize>(),
        2,
        "read by one bake: {reports:?}"
    );
    assert_eq!(
        reports.iter().map(|r| r.zones_reused).sum::<usize>(),
        2 * 5,
        "and reused by the five others: {reports:?}"
    );
}

#[tokio::test]
async fn a_zone_that_gains_an_archive_has_a_new_key() {
    let clock = TestClock::new();
    let origin = origin(&clock).await;
    let global = Counted::over(memory());
    machine(&origin, &global, &clock)
        .project(&footprint(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    let before = shared_keys(global.as_ref()).await;

    // A tile of `top` rewritten: one more archive in the zone.
    let (l, x, y) = TOP[1];
    let w = store_on(origin.clone(), &clock, eager());
    w.put(IMAGERY, l, x, y, body(l, x, y, 2))
        .await
        .expect("put");
    w.flush_all().await.expect("flush");

    let there = machine(&origin, &global, &clock);
    let report = there
        .project(&footprint(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    assert_eq!(
        (report.zones_shared, report.zones_shared_written),
        (1, 1),
        "the cell read, the top made again: {report:?}"
    );
    let after = shared_keys(global.as_ref()).await;
    assert_eq!(
        after.len(),
        3,
        "the old key stays, never rewritten: {after:?}"
    );
    assert!(before.iter().all(|k| after.contains(k)));
    assert_eq!(
        there.get(IMAGERY, l, x, y).await.expect("get"),
        Some(body(l, x, y, 2))
    );
}

#[tokio::test]
async fn a_targeted_zone_is_not_shared_and_its_top_is() {
    let clock = TestClock::new();
    let origin = origin(&clock).await;
    let global = Counted::over(memory());
    let targeted = || footprint().with_targeted_layer(IMAGERY);
    let report = machine(&origin, &global, &clock)
        .project(&targeted(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    assert_eq!(report.zones_shared_written, 1, "{report:?}");
    let keys = shared_keys(global.as_ref()).await;
    assert_eq!(keys.len(), 1);
    assert!(keys[0].starts_with("projections/imagery/top/"), "{keys:?}");

    let there = machine(&origin, &global, &clock);
    let report = there
        .project(&targeted(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    assert_eq!(report.zones_shared, 1, "{report:?}");
    assert_eq!(
        every_tile(&there).await,
        every_tile(&store_on(origin.clone(), &clock, eager())).await
    );
}

#[test]
fn a_key_names_its_layer_its_zone_and_every_archive() {
    let one = shared_projection_key(&imagery(), Zone::Top, "whole\na\nb");
    assert!(one.starts_with("projections/imagery/top/") && one.ends_with(".pmtiles"));
    assert_eq!(
        one,
        shared_projection_key(&imagery(), Zone::Top, "whole\na\nb")
    );
    assert_ne!(
        one,
        shared_projection_key(&imagery(), Zone::Top, "whole\na\nc")
    );
    assert_ne!(
        one,
        shared_projection_key(&imagery(), Zone::Top, "whole\na")
    );
    assert_ne!(
        one,
        shared_projection_key(&terrain(), Zone::Top, "whole\na\nb")
    );
    assert_ne!(
        one,
        shared_projection_key(&imagery(), zone(), "whole\na\nb")
    );
}

#[tokio::test]
async fn a_broken_object_in_the_tier_is_left_and_the_zone_made_here() {
    let clock = TestClock::new();
    let origin = origin(&clock).await;
    let global = Counted::over(memory());
    machine(&origin, &global, &clock)
        .project(&footprint(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    for key in shared_keys(global.as_ref()).await {
        global
            .put(
                &Path::from(key.as_str()),
                bytes::Bytes::from_static(b"not an archive").into(),
            )
            .await
            .expect("put");
    }
    let there = machine(&origin, &global, &clock);
    let report = there
        .project(&footprint(), tempfile::tempdir().expect("dir").path())
        .await
        .expect("project");
    assert_eq!(
        (report.zones_projected, report.zones_shared),
        (2, 0),
        "{report:?}"
    );
    assert_eq!(
        every_tile(&there).await,
        every_tile(&store_on(origin.clone(), &clock, eager())).await
    );
}

#[tokio::test]
async fn a_projection_of_several_slices_is_read_whole() {
    let clock = TestClock::new();
    let bucket = memory();
    let w = store_on(bucket.clone(), &clock, eager());
    // Some 40 MB of `top`: three slices.
    let mut n = 0;
    'fill: for l in 0..9u8 {
        for x in 0..1u32 << l {
            for y in 0..1u32 << l {
                let mut b = body(l, x, y, 0).to_vec();
                b.resize(40_000, (x ^ y ^ u32::from(l)) as u8);
                w.put(IMAGERY, l, x, y, bytes::Bytes::from(b))
                    .await
                    .expect("put");
                n += 1;
                if n == 1_000 {
                    break 'fill;
                }
            }
        }
    }
    w.flush_all().await.expect("flush");
    let origin = Counted::over(bucket);
    let global = Counted::over(memory());
    let a = tempfile::tempdir().expect("dir");
    machine(&origin, &global, &clock)
        .project(&footprint(), a.path())
        .await
        .expect("project");
    let b = tempfile::tempdir().expect("dir");
    let read = machine(&origin, &global, &clock)
        .project(&footprint(), b.path())
        .await
        .expect("project");
    assert_eq!(read.zones_shared, 1, "{read:?}");
    assert_eq!(read.requests, 3, "{read:?}");
    let top = |d: &std::path::Path| {
        std::fs::read(projection_path(d, &imagery(), Zone::Top)).expect("top")
    };
    let made = top(a.path());
    assert!(made.len() > 32 << 20, "{}", made.len());
    assert!(made == top(b.path()), "byte for byte");
}

/// A tier at a distance: every read and write waits `latency`, then the
/// bytes at `bytes_per_s` — a model of a regional bucket seen from a
/// machine of its region, not a measure of one.
#[derive(Debug)]
struct Far {
    inner: Arc<dyn ObjectStore>,
    latency: std::time::Duration,
    bytes_per_s: f64,
}

impl Far {
    async fn wait(&self, bytes: u64) {
        tokio::time::sleep(
            self.latency + std::time::Duration::from_secs_f64(bytes as f64 / self.bytes_per_s),
        )
        .await;
    }
}

impl std::fmt::Display for Far {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Far({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Far {
    async fn put_opts(
        &self,
        l: &Path,
        p: object_store::PutPayload,
        o: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.wait(p.content_length() as u64).await;
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(
        &self,
        l: &Path,
        o: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(
        &self,
        l: &Path,
        o: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        let got = self.inner.get_opts(l, o).await;
        let bytes = got.as_ref().map_or(0, |g| g.range.end - g.range.start);
        self.wait(bytes).await;
        got
    }
    fn delete_stream(
        &self,
        l: futures_util::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(l)
    }
    fn list(
        &self,
        p: Option<&Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(p)
    }
    async fn list_with_delimiter(
        &self,
        p: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(
        &self,
        f: &Path,
        t: &Path,
        o: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(f, t, o).await
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// **Bench, before and after: a fresh machine projecting the `top` zone.**
/// The shape of the measured production `top` (2026-11-14: 11 047 tiles,
/// 225 MB, in ten archives of a tiered compaction), every tile 20 kB, behind
/// a global tier modelled by [`Far`] (`TUILE_BENCH_LATENCY_MS`, 30;
/// `TUILE_BENCH_MBPS`, 80) and an origin the same. Three machines in turn:
/// the first (the tier empty), a second (a fresh instance, the tier warm),
/// and the second again (a warm instance). Before: a store reading through
/// the tiers as it did; after: the same, sharing its whole projections. One
/// JSON line per machine to `TUILE_BENCH_OUT`. Run on demand:
/// `TUILE_BENCH_OUT=… cargo test --release -p tuile-tile-server --test
/// shared_projection -- --ignored bench`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_a_fresh_machine_projecting_top() {
    use std::io::Write as _;
    let out = std::env::var("TUILE_BENCH_OUT").expect("TUILE_BENCH_OUT");
    let latency = std::time::Duration::from_secs_f64(env_f64("TUILE_BENCH_LATENCY_MS", 30.0) / 1e3);
    let bytes_per_s = env_f64("TUILE_BENCH_MBPS", 80.0) * 1e6;
    let tile_bytes = env_f64("TUILE_BENCH_TILE_KB", 20.0) as usize * 1000;
    let mut lines = std::fs::File::create(&out).expect("bench out");
    // A tiered compaction's archives, largest first: 11 047 tiles.
    let runs = [6000u32, 1500, 1500, 500, 500, 400, 300, 200, 100, 47];
    let clock = TestClock::new();
    let bucket = memory();
    {
        let w = store_on(bucket.clone(), &clock, eager());
        let mut ids = (0..10u8).flat_map(|l| {
            let n = 1u32 << l;
            (0..n).flat_map(move |x| (0..n).map(move |y| (l, x, y)))
        });
        for run in runs {
            for (l, x, y) in ids.by_ref().take(run as usize) {
                let mut b = body(l, x, y, 0).to_vec();
                b.resize(tile_bytes, (x ^ y) as u8);
                w.put(IMAGERY, l, x, y, bytes::Bytes::from(b))
                    .await
                    .expect("put");
            }
            w.flush_all().await.expect("flush");
        }
    }
    assert_eq!(
        archives(bucket.as_ref(), "imagery/top").await.len(),
        runs.len()
    );
    for mode in ["before", "after"] {
        let origin = Counted::over(Arc::new(Far {
            inner: bucket.clone(),
            latency,
            bytes_per_s,
        }));
        let kept = memory();
        let global = Counted::over(Arc::new(Far {
            inner: kept.clone(),
            latency,
            bytes_per_s,
        }));
        let second = tempfile::tempdir().expect("dir");
        let first = tempfile::tempdir().expect("dir");
        let new_machine = || {
            let tiered = Tiered::new(
                origin.clone(),
                vec![Tier::new("global", global.clone() as Arc<dyn ObjectStore>)],
            );
            let s = TileStore::with_clock(
                Arc::new(tiered.clone()),
                vec![imagery(), terrain()],
                eager(),
                clock.clock(),
            );
            match mode {
                "before" => s,
                _ => s.with_shared_projections(tiered),
            }
        };
        let fp = Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], DEFAULT_TILE_FACTOR);
        let (one, two) = (new_machine(), new_machine());
        let machines: [(&str, &TileStore, &std::path::Path); 3] = [
            ("first machine, tier empty", &one, first.path()),
            ("fresh machine, tier warm", &two, second.path()),
            ("same machine, next bake", &two, second.path()),
        ];
        for (step, store, dir) in machines {
            let (o, g) = (origin.snapshot(), global.snapshot());
            let began = std::time::Instant::now();
            let r = store.project(&fp, dir).await.expect("project");
            let seconds = began.elapsed().as_secs_f64();
            let top = projection_path(dir, &imagery(), Zone::Top);
            let digest = {
                use sha2::Digest as _;
                let d = sha2::Sha256::digest(std::fs::read(&top).expect("top"));
                d.iter().map(|b| format!("{b:02x}")).collect::<String>()
            };
            writeln!(
                lines,
                "{{\"mode\":\"{mode}\",\"step\":\"{step}\",\"seconds\":{seconds},\"origin_archive_requests\":{},\"global_archive_requests\":{},\"global_projection_requests\":{},\"report_requests\":{},\"report_bytes\":{},\"zones_reused\":{},\"zones_shared\":{},\"zones_shared_written\":{},\"top_sha256\":\"{digest}\"}}",
                origin.since(&o, is_archive_key),
                global.since(&g, is_archive_key),
                global.since(&g, |k| k.starts_with("projections/")),
                r.requests,
                r.bytes,
                r.zones_reused,
                r.zones_shared,
                r.zones_shared_written,
            )
            .expect("write");
        }
    }
}
