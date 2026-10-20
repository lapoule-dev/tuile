// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render service through whole segments: a pack in a store on disk,
//! rendered on the machine's adapter, put back with its receipt — and every
//! part of the receipt said. Skipped when there is no adapter.
//!
//! The file's one test, so that its CPU sampler is the only one in the
//! process: one sampler runs at a time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::DVec3;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_farm::receipt::{SegmentDone, RECEIPT_VERSION};
use tuile_farm::{ObjectRunStore, Tuning};
use tuile_film::{BakedView, OverlayCamera, OverlayMesh, Overlays};
use tuile_film_native::serve::{
    Arrived, Limits, OverlayFactory, SegmentContext, SegmentOrder, Service,
};
use tuile_film_native::Error;
use tuile_pack::{BakedTile, Content, PackWriter, TextureFormat};

const FRAMES: u32 = 3;

/// One flat tile 1 km a side under a camera 600 m up, in a pack that names
/// the tile store's layers but carries its payload: the read ahead runs,
/// and asks the store for nothing.
fn write_pack(dir: &std::path::Path) -> String {
    let at = Geodetic {
        lon: 0.5f64.to_radians(),
        lat: 42.8f64.to_radians(),
        height: 0.0,
    };
    let centre = geodetic_to_ecef(at);
    let enu = enu_frame(at);
    let mut p = Vec::new();
    for (x, y) in [
        (-500.0, -500.0),
        (500.0, -500.0),
        (500.0, 500.0),
        (-500.0, 500.0),
    ] {
        let q = enu.x_axis * x + enu.y_axis * y;
        p.extend([q.x as f32, q.y as f32, q.z as f32]);
    }
    let up = enu.z_axis.as_vec3();
    let n: Vec<f32> = (0..4).flat_map(|_| [up.x, up.y, up.z]).collect();
    let uv = [0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let idx = [0u32, 1, 2, 0, 2, 3];
    let le = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 110, 90, 255]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    let tile = BakedTile {
        id: 1,
        drape: 0,
        origin_ecef: centre.to_array(),
        positions: le(&p),
        normals: le(&n),
        uvs: le(&uv),
        indices: idx.iter().flat_map(|v| v.to_le_bytes()).collect(),
        vertex_count: 4,
        index_count: 6,
        base_color_factor: [1.0; 4],
        texture: Some(png),
        texture_format: TextureFormat::Png,
        refs: None,
    };
    let blob = dir.join("blob");
    let mut writer = PackWriter::new("served", centre.to_array(), blob)
        .expect("writer")
        .with_content(Content::Both, "terrain", "imagery");
    for f in 1..=FRAMES {
        let eye: DVec3 = centre + enu.z_axis * 600.0 + enu.x_axis * f64::from(f);
        let view = BakedView {
            position: eye.to_array(),
            direction: (-enu.z_axis).to_array(),
            up: enu.y_axis.to_array(),
            viewport_px: [64.0, 48.0],
            fovy_rad: 0.8,
        };
        let mut open = writer.begin_frame(f, view);
        if !open.push_known(1, 0) {
            open.push(tile.clone());
        }
        open.end();
    }
    let key = "packs/served/1-3.tuilepack";
    let path = dir.join(key);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("packs dir");
    writer.finish_to(&path).expect("finish");
    key.into()
}

/// A host whose overlays take their time: made in [`MAKING`], each frame's
/// in [`DRAWING`].
struct Slow;

const MAKING: Duration = Duration::from_millis(20);
const DRAWING: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SlowOrder {}

struct SlowOverlays;

impl Overlays for SlowOverlays {
    fn frame(&mut self, _: u32, _: &OverlayCamera, _: &mut Vec<OverlayMesh>) {
        std::thread::sleep(DRAWING);
    }
}

#[async_trait::async_trait]
impl OverlayFactory for Slow {
    type Order = SlowOrder;

    async fn overlays(
        &self,
        _: &SlowOrder,
        _: &SegmentContext<'_>,
    ) -> Result<Box<dyn Overlays + Send>, Error> {
        tokio::time::sleep(MAKING).await;
        Ok(Box::new(SlowOverlays))
    }
}

fn has_adapter() -> bool {
    pollster::block_on(async {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .is_ok()
    })
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs_f64()
}

/// What every receipt of the service says, whoever served it.
fn says_where_its_time_went(done: &SegmentDone, asked_at_unix: f64) {
    assert_eq!(done.version, RECEIPT_VERSION);
    assert_eq!(done.frames, u64::from(FRAMES));
    assert!(
        (done.received_at_unix - asked_at_unix).abs() < 5.0,
        "{} against {asked_at_unix}",
        done.received_at_unix
    );
    let s = &done.stages;
    assert!(s.overlays_seconds >= MAKING.as_secs_f64(), "{s:?}");
    for (name, seconds) in [
        ("generation", s.generation_seconds),
        ("pack wait", s.pack_wait_seconds),
        ("film", s.film_seconds),
        ("put", s.put_seconds),
    ] {
        assert!(seconds > 0.0, "{name}: {s:?}");
    }
    // The wait is its parts, and little else (a scratch directory made).
    let parts = s.overlays_seconds
        + s.generation_seconds
        + s.pack_wait_seconds
        + s.film_seconds
        + s.fetch_seconds
        + s.card_wait_seconds;
    assert!(
        parts <= done.waited_seconds && done.waited_seconds - parts < 0.5,
        "{parts} of {}",
        done.waited_seconds
    );
    assert!(
        done.service_seconds
            >= s.before_seconds + done.waited_seconds + done.render_seconds + s.put_seconds,
        "{done:?}"
    );

    let r = &done.render;
    assert!(r.seconds > 0.0 && r.seconds <= done.render_seconds, "{r:?}");
    // The pack names the store's layers: its frames' tiles are asked for
    // ahead, none of them from the store.
    assert_eq!((r.ahead.packs, r.ahead.tiles), (1, 0), "{r:?}");
    assert!(r.ahead.seconds > 0.0);
    // Each frame's place in the pack, and its host's overlays: the two
    // frames after the opening drew theirs in DRAWING each.
    assert!(r.frames.advance > 0.0, "{r:?}");
    assert!(
        r.frames.overlays >= 2.0 * DRAWING.as_secs_f64() * 1000.0,
        "{r:?}"
    );
    assert!(r.opening.overlays >= DRAWING.as_secs_f64() * 1000.0);
    // The AV1 encoder's own thread.
    assert!(r.encode_seconds > 0.0, "{r:?}");
    assert!(r.encoder.contains("AV1"), "{}", r.encoder);
    assert!(!r.adapter.is_empty());
    // The card's clock: every frame's visibility pass, where it has one.
    assert_eq!(r.gpu.timestamps, !r.gpu.passes.is_empty(), "{:?}", r.gpu);
    if r.gpu.timestamps {
        let visibility = r
            .gpu
            .passes
            .iter()
            .find(|p| p.pass == "visibility")
            .expect("the visibility pass");
        assert_eq!(visibility.count, u64::from(FRAMES));
        assert!(visibility.seconds > 0.0);
    }

    let i = &done.instance;
    assert!(i.started_at_unix > 0.0 && i.started_at_unix <= done.received_at_unix);
    assert!(i.uptime_seconds > 0.0);
    // Its profile's bytes and the segment's are what it put.
    assert_eq!(
        done.io.put_bytes,
        done.bytes + done.profile.as_ref().map_or(0, |p| p.bytes)
    );
    // The phases add up to the service's time.
    let phases: f64 = done.phases().iter().map(|p| p.1).sum();
    assert!((phases - done.service_seconds).abs() < 1e-6, "{phases}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_segments_receipt_says_where_its_time_went_and_who_served_it() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let key = write_pack(dir.path());
    std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
    let runs = Arc::new(ObjectRunStore::local(dir.path(), Tuning::default()).expect("store"));
    let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
    // One render on the card at a time, as on the farm.
    let service = Service::with_overlays(
        runs,
        tiles,
        dir.path().join("cache"),
        Limits::default(),
        Slow,
    );
    // An instance up a moment before its first request.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let order = |n: u32| -> SegmentOrder<SlowOrder> {
        serde_json::from_value(serde_json::json!({
            "pack": key,
            "frames": [1, FRAMES],
            "out": format!("films/a/seg{n}.mp4"),
            "rendering": {"codec": "av1", "supersample": 1},
            "pack_within_seconds": 5,
            "overlay": {},
        }))
        .expect("an order")
    };

    // Two at once on a new instance: one opens the caches and has the
    // card, the other finds both taken — and one of them is sampled.
    let asked_at = unix_now();
    let host_held = Duration::from_millis(50);
    let arrived = Arrived(Instant::now() - host_held);
    let (zero, one) = (order(0), order(1));
    let (a, b) = tokio::join!(
        service.segment_arrived(&zero, arrived),
        service.segment(&one)
    );
    let (a, b) = (a.expect("segment 0"), b.expect("segment 1"));
    says_where_its_time_went(&a, asked_at);
    says_where_its_time_went(&b, asked_at);
    // The host held the first 50 ms before the service read it.
    assert!(a.stages.before_seconds >= host_held.as_secs_f64(), "{a:?}");
    assert!(b.stages.before_seconds < host_held.as_secs_f64());
    // The first segment of the instance is its cold one; the other the
    // second, in the generation the first opened.
    let (first, second) = if a.instance.segments_before == 0 {
        (&a, &b)
    } else {
        (&b, &a)
    };
    assert!(first.instance.cold && !first.instance.generation_reused);
    assert_eq!(second.instance.segments_before, 1);
    assert!(!second.instance.cold && second.instance.generation_reused);
    // One waited for the card while the other rendered.
    let waited = a.stages.card_wait_seconds.max(b.stages.card_wait_seconds);
    let rendered = a.render_seconds.min(b.render_seconds);
    assert!(
        waited >= 0.5 * rendered,
        "card waited {waited} s, a render takes {rendered} s"
    );
    // One sampler at a time: only one of the two has a profile.
    assert_eq!(
        u32::from(a.profile.is_some()) + u32::from(b.profile.is_some()),
        1,
        "{:?} {:?}",
        a.profile,
        b.profile
    );
    // The first read the pack from the bucket.
    assert!(first.io.pack_asked_bytes > 0, "{:?}", first.io);

    // Alone, warm: sampled, its profile beside it and named in its
    // receipt, the receipt in the store as it was answered.
    let two = order(2);
    let c = service.segment(&two).await.expect("segment 2");
    says_where_its_time_went(&c, asked_at);
    assert_eq!(c.instance.segments_before, 2);
    assert!(c.instance.generation_reused && c.instance.generation_age_seconds > 0.0);
    let profile = c.profile.clone().expect("a profile");
    assert_eq!(profile.key, "films/a/seg2.pprof");
    assert_eq!(profile.hz, 100);
    let pprof = std::fs::read(dir.path().join(&profile.key)).expect("the profile");
    assert_eq!(pprof.len() as u64, profile.bytes);
    assert_eq!(&pprof[..2], &[0x1f, 0x8b], "gzipped");
    let kept: SegmentDone = serde_json::from_slice(
        &std::fs::read(dir.path().join("films/a/seg2.json")).expect("the receipt"),
    )
    .expect("a receipt");
    assert_eq!(kept, c);
}
