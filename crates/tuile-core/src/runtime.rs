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
            frame: &mut self.frame,
        };

        enum Event {
            Load(Result<LoadMsg, Aborted>),
            Client(ClientMessage),
            Closed,
        }

        loop {
            // Load completions first (biased), then client messages.
            let event = poll_fn(|cx| {
                if let Poll::Ready(Some(done)) = Pin::new(&mut loads).poll_next(cx) {
                    return Poll::Ready(Event::Load(done));
                }
                match Pin::new(&mut rx).poll_next(cx) {
                    Poll::Ready(Some(msg)) => Poll::Ready(Event::Client(msg)),
                    Poll::Ready(None) => Poll::Ready(Event::Closed),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await;

            match event {
                Event::Load(Ok(msg)) => {
                    if session.on_load_done(msg, &tx, &mut loads).is_err() {
                        break; // consumer dropped
                    }
                }
                // Cancelled load: cleaned up at abort time.
                Event::Load(Err(Aborted)) => {}
                Event::Client(first) => {
                    let mut dirty = session.on_client(first);
                    // Coalesce bursts: drain whatever is already queued so a
                    // stream of ViewerStates yields one traversal.
                    while let Ok(more) = rx.try_recv() {
                        dirty |= session.on_client(more);
                    }
                    if dirty && session.retraverse(&tx, &mut loads).is_err() {
                        break;
                    }
                }
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

    fn on_load_done(
        &mut self,
        (tile, result): LoadMsg,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut FuturesUnordered<LoadFuture>,
    ) -> Result<(), Gone> {
        self.in_flight.remove(&tile);
        match result {
            Err(e) => self.fail(tile, e.to_string(), tx, loads),
            // Topology grew in place (external tileset grafted): the graft
            // cleared the host's content, so the next traversal won't
            // re-request it; the revealed children load on their own.
            Ok(Loaded::Expanded) => self.retraverse(tx, loads),
            Ok(Loaded::Content(decoded)) => {
                let size = decoded.byte_size();
                let evicted = self.cache.insert(tile, size, self.selected);
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
                self.retraverse(tx, loads)
            }
        }
    }

    fn fail(
        &mut self,
        tile: TileId,
        message: String,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut FuturesUnordered<LoadFuture>,
    ) -> Result<(), Gone> {
        self.failed.insert(tile);
        tx.unbounded_send(ServerMessage::Error {
            tile: Some(tile),
            message,
        })
        .map_err(|_| Gone)?;
        // A failed tile may have been holding a REPLACE: re-evaluate.
        self.retraverse(tx, loads)
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

    fn near_view() -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, 120.0),
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
