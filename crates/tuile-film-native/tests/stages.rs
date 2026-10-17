// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A time-lapse through the film's own stages: its tape written with its
//! instants, baked (no ground: no source to reach) with its night lights
//! written into the tile store as an open layer, rendered under the
//! physical atmosphere with each frame lit by its instant and the lights
//! read back from the store, and joined — all
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
    // A night mosaic whose every texel says where it is: written by the
    // bake into the store, read back by the render.
    let (across, down) = tuile_bake::open::night_grid();
    let side = tuile_bake::open::NIGHT_SIDE;
    let (width, height) = (across * side, down * side);
    let mosaic = tuile_bake::open::NightLights {
        width,
        height,
        luma: (0..width * height)
            .map(|n| (((n % width) / 7 + (n / width) / 3) % 255 + 1) as u8)
            .collect(),
    };
    let poses = TapeBake::poses_of(&tape).expect("poses");
    let footprint = tuile_bake::open::night_footprint(&poses);
    let bake = TapeBake {
        token: String::new(),
        cache_dir: None,
        poses,
        viewport: (64.0, 36.0),
        sse: None,
        terrain: None,
        imagery: None,
        content: tuile_pack::Content::References,
        ground: Ground::Empty,
        night_lights: Some(tuile_bake::open::NightLightsSource::Given(Arc::new(
            mosaic.clone(),
        ))),
    };
    // The tile store on a disk: the bake writes it, the render reads it.
    // This file's one test, so the environment is its own.
    let tiles_dir = dir.path().join("tiles");
    std::env::set_var("TUILE_TILES_LOCAL", &tiles_dir);
    std::env::remove_var("TUILE_TILES_BUCKET");
    std::fs::create_dir_all(&tiles_dir).expect("tiles dir");
    let tiles = Arc::new(ObjectRunStore::local(&tiles_dir, Tuning::default()).expect("tiles"));
    let cache = dir.path().join("cache");
    let engine = Engine::open(bake, rendering, 1, store.clone(), tiles.clone(), &cache)
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

    // The night lights went through the store: declared in its catalog with
    // their licence and credit, their footprint's tiles written there, and
    // read back whole by the render's own reader.
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tiles_dir.join("catalog.json")).expect("catalog"))
            .expect("json");
    let layer = tuile_bake::source_namespace(tuile_bake::open::BLACK_MARBLE);
    let declared = catalog["layers"]
        .as_array()
        .expect("layers")
        .iter()
        .find(|l| l["name"] == layer.as_str())
        .expect("the night layer is declared");
    assert_eq!(declared["licence"], "Public domain (NASA)");
    let sources = tuile_film_native::Sources::open(store.clone(), tiles, &dir.path().join("read"))
        .await
        .expect("sources");
    let read = tuile_film_native::render::night_lights_of(&sources.store.tiles, &layer)
        .await
        .expect("the night lights, from the store");
    assert!(!footprint.is_empty() && footprint.len() < (across * down) as usize);
    for y in 0..down {
        for x in 0..across {
            let at = ((y * side + side / 2) * width + x * side + side / 2) as usize;
            let expected = if footprint.contains(&(x, y)) {
                mosaic.luma[at]
            } else {
                0
            };
            assert_eq!(read.luma[at], expected, "tile {x},{y}");
        }
    }
}
