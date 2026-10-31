// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The page's session: a worker running the geometry server, and the handles
//! the application drives it through — under the names the desktop's have.
//!
//! On a desktop the window and the loader share memory: the slot count is an
//! atomic both read, the gate a notification both see. A page and its worker
//! share nothing, so each handle here is a small proxy that posts what the
//! application did to the worker, which does it to the loader's own handle
//! (see [`super::control`]). The application cannot tell the difference,
//! which is the point. What the budget hears — the camera, the
//! acknowledgements, the memory and thermal signals — travels on the stream
//! itself; what it says of itself comes back for the overlay.
//!
//! Relief is the one thing that travels the other way. The camera stands on
//! the terrain's heights, which the desktop's loader records as it decodes;
//! here the page records them from the content it is handed — each terrain
//! tile's highest vertex — before the content goes on to the GPU.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use tuile_core::protocol::{
    ClientMessage, GeometryStream, ResourceContent, ServerMessage, StreamError,
};
use tuile_planetary::LayerBudget;
use tuile_terrain::{GeographicTilingScheme, TerrainHeights, TileCoord};
use tuile_web::post::{Connection, WorkerStream};
use wasm_bindgen::{JsCast, JsValue};
use web_time::Instant;

use super::control::{ToPage, ToWorker};
use super::options::{ProfileChoice, SourcesSpec};
use crate::embed::ImageryChoice;
use crate::globe::Switched;
use crate::launch::{Launch, Outcome, START_TIMEOUT};

/// Everything a start-up needs, kept so a failed one can be tried again.
pub(crate) struct Plan {
    /// Makes the worker: the package's own function, written so a bundler
    /// sees the worker script and ships it.
    pub make_worker: js_sys::Function,
    pub sources: SourcesSpec,
    pub profile: ProfileChoice,
    pub device_memory: Option<f64>,
    pub decode_workers: Option<u32>,
    /// The host's choice of how imagery is held. Off, the session starts at
    /// once; otherwise it waits for the page's device to say what it holds.
    pub compression: tuile_wgpu::ImageryCompression,
}

/// The worker, and the page's ear on it. Dropping the last handle ends both:
/// the listener goes, and the worker is terminated with everything it held.
struct Link {
    worker: web_sys::Worker,
    _connection: Connection,
}

impl Link {
    fn post(&self, message: &ToWorker) {
        match serde_wasm_bindgen::to_value(message) {
            Ok(value) => {
                if let Err(e) = self.worker.post_message(&value) {
                    tracing::error!("the worker could not be told: {e:?}");
                }
            }
            Err(e) => tracing::error!("a message to the worker could not be written: {e}"),
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

/// A session that is up: its stream, and the handles the window adopts.
pub(crate) struct Started {
    pub stream: PageStream,
    pub heights: Arc<TerrainHeights>,
    pub layer_budget: Budget,
    pub switcher: Switcher,
    pub gate: Gate,
}

impl Started {
    /// A page's loader is in its worker, which was given the device's plan
    /// in its start message: there is no handle here to set.
    pub(crate) fn imagery_plan(&self) -> Option<&tuile_planetary::ImageryPlanHandle> {
        None
    }

    /// The 3D Tiles drawn over the globe: none in a page yet — its geometry
    /// server is a worker that serves the globe alone.
    pub(crate) fn take_surfaces(&mut self) -> (Vec<crate::surface::Surface>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    /// Closes the session down. A page has no store to flush and no thread to
    /// join: the worker is terminated, and that is all of it.
    pub(crate) fn close(self, _patience: std::time::Duration, _join_the_server: bool) {
        tracing::info!("session closed; the worker is terminated");
    }

    /// The browser keeps no tile store yet: nothing to write down.
    pub(crate) fn suspend_store(&self) {}

    /// Nor anything to open again.
    pub(crate) fn resume_store(&self) {}

    /// The session's meter lives in the worker, beside the budget it
    /// measures; the page reads the signals it publishes instead (see
    /// [`ToPage::Signals`]).
    pub(crate) fn meter(&self) -> Option<tuile_core::runtime::SharedMeter> {
        None
    }
}

/// The stream, with the relief recorded on the way past.
pub(crate) struct PageStream {
    inner: WorkerStream,
    heights: Arc<TerrainHeights>,
}

impl GeometryStream for PageStream {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
        self.inner.send(msg)
    }

    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        let polled = self.inner.poll_message(cx);
        if let Poll::Ready(Some(ServerMessage::Content {
            resource,
            content: ResourceContent::Mesh(decoded),
            ..
        })) = &polled
        {
            if let Some(highest) = highest_point(decoded) {
                let (z, x, y) = resource.tile().terrain_coord();
                self.heights.record(TileCoord::new(z, x, y), highest);
            }
        }
        polled
    }
}

/// The highest point of a tile's surface above the ellipsoid, metres.
fn highest_point(content: &tuile_core::content::DecodedTileContent) -> Option<f32> {
    content
        .meshes
        .iter()
        .flat_map(|mesh| mesh.positions.iter())
        .map(|p| {
            let ecef = content.local_origin_ecef + glam::DVec3::from(glam::Vec3::from(*p));
            tuile_core::geo::ecef_to_geodetic(ecef).height as f32
        })
        .reduce(f32::max)
}

/// How many imagery layers a drape may carry, as the device says.
#[derive(Clone, Default)]
pub(crate) struct Budget {
    link: Option<Rc<Link>>,
    local: LayerBudget,
}

impl Budget {
    /// See `tuile_planetary::LayerBudget::set_from_device`.
    pub(crate) fn set_from_device(&self, max_sampled_textures_per_stage: u32) {
        self.local.set_from_device(max_sampled_textures_per_stage);
        if let Some(link) = self.link.as_ref() {
            link.post(&ToWorker::Slots {
                reported: max_sampled_textures_per_stage,
            });
        }
    }

    /// The slots a drape carries.
    pub(crate) fn get(&self) -> u32 {
        self.local.get()
    }
}

/// The worker's loads, held while the tab is hidden.
pub(crate) struct Gate {
    link: Rc<Link>,
}

impl Gate {
    /// Holds every load not yet started.
    pub(crate) fn shut(&self) {
        self.link.post(&ToWorker::Pause { paused: true });
    }

    /// Lets them go.
    pub(crate) fn open(&self) {
        self.link.post(&ToWorker::Pause { paused: false });
    }
}

/// A layer resolved, or refused, by the worker.
type Answer = (usize, Result<u64, String>);

/// Changes the imagery the worker's globe drapes. The same two calls as the
/// desktop's; the worker resolves the layer, which it already knows by
/// index.
pub(crate) struct Switcher {
    link: Rc<Link>,
    asked: RefCell<HashMap<usize, Instant>>,
    answers: Rc<RefCell<VecDeque<Answer>>>,
}

impl Switcher {
    /// Starts the switch to the host's layer `layer`.
    pub(crate) fn ask(&self, layer: usize, _choice: &ImageryChoice) {
        self.asked.borrow_mut().insert(layer, Instant::now());
        self.link.post(&ToWorker::Imagery { layer });
    }

    /// The outcome of a switch that has finished resolving, if one has.
    pub(crate) fn poll(&self) -> Option<Switched> {
        let (layer, outcome) = self.answers.borrow_mut().pop_front()?;
        let asked = self
            .asked
            .borrow_mut()
            .remove(&layer)
            .unwrap_or_else(Instant::now);
        Some(Switched {
            layer,
            outcome,
            asked,
        })
    }
}

/// What a start-up holds until the worker says it is up.
struct Waiting {
    answer: std::sync::mpsc::Sender<Outcome>,
    stream: WorkerStream,
    link: Rc<Link>,
}

/// Starts a worker, asks it to bring the session up, and returns at once.
///
/// The outcome arrives through the worker's `started` or `failed` — or, if
/// it says nothing within [`START_TIMEOUT`], as a failure, and the worker is
/// terminated.
pub(crate) fn start(plan: &Plan) -> Launch {
    let (answer, launch) = Launch::pending();
    let worker = match new_worker(&plan.make_worker) {
        Ok(worker) => worker,
        Err(why) => {
            let _ = answer.send(Err(why));
            return launch;
        }
    };
    let answers: Rc<RefCell<VecDeque<Answer>>> = Rc::default();
    let waiting: Rc<RefCell<Option<Waiting>>> = Rc::default();
    let heard = {
        let (waiting, answers) = (Rc::clone(&waiting), Rc::clone(&answers));
        move |value: JsValue| match serde_wasm_bindgen::from_value::<ToPage>(value) {
            Ok(ToPage::Started) => {
                if let Some(w) = waiting.borrow_mut().take() {
                    let _ = w.answer.send(Ok(started(w.stream, w.link, &answers)));
                }
            }
            Ok(ToPage::Failed { reason }) => {
                if let Some(w) = waiting.borrow_mut().take() {
                    let _ = w.answer.send(Err(reason));
                }
            }
            Ok(ToPage::Switched { layer, outcome }) => {
                answers.borrow_mut().push_back((layer, outcome));
            }
            // The worker's budget, published here as a desktop's session
            // publishes its own: the overlay reads it the same way.
            Ok(ToPage::Signals { glance }) => {
                tuile_core::metrics::publish_budget(glance.signals());
            }
            Err(e) => tracing::error!("the worker said something unreadable: {e}"),
        }
    };
    let (stream, connection) = WorkerStream::connect(&worker, heard);
    let link = Rc::new(Link {
        worker,
        _connection: connection,
    });
    let start = {
        let link = Rc::downgrade(&link);
        let (sources, profile, device_memory, decode_workers) = (
            plan.sources.clone(),
            plan.profile,
            plan.device_memory,
            plan.decode_workers,
        );
        move |imagery: super::imagery::Held| {
            // A start-up given up on in the meantime has dropped its worker.
            if let Some(link) = link.upgrade() {
                link.post(&ToWorker::Start {
                    sources,
                    opening: 0,
                    profile,
                    device_memory,
                    decode_workers,
                    imagery,
                });
            }
        }
    };
    *waiting.borrow_mut() = Some(Waiting {
        answer,
        stream,
        link,
    });
    // Off asks nothing of the device: the session starts now, as it always
    // did. A compression waits for the device's answer — the plan every
    // tile is decoded by.
    if plan.compression == tuile_wgpu::ImageryCompression::Off {
        start(super::imagery::Held::RGBA8);
    } else {
        super::imagery::when_the_device_holds(start);
    }
    give_up_after(START_TIMEOUT, move || {
        if let Some(w) = waiting.borrow_mut().take() {
            let _ = w.answer.send(Err(format!(
                "the tile service did not answer within {} s",
                START_TIMEOUT.as_secs()
            )));
        }
    });
    launch
}

/// The session's handles, over a worker that has said it is up.
fn started(
    stream: WorkerStream,
    link: Rc<Link>,
    answers: &Rc<RefCell<VecDeque<Answer>>>,
) -> Started {
    let heights = Arc::new(TerrainHeights::new(GeographicTilingScheme::default()));
    Started {
        stream: PageStream {
            inner: stream,
            heights: Arc::clone(&heights),
        },
        heights,
        layer_budget: Budget {
            link: Some(Rc::clone(&link)),
            local: LayerBudget::default(),
        },
        switcher: Switcher {
            link: Rc::clone(&link),
            asked: RefCell::default(),
            answers: Rc::clone(answers),
        },
        gate: Gate { link },
    }
}

/// A worker running the package's worker script.
fn new_worker(make: &js_sys::Function) -> Result<web_sys::Worker, String> {
    make.call0(&JsValue::NULL)
        .map_err(|e| format!("the geometry worker could not start: {e:?}"))?
        .dyn_into::<web_sys::Worker>()
        .map_err(|_| "the worker factory did not return a Worker".to_owned())
}

/// Runs `then` once, after `wait`, on the page's own timer.
fn give_up_after(wait: std::time::Duration, then: impl FnOnce() + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = wasm_bindgen::closure::Closure::once_into_js(then);
    let millis = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    if let Err(e) = window
        .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), millis)
    {
        tracing::warn!("no start-up timeout: {e:?}");
    }
}
