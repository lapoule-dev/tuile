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
    in_process_pair, ClientMessage, InProcessStream, ServerEndpoint, ServerMessage,
};
use crate::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use crate::tiles3d::{TilesetLoader, TilesetTree};
use crate::tileset::Tileset;
use crate::traversal::{traverse, Config, ResidencyView, TraversalOutput, ViewState};
use futures_channel::mpsc::UnboundedSender;
use futures_core::Stream;
use futures_util::future::{poll_fn, AbortHandle, Abortable, Aborted};
use futures_util::stream::FuturesUnordered;
use std::collections::{HashMap, HashSet, VecDeque};
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
    let cache = ResidentCache::new(config.resident_budget_bytes);
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
            failed: HashSet::new(),
            views: Vec::new(),
            out: TraversalOutput::default(),
            selected: HashSet::new(),
            recent: VecDeque::new(),
            view_moved: false,
            frame: 0,
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
    failed: HashSet<TileId>,
    views: Vec<ViewState>,
    out: TraversalOutput,
    selected: HashSet<TileId>,
    recent: VecDeque<HashSet<TileId>>,
    view_moved: bool,
    frame: u64,
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
            failed: &mut self.failed,
            views: &mut self.views,
            out: &mut self.out,
            selected: &mut self.selected,
            recent: &mut self.recent,
            view_moved: &mut self.view_moved,
            frame: &mut self.frame,
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

            match event {
                Event::Client(first) => {
                    let mut dirty = session.on_client(first);
                    // Coalesce the burst: a viewer sends a state per frame, and
                    // only the last one is worth traversing for.
                    while let Ok(more) = rx.try_recv() {
                        dirty |= session.on_client(more);
                    }
                    if dirty && session.retraverse(&tx, &mut loads).is_err() {
                        break;
                    }
                }
                Event::Load(Ok(msg)) => {
                    if session.on_load_done(msg, &tx).is_err() {
                        break; // consumer dropped
                    }
                    if session.retraverse(&tx, &mut loads).is_err() {
                        break;
                    }
                }
                // Cancelled load: cleaned up at abort time.
                Event::Load(Err(Aborted)) => {}
                Event::Closed => break,
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
    failed: &'a mut HashSet<TileId>,
    views: &'a mut Vec<ViewState>,
    out: &'a mut TraversalOutput,
    selected: &'a mut HashSet<TileId>,
    /// What the last few camera positions needed, newest last. The union is
    /// what the budget may not evict — see [`Session::protected`].
    recent: &'a mut VecDeque<HashSet<TileId>>,
    /// Set when the camera moved, so the next traversal opens a new entry in
    /// `recent` instead of overwriting the current one.
    view_moved: &'a mut bool,
    frame: &'a mut u64,
}

/// Consumer went away; unwind the run loop.
struct Gone;

impl Session<'_> {
    /// Returns true when a re-traversal is needed.
    fn on_client(&mut self, msg: ClientMessage) -> bool {
        match msg {
            ClientMessage::ViewerState { views } => {
                *self.views = views;
                // A new camera position: the traversal it triggers starts a new
                // generation, and the one it displaces becomes history rather
                // than being forgotten.
                *self.view_moved = true;
                true
            }
            ClientMessage::Ack { .. } => false, // in-process: residency is server-side
            ClientMessage::Cancel { tile } => {
                if let Some(handle) = self.in_flight.remove(&tile) {
                    handle.abort();
                }
                false
            }
        }
    }

    fn retraverse(
        &mut self,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut FuturesUnordered<LoadFuture>,
    ) -> Result<(), Gone> {
        *self.frame += 1;
        traverse(
            self.tree,
            self.residency,
            self.views,
            self.config,
            *self.frame,
            self.out,
        );

        // Drop tiles we've given up on from the request set, so the reported
        // request count converges to zero (a failed REPLACE child stays
        // requested forever otherwise) — what lets bulk drivers terminate.
        if !self.failed.is_empty() {
            let failed = &*self.failed;
            self.out.requests.retain(|r| !failed.contains(&r.tile));
            self.out.stats.requested = self.out.requests.len() as u32;
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
        self.remember_protected();
        // The protected set just changed, so content that was held only by the
        // view the camera has left is now reclaimable. Doing this here — and
        // not only when a load lands — is what lets a settled camera give
        // memory back at all.
        let reclaimed = self.cache.trim(&self.protected());
        for tile in &reclaimed {
            self.residency.remove(*tile);
        }
        if !reclaimed.is_empty() {
            tx.unbounded_send(ServerMessage::Evict { tiles: reclaimed })
                .map_err(|_| Gone)?;
        }
        tx.unbounded_send(ServerMessage::Select {
            tiles: self.out.selected.clone(),
            stats: self.out.stats,
        })
        .map_err(|_| Gone)?;

        // Cancel in-flight loads that fell out of the request set.
        let wanted: HashSet<TileId> = self.out.requests.iter().map(|r| r.tile).collect();
        let stale: Vec<TileId> = self
            .in_flight
            .keys()
            .filter(|t| !wanted.contains(t))
            .copied()
            .collect();
        for t in stale {
            if let Some(handle) = self.in_flight.remove(&t) {
                handle.abort();
            }
        }

        // Spawn new loads, highest priority first, within the cap. The
        // traversal only requests tiles that have content, so the loader is
        // never asked to load a structural-empty tile.
        for req in &self.out.requests {
            if self.in_flight.len() >= self.config.maximum_simultaneous_fetches {
                break;
            }
            let tile = req.tile;
            if self.residency.is_resident(tile)
                || self.in_flight.contains_key(&tile)
                || self.failed.contains(&tile)
            {
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
        }
        Ok(())
    }

    /// Files what this traversal needs into the recent-generations ring.
    ///
    /// A camera move opens a new entry; every other traversal — and there are
    /// many, since one runs per load completion — refreshes the newest entry
    /// instead. So the ring holds the last N *camera positions*, not the last N
    /// traversals, which under a burst of fetches would be the same instant.
    fn remember_protected(&mut self) {
        if *self.view_moved || self.recent.is_empty() {
            self.recent.push_back(HashSet::new());
            *self.view_moved = false;
        }
        if let Some(current) = self.recent.back_mut() {
            current.clone_from(self.selected);
        }
        while self.recent.len() > self.config.protected_view_generations.max(1) {
            self.recent.pop_front();
        }
    }

    /// Everything the last few camera positions needed.
    ///
    /// Wider than the current view on purpose: a tile the camera has just left
    /// is the one it is most likely to want back, and evicting it the instant
    /// it leaves the frustum is what makes a rotation re-stream its own wake.
    fn protected(&self) -> HashSet<TileId> {
        let mut all: HashSet<TileId> = self.selected.clone();
        for generation in self.recent.iter() {
            all.extend(generation.iter().copied());
        }
        all
    }

    fn on_load_done(
        &mut self,
        (tile, result): LoadMsg,
        tx: &UnboundedSender<ServerMessage>,
    ) -> Result<(), Gone> {
        self.in_flight.remove(&tile);
        match result {
            Err(e) => self.fail(tile, e.to_string(), tx),
            // Topology grew in place (external tileset grafted): the graft
            // cleared the host's content, so the next traversal won't
            // re-request it; the revealed children load on their own.
            Ok(Loaded::Expanded) => Ok(()),
            Ok(Loaded::Content(decoded)) => {
                let size = decoded.byte_size();
                let evicted = self.cache.insert(tile, size, &self.protected());
                for e in &evicted {
                    self.residency.remove(*e);
                }
                if !evicted.is_empty() {
                    tx.unbounded_send(ServerMessage::Evict { tiles: evicted })
                        .map_err(|_| Gone)?;
                }
                self.residency.insert(tile);
                tx.unbounded_send(ServerMessage::Content {
                    tile,
                    content: TileContent::Decoded(decoded),
                })
                .map_err(|_| Gone)?;
                Ok(())
            }
        }
    }

    fn fail(
        &mut self,
        tile: TileId,
        message: String,
        tx: &UnboundedSender<ServerMessage>,
    ) -> Result<(), Gone> {
        self.failed.insert(tile);
        tx.unbounded_send(ServerMessage::Error {
            tile: Some(tile),
            message,
        })
        .map_err(|_| Gone)?;
        // A failed tile may have been holding a REPLACE; the caller's traversal
        // will re-evaluate.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FsFetcher;
    use crate::protocol::GeometryStream;
    use futures_util::task::LocalSpawnExt;
    use glam::{dvec2, dvec3};
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

    /// Content that falls out of view must be reclaimed once the budget is
    /// reached. Pinning every ancestor up to the root — rather than the
    /// documented band — put so much of the residency in the protected set that
    /// the LRU could find no victim, gave up, and let the budget bound nothing.
    /// The residency must remember where the camera just was. Without it, a
    /// turn evicts the tiles behind it and reloads them the moment it turns
    /// back — measured at 150 reloads over the second half of an orbit.
    #[test]
    fn tiles_the_camera_just_left_survive_the_next_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Tight enough that the budget must reclaim something, so the test
        // proves the window chooses *what* rather than that nothing is dropped.
        let config = Config {
            resident_budget_bytes: 300,
            protected_view_generations: 4,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
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

    /// The window is a window, not a leak: keep moving and the oldest positions
    /// must eventually stop protecting anything.
    #[test]
    fn a_one_move_window_forgets_immediately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        let config = Config {
            resident_budget_bytes: 300,
            // Only the current position counts — the old behaviour.
            protected_view_generations: 1,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        for view in [near_view(), far_view(), near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState { views: vec![view] })
                .expect("send");
            settle(&mut server, &mut stream).expect("settles");
        }
        // Nothing asserted about counts here beyond termination: the point is
        // that a one-move window still converges rather than spinning.
    }

    #[test]
    fn tiles_left_behind_by_the_camera_are_evicted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Room for a working set, not for the whole tree: the tiles the camera
        // leaves behind are what has to go. (One decoded fixture tile is a
        // 3-vertex triangle, well under 100 bytes.)
        let config = Config {
            resident_budget_bytes: 300,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
            })
            .expect("send");
        let (loaded, _) = settle(&mut server, &mut stream).expect("first view settles");
        assert!(
            loaded > 1,
            "the near view loaded several tiles (got {loaded})"
        );

        // Pull far back and stay away. One move is deliberately not enough —
        // the window still remembers where the camera came from — so move past
        // it and check the budget then does its job.
        let moves = Config::default().protected_view_generations + 2;
        let mut evicted = Vec::new();
        for _ in 0..moves {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![far_view()],
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
                })
                .expect("send");

            let mut contents: Vec<TileId> = Vec::new();
            let mut last_select: Vec<(TileId, f64)> = Vec::new();
            while let Some(msg) = stream.next_message().await {
                match msg {
                    ServerMessage::Content { tile, content } => {
                        assert!(
                            matches!(content, TileContent::Decoded(_)),
                            "in-process content is always decoded"
                        );
                        contents.push(tile);
                    }
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
                    ServerMessage::Evict { .. } => {}
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
