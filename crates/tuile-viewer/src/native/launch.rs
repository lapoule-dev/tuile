// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The desktop's and the phone's session: the geometry server on a thread of
//! its own, blocking on the async runtime, its loads behind the tile store on
//! disk.
//!
//! The whole sequence is one task on the runtime, started by [`start`]: the
//! host's credentials and sources, the store, the globe, the server. A
//! desktop waits for it before opening its window ([`Launch::wait`]); a phone
//! opens at once and collects it from its loop.
//!
//! [`Launch::wait`]: crate::launch::Launch::wait

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use tuile_core::protocol::InProcessStream;
use tuile_core::source::TileLoader;
use tuile_core::storage::ContentStore;
use tuile_planetary::LayerBudget;
use tuile_storage_foyer::SuspendableStore as FoyerStore;
use tuile_terrain::TerrainHeights;

use crate::embed::{ImageryChoice, TerrainChoice, ViewerHost};
use crate::gate::Gated;
use crate::globe::{Layers, Switched};
use crate::launch::{Launch, Outcome};

/// What the window declares the device's imagery slots through.
pub(crate) type Budget = LayerBudget;
/// What the session's loads wait on while the application is away.
pub(crate) type Gate = crate::gate::Gate;

/// A session that is up: its stream, and everything the window adopts from
/// the globe.
pub(crate) struct Started {
    pub stream: InProcessStream,
    /// The thread the geometry server runs on, to be joined on the way out.
    pub server: Option<std::thread::JoinHandle<()>>,
    /// The budget's signals, shared with the window's pump.
    pub meter: tuile_core::runtime::SharedMeter,
    pub heights: Arc<TerrainHeights>,
    pub layer_budget: Budget,
    /// How the loader holds imagery: told the device's plan once it exists.
    pub imagery_plan: tuile_planetary::ImageryPlanHandle,
    pub switcher: Switcher,
    /// Kept so it can be flushed on the way out: a store that is merely
    /// dropped leaves the disk as cold as it found it.
    pub store: Option<Arc<FoyerStore>>,
    pub gate: Gate,
    /// The runtime the store is flushed on.
    pub runtime: tokio::runtime::Handle,
    /// The 3D Tiles drawn over the globe, until the window takes them.
    pub surfaces: Vec<crate::surface::Surface>,
    /// Why a tileset asked for is not among them, a sentence each.
    pub surface_failures: Vec<String>,
    /// Their servers' threads, joined on the way out.
    pub surface_servers: Vec<std::thread::JoinHandle<()>>,
}

impl Started {
    /// The 3D Tiles drawn over the globe, and why any asked for is not, for
    /// the window to take once.
    pub(crate) fn take_surfaces(&mut self) -> (Vec<crate::surface::Surface>, Vec<String>) {
        (
            std::mem::take(&mut self.surfaces),
            std::mem::take(&mut self.surface_failures),
        )
    }

    /// How the loader holds imagery, for the window to tell it the device's
    /// plan.
    pub(crate) fn imagery_plan(&self) -> Option<&tuile_planetary::ImageryPlanHandle> {
        Some(&self.imagery_plan)
    }

    /// The budget's signals, shared with the session.
    pub(crate) fn meter(&self) -> Option<tuile_core::runtime::SharedMeter> {
        Some(std::sync::Arc::clone(&self.meter))
    }

    /// Closes the session down: the stream, then the store, then the server.
    ///
    /// The store is closed *before* the server is joined: the server thread
    /// blocks on the runtime, and the runtime waits for the store's blocking
    /// workers — so waiting for the thread first and only then asking the
    /// store to close is a deadlock by construction. `join_the_server` is a
    /// desktop's: a phone on its way out is given seconds for all of it, and
    /// does not wait for the server.
    pub(crate) fn close(self, patience: std::time::Duration, join_the_server: bool) {
        let Started {
            stream,
            server,
            store,
            runtime,
            surfaces,
            surface_servers,
            ..
        } = self;
        drop(stream);
        // A tileset's server ends when its stream does; the window has
        // dropped the ones it took.
        drop(surfaces);
        crate::native::store::close(store, &runtime, patience);
        if join_the_server {
            if let Some(server) = server {
                if server.join().is_err() {
                    tracing::error!("the geometry server panicked; its last work is lost");
                }
            }
            for server in surface_servers {
                if server.join().is_err() {
                    tracing::error!("a tileset's geometry server panicked");
                }
            }
        }
    }

    /// Writes the tile store down and closes it, off the window's thread.
    pub(crate) fn suspend_store(&self) {
        if let Some(store) = self.store.clone() {
            self.runtime.spawn(async move {
                match store.suspend().await {
                    Ok(()) => tracing::info!("tile store written down and closed"),
                    Err(e) => tracing::warn!("the tile store did not close ({e})"),
                }
            });
        }
    }

    /// Opens it again.
    pub(crate) fn resume_store(&self) {
        if let Some(store) = self.store.clone() {
            self.runtime.spawn(async move {
                match store.resume().await {
                    Ok(()) => tracing::info!("tile store open again"),
                    Err(e) => tracing::warn!("the tile store did not open again ({e})"),
                }
            });
        }
    }
}

/// Changes the imagery a running globe drapes.
///
/// Asking is one call that returns at once: resolving a layer is a round trip
/// or two to the host's service, and the window must not wait on it. The
/// resolution runs on the session's async runtime and the answer is collected
/// by the render loop ([`Switcher::poll`]) — the same shape as the location
/// request, for the same reason.
pub(crate) struct Switcher {
    layers: Layers,
    runtime: tokio::runtime::Handle,
    done: (Sender<Switched>, Receiver<Switched>),
}

impl Switcher {
    /// Starts the switch to `layers[layer]`.
    pub(crate) fn ask(&self, layer: usize, choice: &ImageryChoice) {
        let asked = web_time::Instant::now();
        let (layers, done, choice) = (self.layers.clone(), self.done.0.clone(), choice.clone());
        self.runtime.spawn(async move {
            let outcome = layers.switch_to(layer, &choice).await;
            // A closed channel means the window is gone, and with it the
            // point of saying how it went.
            let _ = done.send(Switched {
                layer,
                outcome,
                asked,
            });
        });
    }

    /// The outcome of a switch that has finished resolving, if one has.
    pub(crate) fn poll(&self) -> Option<Switched> {
        self.done.1.try_recv().ok()
    }
}

/// What the launch needs to know, all of it known before the window exists.
#[derive(Clone)]
pub(crate) struct Plan {
    pub host: Arc<dyn ViewerHost>,
    pub terrain: TerrainChoice,
    pub layers: &'static [ImageryChoice],
    pub opening: usize,
    /// Where the tile store lives on disk.
    pub store: std::path::PathBuf,
    pub config: tuile_core::traversal::Config,
    /// The graphics budget the session runs on.
    pub budget: tuile_core::budget::BudgetConfig,
    pub profile: crate::profile::Profile,
    /// See [`crate::launch::START_TIMEOUT`].
    pub timeout: std::time::Duration,
    /// How imagery is held, as far as it is known before the window's
    /// device exists: the person's choice resolved against the adapter
    /// (`App::imagery_plan_before_the_window`). The device confirms it, or
    /// corrects it, once it is up.
    pub imagery_plan: tuile_core::texels::ImageryPlan,
    /// What the same choice does with imagery stored already compressed:
    /// fixed for the session, as the globe is built with it.
    pub stored_imagery: tuile_core::texels::StoredPlan,
    /// The host's 3D Tiles tilesets drawn over the globe, by asset number:
    /// see [`crate::surface`].
    pub tilesets: Vec<i64>,
}

/// Starts the launch on `runtime` and returns at once.
pub(crate) fn start(runtime: &tokio::runtime::Handle, plan: Plan) -> Launch {
    let (tx, launch) = Launch::pending();
    let server_runtime = runtime.clone();
    runtime.spawn(async move {
        let outcome = bring_up(plan, server_runtime).await;
        // The window is gone; so is the reader.
        let _ = tx.send(outcome);
    });
    launch
}

async fn bring_up(plan: Plan, runtime: tokio::runtime::Handle) -> Outcome {
    let timeout = plan.timeout;
    tokio::time::timeout(timeout, bring_up_now(plan, runtime))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "the tile service did not answer within {} s",
                timeout.as_secs()
            ))
        })
}

async fn bring_up_now(plan: Plan, runtime: tokio::runtime::Handle) -> Outcome {
    let sources = plan.host.connect().await?;
    let store = crate::native::store::open_at(
        &plan.store,
        plan.profile.store_memory_bytes,
        plan.profile.store_disk_bytes,
        plan.profile.clear_damaged_store,
    )
    .await;
    // Decoding and resampling go to their own threads. Left on the server's
    // thread they run one at a time no matter how many loads are in flight —
    // measured at sixty-four outstanding and a single core busy.
    // As many as the profile allows: every core on a desktop, a few on a
    // phone, where the window and the runtime need theirs.
    let decode = Arc::new(tuile_core::offload::ThreadPool::with_threads(
        plan.profile.decode_threads,
    ));
    let globe = crate::globe::assemble(
        Arc::clone(&sources),
        store.clone().map(|s| s as Arc<dyn ContentStore>),
        &plan.terrain,
        plan.layers,
        plan.opening,
        decode,
        plan.stored_imagery,
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    // Before the server runs, so the first tile decoded is already held as
    // the person chose.
    globe.plan.set(plan.imagery_plan);
    let gate = Gate::default();
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: globe.loader,
        gate: gate.clone(),
    });
    let (stream, server) = tuile_core::runtime::in_process_with(
        globe.tree,
        loader,
        plan.config.clone(),
        plan.budget.clone(),
    )
    .map_err(|e| format!("the graphics budget cannot run: {e}"))?;
    let meter = server.meter();
    // Every load a task on the runtime's workers, so tiles keep arriving
    // while the server traverses.
    let server = server.with_spawn(tuile_core::runtime::Spawn::Tokio(runtime.clone()));
    // Its own thread, blocking on the runtime: the server's session is one
    // long future that the window talks to over channels.
    let server_runtime = runtime.clone();
    let server = std::thread::Builder::new()
        .name("geometry-server".into())
        .spawn(move || server_runtime.block_on(server.run()))
        .map_err(|e| format!("the geometry server could not start: {e}"))?;
    // The 3D Tiles over it. One that cannot be opened is said and left out:
    // the globe stands without it.
    let mut surfaces = Vec::new();
    let mut surface_failures = Vec::new();
    let mut surface_servers = Vec::new();
    for &asset in &plan.tilesets {
        match open_surface(sources.as_ref(), asset, &plan, &runtime).await {
            Ok((surface, server)) => {
                tracing::info!(asset, "3D Tiles tileset up");
                surfaces.push(surface);
                surface_servers.push(server);
            }
            Err(why) => {
                tracing::error!(asset, "3D Tiles tileset not opened: {why}");
                surface_failures.push(format!("3D Tiles asset {asset}: {why}"));
            }
        }
    }
    Ok(Started {
        stream,
        server: Some(server),
        meter,
        heights: globe.heights,
        layer_budget: globe.budget,
        imagery_plan: globe.plan,
        switcher: Switcher {
            layers: globe.layers,
            runtime: runtime.clone(),
            done: channel(),
        },
        store,
        gate,
        runtime,
        surfaces,
        surface_failures,
        surface_servers,
    })
}

/// Opens one of the host's 3D Tiles tilesets: its root and a geometry server
/// of its own on a thread of its own, its loads tasks on the runtime as the
/// globe's are.
async fn open_surface(
    sources: &dyn crate::embed::Sources,
    asset: i64,
    plan: &Plan,
    runtime: &tokio::runtime::Handle,
) -> Result<(crate::surface::Surface, std::thread::JoinHandle<()>), String> {
    let source = sources.tileset(asset).await?;
    let root = source
        .fetcher
        .fetch(&source.url)
        .await
        .map_err(|e| format!("its root: {e}"))?;
    let tileset = tuile_core::tileset::Tileset::from_json_bytes(&root, &source.url)
        .map_err(|e| format!("its root: {e}"))?;
    let fetcher = Arc::new(tuile_planetary::Held(Arc::clone(&source.fetcher)));
    let (stream, server) = tuile_core::runtime::in_process(
        tileset,
        fetcher,
        crate::surface::traversal(&plan.config),
        crate::surface::budget(&plan.budget),
    )
    .map_err(|e| format!("the graphics budget cannot run: {e}"))?;
    let meter = server.meter();
    let server = server.with_spawn(tuile_core::runtime::Spawn::Tokio(runtime.clone()));
    let server_runtime = runtime.clone();
    let server = std::thread::Builder::new()
        .name(format!("tileset-{asset}"))
        .spawn(move || server_runtime.block_on(server.run()))
        .map_err(|e| format!("its geometry server could not start: {e}"))?;
    Ok((
        crate::surface::Surface {
            asset,
            stream,
            meter,
            credits: source.credits,
        },
        server,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use web_time::Instant;

    /// A host whose service never answers — the network a phone has in a
    /// lift, or a captive portal.
    struct Stalled;

    #[async_trait::async_trait]
    impl ViewerHost for Stalled {
        fn identity(&self) -> crate::embed::Identity {
            crate::embed::Identity::default()
        }
        fn terrain(&self) -> TerrainChoice {
            TerrainChoice {
                asset: 1,
                cache: "t".into(),
            }
        }
        fn imagery(&self) -> Vec<ImageryChoice> {
            vec![ImageryChoice::new("a", "A", 2, "©")]
        }
        async fn connect(&self) -> Result<Arc<dyn crate::embed::Sources>, String> {
            std::future::pending().await
        }
    }

    /// One that refuses at once.
    struct Refusing;

    #[async_trait::async_trait]
    impl ViewerHost for Refusing {
        fn identity(&self) -> crate::embed::Identity {
            crate::embed::Identity::default()
        }
        fn terrain(&self) -> TerrainChoice {
            Stalled.terrain()
        }
        fn imagery(&self) -> Vec<ImageryChoice> {
            Stalled.imagery()
        }
        async fn connect(&self) -> Result<Arc<dyn crate::embed::Sources>, String> {
            Err("there is no key".into())
        }
    }

    fn plan(host: Arc<dyn ViewerHost>) -> Plan {
        static LAYERS: std::sync::OnceLock<Vec<ImageryChoice>> = std::sync::OnceLock::new();
        Plan {
            host,
            terrain: Stalled.terrain(),
            layers: LAYERS.get_or_init(|| Stalled.imagery()),
            opening: 0,
            store: std::env::temp_dir().join("launch-test"),
            config: tuile_core::traversal::Config::default(),
            budget: crate::profile::Profile::desktop().budget(),
            profile: crate::profile::Profile::desktop(),
            timeout: crate::launch::START_TIMEOUT,
            imagery_plan: tuile_core::texels::ImageryPlan::RGBA8,
            stored_imagery: tuile_core::texels::StoredPlan::OFF,
            tilesets: Vec::new(),
        }
    }

    /// The window's side of a launch that never finishes: every poll returns
    /// at once, with nothing, for as long as anyone asks. This is the
    /// property the phone's watchdog holds the application to — the main
    /// thread is never waiting on the network.
    #[test]
    fn polling_a_stalled_launch_never_waits() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut launch = start(runtime.handle(), plan(Arc::new(Stalled)));
        let began = Instant::now();
        let mut slowest = Duration::ZERO;
        while began.elapsed() < Duration::from_millis(500) {
            let t = Instant::now();
            assert!(launch.poll().is_none(), "a stalled launch answered");
            slowest = slowest.max(t.elapsed());
            std::thread::sleep(Duration::from_millis(16));
        }
        // A liveness bound, not a speed one: a poll that waited on the
        // network would wait the whole start-up; a slow runner does not.
        assert!(
            slowest < Duration::from_millis(250),
            "a poll took {slowest:?}"
        );
        runtime.shutdown_background();
    }

    /// A network that never answers is given up on after the timeout, with a
    /// sentence that says so — not a spinner turning for ever.
    #[test]
    fn a_stalled_launch_times_out() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut stalled = plan(Arc::new(Stalled));
        stalled.timeout = Duration::from_millis(100);
        let outcome = start(runtime.handle(), stalled).wait();
        assert!(
            outcome
                .err()
                .is_some_and(|why| why.contains("did not answer")),
            "a stalled start-up was not called off"
        );
        runtime.shutdown_background();
    }

    /// A refusal arrives as the host's own sentence, on a later poll.
    #[test]
    fn a_refused_launch_says_why() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut launch = start(runtime.handle(), plan(Arc::new(Refusing)));
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            if let Some(outcome) = launch.poll() {
                break outcome;
            }
            assert!(Instant::now() < deadline, "no answer");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(outcome.err().as_deref(), Some("there is no key"));
        assert_eq!(
            start(runtime.handle(), plan(Arc::new(Refusing)))
                .wait()
                .err()
                .as_deref(),
            Some("there is no key")
        );
    }
}
