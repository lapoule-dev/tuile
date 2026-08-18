// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Render-agnostic consumer-side driving of a [`GeometryStream`].
//!
//! Every façade (wgpu, USD/Hydra, web, headless snapshot) consumes the same
//! stream, so the selection/residency bookkeeping and the two driving policies
//! live here — **not** in any renderer. A renderer only needs to turn the
//! [`ServerMessage::Content`] it routes into GPU resources; everything about
//! *what is selected and ready* is [`SceneState`].
//!
//! - **Progressive** (interactive viewers): feed each [`ServerMessage`] to
//!   [`SceneState::apply`], route `Content` to your renderer, and draw
//!   [`SceneState::renderable`] every frame — tiles stream in one by one.
//! - **Bulk** (high-quality snapshots): [`drive_until_complete`] pumps the
//!   stream until the selection is stable and fully resident, then hands back
//!   every decoded tile at once.

use crate::content::TileContent;
use crate::protocol::{ClientMessage, GeometryStream, ServerMessage, StreamError};
use crate::source::TileId;
use crate::traversal::{TraversalStats, ViewState};
use std::collections::{HashMap, HashSet};

/// The consumer's view of the scene, rebuilt from the server's messages.
/// Pure bookkeeping — no content payloads, no GPU — so it is identical across
/// renderers.
#[derive(Debug, Clone, Default)]
pub struct SceneState {
    selected: Vec<(TileId, f64)>,
    resident: HashSet<TileId>,
    stats: TraversalStats,
    /// Outstanding loads the server still intends to fulfil (its `Select`'s
    /// request count, already net of tiles it gave up on).
    pending: u32,
    started: bool,
}

impl SceneState {
    /// Folds one server message into the scene view.
    pub fn apply(&mut self, msg: &ServerMessage) {
        match msg {
            ServerMessage::Select { tiles, stats, .. } => {
                self.selected = tiles.clone();
                self.stats = *stats;
                self.pending = stats.requested;
                self.started = true;
            }
            // A stand-in is not residency: the tile it covers for has still
            // not arrived, and counting it here would make a scene look
            // complete while it is being held together by approximations.
            ServerMessage::Fill { .. } => {}
            ServerMessage::Content { tile, .. } => {
                self.resident.insert(*tile);
            }
            ServerMessage::Evict { tiles } => {
                for t in tiles {
                    self.resident.remove(t);
                }
            }
            ServerMessage::Error { tile, .. } => {
                if let Some(t) = tile {
                    self.resident.remove(t);
                }
            }
            // Startup accounting for a host that holds a first frame back. A
            // bulk driver holds nothing back, so it has nothing to do with it.
            ServerMessage::Priming(_) => {}
        }
    }

    /// True once the server has reported a stable selection with no pending
    /// loads — the steady state a bulk render waits for.
    pub fn is_complete(&self) -> bool {
        self.started && self.pending == 0
    }

    /// The current selection (tile + its SSE), regardless of residency.
    pub fn selected(&self) -> &[(TileId, f64)] {
        &self.selected
    }

    pub fn is_resident(&self, tile: TileId) -> bool {
        self.resident.contains(&tile)
    }

    pub fn stats(&self) -> TraversalStats {
        self.stats
    }

    /// Tiles to draw *this frame*: the selection intersected with what has
    /// actually arrived. During progressive loading this grows as content
    /// streams in; at steady state it equals the full selection.
    pub fn renderable(&self) -> impl Iterator<Item = (TileId, f64)> + '_ {
        self.selected
            .iter()
            .copied()
            .filter(move |(t, _)| self.resident.contains(t))
    }
}

/// A fully-loaded frame: every selected tile, decoded and resident.
pub struct BulkFrame {
    /// The stable selection (tile + SSE), in traversal order.
    ///
    /// **This is what a consumer iterates**, looking each tile up in
    /// [`contents`](Self::contents) — not the other way round. See that field.
    pub selected: Vec<(TileId, f64)>,
    /// Decoded content for every resident selected tile.
    ///
    /// A lookup table, not a sequence. `HashMap` iteration order depends on a
    /// per-process random seed, so anything that walks this map emits its
    /// results in a different order on every run. That is invisible in a viewer
    /// and corrosive anywhere output is kept: two renders of the same frame on
    /// two machines differ by prim ordering alone, and a farm comparing them
    /// reports a change that is not there. Iterate
    /// [`selected`](Self::selected), which is ordered.
    pub contents: HashMap<TileId, TileContent>,
    pub stats: TraversalStats,
    /// Load failures encountered while converging. Non-fatal *to the frame* —
    /// it still returns — but not harmless: a failed tile leaves no hole,
    /// because its ancestor stands in, so the frame renders plausibly at the
    /// wrong level of detail. Anything keeping its output should treat a
    /// non-empty `errors` as a failed frame rather than a warning.
    pub errors: Vec<(Option<TileId>, String)>,
}

/// Bulk policy: send `views` once, then pump the stream until the selection is
/// stable and fully resident, and return all decoded tiles together. Renderer-
/// independent — a snapshot tool uploads `contents` in one batch; the WebSocket
/// and static-HTTP bindings drive the very same loop.
///
/// Terminates because the server stops counting tiles it has given up on (see
/// `runtime::Session::retraverse`), so `pending` reaches zero even when some
/// tiles fail. Returns [`StreamError::Closed`] if the server drops first.
pub async fn drive_until_complete<S: GeometryStream + Unpin>(
    stream: &mut S,
    views: Vec<ViewState>,
) -> Result<BulkFrame, StreamError> {
    stream.send(ClientMessage::ViewerState { views })?;
    let mut state = SceneState::default();
    let mut contents: HashMap<TileId, TileContent> = HashMap::new();
    let mut errors: Vec<(Option<TileId>, String)> = Vec::new();

    while let Some(msg) = stream.next_message().await {
        state.apply(&msg);
        match msg {
            ServerMessage::Content { tile, content, .. } => {
                contents.insert(tile, content);
            }
            // This driver collects finished content for inspection; a stand-in
            // is by definition unfinished and would only dilute the answer.
            ServerMessage::Fill { .. } => {}
            ServerMessage::Evict { tiles } => {
                for t in tiles {
                    contents.remove(&t);
                }
            }
            ServerMessage::Error { tile, message } => errors.push((tile, message)),
            ServerMessage::Select { .. } | ServerMessage::Priming(_) => {}
        }
        if state.is_complete() {
            // Keep only what is still selected & resident.
            contents.retain(|t, _| state.is_resident(*t));
            return Ok(BulkFrame {
                selected: state.selected().to_vec(),
                contents,
                stats: state.stats(),
                errors,
            });
        }
    }
    Err(StreamError::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FsFetcher;
    use crate::runtime::in_process;
    use crate::tileset::Tileset;
    use crate::traversal::Config;
    use futures_util::task::LocalSpawnExt;
    use glam::{dvec2, dvec3};
    use std::sync::Arc;
    use url::Url;

    fn write_fixture(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        for name in ["root", "a", "b"] {
            std::fs::write(dir.join(format!("{name}.glb")), &glb).expect("write");
        }
        let tileset = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 50, "refine": "REPLACE",
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
    fn bulk_drive_returns_all_selected_tiles_decoded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let leaves: HashSet<TileId> = tileset.tile(root).children.iter().copied().collect();

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());
        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let frame = pool
            .run_until(drive_until_complete(&mut stream, vec![near_view()]))
            .expect("bulk frame");

        // The stable selection is exactly the two leaves (REPLACE displaced
        // the root), and every selected tile arrived decoded.
        let selected: HashSet<TileId> = frame.selected.iter().map(|(t, _)| *t).collect();
        assert_eq!(selected, leaves);
        assert!(!selected.contains(&root), "REPLACE: leaves displaced root");
        for tile in &selected {
            assert!(
                matches!(frame.contents.get(tile), Some(TileContent::Decoded(_))),
                "selected tile {tile:?} must be decoded and resident"
            );
        }
        assert!(frame.errors.is_empty());
    }
}
