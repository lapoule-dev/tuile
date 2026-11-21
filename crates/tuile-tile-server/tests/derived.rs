// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A parent layer: each tile made from the source's four children one level
//! finer, served, stored and kept up to date like any layer's tiles — with a
//! fake source injected, synthetic pictures, and the store in memory.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use object_store::ObjectStore;
use tuile_tile_server::catalog::layers;
use tuile_tile_server::derived::{
    downsample, encode_webp, linear_to_srgb, ParentConfig, SRGB_TO_LINEAR,
};
use tuile_tile_server::{
    Catalog, Derivations, Fresh, MemoryRegister, ParentLayer, Refreshed, ServiceConfig, Source,
    TileService, TileStore, Upstream, UpstreamError, Zone,
};

const SRC: &str = "src";
const PARENT: &str = "src-parent";
const SIDE: u32 = 256;
/// A parent tile in the test zone (z10 522/373): z12, its children at z13.
const PZ: u8 = 12;
const PX: u32 = 522 * 4 + 1;
const PY: u32 = 373 * 4 + 2;

fn catalog() -> Catalog {
    serde_json::from_value(serde_json::json!({"layers": [
        {"name": SRC, "grid": "web-mercator", "tile_type": "jpeg", "tile_compression": "none",
         "zone_level": 10, "expiry_days": 152, "content_type": "image/jpeg"},
        {"name": PARENT, "grid": "web-mercator", "tile_type": "webp", "tile_compression": "none",
         "zone_level": 10, "expiry_days": 152, "content_type": "image/webp", "derived_from": SRC}
    ]}))
    .expect("catalog")
}

fn jpeg(picture: &image::RgbImage) -> Bytes {
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .expect("a jpeg");
    Bytes::from(out.into_inner())
}

/// A child picture: smooth gradients that differ by tile and by `version`.
fn picture(z: u8, x: u32, y: u32, version: u32) -> image::RgbImage {
    image::RgbImage::from_fn(SIDE, SIDE, move |px, py| {
        let r = 20 + px * 150 / 255 + (x % 4) * 10 + version * 40;
        let g = 30 + py * 150 / 255 + (y % 4) * 10;
        let b = 40 + (px + py) * 100 / 510 + u32::from(z % 4) * 10;
        image::Rgb([r.min(255) as u8, g as u8, b as u8])
    })
}

/// The source: every tile a gradient, except those it is told it has not,
/// that stand for no imagery, or that fail; slow if asked to be.
#[derive(Default)]
struct Source_ {
    calls: AtomicUsize,
    absent: Mutex<HashSet<(u8, u32, u32)>>,
    black: Mutex<HashSet<(u8, u32, u32)>>,
    failing: Mutex<HashSet<(u8, u32, u32)>>,
    versions: Mutex<HashMap<(u8, u32, u32), u32>>,
    delay: Mutex<Duration>,
}

impl Source_ {
    fn bytes(&self, z: u8, x: u32, y: u32) -> Bytes {
        let v = self
            .versions
            .lock()
            .expect("test")
            .get(&(z, x, y))
            .copied()
            .unwrap_or(0);
        jpeg(&picture(z, x, y, v))
    }
}

#[async_trait]
impl Upstream for Source_ {
    async fn fetch(&self, z: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let delay = *self.delay.lock().expect("lock");
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if self.failing.lock().expect("lock").contains(&(z, x, y)) {
            return Err(UpstreamError("the source is down".into()));
        }
        if self.absent.lock().expect("lock").contains(&(z, x, y)) {
            return Ok(None);
        }
        if self.black.lock().expect("lock").contains(&(z, x, y)) {
            return Ok(Some(jpeg(&image::RgbImage::from_pixel(
                SIDE,
                SIDE,
                image::Rgb([0, 0, 0]),
            ))));
        }
        Ok(Some(self.bytes(z, x, y)))
    }
}

fn store_over(objects: &Arc<dyn ObjectStore>) -> Arc<TileStore> {
    Arc::new(TileStore::new(
        objects.clone(),
        layers(&catalog()).expect("layers"),
        eager(),
    ))
}

/// One process: its own store and memory over `objects`, the source
/// injected, the parent layer over the source as served.
fn instance(
    objects: &Arc<dyn ObjectStore>,
    source: Arc<Source_>,
    cfg: ServiceConfig,
) -> Arc<TileService> {
    serve(store_over(objects), source, cfg)
}

fn serve(store: Arc<TileStore>, source: Arc<Source_>, cfg: ServiceConfig) -> Arc<TileService> {
    let derived =
        Derivations::from_catalog(&catalog(), ParentConfig::default()).expect("derivations");
    let mut ups = derived.upstreams();
    ups.insert(SRC.into(), source);
    let service = Arc::new(TileService::with_config(store, ups, cfg));
    derived.bind(&service);
    service
}

fn decoded(bytes: &[u8]) -> image::RgbImage {
    image::load_from_memory(bytes).expect("a picture").to_rgb8()
}

fn children(z: u8, x: u32, y: u32) -> [(u8, u32, u32); 4] {
    ParentLayer::children(z, x, y).expect("children")
}

/// The parent of `source`'s current children, made the reference way.
fn expected(source: &Source_) -> Vec<u8> {
    let kids: Vec<image::RgbImage> = children(PZ, PX, PY)
        .iter()
        .map(|&(z, x, y)| decoded(&source.bytes(z, x, y)))
        .collect();
    downsample(
        [
            kids[0].as_raw(),
            kids[1].as_raw(),
            kids[2].as_raw(),
            kids[3].as_raw(),
        ],
        SIDE as usize,
    )
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&p, &q)| (f64::from(p) - f64::from(q)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

// ── The downsample and the encoding ──────────────────────────────────────

#[test]
fn the_mean_is_taken_in_linear_light() {
    // Black and white pixels side by side: half the light, which is sRGB
    // 188 — not 128, the mean of the codes.
    let checker: Vec<u8> = (0..SIDE * SIDE)
        .flat_map(|i| {
            let (x, y) = (i % SIDE, i / SIDE);
            let v = if (x + y) % 2 == 0 { 0 } else { 255 };
            [v, v, v]
        })
        .collect();
    let out = downsample([&checker, &checker, &checker, &checker], SIDE as usize);
    assert!(out.iter().all(|&v| v == 188), "{:?}", &out[..6]);
    assert_eq!(linear_to_srgb(32768), 188);
}

#[test]
fn each_child_lands_in_its_quarter_and_a_flat_colour_stays_itself() {
    let flat = |c: [u8; 3]| -> Vec<u8> { (0..SIDE * SIDE).flat_map(|_| c).collect() };
    let (nw, ne, sw, se) = (
        flat([200, 10, 10]),
        flat([10, 200, 10]),
        flat([10, 10, 200]),
        flat([77, 90, 86]),
    );
    let out = downsample([&nw, &ne, &sw, &se], SIDE as usize);
    let at = |x: u32, y: u32| -> [u8; 3] {
        let i = ((y * SIDE + x) * 3) as usize;
        [out[i], out[i + 1], out[i + 2]]
    };
    assert_eq!(at(0, 0), [200, 10, 10]);
    assert_eq!(at(255, 0), [10, 200, 10]);
    assert_eq!(at(0, 255), [10, 10, 200]);
    assert_eq!(at(200, 200), [77, 90, 86]);
    // Every code, as a flat child, comes back as itself.
    for k in 0..=255u8 {
        assert_eq!(linear_to_srgb(SRGB_TO_LINEAR[usize::from(k)]), k);
    }
}

#[test]
fn the_same_children_give_the_same_bytes() {
    let kids: Vec<Vec<u8>> = (0..4).map(|i| picture(13, i, i, 0).into_raw()).collect();
    let a = downsample([&kids[0], &kids[1], &kids[2], &kids[3]], SIDE as usize);
    let b = downsample([&kids[0], &kids[1], &kids[2], &kids[3]], SIDE as usize);
    assert_eq!(a, b);
    let (wa, wb) = (
        encode_webp(&a, SIDE, 85.0).expect("webp"),
        encode_webp(&b, SIDE, 85.0).expect("webp"),
    );
    assert_eq!(wa, wb, "libwebp, one thread: the same bytes");
    assert_eq!(&wa[..4], b"RIFF");
    assert_eq!(&wa[8..12], b"WEBP");
    let back = decoded(&wa);
    assert_eq!((back.width(), back.height()), (SIDE, SIDE));
    assert!(
        psnr(back.as_raw(), &a) > 30.0,
        "{} dB",
        psnr(back.as_raw(), &a)
    );
}

// ── Served, stored, served again ─────────────────────────────────────────

#[tokio::test]
async fn a_parent_tile_is_made_on_first_request_stored_and_then_read_from_the_store() {
    let objects = memory();
    let source = Arc::new(Source_::default());
    let a = instance(&objects, source.clone(), ServiceConfig::default());

    let first = a
        .tile(PARENT, PZ, PX, PY)
        .await
        .expect("tile")
        .expect("a parent tile");
    assert_eq!(first.source, Source::Upstream);
    assert_eq!(first.content_type, "image/webp");
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        4,
        "its four children, each asked once"
    );
    let got = decoded(&first.bytes);
    assert_eq!((got.width(), got.height()), (SIDE, SIDE));
    assert!(psnr(got.as_raw(), &expected(&source)) > 30.0);
    // The children were served — and so stored — as the source layer's own.
    for (z, x, y) in children(PZ, PX, PY) {
        assert_eq!(
            a.tile(SRC, z, x, y)
                .await
                .expect("child")
                .expect("held")
                .source,
            Source::Memory
        );
    }
    a.store().flush_all().await.expect("flush");

    // Another process, its memory empty, its source never asked.
    let quiet = Arc::new(Source_::default());
    let b = instance(&objects, quiet.clone(), ServiceConfig::default());
    let again = b
        .tile(PARENT, PZ, PX, PY)
        .await
        .expect("tile")
        .expect("stored");
    assert_eq!(again.source, Source::Store);
    assert_eq!(again.bytes, first.bytes);
    assert_eq!(
        quiet.calls.load(Ordering::SeqCst),
        0,
        "served from the store: no child asked"
    );
}

#[tokio::test]
async fn a_missing_child_makes_an_absence_and_never_a_tile() {
    let objects = memory();
    let source = Arc::new(Source_::default());
    source
        .absent
        .lock()
        .expect("test")
        .insert(children(PZ, PX, PY)[3]);
    let a = instance(&objects, source.clone(), ServiceConfig::default());
    assert!(
        a.tile(PARENT, PZ, PX, PY).await.expect("tile").is_none(),
        "no partial picture"
    );
    a.store().flush_all().await.expect("flush");

    let s = store_over(&objects);
    assert!(
        s.get(PARENT, PZ, PX, PY).await.expect("get").is_none(),
        "no tile written"
    );
    assert!(
        s.get(&format!("{PARENT}.absent"), PZ, PX, PY)
            .await
            .expect("get")
            .is_some(),
        "an absence recorded"
    );
    // Remembered: asked again, nothing is asked of the source.
    let quiet = Arc::new(Source_::default());
    let b = instance(&objects, quiet.clone(), ServiceConfig::default());
    assert!(b.tile(PARENT, PZ, PX, PY).await.expect("tile").is_none());
    assert_eq!(quiet.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_child_that_stands_for_no_imagery_makes_an_absence() {
    let objects = memory();
    let source = Arc::new(Source_::default());
    source
        .black
        .lock()
        .expect("lock")
        .insert(children(PZ, PX, PY)[1]);
    let a = instance(&objects, source.clone(), ServiceConfig::default());
    assert!(a.tile(PARENT, PZ, PX, PY).await.expect("tile").is_none());
    a.store().flush_all().await.expect("flush");
    let s = store_over(&objects);
    assert!(s.get(PARENT, PZ, PX, PY).await.expect("get").is_none());
    assert!(s
        .get(
            SRC,
            13,
            children(PZ, PX, PY)[1].1,
            children(PZ, PX, PY)[1].2
        )
        .await
        .expect("get")
        .is_none());
}

#[tokio::test]
async fn a_child_read_as_a_picture_of_no_imagery_is_told_apart() {
    // The parent over a source handing a black picture straight: what a
    // store that kept one from before its recogniser reads back.
    let recorded = Recorded::start();
    let source = Arc::new(Source_::default());
    source
        .black
        .lock()
        .expect("lock")
        .insert(children(PZ, PX, PY)[0]);
    let layer = tuile_tile_server::catalog::layers(&catalog())
        .expect("layers")
        .into_iter()
        .find(|l| l.name == SRC)
        .expect("test");
    let parent = ParentLayer::new(
        PARENT,
        source,
        Arc::new(move |b: &[u8]| tuile_tile_server::index::is_empty_tile(&layer, b)),
        ParentConfig::default(),
    );
    assert!(parent.fetch(PZ, PX, PY).await.expect("fetch").is_none());
    assert_eq!(
        recorded.counter(
            "tuile_derived_empty_total",
            &[("layer", PARENT), ("cause", "child_empty_imagery")]
        ),
        1
    );
    assert_eq!(
        recorded.counter(
            "tuile_derived_builds_total",
            &[("layer", PARENT), ("outcome", "built")]
        ),
        0
    );
}

#[tokio::test]
async fn a_child_that_fails_is_an_error_and_records_no_absence() {
    let objects = memory();
    let source = Arc::new(Source_::default());
    let child = children(PZ, PX, PY)[2];
    source.failing.lock().expect("lock").insert(child);
    let a = instance(&objects, source.clone(), ServiceConfig::default());
    assert!(
        a.tile(PARENT, PZ, PX, PY).await.is_err(),
        "a failure, not an absence"
    );
    a.store().flush_all().await.expect("flush");
    assert!(store_over(&objects)
        .get(&format!("{PARENT}.absent"), PZ, PX, PY)
        .await
        .expect("get")
        .is_none());
    // The source back: the tile is made.
    source.failing.lock().expect("lock").clear();
    assert!(a.tile(PARENT, PZ, PX, PY).await.expect("tile").is_some());
}

#[tokio::test]
async fn a_tile_past_the_last_level_has_no_children_and_no_tile() {
    let source = Arc::new(Source_::default());
    let a = instance(&memory(), source.clone(), ServiceConfig::default());
    let last = a.meta(PARENT).expect("meta").max_level;
    assert!(a.tile(PARENT, last, 0, 0).await.expect("tile").is_none());
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn requests_at_once_make_the_tile_once() {
    let recorded = Recorded::start();
    let source = Arc::new(Source_::default());
    *source.delay.lock().expect("lock") = Duration::from_millis(50);
    let a = instance(&memory(), source.clone(), ServiceConfig::default());
    let all = futures_util::future::join_all((0..16).map(|_| a.tile(PARENT, PZ, PX, PY))).await;
    let bytes: HashSet<Bytes> = all
        .into_iter()
        .map(|t| t.expect("tile").expect("present").bytes)
        .collect();
    assert_eq!(bytes.len(), 1);
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        4,
        "four children, once each"
    );
    assert_eq!(
        recorded.counter(
            "tuile_derived_builds_total",
            &[("layer", PARENT), ("outcome", "built")]
        ),
        1
    );
    assert_eq!(
        recorded.counter("tuile_tiles_flight_joined_total", &[("layer", PARENT)]),
        15
    );
}

// ── Kept up to date by its source ────────────────────────────────────────

/// A process that makes parent tiles again: its children read fresh from
/// the store each time (no memory of its own).
fn remaker(store: Arc<TileStore>, source: Arc<Source_>) -> Arc<TileService> {
    serve(
        store,
        source,
        ServiceConfig {
            hot_bytes: 0,
            ..ServiceConfig::default()
        },
    )
}

fn zone_of_children() -> Zone {
    Zone::Cell { x: 522, y: 373 }
}

#[tokio::test]
async fn a_changed_child_has_its_parent_made_again_and_the_old_copy_replaced() {
    let objects = memory();
    let source = Arc::new(Source_::default());
    let a = instance(&objects, source.clone(), ServiceConfig::default());
    let before = a
        .tile(PARENT, PZ, PX, PY)
        .await
        .expect("tile")
        .expect("made")
        .bytes;
    a.store().flush_all().await.expect("flush");

    // The source answers one child anew, and the new copy is published —
    // the zone's next generation — as a writer stores it.
    let (z, x, y) = children(PZ, PX, PY)[0];
    source.versions.lock().expect("lock").insert((z, x, y), 1);
    let store = store_over(&objects);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test")
        .as_millis() as u64;
    let written = store
        .publish_batch(
            SRC,
            zone_of_children(),
            vec![Fresh {
                level: z,
                x,
                y,
                bytes: source.bytes(z, x, y),
                fetched_ms: now,
            }],
        )
        .await
        .expect("published");
    assert_eq!(written, 1);

    // Whoever published it names the parent, and has it made again.
    let derived =
        Derivations::from_catalog(&catalog(), ParentConfig::default()).expect("derivations");
    let dependents = derived.dependents(SRC, z, x, y);
    assert_eq!(dependents, vec![(PARENT.to_string(), PZ, PX, PY)]);
    let quiet = Arc::new(Source_::default());
    let w = remaker(store.clone(), quiet.clone());
    let refreshed = w.refresh(PARENT, PZ, PX, PY).await.expect("refresh");
    let Refreshed::Tile(after) = refreshed else {
        panic!("{refreshed:?}")
    };
    assert_ne!(after, before);
    assert_eq!(
        quiet.calls.load(Ordering::SeqCst),
        0,
        "every child read from the store"
    );
    assert!(
        psnr(decoded(&after).as_raw(), &expected(&source)) > 30.0,
        "made of the new child"
    );
    store.flush_all().await.expect("flush");

    let reader = instance(
        &objects,
        Arc::new(Source_::default()),
        ServiceConfig::default(),
    );
    assert_eq!(
        reader
            .tile(PARENT, PZ, PX, PY)
            .await
            .expect("tile")
            .expect("held")
            .bytes,
        after
    );
}

#[tokio::test]
async fn an_unchanged_child_writes_nothing_new() {
    let objects = memory();
    let source = Arc::new(Source_::default());
    let a = instance(&objects, source.clone(), ServiceConfig::default());
    let before = a
        .tile(PARENT, PZ, PX, PY)
        .await
        .expect("tile")
        .expect("made")
        .bytes;
    a.store().flush_all().await.expect("flush");
    let store = store_over(&objects);
    let w = remaker(store.clone(), Arc::new(Source_::default()));
    assert_eq!(
        w.refresh(PARENT, PZ, PX, PY).await.expect("refresh"),
        Refreshed::Unchanged
    );
    assert_eq!(
        store.flush_all().await.expect("flush"),
        0,
        "the same bytes: nothing to write"
    );
    assert_eq!(
        store_over(&objects)
            .get(PARENT, PZ, PX, PY)
            .await
            .expect("get"),
        Some(before)
    );
}

#[tokio::test]
async fn a_child_gone_makes_the_parent_an_absence() {
    let objects = memory();
    let clock = TestClock::new();
    let reg = Arc::new(MemoryRegister::new());
    let open = || {
        Arc::new(
            TileStore::with_clock(
                objects.clone(),
                layers(&catalog()).expect("layers"),
                eager(),
                clock.clock(),
            )
            .with_ledger(reg.clone())
            .with_absences(reg.clone()),
        )
    };
    let source = Arc::new(Source_::default());
    let a = serve(open(), source.clone(), ServiceConfig::default());
    assert!(a.tile(PARENT, PZ, PX, PY).await.expect("tile").is_some());
    a.store().flush_all().await.expect("flush");
    clock.advance(Duration::from_secs(60));

    // The source has the child no more: its absence is published, newer
    // than the copy.
    let (z, x, y) = children(PZ, PX, PY)[3];
    let store = open();
    let fetched_ms = clock
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test")
        .as_millis() as u64;
    let (absence, at) = store
        .place(SRC, z, x, y, true)
        .expect("place")
        .expect("an absence layer");
    store
        .publish_batch(
            &absence,
            at,
            vec![Fresh {
                level: z,
                x,
                y,
                bytes: Bytes::from_static(&[0]),
                fetched_ms,
            }],
        )
        .await
        .expect("published");
    let w = remaker(store.clone(), Arc::new(Source_::default()));
    assert_eq!(
        w.refresh(PARENT, PZ, PX, PY).await.expect("refresh"),
        Refreshed::Absent
    );
    store.flush_all().await.expect("flush");

    let reader = serve(
        open(),
        Arc::new(Source_::default()),
        ServiceConfig::default(),
    );
    assert!(
        reader
            .tile(PARENT, PZ, PX, PY)
            .await
            .expect("tile")
            .is_none(),
        "the old copy no longer served"
    );
}

#[tokio::test]
async fn a_parent_tile_nobody_asked_for_is_not_made_when_a_child_changes() {
    let objects = memory();
    let store = store_over(&objects);
    let source = Arc::new(Source_::default());
    let w = remaker(store.clone(), source.clone());
    assert_eq!(
        w.refresh(PARENT, PZ, PX, PY).await.expect("refresh"),
        Refreshed::NotHeld
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.flush_all().await.expect("flush"), 0);
}

// ── Declared, and kept as long as its source ─────────────────────────────

#[test]
fn the_parent_layer_expires_with_its_source() {
    let all = layers(&catalog()).expect("layers");
    let of = |name: &str| all.iter().find(|l| l.name == name).expect("layer").clone();
    let (src, parent) = (of(SRC), of(PARENT));
    assert_eq!(parent.expiry, src.expiry);
    assert_eq!(parent.zone_level, src.zone_level);
    for epoch in ["202606", "202611", "202612"] {
        assert_eq!(parent.expires_at(epoch), src.expires_at(epoch), "{epoch}");
    }
    // And so is its absence sibling.
    assert_eq!(of(&format!("{PARENT}.absent")).expiry, src.expiry);
}

#[test]
fn the_catalog_says_what_a_layer_is_derived_from() {
    let c = catalog();
    let back: Catalog =
        serde_json::from_str(&serde_json::to_string(&c).expect("json")).expect("parse");
    assert_eq!(back, c);
    assert_eq!(c.layers[1].derived_from.as_deref(), Some(SRC));
    let plain = serde_json::to_value(&c.layers[0]).expect("json");
    assert!(
        plain.get("derived_from").is_none(),
        "absent where there is none"
    );
}
