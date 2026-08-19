// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `ContentPump::advance_with`: several views, one residency — the consumer
//! side of the traversal's multi-view union.
//!
//! The scenario a headless recorder runs: while one slice of a camera path is
//! rendered from residency, the *next* slice's views are sent along with it so
//! its ground streams in eagerly. That only works if a second view genuinely
//! widens what the session loads — which the traversal promises (selection is
//! the union over views) and this test holds the pump to, through the real
//! server, the real traversal and real GPU uploads.
//!
//! Hermetic like the other scenarios here: the loader answers instantly from a
//! generated surface, so whatever passes or fails is the engine's decision
//! making, not latency. Skips without a usable GPU adapter;
//! `TUILE_REQUIRE_GPU=1` turns the skip into a failure.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;

use glam::{DVec2, DVec3};

use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::runtime::in_process_with;
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_core::traversal::{Config, ViewState};
use tuile_terrain::{fill_content, GeographicTilingScheme, LayerJson, TileCoord};
use tuile_wgpu::{ContentPump, GpuContext};

/// Deepest level the synthetic terrain serves.
const DEEPEST: u32 = 5;

/// Levels pinned for the session — everything above stays resident globally,
/// so the discriminating evidence must live *below* this level.
const PINNED: u32 = 2;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU scenario: no usable adapter ({e})");
            None
        }
    }
}

/// A `layer.json` with every tile down to [`DEEPEST`].
fn everywhere() -> LayerJson {
    let ranges: Vec<String> = (0..=DEEPEST)
        .map(|level| {
            let (x, y) = (2u64 << level, 1u64 << level);
            format!(
                r#"[{{"startX":0,"startY":0,"endX":{},"endY":{}}}]"#,
                x - 1,
                y - 1
            )
        })
        .collect();
    let doc = format!(
        r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
            "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
            "bounds":[-180,-90,180,90],"available":[{}]}}"#,
        ranges.join(",")
    );
    LayerJson::from_slice(doc.as_bytes()).expect("layer.json")
}

/// Answers every tile instantly with a flat surface over its own rectangle.
struct Instant;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl TileLoader for Instant {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let (z, x, y) = id.terrain_coord();
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
        Ok(Loaded::Content(fill_content(&rect, [Some(0.0); 4], 0.0)))
    }
}

/// A camera looking straight down over `lon_deg` on the equator, low enough
/// that refinement wants levels below the pin.
fn over(lon_deg: f64) -> ViewState {
    let ground = geodetic_to_ecef(Geodetic {
        lon: lon_deg.to_radians(),
        lat: 0.0,
        height: 0.0,
    });
    let up = ground.normalize();
    let eye = ground + up * 30_000.0;
    let screen_up = DVec3::Z.cross(up).normalize_or(DVec3::Y);
    ViewState::perspective(
        eye,
        -up,
        screen_up,
        DVec2::new(1024.0, 1024.0),
        45f64.to_radians(),
    )
}

/// Drives the session until quiet: poll the server, take uploads, repeat.
fn settle(
    server: &mut Pin<Box<dyn Future<Output = ()>>>,
    stream: &mut tuile_core::protocol::InProcessStream,
    pump: &mut ContentPump,
    gpu: &GpuContext,
    views: &[ViewState],
    origin: DVec3,
) {
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let _ = server.as_mut().poll(&mut cx);
    pump.advance_with(stream, gpu, views, origin);
    let mut quiet = 0;
    for _ in 0..2_000 {
        let _ = server.as_mut().poll(&mut cx);
        let uploaded = pump.pump(stream, gpu, tuile_wgpu::UPLOADS_PER_FRAME);
        let done = !pump.selection.is_empty()
            && pump.provisional() == 0
            && pump.pending_uploads() == 0;
        quiet = if uploaded == 0 && done { quiet + 1 } else { 0 };
        if quiet > 8 {
            break;
        }
    }
}

/// Which longitudinal halves hold prepared tiles finer than the pin.
///
/// Level-`z` terrain tiling has `2·2^z` columns; the western half of the world
/// is `x < 2^z`. The pin keeps coarse levels resident everywhere, so only
/// levels below the pin discriminate between "this ground was loaded for a
/// view" and "this ground is pinned".
fn fine_halves(pump: &ContentPump) -> (bool, bool) {
    let (mut west, mut east) = (false, false);
    for (tile, _) in &pump.selection {
        let (z, x, _) = tile.terrain_coord();
        if z <= PINNED {
            continue;
        }
        if x < (1 << z) {
            west = true;
        } else {
            east = true;
        }
    }
    (west, east)
}

#[test]
fn a_second_view_loads_its_ground_too() {
    let Some(gpu) = gpu() else { return };
    let tree: Box<dyn TileTree> = tuile_planetary::terrain_tree(everywhere());
    let loader: Arc<dyn TileLoader> = Arc::new(Instant);
    let config = Config {
        pinned_level: Some(PINNED),
        ..Config::default()
    };
    let (mut stream, server) = in_process_with(tree, loader, config);
    let mut server: Pin<Box<dyn Future<Output = ()>>> = Box::pin(server.run());

    // One camera over lon −90 (west), one over lon +90 (east) — disjoint
    // ground by construction.
    let (west_view, east_view) = (over(-90.0), over(90.0));
    let origin = tuile_core::traversal::ViewStateParams::from(west_view).position;
    let mut pump = ContentPump::new(origin);

    // A single view first: only its own half refines below the pin — the
    // baseline that proves the union assertion below is not vacuous.
    settle(&mut server, &mut stream, &mut pump, &gpu, &[west_view], origin);
    let (west, east) = fine_halves(&pump);
    assert!(west, "the west view refined its own ground");
    assert!(!east, "nothing east of the pin was asked for by a west view");

    // Both views together: the union loads the east eagerly as well, and the
    // pump's own gate (`missing == 0`) now covers both.
    settle(
        &mut server,
        &mut stream,
        &mut pump,
        &gpu,
        &[west_view, east_view],
        origin,
    );
    let (west, east) = fine_halves(&pump);
    assert!(
        west && east,
        "two views must refine both grounds (west={west}, east={east})"
    );
    assert_eq!(pump.missing(), 0, "the union settled completely");
    assert_eq!(
        pump.provisional(),
        0,
        "nothing selected is a stand-in — real ground everywhere"
    );
}
