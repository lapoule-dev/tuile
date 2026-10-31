// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The worker: the geometry server a page draws from, assembled exactly as
//! the desktop's is.
//!
//! The same [`crate::globe::assemble`] over the page's sources, the same gate
//! in front of the loader, the same `GeometryServer` on the same budget —
//! the profile's, sized from the device's memory — run by the worker's event
//! loop on the worker's clock. What differs is only what a worker has: no
//! tile store, and one thread — so decoding goes to the decode workers this
//! one starts ([`super::decode`]), in the budget's order, where a desktop
//! hands it to a pool of threads. The stream to the page is
//! [`tuile_web::post::serve`]: the camera, the acknowledgements and their
//! bytes, the memory and thermal signals; what the page asks beside it
//! arrives as [`ToWorker`] and is done to the loader's own handles, and the
//! budget's signals go back as [`ToPage::Signals`] for the page's overlay.
//!
//! A host's 3D Tiles tileset, switched on, is a geometry server of its own
//! here too, beside the globe's, as a desktop runs one beside its own: the
//! page asks for it with a `MessageChannel` port ([`ToWorker::OpenTileset`]),
//! and its stream is served on that port ([`tuile_web::post::serve_over`]).

use std::sync::Arc;

use tuile_core::source::TileLoader;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::control::{CreditWire, Glance, ToPage, ToWorker};
use super::options::{ProfileChoice, SourcesSpec};
use super::sources::PageSources;
use crate::gate::{Gate, Gated};
use crate::globe::Layers;
use crate::profile::Profile;

/// The worker's entry point, called by the package's worker script once the
/// module is loaded: waits for the page's `start`.
///
/// `make_decode_worker` returns a new worker running the package's decode
/// worker script — written in the package, so a bundler sees the script and
/// ships it. Without it, decoding stays on this worker's thread.
#[wasm_bindgen(js_name = runWorker)]
pub fn run_worker(make_decode_worker: Option<js_sys::Function>) {
    super::log::install();
    let on_start =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            match serde_wasm_bindgen::from_value::<ToWorker>(event.data()) {
                Ok(ToWorker::Start {
                    sources,
                    opening,
                    profile,
                    device_memory,
                    decode_workers,
                    imagery,
                }) => {
                    // The page speaks no more until it hears back; nothing else
                    // is listened for until the session decides who listens.
                    scope().set_onmessage(None);
                    if let Some(make) = make_decode_worker.clone() {
                        let count = tuile_web::schedule::pool_size(
                            decode_workers,
                            tuile_web::decode::hardware_concurrency(),
                        );
                        if count > 0 {
                            tuile_web::decode::start(make, count);
                        }
                    }
                    wasm_bindgen_futures::spawn_local(async move {
                        let profile =
                            Profile::browser(profile == ProfileChoice::Handheld, device_memory);
                        if let Err(reason) = start(sources, opening, profile, imagery).await {
                            tracing::error!("the session could not start: {reason}");
                            post(&ToPage::Failed { reason });
                        }
                    });
                }
                Ok(other) => tracing::warn!("a message before the start was ignored: {other:?}"),
                Err(e) => tracing::error!("the page said something unreadable: {e}"),
            }
        });
    scope().set_onmessage(Some(on_start.as_ref().unchecked_ref()));
    // The handler lives as long as the worker: it is replaced, never dropped.
    on_start.forget();
}

/// Brings the session up and serves it to the page.
async fn start(
    spec: SourcesSpec,
    opening: usize,
    profile: Profile,
    imagery: super::imagery::Held,
) -> Result<(), String> {
    let (terrain, layers) = spec.choices()?;
    let sources = Arc::new(PageSources::new(spec)?);
    let globe = crate::globe::assemble(
        Arc::clone(&sources) as Arc<dyn tuile_planetary::Sources>,
        None,
        &terrain,
        &layers,
        opening,
        Arc::new(tuile_web::decode::WebDecode),
        // The page's device's: what tiles stored compressed become.
        imagery.stored(),
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    // And how a JPEG or PNG is held: RGBA8 the page's GPU encodes, blocks the
    // decode workers encode, or RGBA8 as it is. Each imagery job carries it
    // to the decode worker that runs it.
    globe.plan.set(imagery.plan());
    tracing::info!(?imagery, "imagery, as the page's device holds it");
    let gate = Gate::default();
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: globe.loader,
        gate: gate.clone(),
    });
    let (stream, server) = tuile_core::runtime::in_process_with(
        globe.tree,
        loader,
        profile.traversal(),
        profile.budget(),
    )
    .map_err(|e| format!("the budget: {e}"))?;
    tracing::info!(
        gpu_mib = profile.gpu_bytes >> 20,
        "the session's budget, from the device's memory"
    );
    // The worker's one thread is also where the page's messages arrive — the
    // camera, the acks: a burst answered from memory must let them in.
    let server = server
        .with_breather(BURST, Arc::new(breathe))
        .with_clock(clock());
    // The decode pool takes its jobs in the order the budget ranks the loads
    // waiting on them.
    tuile_web::decode::follow(server.load_ranks());
    wasm_bindgen_futures::spawn_local(async move {
        server.run().await;
        tracing::info!("the geometry server has stopped");
    });
    let handles = Handles {
        budget: globe.budget,
        layers: globe.layers,
        choices: layers,
        gate,
        sources,
        traversal: crate::surface::traversal(&profile.traversal()),
        tileset_budget: crate::surface::budget(&profile.budget()),
        tilesets: Rc::default(),
        asked: Rc::default(),
    };
    let connection = tuile_web::post::serve(stream, move |value| handles.heard(value));
    // The worker is the session: the connection lasts as long as it does.
    std::mem::forget(connection);
    post(&ToPage::Started);
    tell_the_page_the_budget();
    Ok(())
}

/// The session's clock: the worker's own monotonic time. The core's default
/// on wasm counts steps, not time — a budget timing its loads out on it
/// would cancel them by the number of steps they took.
fn clock() -> tuile_core::runtime::Clock {
    let origin = web_time::Instant::now();
    Arc::new(move || {
        tuile_core::budget::Micros(origin.elapsed().as_micros().min(u128::from(u64::MAX)) as u64)
    })
}

/// How often the budget's signals go to the page: as often as the session
/// publishes them.
const SIGNALS_EVERY_MS: i32 = 500;

/// Posts the budget's signals to the page whenever the session has
/// published new ones, for its overlay.
fn tell_the_page_the_budget() {
    let last: std::rc::Rc<std::cell::RefCell<Option<Glance>>> = std::rc::Rc::default();
    let steps = std::cell::Cell::new(0u64);
    let tick = Closure::<dyn FnMut()>::new(move || {
        let Some(signals) = tuile_core::metrics::budget() else {
            return;
        };
        // Where the worker's one thread goes, for its console: the budget's
        // decisions share it with every fetch and decode reply.
        if signals.decide_micros.count != steps.get() {
            let d = &signals.decide_micros;
            let n = d.count.saturating_sub(steps.replace(d.count));
            tracing::info!(
                steps = n,
                decide_mean_ms = d.sum as f64 / d.count.max(1) as f64 / 1000.0,
                decide_p99_ms = d.quantile_upper(990) as f64 / 1000.0,
                decide_peak_ms = signals.decide_peak_micros as f64 / 1000.0,
                completions = signals.completions_total,
                starts = signals
                    .starts_total
                    .iter()
                    .map(|(_, s)| s[0] + s[1])
                    .sum::<u64>(),
                cancels_grace = signals.cancels_total[0],
                cancels_timeout = signals.cancels_total[1],
                cancels_preempted = signals.cancels_total[2],
                stale = signals.stale_results_total,
                drawn_exact = signals.drawn_tiles[0],
                drawn_held = signals.drawn_tiles[1],
                drawn_fallback = signals.drawn_tiles[2],
                lost = signals.lost_tiles,
                bare = signals.bare_items,
                underdraped = signals.underdraped_tiles,
                evictions = ?signals.evictions_total.0,
                bytes = ?signals.bytes.0,
                targets = ?signals.target_bytes.0,
                "the budget's steps"
            );
        }
        let glance = Glance::of(&signals);
        if last.borrow().as_ref() != Some(&glance) {
            post(&ToPage::Signals {
                glance: Box::new(glance.clone()),
            });
            *last.borrow_mut() = Some(glance);
        }
    });
    if let Err(e) = scope().set_interval_with_callback_and_timeout_and_arguments_0(
        tick.as_ref().unchecked_ref(),
        SIGNALS_EVERY_MS,
    ) {
        tracing::warn!("the page will not see the budget: {e:?}");
    }
    // Lives as long as the worker.
    tick.forget();
}

/// Events the server handles back to back before the worker's messages get
/// their turn: one. A step on the budget is tens of milliseconds of the
/// worker's one thread near the ground; sixteen of them back to back held
/// the fetches and the decode pool's replies for half a second, the pool
/// idle with work queued. Yielding before each step lets everything that
/// has landed in the meantime be answered by that one step.
const BURST: usize = 1;

/// A pause on the worker's timer: a macrotask, so the messages queued behind
/// the server's burst are delivered before it goes on. A promise alone would
/// not do — its continuation is a microtask, which runs before them.
fn breathe() -> tuile_core::runtime::Pause {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = scope().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
    });
    Box::pin(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    })
}

/// The loader's handles, which the page's messages act on.
struct Handles {
    budget: tuile_planetary::LayerBudget,
    layers: Layers,
    choices: Vec<crate::embed::ImageryChoice>,
    gate: Gate,
    /// What a tileset is opened with: the page's sources, and how it is
    /// traversed and budgeted (`crate::surface`).
    sources: Arc<PageSources>,
    traversal: tuile_core::traversal::Config,
    tileset_budget: tuile_core::budget::BudgetConfig,
    /// The tilesets served, by asset: dropping one's connection lets go of
    /// its stream, and its server ends.
    tilesets: Rc<RefCell<HashMap<i64, tuile_web::post::Connection>>>,
    /// Each tileset's latest request, counted: an opening that a close or a
    /// later opening has overtaken is let go of when it lands.
    asked: Rc<RefCell<HashMap<i64, u64>>>,
}

impl Handles {
    fn heard(&self, value: JsValue) {
        // The port a tileset is to be served on travels beside the message.
        let port = js_sys::Reflect::get(&value, &JsValue::from_str("port"))
            .ok()
            .and_then(|p| p.dyn_into::<web_sys::MessagePort>().ok());
        match serde_wasm_bindgen::from_value::<ToWorker>(value) {
            Ok(ToWorker::OpenTileset { asset }) => match port {
                Some(port) => self.open_tileset(asset, port),
                None => post(&ToPage::TilesetFailed {
                    asset,
                    reason: "no port to serve it on".to_owned(),
                }),
            },
            Ok(ToWorker::CloseTileset { asset }) => {
                *self.asked.borrow_mut().entry(asset).or_default() += 1;
                if self.tilesets.borrow_mut().remove(&asset).is_some() {
                    tracing::info!(asset, "3D Tiles tileset closed");
                }
            }
            Ok(ToWorker::Slots { reported }) => self.budget.set_from_device(reported),
            Ok(ToWorker::Pause { paused: true }) => self.gate.shut(),
            Ok(ToWorker::Pause { paused: false }) => self.gate.open(),
            Ok(ToWorker::Imagery { layer }) => {
                let Some(choice) = self.choices.get(layer).cloned() else {
                    post(&ToPage::Switched {
                        layer,
                        outcome: Err(format!("there is no layer {layer}")),
                    });
                    return;
                };
                let layers = self.layers.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let outcome = layers.switch_to(layer, &choice).await;
                    post(&ToPage::Switched { layer, outcome });
                });
            }
            Ok(ToWorker::Start { .. }) => tracing::warn!("a second start was ignored"),
            Err(e) => tracing::error!("the page said something unreadable: {e}"),
        }
    }
}

impl Handles {
    /// Opens the tileset `asset` as a desktop does — its root, then a server
    /// of its own on this worker's loop — and serves it on `port`.
    fn open_tileset(&self, asset: i64, port: web_sys::MessagePort) {
        let sources = Arc::clone(&self.sources);
        let (traversal, budget) = (self.traversal.clone(), self.tileset_budget.clone());
        let tilesets = Rc::clone(&self.tilesets);
        let asked = Rc::clone(&self.asked);
        let request = {
            let mut asked = asked.borrow_mut();
            let n = asked.entry(asset).or_default();
            *n += 1;
            *n
        };
        wasm_bindgen_futures::spawn_local(async move {
            let opened = async {
                use tuile_planetary::Sources as _;
                let source = sources.tileset(asset).await?;
                let root = source
                    .fetcher
                    .fetch(&source.url)
                    .await
                    .map_err(|e| format!("its root: {e}"))?;
                let tileset = tuile_core::tileset::Tileset::from_json_bytes(&root, &source.url)
                    .map_err(|e| format!("its root: {e}"))?;
                let fetcher = Arc::new(tuile_planetary::Held(Arc::clone(&source.fetcher)));
                let (stream, server) =
                    tuile_core::runtime::in_process(tileset, fetcher, traversal, budget)
                        .map_err(|e| format!("the graphics budget cannot run: {e}"))?;
                let server = server
                    .with_breather(BURST, Arc::new(breathe))
                    .with_clock(clock());
                wasm_bindgen_futures::spawn_local(async move {
                    server.run().await;
                    tracing::info!(asset, "a tileset's geometry server has stopped");
                });
                Ok::<_, String>((stream, source.credits))
            }
            .await;
            if asked.borrow().get(&asset) != Some(&request) {
                tracing::info!(asset, "a tileset opened after it was closed; let go");
                return;
            }
            match opened {
                Ok((stream, credits)) => {
                    let connection = tuile_web::post::serve_over(stream, port);
                    tilesets.borrow_mut().insert(asset, connection);
                    tracing::info!(asset, "3D Tiles tileset up");
                    post(&ToPage::TilesetOpened {
                        asset,
                        credits: credits.iter().map(CreditWire::of).collect(),
                    });
                }
                Err(reason) => {
                    tracing::error!(asset, "3D Tiles tileset not opened: {reason}");
                    post(&ToPage::TilesetFailed { asset, reason });
                }
            }
        });
    }
}

fn scope() -> web_sys::DedicatedWorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn post(message: &ToPage) {
    match serde_wasm_bindgen::to_value(message) {
        Ok(value) => {
            if let Err(e) = scope().post_message(&value) {
                tracing::error!("the page could not be told: {e:?}");
            }
        }
        Err(e) => tracing::error!("a message to the page could not be written: {e}"),
    }
}
