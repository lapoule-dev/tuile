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

    /// The occluder that hides what is behind it from the tile `id` and
    /// everything under it: [`TileTree::occluder`], unless the tree combines
    /// sources that do not share one — a composite answers each source's,
    /// so a globe keeps its planet with a tileset beside it. The traversal
    /// asks it of each root.
    fn occluder_of(&self, _id: TileId) -> Option<crate::math::Occluder> {
        self.occluder()
    }

    /// The tile's screen-space error threshold, as a multiple of the
    /// traversal's ([`crate::traversal::Config::maximum_screen_space_error`]):
    /// 1 unless the tree says otherwise. A composite answers its source's
    /// ([`SourceOptions::sse_factor`]): a 3D Tiles tileset's errors are
    /// chosen by its author for 16 px, a terrain's for the traversal's own.
    /// The traversal refines against it, and the need's priority reads it.
    fn sse_factor(&self, _id: TileId) -> f64 {
        1.0
    }

    /// Whether the tile, drawn, stands for the ground under it — a mesh of
    /// the whole surface drawn in place of the globe's — rather than
    /// standing on it ([`SourceOptions::covers_ground`]). False unless the
    /// tree says otherwise.
    fn covers_ground(&self, _id: TileId) -> bool {
        false
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

    /// How many store events this loader has seen: a count that moves on
    /// with every change to what its stores hold that may change an answer
    /// of [`holds`](TileLoader::holds) — a tile fetched into a store or
    /// dropped from it, a decoded copy kept or let go — and never
    /// otherwise. **Synchronous, and never any I/O**: the session reads it
    /// every step and keeps every answer while it stands still, so a still
    /// view asks the loader nothing.
    ///
    /// Zero, for ever (the default), for a loader whose answers never change
    /// (`holds`' default). A change of content is the
    /// [`epoch`](TileLoader::epoch)'s, not this: when the epoch moves, every
    /// answer is dropped anyway.
    fn store_events(&self) -> u64 {
        0
    }

    /// Which of `asked` — resources, with the answer each was given — may
    /// answer [`holds`](TileLoader::holds) differently since the store
    /// events numbered `since` (a count
    /// [`store_events`](TileLoader::store_events) or this returned), pushed
    /// onto `touched`; returns the count up to which every event is
    /// accounted for, the `since` of the next call. **Synchronous, and
    /// never any I/O.**
    ///
    /// Naming one that did not change costs one more `holds`; leaving out
    /// one that did leaves the budget a wrong hint. So a loader that cannot
    /// tell, or no longer remembers that far back, names every one (the
    /// default). The answers let a loader leave out what an event cannot
    /// move: a store that only gained cannot make a held answer absent.
    fn store_changes(
        &self,
        since: u64,
        asked: &[(crate::budget::ResourceId, crate::budget::Presence)],
        touched: &mut Vec<crate::budget::ResourceId>,
    ) -> u64 {
        let _ = since;
        touched.extend(asked.iter().map(|(r, _)| *r));
        self.store_events()
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

/// How a source of a composition is traversed and drawn, beyond its tiles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceOptions {
    /// Its screen-space error threshold, as a multiple of the traversal's
    /// ([`crate::traversal::Config::maximum_screen_space_error`]): 1 for the
    /// globe, whose terrain is priced at the traversal's error; the error a
    /// tileset's author chose its geometric errors for (the specification's
    /// customary 16 px) over the traversal's, for a 3D Tiles tileset. Read by
    /// the traversal and by the need's priority alike
    /// ([`TileTree::sse_factor`]).
    pub sse_factor: f64,
    /// Where it is drawn, it stands for the ground under it — a mesh of the
    /// whole surface, buildings and trees and the ground beneath them, drawn
    /// in place of the globe's — rather than standing on it
    /// ([`TileTree::covers_ground`]).
    pub covers_ground: bool,
}

impl Default for SourceOptions {
    /// Traversed at the traversal's error, standing on the ground.
    fn default() -> Self {
        Self {
            sse_factor: 1.0,
            covers_ground: false,
        }
    }
}

/// Why a source could not join a composition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompositeError {
    /// Every tag is taken: by a source, or by one removed whose resources a
    /// session may still hold (see [`Composite::release`]).
    #[error("no source tag is free (at most {MAX_SOURCES} sources, removed ones included until released)")]
    Full,
    /// The tag asked for is taken.
    #[error("source tag {0} is taken")]
    Taken(u8),
}

/// A tag of a composition, as it stands.
enum Slot {
    /// Free: never used, or removed and released.
    Free,
    /// A source.
    Live(Entry),
    /// A source removed whose resources a session may still hold: its tag is
    /// not given to another source until the session says it holds none of
    /// them ([`Composite::release`]), so that no id of the old source ever
    /// names a tile of the new one.
    Retired,
}

struct Entry {
    tree: Option<std::sync::Arc<dyn TileTree>>,
    loader: Option<std::sync::Arc<dyn TileLoader>>,
    options: SourceOptions,
}

/// What a composition's tree, loader and handle share.
struct Sources {
    slots: Vec<Slot>,
    /// Moves with every source added or removed: part of the tree's
    /// [`revision`](TileTree::revision).
    scene: u64,
    /// Taken from the sum of the loaders' epochs so that a source coming or
    /// going does not move it: the composite's epoch moves only when a
    /// source's does ([`TileLoader::epoch`]).
    epoch_base: u64,
    /// The store events of the loaders removed, and one per removal: the
    /// composite's count only ever moves forward ([`TileLoader::store_events`]).
    events_base: u64,
    /// The count [`TileLoader::store_changes`] returned last, and each tag's
    /// count behind it.
    seen: (u64, [u64; MAX_SOURCES]),
}

impl Sources {
    fn new() -> Self {
        Self {
            slots: (0..MAX_SOURCES).map(|_| Slot::Free).collect(),
            scene: 0,
            epoch_base: 0,
            events_base: 0,
            seen: (0, [0; MAX_SOURCES]),
        }
    }

    fn entry(&self, tag: u8) -> Option<&Entry> {
        match self.slots.get(tag as usize) {
            Some(Slot::Live(e)) => Some(e),
            _ => None,
        }
    }

    fn tree(&self, tag: u8) -> Option<&dyn TileTree> {
        self.entry(tag).and_then(|e| e.tree.as_deref())
    }

    fn loader(&self, tag: u8) -> Option<std::sync::Arc<dyn TileLoader>> {
        self.entry(tag).and_then(|e| e.loader.clone())
    }

    /// The live sources, by tag.
    fn live(&self) -> impl Iterator<Item = (u8, &Entry)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| match s {
            // MAX_SOURCES is 16: every index is a u8.
            Slot::Live(e) => Some((i as u8, e)),
            _ => None,
        })
    }

    fn trees(&self) -> impl Iterator<Item = (u8, &dyn TileTree)> {
        self.live()
            .filter_map(|(tag, e)| e.tree.as_deref().map(|t| (tag, t)))
    }

    fn loaders(&self) -> impl Iterator<Item = (u8, &dyn TileLoader)> {
        self.live()
            .filter_map(|(tag, e)| e.loader.as_deref().map(|l| (tag, l)))
    }
}

/// The state a [`Composite`], its [`CompositeTileTree`] and its
/// [`CompositeLoader`] share: the sources by tag, and who to tell when they
/// change.
struct Shared {
    sources: std::sync::RwLock<Sources>,
    /// The sessions told when a source comes or goes.
    listeners: std::sync::Mutex<Vec<futures_channel::mpsc::UnboundedSender<()>>>,
}

impl Shared {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            sources: std::sync::RwLock::new(Sources::new()),
            listeners: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// The sources, read. A writer never panics while holding the lock, so a
    /// poisoned one is read as it stands.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Sources> {
        self.sources.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Sources> {
        self.sources.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Tells every session listening that the sources changed; forgets the
    /// ones gone.
    fn tell(&self) {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        listeners.retain(|l| l.unbounded_send(()).is_ok());
    }
}

/// A scene of several sources, one geometry server for all of them: the
/// globe and the 3D Tiles over it, each a source under a tag of its own
/// ([`TileId`]'s top four bits), added and removed while the session runs.
///
/// The host keeps this handle; the session runs on its [`tree`](Self::tree)
/// and its [`loader`](Self::loader) (see [`crate::runtime::in_process_with`]),
/// which see the sources as they stand at each call. A source added is traversed from the next pass; a
/// source removed is traversed no more, and its tiles leave the need — the
/// budget lets go of them as of any tile the view left.
///
/// A removed source's tag is not given to another source until the session
/// has let go of everything it held of it ([`Composite::release`]): until
/// then an id under that tag could name the old source's tile.
#[derive(Clone)]
pub struct Composite {
    shared: std::sync::Arc<Shared>,
}

impl Default for Composite {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Composite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composite")
            .field("tags", &self.tags())
            .finish()
    }
}

impl Composite {
    /// A scene with no source.
    pub fn new() -> Self {
        Self {
            shared: Shared::new(),
        }
    }

    /// Adds a source under the lowest free tag, which its tiles carry.
    pub fn add(
        &self,
        tree: Box<dyn TileTree>,
        loader: std::sync::Arc<dyn TileLoader>,
        options: SourceOptions,
    ) -> Result<u8, CompositeError> {
        let tag = {
            let sources = self.shared.read();
            sources
                .slots
                .iter()
                .position(|s| matches!(s, Slot::Free))
                .ok_or(CompositeError::Full)?
        };
        // MAX_SOURCES is 16: every index is a u8.
        self.add_at(tag as u8, tree, loader, options)?;
        Ok(tag as u8)
    }

    /// Adds a source under `tag`, which must be free: the globe under 0, so
    /// that its tiles keep the ids a standalone globe gives them.
    pub fn add_at(
        &self,
        tag: u8,
        tree: Box<dyn TileTree>,
        loader: std::sync::Arc<dyn TileLoader>,
        options: SourceOptions,
    ) -> Result<(), CompositeError> {
        {
            let mut sources = self.shared.write();
            match sources.slots.get(tag as usize) {
                Some(Slot::Free) => {}
                _ => return Err(CompositeError::Taken(tag)),
            }
            // Its store events are read from where it stands now: nothing it
            // stored before it joined was asked of it.
            let count = loader.store_events();
            sources.seen.1[tag as usize] = count;
            sources.events_base = sources.events_base.wrapping_add(count).wrapping_add(1);
            sources.epoch_base = sources.epoch_base.wrapping_sub(loader.epoch());
            sources.slots[tag as usize] = Slot::Live(Entry {
                tree: Some(std::sync::Arc::from(tree)),
                loader: Some(loader),
                options,
            });
            sources.scene += 1;
        }
        self.shared.tell();
        Ok(())
    }

    /// Removes the source under `tag`; whether there was one. Its tag stays
    /// taken until [`Composite::release`].
    pub fn remove(&self, tag: u8) -> bool {
        {
            let mut sources = self.shared.write();
            let Some(Slot::Live(entry)) = sources.slots.get(tag as usize) else {
                return false;
            };
            // Its events folded into the base, plus one: the composite's count
            // moves on, and never comes back to a value it had.
            let (count, epoch) = entry
                .loader
                .as_ref()
                .map_or((0, 0), |l| (l.store_events(), l.epoch()));
            sources.events_base = sources.events_base.wrapping_add(count).wrapping_add(1);
            sources.epoch_base = sources.epoch_base.wrapping_add(epoch);
            sources.slots[tag as usize] = Slot::Retired;
            sources.scene += 1;
        }
        self.shared.tell();
        true
    }

    /// The tags removed and not yet released: those a session must let go of.
    pub fn retired(&self) -> Vec<u8> {
        let sources = self.shared.read();
        sources
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Slot::Retired))
            .map(|(i, _)| i as u8)
            .collect()
    }

    /// Frees a removed source's tag for another source: called by the
    /// session once it holds nothing of that source. Whether it was retired.
    pub fn release(&self, tag: u8) -> bool {
        let mut sources = self.shared.write();
        match sources.slots.get_mut(tag as usize) {
            Some(slot @ Slot::Retired) => {
                *slot = Slot::Free;
                true
            }
            _ => false,
        }
    }

    /// The tags of the sources, in order.
    pub fn tags(&self) -> Vec<u8> {
        self.shared.read().live().map(|(tag, _)| tag).collect()
    }

    /// The options of the source under `tag`, if there is one.
    pub fn options(&self, tag: u8) -> Option<SourceOptions> {
        self.shared.read().entry(tag).map(|e| e.options)
    }

    /// The tree the traversal walks: every source's tiles, tagged.
    pub fn tree(&self) -> CompositeTileTree {
        CompositeTileTree {
            shared: std::sync::Arc::clone(&self.shared),
        }
    }

    /// The loader the session loads through, routing by tag.
    pub fn loader(&self) -> CompositeLoader {
        CompositeLoader {
            shared: std::sync::Arc::clone(&self.shared),
        }
    }

    /// A message each time a source comes or goes, for as long as the
    /// receiver lives: what wakes a session to step again.
    pub fn changes(&self) -> futures_channel::mpsc::UnboundedReceiver<()> {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        self.shared
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    /// A count that moves whenever a source comes or goes.
    pub fn scene(&self) -> u64 {
        self.shared.read().scene
    }
}

/// Combines several [`TileTree`]s into one, tagging each sub-source's tiles.
/// The traversal sees a single tree whose roots are the union of all sources.
///
/// Built whole from its trees ([`CompositeTileTree::new`]), or the tree of a
/// [`Composite`], whose sources come and go while it is walked.
pub struct CompositeTileTree {
    shared: std::sync::Arc<Shared>,
}

impl CompositeTileTree {
    /// Panics if more than [`MAX_SOURCES`] are given.
    pub fn new(sources: Vec<Box<dyn TileTree>>) -> Self {
        assert!(
            sources.len() <= MAX_SOURCES,
            "at most {MAX_SOURCES} sources"
        );
        let shared = Shared::new();
        {
            let mut all = shared.write();
            for (i, tree) in sources.into_iter().enumerate() {
                all.slots[i] = Slot::Live(Entry {
                    tree: Some(std::sync::Arc::from(tree)),
                    loader: None,
                    options: SourceOptions::default(),
                });
            }
        }
        Self { shared }
    }

    /// Calls `f` with the source of `id`'s tag, if there is one.
    fn with<R>(&self, id: TileId, f: impl FnOnce(&dyn TileTree) -> R) -> Option<R> {
        let sources = self.shared.read();
        sources.tree(id.tag()).map(f)
    }
}

impl TileTree for CompositeTileTree {
    /// The occluder the sources agree on, or none.
    ///
    /// Unanimity is the only safe rule for one occluder over everything:
    /// culling against a planet that only one source lives on would hide the
    /// other's ground. The traversal asks each root's own
    /// ([`TileTree::occluder_of`]), which a composite answers per source.
    fn occluder(&self) -> Option<crate::math::Occluder> {
        let sources = self.shared.read();
        let mut trees = sources.trees();
        let first = trees.next()?.1.occluder()?;
        trees
            .all(|(_, s)| s.occluder() == Some(first))
            .then_some(first)
    }

    /// The occluder of the source `id` belongs to: the globe's planet hides
    /// the globe's far side even with a tileset beside it.
    fn occluder_of(&self, id: TileId) -> Option<crate::math::Occluder> {
        self.with(id, |s| s.occluder_of(id.payload())).flatten()
    }

    /// The source's factor (its [`SourceOptions::sse_factor`]) times the one
    /// it gives the tile itself.
    fn sse_factor(&self, id: TileId) -> f64 {
        let sources = self.shared.read();
        sources.entry(id.tag()).map_or(1.0, |e| {
            e.options.sse_factor * e.tree.as_ref().map_or(1.0, |t| t.sse_factor(id.payload()))
        })
    }

    fn covers_ground(&self, id: TileId) -> bool {
        let sources = self.shared.read();
        sources.entry(id.tag()).is_some_and(|e| {
            e.options.covers_ground
                || e.tree
                    .as_ref()
                    .is_some_and(|t| t.covers_ground(id.payload()))
        })
    }

    fn defer_edits(&self) {
        let sources = self.shared.read();
        for (_, source) in sources.trees() {
            source.defer_edits();
        }
    }

    /// The sum of the sources' revisions, plus the count of sources added
    /// and removed in the top bits, when every source has one.
    ///
    /// A sum, because revisions only move forward: any source's moving moves
    /// it. A source coming or going moves the count, which outweighs any
    /// source's own revision (under 2^40), so the sum never comes back to a
    /// value it had. One source that cannot say makes the whole composite
    /// unable to.
    fn revision(&self) -> Option<u64> {
        let sources = self.shared.read();
        let sum = sources
            .trees()
            .try_fold(0u64, |sum, (_, s)| Some(sum.wrapping_add(s.revision()?)))?;
        Some(sum.wrapping_add(sources.scene << 40))
    }

    fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
        let sources = self.shared.read();
        sources
            .trees()
            .flat_map(|(tag, s)| {
                s.apply_deferred_edits()
                    .into_iter()
                    .map(move |(id, why)| (id.with_tag(tag), why))
            })
            .collect()
    }

    fn roots(&self) -> Vec<TileId> {
        let sources = self.shared.read();
        sources
            .trees()
            .flat_map(|(tag, s)| s.roots().into_iter().map(move |id| id.with_tag(tag)))
            .collect()
    }

    fn children(&self, id: TileId) -> Vec<TileId> {
        let tag = id.tag();
        self.with(id, |s| {
            s.children(id.payload())
                .into_iter()
                .map(|c| c.with_tag(tag))
                .collect()
        })
        .unwrap_or_default()
    }

    fn properties(&self, id: TileId) -> TileProperties {
        // A missing tag should never reach here (handles come from roots()),
        // but a source removed between a pass's roots and its descent can:
        // an empty, contentless tile, which the traversal walks past.
        self.with(id, |s| s.properties(id.payload()))
            .unwrap_or(TileProperties {
                bounding_volume: BoundingVolume::Sphere(crate::math::Sphere {
                    center: glam::DVec3::ZERO,
                    radius: 0.0,
                }),
                geometric_error: 0.0,
                refine: Refine::Replace,
                has_content: false,
            })
    }

    fn parent(&self, id: TileId) -> Option<TileId> {
        let tag = id.tag();
        self.with(id, |s| s.parent(id.payload()))
            .flatten()
            .map(|p| p.with_tag(tag))
    }

    /// Forwarded rather than defaulted: the sub-source may know its level
    /// without a walk, and the default here would walk through `with_tag`
    /// round trips for nothing.
    fn level(&self, id: TileId) -> u32 {
        self.with(id, |s| s.level(id.payload())).unwrap_or(0)
    }

    fn mesh_class(&self, id: TileId) -> crate::budget::Class {
        self.with(id, |s| s.mesh_class(id.payload()))
            .unwrap_or(crate::budget::Class::TerrainMesh)
    }
}

/// Routes [`TileLoader::load`] to the sub-loader matching the tile's tag, so
/// each source textures its own tiles. Pair with a [`CompositeTileTree`]
/// built from the same sources in the same order — or take both from one
/// [`Composite`].
pub struct CompositeLoader {
    shared: std::sync::Arc<Shared>,
}

impl CompositeLoader {
    pub fn new(loaders: Vec<Box<dyn TileLoader>>) -> Self {
        assert!(
            loaders.len() <= MAX_SOURCES,
            "at most {MAX_SOURCES} loaders"
        );
        let shared = Shared::new();
        {
            let mut all = shared.write();
            for (i, loader) in loaders.into_iter().enumerate() {
                all.slots[i] = Slot::Live(Entry {
                    tree: None,
                    loader: Some(std::sync::Arc::from(loader)),
                    options: SourceOptions::default(),
                });
            }
        }
        Self { shared }
    }

    /// The sub-loader of `tag`, held for as long as its call lasts: a source
    /// removed meanwhile finishes the call it was asked.
    fn of(&self, tag: u8) -> Result<std::sync::Arc<dyn TileLoader>, LoadError> {
        self.shared.read().loader(tag).ok_or(LoadError::BadTag(tag))
    }
}

/// `r` as its sub-source names it: its tile's tag cleared.
fn untagged(r: crate::budget::ResourceId) -> crate::budget::ResourceId {
    use crate::budget::ResourceId;
    match r {
        ResourceId::Mesh(t) => ResourceId::Mesh(t.payload()),
        ResourceId::Drape { tile, layer, level } => ResourceId::drape(tile.payload(), layer, level),
    }
}

/// `r`, named by a sub-source, as the composite names it: tagged `tag`.
fn tagged(r: crate::budget::ResourceId, tag: u8) -> crate::budget::ResourceId {
    use crate::budget::ResourceId;
    match r {
        ResourceId::Mesh(t) => ResourceId::Mesh(t.with_tag(tag)),
        ResourceId::Drape { tile, layer, level } => {
            ResourceId::drape(tile.with_tag(tag), layer, level)
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TileLoader for CompositeLoader {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let loader = self.of(id.tag())?;
        loader.load(id.payload()).await
    }

    fn drapes(&self, id: TileId) -> Option<DrapeReach> {
        self.shared.read().loader(id.tag())?.drapes(id.payload())
    }

    async fn load_drape(
        &self,
        id: TileId,
        layer: crate::budget::LayerId,
        level: u32,
    ) -> Result<crate::content::DrapeContent, LoadError> {
        let loader = self.of(id.tag())?;
        loader.load_drape(id.payload(), layer, level).await
    }

    fn holds(&self, r: crate::budget::ResourceId) -> crate::budget::Presence {
        self.shared
            .read()
            .loader(r.tile().tag())
            .map_or(crate::budget::Presence::Absent, |l| l.holds(untagged(r)))
    }

    /// The sub-loaders' counts, summed over a base that takes in those of
    /// the sources removed, and one more per source added or removed: it
    /// moves exactly when one of theirs does, or a source comes or goes, and
    /// never comes back to a value it had.
    fn store_events(&self) -> u64 {
        let sources = self.shared.read();
        sources.loaders().fold(sources.events_base, |n, (_, l)| {
            n.wrapping_add(l.store_events())
        })
    }

    /// Each sub-loader asked about its own resources, since the count it
    /// stood at when this returned `since`. A `since` this did not return
    /// (another asker's) is read from each one's first event: every
    /// resource whose answer may have changed since then is named. The
    /// resources of a source that came or went since are named too: their
    /// answers were never its own, or are no longer anyone's.
    fn store_changes(
        &self,
        since: u64,
        asked: &[(crate::budget::ResourceId, crate::budget::Presence)],
        touched: &mut Vec<crate::budget::ResourceId>,
    ) -> u64 {
        let mut guard = self.shared.write();
        let sources = &mut *guard;
        let known = sources.seen.0 == since;
        let (mut mine, mut theirs) = (Vec::new(), Vec::new());
        let mut total = sources.events_base;
        for tag in 0..MAX_SOURCES {
            // MAX_SOURCES is 16: every index is a u8.
            let tag = tag as u8;
            let Some(l) = sources.loader(tag) else {
                // No source under this tag: an answer asked of one that has
                // gone is no longer its.
                touched.extend(
                    asked
                        .iter()
                        .filter(|(r, _)| r.tile().tag() == tag)
                        .map(|(r, _)| *r),
                );
                continue;
            };
            let from = if known {
                sources.seen.1[tag as usize]
            } else {
                0
            };
            let at = if l.store_events() == from {
                from
            } else {
                mine.clear();
                mine.extend(
                    asked
                        .iter()
                        .filter(|(r, _)| r.tile().tag() == tag)
                        .map(|(r, p)| (untagged(*r), *p)),
                );
                theirs.clear();
                let at = l.store_changes(from, &mine, &mut theirs);
                touched.extend(theirs.iter().map(|r| tagged(*r, tag)));
                at
            };
            sources.seen.1[tag as usize] = at;
            total = total.wrapping_add(at);
        }
        sources.seen.0 = total;
        total
    }

    /// Every source's, together: the floor of each is fetched as one.
    async fn warm_up(&self, through_level: u32) {
        let loaders: Vec<std::sync::Arc<dyn TileLoader>> = {
            let sources = self.shared.read();
            sources
                .live()
                .filter_map(|(_, e)| e.loader.clone())
                .collect()
        };
        futures_util::future::join_all(loaders.iter().map(|l| l.warm_up(through_level))).await;
    }

    /// The sub-loaders' epochs, summed: it moves whenever one of theirs
    /// does, and only then — a source coming or going leaves it where it
    /// was (the session refreshes every drape when it moves).
    fn epoch(&self) -> u64 {
        let sources = self.shared.read();
        sources
            .loaders()
            .fold(sources.epoch_base, |n, (_, l)| n.wrapping_add(l.epoch()))
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
                copyright: None,
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

    /// A loader with an epoch of its own, set by the test.
    struct Epoched(std::sync::Arc<std::sync::atomic::AtomicU64>);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl TileLoader for Epoched {
        async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
            Err(LoadError::Failed("nothing".into()))
        }
        fn epoch(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn store_events(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst) * 10
        }
    }

    /// **Sources come and go while the composite is walked**: a source added
    /// takes the lowest free tag and its tiles carry it; one removed is no
    /// longer walked, and its tag is not given to another source until it is
    /// released (an id under it could still name the old source's tile in a
    /// session that has not let go of it); every change is told to whoever
    /// listens, and moves the tree's revision. Red with a removed tag freed at
    /// once: the third source took tag 0.
    #[test]
    fn sources_come_and_go_while_the_composite_is_walked() {
        let scene = Composite::new();
        let mut changes = scene.changes();
        let stub = |ge: f64| -> Box<dyn TileTree> { Box::new(StubTree { ge }) };
        let loader = |m: f32| -> std::sync::Arc<dyn TileLoader> {
            std::sync::Arc::new(StubLoader { marker: m })
        };
        let tree = scene.tree();
        assert_eq!(
            scene.add(stub(10.0), loader(1.0), SourceOptions::default()),
            Ok(0)
        );
        assert_eq!(
            scene.add(stub(20.0), loader(2.0), SourceOptions::default()),
            Ok(1)
        );
        assert_eq!(
            tree.roots(),
            vec![TileId(0).with_tag(0), TileId(0).with_tag(1)]
        );
        assert!(scene.remove(0));
        assert!(!scene.remove(0), "removed once");
        assert_eq!(tree.roots(), vec![TileId(0).with_tag(1)]);
        assert!(tree.children(TileId(0).with_tag(0)).is_empty());
        assert!(!tree.properties(TileId(0).with_tag(0)).has_content);
        assert_eq!(scene.retired(), vec![0]);
        let gone = futures_executor::block_on(scene.loader().load(TileId(0).with_tag(0)));
        assert!(matches!(gone, Err(LoadError::BadTag(0))));
        assert_eq!(
            scene.add(stub(30.0), loader(3.0), SourceOptions::default()),
            Ok(2),
            "a retired tag is not reused"
        );
        assert!(scene.release(0));
        assert!(scene.retired().is_empty());
        assert_eq!(
            scene.add(stub(40.0), loader(4.0), SourceOptions::default()),
            Ok(0)
        );
        assert_eq!(tree.properties(TileId(0).with_tag(0)).geometric_error, 40.0);
        assert_eq!(scene.tags(), vec![0, 1, 2]);
        assert_eq!(
            scene.add_at(1, stub(1.0), loader(1.0), SourceOptions::default()),
            Err(CompositeError::Taken(1))
        );
        // Five changes, each told.
        let mut told = 0;
        while let Ok(()) = changes.try_recv() {
            told += 1;
        }
        assert_eq!(told, 5);
        for _ in 3..MAX_SOURCES {
            scene
                .add(stub(1.0), loader(1.0), SourceOptions::default())
                .expect("a tag");
        }
        assert_eq!(
            scene.add(stub(1.0), loader(1.0), SourceOptions::default()),
            Err(CompositeError::Full)
        );
    }

    /// A tree that says its revision.
    struct Revised(u64);
    impl TileTree for Revised {
        fn roots(&self) -> Vec<TileId> {
            vec![TileId(0)]
        }
        fn children(&self, _id: TileId) -> Vec<TileId> {
            Vec::new()
        }
        fn properties(&self, id: TileId) -> TileProperties {
            StubTree { ge: 1.0 }.properties(id)
        }
        fn revision(&self) -> Option<u64> {
            Some(self.0)
        }
    }

    /// **The composite's counts move on as its sources come and go, and its
    /// epoch does not**: the revision and the store events never come back to
    /// a value they had (a source removed takes its own out of the sum), and
    /// the epoch — which refreshes every drape when it moves — moves only
    /// when a source's own does. Red with a plain sum: the store events fall
    /// back on a removal, the epoch moves on an addition.
    #[test]
    fn a_composites_counts_move_on_and_its_epoch_stays_as_sources_come_and_go() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let scene = Composite::new();
        let (tree, loader) = (scene.tree(), scene.loader());
        let a = std::sync::Arc::new(AtomicU64::new(3));
        scene
            .add(
                Box::new(Revised(7)),
                std::sync::Arc::new(Epoched(std::sync::Arc::clone(&a))),
                SourceOptions::default(),
            )
            .expect("a");
        let (rev, events, epoch) = (tree.revision(), loader.store_events(), loader.epoch());
        let b = std::sync::Arc::new(AtomicU64::new(5));
        let tag = scene
            .add(
                Box::new(Revised(9)),
                std::sync::Arc::new(Epoched(std::sync::Arc::clone(&b))),
                SourceOptions::default(),
            )
            .expect("b");
        assert_eq!(loader.epoch(), epoch, "a source joining moves the epoch");
        assert!(tree.revision() > rev && loader.store_events() > events);
        let (rev, events) = (tree.revision(), loader.store_events());
        scene.remove(tag);
        assert_eq!(loader.epoch(), epoch, "a source leaving moves the epoch");
        assert!(tree.revision() > rev, "the revision came back");
        assert!(loader.store_events() > events, "the store events came back");
        // A source's own epoch moving moves the composite's.
        a.store(4, Ordering::SeqCst);
        assert_ne!(loader.epoch(), epoch);
    }

    /// **A source's options reach the traversal through the tree**: its
    /// screen-space error factor, and whether it stands for the ground it
    /// covers. Red with the defaults answered.
    #[test]
    fn a_sources_options_are_its_tiles() {
        let scene = Composite::new();
        let globe = scene
            .add(
                Box::new(StubTree { ge: 1.0 }),
                std::sync::Arc::new(StubLoader { marker: 0.0 }),
                SourceOptions::default(),
            )
            .expect("globe");
        let mesh = scene
            .add(
                Box::new(StubTree { ge: 1.0 }),
                std::sync::Arc::new(StubLoader { marker: 0.0 }),
                SourceOptions {
                    sse_factor: 8.0,
                    covers_ground: true,
                },
            )
            .expect("mesh");
        let tree = scene.tree();
        assert_eq!(tree.sse_factor(TileId(1).with_tag(globe)), 1.0);
        assert_eq!(tree.sse_factor(TileId(1).with_tag(mesh)), 8.0);
        assert!(!tree.covers_ground(TileId(1).with_tag(globe)));
        assert!(tree.covers_ground(TileId(1).with_tag(mesh)));
        assert_eq!(scene.options(mesh).map(|o| o.sse_factor), Some(8.0));
    }

    /// A sub-loader's store events: the resource (as it names it) each one
    /// changed.
    #[derive(Clone, Default)]
    struct Evented(std::sync::Arc<std::sync::Mutex<Vec<crate::budget::ResourceId>>>);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl TileLoader for Evented {
        async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
            Err(LoadError::Failed("nothing".into()))
        }
        fn store_events(&self) -> u64 {
            self.0.lock().expect("events").len() as u64
        }
        fn store_changes(
            &self,
            since: u64,
            asked: &[(crate::budget::ResourceId, crate::budget::Presence)],
            touched: &mut Vec<crate::budget::ResourceId>,
        ) -> u64 {
            let events = self.0.lock().expect("events");
            let new = &events[(since as usize).min(events.len())..];
            touched.extend(asked.iter().map(|(r, _)| *r).filter(|r| new.contains(r)));
            events.len() as u64
        }
    }

    /// **The composite's store events are its sub-loaders', each asked
    /// about its own resources since where it stood**: a resource is named
    /// under its tag, an event already told is not told again, and a
    /// sub-loader with no new event is not asked.
    #[test]
    fn composite_loader_tells_its_sub_loaders_store_events() {
        use crate::budget::ResourceId;
        let (a, b) = (Evented::default(), Evented::default());
        let loader = CompositeLoader::new(vec![Box::new(a.clone()), Box::new(b.clone())]);
        let mesh = |t: u64, tag: u8| ResourceId::Mesh(TileId(t).with_tag(tag));
        let asked =
            [mesh(1, 0), mesh(2, 0), mesh(1, 1)].map(|r| (r, crate::budget::Presence::Absent));
        let start = loader.store_events();
        a.0.lock()
            .expect("events")
            .push(ResourceId::Mesh(TileId(2)));
        b.0.lock()
            .expect("events")
            .push(ResourceId::Mesh(TileId(1)));
        assert_ne!(loader.store_events(), start);
        let mut touched = Vec::new();
        let seen = loader.store_changes(start, &asked, &mut touched);
        assert_eq!(seen, loader.store_events());
        touched.sort_unstable();
        assert_eq!(touched, vec![mesh(2, 0), mesh(1, 1)]);
        // Told once: nothing new since.
        touched.clear();
        assert_eq!(loader.store_changes(seen, &asked, &mut touched), seen);
        assert!(touched.is_empty(), "{touched:?} told again");
        // A new event of the first only.
        a.0.lock()
            .expect("events")
            .push(ResourceId::Mesh(TileId(1)));
        let seen = loader.store_changes(seen, &asked, &mut touched);
        assert_eq!((touched, seen), (vec![mesh(1, 0)], loader.store_events()));
    }
}
