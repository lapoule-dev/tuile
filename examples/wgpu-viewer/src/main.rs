// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Streaming globe viewer: a winit window driving the logical geometry server
//! in **progressive** mode. The server (terrain × Bing imagery via ion) runs
//! on a background tokio runtime; the window, on the main thread, sends the
//! camera every frame and pumps decoded tiles to the GPU as they arrive —
//! tiles stream in one by one, refining as you orbit and zoom.
//!
//! ```text
//! cargo run -p wgpu-viewer        # reads CESIUM_ION_TOKEN (env or .env)
//! ```
//! Drag: orbit. Right-drag: pan. Wheel: zoom. W: wireframe. F: freeze. Esc: quit.

mod app;

use app::{App, ViewerConfig};
use std::sync::Arc;
use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_camera::{CameraController, GlobeCamera};
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource};
use tuile_native_fetchers::NativeHttp;
use tuile_core::runtime::in_process_with;
use tuile_core::source::{TileLoader, TileTree};
use tuile_core::traversal::Config;
use tuile_planetary::{globe, GlobeOptions, ImageryDetail};
use winit::event_loop::{ControlFlow, EventLoop};

/// Resolves the Cesium-ion globe sources and crosses them through the
/// backend-agnostic `tuile-planetary`. The app decides ion + the native HTTP
/// transport here, not planetary.
async fn ion_globe(
    token: String,
) -> anyhow::Result<(Box<dyn TileTree>, Arc<dyn TileLoader>, ImageryDetail)> {
    // One pooled, cached native transport drives both ion and Bing.
    let http = Arc::new(NativeHttp::shared().await?);
    let terrain = IonTerrainSource::new(IonClient::new(Arc::clone(&http), token.clone()), 1);
    let layer = terrain.layer().await?;

    let ion2 = IonClient::new(Arc::clone(&http), token);
    let endpoint = match ion2.asset_endpoint(2).await? {
        AssetEndpoint::Imagery(e) => e,
        _ => anyhow::bail!("ion asset 2 is not imagery"),
    };
    let o = &endpoint.options;
    let meta_url = BingMetadata::metadata_url(
        o.url.as_deref().ok_or_else(|| anyhow::anyhow!("bing url"))?,
        o.map_style.as_deref().unwrap_or("Aerial"),
        o.key.as_deref().ok_or_else(|| anyhow::anyhow!("bing key"))?,
    );
    let bing = BingImageryProvider::from_metadata_url(Arc::clone(&http), &meta_url)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(globe(terrain, bing, layer, GlobeOptions::default()))
}

/// Logs to stderr; `RUST_LOG` overrides. Default shows tile streaming
/// (`tuile_planetary=debug`) plus app-level info.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tuile_planetary=debug".into()),
        )
        .without_time()
        .with_target(false)
        .init();
}

fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();
    let token = std::env::var("CESIUM_ION_TOKEN")
        .map_err(|_| anyhow::anyhow!("set CESIUM_ION_TOKEN (env or .env)"))?;

    // The geometry server is async (ion fetches over reqwest): build the scene
    // and run the server on a background multi-thread runtime. The window
    // talks to it over the in-process stream (channels, Send across threads).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (tree, loader, detail) = rt.block_on(ion_globe(token))?;
    let config = Config {
        maximum_screen_space_error: 2.0,
        maximum_simultaneous_fetches: 64,
        resident_budget_bytes: 2 << 30,
        ..Config::default()
    };
    let (stream, server) = in_process_with(tree, loader, config);
    std::thread::Builder::new()
        .name("geometry-server".into())
        .spawn(move || rt.block_on(server.run()))?;

    // Start looking straight down at France from ~2000 km up.
    let camera = GlobeCamera::from_geodetic(
        46f64.to_radians(),
        2f64.to_radians(),
        2_000_000.0,
        0.0,
        std::f64::consts::FRAC_PI_2,
        60f64.to_radians(),
    );
    let controller = CameraController::new(camera).with_min_altitude(150.0);

    tracing::info!(
        "tuile globe viewer — streaming Cesium World Terrain + Bing via ion\n\
         drag: pan globe · right-drag: tilt/heading · wheel: zoom · W: wireframe · F: freeze · Esc"
    );
    let app_config = ViewerConfig {
        stream,
        controller,
        detail,
        title: "tuile — globe (streaming)".into(),
    };

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(app_config);
    event_loop.run_app(&mut app)?;
    Ok(())
}
