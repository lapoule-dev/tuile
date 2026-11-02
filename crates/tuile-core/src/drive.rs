// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Render-agnostic consumer-side driving of a [`GeometryStream`].
//!
//! Every façade (wgpu, USD/Hydra, web, headless snapshot) consumes the same
//! stream, so the bookkeeping of what is drawn and what is held lives here —
//! **not** in any renderer. A renderer only needs to turn the
//! [`ServerMessage::Content`] it routes into GPU resources, acknowledge each
//! with its bytes, and draw the draw set; everything about *what is drawn
//! and what it wears* is the budget's, carried by [`ServerMessage::Select`].
//!
//! - **Progressive** (interactive viewers): feed each [`ServerMessage`] to
//!   [`SceneState::apply`], route `Content` to your renderer, acknowledge,
//!   and draw [`SceneState::draw`] every frame.
//! - **Bulk** (high-quality snapshots): [`drive_until_complete`] holds every
//!   delivery as uploaded, acknowledges it, and pumps the stream until the
//!   frame is complete, then hands back every drawn mesh with its drapes.

use crate::budget::{DrawSet, ResourceId, Role};
use crate::content::{DecodedTileContent, DrapeContent};
use crate::protocol::{ClientMessage, GeometryStream, ResourceContent, ServerMessage, StreamError};
use crate::source::TileId;
use crate::traversal::{TraversalStats, ViewState};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// The consumer's view of the scene, rebuilt from the server's messages.
/// Pure bookkeeping — no content payloads, no GPU — so it is identical across
/// renderers.
#[derive(Debug, Clone, Default)]
pub struct SceneState {
    draw: DrawSet,
    resident: HashSet<ResourceId>,
    stats: TraversalStats,
    started: bool,
    frame_ready: bool,
    converged: bool,
    /// The generation the last `Select` answered.
    ///
    /// A consumer that drives frame after frame on one stream needs this: the
    /// server keeps deciding as resources land, so a `Select` for the
    /// *previous* camera can arrive — and satisfy
    /// [`is_complete`](Self::is_complete) — after a new camera was sent.
    /// Comparing this against what was sent is what makes "the selection
    /// answers the current camera" a fact rather than a hope.
    generation: u64,
}

impl SceneState {
    /// Folds one server message into the scene view.
    pub fn apply(&mut self, msg: &ServerMessage) {
        match msg {
            ServerMessage::Select {
                draw,
                stats,
                frame_ready,
                converged,
                generation,
                ..
            } => {
                self.draw = draw.clone();
                self.stats = *stats;
                self.frame_ready = *frame_ready;
                self.converged = *converged;
                self.started = true;
                self.generation = *generation;
            }
            ServerMessage::Content { resource, .. } => {
                self.resident.insert(*resource);
            }
            ServerMessage::Cancel { resources } => {
                for (r, _) in resources {
                    self.resident.remove(r);
                }
            }
            ServerMessage::Evict { resources } => {
                for r in resources {
                    self.resident.remove(r);
                }
            }
            ServerMessage::Error { .. } | ServerMessage::Priming(_) => {}
        }
    }

    /// Whether the server has reported a complete frame: every mesh and
    /// drape it needs drawable (batch), or converged (interactive).
    pub fn is_complete(&self) -> bool {
        self.started && (self.frame_ready || self.converged)
    }

    /// Whether the last `Select` said the frame is ready: every mesh and
    /// drape it draws held (batch, I13).
    pub fn frame_ready(&self) -> bool {
        self.started && self.frame_ready
    }

    /// What to draw: the last draw set.
    pub fn draw(&self) -> &DrawSet {
        &self.draw
    }

    pub fn is_resident(&self, r: ResourceId) -> bool {
        self.resident.contains(&r)
    }

    pub fn stats(&self) -> TraversalStats {
        self.stats
    }

    /// The generation the current selection answers. See the field.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// A fully-loaded frame: every drawn mesh, decoded, with the drapes it wears.
pub struct BulkFrame {
    /// What the frame draws, sorted by mesh.
    ///
    /// **This is what a consumer iterates**, looking each mesh and drape up in
    /// [`meshes`](Self::meshes) and [`drapes`](Self::drapes) — not the other
    /// way round: a map's order is a per-process accident, and output that
    /// is kept must not depend on it.
    pub draw: DrawSet,
    /// Decoded meshes, by tile.
    pub meshes: HashMap<TileId, Arc<DecodedTileContent>>,
    /// Decoded drapes, by resource.
    pub drapes: HashMap<ResourceId, Arc<DrapeContent>>,
    pub stats: TraversalStats,
    /// Load failures encountered while converging. A failure the budget
    /// retried and got past leaves no hole, but anything keeping its output
    /// should treat a non-empty `errors` as worth a look.
    pub errors: Vec<(Option<ResourceId>, String)>,
}

impl BulkFrame {
    /// Each drawn mesh, dressed: its content with the drapes it wears placed
    /// in its own uv square (an ancestor's through its uv transform), as
    /// [`DecodedTileContent::imagery`] — the composed form an offline
    /// renderer or a pack consumes. In draw-set order; exact and held items
    /// only (a fallback ancestor stands in for ground that is not its own).
    pub fn dressed(&self) -> Vec<(TileId, DecodedTileContent)> {
        let mut out = Vec::new();
        for item in &self.draw.items {
            if item.role == Role::Fallback {
                continue;
            }
            let tile = item.mesh.tile();
            let Some(mesh) = self.meshes.get(&tile) else {
                continue;
            };
            out.push((tile, dress(mesh, item, &self.drapes)));
        }
        out
    }
}

/// A mesh's content wearing a draw item's drapes, as layers in its own uv
/// square, above any imagery the mesh carries itself (a loader that drapes
/// its meshes). Shared by every consumer that composes on the CPU.
///
/// A worn drape that was **withheld** — its tiles never fetched, because
/// the consumer said it holds the composed drape — names itself in
/// [`DecodedTileContent::withheld_drape`]: the consumer must resolve it.
/// Only a tile's own drape (identity uv) can be named so; the composed
/// drape a consumer holds is a whole tile's.
pub fn dress(
    mesh: &DecodedTileContent,
    item: &crate::budget::DrawItem,
    drapes: &HashMap<ResourceId, Arc<DrapeContent>>,
) -> DecodedTileContent {
    let mut content = mesh.clone();
    for choice in &item.drapes {
        let Some(drape) = drapes.get(&choice.resource) else {
            continue;
        };
        if drape.withheld && choice.uv == crate::budget::UvTransform::IDENTITY {
            content.withheld_drape = Some(drape.identity);
        }
        content
            .imagery
            .extend(drape.tiles.iter().filter_map(|l| l.through(choice.uv)));
    }
    content
}

/// Bulk policy: send `views` once, then pump the stream until the frame is
/// complete, acknowledging every delivery as held, and return every drawn
/// mesh and drape together. Renderer-independent — a snapshot tool uploads
/// them in one batch; the WebSocket and static-HTTP bindings drive the very
/// same loop.
///
/// Returns [`StreamError::Closed`] if the server drops first.
pub async fn drive_until_complete<S: GeometryStream + Unpin>(
    stream: &mut S,
    views: Vec<ViewState>,
) -> Result<BulkFrame, StreamError> {
    stream.send(ClientMessage::ViewerState {
        views,
        generation: 1,
    })?;
    let mut state = SceneState::default();
    let mut meshes: HashMap<TileId, Arc<DecodedTileContent>> = HashMap::new();
    let mut drapes: HashMap<ResourceId, Arc<DrapeContent>> = HashMap::new();
    let mut errors: Vec<(Option<ResourceId>, String)> = Vec::new();
    // The generation each held resource came with: what a cancel names.
    let mut held: BTreeMap<ResourceId, crate::budget::Generation> = BTreeMap::new();

    while let Some(msg) = stream.next_message().await {
        state.apply(&msg);
        match msg {
            ServerMessage::Content {
                resource,
                gen,
                content,
                ..
            } => {
                // Held as uploaded: acknowledged with the estimator's bytes,
                // which is what a consumer that keeps the decoded content
                // holds.
                let bytes = match &content {
                    ResourceContent::Mesh(m) => m.cost().gpu,
                    ResourceContent::Drape(d) => d.cost().gpu,
                };
                match content {
                    ResourceContent::Mesh(m) => {
                        meshes.insert(resource.tile(), m);
                    }
                    ResourceContent::Drape(d) => {
                        drapes.insert(resource, d);
                    }
                }
                held.insert(resource, gen);
                stream.send(ClientMessage::Ack {
                    resource,
                    gen,
                    bytes: bytes.0,
                })?;
            }
            ServerMessage::Cancel { resources } => {
                for (r, g) in resources {
                    if held.get(&r) == Some(&g) {
                        held.remove(&r);
                        forget(&mut meshes, &mut drapes, r);
                    }
                }
            }
            ServerMessage::Evict { resources } => {
                for r in resources {
                    held.remove(&r);
                    forget(&mut meshes, &mut drapes, r);
                }
            }
            ServerMessage::Error { resource, message } => errors.push((resource, message)),
            ServerMessage::Select { .. } | ServerMessage::Priming(_) => {}
        }
        // Complete for the views sent (generation 1): the session's first
        // step answers no view — it lists the floor before any camera —
        // and a scene with no floor (a tileset alone) converges on it, on
        // nothing.
        if state.is_complete() && state.generation() >= 1 {
            return Ok(BulkFrame {
                draw: state.draw().clone(),
                meshes,
                drapes,
                stats: state.stats(),
                errors,
            });
        }
    }
    Err(StreamError::Closed)
}

fn forget(
    meshes: &mut HashMap<TileId, Arc<DecodedTileContent>>,
    drapes: &mut HashMap<ResourceId, Arc<DrapeContent>>,
    r: ResourceId,
) {
    match r {
        ResourceId::Mesh(t) => {
            meshes.remove(&t);
        }
        ResourceId::Drape { .. } => {
            drapes.remove(&r);
        }
    }
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

    fn bare_content() -> DecodedTileContent {
        DecodedTileContent {
            meshes: Vec::new(),
            textures: Vec::new(),
            imagery: Vec::new(),
            withheld_drape: None,
            copyright: None,
            local_origin_ecef: glam::DVec3::ZERO,
            transform_local: glam::Mat4::IDENTITY,
        }
    }

    fn imagery_tile(x: u64) -> crate::raster::ImageryLayer {
        let rect = crate::raster::GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        crate::raster::ImageryLayer::placed(
            crate::raster::ImageryCoord { level: 3, x, y: 0 },
            Arc::new(crate::content::DecodedTexture::from_rgba8(
                1,
                1,
                vec![0, 0, 0, 255],
            )),
            &rect,
            &rect,
        )
    }

    fn worn(
        tile: TileId,
        drape: &DrapeContent,
    ) -> (
        crate::budget::DrawItem,
        HashMap<ResourceId, Arc<DrapeContent>>,
    ) {
        let resource = ResourceId::drape(tile, drape.layer, drape.level);
        let item = crate::budget::DrawItem {
            mesh: ResourceId::Mesh(tile),
            role: Role::Exact,
            drapes: [crate::budget::DrapeChoice {
                resource,
                layer: drape.layer,
                level: drape.level,
                uv: crate::budget::UvTransform::IDENTITY,
            }]
            .into_iter()
            .collect(),
            serves: [tile].into_iter().collect(),
        };
        (item, HashMap::from([(resource, Arc::new(drape.clone()))]))
    }

    /// **Dressing adds the drapes a mesh wears to what it carries**: a
    /// loader that drapes its meshes itself keeps its imagery, under the
    /// worn drapes — clearing it left such a mesh bare.
    #[test]
    fn dressing_keeps_the_imagery_a_mesh_carries() {
        let tile = TileId(7);
        let mut mesh = bare_content();
        mesh.imagery.push(imagery_tile(0));
        let drape = DrapeContent {
            layer: crate::budget::LayerId(0),
            level: 3,
            tiles: vec![imagery_tile(1)],
            floor_tiles: 0,
            identity: 11,
            withheld: false,
        };
        let (item, drapes) = worn(tile, &drape);
        let dressed = dress(&mesh, &item, &drapes);
        let coords: Vec<u64> = dressed.imagery.iter().map(|l| l.coord.x).collect();
        assert_eq!(coords, vec![0, 1]);
        assert_eq!(dressed.withheld_drape, None);
    }

    /// **A worn drape that was withheld names itself on the dressed mesh**:
    /// its tiles were never fetched because the consumer holds the composed
    /// drape, and a dressed mesh with no imagery and no name would be bare
    /// ground nobody is told about.
    #[test]
    fn a_withheld_drape_names_itself_on_the_dressed_mesh() {
        let tile = TileId(7);
        let drape = DrapeContent {
            layer: crate::budget::LayerId(0),
            level: 3,
            tiles: Vec::new(),
            floor_tiles: 0,
            identity: 0xfeed,
            withheld: true,
        };
        let (item, drapes) = worn(tile, &drape);
        let dressed = dress(&bare_content(), &item, &drapes);
        assert!(dressed.imagery.is_empty());
        assert_eq!(dressed.withheld_drape, Some(0xfeed));
    }

    #[test]
    fn bulk_drive_returns_all_selected_tiles_decoded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let leaves: HashSet<TileId> = tileset.tile(root).children.iter().copied().collect();

        let (mut stream, server) = in_process(
            tileset,
            Arc::new(FsFetcher),
            Config::default(),
            crate::budget::BudgetConfig::interactive(crate::budget::Bytes::mib(256)),
        )
        .expect("a valid budget");
        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let frame = pool
            .run_until(drive_until_complete(&mut stream, vec![near_view()]))
            .expect("bulk frame");

        // The converged frame draws exactly the two leaves (REPLACE displaced
        // the root), each as itself, and every one arrived decoded.
        let drawn: HashSet<TileId> = frame
            .draw
            .items
            .iter()
            .filter(|i| i.role == Role::Exact)
            .map(|i| i.mesh.tile())
            .collect();
        assert_eq!(drawn, leaves);
        assert!(!drawn.contains(&root), "REPLACE: leaves displaced root");
        for tile in &drawn {
            assert!(
                frame.meshes.contains_key(tile),
                "drawn tile {tile:?} must be decoded and held"
            );
        }
        assert!(frame.errors.is_empty());
    }
}
