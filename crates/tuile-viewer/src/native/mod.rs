// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The desktop and phone shell: the command line, the async runtime with its
//! threads, the tile store on disk, the counters' endpoint, the signals —
//! everything around the shared application that a browser does not have.

#[cfg(target_os = "android")]
pub mod android;
// What the Android shell reads from the system, decoded: pure, and tested on
// the host.
#[cfg(any(target_os = "android", test))]
pub(crate) mod android_signals;
mod configured;
pub(crate) mod launch;
mod open;
pub(crate) mod settings;
pub(crate) mod store;

use std::sync::Arc;

pub(crate) use launch::{Budget, Gate, Started};

use crate::app::{App, ViewerConfig};
use crate::embed::{self, ViewerHost};
use crate::{desktop, host, journal, location, profile, signals, start, steer};
use tuile_camera::CameraController;
use tuile_core::traversal::Config;
use winit::event_loop::{ControlFlow, EventLoop};

/// Keeps `link` as the view the next session started from an icon opens on —
/// a file in the application's support directory — or, with `None`, removes
/// the one kept before.
pub(crate) fn keep_the_view(link: Option<&str>) {
    let Some(file) = host::last_view_file(&host::Machine) else {
        return;
    };
    let Some(link) = link else {
        let _ = std::fs::remove_file(&file);
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&file, format!("{link}\n"));
}

/// Keeps `on`, the 3D Tiles tilesets a person switched on, for the next
/// session — a file in the application's support directory, removed when
/// none is on.
pub(crate) fn keep_tilesets_on(on: &[i64]) {
    let Some(file) = host::tilesets_on_file(&host::Machine) else {
        return;
    };
    if on.is_empty() {
        let _ = std::fs::remove_file(&file);
        return;
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line: Vec<String> = on.iter().map(i64::to_string).collect();
    let _ = std::fs::write(&file, format!("{}\n", line.join(",")));
}

/// The 3D Tiles tilesets the last session left switched on. A file that
/// does not read as asset numbers is no choice at all.
pub(crate) fn tilesets_kept_on() -> Vec<i64> {
    host::tilesets_on_file(&host::Machine)
        .and_then(|file| std::fs::read_to_string(file).ok())
        .and_then(|text| crate::start::asset_list("tilesets-on", text.trim()).ok())
        .unwrap_or_default()
}

/// Whether this process was started from an application bundle — from an
/// icon, most likely, with no terminal behind it.
fn in_a_bundle() -> bool {
    std::env::current_exe().is_ok_and(|exe| host::is_bundled(&exe))
}

/// Runs the application for `host`, from the command line to the last flush
/// of the tile store, and ends the process.
///
/// This is a whole `main`: it reads the flags, opens the journal, asks the
/// host for its sources, opens the window and runs the event loop on the
/// calling thread — which must be the main one, as every windowing system
/// requires. Exit code 0 for a session that ended, 1 for one that failed, 2
/// for a command line that was not understood.
pub fn main(embedder: impl ViewerHost) -> ! {
    // The command line first, before the logger, the token, the network and
    // the window: a mistyped flag must cost nothing but the message that says
    // so. An Android application has no command line: its process is forked
    // from the system's, whose arguments are not the viewer's. Its knobs
    // arrive as launch settings instead (see `android`).
    let arguments: Vec<String> = if cfg!(target_os = "android") {
        Vec::new()
    } else {
        std::env::args().skip(1).collect()
    };
    let mut invocation = start::parse(&arguments);
    // The configuration: `--config`'s file, else the desktop's default one,
    // else what a file opened in the application kept. Its terrain and
    // layers take the host's place, and its start view fills in what the
    // flags did not say — a flag wins over the file.
    let identity = embedder.identity();
    let flag = match &invocation {
        Ok(start::Invocation::Run { config, .. }) => config.clone(),
        _ => None,
    };
    let configured = match crate::import::start(
        &identity.config_section,
        flag.as_deref(),
        &host::Machine,
        &identity.name,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("{}: {error}", identity.executable);
            if in_a_bundle() {
                desktop::alert("The configuration file cannot be read", &error.to_string());
            }
            std::process::exit(2);
        }
    };
    if let Ok(start::Invocation::Run { view, .. }) = &mut invocation {
        start::fill_in(view, &arguments, &configured.start);
    }
    // Sources the command line named by address take the host's place —
    // before the layers are adopted, since a template is then the only one.
    let addresses = match &invocation {
        Ok(start::Invocation::Run { addresses, .. }) => addresses.clone(),
        _ => start::Addresses::default(),
    };
    let embedder = configured::Configured::wrap(Arc::new(embedder), configured);
    let embedder = open::Addressed::wrap(embedder, addresses);
    embed::adopt(embedder.identity(), embedder.imagery());
    journal::open();
    if let Err(error) = session(embedder, invocation) {
        journal::note(&format!("session failed: {error:#}"));
        // From an icon, stderr is the system log and nobody is reading it: an
        // application that fails must say so in a window, or it has simply
        // not opened. This is not hypothetical — the first bundle did exactly
        // that, over a limit on open files that no terminal session has.
        if in_a_bundle() {
            desktop::alert(
                &format!("{} stopped", host::app_name()),
                &format!("{error:#}"),
            );
        } else {
            eprintln!("Error: {error:?}");
        }
        std::process::exit(1);
    }
    std::process::exit(0)
}

/// The event loop — on macOS, without the windowing layer's stock menu bar:
/// the application brings its own (`menu`), and two would be one too many.
fn event_loop() -> Result<EventLoop<()>, winit::error::EventLoopError> {
    // Android: bound to the activity the system started.
    #[cfg(target_os = "android")]
    return android::event_loop();
    #[cfg(all(target_os = "macos", feature = "application"))]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        EventLoop::builder().with_default_menu(false).build()
    }
    #[cfg(not(any(
        all(target_os = "macos", feature = "application"),
        target_os = "android"
    )))]
    EventLoop::new()
}

/// Lifts this process's limit on open files as far as the system allows.
///
/// A process started by the desktop gets 256 descriptors, where a shell hands
/// out thousands. The tile store and a pool of connections go through 256
/// before the first frame, and the session then dies of "too many open files"
/// — from an icon only, which is the one place nobody sees the message. The
/// soft limit is the process's own to raise up to the hard one, so it is
/// raised; failing to is worth a line and not worth stopping for.
fn lift_the_open_file_limit() {
    #[cfg(unix)]
    if let Err(error) = rlimit::increase_nofile_limit(8192) {
        tracing::error!("could not raise the open-file limit: {error}");
    }
}

fn session(
    embedder: Arc<dyn ViewerHost>,
    invocation: Result<start::Invocation, String>,
) -> anyhow::Result<()> {
    // The command line, read by `main`. Exit code 2 is the usual one for a
    // command that was not understood.
    let (view, here, imagery, compression, clock, max_altitude, record, tilesets) = match invocation
    {
        Ok(start::Invocation::Run {
            view,
            here,
            imagery,
            compression,
            clock,
            max_altitude,
            record,
            tilesets,
            ..
        }) => (
            view,
            here,
            imagery,
            compression,
            clock,
            max_altitude,
            record,
            tilesets,
        ),
        Ok(start::Invocation::Help) => {
            println!("{}", start::usage());
            return Ok(());
        }
        Err(error) => {
            eprintln!(
                "{}: {error}\n\n{}",
                embed::identity().executable,
                start::usage()
            );
            std::process::exit(2);
        }
    };
    // Which layer `--imagery` meant is the host's list to say, and a name
    // that is not on it is a command that was not understood — said here,
    // with the list, before anything is connected to.
    let layers = embed::layers();
    let opening = match imagery
        .as_deref()
        .map(|name| steer::layer_named(layers, name))
    {
        Some(Ok(index)) => index,
        Some(Err(why)) => {
            eprintln!("{}: {why}", embed::identity().executable);
            std::process::exit(2);
        }
        None => 0,
    };
    // Started from an icon, there is no terminal: the log goes quiet and
    // what would have been an error line becomes a window. A phone's
    // application is only ever started from an icon.
    let bundled = in_a_bundle() || cfg!(any(target_os = "ios", target_os = "android"));
    // Started from an icon with nothing said about where: open where the last
    // such session ended. From a terminal the default view stays the default
    // — a command opens on what the command says.
    let view = if bundled && !here && view == start::StartView::default() {
        start::remembered(&host::Machine).unwrap_or(view)
    } else {
        view
    };
    settings::init_tracing(bundled);
    lift_the_open_file_limit();
    // How the imagery is held on the GPU: the person's choice — the flag,
    // else the phone's settings page — and off when they made none.
    let compression = match settings::imagery_compression(compression, &host::Machine) {
        Ok(choice) => choice,
        Err(why) => {
            eprintln!("{}: {why}", embed::identity().executable);
            std::process::exit(2);
        }
    };
    // The 3D Tiles drawn over the globe: the flag, else the settings page,
    // else TUILE_TILESETS; none when nothing says.
    let tilesets = match settings::tilesets(tilesets, &host::Machine) {
        Ok(tilesets) => tilesets,
        Err(why) => {
            eprintln!("{}: {why}", embed::identity().executable);
            std::process::exit(2);
        }
    };
    // Those on offer, each a switch; and those on: what was asked for, else
    // the phone's settings switch, else the person's choice last time —
    // none, until they turn one on.
    let offered = embedder.tilesets();
    let on = settings::tilesets_on(tilesets, &offered, &host::Machine, tilesets_kept_on);
    // `TUILE_BACKEND=gl` forces one — a launch setting on Android.
    let backends = crate::app::backend_named(std::env::var("TUILE_BACKEND").ok().as_deref())
        .map_err(anyhow::Error::msg)?;
    // What that comes to on this machine's GPU, asked before there is a
    // window, so the first tiles decoded are already held as chosen; the
    // device confirms it when the window brings it up.
    let (imagery_plan, stored_imagery) =
        pollster::block_on(tuile_wgpu::adapter_plan(compression, backends));
    tracing::info!(
        ?compression,
        held = tuile_wgpu::format_name(imagery_plan.target),
        "imagery compression"
    );
    // How hard this machine may be pushed: the desktop's numbers, or a
    // phone's, sized from its memory. See `profile`.
    let mut profile = profile::Profile::current();
    tracing::info!(
        gpu_mib = profile.gpu_bytes >> 20,
        local_reads = profile.max_local_reads,
        decode_threads = profile.decode_threads,
        runtime_threads = profile.runtime_workers,
        "profile"
    );
    // Before the loop exists, so that a URL which *launched* the application
    // is not missed.
    let requests = desktop::listen();

    // The geometry server is async (fetches over reqwest): the scene is built
    // and the server run on a background multi-thread runtime, bounded by
    // the profile — a phone gets two workers, not one per core. The window
    // talks to the server over the in-process stream (channels).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(profile.runtime_workers)
        .max_blocking_threads(profile.blocking_threads)
        .thread_name("tuile-runtime")
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();
    // What the session may hold, load and upload: the graphics budget, sized
    // from the device by the profile — a desktop's quarter of the machine, a
    // phone's share of what the system leaves the process. The floor it pins
    // (`TUILE_PIN_LEVEL` tunes it) is loaded first and never evicted.
    let mut budget = profile.budget();
    budget.pinned_level = settings::pinned_level(profile.pinned_level);
    let config = Config {
        // TUILE_NO_CULL=1 keeps every tile the traversal reaches, however far
        // off screen. Expensive and not a mode anyone should run in — it exists
        // to answer one question: whether geometry that is missing was culled.
        cull: std::env::var("TUILE_NO_CULL").is_err(),
        // And TUILE_NO_HORIZON_CULL=1 keeps the far side of the planet, which
        // the frustum alone cannot drop: from near the ground the frustum runs
        // straight through the Earth. Same purpose as the flag above — when
        // ground is missing, each of the two things that can remove it has to
        // be switchable off on its own.
        horizon_culling: std::env::var("TUILE_NO_HORIZON_CULL").is_err(),
        ..profile.traversal()
    };
    // Everything between here and a running server is network, and runs on
    // the runtime: see `launch`. The host's credentials and sources, the
    // store, the globe.
    let plan = launch::Plan {
        host: Arc::clone(&embedder),
        terrain: embedder.terrain(),
        layers,
        opening,
        store: tuile_storage_foyer::default_cache_dir().join(&embed::identity().store),
        config,
        budget: budget.clone(),
        profile: profile.clone(),
        timeout: crate::launch::START_TIMEOUT,
        imagery_plan,
        stored_imagery,
        // Only those on: one switched off costs nothing, its root not even
        // asked for.
        tilesets: on.clone(),
    };
    let launch = launch::start(&handle, plan.clone());
    // A phone tries again by itself; a desktop says why and stops, as it
    // always has — its command line is how it is tried again.
    let retry = {
        let handle = handle.clone();
        move || crate::launch::Retries::new(move || launch::start(&handle, plan.clone()))
    };
    let mut retries = None;
    // A desktop waits for it here, with no window yet: its window is held
    // shut until the globe has a floor anyway, and a refusal is a sentence
    // for the person — shown in a window when there is no terminal. A phone
    // opens its window now and collects the launch from its loop, drawing the
    // loading indicator meanwhile.
    //
    // Started from an icon, a desktop that cannot start — no token yet, most
    // often — does not quit: it opens its window as a phone does, says why,
    // and tries again by itself and as soon as a configuration file is
    // opened (File > Open, a double-click, a link).
    let launch = if profile.hold_window_until_warm {
        match launch.wait() {
            Ok(started) => crate::launch::Launch::finished(Ok(started)),
            Err(why) if bundled => {
                profile.hold_window_until_warm = false;
                retries = Some(retry());
                crate::launch::Launch::finished(Err(why))
            }
            Err(why) => anyhow::bail!("{why}"),
        }
    } else {
        retries = Some(retry());
        launch
    };

    // Where the command line said; straight down at France from ~2000 km up
    // when it said nothing.
    let camera = view.camera();
    // Clamped against the terrain, not the ellipsoid, once the globe is up:
    // a metre over the sea and a metre over a summit are the same request,
    // and only the relief tells them apart. The handle is shared and live, so
    // the floor sharpens as tiles land — see `App::adopt`.
    //
    // One metre, not the hundred and fifty it was. A floor that high is a
    // helicopter: it puts the eye above everything a person might want to stand
    // next to, and at the levels the source actually serves there is detail well
    // below it. The near plane follows — it is a quarter of the clearance — so
    // getting close costs nothing but the precision that rebasing already
    // provides.
    //
    // And under the ceiling, the camera's own unless `--max-altitude` says
    // otherwise: an `--altitude` above it opens at it.
    let controller = CameraController::new(camera)
        .with_min_altitude(1.0)
        .with_max_altitude(max_altitude.unwrap_or_else(tuile_camera::default_max_altitude));

    tracing::info!(
        "globe viewer — streaming terrain and imagery\n\
         drag: pan globe · right-drag: tilt/heading · wheel: zoom · N: north up · \
         L: current location · C: copy link · I: next imagery · W: wireframe · \
         F: freeze · Esc"
    );
    let app_config = ViewerConfig {
        controller,
        // The viewer's own binary is nobody's host: the person at the keyboard
        // owns this camera.
        frame_host: None,
        title: host::app_name().into(),
        layers,
        layer: opening,
        requests,
        remembers: bundled,
        // `--here`: the window opens on the view above, and moves over the
        // current location — same altitude, heading and pitch — when the
        // system says where that is.
        here: here.then_some(location::Placement {
            altitude: Some(view.altitude),
            heading: view.heading,
            pitch: view.pitch,
        }),
        clock: settings::clock(clock)?,
        air: tuile_atmosphere::AirTuning::from_env()?,
        pinned_level: budget.pinned_level,
        profile: profile.clone(),
        retries,
        window: winit::window::Window::default_attributes(),
        controls: true,
        backends,
        imagery_compression: compression,
        record,
        tilesets: offered,
        tilesets_on: on.clone(),
        keeps_tilesets: true,
    };

    // Everything the engine counts, scrapeable, so the log can stop being a
    // wall of numbers and go back to reporting events. Not on a phone: an
    // HTTP listener is a thread and a socket nobody there can reach.
    if profile.metrics_endpoint {
        tuile_metrics::serve(&handle);
    }

    // Before the loop: Ctrl-C and `kill` must end the session, not the process,
    // or a recording dies with it. A phone's application is never sent
    // either, and could not leave its loop if it were.
    if !cfg!(any(target_os = "ios", target_os = "android")) {
        signals::catch_interruptions();
    }
    let event_loop = event_loop()?;
    desktop::adopt_the_loop();
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(app_config, launch);
    // On a phone this never returns: the system owns the loop, and the
    // session ends in `App::exiting`, or not at all.
    let outcome = event_loop.run_app(&mut app);
    // Belt and braces: `exiting` covers a loop that unwinds normally, this
    // covers one that does not. Closing an already-closed tape is a no-op.
    app.close_the_tape();
    let report = app.report();

    // Shut down in dependency order. The store is closed *before* the server
    // thread is joined: the thread blocks on the runtime, and the runtime
    // waits for the store's blocking workers — waiting for the thread first
    // and only then asking the store to close is a deadlock by construction.
    app.shut_down();
    drop(app);
    tracing::info!("{report}");
    drop(rt);
    outcome?;
    Ok(())
}
