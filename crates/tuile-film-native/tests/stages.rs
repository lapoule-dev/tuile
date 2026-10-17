// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A time-lapse through the film's own stages: its tape written with its
//! instants, baked (no ground: no source to reach), rendered under the
//! physical atmosphere with each frame lit by its instant, and joined — all
//! of it by the code `tuile-film-produce` runs, on a store on disk, on the
//! machine's adapter. Skipped when there is none, and built only where an
//! H.264 encoder is (a join reads H.264): VideoToolbox on a Mac, NVENC on
//! a card.

#![allow(unused_imports, dead_code)]

use std::sync::Arc;

use tuile_bake::open::Ground;
use tuile_bake::tape::TapeBake;
use tuile_farm::produce::{produce, Host, Journal, Order, Stretch};
use tuile_farm::{ObjectRunStore, Tuning};
use tuile_film_native::engine::{Engine, RenderSpec};
use tuile_tape::timelapse::TimeLapse;

fn has_adapter() -> bool {
    pollster::block_on(async {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .is_ok()
    })
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

#[cfg(any(
    all(target_os = "macos", feature = "videotoolbox"),
    all(target_os = "linux", feature = "nvenc")
))]
#[tokio::test(flavor = "multi_thread")]
async fn a_time_lapse_is_baked_rendered_and_joined_lit_by_its_instants() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let tape = dir.path().join("sky.mcap");
    let lapse = TimeLapse::parse(&words(
        "start=2026-10-19T16:00Z hours=3 length=1 fps=3 track=sun-moon pitch=-4 fov=70",
    ))
    .expect("a time-lapse");
    tuile_film_native::timelapse::write_tape(&lapse, &tape).expect("tape");
    let clock = tuile_tape::Clock::of_tape(&tape)
        .expect("read")
        .expect("instants");

    // Without instants, a physical atmosphere is refused.
    let unlit = RenderSpec {
        atmosphere: true,
        ..RenderSpec::default()
    };
    assert!(unlit.rendering().is_err());

    let spec = RenderSpec {
        fps: 3,
        codec: "h264".into(),
        atmosphere: true,
        clock: Some(clock.to_arg()),
        sun_scale: Some(3.0),
        moon_scale: Some(5.0),
        ..RenderSpec::default()
    };
    let rendering = spec.rendering().expect("rendering");
    let store = Arc::new(
        ObjectRunStore::local(&dir.path().join("store"), Tuning::default()).expect("store"),
    );
    let bake = TapeBake {
        token: String::new(),
        cache_dir: None,
        poses: TapeBake::poses_of(&tape).expect("poses"),
        viewport: (64.0, 36.0),
        sse: None,
        terrain: None,
        imagery: None,
        content: tuile_pack::Content::Embedded,
        ground: Ground::Empty,
    };
    // A tile store of no layer: the ground is empty.
    let tiles_dir = dir.path().join("tiles");
    std::fs::create_dir_all(&tiles_dir).expect("tiles dir");
    std::fs::write(tiles_dir.join("catalog.json"), br#"{"layers":[]}"#).expect("catalog");
    let tiles = Arc::new(ObjectRunStore::local(&tiles_dir, Tuning::default()).expect("tiles"));
    let cache = dir.path().join("cache");
    let engine = Engine::open(bake, rendering, 1, store.clone(), tiles, &cache)
        .await
        .expect("engine");
    let host: Arc<dyn Host> = Arc::new(engine);
    let order = Order {
        fps: Some(3.0),
        ..Order::of(Stretch { first: 1, last: 3 })
    };
    let film = produce(&order, store.as_ref(), host, &Journal::default())
        .await
        .expect("a film");
    assert_eq!(film.frames, 3);
    let out = dir.path().join("film.mp4");
    tuile_farm::RunStore::get(store.as_ref(), &film.key, &out)
        .await
        .expect("fetched");
    let probe = tuile_farm::concat::probe(&out).expect("probed");
    assert_eq!(probe.frames, 3);
    assert!((probe.seconds - 1.0).abs() < 1e-3, "{}", probe.seconds);
}
