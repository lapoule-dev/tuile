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
use tuile_core::raster::CachedImagery;
use tuile_core::runtime::in_process_with;
use tuile_core::source::{TileLoader, TileTree};
use tuile_core::storage::ContentStore;
use tuile_core::traversal::Config;
use tuile_native_fetchers::NativeHttp;
use tuile_planetary::{globe, GlobeOptions, ImageryDetail};
use tuile_storage_foyer::FoyerStore;
use tuile_terrain::{CachedTerrain, TerrainHeights};
use winit::event_loop::{ControlFlow, EventLoop};

/// Resolves the Cesium-ion globe sources and crosses them through the
/// backend-agnostic `tuile-planetary`. The app decides ion + the native HTTP
/// transport here, not planetary.
async fn ion_globe(
    token: String,
) -> anyhow::Result<(
    Box<dyn TileTree>,
    Arc<dyn TileLoader>,
    ImageryDetail,
    Arc<TerrainHeights>,
    Option<Arc<FoyerStore>>,
)> {
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
        o.url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("bing url"))?,
        o.map_style.as_deref().unwrap_or("Aerial"),
        o.key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("bing key"))?,
    );
    let bing = BingImageryProvider::from_metadata_url(Arc::clone(&http), &meta_url)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // One store, two tiers of caller: the terrain source and the imagery
    // provider each keep their own bytes in it, keyed by tile rather than by
    // URL. The server evicts decoded tiles to stay inside its GPU budget, and
    // this is what makes coming back to them cheap. A store that fails to open
    // is not worth failing the app over.
    let store = match FoyerStore::shared("tiles").await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("no tile store ({e}); every tile will be re-fetched");
            None
        }
    };

    let Some(store) = store else {
        let (tree, loader, detail, heights) = globe(terrain, bing, layer, GlobeOptions::default());
        return Ok((tree, loader, detail, heights, None));
    };
    let shared = Arc::clone(&store) as Arc<dyn ContentStore>;
    let (tree, loader, detail, heights) = globe(
        CachedTerrain::new(terrain, Arc::clone(&shared), "ion-cwt"),
        CachedImagery::new(bing, shared, "bing-aerial"),
        layer,
        GlobeOptions::default(),
    );
    Ok((tree, loader, detail, heights, Some(store)))
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

/// The instant the scene is lit for, UTC seconds since the Unix epoch.
///
/// `TUILE_LIT_AT` overrides it, so a session can be pinned to a stated moment —
/// which is the only way two runs, or two machines, can be compared. Without it
/// the answer is "now", and "now" is never the same twice.
///
/// Seconds rather than a formatted date because this crate has no calendar in
/// it and adding one to parse a debugging knob would be the wrong trade. `date
/// -u -d '2024-06-21 06:00' +%s` produces the number.
fn lit_at() -> anyhow::Result<f64> {
    if let Ok(pinned) = std::env::var("TUILE_LIT_AT") {
        let seconds: f64 = pinned
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("TUILE_LIT_AT must be UTC seconds, got {pinned:?}"))?;
        tracing::info!("scene lit for the instant TUILE_LIT_AT={seconds}");
        return Ok(seconds);
    }
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64())
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
    // The runtime moves to the server thread; keep a handle so the store can
    // still be flushed from here on the way out.
    let handle = rt.handle().clone();
    let (tree, loader, detail, heights, store) = rt.block_on(ion_globe(token))?;
    // The budget counts decoded CPU bytes; the GPU copy costs about 1.35× that
    // (mip chains, interleaved vertices), measured — so 3 GiB here is ~4 GiB of
    // GPU, which unified memory carries comfortably now that a drape is a
    // quarter of what it was.
    //
    // Sized above the working set on purpose. `forbid_holes` loads a whole
    // subtree before selecting any of it, and a tile loaded but not yet
    // selected is protected by nothing: squeeze the budget below what a view
    // needs and those tiles are evicted and re-requested forever. At 768 MiB a
    // motionless camera still churned twenty tiles a second — the session never
    // settles, and it reads as the app hanging.
    let config = Config {
        maximum_screen_space_error: 2.0,
        maximum_simultaneous_fetches: 64,
        resident_budget_bytes: 3072 * 1024 * 1024,
        ..Config::default()
    };
    let (stream, server) = in_process_with(tree, loader, config);
    let server_thread = std::thread::Builder::new()
        .name("geometry-server".into())
        .spawn(move || rt.block_on(server.run()))?;

    // Start looking straight down at France from ~2000 km up.
    let camera = GlobeCamera::from_geodetic(
        46f64.to_radians(),
        2f64.to_radians(),
        2_000_000.0,
        0.0,
        std::f64::consts::FRAC_PI_2,
        tuile_camera::DEFAULT_GLOBE_FOVY,
    );
    // Clamp against the terrain, not the ellipsoid: 150 m over the sea and
    // 150 m over a summit are the same request, and only the relief tells them
    // apart. The handle is shared and live, so the floor sharpens as tiles land.
    let controller = CameraController::new(camera)
        .with_min_altitude(150.0)
        .with_ground(heights);

    tracing::info!(
        "tuile globe viewer — streaming Cesium World Terrain + Bing via ion\n\
         drag: pan globe · right-drag: tilt/heading · wheel: zoom · W: wireframe · F: freeze · Esc"
    );
    let app_config = ViewerConfig {
        stream,
        controller,
        detail,
        title: "tuile — globe (streaming)".into(),
        lit_at_unix_seconds: lit_at()?,
    };

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(app_config);
    let outcome = event_loop.run_app(&mut app);
    let report = app.report();

    // Shut down in dependency order. The server writes into the store, so
    // closing the store first leaves its flusher shouting into a closed channel
    // — thousands of lines of it, and nothing persisted. Dropping the app drops
    // the client stream, which is how the server learns the session is over.
    drop(app);
    if server_thread.join().is_err() {
        tracing::error!("the geometry server panicked; its last work is lost");
    }
    if let Some(store) = store {
        match handle.block_on(store.close()) {
            Ok(()) => tracing::info!("tile store flushed"),
            Err(e) => tracing::warn!("tile store not flushed ({e}); the next run starts cold"),
        }
    }
    tracing::info!("{report}");
    outcome?;
    Ok(())
}
