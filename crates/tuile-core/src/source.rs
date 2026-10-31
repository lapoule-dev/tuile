// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile-source seam: the abstraction the SSE traversal and the loader
//! consume, so the same code drives 3D Tiles, quantized-mesh terrain, and
//! **compositions** of both.
//!
//! [`TileId`] is an opaque `u64` handle, laid out as:
//! - bits 60..64 — a **source tag** (0..16), used by [`CompositeTileTree`]
//!   to route to a sub-source (0 for a standalone source);
//! - bits 0..60 — the source-private payload (arena index for a
//!   [`crate::tileset::Tileset`], encoded `(level, x, y)` for a terrain
//!   quadtree).
//!
//! Two traits split the concern: [`TileTree`] is pure/synchronous (the
//! traversal walks it every frame), [`TileLoader`] is async (the runtime
//! fetches + decodes + textures a tile). A source implements `TileTree`; its
//! loader implements `TileLoader`. [`CompositeTileTree`]/[`CompositeLoader`]
//! combine several of each by tag — so a terrain (draped with imagery) and a
//! 3D Tiles building set (with its own glb textures) traverse and load
//! together, each tile carrying the textures that correspond to its source
//! — a terrain tile's imagery as drapes of its own ([`TileLoader::load_drape`]).

use crate::content::DecodedTileContent;
use crate::math::BoundingVolume;
use crate::tileset::Refine;
use async_trait::async_trait;

pub use crate::budget::tile_id::{TileId, MAX_SOURCES};

/// Topological properties of a tile — everything the traversal needs to make
/// a refine/render/cull decision, without loading the tile.
#[derive(Debug, Clone, Copy)]
pub struct TileProperties {
    /// Bounding volume in world (ECEF) coordinates.
    pub bounding_volume: BoundingVolume,
    /// Geometric error in meters.
    pub geometric_error: f64,
    pub refine: Refine,
    /// Whether the tile has renderable content (vs a structural empty tile).
    pub has_content: bool,
}

/// The tile hierarchy as the traversal sees it: pure, synchronous, walked
/// every frame. Implemented by [`crate::tileset::Tileset`] (3D Tiles), by
/// terrain quadtrees (`tuile-terrain`), and by [`CompositeTileTree`].
///
/// `Send + Sync` so the [`crate::runtime::GeometryServer`] can move a boxed
/// tree into the session future and run it on any executor — including
/// multi-threaded ones (`tokio::spawn`), where the future is held across
/// threads and the traversal borrows the tree by shared reference.
pub trait TileTree: Send + Sync {
    /// Root tiles (one for a 3D Tiles tileset, several for a global terrain
    /// or a composition).
    fn roots(&self) -> Vec<TileId>;

    /// Children of a tile (already filtered by availability for terrain).
    fn children(&self, id: TileId) -> Vec<TileId>;

    /// Topological properties of a tile.
    fn properties(&self, id: TileId) -> TileProperties;

    /// Parent of a tile, or `None` at a root. Lets the server keep the path
    /// from the rendered frontier up to the root resident, so a coarser
    /// ancestor is always available as a fallback while finer tiles stream in
    /// (no holes). Default `None` — sources without cheap parent lookup (a
    /// 3D Tiles arena) simply don't pin ancestors.
    /// The tile one level up, or `None` at a root.
    ///
    /// Defaults to `None`, which is honest but costly: the server pins the
    /// ancestor chain of everything it selects (so a coarser tile is always
    /// available to stand in while a finer one streams), and a consumer walks
    /// the same chain to fill gaps. A tree that does not answer this gets
    /// neither — no fallback, and holes where a coarse tile would have done.
    /// Implement it wherever the topology allows.
    fn parent(&self, _id: TileId) -> Option<TileId> {
        None
    }

    /// A sphere that hides what is behind it, if this dataset has one.
    ///
    /// `None` for a tileset that is just a building or a mesh: nothing occludes
    /// anything, and the frustum is the whole of visibility. A global terrain
    /// answers with its planet, and that changes the arithmetic completely — a
    /// camera near the ground has a frustum that runs through the planet and
    /// selects the far side of it, which no amount of frustum culling can see.
    ///
    /// It lives on the tree rather than in the consumer's [`crate::traversal::Config`]
    /// because it is a fact about the data, not a preference: a consumer that
    /// forgot to set it would quietly pay for ground it can never see, which is
    /// exactly what happened while it did not exist.
    fn occluder(&self) -> Option<crate::math::Occluder> {
        None
    }

    /// From now on, hold every change a load makes to this tree's shape until
    /// [`apply_deferred_edits`](TileTree::apply_deferred_edits) is called.
    ///
    /// A load can grow the tree: a terrain tile reveals which of its
    /// descendants exist, an external tileset is grafted under its host. While
    /// the loads are driven by the same future as the traversal those changes
    /// land between two passes by construction. Once loads run as tasks of
    /// their own — see [`crate::runtime::Spawn`] — they land whenever the
    /// task does, which can be halfway through a pass: one subtree walked
    /// before a reveal and its sibling after it, a selection that belongs to
    /// no state the tree was ever in. The server calls this when it starts
    /// spawning, and applies what was held at the top of every pass.
    ///
    /// Default: nothing, for a tree no load ever changes. A tree whose loader
    /// does change it and that does not implement this keeps the race.
    fn defer_edits(&self) {}

    /// Applies every edit held since the last call, in an order that does not
    /// depend on which load finished first. Returns the tiles whose edit
    /// could not be applied, with why — the session ends on them as it does
    /// on a failed load.
    fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
        Vec::new()
    }

    /// A number that changes whenever any answer of
    /// [`children`](TileTree::children) or [`properties`](TileTree::properties)
    /// could, or `None` when the tree cannot say.
    ///
    /// The contract is one way: two calls that return the same `Some` value
    /// promise that every `children` and `properties` answer in between was
    /// the same. It does not cover [`roots`](TileTree::roots), which callers
    /// compare themselves. It never goes down, which is what lets a
    /// composite add its sources' up. A tree that only grows may count its
    /// edits; a tree whose shape never changes may answer a constant.
    ///
    /// What it buys: work that depends on the tree's shape alone can be kept
    /// from one pass to the next — see [`crate::traversal::NearestContent`].
    /// Default `None`, which makes that work run again on every pass: always
    /// right, only slower. Answer `Some` only when it is true.
    fn revision(&self) -> Option<u64> {
        None
    }

    /// How deep this tile sits, roots at 0.
    ///
    /// **Only the tree can answer this, and that is the whole point.** A
    /// [`TileId`] is an opaque handle — the doc on the type says so — whose
    /// payload the owning tree interprets: a packed `(level, x, y)` for terrain,
    /// a flat arena index for a 3D Tiles tileset. Reading a level out of the
    /// bits works for one of those and silently answers 0 for the other, which
    /// is what made a pinned floor pin every tile in a tileset and the residency
    /// grow without bound.
    ///
    /// The default walks [`TileTree::parent`], so any tree that answers parent
    /// answers this. Override it wherever the level is cheaper than the walk —
    /// terrain has it in the id, an arena node stores it.
    fn level(&self, id: TileId) -> u32 {
        let mut level = 0;
        let mut cur = self.parent(id);
        while let Some(p) = cur {
            level += 1;
            cur = self.parent(p);
        }
        level
    }

    /// The budget class a tile's mesh is charged to: a terrain mesh unless
    /// the tree says otherwise (a 3D Tiles tileset's content is a
    /// [`Class::Tiles3dMesh`](crate::budget::Class::Tiles3dMesh)), so that
    /// one kind of content cannot starve the other.
    fn mesh_class(&self, _id: TileId) -> crate::budget::Class {
        crate::budget::Class::TerrainMesh
    }
}

/// Outcome of a [`TileLoader::load`].
pub enum Loaded {
    /// Render-ready content: the server caches it and streams it to the
    /// consumer (terrain → draped imagery, glb → intrinsic textures).
    Content(DecodedTileContent),
    /// The load expanded the tile tree in place — e.g. an external 3D Tiles
    /// tileset whose root was grafted into the shared arena. Nothing to
    /// render: the server re-traverses and the newly revealed children load
    /// on their own. Mirrors Cesium's `TileExternalContent` junction node.
    Expanded,
}

/// Loads + decodes + textures a tile into render-ready content. Async; the
/// runtime drives it off the frame. Each source's loader is responsible for
/// the textures that correspond to its tiles (terrain → draped imagery, glb
/// → intrinsic textures).
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait TileLoader: Send + Sync {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError>;

    /// The imagery that drapes `id`, or `None` for content that carries its
    /// own textures (a glb's). **Synchronous, and never any I/O**: the
    /// session asks it of every tile it lists, every step, to tell the budget
    /// which drapes each tile needs (the need's imagery, chosen per tile and
    /// independently of the terrain level, I10).
    fn drapes(&self, _id: TileId) -> Option<DrapeReach> {
        None
    }

    /// Composes `id`'s drape of `layer` at the budget's imagery `level`: the
    /// drape is a resource of its own, loaded beside the tile's mesh, at the
    /// level the budget asks for (D-A2).
    async fn load_drape(
        &self,
        id: TileId,
        layer: crate::budget::LayerId,
        level: u32,
    ) -> Result<crate::content::DrapeContent, LoadError> {
        let _ = (id, layer, level);
        Err(LoadError::Failed("this source drapes nothing".into()))
    }

    /// Whether a local store holds what loading `r` would read, asked of
    /// its index without reading the payload (spec §4b): the budget's
    /// presence oracle. A hint, never a promise. **Synchronous, and never
    /// any I/O.** Default: nothing is held locally.
    fn holds(&self, _r: crate::budget::ResourceId) -> crate::budget::Presence {
        crate::budget::Presence::Absent
    }

    /// Fetches everything down to `through_level` into whatever store sits
    /// behind this loader, before anything asks for it.
    ///
    /// The coarse levels are what every fallback lands on: a tile that has not
    /// arrived is drawn by its nearest resident ancestor, and if that walk
    /// reaches the top and finds nothing, the ground is bare. Waiting for the
    /// camera to happen upon them means the safety net is built out of exactly
    /// the tiles a fast movement has not fetched yet — measured on a session
    /// that had flown the same ground repeatedly, level 5 held 291 of its 1024
    /// tiles and level 3 held none at all.
    ///
    /// The whole planet at level 5 is `4^5` tiles, a few tens of megabytes once
    /// and cached on disk thereafter. Every level below that is a quarter of
    /// the one above, so the sum is a third again — cheap for a floor that
    /// never moves.
    ///
    /// Default: nothing. A source with no store, or one that is already local,
    /// has nothing to gain.
    async fn warm_up(&self, _through_level: u32) {}

    /// Which version of its content this loader is producing: a number that
    /// goes up when what [`load`](TileLoader::load) returns for the *same*
    /// tile changes — a globe told to drape other imagery, say.
    ///
    /// The server remembers the epoch each resident tile was loaded under.
    /// When this number moves on, every such tile is out of date, and the
    /// server **refreshes** it: loads it again, coarse tiles first, and sends
    /// the new content under the same id. The old content stays resident — and
    /// stays with the consumer — until the new one has arrived, so the change
    /// crosses the screen tile by tile and never through a frame with nothing
    /// to draw. See `Session::refresh` in the runtime.
    ///
    /// Zero, for ever, for a loader whose content does not change.
    fn epoch(&self) -> u64 {
        0
    }
}

/// What drapes a tile: the ground size of one imagery texel at level 0, which
/// a view turns into the tile's texel footprint, and each layer that drapes
/// it with how deep it reaches there.
#[derive(Debug, Clone, PartialEq)]
pub struct DrapeReach {
    /// Metres of ground under one texel of a level-0 imagery tile of 256
    /// texels: level `L` halves it `L` times.
    pub texel_l0_m: f64,
    /// The tile's footprint in longitude and latitude: what places one
    /// tile's drape over another's ground.
    pub rect: crate::budget::GeoRect,
    /// The layers that drape the tile, in stacking order.
    pub layers: Vec<crate::budget::LayerReach>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("tile load failed: {0}")]
    Failed(String),
    #[error("no sub-source for tag {0}")]
    BadTag(u8),
}

/// Combines several [`TileTree`]s into one, tagging each sub-source's tiles.
/// The traversal sees a single tree whose roots are the union of all sources.
pub struct CompositeTileTree {
    sources: Vec<Box<dyn TileTree>>,
}

impl CompositeTileTree {
    /// Panics if more than [`MAX_SOURCES`] are given.
    pub fn new(sources: Vec<Box<dyn TileTree>>) -> Self {
        assert!(
            sources.len() <= MAX_SOURCES,
            "at most {MAX_SOURCES} sources"
        );
        Self { sources }
    }
}

impl TileTree for CompositeTileTree {
    /// The occluder the sources agree on, or none.
    ///
    /// Unanimity is the only safe rule: culling against a planet that only one
    /// source lives on would hide the other's ground. Sources that agree —
    /// terrain and imagery over the same globe, the usual case — keep it.
    fn occluder(&self) -> Option<crate::math::Occluder> {
        let mut sources = self.sources.iter();
        let first = sources.next()?.occluder()?;
        sources
            .all(|s| s.occluder() == Some(first))
            .then_some(first)
    }

    fn defer_edits(&self) {
        for source in &self.sources {
            source.defer_edits();
        }
    }

    /// The sum of the sources' revisions, when every source has one.
    ///
    /// A sum, because revisions only move forward: any source's moving moves
    /// it. One source that cannot say makes the whole composite unable to.
    fn revision(&self) -> Option<u64> {
        self.sources
            .iter()
            .try_fold(0u64, |sum, s| Some(sum.wrapping_add(s.revision()?)))
    }

    fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
        self.sources
            .iter()
            .enumerate()
            .flat_map(|(i, s)| {
                let tag = i as u8;
                s.apply_deferred_edits()
                    .into_iter()
                    .map(move |(id, why)| (id.with_tag(tag), why))
            })
            .collect()
    }

    fn roots(&self) -> Vec<TileId> {
        self.sources
            .iter()
            .enumerate()
            .flat_map(|(i, s)| {
                let tag = i as u8;
                s.roots().into_iter().map(move |id| id.with_tag(tag))
            })
            .collect()
    }

    fn children(&self, id: TileId) -> Vec<TileId> {
        let tag = id.tag();
        match self.sources.get(tag as usize) {
            Some(s) => s
                .children(id.payload())
                .into_iter()
                .map(|c| c.with_tag(tag))
                .collect(),
            None => Vec::new(),
        }
    }

    fn properties(&self, id: TileId) -> TileProperties {
        // A missing tag should never reach here (handles come from roots()).
        self.sources[id.tag() as usize].properties(id.payload())
    }

    fn parent(&self, id: TileId) -> Option<TileId> {
        let tag = id.tag();
        self.sources
            .get(tag as usize)
            .and_then(|s| s.parent(id.payload()))
            .map(|p| p.with_tag(tag))
    }

    /// Forwarded rather than defaulted: the sub-source may know its level
    /// without a walk, and the default here would walk through `with_tag`
    /// round trips for nothing.
    fn level(&self, id: TileId) -> u32 {
        self.sources
            .get(id.tag() as usize)
            .map_or(0, |s| s.level(id.payload()))
    }

    fn mesh_class(&self, id: TileId) -> crate::budget::Class {
        self.sources
            .get(id.tag() as usize)
            .map_or(crate::budget::Class::TerrainMesh, |s| {
                s.mesh_class(id.payload())
            })
    }
}

/// Routes [`TileLoader::load`] to the sub-loader matching the tile's tag, so
/// each source textures its own tiles. Pair with a [`CompositeTileTree`]
/// built from the same sources in the same order.
pub struct CompositeLoader {
    loaders: Vec<Box<dyn TileLoader>>,
}

impl CompositeLoader {
    pub fn new(loaders: Vec<Box<dyn TileLoader>>) -> Self {
        assert!(
            loaders.len() <= MAX_SOURCES,
            "at most {MAX_SOURCES} loaders"
        );
        Self { loaders }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TileLoader for CompositeLoader {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let tag = id.tag();
        match self.loaders.get(tag as usize) {
            Some(l) => l.load(id.payload()).await,
            None => Err(LoadError::BadTag(tag)),
        }
    }

    fn drapes(&self, id: TileId) -> Option<DrapeReach> {
        self.loaders.get(id.tag() as usize)?.drapes(id.payload())
    }

    async fn load_drape(
        &self,
        id: TileId,
        layer: crate::budget::LayerId,
        level: u32,
    ) -> Result<crate::content::DrapeContent, LoadError> {
        let tag = id.tag();
        match self.loaders.get(tag as usize) {
            Some(l) => l.load_drape(id.payload(), layer, level).await,
            None => Err(LoadError::BadTag(tag)),
        }
    }

    fn holds(&self, r: crate::budget::ResourceId) -> crate::budget::Presence {
        use crate::budget::ResourceId;
        let tile = r.tile();
        let local = match r {
            ResourceId::Mesh(_) => ResourceId::Mesh(tile.payload()),
            ResourceId::Drape { layer, level, .. } => {
                ResourceId::drape(tile.payload(), layer, level)
            }
        };
        self.loaders
            .get(tile.tag() as usize)
            .map_or(crate::budget::Presence::Absent, |l| l.holds(local))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Sphere;
    use glam::{DVec3, Mat4};

    #[test]
    fn terrain_handle_round_trips() {
        for (l, x, y) in [
            (0u32, 0u64, 0u64),
            (19, 541, 386),
            (12, 1083, 773),
            (31, 7, 9),
        ] {
            let id = TileId::from_terrain(l, x, y);
            assert_eq!(id.terrain_coord(), (l, x, y));
        }
    }

    #[test]
    fn tag_round_trips_and_preserves_payload() {
        let id = TileId::from_terrain(12, 1083, 773);
        let tagged = id.with_tag(5);
        assert_eq!(tagged.tag(), 5);
        assert_eq!(tagged.payload(), id);
        // The payload still decodes to the same coordinate under the tag.
        assert_eq!(tagged.terrain_coord(), (12, 1083, 773));
        assert_eq!(TileId(42).index(), 42);
    }

    // A trivial two-tile tree: one root, one child, used to test composition.
    struct StubTree {
        ge: f64,
    }
    impl TileTree for StubTree {
        fn roots(&self) -> Vec<TileId> {
            vec![TileId(0)]
        }
        fn children(&self, id: TileId) -> Vec<TileId> {
            if id == TileId(0) {
                vec![TileId(1)]
            } else {
                vec![]
            }
        }
        fn properties(&self, _id: TileId) -> TileProperties {
            TileProperties {
                bounding_volume: BoundingVolume::Sphere(Sphere {
                    center: DVec3::ZERO,
                    radius: 1.0,
                }),
                geometric_error: self.ge,
                refine: Refine::Replace,
                has_content: true,
            }
        }
    }

    /// A tree whose edits are a count of calls and one failure.
    struct Editing {
        deferred: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        failing: TileId,
    }
    impl TileTree for Editing {
        fn roots(&self) -> Vec<TileId> {
            vec![TileId(0)]
        }
        fn children(&self, _id: TileId) -> Vec<TileId> {
            Vec::new()
        }
        fn properties(&self, id: TileId) -> TileProperties {
            StubTree { ge: 1.0 }.properties(id)
        }
        fn defer_edits(&self) {
            self.deferred
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
            vec![(self.failing, "no".into())]
        }
    }

    /// Deferral reaches every source of a composition, and a source's
    /// failed edit comes back under the tag that source's tiles carry.
    #[test]
    fn composite_tree_forwards_its_sources_edits() {
        let counts = [
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        ];
        let editing = |i: usize, failing| -> Box<dyn TileTree> {
            Box::new(Editing {
                deferred: std::sync::Arc::clone(&counts[i]),
                failing: TileId(failing),
            })
        };
        let composite = CompositeTileTree::new(vec![editing(0, 3), editing(1, 5)]);
        composite.defer_edits();
        for count in &counts {
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        assert_eq!(
            composite.apply_deferred_edits(),
            vec![
                (TileId(3).with_tag(0), "no".to_string()),
                (TileId(5).with_tag(1), "no".to_string())
            ]
        );
        // The stub tree has nothing to defer and nothing to apply.
        let plain = StubTree { ge: 1.0 };
        plain.defer_edits();
        assert!(plain.apply_deferred_edits().is_empty());
    }

    #[test]
    fn composite_tree_tags_and_routes() {
        let composite = CompositeTileTree::new(vec![
            Box::new(StubTree { ge: 10.0 }),
            Box::new(StubTree { ge: 20.0 }),
        ]);
        // Two roots, one per source, each tagged.
        let roots = composite.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].tag(), 0);
        assert_eq!(roots[1].tag(), 1);

        // Children keep their source tag.
        let kids0 = composite.children(roots[0]);
        assert_eq!(kids0.len(), 1);
        assert_eq!(kids0[0].tag(), 0);

        // Properties route to the right sub-source.
        assert_eq!(composite.properties(roots[0]).geometric_error, 10.0);
        assert_eq!(composite.properties(roots[1]).geometric_error, 20.0);
    }

    struct StubLoader {
        marker: f32,
    }
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl TileLoader for StubLoader {
        async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
            Ok(Loaded::Content(DecodedTileContent {
                withheld_drape: None,
                meshes: Vec::new(),
                textures: Vec::new(),
                imagery: Vec::new(),
                local_origin_ecef: DVec3::new(self.marker as f64, 0.0, 0.0),
                transform_local: Mat4::IDENTITY,
            }))
        }
    }

    fn content_origin_x(loaded: Loaded) -> f64 {
        let Loaded::Content(c) = loaded else {
            unreachable!("expected content, got Expanded");
        };
        c.local_origin_ecef.x
    }

    #[test]
    fn composite_loader_routes_by_tag() {
        let loader = CompositeLoader::new(vec![
            Box::new(StubLoader { marker: 1.0 }),
            Box::new(StubLoader { marker: 2.0 }),
        ]);
        let from_a = futures_executor::block_on(loader.load(TileId(0).with_tag(0))).expect("a");
        let from_b = futures_executor::block_on(loader.load(TileId(0).with_tag(1))).expect("b");
        assert_eq!(content_origin_x(from_a), 1.0);
        assert_eq!(content_origin_x(from_b), 2.0);
        // Unknown tag → typed error, not a panic.
        let err = futures_executor::block_on(loader.load(TileId(0).with_tag(7)));
        assert!(matches!(err, Err(LoadError::BadTag(7))));
    }
}
