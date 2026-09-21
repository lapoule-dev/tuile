// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The geometry server: traversal + scheduling + fetch + decode,
//! orchestrated around the pure [`traverse`] function.
//!
//! This is a LOGICAL server, not a network server: [`in_process`] binds it
//! to a consumer over channels (the fully decoded liaison). Network
//! middleware (M2, `tuile-server`) drives the same loop and serializes the
//! protocol — glb on the wire.
//!
//! Runtime-agnostic: no tokio, no spawning. The whole server is one future
//! ([`GeometryServer::run`]) multiplexing client messages and in-flight
//! fetches through `FuturesUnordered`; the caller decides where it runs.

use crate::cache::ResidentCache;
use crate::content::TileContent;
use crate::fetch::TileFetcher;
use crate::protocol::{
    in_process_pair, ClientMessage, InProcessStream, Priming, ServerEndpoint, ServerMessage,
};
use crate::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use crate::tiles3d::{TilesetLoader, TilesetTree};
use crate::tileset::Tileset;
use crate::traversal::{traverse, Config, ResidencyView, TraversalOutput, ViewState};
use futures_channel::mpsc::UnboundedSender;
use futures_core::Stream;
use futures_util::future::{poll_fn, AbortHandle, Abortable, Aborted};
use futures_util::stream::FuturesUnordered;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::Poll;

type LoadMsg = (TileId, Result<Loaded, LoadError>);
// The boxed load future is `Send` on native (driven by a multi-thread executor
// like tokio) but `?Send` on wasm, where it holds JS futures (`!Send`) and runs
// single-threaded under `spawn_local`. Same server loop, two host models.
#[cfg(not(target_arch = "wasm32"))]
type BoxLoadFut = Pin<Box<dyn Future<Output = LoadMsg> + Send>>;
#[cfg(target_arch = "wasm32")]
type BoxLoadFut = Pin<Box<dyn Future<Output = LoadMsg>>>;
type LoadFuture = Abortable<BoxLoadFut>;

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
) -> (InProcessStream, GeometryServer) {
    let arena = Arc::new(RwLock::new(tileset));
    let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
    let loader: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, fetcher));
    in_process_with(tree, loader, config)
}

/// Creates a geometry server over **any** tile source: a [`TileTree`] for the
/// traversal and a [`TileLoader`] for async content. This is the seam the
/// globe (terrain × imagery) and compositions plug into — the server knows
/// neither glb nor terrain nor grafting.
pub fn in_process_with(
    tree: Box<dyn TileTree>,
    loader: Arc<dyn TileLoader>,
    config: Config,
) -> (InProcessStream, GeometryServer) {
    let (client, endpoint) = in_process_pair();
    let cache = ResidentCache::pinning(
        config.resident_budget_bytes,
        config.resident_tile_limit,
        config.pinned_level,
    );
    (
        client,
        GeometryServer {
            tree,
            loader,
            config,
            endpoint,
            cache,
            residency: ResidencyView::default(),
            in_flight: HashMap::new(),
            acked: HashSet::new(),
            filled: HashSet::new(),
            priming: Vec::new(),
            priming_outstanding: HashSet::new(),
            priming_state: Priming::default(),
            priming_reported: None,
            views: Vec::new(),
            view_generation: 0,
            out: TraversalOutput::default(),
            selected: HashSet::new(),
            rendered_last: HashSet::new(),
            view_moved: false,
            frame: 0,
            primed: false,
        },
    )
}

/// One streaming session: owns the tile tree, loader, residency and in-flight
/// state for one consumer. N sessions = N servers sharing nothing mutable.
pub struct GeometryServer {
    tree: Box<dyn TileTree>,
    loader: Arc<dyn TileLoader>,
    config: Config,
    endpoint: ServerEndpoint,
    cache: ResidentCache,
    residency: ResidencyView,
    in_flight: HashMap<TileId, AbortHandle>,
    /// Tiles the consumer has confirmed it holds, from `ClientMessage::Ack`.
    ///
    /// The server's residency is not the consumer's: content is sent, then
    /// queued, then uploaded within a per-frame budget, and every step of that
    /// is a frame where the tile is resident here and absent there. That gap is
    /// what a stand-in covers, and an ack is the only report of it.
    acked: HashSet<TileId>,
    /// Tiles a stand-in has been sent for, so it is sent once and taken back
    /// when its ground stops being looked at.
    filled: HashSet<TileId>,
    priming: Vec<TileId>,
    priming_outstanding: HashSet<TileId>,
    priming_state: Priming,
    priming_reported: Option<Priming>,
    views: Vec<ViewState>,
    view_generation: u64,
    out: TraversalOutput,
    selected: HashSet<TileId>,
    rendered_last: HashSet<TileId>,
    view_moved: bool,
    frame: u64,
    /// Whether the coarse pyramid has been asked for yet. See
    /// [`Session::prime`].
    primed: bool,
}

impl GeometryServer {
    /// Runs the session until the consumer goes away.
    pub async fn run(mut self) {
        let mut rx = self.endpoint.rx;
        let tx = self.endpoint.tx.clone();
        let mut loads: FuturesUnordered<LoadFuture> = FuturesUnordered::new();
        let mut session = Session {
            tree: self.tree.as_ref(),
            loader: &self.loader,
            config: &self.config,
            cache: &mut self.cache,
            residency: &mut self.residency,
            in_flight: &mut self.in_flight,
            priming: &mut self.priming,
            priming_outstanding: &mut self.priming_outstanding,
            priming_state: &mut self.priming_state,
            priming_reported: &mut self.priming_reported,
            views: &mut self.views,
            view_generation: &mut self.view_generation,
            out: &mut self.out,
            selected: &mut self.selected,
            rendered_last: &mut self.rendered_last,
            acked: &mut self.acked,
            filled: &mut self.filled,
            view_moved: &mut self.view_moved,
            frame: &mut self.frame,
            primed: &mut self.primed,
        };

        enum Event {
            Load(Result<LoadMsg, Aborted>),
            Client(ClientMessage),
            Closed,
        }

        // Which source gets looked at first, flipped every iteration.
        //
        // A fixed order starves one side or the other. Loads first — the
        // original — meant that while completions kept arriving the camera was
        // never read at all, and every decision was taken on a view it had
        // already left: measured with the altitude climbing from 235 km to
        // 4274 km while `selected`, `visited` and `culled` did not move a
        // single count. Client first starves the mirror image, since a viewer
        // sends a state every frame and completions would go unread.
        //
        // Alternating costs a branch and needs no batching: traversal is cheap
        // enough to run on every event, and running it on every event is what
        // keeps the picture current.
        let mut client_first = true;

        loop {
            let event = poll_fn(|cx| {
                let mut poll_client =
                    |cx: &mut std::task::Context<'_>| match Pin::new(&mut rx).poll_next(cx) {
                        Poll::Ready(Some(msg)) => Poll::Ready(Some(Event::Client(msg))),
                        Poll::Ready(None) => Poll::Ready(Some(Event::Closed)),
                        Poll::Pending => Poll::Ready(None),
                    };
                if client_first {
                    if let Poll::Ready(Some(event)) = poll_client(cx) {
                        return Poll::Ready(event);
                    }
                    match Pin::new(&mut loads).poll_next(cx) {
                        Poll::Ready(Some(done)) => Poll::Ready(Event::Load(done)),
                        Poll::Ready(None) | Poll::Pending => Poll::Pending,
                    }
                } else {
                    if let Poll::Ready(Some(done)) = Pin::new(&mut loads).poll_next(cx) {
                        return Poll::Ready(Event::Load(done));
                    }
                    match poll_client(cx) {
                        Poll::Ready(Some(event)) => Poll::Ready(event),
                        _ => Poll::Pending,
                    }
                }
            })
            .await;
            client_first = !client_first;

            // Why this session ended, named rather than implied.
            //
            // Every one of these used to be a bare `break`. A server that has
            // stopped is then indistinguishable from a server with nothing to
            // do: same silence, same last log line, and a consumer that keeps
            // sending into a channel nobody reads (`let _ = stream.send(..)`).
            // A whole afternoon went into deciding, from the outside, whether
            // this loop was still turning. It says so now.
            let ended: Option<String> = match event {
                Event::Client(first) => {
                    let mut dirty = session.on_client(first);
                    // Coalesce the burst: a viewer sends a state per frame, and
                    // only the last one is worth traversing for.
                    while let Ok(more) = rx.try_recv() {
                        dirty |= session.on_client(more);
                    }
                    if dirty {
                        session
                            .retraverse(&tx, &mut loads)
                            .err()
                            .map(|stop| stop.why("answering a camera move"))
                    } else {
                        None
                    }
                }
                Event::Load(Ok(msg)) => match session.on_load_done(msg, &tx) {
                    Err(stop) => Some(stop.why("delivering a tile")),
                    Ok(()) => session
                        .retraverse(&tx, &mut loads)
                        .err()
                        .map(|stop| stop.why("answering an arrival")),
                },
                // Cancelled load: cleaned up at abort time.
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
    }
}

/// Borrowed view of the server state for the message handlers.
struct Session<'a> {
    tree: &'a dyn TileTree,
    loader: &'a Arc<dyn TileLoader>,
    config: &'a Config,
    cache: &'a mut ResidentCache,
    residency: &'a mut ResidencyView,
    in_flight: &'a mut HashMap<TileId, AbortHandle>,
    /// Coarse tiles asked for once at startup; drained as fetch slots free up.
    priming: &'a mut Vec<TileId>,
    /// Every primed tile that has not yet been answered — still queued above,
    /// or in flight. A tile leaves this set exactly once, when its content is
    /// delivered or when the session gives up on it, which is what makes
    /// [`Priming::settled`] a condition a host can wait on. Kept apart from the
    /// queue because a tile popped from the queue is not yet resolved, and it
    /// was that gap the window used to hang in.
    priming_outstanding: &'a mut HashSet<TileId>,
    /// What the coarse pyramid looks like right now, and what was last sent —
    /// so the report goes out on a change rather than on every pass.
    priming_state: &'a mut Priming,
    priming_reported: &'a mut Option<Priming>,
    views: &'a mut Vec<ViewState>,
    view_generation: &'a mut u64,
    out: &'a mut TraversalOutput,
    /// What this pass touched: the frontier, every ancestor of it, and
    /// everything it asked for. Never a victim — the reference implementation's
    /// "used this frame", which it will go over its cap rather than reclaim.
    selected: &'a mut HashSet<TileId>,
    /// What the previous pass drew. Consulted by the descendant limit — see
    /// [`crate::traversal::Config::loading_descendant_limit`].
    rendered_last: &'a mut HashSet<TileId>,
    acked: &'a mut HashSet<TileId>,
    filled: &'a mut HashSet<TileId>,
    /// Set when the camera moved, which is the only time in-flight loads may be
    /// cancelled. Nothing else reads it: residency is ordered by use, not by
    /// which view asked.
    view_moved: &'a mut bool,
    frame: &'a mut u64,
    primed: &'a mut bool,
}

/// Why the run loop unwinds. Both end the session; only the reason differs,
/// and only the reason is worth a log line.
enum Stop {
    /// The consumer went away: nothing left to send to.
    Gone,
    /// A tile could not be loaded, after the transport had already retried it.
    LoadFailed { tile: TileId, message: String },
}

impl Stop {
    /// What to say about it, given what the session was doing at the time.
    fn why(&self, during: &str) -> String {
        match self {
            Self::Gone => format!("the consumer went away while {during}"),
            Self::LoadFailed { tile, message } => format!(
                "tile {tile:?} could not be loaded and the transport had \
                 already retried it, so the session ends here rather than \
                 serving ground with a hole in it: {message}"
            ),
        }
    }
}

impl Session<'_> {
    /// Returns true when a re-traversal is needed.
    fn on_client(&mut self, msg: ClientMessage) -> bool {
        match msg {
            ClientMessage::ViewerState { views, generation } => {
                *self.views = views;
                *self.view_generation = generation;
                // A new camera position: the traversal it triggers starts a new
                // generation, and the one it displaces becomes history rather
                // than being forgotten.
                *self.view_moved = true;
                true
            }
            // Recorded, not acted on: it changes nothing about what to load —
            // residency is server-side — and it is the only evidence of what
            // the consumer actually holds, which is what decides whether a
            // stand-in is needed. It used to be dropped here.
            ClientMessage::Ack { tile } => {
                self.acked.insert(tile);
                // The ack is the proof the stand-in is gone: real geometry is
                // filed under the same id, so uploading it replaced the fill
                // on the consumer's side. Nothing left to take back.
                self.filled.remove(&tile);
                false
            }
            ClientMessage::Cancel { tile } => {
                if let Some(handle) = self.in_flight.remove(&tile) {
                    handle.abort();
                }
                false
            }
        }
    }

    /// Sends a stand-in for every selected tile the consumer does not hold, and
    /// takes back the ones whose ground it has stopped looking at.
    ///
    /// # The gap
    ///
    /// A tile is selected only once it is resident *here*. It is drawn only once
    /// it is uploaded *there*, and between the two lie a channel, a queue and a
    /// per-frame upload budget. In those frames the consumer falls back to the
    /// nearest ancestor it holds — which covers the missing tile's siblings as
    /// well, so two approximations of one hillside end up over the same ground
    /// and the depth test picks a winner per pixel. That is the shimmer.
    ///
    /// An ack is the only report of that gap, which is why one is now kept.
    ///
    /// # Bookkeeping
    ///
    /// Sent once per tile, and taken back when the tile leaves the selection.
    /// A stand-in is *not* evicted when the real content arrives: the consumer
    /// files both under the same id, so the real one simply replaces it. The set
    /// is bounded by the selection, so a session cannot accumulate them.
    fn fill_the_gaps(&mut self, tx: &UnboundedSender<ServerMessage>) -> Result<(), Stop> {
        let mut sent = Vec::new();
        for (tile, _) in self.out.selected.iter() {
            if self.acked.contains(tile) || self.filled.contains(tile) {
                continue;
            }
            // Synchronous and cache-only by contract: this runs inside the
            // traversal, and a stand-in that waited on the network would arrive
            // together with the tile it was standing in for.
            if let Some(content) = self.loader.fill(*tile) {
                self.filled.insert(*tile);
                sent.push((*tile, content));
            }
        }
        let just_sent = sent.len();
        for (tile, content) in sent {
            tx.unbounded_send(ServerMessage::Fill {
                tile,
                ancestry: self.ancestry(tile),
                content,
            })
            .map_err(|_| Stop::Gone)?;
        }

        // Ground the camera has left: take those stand-ins back, so the
        // consumer does not keep a surface for every tile ever briefly
        // selected — a leak the server cannot see, because it never charged
        // itself for any of them.
        //
        // **As a `Retire`, never as an `Evict`.** `Content` is sent once per
        // residency, so an `Evict` reaching a consumer whose real copy is
        // still in its upload queue destroys the only delivery there will
        // ever be — the ground then wears its stand-in for as long as the
        // cache holds the tile. Two guards were tried before this message
        // existed and both leaked: clearing `filled` when the content is sent
        // races the traversal in between, and sparing acked tiles turns
        // `filled` into a set nothing ever prunes (measured at 3912 and
        // climbing). `Retire` removes the race by construction: the consumer
        // drops only what it holds as a stand-in, so sending it on stale
        // knowledge costs nothing.
        let stale: Vec<TileId> = self
            .filled
            .iter()
            .copied()
            .filter(|t| !self.selected.contains(t))
            .collect();
        for tile in &stale {
            self.filled.remove(tile);
        }
        if !stale.is_empty() {
            tracing::debug!(
                taken = stale.len(),
                held = self.filled.len(),
                "stand-ins retired"
            );
            tx.unbounded_send(ServerMessage::Retire { tiles: stale })
                .map_err(|_| Stop::Gone)?;
        }
        if just_sent > 0 {
            tracing::debug!(just_sent, held = self.filled.len(), "stand-in surfaces");
        }
        crate::metrics::metrics()
            .tiles_filled
            .set(self.filled.len() as u64);
        Ok(())
    }

    /// What the tree knows about a tile, for the wire.
    ///
    /// One place, so no emission site is tempted to derive it from the handle —
    /// which is the mistake this whole change exists to remove.
    fn ancestry(&self, tile: TileId) -> crate::protocol::Ancestry {
        crate::protocol::Ancestry {
            level: self.tree.level(tile),
            parent: self.tree.parent(tile),
        }
    }

    fn retraverse(
        &mut self,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut FuturesUnordered<LoadFuture>,
    ) -> Result<(), Stop> {
        // Whether this pass was provoked by the camera or by a tile arriving.
        // It decides one thing only: whether in-flight loads may be cancelled
        // below. Consumed here, so a pass provoked by an arrival does not
        // inherit the last camera move's licence to cancel.
        let camera_moved = std::mem::take(self.view_moved);
        *self.frame += 1;
        self.prime();
        // Open a new pass before anything is touched: from here on, every tile
        // this traversal reaches is off limits to the sweep at the end of it.
        self.cache.start_pass();
        let started = crate::metrics::stamp();
        traverse(
            self.tree,
            self.residency,
            self.views,
            self.config,
            *self.frame,
            self.rendered_last,
            self.out,
        );
        // What this pass actually put on screen, for the next one to consult.
        // A held REPLACE only cuts its subtree loose when it has nothing of its
        // own drawn yet — once it is on screen there is no black to avoid, and
        // cutting off then would stall refinement rather than accelerate it.
        self.rendered_last.clear();
        self.rendered_last
            .extend(self.out.selected.iter().map(|(tile, _)| *tile));
        // Every pass, with everything that decided it.
        //
        // A selection is meant to be a function of (stage, time). It is not,
        // and this is the trace that shows why: the passes of one render are
        // printed in order, so two runs of the SAME frame can be diffed pass
        // by pass and the exact pass where they part company can be named.
        // Without it the only observable was the final tile count, which says
        // that two runs disagreed but never where.
        crate::det!(
            "pass",
            n = *self.frame,
            selected = self.out.selected.len(),
            sel_digest = crate::determinism::digest(self.out.selected.iter().map(|(t, _)| t.0)),
            requested = self.out.requests.len(),
            req_digest = crate::determinism::digest(self.out.requests.iter().map(|r| r.tile.0)),
            visited = self.out.stats.visited,
            culled = self.out.stats.culled,
            gaps = self.out.stats.gaps,
            // The two counters that say the traversal answered the CLOCK
            // rather than the scene. `deferred_subtrees` above zero means a
            // hold cancelled its subtree because the network was slow, which
            // is how one frame selected 106 tiles on five runs and 7 on the
            // sixth. Untraced, it took an afternoon to find; traced, it is one
            // line of the diff.
            deferred = self.out.stats.deferred_subtrees,
            held_but_drawn = self.out.stats.held_but_drawn,
            resident = self.residency.len(),
            in_flight = self.in_flight.len(),
            priming_left = self.priming.len(),
            camera_moved = camera_moved,
        );
        let m = crate::metrics::metrics();
        m.traversals.inc();
        m.traversal_seconds.record(started.elapsed());
        m.tiles_visited.set(u64::from(self.out.stats.visited));
        m.tiles_culled.set(u64::from(self.out.stats.culled));
        m.tiles_selected.set(u64::from(self.out.stats.selected));
        m.gaps.set(u64::from(self.out.stats.gaps));
        m.selected_by_level.clear();
        for (tile, _) in &self.out.selected {
            m.selected_by_level.inc(self.tree.level(*tile));
        }
        m.queued_by_level.clear();
        for req in &self.out.requests {
            m.queued_by_level.inc(self.tree.level(req.tile));
        }

        // `selected` is the *protected* set: what the budget may not evict.
        // The frontier is always in it — never drop what is on screen.
        self.selected.clear();
        for (t, _) in &self.out.selected {
            self.selected.insert(*t);
            self.cache.touch(*t);
        }
        // Pin the path from each selected tile up to the root: keep ancestors
        // resident (protected from eviction, kept LRU-warm) so the consumer
        // always has a coarser fallback to draw while finer tiles stream in —
        // no holes. Pinned, but NOT sent as selection (they're not the
        // frontier; the consumer renders them only where finer tiles are not
        // ready yet).
        //
        // The whole chain, not a band near the frontier. A traversal asks for
        // every ancestor it does not have, so an unpinned one is evicted and
        // re-requested at once: the session then spends itself reloading the
        // same tiles — 44k loads for 1.2k tiles, in the run that proved it —
        // and never converges. When the working set genuinely exceeds the
        // budget the cache goes over it instead, which is degraded but
        // progressing.
        let frontier: Vec<TileId> = self.out.selected.iter().map(|(t, _)| *t).collect();
        for tile in frontier {
            let mut ancestor = self.tree.parent(tile);
            while let Some(a) = ancestor {
                if !self.selected.insert(a) {
                    break; // this ancestor (and its chain) is already pinned
                }
                self.cache.touch(a);
                ancestor = self.tree.parent(a);
            }
        }
        // Whatever this traversal still asks for is protected too. Evicting a
        // tile the very next traversal re-requests is thrash: it would load,
        // evict something else, be requested again, and the session would spin
        // forever without converging. When the working set genuinely does not
        // fit, the cache goes over budget instead (its documented fallback) —
        // degraded, but progressing.
        for req in &self.out.requests {
            self.selected.insert(req.tile);
        }
        // And so is what a held REPLACE is waiting on. Those are resident, not
        // drawn and not requested — the one category a residency built from
        // "selected plus requested" cannot see — so without this they are swept
        // and asked for again on the very next pass, for ever.
        for tile in &self.out.awaiting {
            self.selected.insert(*tile);
            self.cache.touch(*tile);
        }
        // The protected set just changed, so content held only by the view the
        // camera has left is now the most depletable thing in the cache. Doing
        // this here — and not only when a load lands — is what lets a settled
        // camera give memory back at all.
        let reclaimed = self.cache.trim(self.selected);
        for tile in &reclaimed {
            self.residency.remove(*tile);
            // Same hygiene as the insert-path eviction: the `Evict` below takes
            // the consumer's copy and its stand-in alike, so both records stop
            // being true here. `acked` left behind is the worse of the two — it
            // suppresses the stand-in the next time this ground is looked at,
            // and grows for the life of the session.
            self.acked.remove(tile);
            self.filled.remove(tile);
        }
        // Counted here as well as on insert. This is now the path that reclaims
        // most of what a session gives back — expiry fires while there is still
        // budget headroom, so an insert-only count would have kept reading zero
        // and said nothing had been freed.
        let m = crate::metrics::metrics();
        m.tiles_evicted.add(reclaimed.len() as u64);
        m.resident_bytes.set(self.cache.used_bytes() as u64);
        let (textures, imagery_bytes) = self.cache.imagery_bytes();
        m.imagery_textures.set(textures as u64);
        m.imagery_bytes.set(imagery_bytes as u64);
        // Selection first, reclaim second — and the order is load-bearing.
        //
        // A consumer drains every queued message in one go before it draws, so
        // it applies both in the same frame whatever the order. But it walks up
        // from each selected tile to the nearest ancestor it actually holds, and
        // draws that when the fine tile has not landed. Reclaim first and that
        // ancestor can be gone before the selection naming it is even read: the
        // ground it was covering has nothing left to fall back to, and goes
        // black. Announcing what is wanted before taking anything away costs a
        // line and removes the window entirely.
        tx.unbounded_send(ServerMessage::Select {
            tiles: self.out.selected.clone(),
            // `self.selected` is the protected closure — the frontier, the chain
            // from each of its tiles up to the root, what is requested and what
            // a held REPLACE waits on. That is exactly the set a consumer's walk
            // can pass through, so it is exactly the set whose shape it needs.
            ancestry: self
                .selected
                .iter()
                .map(|tile| (*tile, self.ancestry(*tile)))
                .collect(),
            stats: self.out.stats,
            generation: *self.view_generation,
        })
        .map_err(|_| Stop::Gone)?;
        if !reclaimed.is_empty() {
            tx.unbounded_send(ServerMessage::Evict { tiles: reclaimed })
                .map_err(|_| Stop::Gone)?;
        }
        if self.config.stand_ins {
            self.fill_the_gaps(tx)?;
        }

        // Cancel in-flight loads the camera has moved away from — and only
        // then.
        //
        // A traversal runs on **every load completion**, not only on a camera
        // move, and cancelling on all of them livelocks. One tile arrives, the
        // pass re-runs, `forbid_holes` rolls back a different branch, other
        // tiles fall out of the request set and are killed; the next pass asks
        // for them again, they are spawned again, and the pass after that kills
        // them again. Nothing ever finishes. The symptom is a request count
        // frozen at some number, a selection that stops following the camera,
        // and — the part that cost the most time — *no log output at all*, since
        // every load dies before it can say anything.
        //
        // A load provoked by a tile arriving is still wanted; the pass simply
        // reordered what it needs first. Only a camera move can make one
        // pointless, and even then finishing is often cheaper than restarting.
        // A primed tile is never stale, whatever the camera does. It is not
        // wanted for this view — it is the floor every view falls back to — so
        // it appears in no request set, and killing it would drop it out of the
        // pyramid for good: popped from the queue, aborted, never re-asked, and
        // a host counting arrivals waits on it for ever.
        if camera_moved {
            let wanted: HashSet<TileId> = self.out.requests.iter().map(|r| r.tile).collect();
            let stale: Vec<TileId> = self
                .in_flight
                .keys()
                .filter(|t| !wanted.contains(t) && !self.priming_outstanding.contains(t))
                .copied()
                .collect();
            for t in stale {
                if let Some(handle) = self.in_flight.remove(&t) {
                    handle.abort();
                    crate::metrics::metrics().loads_cancelled.inc();
                }
            }
        }

        // The coarse pyramid first, for as long as there is any of it left.
        //
        // Nothing it competes with is on screen: the host holds its window back
        // until this is on the GPU, so a frontier tile fetched now is a tile
        // fetched for nobody. It is finite — a few thousand tiles — and drains
        // once, at the start of a session, after which this costs a branch.
        while let Some(&tile) = self.priming.last() {
            if self.in_flight.len() >= self.config.maximum_simultaneous_fetches {
                break;
            }
            self.priming.pop();
            // Answered before its turn came round: the traversal asked for
            // it first. Resolved here rather than left outstanding for a load
            // that will never be started.
            if self.residency.is_resident(tile) {
                self.resolve_primed(tile, true);
                continue;
            }
            if self.in_flight.contains_key(&tile) {
                continue;
            }
            // Protected like anything else asked for, so a sweep between now
            // and its arrival cannot make the work pointless.
            self.selected.insert(tile);
            let loader = Arc::clone(self.loader);
            let (handle, registration) = AbortHandle::new_pair();
            let fut: BoxLoadFut = Box::pin(async move {
                let result = loader.load(tile).await;
                (tile, result)
            });
            loads.push(Abortable::new(fut, registration));
            self.in_flight.insert(tile, handle);
            let m = crate::metrics::metrics();
            m.loads_started.inc();
            m.loads_by_level.inc(self.tree.level(tile));
            m.loads_in_flight.set(self.in_flight.len() as u64);
        }
        // Spawn new loads, highest priority first, within the cap. The
        // traversal only requests tiles that have content, so the loader is
        // never asked to load a structural-empty tile.
        for req in &self.out.requests {
            if self.in_flight.len() >= self.config.maximum_simultaneous_fetches {
                break;
            }
            let tile = req.tile;
            if self.residency.is_resident(tile) || self.in_flight.contains_key(&tile) {
                continue;
            }
            let loader = Arc::clone(self.loader);
            let (handle, registration) = AbortHandle::new_pair();
            let fut: BoxLoadFut = Box::pin(async move {
                let result = loader.load(tile).await;
                (tile, result)
            });
            loads.push(Abortable::new(fut, registration));
            self.in_flight.insert(tile, handle);
            let m = crate::metrics::metrics();
            m.loads_started.inc();
            m.loads_by_level.inc(self.tree.level(tile));
            m.loads_in_flight.set(self.in_flight.len() as u64);
        }

        if *self.primed && self.priming_outstanding.is_empty() {
            crate::metrics::metrics().priming_done.set(1);
        }
        self.report_priming(tx)?;
        Ok(())
    }

    /// Asks for the whole coarse pyramid, once, at the start of a session.
    ///
    /// These tiles are pinned in the residency and are what every fallback
    /// walks up to, so they should be on the GPU before anything needs them —
    /// not discovered by a camera that happens to fly over them. Measured
    /// without this, on ground already flown several times: level 5 held 291 of
    /// its 1024 tiles and level 3 held none at all, so a fast movement fell
    /// through the safety net onto bare ground.
    ///
    /// They are **requested, not selected**. `prepared` and `selection` are
    /// separate sets in the consumer, so a tile whose content arrives is
    /// uploaded and held without being drawn — which is exactly what a fallback
    /// is: ready, and invisible until something needs it.
    ///
    /// The count is bounded by the tree, not by the view: `4^level` tiles, so
    /// 1365 of them through level 5, about 480 MiB of imagery on the GPU
    /// against a budget measured in gigabytes.
    fn prime(&mut self) {
        if *self.primed {
            return;
        }
        let m = crate::metrics::metrics();
        let Some(level) = self.config.pinned_level else {
            *self.primed = true;
            // Nothing to wait for, so anyone gating on this may start at once.
            m.priming_done.set(1);
            return;
        };
        *self.primed = true;

        let mut wanted = Vec::new();
        let mut frontier = self.tree.roots();
        for _ in 0..=level {
            let mut next = Vec::new();
            for tile in frontier {
                if self.tree.properties(tile).has_content {
                    wanted.push(tile);
                }
                next.extend(self.tree.children(tile));
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        // The coarse pyramid is the floor the whole selection stands on, and
        // its membership depends on `tree.children()` — which is exactly the
        // thing that changes shape as availability arrives. A pyramid that
        // differs between two runs is a selection that will differ too.
        crate::det!(
            "prime",
            through_level = level,
            tiles = wanted.len(),
            digest = crate::determinism::digest(wanted.iter().map(|t| t.0)),
        );
        tracing::info!(
            through_level = level,
            tiles = wanted.len(),
            "priming the coarse pyramid onto the GPU"
        );
        m.priming_total.set(wanted.len() as u64);
        self.priming_state.total = wanted.len() as u32;
        self.priming_outstanding.extend(wanted.iter().copied());
        *self.priming = wanted;
    }

    /// Marks a primed tile as answered — delivered, or given up on.
    ///
    /// Called on **every** terminal outcome of a load, because the count a host
    /// waits on has to fall to zero however the tile ended. A tile the source
    /// does not serve is resolved, not pending: it will never be on the GPU, and
    /// a gate that keeps waiting for it is a gate that never opens.
    fn resolve_primed(&mut self, tile: TileId, arrived: bool) {
        if !self.priming_outstanding.remove(&tile) {
            return;
        }
        if !arrived {
            self.priming_state.unavailable += 1;
        }
    }

    /// Sends the coarse-pyramid count when it has moved.
    fn report_priming(&mut self, tx: &UnboundedSender<ServerMessage>) -> Result<(), Stop> {
        self.priming_state.outstanding = self.priming_outstanding.len() as u32;
        let m = crate::metrics::metrics();
        m.priming_pending
            .set(u64::from(self.priming_state.outstanding));
        m.priming_unavailable
            .set(u64::from(self.priming_state.unavailable));
        if *self.priming_reported == Some(*self.priming_state) {
            return Ok(());
        }
        *self.priming_reported = Some(*self.priming_state);
        tx.unbounded_send(ServerMessage::Priming(*self.priming_state))
            .map_err(|_| Stop::Gone)
    }

    fn on_load_done(
        &mut self,
        (tile, result): LoadMsg,
        tx: &UnboundedSender<ServerMessage>,
    ) -> Result<(), Stop> {
        self.in_flight.remove(&tile);
        let m = crate::metrics::metrics();
        m.loads_in_flight.set(self.in_flight.len() as u64);
        match result {
            Err(e) => {
                m.loads_failed.inc();
                crate::det!("load_err", tile = tile.0, level = self.tree.level(tile));
                self.fail(tile, e.to_string(), tx)
            }
            // Topology grew in place (external tileset grafted): the graft
            // cleared the host's content, so the next traversal won't
            // re-request it; the revealed children load on their own. Nothing
            // will ever be uploaded for this tile, so a pyramid waiting on it
            // is waiting on nothing.
            Ok(Loaded::Expanded) => {
                self.resolve_primed(tile, false);
                Ok(())
            }
            Ok(Loaded::Content(decoded)) => {
                let size = decoded.byte_size();
                // Imagery is charged separately because it is shared: what this
                // tile costs the budget is its geometry plus whatever share of
                // the draped textures no other resident tile is already paying.
                let imagery: Vec<_> = decoded
                    .imagery
                    .iter()
                    .map(|l| (l.coord, l.texture.resident_bytes()))
                    .collect();
                // The level comes from the tree, which is the only thing that
                // can read an opaque handle — see `TileTree::level`.
                let evicted =
                    self.cache
                        .insert(tile, self.tree.level(tile), size, &imagery, self.selected);
                m.loads_completed.inc();
                m.tiles_evicted.add(evicted.len() as u64);
                m.resident_bytes.set(self.cache.used_bytes() as u64);
                let (textures, bytes) = self.cache.imagery_bytes();
                m.imagery_textures.set(textures as u64);
                m.imagery_bytes.set(bytes as u64);
                for e in &evicted {
                    self.residency.remove(*e);
                    // The consumer is about to drop it too, so its ack stops
                    // being true. Left behind, it would suppress the stand-in
                    // the next time this ground is looked at. The same goes for
                    // `filled`: an `Evict` takes the stand-in with everything
                    // else, so tracking it past this point is fiction.
                    self.acked.remove(e);
                    self.filled.remove(e);
                }
                if !evicted.is_empty() {
                    crate::det!(
                        "evict",
                        count = evicted.len(),
                        digest = crate::determinism::digest(evicted.iter().map(|t| t.0)),
                    );
                    tx.unbounded_send(ServerMessage::Evict { tiles: evicted })
                        .map_err(|_| Stop::Gone)?;
                }
                self.residency.insert(tile);
                self.resolve_primed(tile, true);
                // Arrivals are genuinely unordered — the network decides. On
                // its own event name so a comparison can drop it and still
                // compare every decision that was taken.
                crate::det!(
                    "load_ok",
                    tile = tile.0,
                    level = self.tree.level(tile),
                    resident = self.residency.len(),
                );
                tx.unbounded_send(ServerMessage::Content {
                    tile,
                    ancestry: self.ancestry(tile),
                    content: TileContent::Decoded(decoded),
                })
                .map_err(|_| Stop::Gone)?;
                Ok(())
            }
        }
    }

    /// A tile that could not be loaded ends the session.
    ///
    /// There used to be a set of given-up-on tiles here instead, and it was the
    /// most expensive twelve lines in the repository. It was never emptied, so
    /// a tile lost to one dropped socket was lost for as long as the process
    /// lived; it was consulted by the traversal, so the shape of the ground
    /// silently depended on which requests had happened to fail; and it let the
    /// session carry on pretending, which is how a render shipped forty-eight
    /// black frames instead of stopping on the first one. Two nodes rendering
    /// the same frame would not even agree with each other — the reproducibility
    /// the whole design is for, traded away for a hidden mutable set.
    ///
    /// So the tile is not remembered: the transport has already retried (see
    /// `is_retryable_transport`), and a failure that survives that is not a
    /// hiccup. The consumer is told which tile and why, and then the stream
    /// closes under it, which is a thing a caller cannot ignore.
    fn fail(
        &mut self,
        tile: TileId,
        message: String,
        tx: &UnboundedSender<ServerMessage>,
    ) -> Result<(), Stop> {
        // Sent before unwinding: a bare closed channel says a tile is missing
        // without ever saying which, and that was a whole afternoon once.
        tx.unbounded_send(ServerMessage::Error {
            tile: Some(tile),
            message: message.clone(),
        })
        .map_err(|_| Stop::Gone)?;
        Err(Stop::LoadFailed { tile, message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FsFetcher;
    use crate::protocol::GeometryStream;
    use futures_util::task::LocalSpawnExt;
    use glam::{dvec2, dvec3};
    use std::sync::Mutex;
    use url::Url;

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

    /// The same fixture with one child's content missing from disk — a tile the
    /// source will never serve, which over a globe is most of an ocean.
    fn write_fixture_missing_one_child(dir: &std::path::Path) -> Url {
        let url = write_fixture(dir);
        std::fs::remove_file(dir.join("b.glb")).expect("remove b");
        url
    }

    /// A tile the source cannot serve ends the session, loudly.
    ///
    /// This asserted the opposite until 2026-09-08, and the opposite was a
    /// disaster. The session used to write the tile down in a `failed` set,
    /// stop asking for it, and carry on: the pyramid settled, the gate opened,
    /// and the ground simply had a hole in it that nothing downstream could
    /// see. On the farm that hole reached the procedural as "the frame did not
    /// converge", which emits nothing at all — forty-eight black frames, shipped
    /// as a video, because one endpoint dropped three connections.
    ///
    /// The transport retries a transient failure before we ever hear about it
    /// (`is_retryable_transport`), so a failure that reaches here is real. The
    /// contract is now: say which tile, say why, and stop. A caller that is
    /// waiting on a stream gets a closed stream, which it cannot mistake for
    /// success.
    #[test]
    fn a_tile_the_source_cannot_serve_ends_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture_missing_one_child(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Level 0 and 1: the root and its two children, one of which has no
        // content on disk.
        let config = Config {
            pinned_level: Some(1),
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");

        const MAX_STEPS: usize = 2_000;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut reported: Option<(Option<TileId>, String)> = None;
        let mut stopped = false;
        for _ in 0..MAX_STEPS {
            if !stopped && server.as_mut().poll(&mut cx).is_ready() {
                stopped = true;
            }
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                if let ServerMessage::Error { tile, message } = msg {
                    reported.get_or_insert((tile, message));
                }
            }
            if stopped {
                break;
            }
        }

        assert!(
            stopped,
            "the session kept running after a tile it can never serve"
        );
        let (tile, message) =
            reported.expect("the session stopped without ever saying which tile it could not load");
        assert!(tile.is_some(), "an unattributed failure is not actionable");
        assert!(
            !message.is_empty(),
            "the reason has to travel with the failure"
        );
        // And the stream is closed under the consumer, so nothing downstream
        // can read the truncated selection as a finished one.
        assert!(
            matches!(stream.poll_message(&mut cx), Poll::Ready(None)),
            "the stream stayed open after the session ended"
        );
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

    /// Steps a session by hand until it goes quiet, returning
    /// `(loads, evictions)` — or `None` if it never settled.
    ///
    /// Hand-stepping rather than running to quiescence because a session can
    /// fail to converge (evicting a tile the next traversal re-requests), and a
    /// test must report that as a failure rather than hang. `FsFetcher`
    /// resolves inline, so every poll makes progress: the bound is a step
    /// count, not a timeout.
    fn settle(
        server: &mut Pin<Box<impl Future<Output = ()>>>,
        stream: &mut InProcessStream,
    ) -> Option<(usize, Vec<TileId>)> {
        const MAX_STEPS: usize = 2_000;
        const QUIET_STEPS: usize = 8;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let (mut loaded, mut quiet) = (0usize, 0usize);
        let mut evicted: Vec<TileId> = Vec::new();
        for _ in 0..MAX_STEPS {
            let _ = server.as_mut().poll(&mut cx);
            let mut got = false;
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                got = true;
                match msg {
                    ServerMessage::Content { .. } => loaded += 1,
                    ServerMessage::Evict { tiles } => evicted.extend(tiles),
                    _ => {}
                }
            }
            quiet = if got { 0 } else { quiet + 1 };
            if quiet > QUIET_STEPS {
                return Some((loaded, evicted));
            }
        }
        None
    }

    /// **A stand-in that has been replaced is not evicted.**
    ///
    /// `Fill` and `Content` are filed under the same id by the consumer, so the
    /// real thing simply replaces the stand-in when it lands. The server has to
    /// notice: it keeps a `filled` set so it can take a stand-in back when the
    /// camera leaves that ground, and if the tile is still in that set once its
    /// real content has been sent, the take-back names real geometry.
    ///
    /// What that costs, measured in the viewer before this test existed: the
    /// consumer drops the tile, the server still holds it resident and still has
    /// its ack, so it sends neither `Content` (resident) nor `Fill` (acked)
    /// when the ground is looked at again. The walk climbs to an ancestor and
    /// stays there — **28 tiles drawn coarse with nothing loading and no error
    /// reported**, stable, for as long as the session ran. On screen that is
    /// blurry rectangles with straight tile-aligned edges, which is a different
    /// artefact from the ragged organic outlines of two surfaces fighting, and
    /// was mistaken for it.
    #[test]
    fn a_stand_in_that_has_been_replaced_is_not_evicted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        /// Real content for everything, and a stand-in on demand.
        struct Standing(Arc<dyn TileLoader>);

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Standing {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
                Some(crate::content::DecodedTileContent {
                    withheld_drape: None,
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    imagery: Vec::new(),
                    local_origin_ecef: glam::DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                })
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Standing(inner)) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());

        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut delivered: HashSet<TileId> = HashSet::new();
        let mut wrongly_taken: Vec<TileId> = Vec::new();

        // Look somewhere, then look elsewhere: the second view is what makes the
        // first view's ground leave the selection, which is when a stand-in is
        // taken back.
        for view in [near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![view],
                    generation: 0,
                })
                .expect("send");
            for _ in 0..256 {
                let _ = server.as_mut().poll(&mut cx);
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    match msg {
                        ServerMessage::Content { tile, .. } => {
                            delivered.insert(tile);
                            // A real consumer acks what it uploads; a stand-in
                            // is never acked, which is what the server relies on.
                            let _ = stream.send(ClientMessage::Ack { tile });
                        }
                        ServerMessage::Evict { tiles } => {
                            for t in tiles {
                                if delivered.contains(&t) {
                                    wrongly_taken.push(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        assert!(
            wrongly_taken.is_empty(),
            "the server took back {} tiles whose real content it had already \
             sent: {wrongly_taken:?}. The consumer drops them, the server still \
             holds them resident and acked, and that ground is coarse for the \
             rest of the session",
            wrongly_taken.len()
        );
    }

    /// **Taking back a stand-in must never cost the consumer real content.**
    ///
    /// `Content` is sent exactly once per residency: the server marks the tile
    /// resident and never re-sends it. So anything that makes the consumer
    /// discard that one delivery — including its copy still waiting in the
    /// upload queue, which is the state `Evict` explicitly purges — loses the
    /// tile for as long as the server's cache holds it. Measured in the viewer:
    /// 1853 tiles taken back in one flight, 232 of them re-issued a stand-in
    /// right after, coarse for the rest of the session with nothing loading.
    ///
    /// The consumer that matters here is the honest one: it uploads on a
    /// budget, so its ack arrives *later* than the content. This test holds
    /// that gap open by never acking, and asserts the server still never sends
    /// an `Evict` for a tile whose content it has delivered — a take-back of
    /// the stand-in must travel as [`ServerMessage::Retire`], which the
    /// consumer applies only to surfaces it holds *as stand-ins*.
    #[test]
    fn a_stand_in_take_back_never_travels_as_evict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        struct Standing(Arc<dyn TileLoader>);
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Standing {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
                Some(crate::content::DecodedTileContent {
                    withheld_drape: None,
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    imagery: Vec::new(),
                    local_origin_ecef: glam::DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                })
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Standing(inner)) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());

        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut delivered: HashSet<TileId> = HashSet::new();
        let mut evicted_after_delivery: Vec<TileId> = Vec::new();

        // Look somewhere, then elsewhere — the move is what makes the first
        // view's stand-ins leave the selection. **Never ack**: the upload queue
        // of a real consumer is exactly this window, held open.
        for view in [near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![view],
                    generation: 0,
                })
                .expect("send");
            for _ in 0..256 {
                let _ = server.as_mut().poll(&mut cx);
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    match msg {
                        ServerMessage::Content { tile, .. } => {
                            delivered.insert(tile);
                        }
                        ServerMessage::Evict { tiles } => {
                            for t in tiles {
                                if delivered.contains(&t) {
                                    evicted_after_delivery.push(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        assert!(
            evicted_after_delivery.is_empty(),
            "{} tiles were named by an Evict after their one Content delivery, \
             with no cache pressure to justify it: the consumer purges them from \
             its upload queue and the server never re-sends — that ground is a \
             stand-in for the rest of the session. A stand-in take-back must be \
             a Retire. Tiles: {evicted_after_delivery:?}",
            evicted_after_delivery.len()
        );
    }

    /// A turn must not re-stream its own wake.
    ///
    /// The tiles the camera has just left are the ones it is most likely to want
    /// back, and evicting them the instant they leave the frustum was measured at
    /// 150 reloads over the second half of an orbit. What keeps them is headroom
    /// and nothing else: an LRU only evicts when it is over its cap, so with room
    /// for both working sets the previous view's tiles simply stay, ordered
    /// behind the current one's and ahead of anything older.
    ///
    /// This is where a ring of "recent camera positions" used to be — a second
    /// mechanism protecting what ordering already protects, and one that, when
    /// it was also allowed to *evict*, threw away 93 185 tiles in a session.
    /// A consumer that has not acknowledged a selected tile is sent a stand-in.
    ///
    /// The trigger is the *consumer's* gap, not the server's: a tile is selected
    /// only once it is resident here, and drawn only once it is uploaded there.
    /// Between the two lie a channel, a queue and an upload budget, and in those
    /// frames the consumer falls back to an ancestor that also covers the
    /// missing tile's siblings — two surfaces over one patch of ground.
    ///
    /// This test never acks, which is the extreme of that gap and the only shape
    /// a test can hold still. Written first against "selected but not resident",
    /// where it failed with "the view selected nothing to stand in for" — a
    /// condition that cannot occur, because selection implies residency.
    #[test]
    fn a_tile_the_consumer_has_not_acknowledged_gets_a_stand_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        /// Real content for everything, and a stand-in on demand. Both halves
        /// matter: without content nothing is ever selected, and without a fill
        /// there is nothing to observe.
        struct Standing(Arc<dyn TileLoader>);

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Standing {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
                Some(crate::content::DecodedTileContent {
                    withheld_drape: None,
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    imagery: Vec::new(),
                    local_origin_ecef: glam::DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                })
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Standing(inner)) as Arc<dyn TileLoader>,
            // The default configuration, deliberately: stand-ins are on by
            // default, and a test that switched them on itself would keep
            // passing after the default was quietly turned back off.
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());
        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");

        // Drive to quiescence without ever acking, which is what a consumer
        // whose uploads never finish looks like from here.
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut selected: Vec<TileId> = Vec::new();
        let mut filled: HashSet<TileId> = HashSet::new();
        for _ in 0..64 {
            let _ = server.as_mut().poll(&mut cx);
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                match msg {
                    ServerMessage::Select { tiles, .. } => {
                        selected = tiles.iter().map(|(t, _)| *t).collect();
                    }
                    ServerMessage::Fill { tile, .. } => {
                        filled.insert(tile);
                    }
                    ServerMessage::Evict { tiles } => {
                        for t in tiles {
                            filled.remove(&t);
                        }
                    }
                    _ => {}
                }
            }
        }

        assert!(!selected.is_empty(), "the view selected nothing");
        for tile in &selected {
            assert!(
                filled.contains(tile),
                "{tile:?} is selected and unacknowledged, and got no stand-in \
                 (stand-ins: {filled:?})"
            );
        }
    }

    #[test]
    fn tiles_the_camera_just_left_survive_the_next_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Room for the whole fixture, which is the point: nothing is over any
        // ceiling, so nothing may be reclaimed.
        let config = Config::default();
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        let near_selection = selection_after(&mut server, &mut stream);
        assert!(
            near_selection.len() > 1,
            "the near view selected several tiles"
        );

        stream
            .send(ClientMessage::ViewerState {
                views: vec![far_view()],
                generation: 0,
            })
            .expect("send");
        let (_, evicted) = settle(&mut server, &mut stream).expect("far view settles");

        let lost: Vec<_> = evicted
            .iter()
            .filter(|t| near_selection.contains(t))
            .collect();
        assert!(
            lost.is_empty(),
            "the camera moved once and already lost {lost:?} from where it came"
        );
    }

    /// Drives one view to quiescence and reports what it settled on.
    fn selection_after(
        server: &mut Pin<Box<impl Future<Output = ()>>>,
        stream: &mut InProcessStream,
    ) -> HashSet<TileId> {
        const MAX_STEPS: usize = 2_000;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut selection = HashSet::new();
        let mut quiet = 0;
        for _ in 0..MAX_STEPS {
            let _ = server.as_mut().poll(&mut cx);
            let mut got = false;
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                got = true;
                if let ServerMessage::Select { tiles, .. } = msg {
                    selection = tiles.iter().map(|(t, _)| *t).collect();
                }
            }
            quiet = if got { 0 } else { quiet + 1 };
            if quiet > 8 {
                break;
            }
        }
        selection
    }

    /// A loader that refuses to serve the same tile past a sane number of
    /// times.
    ///
    /// The failure this guards against is a livelock, and a livelock here does
    /// not fail a test on its own. `settle` gives up after a fixed number of
    /// steps and returns whatever it saw, so a server reloading the same tile
    /// for ever looks, from the outside, exactly like one that finished: the
    /// selection is right, the eviction counts are plausible, and nothing
    /// asserts. It is only visible against the network, which a test does not
    /// have. Counting loads is what turns it into an assertion — and the count
    /// has to be per tile, since the totals alone stay unremarkable.
    struct CountingLoader {
        inner: Arc<dyn TileLoader>,
        counts: Arc<Mutex<HashMap<TileId, usize>>>,
    }

    /// Generous: a tile may legitimately be loaded again after the camera has
    /// genuinely left and come back. Nothing converging comes near this.
    const SANE_RELOADS: usize = 20;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl TileLoader for CountingLoader {
        async fn load(&self, tile: TileId) -> Result<Loaded, LoadError> {
            let seen = {
                let mut counts = self.counts.lock().expect("counts");
                let seen = counts.entry(tile).or_insert(0);
                *seen += 1;
                *seen
            };
            assert!(
                seen <= SANE_RELOADS,
                "{tile:?} has been loaded {seen} times: the server is expiring \
                 content it is about to ask for again"
            );
            self.inner.load(tile).await
        }
    }

    /// Expiry must not reclaim what the current camera position still needs.
    ///
    /// One camera position takes many passes to settle, and a tile that has just
    /// arrived is often selected by none of them — `forbid_holes` draws its
    /// parent until all four siblings are ready. It is then resident, not
    /// selected, and no longer requested. Recording only the newest pass's needs
    /// drops it from the sole generation that ever named it; expiry takes it;
    /// the next pass asks for it again. Nothing converges, and because the loads
    /// resolve inside the same poll, nothing yields either.
    #[test]
    fn a_still_camera_does_not_expire_what_it_is_about_to_ask_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let loader: Arc<dyn TileLoader> = Arc::new(CountingLoader {
            inner: Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher))),
            counts: Arc::clone(&counts),
        });

        // A cap low enough that the LRU must evict while the view is still
        // filling in — the configuration in which a reload cycle is easiest to
        // close, since eviction and loading are then happening at once.
        let config = Config {
            resident_tile_limit: 3,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process_with(tree, loader, config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        settle(&mut server, &mut stream).expect("a still camera settles");

        let counts = counts.lock().expect("counts");
        assert!(!counts.is_empty(), "the near view loaded something");
        for (tile, seen) in counts.iter() {
            assert_eq!(
                *seen, 1,
                "{tile:?} was loaded {seen} times for one camera position"
            );
        }
    }

    /// A camera going back and forth over a cache too small for both views has
    /// to converge, not oscillate. Each move evicts what the other view wanted;
    /// the guarantee is that every pass still reaches quiescence rather than
    /// evicting and reloading inside a single traversal.
    #[test]
    fn a_camera_moving_back_and_forth_converges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        let config = Config {
            resident_tile_limit: 2,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        for view in [near_view(), far_view(), near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![view],
                    generation: 0,
                })
                .expect("send");
            settle(&mut server, &mut stream).expect("settles");
        }
    }

    /// The cap is what bounds a session: park the camera somewhere that needs
    /// fewer tiles than it holds, and the ones it left must be reclaimed.
    #[test]
    fn tiles_left_behind_by_the_camera_are_evicted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Room for a working set, not for the whole tree: the tiles the camera
        // leaves behind are what has to go. The count is the ceiling that
        // matters — the fixture's tiles are 3-vertex triangles, so no plausible
        // byte budget would ever separate them.
        let config = Config {
            resident_tile_limit: 3,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        let (loaded, _) = settle(&mut server, &mut stream).expect("first view settles");
        assert!(
            loaded > 1,
            "the near view loaded several tiles (got {loaded})"
        );

        // Pull far back and stay there. The far view needs fewer tiles than the
        // near one did, so what it stops selecting stops being protected, and
        // scores lowest for the camera's new distance.
        let moves = 3;
        let mut evicted = Vec::new();
        for _ in 0..moves {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![far_view()],
                    generation: 0,
                })
                .expect("send");
            let (_, dropped) = settle(&mut server, &mut stream).expect("far view settles");
            evicted.extend(dropped);
        }

        assert!(
            !evicted.is_empty(),
            "after {moves} moves away, the tiles left behind were still not reclaimed"
        );
    }

    /// Depletion order is by what a tile is worth from where the camera *is*,
    /// which is both its depth and its distance in one number — its
    /// screen-space error.
    ///
    /// Pull back until the root alone satisfies the error, and the leaves the
    /// near view pulled in are worth almost nothing while the coarse tiles
    /// covering the same ground are worth a great deal. So the leaves must go
    /// first and the coarse tiles last, whatever order they arrived in. Age
    /// alone gets this backwards on a zoom-out: the root is the *oldest* tile
    /// in the cache and the leaves are the newest.
    #[test]
    fn depletion_takes_the_deepest_tiles_before_the_coarse_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // A second view of the same tileset, to read each tile's geometric error
        // back out — the test must not hard-code which id is a leaf.
        let reference = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let tree = TilesetTree::new(Arc::new(RwLock::new(reference)));
        let error_of = |t: TileId| tree.properties(t).geometric_error;

        // Tight enough that the sweep has to cross a level. A cap that only
        // forces it to drop a couple of leaves proves nothing about order —
        // every candidate has the same geometric error, and any sweep at all
        // looks correct.
        let config = Config {
            resident_tile_limit: 2,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        settle(&mut server, &mut stream).expect("the near view settles");

        stream
            .send(ClientMessage::ViewerState {
                views: vec![far_view()],
                generation: 0,
            })
            .expect("send");
        let (_, evicted) = settle(&mut server, &mut stream).expect("the far view settles");
        assert!(!evicted.is_empty(), "pulling back reclaimed nothing at all");

        // Deepest first: a smaller geometric error is a finer tile, and the
        // sweep must work its way up the pyramid, never down.
        //
        // This fixture has three levels and drops a whole level at a time, so
        // every tile in one sweep tends to share a geometric error and the
        // ordering here cannot, on its own, tell the weight apart from plain
        // recency — checked, and it passes either way. It stands as a regression
        // guard; what proves the weight is
        // [`the_depletion_weight_rises_with_depth_and_with_distance`].
        let errors: Vec<f64> = evicted.iter().map(|t| error_of(*t)).collect();
        for pair in errors.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "the sweep took a coarser tile before a finer one: {errors:?}"
            );
        }
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

    #[test]
    fn end_to_end_session_converges_to_leaves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let leaves = tileset.tile(root).children.clone();

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());

        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let expected: HashSet<TileId> = leaves.iter().copied().collect();
        let final_selection = pool.run_until(async move {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");

            let mut contents: Vec<TileId> = Vec::new();
            let mut last_select: Vec<(TileId, f64)> = Vec::new();
            while let Some(msg) = stream.next_message().await {
                match msg {
                    ServerMessage::Content { tile, content, .. } => {
                        assert!(
                            matches!(content, TileContent::Decoded(_)),
                            "in-process content is always decoded"
                        );
                        contents.push(tile);
                    }
                    ServerMessage::Retire { .. } => {}
                    // Stand-ins are not content: this test counts what actually
                    // arrived, and folding them in would let it pass on
                    // approximations.
                    ServerMessage::Fill { .. } => {}
                    ServerMessage::Select { tiles, .. } => {
                        last_select = tiles;
                        // Steady state: both leaves selected, nothing pending.
                        let sel: HashSet<TileId> = last_select.iter().map(|(t, _)| *t).collect();
                        if sel == expected {
                            break;
                        }
                    }
                    ServerMessage::Error { message, .. } => {
                        unreachable!("unexpected error: {message}")
                    }
                    ServerMessage::Evict { .. } | ServerMessage::Priming(_) => {}
                }
            }
            (contents, last_select)
        });

        let (contents, selection) = final_selection;
        let sel: HashSet<TileId> = selection.iter().map(|(t, _)| *t).collect();
        assert_eq!(sel, leaves.iter().copied().collect::<HashSet<_>>());
        assert!(!sel.contains(&root), "REPLACE: leaves displaced the root");
        assert!(
            contents.len() >= 2,
            "both leaves must have been decoded (got {contents:?})"
        );
    }

    #[test]
    fn external_tileset_is_grafted_and_its_leaf_rendered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let glb = crate::content::tests::test_glb();
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/leaf.glb"), &glb).expect("write");
        // The external root resolves its content against ITS json, in sub/.
        std::fs::write(
            dir.path().join("sub/external.json"),
            r#"{ "asset": { "version": "1.1" }, "geometricError": 4,
                 "root": { "boundingVolume": { "sphere": [0,0,0,80] },
                           "geometricError": 0, "content": { "uri": "leaf.glb" } } }"#,
        )
        .expect("write external");
        let root_json = r#"{
          "asset": { "version": "1.1" }, "geometricError": 200,
          "root": { "boundingVolume": { "sphere": [0,0,0,100] },
                    "geometricError": 0, "refine": "REPLACE",
                    "content": { "uri": "sub/external.json" } }
        }"#;
        let path = dir.path().join("tileset.json");
        std::fs::write(&path, root_json).expect("write root");
        let url = Url::from_file_path(&path).expect("url");
        let tileset = Tileset::from_json_bytes(root_json.as_bytes(), &url).expect("ts");

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());
        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let saw_content = pool.run_until(async move {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");
            while let Some(msg) = stream.next_message().await {
                match msg {
                    // The grafted external leaf must arrive decoded.
                    ServerMessage::Content { content, .. } => {
                        return matches!(content, TileContent::Decoded(_));
                    }
                    ServerMessage::Error { message, .. } => {
                        unreachable!("unexpected error: {message}")
                    }
                    _ => {}
                }
            }
            false
        });
        assert!(saw_content, "external tileset leaf decoded end-to-end");
    }

    #[test]
    fn missing_content_reports_error_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset_json = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 0,
            "refine": "REPLACE",
            "content": { "uri": "missing.glb" }
          }
        }"#;
        let path = dir.path().join("tileset.json");
        std::fs::write(&path, tileset_json).expect("write");
        let url = Url::from_file_path(&path).expect("url");
        let tileset = Tileset::from_json_bytes(tileset_json.as_bytes(), &url).expect("tileset");

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());
        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let got_error = pool.run_until(async move {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");
            while let Some(msg) = stream.next_message().await {
                if let ServerMessage::Error { tile, .. } = msg {
                    return tile.is_some();
                }
            }
            false
        });
        assert!(got_error, "fetch failure must surface as a typed Error");
    }
}
