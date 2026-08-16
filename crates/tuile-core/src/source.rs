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
//! together, each tile carrying the textures that correspond to its source.

use crate::content::DecodedTileContent;
use crate::math::BoundingVolume;
use crate::tileset::Refine;
use async_trait::async_trait;

const TAG_SHIFT: u32 = 60;
const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;
/// Maximum number of sub-sources a [`CompositeTileTree`] can combine.
pub const MAX_SOURCES: usize = 1 << (64 - TAG_SHIFT);

// Terrain payload layout within the 60 payload bits.
const T_LEVEL_SHIFT: u32 = 54;
const T_X_SHIFT: u32 = 27;
const T_COORD_MASK: u64 = (1 << 27) - 1;

/// Opaque tile handle. The payload is interpreted by the owning [`TileTree`];
/// the tag routes between sub-sources in a [`CompositeTileTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId(pub u64);

impl TileId {
    /// Encodes a terrain quadtree coordinate `(level, x, y)` (level < 64,
    /// x/y < 2^27 — well beyond CWT's max zoom).
    pub fn from_terrain(level: u32, x: u64, y: u64) -> Self {
        debug_assert!(level < 64 && x <= T_COORD_MASK && y <= T_COORD_MASK);
        TileId(
            ((level as u64) << T_LEVEL_SHIFT)
                | ((x & T_COORD_MASK) << T_X_SHIFT)
                | (y & T_COORD_MASK),
        )
    }

    /// Decodes a terrain payload back to `(level, x, y)` (tag ignored).
    pub fn terrain_coord(self) -> (u32, u64, u64) {
        let p = self.0 & PAYLOAD_MASK;
        (
            (p >> T_LEVEL_SHIFT) as u32,
            (p >> T_X_SHIFT) & T_COORD_MASK,
            p & T_COORD_MASK,
        )
    }

    /// As an arena index (for [`Tileset`]-backed sources). Ignores the tag.
    pub fn index(self) -> usize {
        (self.0 & PAYLOAD_MASK) as usize
    }

    /// The source tag (0..[`MAX_SOURCES`]).
    pub fn tag(self) -> u8 {
        (self.0 >> TAG_SHIFT) as u8
    }

    /// The payload with the tag cleared (what a sub-source sees).
    pub fn payload(self) -> TileId {
        TileId(self.0 & PAYLOAD_MASK)
    }

    /// This handle re-tagged for source `tag`.
    pub fn with_tag(self, tag: u8) -> TileId {
        debug_assert!((tag as usize) < MAX_SOURCES);
        TileId((self.0 & PAYLOAD_MASK) | ((tag as u64) << TAG_SHIFT))
    }
}

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

    /// A stand-in surface for a tile whose real content has not arrived, built
    /// from what is already in memory. **Synchronous, and never any I/O.**
    ///
    /// Synchronous because it exists to answer *now*: the selection names ground
    /// the camera is looking at this instant, and a stand-in that arrives after
    /// a fetch would arrive alongside the real tile and be pointless.
    ///
    /// # Why the engine needs this at all
    ///
    /// A consumer that lacks a selected tile draws its nearest ready ancestor.
    /// That works and it has a defect: the ancestor also covers the siblings
    /// that *did* arrive, so two approximations of the same hillside end up
    /// over the same ground, interpenetrating within a few metres, and the
    /// depth test picks a winner per pixel. The reference implementation builds
    /// a fill mesh for exactly this reason (`TerrainFillMesh`), and so does
    /// this: one surface per patch of ground, always.
    ///
    /// # Why here and not in a renderer
    ///
    /// It is pure geometry over a tile rectangle — no device, no pipeline, no
    /// texture. Putting it in a backend would mean every backend reimplementing
    /// it, and eventually disagreeing about where the ground is. Emitted by the
    /// server, it reaches wgpu, a browser and a USD renderer identically, and
    /// each one draws it through the path it already has for content.
    ///
    /// Default: `None`, for the many sources with nothing coarse to build from.
    fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
        None
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
