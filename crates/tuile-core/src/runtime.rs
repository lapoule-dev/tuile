// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The geometry server: traversal, the graphics budget, fetch and decode,
//! orchestrated around the pure [`traverse`](crate::traversal::traverse)
//! function and the pure [`crate::budget::Budget`].
//!
//! This is a LOGICAL server, not a network server: [`in_process`] binds it
//! to a consumer over channels (the fully decoded liaison). Network
//! middleware (`tuile-server`) drives the same loop and serializes the
//! protocol — glb on the wire.
//!
//! # One step
//!
//! Every coalesced burst of client messages and of arrivals is answered by one
//! step: the tree's deferred edits are applied, the traversal walks the views
//! against the budget's residency and lists what it needs — drawn, the
//! children a hold waits on, the ancestor chain, the floor — and the budget
//! decides once (`decide`). The decisions are executed in order: loads started
//! and cancelled, decoded content delivered, the draw set and the upload plan
//! sent, GPU copies evicted after the draw that stops using them. Arrivals
//! between two steps are recorded with the budget and applied at the next.
//! The budget lives here, with the session (D-A5): its inputs are messages
//! only, so it runs the same in-process, in a worker or across a socket.
//!
//! Runtime-agnostic by default: no tokio, no spawning. The whole server is
//! one future ([`GeometryServer::run`]) multiplexing client messages and
//! in-flight fetches through `FuturesUnordered`; the caller decides where it
//! runs. On native, with the optional `rt-tokio` feature, the host can hand
//! it a runtime handle instead and every load runs as a task of its own —
//! see [`Spawn`].

use crate::budget::{
    Budget, BudgetConfig, BudgetEvent, BudgetMeter, BudgetObserver, Bytes, Class, ConfigError,
    Decisions, Failure, FrameInput, Generation, ImageryNeed, Micros, Need, NeedDelta, NeedSet,
    Presence, PresenceOracle, ResourceId, StepRecord, TexelFootprint,
};
use crate::content::{DecodedTileContent, DrapeContent};
use crate::fetch::TileFetcher;
use crate::protocol::{
    in_process_pair, Ancestry, ClientMessage, InProcessStream, Priming, ResourceContent,
    ServerEndpoint, ServerMessage,
};
use crate::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use crate::tiles3d::{TilesetLoader, TilesetTree};
use crate::tileset::Tileset;
use crate::traversal::{
    traverse_with, Config, NearestContent, PriorityGroup, TraversalOutput, ViewState,
};
use futures_channel::mpsc::UnboundedSender;
use futures_core::Stream;
use futures_util::future::{poll_fn, AbortHandle, Abortable, Aborted};
use futures_util::stream::FuturesUnordered;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::Poll;

/// What a load produced: a tile's mesh (or the news that it grew the tree),
/// or a tile's drape.
enum Fetched {
    Mesh(Loaded),
    Drape(DrapeContent),
}

type LoadMsg = (ResourceId, Generation, Result<Fetched, LoadError>);
/// A load's result and the moment its own future resolved — what the loop
/// measures [`arrival_wait_seconds`](crate::metrics::Metrics::arrival_wait_seconds)
/// from when it gets round to handling it.
type Arrival = (LoadMsg, crate::metrics::Stamp);
// The boxed load future is `Send` on native (driven by a multi-thread executor
// like tokio) but `?Send` on wasm, where it holds JS futures (`!Send`) and runs
// single-threaded under `spawn_local`. Same server loop, two host models.
#[cfg(not(target_arch = "wasm32"))]
type BoxLoadFut = Pin<Box<dyn Future<Output = Arrival> + Send>>;
#[cfg(target_arch = "wasm32")]
type BoxLoadFut = Pin<Box<dyn Future<Output = Arrival>>>;
type LoadFuture = Abortable<BoxLoadFut>;

/// A load future that notes when it was woken.
///
/// The loads live in one `FuturesUnordered` that only the loop polls, so a
/// load whose data has arrived — woken by its socket, its decode pool, its
/// store — makes no progress until the loop comes round to it, which may be
/// after a whole traversal. Stamping when the future *resolves* would hide
/// exactly that, since it resolves only once it is polled. So the waker is
/// wrapped: every wake is stamped, the time from a wake to the poll that
/// answers it goes to `waits`, and the future's output carries the last wake
/// before it completed — what [`crate::metrics::Metrics::arrival_wait_seconds`]
/// is measured from. A load that completes on a poll nothing woke (its first,
/// or one the loop made on its own) carries the instant of that poll.
///
/// Wakes pass straight through to the loop's waker, and the same waker is
/// handed over on every poll so `will_wake` keeps answering yes: nothing about
/// when or how often the load is polled changes.
struct Woken<F: Future> {
    inner: Pin<Box<F>>,
    hook: Arc<WakeHook>,
    waits: &'static crate::metrics::Timer,
}

#[derive(Default)]
struct WakeHook {
    /// The waker of whoever polls us: the `FuturesUnordered`'s.
    waker: std::sync::Mutex<Option<std::task::Waker>>,
    /// The first wake since the last poll.
    woke: std::sync::Mutex<Option<crate::metrics::Stamp>>,
}

impl std::task::Wake for WakeHook {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Ok(mut woke) = self.woke.lock() {
            woke.get_or_insert_with(crate::metrics::stamp);
        }
        if let Ok(waker) = self.waker.lock() {
            if let Some(waker) = waker.as_ref() {
                waker.wake_by_ref();
            }
        }
    }
}

impl<F: Future> Woken<F> {
    fn new(inner: F, waits: &'static crate::metrics::Timer) -> Self {
        Self {
            inner: Box::pin(inner),
            hook: Arc::new(WakeHook::default()),
            waits,
        }
    }
}

impl<F: Future> Future for Woken<F> {
    type Output = (F::Output, crate::metrics::Stamp);

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if let Ok(mut waker) = self.hook.waker.lock() {
            match waker.as_mut() {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => *waker = Some(cx.waker().clone()),
            }
        }
        let woke = self.hook.woke.lock().ok().and_then(|mut w| w.take());
        if let Some(woke) = woke {
            self.waits.record(woke.elapsed());
        }
        let waker = std::task::Waker::from(Arc::clone(&self.hook));
        let mut inner_cx = std::task::Context::from_waker(&waker);
        match self.inner.as_mut().poll(&mut inner_cx) {
            Poll::Ready(out) => Poll::Ready((out, woke.unwrap_or_else(crate::metrics::stamp))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Where a session's loads run.
///
/// # Inline — the default, and the only mode on wasm
///
/// Every load is a future inside the server's own, polled by the loop
/// between two traversals. Nothing advances while a pass runs: no body is
/// read, no next request is sent, no decode is submitted. It is also the
/// mode in which a session can be stepped by hand, one poll at a time, which
/// is what every test of the loop does.
///
/// A browser worker gains nothing from anything else: it has one thread, so
/// a load spawned beside the server still waits for the server to yield.
///
/// # Tokio — native, behind the `rt-tokio` feature
///
/// Every load is spawned on the host's runtime — a handle passed in, never a
/// runtime built here — and reports back through a channel the loop drains
/// in batches. Loads then advance on the other workers while a traversal
/// runs, which is where an arrival used to wait. Cancelling still goes
/// through the load's abort handle, and a result that lands after its load
/// was cancelled is recognised as such and dropped.
///
/// A load that changes the tree's shape (a reveal, a graft) could then land
/// in the middle of a pass; the server therefore asks the tree to
/// [defer its edits](TileTree::defer_edits) and applies them at the top of
/// every pass.
#[derive(Clone, Default)]
pub enum Spawn {
    /// Loads polled by the server's own future.
    #[default]
    Inline,
    /// Loads spawned as tasks on this runtime.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    Tokio(tokio::runtime::Handle),
}

impl std::fmt::Debug for Spawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline => f.write_str("Inline"),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tokio(_) => f.write_str("Tokio"),
        }
    }
}

/// A load's result, or the news that it was cancelled.
type Done = Result<Arrival, Aborted>;

/// The loads in flight, wherever they run. See [`Spawn`].
enum Loads {
    Inline(FuturesUnordered<LoadFuture>),
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    Tasks(tasks::Tasks),
}

impl Loads {
    fn new(spawn: &Spawn) -> Self {
        match spawn {
            Spawn::Inline => Self::Inline(FuturesUnordered::new()),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Spawn::Tokio(handle) => Self::Tasks(tasks::Tasks::new(handle.clone())),
        }
    }

    /// Whether loads run beside the server rather than inside it.
    fn spawns(&self) -> bool {
        !matches!(self, Self::Inline(_))
    }

    fn push(&mut self, load: LoadFuture, _handle: &AbortHandle) {
        match self {
            Self::Inline(loads) => loads.push(load),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tasks(tasks) => tasks.spawn(load, _handle.clone()),
        }
    }

    /// The next result, waiting for it. `Pending` when there is nothing in
    /// flight as well as when nothing has landed yet.
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Done> {
        match self {
            Self::Inline(loads) => match Pin::new(loads).poll_next(cx) {
                Poll::Ready(Some(done)) => Poll::Ready(done),
                Poll::Ready(None) | Poll::Pending => Poll::Pending,
            },
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tasks(tasks) => tasks.poll_next(cx),
        }
    }

    /// The next result that has already landed, without waiting.
    fn try_next(&mut self) -> Option<Done> {
        use futures_util::{FutureExt, StreamExt};
        match self {
            Self::Inline(loads) => loads.next().now_or_never().flatten(),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tasks(tasks) => tasks.try_next(),
        }
    }
}

/// Loads as tokio tasks. See [`Spawn::Tokio`].
#[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
mod tasks {
    use super::{AbortHandle, Aborted, Arrival, Done, LoadFuture};
    use std::task::{Context, Poll};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    /// A result, with the handle of the load that produced it.
    type Landed = (Arrival, AbortHandle);

    pub(super) struct Tasks {
        runtime: tokio::runtime::Handle,
        tx: UnboundedSender<Landed>,
        pub(super) rx: UnboundedReceiver<Landed>,
    }

    impl Tasks {
        pub(super) fn new(runtime: tokio::runtime::Handle) -> Self {
            let (tx, rx) = unbounded_channel();
            Self { runtime, tx, rx }
        }

        /// Runs `load` as a task of its own. Its abort handle travels with
        /// the result, because aborting a task that has already sent cannot
        /// take the message back.
        pub(super) fn spawn(&self, load: LoadFuture, handle: AbortHandle) {
            let tx = self.tx.clone();
            // Detached: the abort handle is the only way to stop it, and a
            // cancelled load wakes, sees it, and ends.
            drop(self.runtime.spawn(async move {
                if let Ok(msg) = load.await {
                    // The server may have gone: nobody left to tell.
                    let _ = tx.send((msg, handle));
                }
            }));
        }

        /// A result whose load was cancelled after it finished — the loop
        /// cancels only from its own thread, before it reads the next
        /// result, so the flag is never late — is the news of that cancel,
        /// exactly as an inline load would have reported it.
        pub(super) fn screen((msg, handle): Landed) -> Done {
            if handle.is_aborted() {
                Err(Aborted)
            } else {
                Ok(msg)
            }
        }

        pub(super) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Done> {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(landed)) => Poll::Ready(Self::screen(landed)),
                // Never: the sender lives as long as this.
                Poll::Ready(None) | Poll::Pending => Poll::Pending,
            }
        }

        pub(super) fn try_next(&mut self) -> Option<Done> {
            self.rx.try_recv().ok().map(Self::screen)
        }
    }
}

/// A pause the host's event loop gets to run in: see
/// [`GeometryServer::with_breather`]. `Send` on native, like the loads, and
/// not on wasm, where it is a JS timer.
#[cfg(not(target_arch = "wasm32"))]
pub type Pause = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
pub type Pause = Pin<Box<dyn Future<Output = ()>>>;

/// Makes a [`Pause`].
pub type Breather = Arc<dyn Fn() -> Pause + Send + Sync>;

/// The session's time, as the budget reads it: microseconds from an origin
/// the host chooses. The budget never reads a clock; the session hands it
/// this one's answer at every step.
pub type Clock = Arc<dyn Fn() -> Micros + Send + Sync>;

/// A monotonic clock from now. On wasm, where the standard clock panics, a
/// host passes its own ([`GeometryServer::with_clock`]); this one then
/// counts steps as 16 ms each, so a session still ages.
fn default_clock() -> Clock {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let origin = std::time::Instant::now();
        Arc::new(move || Micros(origin.elapsed().as_micros().min(u128::from(u64::MAX)) as u64))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        Arc::new(move || Micros(ticks.fetch_add(16_000, std::sync::atomic::Ordering::Relaxed)))
    }
}

/// The budget's signals, shared between the session that decides and the
/// executor that draws: the session records each step, the executor each
/// render frame, and any sink — the metrics endpoint, an overlay, a test —
/// reads the same numbers (spec §16).
pub type SharedMeter = Arc<Mutex<BudgetMeter>>;

/// How often the session copies its signals to the process's metrics.
const PUBLISH_EVERY: Micros = Micros::from_millis(500);

/// How often, in steps, the pass over the views ahead runs again while they
/// hold still: their ring grows a level each time its tiles arrive, and a
/// pass over many views is not to be paid at every arrival.
const AHEAD_EVERY: u64 = 4;

/// Creates a geometry server for a **3D Tiles** tileset, bound in-process to
/// one consumer. Convenience over [`in_process_with`]: it wraps the tileset in
/// a shared arena and builds the [`TilesetTree`]/[`TilesetLoader`] pair.
///
/// Drive [`GeometryServer::run`] on whatever executor fits your host
/// (a tokio task, a wasm spawn_local, `futures_executor` in tests) and hand
/// the [`InProcessStream`] to the renderer.
pub fn in_process<F: TileFetcher + 'static>(
    tileset: Tileset,
    fetcher: Arc<F>,
    config: Config,
    budget: BudgetConfig,
) -> Result<(InProcessStream, GeometryServer), ConfigError> {
    let arena = Arc::new(RwLock::new(tileset));
    let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
    let loader: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, fetcher));
    in_process_with(tree, loader, config, budget)
}

/// Creates a geometry server over **any** tile source: a [`TileTree`] for the
/// traversal and a [`TileLoader`] for async content, under the graphics
/// budget `budget`. This is the seam the globe (terrain × imagery) and
/// compositions plug into — the server knows neither glb nor terrain nor
/// grafting.
pub fn in_process_with(
    tree: Box<dyn TileTree>,
    loader: Arc<dyn TileLoader>,
    config: Config,
    budget: BudgetConfig,
) -> Result<(InProcessStream, GeometryServer), ConfigError> {
    let meter: SharedMeter = Arc::new(Mutex::new(BudgetMeter::new(&budget)));
    let budget = Budget::new(budget)?;
    let (client, endpoint) = in_process_pair();
    Ok((
        client,
        GeometryServer {
            tree,
            loader,
            endpoint,
            spawn: Spawn::Inline,
            breather: None,
            clock: default_clock(),
            state: State::new(config, budget, meter),
        },
    ))
}

/// One streaming session: owns the tile tree, the loader, the budget and the
/// in-flight state for one consumer. N sessions = N servers sharing nothing
/// mutable.
pub struct GeometryServer {
    tree: Box<dyn TileTree>,
    loader: Arc<dyn TileLoader>,
    endpoint: ServerEndpoint,
    /// Where the loads run. See [`Spawn`].
    spawn: Spawn,
    /// Handing the host its turn: after this many events in a row with no
    /// wait between them, the pause. See [`GeometryServer::with_breather`].
    breather: Option<(usize, Breather)>,
    clock: Clock,
    state: State,
}

/// Takes back what a session said of itself, once it has gone.
///
/// A session's gauges are set as it works and were left as they last read
/// when it went: a second session in one process — a page that mounts
/// another viewer, a headless run after another — then read the first one's
/// residency and selection as its own until its first pass. A session that
/// has gone holds nothing and selects nothing. Held by [`GeometryServer::run`]
/// for as long as the session runs, so it is let go of however the future
/// ends: finished, or dropped.
struct Gone;

impl Drop for Gone {
    fn drop(&mut self) {
        let m = crate::metrics::metrics();
        for gauge in [
            &m.tiles_visited,
            &m.tiles_culled,
            &m.tiles_selected,
            &m.gaps,
            &m.resident_bytes,
            &m.imagery_bytes,
            &m.loads_in_flight,
            &m.priming_total,
            &m.priming_pending,
            &m.priming_unavailable,
            &m.priming_done,
        ] {
            gauge.set(0);
        }
    }
}

impl GeometryServer {
    /// Runs this session's loads as given. Default: [`Spawn::Inline`].
    #[must_use]
    pub fn with_spawn(mut self, spawn: Spawn) -> Self {
        self.spawn = spawn;
        self
    }

    /// Hands the host its turn after every `every` events handled back to
    /// back, by awaiting `pause()`.
    ///
    /// A load answered from memory completes on its first poll, and so does
    /// the step it triggers, and the next load: a burst can run for seconds
    /// without the loop ever waiting. On a desktop's thread pool that costs a
    /// core. In a browser's worker, which has one thread and receives the
    /// camera as messages, it is a server that hears nothing — no camera, no
    /// acks — while it works. The worker's pause is a timer, which lets its
    /// messages in.
    #[must_use]
    pub fn with_breather(mut self, every: usize, pause: Breather) -> Self {
        self.breather = Some((every.max(1), pause));
        self
    }

    /// Reads the session's time from `clock` rather than the default
    /// monotonic one: a browser's `performance.now`, a test's stepped
    /// clock. The budget's grace times, timeouts, relaxation and retries all
    /// run on it.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The budget's signals, shared: what an executor records its frames
    /// into and what an overlay, a metrics sink or a test reads.
    pub fn meter(&self) -> SharedMeter {
        Arc::clone(&self.state.meter)
    }

    /// Where each load in flight stands in the budget's priority order, as
    /// of its latest step — for a host's pool that orders the work those
    /// loads wait on (a browser's decode workers), reading
    /// [`crate::offload::loading`] when a job is submitted.
    pub fn load_ranks(&self) -> crate::offload::LoadRanks {
        self.state.ranks.clone()
    }

    /// The budget this session runs on.
    pub fn budget_config(&self) -> &BudgetConfig {
        self.state.budget.config()
    }

    /// Runs the session until the consumer goes away.
    pub async fn run(self) {
        let _gone = Gone;
        let GeometryServer {
            tree,
            loader,
            endpoint,
            spawn,
            breather,
            clock,
            mut state,
        } = self;
        let mut loads = Loads::new(&spawn);
        if loads.spawns() {
            // A load landing on another thread must not reshape the tree under
            // a pass: its edits wait for the top of the next one.
            tree.defer_edits();
        }
        let breather = breather.or_else(|| default_breather(&loads));
        let mut busy = 0usize;
        let mut rx = endpoint.rx;
        let tx = endpoint.tx.clone();
        let host = Host {
            tree: tree.as_ref(),
            loader: &loader,
            clock: &clock,
        };

        enum Event {
            Load(Result<Arrival, Aborted>),
            Client(ClientMessage),
            Closed,
        }

        // Which source gets looked at first, flipped every iteration.
        //
        // A fixed order starves one side or the other. Loads first meant that
        // while completions kept arriving the camera was never read at all,
        // and every decision was taken on a view it had already left. Client
        // first starves the mirror image, since a viewer sends a state every
        // frame and completions would go unread.
        //
        // Alternating costs a branch. Within one side, what is already waiting
        // is taken together: the camera's burst keeps only its last state, and
        // the loads that have landed are recorded before the one step that
        // answers them all (`drain_ready`).
        let mut client_first = true;

        // The floor first, before any view: a step at once, with nothing to
        // see yet, lists the pinned levels and starts their loads — so the
        // ground every fallback lands on is on its way before the first
        // camera arrives.
        if let Err(stop) = state.step(&host, &tx, &mut loads) {
            tracing::error!(reason = stop.why("starting"), "geometry server stopping");
            return;
        }

        loop {
            // Whether this turn had to wait: a turn that waited has given the
            // host its turn already.
            let mut waited = false;
            let event = poll_fn(|cx| {
                let mut poll_client =
                    |cx: &mut std::task::Context<'_>| match Pin::new(&mut rx).poll_next(cx) {
                        Poll::Ready(Some(msg)) => Poll::Ready(Some(Event::Client(msg))),
                        Poll::Ready(None) => Poll::Ready(Some(Event::Closed)),
                        Poll::Pending => Poll::Ready(None),
                    };
                let polled = if client_first {
                    if let Poll::Ready(Some(event)) = poll_client(cx) {
                        return Poll::Ready(event);
                    }
                    loads.poll_next(cx).map(Event::Load)
                } else {
                    if let Poll::Ready(done) = loads.poll_next(cx) {
                        return Poll::Ready(Event::Load(done));
                    }
                    match poll_client(cx) {
                        Poll::Ready(Some(event)) => Poll::Ready(event),
                        _ => Poll::Pending,
                    }
                };
                if polled.is_pending() {
                    waited = true;
                }
                polled
            })
            .await;
            client_first = !client_first;
            busy = if waited { 0 } else { busy + 1 };
            if let Some((every, pause)) = &breather {
                if busy >= *every {
                    pause().await;
                    busy = 0;
                }
            }

            // Why this session ended, named rather than implied: a server
            // that has stopped is otherwise indistinguishable from a server
            // with nothing to do.
            let ended: Option<String> = match event {
                Event::Client(first) => {
                    let mut dirty = state.on_client(first, &tx);
                    // Coalesce the burst: a viewer sends a state per frame, and
                    // only the last one is worth deciding for.
                    while let Ok(more) = rx.try_recv() {
                        dirty |= state.on_client(more, &tx);
                    }
                    if dirty {
                        state
                            .step(&host, &tx, &mut loads)
                            .err()
                            .map(|stop| stop.why("answering the consumer"))
                    } else {
                        None
                    }
                }
                Event::Load(Ok((msg, landed))) => {
                    let m = crate::metrics::metrics();
                    m.arrival_wait_seconds.record(landed.elapsed());
                    // Coalesce the arrivals, as the camera's burst is above:
                    // every completion that is already ready is recorded
                    // before the one step that answers them all.
                    state.on_load_done(msg, &tx);
                    let more = drain_ready(&mut state, &mut loads, &tx);
                    m.arrivals_per_pass.record(1 + more);
                    state
                        .step(&host, &tx, &mut loads)
                        .err()
                        .map(|stop| stop.why("answering an arrival"))
                }
                // Cancelled load: the budget cancelled it, and forgot it then.
                Event::Load(Err(Aborted)) => None,
                Event::Closed => Some("the client hung up".to_string()),
            };
            if let Some(why) = ended {
                tracing::error!(
                    reason = why,
                    traversals = crate::metrics::metrics().traversals.get(),
                    "geometry server stopping"
                );
                break;
            }
        }
        // Spawned loads outlive the loop otherwise: nobody would read them.
        for flight in state.in_flight.values() {
            flight.handle.abort();
        }
    }
}

/// Records every load completion that is already ready, without waiting.
///
/// Each of a burst's arrivals answered by a step of its own would decide on
/// the first and re-decide on every one after it — a consumer drains every
/// queued message before it draws, so it only ever acts on the last. Bounded
/// by the budget's quotas: at most that many loads can be in flight, so a
/// drain cannot outlast the burst it was called for, and the camera is read
/// again after it. A batch counts as one event for the breather. Returns how
/// many it recorded, for `arrivals_per_pass`.
fn drain_ready(state: &mut State, loads: &mut Loads, tx: &UnboundedSender<ServerMessage>) -> u64 {
    let m = crate::metrics::metrics();
    let cfg = state.budget.config();
    let bound = cfg
        .max_in_flight
        .iter()
        .fold(cfg.max_local_reads, |a, (_, n)| a.saturating_add(n));
    let mut recorded = 0;
    for _ in 0..bound {
        match loads.try_next() {
            Some(Ok((msg, landed))) => {
                m.arrival_wait_seconds.record(landed.elapsed());
                state.on_load_done(msg, tx);
                recorded += 1;
            }
            // Cancelled: forgotten when it was cancelled, as in the loop.
            Some(Err(Aborted)) => {}
            // Nothing in flight, or nothing ready yet.
            None => break,
        }
    }
    recorded
}

/// The pause a session takes when its host gave none: for spawned loads, a
/// turn handed back to the runtime every so often, so a server answering a
/// long burst from memory does not keep its worker from the tasks queued on
/// it. Inline, none — the host decides, as it always has.
fn default_breather(_loads: &Loads) -> Option<(usize, Breather)> {
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    if _loads.spawns() {
        let pause: Breather = Arc::new(|| Box::pin(tokio::task::yield_now()));
        return Some((16, pause));
    }
    None
}

/// Why the run loop unwinds. Both end the session; only the reason differs,
/// and only the reason is worth a log line.
enum Stop {
    /// The consumer went away: nothing left to send to.
    Gone,
    /// The session cannot go on: a batch frame's load failed (a film never
    /// ships a hole), or the tree could not take a load's edit.
    Failed {
        resource: Option<ResourceId>,
        message: String,
    },
}

impl Stop {
    /// What to say about it, given what the session was doing at the time.
    fn why(&self, during: &str) -> String {
        match self {
            Self::Gone => format!("the consumer went away while {during}"),
            Self::Failed { resource, message } => format!(
                "{resource:?} could not be loaded, so the session ends here \
                 rather than serving ground with a hole in it: {message}"
            ),
        }
    }
}

/// What a step reads but does not own.
struct Host<'a> {
    tree: &'a dyn TileTree,
    loader: &'a Arc<dyn TileLoader>,
    clock: &'a Clock,
}

/// A load in flight: its generation, and how to stop it.
struct Flight {
    gen: Generation,
    handle: AbortHandle,
}

/// The loader's presence index, as the budget's oracle, with every answer
/// noted for the step's record.
///
/// Answers are kept for a window of [`PRESENCE_FOR`]: the budget asks of
/// every drape it weighs, at every level it weighs it, and a loader answers
/// a drape by working out every imagery tile it would stack — in a browser's
/// worker, two thirds of the thread near the ground, the loads' own replies
/// waiting seconds behind it. A store's index changes by what is fetched
/// into it, which a load answers anyway; an answer a second old misplaces a
/// read between the store and the network at worst, never a tile.
///
/// The answers change only when the window does: the clock enters the next
/// [`PRESENCE_FOR`], or the loader's epoch moves on (its content, and so
/// what its stores hold, changed). Then every answer is dropped and the
/// oracle's [`generation`](PresenceOracle::generation) moves on with them;
/// within a window it is the same, and the budget does not read again the
/// levels it chose by asking (O1.4).
struct Presences<'a> {
    loader: &'a dyn TileLoader,
    asked: std::cell::RefCell<Vec<(ResourceId, Presence)>>,
    kept: &'a std::cell::RefCell<HashMap<ResourceId, Presence>>,
    generation: u64,
}

/// The window the loader's answers are kept for.
const PRESENCE_FOR: Micros = Micros::from_millis(1_000);

impl PresenceOracle for Presences<'_> {
    fn holds(&self, r: ResourceId) -> Presence {
        let kept = self.kept.borrow().get(&r).copied();
        let p = match kept {
            Some(p) => p,
            None => {
                let p = self.loader.holds(r);
                self.kept.borrow_mut().insert(r, p);
                p
            }
        };
        self.asked.borrow_mut().push((r, p));
        p
    }

    fn generation(&self) -> Option<u64> {
        Some(self.generation)
    }
}

/// The session's state, owned: everything a step reads and changes.
struct State {
    config: Config,
    budget: Budget,
    meter: SharedMeter,
    /// Loads in flight, by resource.
    in_flight: BTreeMap<ResourceId, Flight>,
    /// Where each of them stands in the budget's order, as of the last step:
    /// for a pool that orders the work they wait on.
    ranks: crate::offload::LoadRanks,
    /// The loader's answers about presence in the current window, the
    /// window (its number and the loader's epoch), and the answers'
    /// generation (see [`Presences`]).
    presence: std::cell::RefCell<HashMap<ResourceId, Presence>>,
    presence_window: Option<(u64, u64)>,
    presence_generation: u64,
    /// The decoded copies the budget counts (its decoded tier): delivered
    /// from here, again after an eviction, until the budget releases them.
    decoded: BTreeMap<ResourceId, (Generation, ResourceContent)>,
    /// The steepest ground of each terrain tile whose mesh has landed, as
    /// the cosine of its steepest face against the globe's up there (see
    /// [`steepest_ground`]): what lets a slope's imagery need be read square.
    /// Kept once measured, for as long as the session: a tile still drawn
    /// after its decoded copy is released keeps its need, and an entry is a
    /// few bytes.
    steepest: HashMap<TileId, f64>,
    /// What the host handed the budget since the last step, for its record.
    events: Vec<BudgetEvent>,
    /// The loader's epoch the drapes were last refreshed for.
    epoch: u64,
    views: Vec<ViewState>,
    view_generation: u64,
    /// Set when the camera moved since the last step.
    view_moved: bool,
    out: TraversalOutput,
    /// The views of the frames to come ([`ClientMessage::Ahead`]), and the
    /// pass over them that lists the need's ring: a pass of its own, with
    /// its own memo, reading the current pass's hysteresis memory and
    /// writing nothing the current pass reads.
    ahead: Vec<ViewState>,
    ahead_out: TraversalOutput,
    ahead_near: NearestContent,
    /// Set when the views ahead changed since the pass over them.
    ahead_moved: bool,
    /// What the last step drew, for the traversal's hold logic.
    rendered_last: HashSet<TileId>,
    /// What the last pass under the previous views chose at its own level:
    /// the refine hysteresis's memory (`Config::refine_hysteresis`). Moved on
    /// only when the camera moves, so every pass of one frame refines against
    /// the same memory.
    chosen_before: HashSet<TileId>,
    /// The scene-wide distance a uniform-detail pass is priced at, kept from
    /// one pass to the next while the camera and the tree's shape hold still.
    near: NearestContent,
    /// The relaxation the budget fed back, thousandths (1000 = none).
    sse_scale_milli: u32,
    /// The floor, by the tree's shape at the last look: every tile at or
    /// above the pinned level that has content.
    floor: Vec<TileId>,
    floor_revision: Option<Option<u64>>,
    priming_reported: Option<Priming>,
    frame: u64,
    steps: u64,
    published_at: Option<Micros>,
    /// The platform's signals as last handed to the budget, for its record.
    pressure: crate::budget::Pressure,
    thermal: crate::budget::Thermal,
    /// The need the budget last read, `None` before the first step: a step
    /// tells the budget what changed since it ([`NeedDelta`]), not the
    /// whole need again.
    need_last: Option<NeedSet>,
    /// What changed, in a buffer kept across steps.
    need_delta: NeedDelta,
}

impl State {
    fn new(config: Config, budget: Budget, meter: SharedMeter) -> Self {
        State {
            config,
            budget,
            meter,
            in_flight: BTreeMap::new(),
            ranks: crate::offload::LoadRanks::default(),
            presence: std::cell::RefCell::default(),
            presence_window: None,
            presence_generation: 0,
            decoded: BTreeMap::new(),
            steepest: HashMap::new(),
            events: Vec::new(),
            epoch: 0,
            views: Vec::new(),
            view_generation: 0,
            view_moved: false,
            out: TraversalOutput::default(),
            ahead: Vec::new(),
            ahead_out: TraversalOutput::default(),
            ahead_near: NearestContent::default(),
            ahead_moved: false,
            rendered_last: HashSet::new(),
            chosen_before: HashSet::new(),
            near: NearestContent::default(),
            sse_scale_milli: 1_000,
            floor: Vec::new(),
            floor_revision: None,
            priming_reported: None,
            frame: 0,
            steps: 0,
            published_at: None,
            pressure: crate::budget::Pressure::Normal,
            thermal: crate::budget::Thermal::Nominal,
            need_last: None,
            need_delta: NeedDelta::new(),
        }
    }

    /// Hands a client message to the budget. Returns whether a step is due.
    fn on_client(&mut self, msg: ClientMessage, tx: &UnboundedSender<ServerMessage>) -> bool {
        match msg {
            ClientMessage::ViewerState { views, generation } => {
                self.views = views;
                self.view_generation = generation;
                self.view_moved = true;
            }
            ClientMessage::Ahead { views } => {
                self.ahead = views;
                self.ahead_moved = true;
            }
            ClientMessage::Ack {
                resource,
                gen,
                bytes,
            } => {
                self.budget.on_drawable(resource, gen, Bytes(bytes));
                self.events
                    .push(BudgetEvent::Drawable(resource, gen, Bytes(bytes)));
            }
            ClientMessage::Discarded { resource, gen } => {
                self.budget.on_discarded(resource, gen);
                self.events.push(BudgetEvent::Discarded(resource, gen));
            }
            ClientMessage::Pressure(p) => {
                self.pressure = p;
                self.budget.set_pressure(p);
                self.events.push(BudgetEvent::Pressure(p));
            }
            ClientMessage::Thermal(t) => {
                self.thermal = t;
                self.budget.set_thermal(t);
                self.events.push(BudgetEvent::Thermal(t));
            }
            ClientMessage::Retarget { total_bytes } => {
                self.budget.retarget(Bytes(total_bytes));
                self.events.push(BudgetEvent::Retarget(Bytes(total_bytes)));
            }
            // Between frames, now: against what the last step drew.
            // Announced as any eviction is.
            ClientMessage::ReleaseBeyond { bytes } => {
                let released = self.budget.release_beyond(Bytes(bytes));
                self.events.push(BudgetEvent::ReleaseBeyond(
                    Bytes(bytes),
                    released.gpu.len() as u32,
                ));
                // Let go whole: the decoded copy goes with the consumer's.
                for r in &released.decoded {
                    self.decoded.remove(r);
                }
                if !released.gpu.is_empty() {
                    crate::metrics::metrics()
                        .tiles_evicted
                        .add(released.gpu.len() as u64);
                    // A send to a consumer that has gone is noticed at the
                    // next step, which stops the session.
                    let _ = tx.unbounded_send(ServerMessage::Evict {
                        resources: released.gpu,
                    });
                }
                return false;
            }
        }
        true
    }

    /// A load ended: its content becomes the budget's decoded copy, or its
    /// failure is the budget's to retry.
    fn on_load_done(&mut self, (r, gen, result): LoadMsg, tx: &UnboundedSender<ServerMessage>) {
        let m = crate::metrics::metrics();
        // Only the load the budget is waiting for: a result that lands after
        // its load was cancelled is not this generation's any more.
        match self.in_flight.get(&r) {
            Some(f) if f.gen == gen => {
                self.in_flight.remove(&r);
            }
            _ => return,
        }
        m.loads_in_flight.set(self.in_flight.len() as u64);
        match result {
            Ok(fetched) => {
                let (content, cost) = match fetched {
                    Fetched::Mesh(Loaded::Content(c)) => {
                        if let (ResourceId::Mesh(tile), Some(cos)) = (r, steepest_ground(&c)) {
                            self.steepest.insert(tile, cos);
                        }
                        let cost = c.cost();
                        (ResourceContent::Mesh(Arc::new(c)), cost)
                    }
                    // The load grew the tree (an external tileset grafted):
                    // nothing to draw here, the revealed children load on
                    // their own. Recorded as an empty mesh, so the budget
                    // counts the tile resident, as the traversal does.
                    Fetched::Mesh(Loaded::Expanded) => {
                        let empty = DecodedTileContent {
                            meshes: Vec::new(),
                            textures: Vec::new(),
                            imagery: Vec::new(),
                            withheld_drape: None,
                            local_origin_ecef: glam::DVec3::ZERO,
                            transform_local: glam::Mat4::IDENTITY,
                        };
                        let cost = empty.cost();
                        (ResourceContent::Mesh(Arc::new(empty)), cost)
                    }
                    Fetched::Drape(d) => {
                        let cost = d.cost();
                        (ResourceContent::Drape(Arc::new(d)), cost)
                    }
                };
                m.loads_completed.inc();
                crate::det!("load_ok", resource = format!("{r:?}"));
                self.decoded.insert(r, (gen, content));
                self.budget.on_loaded(r, gen, cost);
                self.events.push(BudgetEvent::Loaded(r, gen, cost));
            }
            Err(e) => {
                m.loads_failed.inc();
                crate::det!("load_err", resource = format!("{r:?}"));
                tracing::warn!(?r, "load failed: {e}");
                self.budget.on_failed(r, gen, Failure::Transient);
                self.events
                    .push(BudgetEvent::Failed(r, gen, Failure::Transient));
                let _ = tx.unbounded_send(ServerMessage::Error {
                    resource: Some(r),
                    message: e.to_string(),
                });
            }
        }
    }

    /// What the tree says of a tile, for the wire.
    fn ancestry(tree: &dyn TileTree, tile: TileId) -> Ancestry {
        Ancestry {
            level: tree.level(tile),
            parent: tree.parent(tile),
        }
    }

    /// The floor: every tile with content down to the budget's pinned level,
    /// listed every step so it is loaded first and never evicted. Walked
    /// again only when the tree's shape may have changed.
    fn floor(&mut self, tree: &dyn TileTree) -> &[TileId] {
        let revision = tree.revision();
        if self.floor_revision != Some(revision) || revision.is_none() {
            self.floor_revision = Some(revision);
            self.floor.clear();
            if let Some(pinned) = self.budget.config().pinned_level {
                let mut frontier = tree.roots();
                for _ in 0..=pinned {
                    let mut next = Vec::new();
                    for tile in frontier {
                        if tree.properties(tile).has_content {
                            self.floor.push(tile);
                        }
                        next.extend(tree.children(tile));
                    }
                    if next.is_empty() {
                        break;
                    }
                    frontier = next;
                }
            }
        }
        &self.floor
    }

    /// One tile's need, over every view: the largest screen-space error and
    /// centrality, the smallest distance, and the finest imagery footprint
    /// any view gives it (spec §13.3).
    fn need_of(
        host: &Host<'_>,
        steepest: &HashMap<TileId, f64>,
        views: &[ViewState],
        tau: f64,
        tile: TileId,
    ) -> Need {
        let tree = host.tree;
        let props = tree.properties(tile);
        let mut need = Need::new(tree.level(tile), tree.mesh_class(tile));
        if let Some(p) = tree.parent(tile) {
            need = need.with_parent(p);
        }
        let center = props.bounding_volume.center();
        let mut distance = f64::INFINITY;
        let mut sse = 0.0f64;
        let mut centrality = 0.0f64;
        let mut footprint: Option<TexelFootprint> = None;
        let reach = host.loader.drapes(tile);
        for v in views {
            let d = props.bounding_volume.distance_to_point(v.position());
            distance = distance.min(d);
            sse = sse.max(v.screen_space_error(props.geometric_error, d));
            let to = center - v.position();
            let len = to.length();
            let cos_off = if len > 1e-9 {
                (to / len).dot(v.direction()).clamp(-1.0, 1.0)
            } else {
                1.0
            };
            centrality = centrality.max((1.0 + cos_off) / 2.0);
            if let Some(reach) = &reach {
                // A texel seen obliquely covers fewer pixels. The angle is
                // the view ray's against the globe's up at the tile, eased by
                // the tile's steepest ground: a slope facing the eye is seen
                // square however grazing its up says (see `seen_on_slope`).
                let up = center.normalize_or_zero();
                let cos_obliquity = if len > 1e-9 && up != glam::DVec3::ZERO {
                    let against_up = (to / len).dot(up).abs();
                    match steepest.get(&tile) {
                        Some(&steepest) => seen_on_slope(against_up, steepest),
                        None => against_up,
                    }
                    .clamp(0.05, 1.0)
                } else {
                    1.0
                };
                if let Ok(fp) = TexelFootprint::new(
                    d.max(1.0),
                    cos_obliquity,
                    v.focal_px(),
                    reach.texel_l0_m,
                    1.0,
                ) {
                    if footprint.is_none_or(|f| fp.ratio() > f.ratio()) {
                        footprint = Some(fp);
                    }
                }
            }
        }
        if views.is_empty() {
            distance = 0.0;
        }
        let excess = if sse > 0.0 && tau > 0.0 {
            (sse / tau).log2().clamp(-16.0, 16.0)
        } else {
            -16.0
        };
        need = need
            .with_sse_excess(excess as f32)
            .with_centrality(centrality as f32)
            .with_distance(distance.min(f64::from(f32::MAX)) as f32);
        if let Some(reach) = reach {
            need = need.with_rect(reach.rect);
            // No view: the floor, wanted at its coarsest.
            let footprint = footprint
                .or_else(|| TexelFootprint::new(1.0e9, 1.0, 1.0, reach.texel_l0_m, 1.0).ok());
            if let Some(footprint) = footprint {
                need = need.with_imagery(ImageryNeed {
                    footprint,
                    layers: reach.layers,
                });
            }
        }
        need
    }

    /// This step's need, from the traversal's output (spec §5, §11.1):
    /// drawn — what the traversal draws or wants drawn; blockers — what a
    /// held REPLACE waits on, resident or not; ancestors — the chain of
    /// everything listed, what the traversal preloads, and the floor.
    fn need(&mut self, host: &Host<'_>) -> NeedSet {
        let tree = host.tree;
        let tau = self.config.maximum_screen_space_error * f64::from(self.sse_scale_milli) / 1000.0;
        let blockers: BTreeSet<TileId> = self.out.blockers.iter().copied().collect();
        let mut drawn: BTreeSet<TileId> = self.out.selected.iter().map(|(t, _)| *t).collect();
        for r in &self.out.requests {
            if r.group != PriorityGroup::Preload && !blockers.contains(&r.tile) {
                drawn.insert(r.tile);
            }
        }
        let blockers: BTreeSet<TileId> = blockers.difference(&drawn).copied().collect();
        let mut ancestors: BTreeSet<TileId> = BTreeSet::new();
        for t in drawn.iter().chain(&blockers) {
            let mut cur = tree.parent(*t);
            while let Some(p) = cur {
                if drawn.contains(&p) || blockers.contains(&p) || !ancestors.insert(p) {
                    break;
                }
                cur = tree.parent(p);
            }
        }
        // Structural tiles carry nothing to load.
        ancestors.retain(|t| tree.properties(*t).has_content);
        for r in &self.out.requests {
            if r.group == PriorityGroup::Preload
                && !drawn.contains(&r.tile)
                && !blockers.contains(&r.tile)
            {
                ancestors.insert(r.tile);
            }
        }
        let floor: Vec<TileId> = self.floor(tree).to_vec();
        for t in floor {
            if !drawn.contains(&t) && !blockers.contains(&t) {
                ancestors.insert(t);
            }
        }
        // The ring: what the views ahead select and ask for, beyond what
        // the current ones need — loaded when they leave a slot, never drawn.
        let ring: BTreeSet<TileId> = if self.ahead.is_empty() {
            BTreeSet::new()
        } else {
            self.ahead_out
                .selected
                .iter()
                .map(|(t, _)| *t)
                .chain(self.ahead_out.requests.iter().map(|r| r.tile))
                .filter(|t| {
                    !drawn.contains(t)
                        && !blockers.contains(t)
                        && !ancestors.contains(t)
                        && tree.properties(*t).has_content
                })
                .collect()
        };
        let list = |set: &BTreeSet<TileId>, views: &[ViewState]| -> Vec<(TileId, Need)> {
            set.iter()
                .map(|t| (*t, Self::need_of(host, &self.steepest, views, tau, *t)))
                .collect()
        };
        let views = &self.views;
        NeedSet {
            drawn: list(&drawn, views),
            blockers: list(&blockers, views),
            ancestors: list(&ancestors, views),
            ring: list(&ring, &self.ahead),
        }
    }

    /// Starts one load the budget decided on.
    fn start(&mut self, host: &Host<'_>, r: ResourceId, gen: Generation, loads: &mut Loads) {
        let loader = Arc::clone(host.loader);
        let (handle, registration) = AbortHandle::new_pair();
        // Named while it is polled: the work it hands a pool says which load
        // it is for, and the pool orders it by that load's rank.
        let fut: BoxLoadFut = Box::pin(Woken::new(
            crate::offload::ForLoad::new(r, async move {
                let result = match r {
                    ResourceId::Mesh(t) => loader.load(t).await.map(Fetched::Mesh),
                    ResourceId::Drape { tile, layer, level } => loader
                        .load_drape(tile, layer, level)
                        .await
                        .map(Fetched::Drape),
                };
                (r, gen, result)
            }),
            &crate::metrics::metrics().load_wake_wait_seconds,
        ));
        loads.push(Abortable::new(fut, registration), &handle);
        self.in_flight.insert(r, Flight { gen, handle });
        let m = crate::metrics::metrics();
        m.loads_started.inc();
        m.loads_by_level.inc(host.tree.level(r.tile()));
        m.loads_in_flight.set(self.in_flight.len() as u64);
    }

    /// The floor's progress, for a host that holds its first frame back
    /// until the ground is covered: every floor mesh and drape the budget
    /// lists, and those not yet on the GPU.
    fn priming(&self, need: &NeedSet) -> Priming {
        let pinned = self.budget.config().pinned_level;
        let mut total = 0u32;
        let mut outstanding = 0u32;
        for (_, t, n) in need.entries() {
            if !pinned.is_some_and(|p| n.level <= p) {
                continue;
            }
            let mut resources = vec![ResourceId::Mesh(t)];
            if let Some(img) = &n.imagery {
                resources.extend(
                    img.layers
                        .iter()
                        .map(|l| ResourceId::drape(t, l.layer, n.level)),
                );
            }
            for r in resources {
                total += 1;
                let drawable = self.budget.resource(r).is_some_and(|v| {
                    matches!(v.state, crate::budget::ResourceState::Drawable) || v.gpu > Bytes::ZERO
                });
                if !drawable {
                    outstanding += 1;
                }
            }
        }
        Priming {
            total,
            unavailable: 0,
            outstanding,
        }
    }

    /// One step: traverse, list the need, decide once, execute.
    fn step(
        &mut self,
        host: &Host<'_>,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut Loads,
    ) -> Result<(), Stop> {
        let tree = host.tree;
        let camera_moved = std::mem::take(&mut self.view_moved);
        // What loads changed in the tree's shape since the last step, applied
        // before anything reads it — so the whole step walks one tree.
        if let Some((tile, why)) = tree.apply_deferred_edits().into_iter().next() {
            let message = format!("the tree could not take {tile:?}: {why}");
            let _ = tx.unbounded_send(ServerMessage::Error {
                resource: Some(ResourceId::Mesh(tile)),
                message: message.clone(),
            });
            return Err(Stop::Failed {
                resource: Some(ResourceId::Mesh(tile)),
                message,
            });
        }
        self.frame += 1;
        self.steps += 1;
        // A change of what the loader drapes (a switch of imagery) refreshes
        // every drape, new before old; meshes are untouched.
        let epoch = host.loader.epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.budget.new_epoch(Class::Drape, None);
            self.events.push(BudgetEvent::NewEpoch(Class::Drape, None));
        }
        if camera_moved {
            // `out` still holds the last pass under the views just left.
            self.chosen_before.clear();
            self.chosen_before.extend(self.out.chosen.iter().copied());
        }
        // This step's arrivals first, so the traversal walks what they made
        // drawable; `decide` below, at the same instant, decides as if it had
        // applied them itself.
        let now = (host.clock)();
        self.budget.apply_arrivals(now);
        // The traversal, relaxed as the budget asks, against the budget's
        // residency: what is drawable (interactive) or loaded (batch).
        let config = Config {
            maximum_screen_space_error: self.config.maximum_screen_space_error
                * f64::from(self.sse_scale_milli)
                / 1000.0,
            ..self.config.clone()
        };
        let started = crate::metrics::stamp();
        {
            let residency = self.budget.residency();
            traverse_with(
                tree,
                &residency,
                &self.views,
                &config,
                self.frame,
                &self.rendered_last,
                &self.chosen_before,
                &mut self.near,
                &mut self.out,
            );
        }
        // The pass over the views ahead, when they moved, and again every few
        // steps while their tiles arrive: it reads the residency and the
        // current pass's memory, and writes only its own.
        if !self.ahead.is_empty()
            && (std::mem::take(&mut self.ahead_moved) || self.steps.is_multiple_of(AHEAD_EVERY))
        {
            let residency = self.budget.residency();
            traverse_with(
                tree,
                &residency,
                &self.ahead,
                &config,
                self.frame,
                &self.rendered_last,
                &self.chosen_before,
                &mut self.ahead_near,
                &mut self.ahead_out,
            );
        }
        let walked = started.elapsed();
        // Every pass of every session these tests drive is checked against
        // the reference pass, which works everything out from scratch.
        #[cfg(test)]
        tests::matches_the_reference_pass(
            tree,
            &self.budget.residency(),
            &self.views,
            &config,
            self.frame,
            &self.rendered_last,
            &self.chosen_before,
            &self.near,
            &self.out,
        );
        crate::det!(
            "pass",
            n = self.frame,
            selected = self.out.selected.len(),
            sel_digest = crate::determinism::digest(self.out.selected.iter().map(|(t, _)| t.0)),
            requested = self.out.requests.len(),
            req_digest = crate::determinism::digest(self.out.requests.iter().map(|r| r.tile.0)),
            visited = self.out.stats.visited,
            culled = self.out.stats.culled,
            gaps = self.out.stats.gaps,
            deferred = self.out.stats.deferred_subtrees,
            held_but_drawn = self.out.stats.held_but_drawn,
            in_flight = self.in_flight.len(),
            camera_moved = camera_moved,
        );
        let need = self.need(host);
        // A new window drops every answer, and their generation moves on.
        let window = (now.0 / PRESENCE_FOR.0, host.loader.epoch());
        if self.presence_window != Some(window) {
            self.presence_window = Some(window);
            self.presence_generation = self.presence_generation.wrapping_add(1);
            self.presence.get_mut().clear();
        }
        // Taken for the decision and put back after it.
        let kept = std::cell::RefCell::new(std::mem::take(self.presence.get_mut()));
        let presence = Presences {
            loader: host.loader.as_ref(),
            asked: std::cell::RefCell::new(Vec::new()),
            kept: &kept,
            generation: self.presence_generation,
        };
        // The first step reads the whole need; every later one what changed
        // since the one before, which the budget kept.
        let input = match &self.need_last {
            Some(last) => {
                self.need_delta.diff(last, &need);
                FrameInput::from_delta(self.frame, now, &self.need_delta)
            }
            None => FrameInput::new(self.frame, now, &need),
        };
        let deciding = crate::metrics::stamp();
        let d = self.budget.decide(input.with_presence(&presence));
        let decide_wall = Micros(deciding.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
        self.sse_scale_milli = d.sse_scale_milli;

        let m = crate::metrics::metrics();
        m.traversals.inc();
        m.traversal_seconds.record(walked);
        m.tiles_visited.set(u64::from(self.out.stats.visited));
        m.tiles_culled.set(u64::from(self.out.stats.culled));
        m.tiles_selected.set(d.draw.items.len() as u64);
        m.gaps.set(u64::from(self.out.stats.gaps));
        m.selected_by_level.clear();
        for item in &d.draw.items {
            m.selected_by_level.inc(tree.level(item.mesh.tile()));
        }
        m.queued_by_level.clear();
        for s in &d.start {
            m.queued_by_level.inc(tree.level(s.resource.tile()));
        }
        let ledger = self.budget.ledger();
        m.resident_bytes.set(
            Class::ALL
                .iter()
                .fold(Bytes::ZERO, |a, c| a.plus(ledger.gpu[*c]))
                .0,
        );
        m.imagery_bytes.set(ledger.gpu[Class::Drape].0);

        let result = self.execute(host, &d, tx, loads);

        // What was drawn, for the next pass's hold logic.
        self.rendered_last.clear();
        self.rendered_last
            .extend(d.draw.items.iter().map(|i| i.mesh.tile()));

        // The record, for every sink at once.
        let events = std::mem::take(&mut self.events);
        let asked = presence.asked.into_inner();
        *self.presence.get_mut() = kept.into_inner();
        if let Ok(mut meter) = self.meter.lock() {
            meter.step(&StepRecord {
                step: self.steps - 1,
                frame: self.frame,
                now,
                views_changed: camera_moved,
                eyes: self.views.len().min(255) as u8,
                need: &need,
                events: &events,
                presence: &asked,
                decisions: &d,
                ledger,
                pressure: self.pressure,
                thermal: self.thermal,
                decide_wall: Some(decide_wall),
            });
            if self
                .published_at
                .is_none_or(|at| now.since(at) >= PUBLISH_EVERY)
            {
                self.published_at = Some(now);
                crate::metrics::publish_budget(meter.publish());
            }
        }
        // The loads in flight, in the budget's order of this step; those it
        // no longer ranks (not wanted, waiting out their grace) after them.
        let mut ranked: Vec<(Option<crate::budget::PriorityKey>, ResourceId)> = self
            .in_flight
            .keys()
            .map(|r| (self.budget.resource(*r).and_then(|v| v.priority), *r))
            .collect();
        ranked.sort_by(|a, b| match (&a.0, &b.0) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        self.ranks.publish(ranked.into_iter().map(|(_, r)| r));
        let priming = self.priming(&need);
        self.need_last = Some(need);
        let mm = crate::metrics::metrics();
        mm.priming_total.set(u64::from(priming.total));
        mm.priming_pending.set(u64::from(priming.outstanding));
        mm.priming_done.set(u64::from(priming.settled()));
        if self.priming_reported != Some(priming) {
            self.priming_reported = Some(priming);
            tx.unbounded_send(ServerMessage::Priming(priming))
                .map_err(|_| Stop::Gone)?;
        }
        result
    }

    /// Executes one step's decisions, in their order: cancels, deliveries,
    /// releases, starts, the draw set and the plan, then the evictions —
    /// after the draw that stops using them.
    fn execute(
        &mut self,
        host: &Host<'_>,
        d: &Decisions,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut Loads,
    ) -> Result<(), Stop> {
        let tree = host.tree;
        let m = crate::metrics::metrics();
        // Cancels: a load in flight is stopped; a delivery not yet uploaded
        // is the consumer's to drop.
        let mut withdrawn = Vec::new();
        for (r, g) in &d.cancel {
            match self.in_flight.get(r) {
                Some(f) if f.gen == *g => {
                    f.handle.abort();
                    self.in_flight.remove(r);
                    m.loads_cancelled.inc();
                }
                _ => withdrawn.push((*r, *g)),
            }
        }
        m.loads_in_flight.set(self.in_flight.len() as u64);
        if !withdrawn.is_empty() {
            tx.unbounded_send(ServerMessage::Cancel {
                resources: withdrawn,
            })
            .map_err(|_| Stop::Gone)?;
        }
        // Deliveries, from the decoded tier, in the plan's priority order.
        for (r, g) in &d.deliver {
            let Some((held, content)) = self.decoded.get(r) else {
                tracing::error!(
                    ?r,
                    "the budget delivered a resource the session does not hold"
                );
                continue;
            };
            if held != g {
                tracing::error!(
                    ?r,
                    "the budget delivered another generation than the one held"
                );
                continue;
            }
            let class = match r {
                ResourceId::Mesh(t) => tree.mesh_class(*t),
                ResourceId::Drape { .. } => Class::Drape,
            };
            tx.unbounded_send(ServerMessage::Content {
                resource: *r,
                gen: *g,
                class,
                ancestry: Self::ancestry(tree, r.tile()),
                content: content.clone(),
            })
            .map_err(|_| Stop::Gone)?;
        }
        for r in &d.release {
            self.decoded.remove(r);
        }
        for s in &d.start {
            self.start(host, s.resource, s.gen, loads);
        }
        // The draw set, with the shape of the tree around it.
        let mut named: BTreeSet<TileId> = BTreeSet::new();
        for item in &d.draw.items {
            named.insert(item.mesh.tile());
            named.extend(item.serves.iter().copied());
            named.extend(item.drapes.iter().map(|x| x.resource.tile()));
        }
        let selecting = crate::metrics::stamp();
        let sent = tx.unbounded_send(ServerMessage::Select {
            draw: d.draw.clone(),
            plan: d.upload_plan.clone(),
            ancestry: named
                .into_iter()
                .map(|t| (t, Self::ancestry(tree, t)))
                .collect(),
            stats: self.out.stats,
            hard_limit: d.stats.hard_limit,
            frame_ready: d.frame_ready,
            converged: d.stats.converged,
            lost: d.stats.lost,
            bare: d.stats.bare,
            generation: self.view_generation,
        });
        m.select_seconds.record(selecting.elapsed());
        sent.map_err(|_| Stop::Gone)?;
        if !d.evict.is_empty() {
            m.tiles_evicted.add(d.evict.len() as u64);
            crate::det!(
                "evict",
                count = d.evict.len(),
                digest = crate::determinism::digest(d.evict.iter().map(|r| r.tile().0)),
            );
            tx.unbounded_send(ServerMessage::Evict {
                resources: d.evict.clone(),
            })
            .map_err(|_| Stop::Gone)?;
        }
        if let Some(fatal) = &d.fatal {
            let resource = fatal.resource();
            let message = fatal.to_string();
            tx.unbounded_send(ServerMessage::Error {
                resource,
                message: message.clone(),
            })
            .map_err(|_| Stop::Gone)?;
            return Err(Stop::Failed { resource, message });
        }
        Ok(())
    }
}
/// The cosine of a terrain tile's steepest face against the globe's up at
/// the tile: 1 for flat ground, 0 for a vertical face. Skirts are left out
/// (they are vertical by construction). `None` for content with no
/// triangle to measure.
///
/// Computed with `+ − × ÷ √` and comparisons only, as the imagery level it
/// feeds (`budget::imagery`): the same mesh gives the same value everywhere.
fn steepest_ground(content: &DecodedTileContent) -> Option<f64> {
    let origin = content.local_origin_ecef;
    let up = origin.normalize_or_zero();
    if up == glam::DVec3::ZERO {
        return None;
    }
    let mut steepest: Option<f64> = None;
    for mesh in &content.meshes {
        let ground = crate::stitch::without_skirts(mesh, origin);
        let at = |i: u32| glam::Vec3::from(ground.positions[i as usize]).as_dvec3();
        for face in ground.indices.as_chunks::<3>().0 {
            let normal = (at(face[1]) - at(face[0])).cross(at(face[2]) - at(face[0]));
            let length = normal.length();
            if length.is_nan() || length <= 0.0 {
                continue;
            }
            let cos = (normal.dot(up) / length).abs().min(1.0);
            steepest = Some(steepest.map_or(cos, |s| s.min(cos)));
        }
    }
    steepest
}

/// The cosine of the angle a surface is seen at, for a view ray at
/// `against_up` (its cosine against the globe's up) over ground whose
/// steepest face is at `steepest` (its cosine against the same up).
///
/// The ground may tilt toward the eye by as much as its steepest face, so the
/// ray's angle from up, `β`, is eased by that tilt, `α`: square (1) when
/// `β ≤ α`, `cos(β − α)` beyond. Flat ground (`α = 0`) is seen at `β`, as
/// before; a slope is never needed coarser than its steepest face allows.
/// `cos(β − α) = cos β cos α + sin β sin α`, so no trigonometric function
/// is called and the value is the same everywhere.
fn seen_on_slope(against_up: f64, steepest: f64) -> f64 {
    let (cos_b, cos_a) = (against_up.clamp(0.0, 1.0), steepest.clamp(0.0, 1.0));
    if cos_b >= cos_a {
        return 1.0;
    }
    let sin = |c: f64| (1.0 - c * c).max(0.0).sqrt();
    (cos_b * cos_a + sin(cos_b) * sin(cos_a)).min(1.0)
}

#[cfg(test)]
mod tests {
    /// Flat ground is seen at the ray's own angle; ground whose steepest
    /// face tilts as far as the ray is seen square; in between, the ray's
    /// angle less the tilt.
    #[test]
    fn a_slope_eases_the_angle_its_imagery_is_needed_at() {
        let deg = |d: f64| d.to_radians().cos();
        let near = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(
            near(super::seen_on_slope(deg(70.0), 1.0), deg(70.0)),
            "flat: as before"
        );
        assert_eq!(
            super::seen_on_slope(deg(70.0), deg(75.0)),
            1.0,
            "a face steeper than the ray: square"
        );
        assert_eq!(super::seen_on_slope(deg(70.0), deg(70.0)), 1.0);
        assert!(
            near(super::seen_on_slope(deg(70.0), deg(30.0)), deg(40.0)),
            "70° less 30°"
        );
        assert!(
            near(super::seen_on_slope(1.0, deg(30.0)), 1.0),
            "straight down stays square"
        );
        for (b, a) in [(0.0, 0.0), (0.3, 0.9), (0.9, 0.3), (1.0, 0.0)] {
            let s = super::seen_on_slope(b, a);
            assert!(
                s >= b && s <= 1.0,
                "never coarser than the up alone says: {b} {a} -> {s}"
            );
        }
    }

    /// The steepest face of a mesh, against the globe's up at its origin:
    /// a flat square reads 1, a ramp at 45° reads cos 45°, and a mesh with
    /// no triangle reads nothing.
    #[test]
    fn a_tile_s_steepest_ground_is_its_steepest_face() {
        use crate::content::{DecodedMesh, DecodedTileContent};
        let origin = glam::DVec3::new(0.0, 0.0, 6_371_000.0);
        let content = |positions: Vec<[f32; 3]>, indices: Vec<u32>| DecodedTileContent {
            meshes: vec![DecodedMesh {
                positions,
                normals: None,
                uvs: None,
                indices,
                material: Default::default(),
            }],
            textures: Vec::new(),
            imagery: Vec::new(),
            withheld_drape: None,
            local_origin_ecef: origin,
            transform_local: glam::Mat4::IDENTITY,
        };
        // Up is +z at this origin.
        let flat = content(
            vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]],
            vec![0, 1, 2],
        );
        assert_eq!(super::steepest_ground(&flat), Some(1.0));
        let ramp = content(
            vec![
                [0.0, 0.0, 0.0],
                [10.0, 0.0, 0.0],
                [0.0, 10.0, 0.0],
                [10.0, 10.0, 10.0],
                [0.0, 20.0, 10.0],
            ],
            vec![0, 1, 2, 2, 3, 4],
        );
        let cos = super::steepest_ground(&ramp).expect("a ramp");
        assert!(cos < 1.0 && cos > 0.0, "{cos}");
        assert!(super::steepest_ground(&content(Vec::new(), Vec::new())).is_none());
    }

    use super::*;
    use crate::budget::{Bytes, DrawSet, GeoRect, LayerId, LayerReach, Policy, Role};
    use crate::fetch::FsFetcher;
    use crate::protocol::GeometryStream;
    use crate::source::DrapeReach;
    use crate::traversal::Residency;
    use glam::{dvec2, dvec3};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use url::Url;

    thread_local! {
        /// Uniform-detail passes checked against the reference on this
        /// thread, and the descents the session's memo had run by the last.
        static CHECKED: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    }

    /// The pass the session just ran, run again from scratch with
    /// [`crate::traversal::traverse`], and the two compared. Called on every
    /// pass of every session the tests in this module drive.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn matches_the_reference_pass(
        tree: &dyn TileTree,
        residency: &dyn Residency,
        views: &[ViewState],
        config: &Config,
        frame: u64,
        rendered_last: &HashSet<TileId>,
        chosen_before: &HashSet<TileId>,
        near: &NearestContent,
        out: &TraversalOutput,
    ) {
        let mut fresh = TraversalOutput::default();
        crate::traversal::traverse(
            tree,
            residency,
            views,
            config,
            frame,
            rendered_last,
            chosen_before,
            &mut fresh,
        );
        let at = format!("pass {frame}");
        assert_eq!(out.selected, fresh.selected, "{at}: selection");
        assert_eq!(out.requests, fresh.requests, "{at}: requests");
        assert_eq!(out.awaiting, fresh.awaiting, "{at}: awaiting");
        assert_eq!(out.blockers, fresh.blockers, "{at}: blockers");
        assert_eq!(out.chosen, fresh.chosen, "{at}: chosen");
        assert_eq!(out.stats, fresh.stats, "{at}: stats");
        if config.uniform_detail {
            CHECKED.with(|c| c.set((c.get().0 + 1, near.descents())));
        }
    }

    /// A roomy interactive budget with no floor: every test here says what
    /// it pins.
    fn roomy() -> BudgetConfig {
        let mut b = BudgetConfig::interactive(Bytes::mib(512));
        b.pinned_level = None;
        b
    }

    /// The consumer every test drives: it holds what it is delivered, acks
    /// it with its estimated bytes (unless told to hold back), drops what is
    /// cancelled and evicted, and checks the protocol's promises on every
    /// message — nothing drawn that it does not hold, nothing evicted that the
    /// draw set of the same step still uses.
    #[derive(Default)]
    struct Consumer {
        held: BTreeMap<ResourceId, Generation>,
        queued: BTreeMap<ResourceId, (Generation, u64)>,
        draw: DrawSet,
        selects: usize,
        contents: Vec<ResourceId>,
        evicted: Vec<ResourceId>,
        errors: Vec<(Option<ResourceId>, String)>,
        ready: bool,
        converged: bool,
        /// Acks withheld while true.
        hold_back: bool,
    }

    impl Consumer {
        fn take(&mut self, stream: &mut InProcessStream, cx: &mut std::task::Context<'_>) -> bool {
            let mut got = false;
            while let Poll::Ready(Some(msg)) = stream.poll_message(cx) {
                got = true;
                match msg {
                    ServerMessage::Content {
                        resource,
                        gen,
                        content,
                        ..
                    } => {
                        let bytes = match &content {
                            ResourceContent::Mesh(m) => m.cost().gpu.0,
                            ResourceContent::Drape(d) => d.cost().gpu.0,
                        };
                        self.contents.push(resource);
                        self.queued.insert(resource, (gen, bytes));
                    }
                    ServerMessage::Cancel { resources } => {
                        for (r, g) in resources {
                            if self.queued.get(&r).is_some_and(|q| q.0 == g) {
                                self.queued.remove(&r);
                            }
                        }
                    }
                    ServerMessage::Select {
                        draw,
                        frame_ready,
                        converged,
                        ..
                    } => {
                        for item in &draw.items {
                            assert!(
                                self.held.contains_key(&item.mesh),
                                "{:?} drawn before the consumer acknowledged it",
                                item.mesh
                            );
                            for d in &item.drapes {
                                assert!(
                                    self.held.contains_key(&d.resource),
                                    "{:?} worn before the consumer acknowledged it",
                                    d.resource
                                );
                            }
                        }
                        self.draw = draw;
                        self.ready = frame_ready;
                        self.converged = converged;
                        self.selects += 1;
                    }
                    ServerMessage::Evict { resources } => {
                        for r in resources {
                            let worn =
                                self.draw.items.iter().any(|i| {
                                    i.mesh == r || i.drapes.iter().any(|d| d.resource == r)
                                });
                            assert!(!worn, "{r:?} evicted while the draw set still uses it");
                            self.held.remove(&r);
                            self.evicted.push(r);
                        }
                    }
                    ServerMessage::Error { resource, message } => {
                        self.errors.push((resource, message))
                    }
                    ServerMessage::Priming(_) => {}
                }
            }
            if !self.hold_back {
                for (r, (gen, bytes)) in std::mem::take(&mut self.queued) {
                    self.held.insert(r, gen);
                    let _ = stream.send(ClientMessage::Ack {
                        resource: r,
                        gen,
                        bytes,
                    });
                }
            }
            got
        }

        /// The tiles drawn as themselves.
        fn exact(&self) -> HashSet<TileId> {
            self.draw
                .items
                .iter()
                .filter(|i| i.role == Role::Exact)
                .map(|i| i.mesh.tile())
                .collect()
        }
    }

    /// Steps a session by hand until it goes quiet, the consumer answering
    /// as it goes. Returns whether it settled.
    fn settle(
        server: &mut Pin<Box<impl Future<Output = ()>>>,
        stream: &mut InProcessStream,
        consumer: &mut Consumer,
    ) -> bool {
        const MAX_STEPS: usize = 4_000;
        const QUIET_STEPS: usize = 8;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut quiet = 0usize;
        for _ in 0..MAX_STEPS {
            let _ = server.as_mut().poll(&mut cx);
            let got = consumer.take(stream, &mut cx);
            quiet = if got { 0 } else { quiet + 1 };
            if quiet > QUIET_STEPS {
                return true;
            }
        }
        false
    }

    fn look(stream: &InProcessStream, view: ViewState, generation: u64) {
        stream
            .send(ClientMessage::ViewerState {
                views: vec![view],
                generation,
            })
            .expect("send");
    }

    fn deep_tileset(dir: &std::path::Path) -> Tileset {
        let url = write_deep_fixture(dir);
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        Tileset::from_json_bytes(&bytes, &url).expect("tileset")
    }

    fn leaves_of(tileset: &Tileset) -> HashSet<TileId> {
        tileset
            .tile(tileset.root())
            .children
            .iter()
            .flat_map(|m| tileset.tile(*m).children.clone())
            .collect()
    }

    /// **A session prices a still camera once, and selects as a fresh pass
    /// would on every pass.**
    #[test]
    fn a_still_camera_is_priced_once_per_camera() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let config = Config {
            uniform_detail: true,
            ..Config::default()
        };
        let (mut stream, server) =
            in_process(tileset, Arc::new(FsFetcher), config, roomy()).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        CHECKED.with(|c| c.set((0, 0)));

        look(&stream, near_view(), 0);
        assert!(
            settle(&mut server, &mut stream, &mut consumer),
            "first view settles"
        );
        let (first, descents) = CHECKED.with(|c| c.get());
        assert!(consumer.contents.len() >= 3, "nothing arrived to answer");
        assert!(first >= 2, "{first} passes: the arrivals were not answered");
        assert_eq!(descents, 1, "{first} passes under one camera");

        let farther = ViewState::perspective(
            dvec3(0.0, 0.0, 400.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        );
        look(&stream, farther, 1);
        assert!(
            settle(&mut server, &mut stream, &mut consumer),
            "second view settles"
        );
        let (total, descents) = CHECKED.with(|c| c.get());
        assert!(total > first, "the second camera was never traversed");
        assert_eq!(descents, 2, "{total} passes under two cameras");
    }

    /// **A load that is woken carries the wake, not the poll.** The loop polls
    /// the loads only when it gets round to them, so a load whose data arrived
    /// during a traversal resolves after it — and the arrival wait is the time
    /// in between, which a stamp taken on resolving would always read as zero.
    #[test]
    fn a_woken_load_carries_its_wake_not_its_poll() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        /// Pending until told otherwise, with its waker kept for the test.
        struct Later {
            ready: Arc<AtomicBool>,
            waker: Arc<Mutex<Option<std::task::Waker>>>,
        }
        impl Future for Later {
            type Output = u8;
            fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<u8> {
                if self.ready.load(Ordering::SeqCst) {
                    return Poll::Ready(7);
                }
                *self.waker.lock().expect("lock") = Some(cx.waker().clone());
                Poll::Pending
            }
        }
        /// The loop's side: counts the wakes passed through to it.
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let waits: &'static crate::metrics::Timer = Box::leak(Box::default());
        let ready = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None));
        let mut load = Box::pin(Woken::new(
            Later {
                ready: Arc::clone(&ready),
                waker: Arc::clone(&slot),
            },
            waits,
        ));
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&count));
        let mut cx = std::task::Context::from_waker(&waker);

        assert!(load.as_mut().poll(&mut cx).is_pending());
        assert_eq!(waits.count(), 0, "nothing woke it yet");

        ready.store(true, Ordering::SeqCst);
        slot.lock().expect("lock").take().expect("a waker").wake();
        assert_eq!(
            count.0.load(Ordering::SeqCst),
            1,
            "the wake reaches the loop"
        );
        // The loop is busy elsewhere.
        std::thread::sleep(std::time::Duration::from_millis(30));

        let Poll::Ready((out, woke)) = load.as_mut().poll(&mut cx) else {
            unreachable!("ready once woken");
        };
        assert_eq!(out, 7);
        assert!(
            woke.elapsed() >= std::time::Duration::from_millis(30),
            "the stamp is the wake's, 30 ms ago, not the poll's: {:?}",
            woke.elapsed()
        );
        assert_eq!(waits.count(), 1, "one wake, one wait");
        assert!(waits.peak_millis() >= 30.0, "{}", waits.peak_millis());
    }

    fn write_fixture(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        std::fs::write(dir.join("root.glb"), &glb).expect("write root");
        std::fs::write(dir.join("a.glb"), &glb).expect("write a");
        std::fs::write(dir.join("b.glb"), &glb).expect("write b");
        let tileset = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 50,
            "refine": "REPLACE",
            "content": { "uri": "root.glb" },
            "children": [
              { "boundingVolume": { "sphere": [-50, 0, 0, 50] },
                "geometricError": 0, "content": { "uri": "a.glb" } },
              { "boundingVolume": { "sphere": [50, 0, 0, 50] },
                "geometricError": 0, "content": { "uri": "b.glb" } }
            ]
          }
        }"#;
        let path = dir.join("tileset.json");
        std::fs::write(&path, tileset).expect("write tileset");
        Url::from_file_path(&path).expect("url")
    }

    /// A three-level REPLACE tileset: root → two mids → four leaves. Deep
    /// enough that selecting the leaves leaves an ancestor (the root) outside
    /// the pinned band.
    fn write_deep_fixture(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        for name in ["root", "m0", "m1", "l0", "l1", "l2", "l3"] {
            std::fs::write(dir.join(format!("{name}.glb")), &glb).expect("write glb");
        }
        let leaf = |x: f64, uri: &str| {
            format!(
                r#"{{ "boundingVolume": {{ "sphere": [{x}, 0, 0, 25] }},
                     "geometricError": 0, "content": {{ "uri": "{uri}" }} }}"#
            )
        };
        let tileset = format!(
            r#"{{
              "asset": {{ "version": "1.1" }},
              "geometricError": 400,
              "root": {{
                "boundingVolume": {{ "sphere": [0, 0, 0, 100] }},
                "geometricError": 100,
                "refine": "REPLACE",
                "content": {{ "uri": "root.glb" }},
                "children": [
                  {{ "boundingVolume": {{ "sphere": [-50, 0, 0, 50] }},
                     "geometricError": 50, "refine": "REPLACE",
                     "content": {{ "uri": "m0.glb" }},
                     "children": [{}, {}] }},
                  {{ "boundingVolume": {{ "sphere": [50, 0, 0, 50] }},
                     "geometricError": 50, "refine": "REPLACE",
                     "content": {{ "uri": "m1.glb" }},
                     "children": [{}, {}] }}
                ]
              }}
            }}"#,
            leaf(-75.0, "l0.glb"),
            leaf(-25.0, "l1.glb"),
            leaf(25.0, "l2.glb"),
            leaf(75.0, "l3.glb"),
        );
        let path = dir.join("tileset.json");
        std::fs::write(&path, &tileset).expect("write tileset");
        Url::from_file_path(&path).expect("url")
    }

    /// **The session converges to the leaves, drawing only what the consumer
    /// holds.** Every `Select` names acknowledged resources only (checked by
    /// the consumer on every message), the parent is drawn while its
    /// children load, and the settled draw set is the four leaves, each as
    /// itself.
    #[test]
    fn the_session_converges_drawing_only_what_the_consumer_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let leaves = leaves_of(&tileset);
        let (mut stream, server) =
            in_process(tileset, Arc::new(FsFetcher), Config::default(), roomy()).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        assert_eq!(
            consumer.exact(),
            leaves,
            "the settled frame draws the leaves"
        );
        assert!(consumer.converged, "the budget says the view converged");
        assert!(consumer.errors.is_empty(), "{:?}", consumer.errors);
    }

    /// **Nothing is drawn before it is acknowledged.** A consumer that
    /// holds back its acks gets content delivered and an empty draw set —
    /// never a resource it has not uploaded — and the ground appears as soon
    /// as the acks arrive.
    #[test]
    fn nothing_is_drawn_before_its_ack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let (mut stream, server) =
            in_process(tileset, Arc::new(FsFetcher), Config::default(), roomy()).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer {
            hold_back: true,
            ..Consumer::default()
        };
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        assert!(!consumer.contents.is_empty(), "something was delivered");
        assert!(
            consumer.draw.items.is_empty(),
            "drawn without an ack: {:?}",
            consumer.draw.items
        );
        consumer.hold_back = false;
        // An ack is a client message: the next step answers it.
        assert!(settle(&mut server, &mut stream, &mut consumer));
        assert!(
            !consumer.draw.items.is_empty(),
            "the acks were never answered"
        );
    }

    /// **A tile the source cannot serve is retried, and its ground stays
    /// covered.** Interactive: the failure is reported with the resource it
    /// concerns, the session keeps running, and the missing child's ground
    /// is drawn by its parent (the budget's fallback) rather than left bare.
    #[test]
    fn a_failed_load_is_retried_and_its_ground_stays_covered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture(dir.path());
        std::fs::remove_file(dir.path().join("b.glb")).expect("remove b");
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let (mut stream, server) =
            in_process(tileset, Arc::new(FsFetcher), Config::default(), roomy()).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        assert!(
            consumer.errors.iter().any(|(r, _)| r.is_some()),
            "the failure was not reported with its resource"
        );
        assert!(
            consumer
                .draw
                .items
                .iter()
                .any(|i| i.mesh == ResourceId::Mesh(root)),
            "the parent no longer covers the missing child's ground: {:?}",
            consumer.draw.items
        );
        // Still answering.
        let before = consumer.selects;
        look(&stream, far_view(), 2);
        settle(&mut server, &mut stream, &mut consumer);
        assert!(consumer.selects > before, "the session stopped answering");
    }

    /// **A batch session ends on a failed load**: a film never ships a hole.
    /// The consumer is told which resource and why, and the stream closes.
    #[test]
    fn a_batch_session_ends_on_a_failed_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture(dir.path());
        std::fs::remove_file(dir.path().join("b.glb")).expect("remove b");
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let mut batch = BudgetConfig::batch();
        batch.pinned_level = None;
        assert_eq!(batch.policy, Policy::Batch);
        let (mut stream, server) =
            in_process(tileset, Arc::new(FsFetcher), Config::default(), batch).expect("budget");
        let mut server = Box::pin(server.run());
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        let mut stopped = false;
        for _ in 0..2_000 {
            if server.as_mut().poll(&mut cx).is_ready() {
                stopped = true;
                break;
            }
            consumer.take(&mut stream, &mut cx);
        }
        consumer.take(&mut stream, &mut cx);
        assert!(
            stopped,
            "the batch session kept running after a failed load"
        );
        assert!(
            consumer
                .errors
                .iter()
                .any(|(r, m)| r.is_some() && !m.is_empty()),
            "the session stopped without saying which resource: {:?}",
            consumer.errors
        );
        assert!(matches!(stream.poll_message(&mut cx), Poll::Ready(None)));
    }

    /// **A cancelled load is aborted for real, and a resource is never
    /// loaded twice at once.** A loader whose loads never answer records
    /// each load's start and the drop of its future — which is what aborts
    /// the request under it (a browser's `AbortController`, a native
    /// client's connection). The camera leaves the ground those loads were
    /// for; once their grace has passed, every one of them is dropped within
    /// the step that cancels it, and at no moment are two loads of one tile
    /// alive together.
    #[test]
    fn a_cancelled_load_is_aborted_and_never_overlaps_its_restart() {
        use std::sync::atomic::AtomicU64;
        #[derive(Default)]
        struct Live {
            alive: std::collections::HashMap<TileId, u32>,
            overlap: u32,
            started: u32,
            dropped: u32,
        }
        struct Guard(Arc<std::sync::Mutex<Live>>, TileId);
        impl Drop for Guard {
            fn drop(&mut self) {
                let mut l = self.0.lock().expect("live");
                l.dropped += 1;
                if let Some(n) = l.alive.get_mut(&self.1) {
                    *n -= 1;
                }
            }
        }
        struct Hanging(Arc<std::sync::Mutex<Live>>);
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Hanging {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                {
                    let mut l = self.0.lock().expect("live");
                    l.started += 1;
                    let n = l.alive.entry(id).or_default();
                    *n += 1;
                    if *n > 1 {
                        l.overlap += 1;
                    }
                }
                let _guard = Guard(Arc::clone(&self.0), id);
                futures_util::future::pending().await
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let live = Arc::new(std::sync::Mutex::new(Live::default()));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::new(RwLock::new(tileset))));
        let now = Arc::new(AtomicU64::new(0));
        let clock = Arc::clone(&now);
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Hanging(Arc::clone(&live))),
            Config::default(),
            roomy(),
        )
        .expect("budget");
        let server = server.with_clock(Arc::new(move || Micros(clock.load(Ordering::SeqCst))));
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        let started = live.lock().expect("live").started;
        assert!(started > 0, "nothing started");
        // Away, long enough for the grace to pass; the steps that follow
        // cancel what the near view wanted.
        for (k, view) in [far_view(), near_view(), far_view()]
            .into_iter()
            .enumerate()
        {
            now.fetch_add(2_000_000, Ordering::SeqCst);
            look(&stream, view, 2 + k as u64);
            settle(&mut server, &mut stream, &mut consumer);
        }
        let l = live.lock().expect("live");
        assert!(l.dropped > 0, "no cancelled load was dropped");
        assert_eq!(
            l.overlap, 0,
            "{} loads overlapped a live one of their tile",
            l.overlap
        );
        let alive: u32 = l.alive.values().sum();
        assert_eq!(
            alive + l.dropped,
            l.started,
            "a load neither alive nor dropped"
        );
    }

    /// **The loader is asked about a resource's presence once a second at
    /// most**, however many steps weigh it: answering a drape costs the
    /// loader its imagery mosaic, and asking it at every step of a settling
    /// view took two thirds of a browser worker's thread. A clock that never
    /// moves keeps every answer.
    #[test]
    fn presence_is_asked_once_within_its_window() {
        struct Counting(
            TilesetLoader<FsFetcher>,
            Arc<std::sync::Mutex<HashMap<ResourceId, u32>>>,
        );
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Counting {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn holds(&self, r: ResourceId) -> crate::budget::Presence {
                *self.1.lock().expect("asked").entry(r).or_default() += 1;
                crate::budget::Presence::Absent
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let arena = Arc::new(RwLock::new(deep_tileset(dir.path())));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let asked = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let loader = Counting(
            TilesetLoader::new(arena, Arc::new(FsFetcher)),
            Arc::clone(&asked),
        );
        let (mut stream, server) =
            in_process_with(tree, Arc::new(loader), Config::default(), roomy()).expect("budget");
        let server = server.with_clock(Arc::new(|| Micros(1)));
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        look(&stream, far_view(), 2);
        settle(&mut server, &mut stream, &mut consumer);
        let asked = asked.lock().expect("asked");
        assert!(!asked.is_empty(), "nothing was asked");
        let most = asked.values().copied().max().unwrap_or(0);
        assert_eq!(most, 1, "a resource asked {most} times within one window");
    }

    /// **The loads in flight are ranked in the budget's order, and each
    /// names itself to the work it hands a pool**: what a decode pool orders
    /// its queue by. A loader that never answers keeps every load in flight;
    /// each one notes the resource [`crate::offload::loading`] names while
    /// it is polled, and every one of those is ranked, the ranks a
    /// permutation of the loads.
    #[test]
    fn loads_in_flight_are_ranked_and_name_themselves() {
        struct Noting(Arc<std::sync::Mutex<Vec<ResourceId>>>);
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Noting {
            async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
                if let Some(r) = crate::offload::loading() {
                    self.0.lock().expect("noted").push(r);
                }
                futures_util::future::pending().await
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let noted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut stream, server) = in_process_with(
            Box::new(tileset),
            Arc::new(Noting(Arc::clone(&noted))),
            Config::default(),
            roomy(),
        )
        .expect("budget");
        let ranks = server.load_ranks();
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        let noted = noted.lock().expect("noted").clone();
        assert!(!noted.is_empty(), "no load named itself");
        let mut seen: Vec<u32> = noted
            .iter()
            .map(|r| ranks.rank(*r).expect("a load in flight is ranked"))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..noted.len() as u32).collect::<Vec<_>>());
    }

    /// **A release between frames is announced** as any eviction, and takes
    /// nothing the last step drew.
    /// A batch session told the frames ahead loads their tiles beside the
    /// frame it converges — with the slots that frame leaves, never drawn,
    /// never waited on — and the frame draws what it draws without them; a
    /// release between frames keeps them for the frame that needs them.
    #[test]
    fn the_frames_ahead_load_beside_the_frame_and_change_nothing_it_draws() {
        let session = |ahead: bool| {
            let dir = tempfile::tempdir().expect("tempdir");
            let tileset = deep_tileset(dir.path());
            let leaves = leaves_of(&tileset);
            let mut batch = BudgetConfig::batch();
            batch.pinned_level = None;
            let (mut stream, server) =
                in_process(tileset, Arc::new(FsFetcher), Config::default(), batch).expect("budget");
            let mut server = Box::pin(server.run());
            let mut consumer = Consumer::default();
            if ahead {
                stream
                    .send(ClientMessage::Ahead {
                        views: vec![near_view()],
                    })
                    .expect("send");
            }
            look(&stream, far_view(), 1);
            assert!(settle(&mut server, &mut stream, &mut consumer));
            assert!(consumer.ready, "the far frame is ready");
            let far = (consumer.exact(), consumer.draw.clone());
            stream
                .send(ClientMessage::ReleaseBeyond { bytes: 0 })
                .expect("send");
            assert!(settle(&mut server, &mut stream, &mut consumer));
            let loaded_leaves: HashSet<TileId> = consumer
                .held
                .keys()
                .map(|r| r.tile())
                .filter(|t| leaves.contains(t))
                .collect();
            look(&stream, near_view(), 2);
            assert!(settle(&mut server, &mut stream, &mut consumer));
            assert!(consumer.ready, "the near frame is ready");
            (far, loaded_leaves, consumer.exact(), dir)
        };
        let (without, none_ahead, near_without, _a) = session(false);
        let (with, leaves_ahead, near_with, _b) = session(true);
        assert_eq!(with.0, without.0, "the far frame draws other tiles");
        assert_eq!(
            format!("{:?}", with.1),
            format!("{:?}", without.1),
            "the far frame's draw set differs"
        );
        assert!(none_ahead.is_empty(), "the far frame alone loads no leaf");
        assert!(
            !leaves_ahead.is_empty(),
            "the near frame's leaves were loaded ahead, and kept through the release"
        );
        assert_eq!(near_with, near_without, "the near frame draws other tiles");
    }

    #[test]
    fn a_release_beyond_is_announced_and_spares_what_is_drawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let mut batch = BudgetConfig::batch();
        batch.pinned_level = None;
        let (mut stream, server) =
            in_process(tileset, Arc::new(FsFetcher), Config::default(), batch).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        assert!(consumer.ready, "the batch frame is ready");
        // Another view: the first one's leaves are no longer needed.
        look(&stream, far_view(), 2);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        let drawn: HashSet<ResourceId> = consumer.draw.items.iter().map(|i| i.mesh).collect();
        stream
            .send(ClientMessage::ReleaseBeyond { bytes: 0 })
            .expect("send");
        assert!(settle(&mut server, &mut stream, &mut consumer));
        assert!(!consumer.evicted.is_empty(), "nothing was released");
        for r in &consumer.evicted {
            assert!(!drawn.contains(r), "{r:?} released while drawn");
        }
    }

    /// A tileset whose tiles are draped: each tile's drape is one imagery
    /// tile placed over it, of a layer that reaches level 4, and the drape's
    /// pixels change with the loader's epoch.
    struct Draped {
        inner: TilesetLoader<FsFetcher>,
        arena: crate::tiles3d::SharedTileset,
        epoch: Arc<AtomicU64>,
        meshes: Arc<AtomicUsize>,
        drapes: Arc<Mutex<Vec<ResourceId>>>,
    }

    impl Draped {
        /// A footprint in the budget's radians that nests as the fixture's
        /// spheres do: the root's square holds its children's.
        fn rect(&self, id: TileId) -> GeoRect {
            let ts = self.arena.read().expect("arena");
            let c = ts.properties(id).bounding_volume.center().x;
            let half = match ts.level(id) {
                0 => 100.0,
                1 => 50.0,
                _ => 25.0,
            };
            GeoRect {
                west: (c - half) * 1e-4,
                east: (c + half) * 1e-4,
                south: -half * 1e-4,
                north: half * 1e-4,
            }
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl TileLoader for Draped {
        async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
            self.meshes.fetch_add(1, Ordering::SeqCst);
            self.inner.load(id).await
        }
        fn drapes(&self, id: TileId) -> Option<DrapeReach> {
            Some(DrapeReach {
                texel_l0_m: 1.0e4,
                rect: self.rect(id),
                layers: vec![LayerReach {
                    layer: LayerId(0),
                    max_level: 4,
                }],
            })
        }
        async fn load_drape(
            &self,
            id: TileId,
            layer: LayerId,
            level: u32,
        ) -> Result<DrapeContent, LoadError> {
            self.drapes
                .lock()
                .expect("drapes")
                .push(ResourceId::drape(id, layer, level));
            let texture = Arc::new(crate::content::DecodedTexture::from_rgba8(
                4,
                4,
                vec![self.epoch.load(Ordering::SeqCst) as u8; 64],
            ));
            let tile = crate::raster::GeoRect {
                west: 0.0,
                south: 0.0,
                east: 1.0,
                north: 1.0,
            };
            Ok(DrapeContent {
                layer,
                level,
                tiles: vec![crate::raster::ImageryLayer::placed(
                    crate::raster::ImageryCoord { level, x: 0, y: 0 },
                    texture,
                    &tile,
                    &tile,
                )],
                floor_tiles: 0,
                identity: u64::from(level),
                withheld: false,
            })
        }
        fn epoch(&self) -> u64 {
            self.epoch.load(Ordering::SeqCst)
        }
    }

    #[allow(clippy::type_complexity)]
    fn draped_session(
        dir: &std::path::Path,
    ) -> (
        InProcessStream,
        GeometryServer,
        Arc<AtomicU64>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<ResourceId>>>,
    ) {
        let tileset = deep_tileset(dir);
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let epoch = Arc::new(AtomicU64::new(1));
        let meshes = Arc::new(AtomicUsize::new(0));
        let drapes = Arc::new(Mutex::new(Vec::new()));
        let loader: Arc<dyn TileLoader> = Arc::new(Draped {
            inner: TilesetLoader::new(Arc::clone(&arena), Arc::new(FsFetcher)),
            arena,
            epoch: Arc::clone(&epoch),
            meshes: Arc::clone(&meshes),
            drapes: Arc::clone(&drapes),
        });
        let (stream, server) =
            in_process_with(tree, loader, Config::default(), roomy()).expect("budget");
        (stream, server, epoch, meshes, drapes)
    }

    /// **A drape is a load of its own, and every drawn item is dressed.**
    /// Once the view has settled, every drawn tile wears a drape of the layer
    /// it needs (I1 extended by D-A1), the drapes were loaded beside the
    /// meshes, and nothing bare is drawn.
    #[test]
    fn drapes_are_their_own_loads_and_dress_every_drawn_tile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut stream, server, _, meshes, drapes) = draped_session(dir.path());
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        assert!(meshes.load(Ordering::SeqCst) > 0);
        assert!(
            !drapes.lock().expect("drapes").is_empty(),
            "no drape was loaded"
        );
        assert!(!consumer.draw.items.is_empty());
        for item in &consumer.draw.items {
            assert!(
                item.drapes.iter().any(|d| d.layer == LayerId(0)),
                "{:?} drawn bare",
                item.mesh
            );
        }
    }

    /// **A still view reads its drape levels again only when the store's
    /// answers may have changed** (O1.4, `PresenceOracle::generation`). The
    /// drapes never arrive, so every drawn tile's level is chosen by asking
    /// the loader's presence index, every step. Within one window of
    /// [`PRESENCE_FOR`] the answers cannot change: a still step reads no
    /// level. The clock enters the next window, the store now holding a
    /// drape: the levels are read again, the loader asked again, and the
    /// new answer used.
    #[test]
    fn a_still_view_reads_its_levels_again_only_when_the_store_may_have_changed() {
        struct Stalled {
            draped: Draped,
            stored: Arc<Mutex<HashSet<ResourceId>>>,
            asked: Arc<Mutex<HashMap<ResourceId, u32>>>,
        }
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Stalled {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.draped.load(id).await
            }
            fn drapes(&self, id: TileId) -> Option<DrapeReach> {
                self.draped.drapes(id)
            }
            async fn load_drape(
                &self,
                _id: TileId,
                _layer: LayerId,
                _level: u32,
            ) -> Result<DrapeContent, LoadError> {
                futures_util::future::pending().await
            }
            fn holds(&self, r: ResourceId) -> crate::budget::Presence {
                *self.asked.lock().expect("asked").entry(r).or_default() += 1;
                if self.stored.lock().expect("stored").contains(&r) {
                    crate::budget::Presence::Stored
                } else {
                    crate::budget::Presence::Absent
                }
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let stored = Arc::new(Mutex::new(HashSet::new()));
        let asked = Arc::new(Mutex::new(HashMap::new()));
        let loader = Stalled {
            draped: Draped {
                inner: TilesetLoader::new(Arc::clone(&arena), Arc::new(FsFetcher)),
                arena,
                epoch: Arc::new(AtomicU64::new(1)),
                meshes: Arc::new(AtomicUsize::new(0)),
                drapes: Arc::new(Mutex::new(Vec::new())),
            },
            stored: Arc::clone(&stored),
            asked: Arc::clone(&asked),
        };
        let (mut stream, server) =
            in_process_with(tree, Arc::new(loader), Config::default(), roomy()).expect("budget");
        let clock = Arc::new(AtomicU64::new(1));
        let at = Arc::clone(&clock);
        let server = server.with_clock(Arc::new(move || Micros(at.load(Ordering::SeqCst))));
        let meter = server.meter();
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        let mut generation = 1;
        // One still step, prompted by the same view told again; its work.
        let mut still = |stream: &mut InProcessStream, server: &mut Pin<Box<_>>| {
            generation += 1;
            look(stream, near_view(), generation);
            assert!(settle(server, stream, &mut consumer));
            meter.lock().expect("meter").signals().decide_work
        };
        // Still steps at 60 a second, three windows: each step's work, and
        // whether it is the first of a window.
        const DT: u64 = 16_667;
        let mut steps = Vec::new();
        let mut asked_by_window = Vec::new();
        for i in 0..190u64 {
            let t = 1 + PRESENCE_FOR.0 / 2 + i * DT;
            let before = clock.swap(t, Ordering::SeqCst);
            let first = t / PRESENCE_FOR.0 != before / PRESENCE_FOR.0;
            if first {
                asked_by_window.push(asked.lock().expect("asked").clone());
            }
            if i == 100 {
                // The store fills mid-window: every drape asked of it so far.
                let all: Vec<ResourceId> = asked.lock().expect("asked").keys().copied().collect();
                stored.lock().expect("stored").extend(all);
            }
            steps.push((still(&mut stream, &mut server), first));
        }
        assert!(asked_by_window.len() >= 3, "three windows");
        assert!(!asked_by_window[1].is_empty(), "no level asked the store");
        // By window, from the first whole one (the steps before it settle
        // what was in flight): the still steps' work is the least, but for
        // the prune's (every eighth, with work of its own); the window's
        // first step reads the levels again, and does more.
        let starts: Vec<usize> = (0..steps.len()).filter(|&i| steps[i].1).collect();
        let mut pruned = None;
        for w in starts
            .windows(2)
            .map(|w| (w[0], w[1]))
            .chain([(starts[starts.len() - 1], steps.len())])
        {
            let (from, to) = w;
            let steady = steps[from + 1..to].iter().map(|s| s.0).min().unwrap_or(0);
            let p = *pruned.get_or_insert_with(|| {
                (from + 1..to).find(|&i| steps[i].0 > steady).unwrap_or(0) % 8
            });
            for (i, &(work, _)) in steps.iter().enumerate().take(to).skip(from + 1) {
                if i % 8 != p {
                    assert_eq!(work, steady, "step {i}: a still step read its levels again");
                }
            }
            if from % 8 != p {
                assert!(
                    steps[from].0 > steady,
                    "step {from}: the levels were not read again in a new window ({} against {steady})",
                    steps[from].0
                );
            }
        }
        // The loader is asked once a window at most, and again in the
        // window after the store filled.
        asked_by_window.push(asked.lock().expect("asked").clone());
        for w in asked_by_window.windows(2) {
            for (r, n) in &w[1] {
                let m = w[0].get(r).copied().unwrap_or(0);
                assert!(*n <= m + 1, "{r:?} asked {} times within a window", n - m);
            }
        }
        let (filled, after) = (
            &asked_by_window[asked_by_window.len() - 2],
            &asked_by_window[asked_by_window.len() - 1],
        );
        assert!(
            filled.iter().any(|(r, n)| after.get(r) > Some(n)),
            "the loader was not asked again in a new window"
        );
    }

    /// **A change of imagery redrapes without reloading a mesh, and without
    /// a bare frame.** The loader's epoch moves on: every drawn drape is
    /// loaded again, new before old — each draw set in between still dresses
    /// every item — and no mesh is fetched a second time (I10).
    #[test]
    fn a_change_of_imagery_redrapes_without_reloading_a_mesh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut stream, server, epoch, meshes, drapes) = draped_session(dir.path());
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        let meshes_before = meshes.load(Ordering::SeqCst);
        let drapes_before = drapes.lock().expect("drapes").len();
        epoch.store(2, Ordering::SeqCst);
        // Any message prompts a step; the step sees the new epoch.
        look(&stream, near_view(), 2);
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        for _ in 0..2_000 {
            let _ = server.as_mut().poll(&mut cx);
            consumer.take(&mut stream, &mut cx);
            for item in &consumer.draw.items {
                assert!(
                    !item.drapes.is_empty(),
                    "{:?} drawn bare mid-change",
                    item.mesh
                );
            }
        }
        assert!(
            drapes.lock().expect("drapes").len() > drapes_before,
            "the drapes were not refreshed"
        );
        assert_eq!(
            meshes.load(Ordering::SeqCst),
            meshes_before,
            "a change of imagery reloaded a mesh"
        );
    }

    /// **A server answered from memory still hands its host a turn.**
    #[test]
    fn a_busy_server_hands_its_host_a_turn() {
        /// Ready on the second poll: a turn given back to the executor.
        struct Yield(bool);
        impl Future for Yield {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
                if std::mem::replace(&mut self.0, true) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        let run = |every: usize| -> (usize, usize) {
            let dir = tempfile::tempdir().expect("tempdir");
            let tileset = deep_tileset(dir.path());
            let pauses = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&pauses);
            let (mut stream, server) =
                in_process(tileset, Arc::new(FsFetcher), Config::default(), roomy())
                    .expect("budget");
            let server = server.with_breather(
                every,
                Arc::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Box::pin(Yield(false))
                }),
            );
            let mut server = Box::pin(server.run());
            let mut consumer = Consumer::default();
            look(&stream, near_view(), 0);
            settle(&mut server, &mut stream, &mut consumer);
            (pauses.load(Ordering::SeqCst), consumer.contents.len())
        };
        let (pauses, delivered) = run(2);
        assert!(pauses > 0, "the host never had a turn");
        assert!(
            delivered >= 7,
            "the session stopped after a pause: {delivered}"
        );
        assert_eq!(run(1000).0, 0, "paused before its count was reached");
    }

    /// A load that waits behind a gate the test opens, for the tiles it holds.
    struct Gate {
        open: std::sync::atomic::AtomicBool,
        wakers: Mutex<Vec<std::task::Waker>>,
        waiting: std::sync::atomic::AtomicUsize,
    }

    impl Gate {
        fn closed() -> Arc<Self> {
            Arc::new(Self {
                open: std::sync::atomic::AtomicBool::new(false),
                wakers: Mutex::new(Vec::new()),
                waiting: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn open(&self) {
            self.open.store(true, std::sync::atomic::Ordering::SeqCst);
            for waker in self.wakers.lock().expect("wakers").drain(..) {
                waker.wake();
            }
        }

        async fn pass(&self) {
            use std::sync::atomic::Ordering;
            self.waiting.fetch_add(1, Ordering::SeqCst);
            poll_fn(|cx| {
                if self.open.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    self.wakers.lock().expect("wakers").push(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await;
            self.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// **Loads that land together are answered by one step.** Four leaves
    /// are held behind a gate; when it opens all four are ready on the same
    /// turn, and one decision answers them all.
    #[test]
    fn loads_that_land_together_are_answered_by_one_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset = deep_tileset(dir.path());
        let leaves = leaves_of(&tileset);

        /// Real content, the leaves only once the gate is open.
        struct Gated {
            inner: TilesetLoader<FsFetcher>,
            leaves: HashSet<TileId>,
            gate: Arc<Gate>,
        }
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Gated {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                if self.leaves.contains(&id) {
                    self.gate.pass().await;
                }
                self.inner.load(id).await
            }
        }

        let gate = Gate::closed();
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let loader: Arc<dyn TileLoader> = Arc::new(Gated {
            inner: TilesetLoader::new(arena, Arc::new(FsFetcher)),
            leaves: leaves.clone(),
            gate: Arc::clone(&gate),
        });
        let (mut stream, server) =
            in_process_with(tree, loader, Config::default(), roomy()).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        assert_eq!(
            gate.waiting.load(Ordering::SeqCst),
            4,
            "the four leaves wait"
        );
        let selects = consumer.selects;
        gate.open();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        // One poll: the four arrivals are recorded and answered together.
        let _ = server.as_mut().poll(&mut cx);
        let mut arrivals = Vec::new();
        while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
            match msg {
                ServerMessage::Content { resource, .. } => arrivals.push(resource),
                ServerMessage::Select { .. } => consumer.selects += 1,
                _ => {}
            }
        }
        assert_eq!(arrivals.len(), 4, "the four leaves delivered");
        assert_eq!(consumer.selects - selects, 1, "one step for four arrivals");
    }

    /// **A graft is walked and its content drawn.** The external tileset's
    /// root arrives as an expanded tile; the next step walks the grafted
    /// tree and its content is delivered and drawn.
    #[test]
    fn an_external_tileset_is_grafted_and_its_leaf_drawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_external(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let host = tileset.root();
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let loader: Arc<dyn TileLoader> =
            Arc::new(TilesetLoader::new(Arc::clone(&arena), Arc::new(FsFetcher)));
        let (mut stream, server) =
            in_process_with(tree, loader, Config::default(), roomy()).expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        assert!(settle(&mut server, &mut stream, &mut consumer));
        let grafted: Vec<TileId> = consumer
            .contents
            .iter()
            .map(|r| r.tile())
            .filter(|t| arena.read().expect("arena").parent(*t) == Some(host))
            .collect();
        assert!(
            !grafted.is_empty(),
            "the grafted root's content never arrived: {:?}",
            consumer.contents
        );
        assert!(
            consumer.exact().contains(&grafted[0]),
            "the grafted root is not drawn: {:?}",
            consumer.draw.items
        );
        assert!(consumer.errors.is_empty(), "{:?}", consumer.errors);
    }

    fn write_external(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        std::fs::write(dir.join("a.glb"), &glb).expect("write a");
        std::fs::write(
            dir.join("sub.json"),
            r#"{ "asset": { "version": "1.1" }, "geometricError": 50,
                 "root": { "boundingVolume": { "sphere": [0, 0, 0, 100] },
                           "geometricError": 0, "content": { "uri": "a.glb" } } }"#,
        )
        .expect("write sub");
        let path = dir.join("tileset.json");
        std::fs::write(
            &path,
            r#"{ "asset": { "version": "1.1" }, "geometricError": 200,
                 "root": { "boundingVolume": { "sphere": [0, 0, 0, 100] },
                           "geometricError": 50, "refine": "REPLACE",
                           "content": { "uri": "sub.json" } } }"#,
        )
        .expect("write tileset");
        Url::from_file_path(&path).expect("url")
    }

    /// **A result that lands after its load was cancelled is the news of the
    /// cancel.** A task cannot take back what it has sent; the abort handle
    /// that travels with the result says whether the loop still wants it.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    #[test]
    fn a_task_cancelled_after_it_finished_reports_a_cancel() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("runtime");
        let mut loads = Loads::new(&Spawn::Tokio(runtime.handle().clone()));
        let mut start = |tile: TileId| {
            let (handle, registration) = AbortHandle::new_pair();
            let fut: BoxLoadFut = Box::pin(async move {
                (
                    (
                        ResourceId::Mesh(tile),
                        Generation(1),
                        Ok(Fetched::Mesh(Loaded::Expanded)),
                    ),
                    crate::metrics::stamp(),
                )
            });
            loads.push(Abortable::new(fut, registration), &handle);
            handle
        };
        let kept = start(TileId(1));
        let cancelled = start(TileId(2));
        // Both have finished and sent: wait until both are in the channel.
        let mut landed = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let Loads::Tasks(tasks) = &mut loads else {
            unreachable!("tasks")
        };
        while landed.len() < 2 {
            assert!(std::time::Instant::now() < deadline, "the tasks never sent");
            if let Ok(item) = tasks.rx.try_recv() {
                landed.push(item);
            }
            std::thread::yield_now();
        }
        cancelled.abort();
        let mut outcomes: Vec<(u64, bool)> = landed
            .into_iter()
            .map(|(msg, handle)| {
                (
                    msg.0 .0.tile().0,
                    tasks::Tasks::screen((msg, handle)).is_ok(),
                )
            })
            .collect();
        outcomes.sort_unstable();
        assert_eq!(outcomes, vec![(1, true), (2, false)]);
        drop(kept);
    }

    /// The mode names itself, for the log line a host writes.
    #[test]
    fn a_spawn_says_which_it_is() {
        assert_eq!(format!("{:?}", Spawn::default()), "Inline");
        #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            assert_eq!(
                format!("{:?}", Spawn::Tokio(runtime.handle().clone())),
                "Tokio"
            );
        }
    }

    /// **Inline, nothing is deferred and no pause is added.**
    #[test]
    fn inline_loads_neither_defer_edits_nor_pause() {
        struct Counting(Arc<AtomicUsize>);
        impl TileTree for Counting {
            fn roots(&self) -> Vec<TileId> {
                vec![TileId(0)]
            }
            fn children(&self, _id: TileId) -> Vec<TileId> {
                Vec::new()
            }
            fn properties(&self, _id: TileId) -> crate::source::TileProperties {
                crate::source::TileProperties {
                    bounding_volume: crate::math::BoundingVolume::Sphere(crate::math::Sphere {
                        center: glam::DVec3::ZERO,
                        radius: 1.0,
                    }),
                    geometric_error: 1.0,
                    refine: crate::tileset::Refine::Replace,
                    has_content: false,
                }
            }
            fn defer_edits(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Never;
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Never {
            async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
                futures_util::future::pending().await
            }
        }
        let deferred = Arc::new(AtomicUsize::new(0));
        let (mut stream, server) = in_process_with(
            Box::new(Counting(Arc::clone(&deferred))),
            Arc::new(Never),
            Config::default(),
            roomy(),
        )
        .expect("budget");
        let mut server = Box::pin(server.run());
        let mut consumer = Consumer::default();
        look(&stream, near_view(), 1);
        settle(&mut server, &mut stream, &mut consumer);
        assert_eq!(deferred.load(Ordering::SeqCst), 0, "inline defers nothing");
        assert!(default_breather(&Loads::new(&Spawn::Inline)).is_none());
    }

    /// Spawned loads defer, and pause for the runtime by default; a result
    /// that has landed is taken without waiting.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    #[test]
    fn spawned_loads_land_without_waiting_and_pause_by_default() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("runtime");
        let mut loads = Loads::new(&Spawn::Tokio(runtime.handle().clone()));
        assert!(loads.spawns());
        assert!(default_breather(&loads).is_some());
        for tile in [TileId(1), TileId(2)] {
            let (handle, registration) = AbortHandle::new_pair();
            let fut: BoxLoadFut = Box::pin(async move {
                (
                    (
                        ResourceId::Mesh(tile),
                        Generation(1),
                        Ok(Fetched::Mesh(Loaded::Expanded)),
                    ),
                    crate::metrics::stamp(),
                )
            });
            loads.push(Abortable::new(fut, registration), &handle);
        }
        let mut landed = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while landed.len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "try_next never handed over what had landed"
            );
            if let Some(Ok(((r, _, _), _))) = loads.try_next() {
                landed.push(r.tile().0);
            }
            std::thread::yield_now();
        }
        landed.sort_unstable();
        assert_eq!(landed, vec![1, 2]);
        assert!(loads.try_next().is_none(), "nothing more");
    }

    fn near_view() -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, 120.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        )
    }

    /// Far enough that the root alone satisfies the SSE — the leaves the near
    /// view pulled in become dead weight.
    fn far_view() -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, 100_000.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        )
    }
}
