// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Streaming globe viewer: a winit window driving the logical geometry server
//! in **progressive** mode. The server (terrain × Bing imagery via ion) runs
//! on a background tokio runtime; the window, on the main thread, sends the
//! camera every frame and pumps decoded tiles to the GPU as they arrive —
//! tiles stream in one by one, refining as you orbit and zoom.
//!
//! ```text
//! CESIUM_ION_TOKEN=... cargo run -p wgpu-viewer
//! ```
//! Drag: orbit. Right-drag: pan. Wheel: zoom. W: wireframe. F: freeze. Esc: quit.
//!
//! `TUILE_RECORD=path.jsonl` writes the camera path; `TUILE_REPLAY=path.jsonl`
//! flies it again exactly. The format and the replay live in the `tuile-tape`
//! crate.

mod app;
mod backdrop;
mod recording;
mod session;
mod settings;
mod signals;
mod sources;

use app::{App, ViewerConfig};
use tuile_camera::{CameraController, GlobeCamera};
use tuile_core::runtime::in_process_with;
use tuile_core::traversal::Config;
use winit::event_loop::{ControlFlow, EventLoop};

fn main() -> anyhow::Result<()> {
    settings::init_tracing();
    // From the environment, and only from the environment.
    //
    // A `.env` file used to be read here as well. That is convenient exactly
    // once and misleading afterwards: the process then behaves differently
    // depending on the directory it was launched from, a stale file silently
    // wins over the variable that was deliberately exported, and a credential
    // ends up sitting in the working tree where it is one `git add -A` away from
    // being published. What a session ran with should be visible in the command
    // that started it.
    let token = std::env::var("CESIUM_ION_TOKEN").map_err(|_| {
        anyhow::anyhow!(
            "no CESIUM_ION_TOKEN in the environment — export it, or prefix the \
             command: CESIUM_ION_TOKEN=... cargo run -p wgpu-viewer"
        )
    })?;

    // The geometry server is async (ion fetches over reqwest): build the scene
    // and run the server on a background multi-thread runtime. The window
    // talks to it over the in-process stream (channels, Send across threads).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // The runtime moves to the server thread; keep a handle so the store can
    // still be flushed from here on the way out.
    let handle = rt.handle().clone();
    let (tree, loader, detail, heights, store, layer_budget) =
        rt.block_on(sources::ion_globe(token))?;
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
    // The reference session, shared with the headless tests rather than written
    // out here. When these numbers lived only in this file, every test invented
    // its own and a harness that never evicted passed while the globe went
    // black — see `Config::interactive_globe`.
    let config = Config {
        // TUILE_NO_CULL=1 keeps every tile the traversal reaches, however far
        // off screen. Expensive and not a mode anyone should run in — it exists
        // to answer one question: whether geometry that is missing was culled.
        cull: std::env::var("TUILE_NO_CULL").is_err(),
        ..Config::interactive_globe(settings::pinned_level())
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
    // Clamp against the terrain, not the ellipsoid: a metre over the sea and a
    // metre over a summit are the same request, and only the relief tells them
    // apart. The handle is shared and live, so the floor sharpens as tiles land.
    //
    // One metre, not the hundred and fifty it was. A floor that high is a
    // helicopter: it puts the eye above everything a person might want to stand
    // next to, and at the levels the source actually serves there is detail well
    // below it. The near plane follows — it is a quarter of the clearance — so
    // getting close costs nothing but the precision that rebasing already
    // provides.
    let controller = CameraController::new(camera)
        .with_min_altitude(1.0)
        .with_ground(heights);

    tracing::info!(
        "tuile globe viewer — streaming Cesium World Terrain + Bing via ion\n\
         drag: pan globe · right-drag: tilt/heading · wheel: zoom · W: wireframe · F: freeze · Esc"
    );
    let app_config = ViewerConfig {
        stream,
        controller,
        detail,
        layer_budget,
        title: "tuile — globe (streaming)".into(),
        lit_at_unix_seconds: settings::lit_at()?,
    };

    // Everything the engine counts, scrapeable, so the log can stop being a
    // wall of numbers and go back to reporting events.
    tuile_metrics::serve(&handle);

    // Before the loop: Ctrl-C and `kill` must end the session, not the process,
    // or a recording dies with it.
    signals::catch_interruptions();
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(app_config);
    let outcome = event_loop.run_app(&mut app);
    // Belt and braces: `exiting` covers a loop that unwinds normally, this
    // covers one that does not. Closing an already-closed tape is a no-op.
    app.close_the_tape();
    let report = app.report();

    // Shut down in dependency order. Dropping the app drops the client stream,
    // which is how the server learns the session is over.
    drop(app);
    // The store is closed *before* the join, not after.
    //
    // The server thread owns the tokio runtime and `block_on`s the session on
    // it, so when `run()` returns the runtime drops on that thread — and
    // `Runtime::drop` waits for the store's blocking workers to finish. Waiting
    // for the thread first and only then asking the store to close is a
    // deadlock by construction: the thread cannot finish until a `close()` that
    // cannot be issued until it has. It survives only because `run()` normally
    // outlives this point.
    settings::close_the_store(store, &handle);
    if server_thread.join().is_err() {
        tracing::error!("the geometry server panicked; its last work is lost");
    }
    tracing::info!("{report}");
    outcome?;
    Ok(())
}
