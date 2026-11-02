// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod jobs;
mod provenance;
mod sources;
mod store_log;
mod switch;

pub use provenance::{digest, Provenance, TerrainOrigin};
pub use sources::{Credit, Held, Sources, TilesetSource};
pub use switch::SwitchableImagery;

use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tuile_core::budget::{LayerId, LayerReach, Presence, ResourceId};
use tuile_core::content::{DecodedTexture, DrapeContent};
use tuile_core::fetch::FetchError;
use tuile_core::geo::WGS84_A;
use tuile_core::offload::{self, Offload};
use tuile_core::raster::{self, GeoRect, ImageryCoord, ImageryProvider, RasterError};
use tuile_core::source::{DrapeReach, LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_terrain::{
    to_ground, Availability, AvailabilityRange, GeographicTilingScheme, LayerJson, TerrainHeights,
    TerrainSource, TerrainTree, TileCoord,
};

/// How coarse the guaranteed bottom layer is.
///
/// Level 4 is 256 tiles for the whole planet at most — and in practice a
/// handful resident, since one tile spans a region and is shared by every
/// terrain tile inside it. That is what makes it a floor worth having: it is
/// the level that is *always* there, because nothing evicts it and everything
/// wants it.
///
/// It must not be deeper than the level the host pins and preloads, or the
/// guarantee is a promise about tiles nothing keeps: the floor would name a
/// level-5 tile that the GPU never held and that eviction is free to drop, and
/// the ground under it would be bare exactly when the fallback was needed.
/// `tuile-wgpu-viewer`'s `TUILE_PIN_LEVEL` defaults to 4 to match.
///
/// Deeper would also be sharper and would defeat the purpose — a level-17
/// stand-in is as likely to be missing as the level-18 tile it stands in for.
const FLOOR_LEVEL: u32 = 4;

/// The recursive upsample walk's return type, with `Send` where `Send` exists.
///
/// `terrain_mesh` recurses onto its parent, so it must name its own future
/// rather than be an `async fn`. Naming it means restating the auto-traits by
/// hand, and the honest restatement differs by target: natively the future
/// crosses to whatever thread polls the server and must be `Send`; in a browser
/// there is one thread, [`TerrainSource`] is declared `?Send` there, and
/// demanding `Send` here would make this crate uncompilable for wasm — which it
/// was, for exactly this line.
#[cfg(not(target_arch = "wasm32"))]
type MeshFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Arc<tuile_terrain::QuantizedMesh>, LoadError>>
            + Send
            + 'a,
    >,
>;

#[cfg(target_arch = "wasm32")]
type MeshFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Arc<tuile_terrain::QuantizedMesh>, LoadError>> + 'a,
    >,
>;

impl GlobeOptions {
    /// These options with the consumer's imagery compression: one choice,
    /// resolved against its own device into both
    /// [`imagery_plan`](Self::imagery_plan) (JPEG/PNG tiles, encoded at load)
    /// and [`stored_imagery`](Self::stored_imagery) (tiles stored already
    /// compressed, transcoded only), so the two cannot disagree.
    ///
    /// Not calling it is [`ImageryCompression::Off`](tuile_core::texels::ImageryCompression::Off):
    /// RGBA8 everywhere.
    pub fn with_imagery_compression(
        mut self,
        choice: tuile_core::texels::ImageryCompression,
        caps: tuile_core::texels::GpuTextureCaps,
    ) -> Self {
        self.imagery_plan = ImageryPlanHandle::new(choice.plan(caps));
        self.stored_imagery = choice.stored(caps);
        self
    }

    /// Which coarse tile goes under a drape.
    ///
    /// The choice lives here, on the options, rather than inline at the one
    /// call site — so it can be exercised on its own. Testing `fixed_floor`
    /// and `coarse_floor` separately proves each does what it says and proves
    /// nothing about which one a bulk render gets, and that dispatch is the
    /// whole of the behaviour.
    fn floor_under(
        &self,
        scheme: &raster::TilingScheme,
        rect: &GeoRect,
        cache: &Mutex<ImageryCache>,
    ) -> ImageryCoord {
        if self.deterministic_floor {
            fixed_floor(scheme, rect)
        } else {
            coarse_floor(scheme, rect, cache)
        }
    }
}

/// The floor a rectangle always gets, whatever any cache holds.
///
/// A pure function of the geometry, which is the whole point: see
/// [`GlobeOptions::deterministic_floor`].
fn fixed_floor(scheme: &raster::TilingScheme, rect: &GeoRect) -> ImageryCoord {
    let deepest = scheme.containing_tile(rect);
    let level = FLOOR_LEVEL.max(scheme.minimum_level).min(deepest.level);
    let up = deepest.level - level;
    ImageryCoord {
        level,
        x: deepest.x >> up,
        y: deepest.y >> up,
    }
}

/// The tile to lay underneath the sharp mosaic: the closest ancestor already
/// decoded, or the guaranteed floor if none is.
///
/// A fixed level would be the simple answer and it wastes what is already in
/// hand. Descending to level 18 leaves every level on the way down in the cache;
/// pulling back and covering the ground with level 5 — 4.9 km per texel — when
/// level 12 is sitting right there is gratuitous blur, and blur that lasts for
/// as long as the network takes.
///
/// So the walk starts at the deepest tile that covers the rectangle whole and
/// climbs until it finds one that is decoded, stopping at [`FLOOR_LEVEL`],
/// which is pinned and therefore always there. As sharp as what is available,
/// never sharper, and never nothing. This is what the reference implementation
/// does when a tile's own imagery has not arrived — it walks up to the closest
/// ready ancestor rather than to a level chosen in advance.
fn coarse_floor(
    scheme: &raster::TilingScheme,
    rect: &GeoRect,
    decoded: &Mutex<ImageryCache>,
) -> ImageryCoord {
    let deepest = scheme.containing_tile(rect);
    let floor = FLOOR_LEVEL.max(scheme.minimum_level).min(deepest.level);
    let ancestor = |level: u32| {
        let up = deepest.level - level;
        ImageryCoord {
            level,
            x: deepest.x >> up,
            y: deepest.y >> up,
        }
    };
    // Deepest first: the sharpest ancestor that is already in hand wins.
    if let Ok(cache) = decoded.lock() {
        for level in (floor..=deepest.level).rev() {
            let candidate = ancestor(level);
            if cache.has(&candidate) {
                return candidate;
            }
        }
    }
    // Nothing cached on the way up. The floor is pinned in the server's
    // residency, so asking for it is the request most likely to be free.
    ancestor(floor)
}

/// Whether a decoded texture is entirely opaque black.
///
/// Sampled rather than scanned: a 256×256 tile is 65 536 texels and this runs on
/// every arrival, but a tile that is black at a hundred spread-out points and
/// not elsewhere does not exist in aerial photography. One false negative in
/// exchange for a check that costs nothing.
///
/// Read on RGBA8 only: block-compressed texels are encoded after this check
/// ran on their pixels, and are never called black here.
pub(crate) fn is_opaque_black(tex: &DecodedTexture) -> bool {
    const SAMPLES: usize = 100;
    let Some(rgba8) = tex.rgba8() else {
        return false;
    };
    let texels = rgba8.len() / 4;
    if texels == 0 {
        return false;
    }
    let step = (texels / SAMPLES).max(1);
    (0..texels)
        .step_by(step)
        .all(|i| rgba8[i * 4] == 0 && rgba8[i * 4 + 1] == 0 && rgba8[i * 4 + 2] == 0)
}

/// What shows under a mosaic wherever no layer reaches.
///
/// A muted slate, close to deep water and to ground in shadow — the two things
/// most of a globe is.
const UNCOVERED_GROUND: [f32; 4] = [0.16, 0.20, 0.24, 1.0];

/// Extends the top and bottom rows of an imagery grid to the poles.
///
/// Web Mercator stops at ±85°: the projection has no tile for the caps, and it
/// never will, because the projection sends them to infinity. Terrain does not
/// stop there. So a geometry tile reaching past the limit had fragments that
/// fell outside *every* layer's coverage, kept the material's base colour, and
/// showed as a white disc centred on the pole with its edge exactly at 85° — one
/// of the more legible bugs to come out of a render.
///
/// The answer is the one the per-tile resample used to give implicitly, by
/// clamping its sampling coordinate: the edge row of imagery is the best data
/// there is up there, so let it stand for everything beyond. Widening the
/// *coverage* does that, and the clamping sampler supplies the rest — texture
/// coordinates past the tile resolve to its edge, which is what a polar cap
/// looks like anyway.
fn to_the_pole(mut rect: GeoRect, coord: ImageryCoord, rows: u64) -> GeoRect {
    use std::f64::consts::FRAC_PI_2;
    if coord.y == 0 {
        rect.north = FRAC_PI_2;
    }
    if coord.y + 1 >= rows {
        rect.south = -FRAC_PI_2;
    }
    rect
}

/// Shared, host-declared count of imagery layers one drape may carry.
///
/// A handle rather than a number for a reason of ordering, not of taste: the
/// answer belongs to the renderer — how many textures it will bind in one draw,
/// which a wgpu backend reads off the device and a WebGL2 one off its context —
/// and the loader is usually built **before** there is a window to have a device
/// in. The viewer builds its stream, then its surface, then its GPU; asking the
/// loader to know at construction time would mean either creating the device
/// twice or writing the number down, and writing it down is what this replaces.
///
/// Until the host says otherwise it reads [`raster::MIN_IMAGERY_SLOTS`], the
/// fewest a drape can be correct with — so a host that never speaks gets coarse
/// ground rather than layers nothing samples.
#[derive(Clone)]
pub struct LayerBudget(Arc<std::sync::atomic::AtomicU32>);

impl Default for LayerBudget {
    fn default() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU32::new(
            raster::MIN_IMAGERY_SLOTS,
        )))
    }
}

impl std::fmt::Debug for LayerBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LayerBudget({})", self.get())
    }
}

impl LayerBudget {
    /// Declares how many imagery textures one draw will bind. Call once, as soon
    /// as the renderer exists.
    ///
    /// Clamped through [`raster::imagery_slots`] rather than taken as given: the
    /// floor is what a 3×3 drape needs to be correct and the ceiling is what the
    /// per-fragment fetch cost allows, and neither is the host's to overrule.
    /// The argument is therefore what the *device reported*, not a budget the
    /// host already computed.
    pub fn set_from_device(&self, max_sampled_textures_per_stage: u32) {
        self.0.store(
            raster::imagery_slots(max_sampled_textures_per_stage),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Declares that a drape may carry as many layers as its mosaic needs.
    ///
    /// For consumers that COMPOSE on the CPU (the bake path behind the Hydra
    /// boundary): there is no per-fragment fetch there, so the 5×5 shading
    /// ceiling that [`LayerBudget::set_from_device`] enforces would only
    /// coarsen the mosaic back — measured as an exposure checkerboard, one
    /// imagery level per terrain level. A GPU viewer must never call this:
    /// its ceiling is a real per-fragment cost.
    pub fn set_unbounded(&self) {
        self.0.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn get(&self) -> u32 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A small FIFO memo of decoded terrain meshes.
///
/// It exists for the upsample chain. A tile past the source's data is built from
/// its **immediate parent's** mesh, one level at a time — which is what the
/// reference implementation does, and the only affordable way: rebuilding each
/// descendant from a distant ancestor re-clips that ancestor's whole mesh once
/// per tile, which is quadratic in the gap and pinned a core at a hundred
/// percent over ten levels.
///
/// With each rung remembered, a chain is walked once and a tile's siblings find
/// their shared parent already built. Without this, "start from the real data
/// and come back down" is exactly the quadratic thing again.
struct MeshCache {
    map: HashMap<TileCoord, Arc<tuile_terrain::QuantizedMesh>>,
    order: VecDeque<TileCoord>,
    cap: usize,
}

impl MeshCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, c: TileCoord) -> Option<Arc<tuile_terrain::QuantizedMesh>> {
        self.map.get(&c).map(Arc::clone)
    }

    /// Keeps `mesh` for `c`; `changed` hears of `c` when it is new here
    /// (kept), and of each tile let go to make room.
    fn put(
        &mut self,
        c: TileCoord,
        mesh: Arc<tuile_terrain::QuantizedMesh>,
        changed: &mut dyn FnMut(TileCoord, store_log::Way),
    ) {
        if self.map.insert(c, mesh).is_none() {
            changed(c, store_log::Way::Kept);
            self.order.push_back(c);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                    changed(old, store_log::Way::LetGo);
                }
            }
        }
    }
}

/// A small FIFO cache of decoded, reprojected imagery tiles, so the many
/// terrain tiles that share one imagery tile neither re-decode nor re-resample
/// it. (The network cache — disk, in-memory — belongs to the injected provider.)
///
/// It holds `Arc`s and hands out `Arc`s: an entry evicted here is still alive
/// for as long as a loaded terrain tile references it, so eviction bounds how
/// long a tile is remembered, never whether one is still valid.
struct ImageryCache {
    map: HashMap<ImageryCoord, Arc<DecodedTexture>>,
    order: VecDeque<ImageryCoord>,
    cap: usize,
    /// The provider's [`generation`](ImageryProvider::generation) everything
    /// in `map` was fetched under. A coord names a different picture on each
    /// side of a switch of imagery, so the cache belongs to one generation at
    /// a time — see [`ImageryCache::turn_to`].
    generation: u64,
}

impl ImageryCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
            generation: 0,
        }
    }

    /// Makes this the cache of `generation`, forgetting every tile of any
    /// other: whether it did.
    ///
    /// Forgetting is safe and it is not what takes the old picture off the
    /// screen: the cache hands out `Arc`s, so every drape already built keeps
    /// the textures it names for as long as its tile is resident. What is
    /// forgotten is only the *lookup*, which from here on would answer the
    /// new imagery's question with the old imagery's tile.
    fn turn_to(&mut self, generation: u64) -> bool {
        if self.generation == generation {
            return false;
        }
        self.map.clear();
        self.order.clear();
        self.generation = generation;
        true
    }

    /// Whether this tile is already decoded, without taking a reference.
    fn has(&self, c: &ImageryCoord) -> bool {
        self.map.contains_key(c)
    }

    fn get(&self, c: &ImageryCoord) -> Option<Arc<DecodedTexture>> {
        self.map.get(c).map(Arc::clone)
    }

    /// Keeps `tex` for `c`; `changed` hears of `c` when it is new here
    /// (kept), and of each tile let go to make room.
    fn put(
        &mut self,
        c: ImageryCoord,
        tex: Arc<DecodedTexture>,
        changed: &mut dyn FnMut(ImageryCoord, store_log::Way),
    ) {
        if self.map.insert(c, tex).is_none() {
            changed(c, store_log::Way::Kept);
            self.order.push_back(c);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                    changed(old, store_log::Way::LetGo);
                }
            }
        }
    }
}

/// Tuning for [`globe`].
///
/// Caching is deliberately absent here. A terrain source and an imagery
/// provider each cache their own bytes (`tuile_terrain::CachedTerrain`,
/// `tuile_core::raster::CachedImagery`), which is where the bytes and their
/// stated lifetimes actually are — this crate composes what the host hands it
/// and adds no tier of its own.
///
/// Its default is the interactive contract: in particular no deterministic
/// floor, for a viewer showing blur it did not have to show is a
/// regression.
#[derive(Debug, Clone, Default)]
pub struct GlobeOptions {
    /// When true, terrain only (no imagery) — the geometry debug view.
    pub no_imagery: bool,
    /// How deep the quadtree may divide, or `None` for "as deep as the imagery
    /// provider goes".
    ///
    /// A ceiling on refinement, not on reach. Set it to see what a shallower
    /// globe would have selected without regenerating anything — and to bound
    /// a diagnostic run whose tile count would otherwise be the source's.
    pub max_level: Option<u32>,
    /// How many imagery layers one drape may carry.
    ///
    /// This is a **consumer** limit and the loader cannot know it: it is how
    /// many textures the renderer will bind in one draw, which a wgpu backend
    /// reads off the device (`GpuContext::imagery_slots`) and a WebGL2 one gets
    /// from its own context. Asking for more than the consumer can bind loses
    /// the extra layers silently — they are packed into a table nothing samples,
    /// and the ground quietly reverts to the coarse layer underneath at exactly
    /// the tiles that straddle worst.
    ///
    /// A handle rather than a number because the renderer usually does not
    /// exist yet when the loader is built — see [`LayerBudget`].
    pub imagery_slots: LayerBudget,
    /// Whether the coarse layer under a drape is chosen by geometry alone.
    ///
    /// **This is the difference between a render that reproduces and one that
    /// does not.** `coarse_floor` normally lays down the sharpest ancestor
    /// that happens to be decoded already, which is right for a viewer — it
    /// spends what is in hand rather than blurring ground it could show. But
    /// "what happens to be decoded" is a function of the order concurrent
    /// loads finished and of what a bounded LRU evicted, so the same tile in
    /// the same frame gets a different floor from one run to the next, the
    /// drape key changes with it, and the composed pixels change.
    ///
    /// Measured, 2026-09-10: two bakes of one scene, one warm cache, identical
    /// settings, selected **exactly the same 80 and 60 tiles** — and 16 of the
    /// 80 came out draped differently. The ground was never in doubt; the
    /// imagery was.
    ///
    /// On, the floor is always the tile's `FLOOR_LEVEL` ancestor: pinned,
    /// therefore always available, and a pure function of the rectangle. It
    /// costs sharpness only where the sharp mosaic has a hole, and a converged
    /// bulk frame has none — so a batch render pays approximately nothing for
    /// being reproducible.
    pub deterministic_floor: bool,
    /// What the consumer already holds, asked **before** any imagery is
    /// requested.
    ///
    /// Given `(tile id, drape identity)` it answers whether the composed drape
    /// is already in hand. When it says yes, the layers are neither downloaded
    /// nor decoded nor reprojected nor composed: the content comes back with no
    /// imagery and [`tuile_core::DecodedTileContent::withheld_drape`] set to
    /// the identity it would have had.
    ///
    /// A camera orbit re-selects nearly the same ground every frame, so on a
    /// long shot this is most of the imagery work. The HTTP cache already
    /// spares the *network* for a second look at the same tile; it does not
    /// spare the JPEG decode, the reprojection to geographic spacing, or the
    /// mosaic compose — which are the expensive halves, and the ones that hold
    /// megabytes while they run.
    ///
    /// Left `None`, nothing changes and nothing is withheld.
    pub held_drape: Option<HeldDrape>,
    /// The size a drape is composed at, which is part of its identity.
    ///
    /// The loader does not compose anything — that happens in the consumer —
    /// but it cannot ask [`GlobeOptions::held_drape`] about an identity without
    /// it. Ignored when `held_drape` is `None`.
    pub composed_at: u32,
    /// Whether the imagery's pixels go unused: the consumer names each
    /// drape's tiles — their coords, placements and digests — and composes
    /// nothing. A bake by reference.
    ///
    /// Then a tile the provider can describe without its bytes
    /// ([`ImageryProvider::fetch_tile_stamp`]) is neither downloaded nor
    /// decoded: it is placed from its coord and the image size the provider
    /// states, and its digest noted from the provider's word. The layer
    /// carries a texture of that size **and no pixels** — nothing may read
    /// them, which is what this option promises. Every drape comes out with
    /// the same layers, placements, digests and composed size as from the
    /// bytes; a tile the provider cannot describe is fetched as always.
    pub imagery_by_stamp: bool,
    /// How the consumer holds the imagery it is handed: its own
    /// [`tuile_core::texels::ImageryCompression`] resolved against its own
    /// device. The default is RGBA8, and nothing here chooses otherwise.
    ///
    /// Read at each decode: a texture decoded under a plan that encodes on the
    /// consumer's GPU comes out RGBA8 marked with the plan's target
    /// ([`tuile_core::content::DecodedTexture::planned`]), so the budget charges it in
    /// that format. A plan that encodes on the CPU leaves it RGBA8 for now:
    /// that encoder runs in the decode job, which does not exist yet.
    ///
    /// A handle, like [`Self::imagery_slots`], because the plan needs the
    /// device and the device comes after the loader — see [`ImageryPlanHandle`].
    pub imagery_plan: ImageryPlanHandle,
    /// What the consumer does with imagery tiles stored already compressed
    /// (KTX 2.0 holding UASTC, BC7 or ASTC 4×4): its own
    /// [`tuile_core::texels::ImageryCompression::stored`] on its own device.
    ///
    /// The default, [`StoredPlan::OFF`](tuile_core::texels::StoredPlan::OFF),
    /// unpacks UASTC to RGBA8 and holds nothing as blocks. A JPEG or PNG
    /// tile ignores it.
    pub stored_imagery: tuile_core::texels::StoredPlan,
}

/// The consumer's imagery plan, shared with the loader and set once the
/// consumer's device exists: the same ordering as [`LayerBudget`]. Until it
/// is set — and for every consumer that never sets it — it reads
/// [`tuile_core::texels::ImageryPlan::RGBA8`].
///
/// A texture is marked when it is decoded and keeps that mark in the decoded
/// cache, so a plan set later applies to what is decoded from then on.
#[derive(Clone, Default)]
pub struct ImageryPlanHandle(Arc<std::sync::Mutex<tuile_core::texels::ImageryPlan>>);

impl ImageryPlanHandle {
    /// A handle that already reads `plan`.
    pub fn new(plan: tuile_core::texels::ImageryPlan) -> Self {
        Self(Arc::new(std::sync::Mutex::new(plan)))
    }

    /// Declares the plan, as soon as the consumer's device says what it is.
    pub fn set(&self, plan: tuile_core::texels::ImageryPlan) {
        if let Ok(mut held) = self.0.lock() {
            *held = plan;
        }
    }

    pub fn get(&self) -> tuile_core::texels::ImageryPlan {
        self.0.lock().map(|p| *p).unwrap_or_default()
    }
}

impl std::fmt::Debug for ImageryPlanHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageryPlanHandle({:?})", self.get())
    }
}

/// The consumer's answer to "do you already hold this drape?".
///
/// A named type rather than a bare `Arc<dyn Fn>` so [`GlobeOptions`] can keep
/// its `Debug`, which every option struct here has and which is worth more than
/// the closure's identity.
#[derive(Clone)]
pub struct HeldDrape(pub std::sync::Arc<dyn Fn(u64, u64) -> bool + Send + Sync>);

impl HeldDrape {
    pub fn new(f: impl Fn(u64, u64) -> bool + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    fn holds(&self, tile: u64, drape: u64) -> bool {
        (self.0)(tile, drape)
    }
}

impl std::fmt::Debug for HeldDrape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HeldDrape(..)")
    }
}

/// What every asker of one imagery tile is answered: the coord served and its
/// texture, or why there is none.
type ImageryAnswer = Result<(ImageryCoord, Arc<DecodedTexture>), String>;

/// The imagery tiles being fetched, by imagery generation and coord, with
/// whoever else is waiting for each.
///
/// Without it, every terrain tile under one imagery tile fetched that tile
/// itself, at the same moment: a browser showed the same quadkeys two and
/// three times over, each holding a connection, and serialised them behind
/// its cache lock — the pending requests under which the page stopped.
///
/// The same holds of terrain: every tile past the source's data is cut from
/// its ancestor, and a whole block of them loading at once fetched that one
/// ancestor once each — a browser showed one terrain tile requested 23 times,
/// many of them at once ([`TerrainInFlight`]).
struct InFlight<K, A>(Mutex<HashMap<K, Vec<futures_channel::oneshot::Sender<A>>>>);

impl<K, A> Default for InFlight<K, A> {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

type ImageryInFlight = InFlight<(u64, ImageryCoord), ImageryAnswer>;

/// What every asker of one terrain tile is answered: its mesh, or why not.
type TerrainAnswer = Result<Arc<tuile_terrain::QuantizedMesh>, String>;

/// The terrain tiles being fetched or built, by coord.
type TerrainInFlight = InFlight<TileCoord, TerrainAnswer>;

/// Whether an asker fetches, or waits for the one that does.
enum Turn<A> {
    Lead,
    Wait(futures_channel::oneshot::Receiver<A>),
}

impl<K: std::hash::Hash + Eq + Copy, A: Clone> InFlight<K, A> {
    fn join(&self, key: K) -> Turn<A> {
        let mut flights = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match flights.get_mut(&key) {
            Some(waiting) => {
                let (tell, answer) = futures_channel::oneshot::channel();
                waiting.push(tell);
                Turn::Wait(answer)
            }
            None => {
                flights.insert(key, Vec::new());
                Turn::Lead
            }
        }
    }

    /// The tiles being fetched right now.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// The asker that fetches. Dropped without [`Leading::finish`] — cancelled —
/// it lets go of the tile, and its waiters' channels close: the next of them
/// fetches.
struct Leading<'a, K: std::hash::Hash + Eq, A> {
    flight: &'a InFlight<K, A>,
    key: K,
}

impl<K: std::hash::Hash + Eq, A: Clone> Leading<'_, K, A> {
    fn finish(self, answer: &A) {
        let waiting = self
            .flight
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
        for tell in waiting.into_iter().flatten() {
            let _ = tell.send(answer.clone());
        }
        std::mem::forget(self);
    }
}

impl<K: std::hash::Hash + Eq, A> Drop for Leading<'_, K, A> {
    fn drop(&mut self) {
        self.flight
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

/// Loads a terrain tile and drapes the imagery covering it, by reference.
/// Generic over the terrain source `T` and imagery provider `I` — no ion, no
/// Bing, no transport.
pub struct PlanetaryLoader<T: TerrainSource, I: ImageryProvider> {
    terrain: T,
    imagery: I,
    scheme: GeographicTilingScheme,
    opts: GlobeOptions,
    cache: Mutex<ImageryCache>,
    /// Decoded terrain meshes, so an upsample chain is walked once.
    meshes: Mutex<MeshCache>,
    /// Shared with the tree: each terrain tile's `metadata` extension reveals
    /// the availability of its descendants, folded in here so traversal can
    /// keep refining toward the finest LOD.
    availability: Arc<Availability>,
    /// What each mesh and each imagery tile was made from, noted as it is
    /// fetched.
    provenance: Arc<Provenance>,
    /// Shared with the host's camera: each decoded tile's relief, so the eye can
    /// be kept above the ground rather than above the ellipsoid.
    heights: Arc<TerrainHeights>,
    /// The imagery tiles being fetched, so a tile several terrain tiles want
    /// at once is fetched once: see [`ImageryInFlight`].
    fetching: ImageryInFlight,
    /// The terrain tiles being fetched or built, likewise.
    building: TerrainInFlight,
    /// What changed in what [`TileLoader::holds`] reads, as it changed: the
    /// store events the session keeps its presence answers by.
    changes: store_log::StoreLog,
    /// Where decoding and resampling run.
    ///
    /// Not a detail of taste: the server is one future, so any CPU work left in
    /// an `async fn` here runs on the single thread polling it, and the sixty
    /// other loads in flight queue behind it. Defaults to
    /// [`offload::Inline`](tuile_core::offload::Inline), which is the old
    /// behaviour and the only possible one on wasm.
    offload: Arc<dyn Offload>,
}

impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> PlanetaryLoader<T, I> {
    /// The decoded-imagery cache, as the cache of the imagery being served
    /// **now**. Every access goes through here, so no lookup can answer with
    /// a tile fetched before a switch.
    fn decoded(&self) -> std::sync::MutexGuard<'_, ImageryCache> {
        let mut cache = self.cache.lock().expect("imagery cache");
        if cache.turn_to(self.imagery.generation()) {
            self.changes.note(store_log::Change::All);
        }
        cache
    }

    /// Notes a change to what a drape's presence reads at imagery tile `c`:
    /// decoded (`decoded`) or stored, kept or let go.
    fn imagery_changed(
        &self,
        scheme: &raster::TilingScheme,
        c: ImageryCoord,
        decoded: bool,
        way: store_log::Way,
    ) {
        self.changes.note(store_log::Change::Imagery {
            rect: scheme.tile_rect(c),
            level: c.level,
            decoded,
            way,
        });
    }

    /// Keeps a decoded imagery tile, noting the tiles it adds and lets go
    /// whose answers may move.
    ///
    /// A drape is held when each of its tiles is decoded or stored, and its
    /// floor is the deepest decoded ancestor or, failing one, the ancestor
    /// at the floor level. So a tile the imagery store holds, under a floor
    /// tile that is held, changes no answer by being decoded or let go: it
    /// was held and stays held, and the floor it gives or takes is held
    /// either way. Every other change is noted.
    fn keep_imagery(
        &self,
        cache: &mut ImageryCache,
        scheme: &raster::TilingScheme,
        c: ImageryCoord,
        tex: Arc<DecodedTexture>,
    ) {
        let mut changed = Vec::new();
        cache.put(c, tex, &mut |at, way| changed.push((at, way)));
        let floor = FLOOR_LEVEL.max(scheme.minimum_level);
        let held = |t: ImageryCoord| cache.has(&t) || self.imagery.holds(t);
        for (at, way) in changed {
            let under = if at.level > floor {
                let up = at.level - floor;
                ImageryCoord {
                    level: floor,
                    x: at.x >> up,
                    y: at.y >> up,
                }
            } else {
                at
            };
            if !(self.imagery.holds(at) && held(under)) {
                self.imagery_changed(scheme, at, true, way);
            }
        }
    }

    /// Fetches imagery tile `c`'s bytes, noting a change when the imagery
    /// store's index says otherwise of it after than before.
    async fn imagery_bytes(
        &self,
        scheme: &raster::TilingScheme,
        c: ImageryCoord,
    ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, RasterError> {
        let before = self.imagery.holds(c);
        let fetched = self.imagery.fetch_tile_bytes(c).await;
        let after = self.imagery.holds(c);
        if after != before {
            self.imagery_changed(scheme, c, false, store_log::Way::of(after));
        }
        fetched
    }

    /// Fetches terrain tile `c`'s bytes, noting a change when the terrain
    /// store's index says otherwise of it after than before.
    async fn terrain_bytes(
        &self,
        c: TileCoord,
    ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError> {
        let before = self.terrain.holds(c);
        let fetched = self.terrain.fetch_tile(c).await;
        let after = self.terrain.holds(c);
        if after != before {
            self.changes
                .note(store_log::Change::Mesh(c, store_log::Way::of(after)));
        }
        fetched
    }

    /// Folds a tile's `metadata` ranges into the shared availability, so the
    /// next traversal can refine past this level (how Cesium World Terrain
    /// reaches its finest LOD). Idempotent by way of the availability's own
    /// range merging — a store hit may replay ranges already known.
    fn reveal(&self, c: TileCoord, ranges: Option<&[Vec<AvailabilityRange>]>) {
        let Some(ranges) = ranges else { return };
        tracing::debug!(
            z = c.level,
            x = c.x,
            y = c.y,
            levels = ranges.len(),
            deepest = c.level as usize + ranges.len(),
            "metadata availability"
        );
        self.availability.add_descendant_ranges(c.level, ranges);
    }

    /// One imagery tile, decoded and reprojected onto geographic spacing over
    /// its own rectangle — the form that can be shared, because it depends on
    /// the imagery tile and nothing else.
    ///
    /// Returns the coord **actually served**, which is not always the one asked
    /// for: where a provider has no tile at this zoom, the nearest available
    /// ancestor is returned as-is and the caller masks it to the ground the
    /// request stood for. That is strictly better than the upsampled copy this
    /// used to fabricate — no second generation of filtering, no private texture
    /// per missing coord, and the ancestor is shared with everything else
    /// leaning on it.
    ///
    /// The small in-process cache spares the decode *and* the resample for the
    /// many terrain tiles that share a tile; persistence across runs is the
    /// provider's business, not this one's.
    async fn fetch_imagery(
        &self,
        c: ImageryCoord,
    ) -> Result<(ImageryCoord, Arc<DecodedTexture>), LoadError> {
        if let Some(tex) = self.decoded().get(&c) {
            return Ok((c, tex));
        }
        // One fetch per tile at a time: an imagery tile spans several terrain
        // tiles, and their loads ask for it together. Whoever asks first
        // fetches; the others wait for its answer. If that first one is
        // cancelled — its terrain tile left the view — the next in line
        // fetches instead, so nobody waits on a fetch nobody makes.
        let key = (self.imagery.generation(), c);
        loop {
            match self.fetching.join(key) {
                Turn::Wait(answer) => match answer.await {
                    Ok(answer) => return answer.map_err(LoadError::Failed),
                    Err(futures_channel::oneshot::Canceled) => continue,
                },
                Turn::Lead => {
                    let lead = Leading {
                        flight: &self.fetching,
                        key,
                    };
                    let answer = self.fetch_imagery_once(c).await.map_err(|e| match e {
                        LoadError::Failed(why) => why,
                        other => other.to_string(),
                    });
                    lead.finish(&answer);
                    return answer.map_err(LoadError::Failed);
                }
            }
        }
    }

    /// [`Self::fetch_imagery`], for the one asker that fetches.
    async fn fetch_imagery_once(
        &self,
        c: ImageryCoord,
    ) -> Result<(ImageryCoord, Arc<DecodedTexture>), LoadError> {
        // Read before the fetch, and compared after it: bytes that were asked
        // of one imagery set and answered across a switch belong to neither
        // side's cache.
        let generation = self.imagery.generation();
        if let Some(tex) = self.decoded().get(&c) {
            return Ok((c, tex));
        }
        let scheme = self.imagery.tiling_scheme();
        if self.opts.imagery_by_stamp {
            match self.imagery.fetch_tile_stamp(c).await {
                tuile_core::storage::Stamp::Present(stamp) => {
                    self.provenance.stamped_imagery(c, stamp.digest);
                    // The size the bytes would have decoded to — reprojection
                    // keeps it — and no pixels: nothing composes this drape.
                    let tex = Arc::new(DecodedTexture::from_rgba8(
                        stamp.width,
                        stamp.height,
                        Vec::new(),
                    ));
                    let mut cache = self.decoded();
                    if cache.generation == generation {
                        self.keep_imagery(&mut cache, &scheme, c, Arc::clone(&tex));
                    }
                    return Ok((c, tex));
                }
                // Remembered as absent: stand on the parent, exactly as the
                // `NotFound` below would. At the coarsest level the bytes
                // path says what it says.
                tuile_core::storage::Stamp::Absent if c.level > scheme.minimum_level => {
                    let parent = ImageryCoord {
                        level: c.level - 1,
                        x: c.x / 2,
                        y: c.y / 2,
                    };
                    return Box::pin(self.fetch_imagery(parent)).await;
                }
                _ => {}
            }
        }
        // As above: the cache hit is already gone, and the recursion onto the
        // parent below counts itself when it reaches this line.
        let m = tuile_core::metrics::metrics();
        m.texture_fetches.inc();
        let outstanding =
            tuile_core::metrics::InFlight::new(&m.textures_in_flight, &m.texture_fetch_seconds);
        // Bytes, not the decoded texture: decoding is CPU work, and taking it
        // here rather than inside the provider is what lets it leave this
        // thread together with the resampling below, in one hop.
        let fetched = self.imagery_bytes(&scheme, c).await;
        drop(outstanding);
        match fetched {
            Ok(fetched) => {
                tracing::debug!(z = c.level, x = c.x, y = c.y, "imagery tile");
                self.provenance.fetched_imagery(c, &fetched.value);
                // Decode, resample and inspect in a single offloaded job. All
                // three are pure CPU over the same buffer, so splitting them
                // would only pay the hop three times and undo the locality.
                //
                // As data rather than a closure, so that in a browser it can
                // cross to a decode worker: see `jobs`.
                let off = Arc::clone(&self.offload);
                let job = jobs::ImageryDecode {
                    bytes: fetched.value,
                    scheme,
                    coord: c,
                    stored: self.opts.stored_imagery,
                    plan: self.opts.imagery_plan.get(),
                };
                let (tex, black) = offload::run_portable(off.as_ref(), job)
                    .await
                    .map_err(|e| LoadError::Failed(format!("imagery {c:?}: {e}")))?;
                // Held as the consumer's plan says: what the budget charges.
                let tex = Arc::new(tex.planned(self.opts.imagery_plan.get()));
                // A provider's way of saying "no data here" is often an opaque
                // black image. Draped on good geometry that is a clean black
                // quad with sharp edges — visually identical to a rendering
                // fault, and argued about as one. Named at the source instead.
                if black {
                    tracing::warn!(
                        z = c.level,
                        x = c.x,
                        y = c.y,
                        "BLACK IMAGERY: the provider returned an opaque black tile"
                    );
                    tuile_core::metrics::metrics().black_textures.inc();
                }
                {
                    let mut cache = self.decoded();
                    if cache.generation == generation {
                        self.keep_imagery(&mut cache, &scheme, c, Arc::clone(&tex));
                    }
                }
                Ok((c, tex))
            }
            // Absent at this zoom: stand on the parent. Recursing returns the
            // ancestor's own coord, so it is cached and shared under that coord
            // rather than copied once per descendant that wanted it.
            Err(RasterError::Fetch(FetchError::NotFound(_))) if c.level > scheme.minimum_level => {
                let parent = ImageryCoord {
                    level: c.level - 1,
                    x: c.x / 2,
                    y: c.y / 2,
                };
                tracing::debug!(
                    z = c.level,
                    x = c.x,
                    y = c.y,
                    "imagery absent, standing on parent"
                );
                Box::pin(self.fetch_imagery(parent)).await
            }
            Err(e) => Err(LoadError::Failed(format!("imagery {c:?}: {e}"))),
        }
    }

    /// The mesh for a tile, built from its parent's when the source has none of
    /// its own.
    ///
    /// One level at a time, always, and every rung remembered. That is the whole
    /// design and both halves earn their place:
    ///
    /// - **From the parent**, because the parent's mesh already covers only the
    ///   parent's ground. Rebuilding a deep tile from a distant ancestor re-clips
    ///   that ancestor's entire mesh once per descendant, which is quadratic in
    ///   the gap; ten levels of it pinned a core at a hundred percent and stopped
    ///   the traversal getting a turn at all.
    /// - **Remembered**, because a tile's three siblings share its parent, and
    ///   its own children will share it in turn. Without the memo, walking down
    ///   from the real data is the quadratic thing wearing a different hat.
    ///
    /// The reference implementation does exactly this: `upsample` there reads
    /// `parent.data.terrainData` and waits if the parent is not ready yet.
    ///
    /// No new detail is invented. The tile has its ancestor's shape described by
    /// fewer triangles per unit of ground — the mesh stops improving where the
    /// data stops, while the imagery draped on it keeps sharpening.
    fn terrain_mesh<'a>(&'a self, coord: TileCoord) -> MeshFuture<'a> {
        Box::pin(async move {
            if let Some(mesh) = self.meshes.lock().expect("mesh cache").get(coord) {
                return Ok(mesh);
            }
            // One fetch or build per tile at a time: whoever else wants it
            // waits for that one's answer. A leader dropped — its load
            // cancelled — lets the tile go, and the next asker leads.
            loop {
                match self.building.join(coord) {
                    Turn::Wait(answer) => match answer.await {
                        Ok(answer) => return answer.map_err(LoadError::Failed),
                        Err(futures_channel::oneshot::Canceled) => continue,
                    },
                    Turn::Lead => {
                        let lead = Leading {
                            flight: &self.building,
                            key: coord,
                        };
                        let answer = self.terrain_mesh_once(coord).await.map_err(|e| match e {
                            LoadError::Failed(why) => why,
                            other => other.to_string(),
                        });
                        lead.finish(&answer);
                        return answer.map_err(LoadError::Failed);
                    }
                }
            }
        })
    }

    /// [`Self::terrain_mesh`], for the one asker that fetches or builds.
    fn terrain_mesh_once<'a>(&'a self, coord: TileCoord) -> MeshFuture<'a> {
        Box::pin(async move {
            if let Some(mesh) = self.meshes.lock().expect("mesh cache").get(coord) {
                return Ok(mesh);
            }

            // Whether the source has this tile at all. Asking the network to find
            // out costs a round trip per level and, over ground the source does
            // not reach, drowned the server in failures; availability is already
            // here and answers without one.
            let known = self.availability.range_count() > 0;
            let has_data = !known || self.availability.is_available(coord);

            let mesh = if has_data {
                // Only the network counts. A cache hit returned above and an
                // upsample below are not fetches, and folding either in would
                // make the mean say the source is fast when it is simply not
                // being asked.
                let m = tuile_core::metrics::metrics();
                m.mesh_fetches.inc();
                let outstanding =
                    tuile_core::metrics::InFlight::new(&m.meshes_in_flight, &m.mesh_fetch_seconds);
                let fetched = self.terrain_bytes(coord).await;
                drop(outstanding);
                match fetched {
                    Ok(fetched) => {
                        tracing::debug!(
                            z = coord.level,
                            x = coord.x,
                            y = coord.y,
                            kib = fetched.value.len() / 1024,
                            "terrain tile"
                        );
                        let digest = provenance::digest(&fetched.value);
                        // Decoding a quantized mesh is the same kind of work as
                        // decoding a JPEG and belongs off this thread for the
                        // same reason: nothing in it awaits, and holding the
                        // poller for its duration stalls every other load.
                        let off = Arc::clone(&self.offload);
                        let job = jobs::TerrainDecode {
                            bytes: fetched.value,
                        };
                        let decoded = offload::run_portable(off.as_ref(), job)
                            .await
                            .map_err(LoadError::Failed)?;
                        // Decoding is also what reveals which descendants exist —
                        // the source may have served these bytes from a cache, but
                        // the ranges still reach the shared availability, so
                        // refinement never stalls at a cached level.
                        self.provenance
                            .fetched_terrain(coord, decoded.header.center, digest);
                        self.reveal(coord, decoded.metadata_available.as_deref());
                        Some(Arc::new(decoded))
                    }
                    // Availability said yes and the source disagreed. Fall through
                    // and build it from the parent rather than fail the tile.
                    Err(e) if coord.level > 0 => {
                        tracing::debug!(
                            z = coord.level,
                            x = coord.x,
                            y = coord.y,
                            "terrain absent though available: {e}"
                        );
                        None
                    }
                    Err(e) => {
                        return Err(LoadError::Failed(format!(
                            "terrain {}/{}/{}: {e}",
                            coord.level, coord.x, coord.y
                        )))
                    }
                }
            } else {
                None
            };

            let mesh = match mesh {
                Some(mesh) => mesh,
                None => {
                    if coord.level == 0 {
                        return Err(LoadError::Failed(
                            "terrain 0/0/0 is missing and has no parent to stand on".into(),
                        ));
                    }
                    let parent = TileCoord::new(coord.level - 1, coord.x / 2, coord.y / 2);
                    let from = self.terrain_mesh(parent).await?;
                    let built = tuile_terrain::upsample(&from, parent, coord).ok_or_else(|| {
                        LoadError::Failed(format!(
                            "terrain {}/{}/{}: nothing of its parent covers it",
                            coord.level, coord.x, coord.y
                        ))
                    })?;
                    self.provenance
                        .upsampled_terrain(coord, parent, from.header.center);
                    let m = tuile_core::metrics::metrics();
                    m.tiles_upsampled.inc();
                    m.upsampled_by_level.inc(coord.level);
                    tracing::debug!(
                        z = coord.level,
                        x = coord.x,
                        y = coord.y,
                        "terrain upsampled from its parent"
                    );
                    Arc::new(built)
                }
            };

            self.keep_mesh(coord, Arc::clone(&mesh));
            Ok(mesh)
        })
    }

    /// Keeps a decoded mesh, noting the tiles it adds and lets go whose
    /// answers may move. A tile the source has and the terrain store holds
    /// answers from the store, decoded or not: its mesh, and those built
    /// from it, are held either way.
    fn keep_mesh(&self, coord: TileCoord, mesh: Arc<tuile_terrain::QuantizedMesh>) {
        let mut changed = Vec::new();
        self.meshes
            .lock()
            .expect("mesh cache")
            .put(coord, mesh, &mut |at, way| changed.push((at, way)));
        let known = self.availability.range_count() > 0;
        for (at, way) in changed {
            let available = !known || self.availability.is_available(at);
            if !(available && self.terrain.holds(at)) {
                self.changes.note(store_log::Change::Mesh(at, way));
            }
        }
    }

    /// The ground under one texel of a 256-texel tile at terrain level 0:
    /// what the budget's imagery levels count from. A budget level `L` is the
    /// imagery whose texels are this halved `L` times, so it compares with
    /// the terrain's own level (spec §4).
    fn texel_l0(&self) -> f64 {
        WGS84_A * std::f64::consts::TAU / (256.0 * self.scheme.root_tiles_x as f64)
    }

    /// How many levels the provider's grid sits above the budget's at
    /// latitude `lat`: the provider's level for budget level `L` is
    /// `L + offset`, clamped to what it serves.
    fn level_offset(&self, scheme: &raster::TilingScheme, lat: f64) -> i64 {
        let lat_factor = match scheme.projection {
            raster::Projection::WebMercator => lat.cos(),
            raster::Projection::Geographic => 1.0,
        };
        let level_zero = WGS84_A * std::f64::consts::TAU * lat_factor
            / (f64::from(scheme.tile_size) * scheme.root_tiles_x as f64);
        (level_zero / self.texel_l0()).max(1e-12).log2().round() as i64
    }

    /// The provider's level for the budget's imagery level `level` over a
    /// tile at latitude `lat`.
    fn provider_level(&self, scheme: &raster::TilingScheme, level: u32, lat: f64) -> u32 {
        (i64::from(level) + self.level_offset(scheme, lat)).clamp(
            i64::from(scheme.minimum_level),
            i64::from(scheme.maximum_level),
        ) as u32
    }

    /// A terrain tile's rectangle, as the imagery's geography.
    fn georect(&self, coord: TileCoord) -> GeoRect {
        let rect = self.scheme.tile_rect(coord);
        GeoRect {
            west: rect.west,
            south: rect.south,
            east: rect.east,
            north: rect.north,
        }
    }

    /// Whether a tile's mesh is in hand without the network: decoded here,
    /// held by the terrain store, or — past the source's data — built from
    /// an ancestor that is.
    fn mesh_held(&self, coord: TileCoord) -> bool {
        let mut at = coord;
        // Upsampled tiles climb to the data they come from; the source's
        // data is a handful of levels from any tile.
        for _ in 0..32 {
            if self
                .meshes
                .lock()
                .map(|m| m.get(at).is_some())
                .unwrap_or(false)
            {
                return true;
            }
            let known = self.availability.range_count() > 0;
            if !known || self.availability.is_available(at) {
                return self.terrain.holds(at);
            }
            if at.level == 0 {
                return false;
            }
            at = TileCoord::new(at.level - 1, at.x / 2, at.y / 2);
        }
        false
    }

    /// The imagery tiles `id`'s drape at the budget's `level` would be made
    /// of — the floor under it first — without fetching any.
    fn drape_coords(&self, id: TileId, level: u32) -> Vec<ImageryCoord> {
        let (z, x, y) = id.terrain_coord();
        let rect = self.georect(TileCoord::new(z, x, y));
        let scheme = self.imagery.tiling_scheme();
        let lat = 0.5 * (rect.south + rect.north);
        let mosaic = scheme.mosaic_at_level(
            &rect,
            self.provider_level(&scheme, level, lat),
            self.opts.imagery_slots.get() - 1,
        );
        let mut coords = vec![self.opts.floor_under(&scheme, &rect, &self.cache)];
        coords.extend(mosaic.tiles());
        coords
    }

    /// Composes `id`'s drape of `layer` at the budget's imagery `level`,
    /// whole and from one imagery set.
    ///
    /// A drape is several fetches, and a provider that can be switched
    /// ([`ImageryProvider::generation`]) may be switched between two of them.
    /// The result would be a tile half in each picture — and, when the two
    /// are cut on different grids, tiles of one placed by the other's
    /// rectangles. So the generation is read before and after, and a drape
    /// that straddled a switch is thrown away and built again; every layer of
    /// the one that is kept is stamped with the generation it belongs to.
    async fn compose(
        &self,
        id: TileId,
        layer: LayerId,
        level: u32,
    ) -> Result<DrapeContent, LoadError> {
        let (z, x, y) = id.terrain_coord();
        let rect = self.georect(TileCoord::new(z, x, y));
        loop {
            let generation = self.imagery.generation();
            let mut drape = self.compose_once(&rect, id, layer, level).await?;
            if self.imagery.generation() == generation {
                for tile in &mut drape.tiles {
                    // Truncated on purpose: an identity among the imagery sets
                    // one session holds at once, of which there are two.
                    tile.source = generation as u32;
                }
                return Ok(drape);
            }
        }
    }

    async fn compose_once(
        &self,
        rect: &GeoRect,
        id: TileId,
        layer: LayerId,
        budget_level: u32,
    ) -> Result<DrapeContent, LoadError> {
        // The cache is turned to the imagery being served before anything
        // looks into it — `floor_under` below does, by its own lock.
        drop(self.decoded());
        let scheme = self.imagery.tiling_scheme();
        // The level the budget asked for, on the provider's grid: chosen per
        // tile from how large a texel looks on screen, independently of the
        // terrain level (I10) — not from a global altitude, not frozen when
        // the mesh loads.
        let lat = 0.5 * (rect.south + rect.north);
        let level = self.provider_level(&scheme, budget_level, lat);
        // One slot is reserved for a coarse layer underneath everything else.
        //
        // What **one draw** binds, not what the passes could carry.
        //
        // The passes exist so that a mosaic is never silently truncated, which
        // is a correctness property. Spending them on a *finer* mosaic is a
        // different decision, and a much larger one: raising this to
        // `imagery_layer_budget` took the ceiling from 24 layers to 99, which is
        // up to three levels deeper, sixteen times the imagery tiles to fetch
        // and upload for the same ground, and — measured — a globe that spent
        // its time waiting rather than drawing. If a deeper drape is wanted it
        // has to be asked for on its own, with the memory and the fetch count
        // in front of it.
        let mosaic = scheme.mosaic_at_level(rect, level, self.opts.imagery_slots.get() - 1);
        let terrain_level = id.terrain_coord().0;
        // The four numbers that decide how sharp the ground gets, in the order
        // they constrain each other: what the terrain's own error asks for, what
        // the camera's altitude asks for, what was requested, and what survived
        // the layer budget. A gap between the last two is the budget coarsening
        // the request back, which is the thing to watch — it is the only reason
        // imagery cannot go deeper than the mesh it drapes.
        tracing::debug!(
            terrain_level,
            budget_level,
            wanted = level,
            got = mosaic.level,
            tiles = mosaic.tile_count(),
            "imagery level"
        );
        let coords = mosaic.tiles();

        // The floor of the stack, decided before anything is asked for.
        //
        // It was chosen further down, between the mosaic's fetch and its own —
        // which was fine while every layer was going to be fetched anyway. It
        // has to move up here because it is part of what identifies the drape,
        // and the identity has to exist before the first request.
        let floor = self.opts.floor_under(&scheme, rect, &self.cache);

        // Already composed by the consumer: fetch nothing, decode nothing.
        //
        // The identity is taken over the coords about to be REQUESTED, in the
        // order the layers would be stacked — floor first, then the mosaic —
        // because that is the order the consumer hashed them in when it
        // composed the drape it is holding. See `raster::drape_identity` for
        // why the two agree, and for the one direction in which they may not.
        if let Some(held) = &self.opts.held_drape {
            let identity = tuile_core::raster::drape_identity(
                std::iter::once(floor).chain(coords.iter().copied()),
                self.opts.composed_at,
            );
            if held.holds(id.0, identity) {
                // `withheld` is what stops an empty drape from being read as
                // "this ground has no picture". The consumer must resolve it;
                // there is no fallback here, deliberately.
                tuile_core::metrics::metrics().drapes_withheld.inc();
                return Ok(DrapeContent {
                    layer,
                    level: budget_level,
                    tiles: Vec::new(),
                    floor_tiles: 0,
                    identity,
                    withheld: true,
                });
            }
        }

        let fetched =
            futures_util::future::join_all(coords.iter().map(|c| self.fetch_imagery(*c))).await;

        let mut layers = Vec::with_capacity(coords.len() + 1);

        // The floor of the stack: one coarse tile that covers this whole
        // rectangle on its own, laid down before anything else.
        //
        // Layers blend in order — each one paints only where its coverage
        // rectangle says, over whatever is already there — so the sharp mosaic
        // still decides every pixel it actually has. What changes is the pixels
        // it does *not* have: a tile the provider never served, or served as an
        // opaque black "no data" image, used to leave the ground bare, and a
        // clean black quad over good terrain is indistinguishable from a
        // rendering fault. Now the coarse tile shows through: softer ground,
        // which is what a stand-in should look like.
        //
        // It costs one binding out of twelve and almost no memory: a tile
        // spanning a whole region is shared by every terrain tile inside it, so
        // it is fetched once and drawn by hundreds. It is also, for the same
        // reason, the tile most likely to be resident already.
        match self.fetch_imagery(floor).await {
            Ok((served, texture)) => {
                // The floor covers the whole tile on its own, so there is no
                // neighbour to share an edge with and `placed` is the right
                // call: it derives the one rectangle it needs.
                let layer = raster::ImageryLayer::placed(
                    served,
                    texture,
                    rect,
                    &to_the_pole(
                        scheme.tile_rect(floor),
                        floor,
                        scheme.tiles_at(floor.level).1,
                    ),
                );
                if layer.is_visible() {
                    layers.push(layer);
                }
            }
            // Not fatal: the sharp mosaic may well cover everything, and
            // failing the whole tile because its safety net is missing would
            // trade a soft patch for no patch at all.
            Err(e) => tracing::warn!(
                z = floor.level,
                x = floor.x,
                y = floor.y,
                "no coarse imagery under this tile: {e}"
            ),
        }

        // The mosaic's coverage rectangles, computed once for the whole grid so
        // neighbours share an edge **to the bit**. Deriving each one separately
        // from geography rounds the shared edge twice, and a fragment landing
        // between the two values is covered by neither — a hairline grid over
        // otherwise perfect imagery. See `ImageryMosaic::coverage`.
        let coverage = mosaic.coverage(&scheme, rect);
        let floor_layers = layers.len();
        let floor_tiles = floor_layers;
        let mut dropped = 0usize;
        // `zip` stops at the shorter of the two and says nothing. If the mosaic
        // ever hands back fewer rectangles than tiles, the surplus tiles are
        // fetched, decoded, uploaded — and then silently never placed.
        if coverage.len() != fetched.len() {
            tracing::warn!(
                z = terrain_level,
                tiles = fetched.len(),
                rectangles = coverage.len(),
                "MOSAIC MISMATCH: zip drops the surplus without a word"
            );
        }
        for (got, covers) in fetched.into_iter().zip(coverage) {
            let (served, texture) = got?;
            let layer = raster::ImageryLayer::substituted(
                served,
                texture,
                rect,
                // The served tile's own rectangle, with its outer row reaching
                // to the pole. Web Mercator stops at ±85° and always will:
                // the projection sends the caps to infinity. Terrain does not
                // stop there, so a geometry tile reaching past the limit had
                // fragments outside *every* layer's source rectangle and kept
                // the material's base colour — a white disc centred on the pole
                // with its edge exactly at 85°, which is one of the more legible
                // bugs this engine has produced.
                &to_the_pole(
                    scheme.tile_rect(served),
                    served,
                    scheme.tiles_at(served.level).1,
                ),
                covers,
            );
            // A tile the mosaic's bounding box included but the rectangle only
            // touches contributes no pixels and would still cost a binding.
            if layer.is_visible() {
                layers.push(layer);
            } else {
                dropped += 1;
            }
        }

        // **The mosaic contributed nothing, and the floor is holding the tile
        // on its own.**
        //
        // Not the same fault as no imagery at all, which is counted just below
        // just below. Here the tile *has* a layer, so every
        // instrument reads it as covered — and what it has is one coarse image
        // stretched over ground it does not describe, which at any depth is a
        // rectangle of flat colour with straight tile-aligned edges. That is a
        // different artefact from the ragged organic outlines of two surfaces
        // fighting, and it was mistaken for one.
        //
        // A rectangle that fails `is_visible` is inverted or empty, which the
        // arithmetic in `ImageryMosaic::coverage` should never produce for a
        // rect the mosaic was built from — so reaching here at all says the
        // geometry tile's rectangle is degenerate or crosses the antimeridian,
        // which `rectangle_from_obb` states is out of scope.
        if layers.len() == floor_layers {
            tracing::warn!(
                z = terrain_level,
                west = rect.west,
                south = rect.south,
                east = rect.east,
                north = rect.north,
                dropped,
                wanted = level,
                got = mosaic.level,
                mosaic_tiles = mosaic.tile_count(),
                "MOSAIC LOST over this tile: one layer covers all of it, so the coverage \
                 view reads green where it should read blue, and the ground is one \
                 flat colour"
            );
        }

        // The uv set is the TILE's own space, and the mesh already stated it —
        // `to_decoded` carries it straight from the quantized mesh. It used to
        // be recovered here instead, by projecting every vertex back to a
        // longitude and measuring it against the tile's rectangle, which is
        // slower and tears at the antimeridian: see `tuile_terrain::surface_uvs`.
        // Every layer maps out of that space by its own affine transform, so the
        // vertices carry no imagery level and a layer can be swapped without
        // touching the geometry.
        // Ground with no imagery over it at all, painted so it cannot be
        // mistaken for anything else.
        //
        // The default base colour is white, so such a tile renders as a pale
        // patch — indistinguishable from haze, from a snowfield, or from a
        // texture that simply has not arrived. A colour no aerial photograph
        // contains says instead: *this tile was drawn, and nothing covered it*.
        // Which is a different fault from a tile that was never selected, and
        // the two were impossible to tell apart on screen.
        if layers.is_empty() {
            tracing::warn!(
                z = terrain_level,
                west = rect.west,
                south = rect.south,
                east = rect.east,
                north = rect.north,
                wanted = level,
                got = mosaic.level,
                mosaic_tiles = mosaic.tile_count(),
                "NO IMAGERY over this tile: the drape is empty"
            );
            tuile_core::metrics::metrics().tiles_without_imagery.inc();
        }

        // **Do the layers actually reach the ground, or merely exist?**
        //
        // Every count above asks whether a layer is *present*. None asks whether
        // it covers, and those are different questions: the shader masks each
        // layer to its own coverage rectangle, so a tile can hold a full set of
        // visible layers and still show `UNCOVERED_GROUND` on every fragment.
        // That is a rectangle of flat dark blue with straight tile-aligned
        // edges — reported from the viewer, and invisible to every instrument
        // here, because `layers` was neither empty nor short.
        //
        // The union must reach both corners. `placed` gives the floor the whole
        // tile with `EDGE_REACH` past each side, so this can only fail if the
        // floor is missing *and* the mosaic's own rectangles fall short.
        if !layers.is_empty() {
            let (mut umin, mut vmin) = (f32::INFINITY, f32::INFINITY);
            let (mut umax, mut vmax) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
            for l in &layers {
                umin = umin.min(l.coverage[0]);
                vmin = vmin.min(l.coverage[1]);
                umax = umax.max(l.coverage[2]);
                vmax = vmax.max(l.coverage[3]);
            }
            if umin > 0.0 || vmin > 0.0 || umax < 1.0 || vmax < 1.0 {
                tracing::warn!(
                    z = terrain_level,
                    layers = layers.len(),
                    umin,
                    vmin,
                    umax,
                    vmax,
                    "COVERAGE SHORT: the layers span u {umin}..{umax}, v {vmin}..{vmax} \
                     of a tile that is 0..1 in both — the rest draws UNCOVERED_GROUND"
                );
            }
        }
        let identity = tuile_core::raster::drape_identity(
            layers.iter().map(|l| l.coord),
            self.opts.composed_at,
        );
        Ok(DrapeContent {
            layer,
            level: budget_level,
            tiles: layers,
            floor_tiles,
            identity,
            withheld: false,
        })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> TileLoader
    for PlanetaryLoader<T, I>
{
    /// Pulls the coarse pyramid into the store before the camera asks for it.
    ///
    /// Terrain and imagery both, level by level from the top, because the
    /// fallback chain needs both: a mesh with no imagery draws bare,
    /// imagery with no mesh draws nothing at all.
    ///
    /// Availability is consulted first for terrain, so ground the source does
    /// not cover costs no request. Imagery has no such oracle, and a level the
    /// provider declines is simply a miss — which the store remembers as
    /// cheaply as a hit.
    ///
    /// Bounded concurrency: this competes with the loads the picture is waiting
    /// on, and a warm-up that starves the first frame has defeated itself.
    /// The imagery being served: a switch of imagery is a change of what
    /// every tile's content is, which is exactly what an epoch says.
    fn epoch(&self) -> u64 {
        self.imagery.generation()
    }

    async fn warm_up(&self, through_level: u32) {
        use futures_util::stream::StreamExt;

        /// Few enough to leave the visible frontier its share of the pipe.
        const AT_ONCE: usize = 8;

        let started = tuile_core::metrics::stamp();
        let scheme = self.imagery.tiling_scheme();
        let mut terrain_tiles = 0u64;
        let mut imagery_tiles = 0u64;

        for level in 0..=through_level {
            // Terrain, over the geographic scheme the tree uses.
            let (tx, ty) = (
                self.scheme.root_tiles_x << level,
                self.scheme.root_tiles_y << level,
            );
            let coords: Vec<TileCoord> = (0..ty)
                .flat_map(|y| (0..tx).map(move |x| TileCoord::new(level, x, y)))
                .filter(|c| {
                    self.availability.range_count() == 0 || self.availability.is_available(*c)
                })
                .collect();
            terrain_tiles += coords.len() as u64;
            futures_util::stream::iter(coords)
                .for_each_concurrent(AT_ONCE, |c| async move {
                    let _ = self.terrain_bytes(c).await;
                })
                .await;

            // Imagery, over its own scheme, which may start deeper than 0.
            if self.opts.no_imagery || level < scheme.minimum_level {
                continue;
            }
            let (ix, iy) = scheme.tiles_at(level);
            let coords: Vec<ImageryCoord> = (0..iy)
                .flat_map(|y| (0..ix).map(move |x| ImageryCoord { level, x, y }))
                .collect();
            imagery_tiles += coords.len() as u64;
            futures_util::stream::iter(coords)
                .for_each_concurrent(AT_ONCE, |c| async move {
                    let _ = self.imagery_bytes(&scheme, c).await;
                })
                .await;
        }

        tracing::info!(
            through_level,
            terrain_tiles,
            imagery_tiles,
            seconds = started.elapsed().as_secs_f32(),
            "coarse pyramid warmed"
        );
    }

    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let started = tuile_core::metrics::stamp();
        let (z, x, y) = id.terrain_coord();
        let coord = TileCoord::new(z, x, y);
        let qm = self.terrain_mesh(coord).await?;
        // The header already carries the tile's relief, so the surface a camera
        // is clamped against sharpens for free as the globe refines.
        self.heights.record(coord, qm.header.max_height);
        let rect = self.scheme.tile_rect(coord);
        // Skirts, at the size both reference implementations use. `mesh.edges`
        // arrives in whatever order the server wrote it, and `append_skirts`
        // sorts each edge before stitching it into a wall.
        let mut content = to_ground(&qm, &rect);
        if !self.opts.no_imagery {
            for mesh in &mut content.meshes {
                // Terrain owns no base-colour texture: its drapes are the
                // ground, loaded on their own (`load_drape`). What shows
                // wherever no layer reaches is the colour of unremarkable
                // ground, so a gap of one pixel costs a pixel nobody notices
                // instead of announcing itself — `D` cycles to the coverage
                // view, which paints magenta there.
                mesh.material.base_color_texture = None;
                mesh.material.base_color_factor = UNCOVERED_GROUND;
            }
        }
        tuile_core::metrics::metrics()
            .load_seconds
            .record(started.elapsed());
        Ok(Loaded::Content(content))
    }

    /// The imagery that drapes a terrain tile: one layer, the provider's,
    /// reaching as deep as it serves over the tile's latitude.
    fn drapes(&self, id: TileId) -> Option<DrapeReach> {
        if self.opts.no_imagery {
            return None;
        }
        let (z, x, y) = id.terrain_coord();
        let rect = self.georect(TileCoord::new(z, x, y));
        let scheme = self.imagery.tiling_scheme();
        let lat = 0.5 * (rect.south + rect.north);
        let deepest =
            (i64::from(scheme.maximum_level) - self.level_offset(&scheme, lat)).max(0) as u32;
        Some(DrapeReach {
            texel_l0_m: self.texel_l0(),
            rect: tuile_core::budget::GeoRect {
                west: rect.west,
                south: rect.south,
                east: rect.east,
                north: rect.north,
            },
            layers: vec![LayerReach {
                layer: LayerId(0),
                max_level: deepest,
            }],
        })
    }

    async fn load_drape(
        &self,
        id: TileId,
        layer: LayerId,
        level: u32,
    ) -> Result<DrapeContent, LoadError> {
        let started = tuile_core::metrics::stamp();
        let drape = self.compose(id, layer, level).await?;
        tuile_core::metrics::metrics()
            .load_seconds
            .record(started.elapsed());
        Ok(drape)
    }

    /// Whether the terrain store holds a tile's mesh (or the ancestor it is
    /// built from, past the source's data), and whether the imagery store
    /// holds every tile of a drape: asked of their indexes, never read.
    fn holds(&self, r: ResourceId) -> Presence {
        let held = match r {
            ResourceId::Mesh(t) => {
                let (z, x, y) = t.terrain_coord();
                self.mesh_held(TileCoord::new(z, x, y))
            }
            ResourceId::Drape { tile, level, .. } => {
                // The coords first: choosing the floor reads the cache under
                // its own lock, which must not be held here yet.
                let coords = self.drape_coords(tile, level);
                let cache = self.decoded();
                coords
                    .into_iter()
                    .all(|c| cache.has(&c) || self.imagery.holds(c))
            }
        };
        if held {
            Presence::Stored
        } else {
            Presence::Absent
        }
    }

    /// Noted as the loader changes what [`holds`](TileLoader::holds) reads
    /// — a decoded mesh or imagery tile kept or let go, a store's index
    /// changed by a fetch through this loader — and polled for what changes
    /// without it: the imagery switched, the availability grown.
    ///
    /// Not seen: an entry a store drops on its own (a disk tier's eviction,
    /// a store suspended, another writer's put). An answer it makes wrong is
    /// a hint the budget believes until the resource is loaded — its read
    /// then goes to the network under the local quota, once, and the load
    /// puts it back — or until a change near it is noted.
    fn store_events(&self) -> u64 {
        self.changes.count(self.imagery.generation(), |from, out| {
            self.availability.added_since(from, out)
        })
    }

    fn store_changes(
        &self,
        since: u64,
        asked: &[(ResourceId, Presence)],
        touched: &mut Vec<ResourceId>,
    ) -> u64 {
        self.store_events();
        self.changes.touched(since, asked, self, touched)
    }
}

/// What the store events read of the loader as it is now.
impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> store_log::Reads
    for PlanetaryLoader<T, I>
{
    fn rect(&self, t: TileCoord) -> GeoRect {
        self.georect(t)
    }

    /// As [`PlanetaryLoader::mesh_held`] climbs.
    fn stops(&self, t: TileCoord) -> bool {
        let decoded = self
            .meshes
            .lock()
            .map(|m| m.get(t).is_some())
            .unwrap_or(false);
        decoded || self.availability.range_count() == 0 || self.availability.is_available(t)
    }

    /// As [`PlanetaryLoader::drape_coords`] chooses them.
    fn levels(&self, t: TileCoord, level: u32) -> (u32, u32, u32) {
        let rect = self.georect(t);
        let scheme = self.imagery.tiling_scheme();
        let covering = scheme.containing_tile(&rect).level;
        let provider = self.provider_level(&scheme, level, 0.5 * (rect.south + rect.north));
        // A level of margin under the covering one: where the mosaic stops
        // coarsening is counted by its own ranges, which may take in a tile
        // the covering one's edge only touches.
        (
            FLOOR_LEVEL.max(scheme.minimum_level).min(covering),
            provider.min(covering).saturating_sub(1),
            provider,
        )
    }
}

/// The terrain quadtree of a parsed `layer.json`, as a boxed [`TileTree`].
pub fn terrain_tree(layer: LayerJson) -> Box<dyn TileTree> {
    Box::new(TerrainTree::new(layer))
}

/// Crosses an already-resolved terrain source and imagery provider into a
/// `(tree, loader, heights)` ready for `tuile_core::runtime::in_process_with`.
/// `layer` is the terrain's `layer.json` (the host resolves it). The loader
/// loads each terrain tile's mesh and, as resources of their own, its drapes
/// at the imagery level the session's budget asks for; the returned
/// [`TerrainHeights`] is the surface to hand a camera controller so it stays
/// above the ground. Pure: the host has already chosen backends + transport.
pub fn globe<T, I>(
    terrain: T,
    imagery: I,
    layer: LayerJson,
    opts: GlobeOptions,
) -> (Box<dyn TileTree>, Arc<dyn TileLoader>, Arc<TerrainHeights>)
where
    T: TerrainSource + 'static,
    I: ImageryProvider + 'static,
{
    globe_on(terrain, imagery, layer, opts, offload::inline())
}

/// [`globe`], with the decode and resample work placed explicitly.
///
/// A native host should pass an [`Offload`] backed by a real pool. `globe`'s
/// default runs that work on whichever thread polls the server, and since the
/// server is a single future that means one core doing the work of every load
/// in flight — measured at sixty-four loads outstanding and one thread busy.
pub fn globe_on<T, I>(
    terrain: T,
    imagery: I,
    layer: LayerJson,
    opts: GlobeOptions,
    offload: Arc<dyn Offload>,
) -> (Box<dyn TileTree>, Arc<dyn TileLoader>, Arc<TerrainHeights>)
where
    T: TerrainSource + 'static,
    I: ImageryProvider + 'static,
{
    let (tree, loader, heights, _) = globe_with_provenance(terrain, imagery, layer, opts, offload);
    (tree, loader, heights)
}

/// What [`globe_with_provenance`] hands back: [`globe_on`]'s three, and the
/// provenance record the loader fills.
pub type GlobeWithProvenance = (
    Box<dyn TileTree>,
    Arc<dyn TileLoader>,
    Arc<TerrainHeights>,
    Arc<Provenance>,
);

/// [`globe_on`], with the record of what each tile is made from: for a
/// consumer that will want to build the same tiles again from the same
/// source tiles, and so has to know which they were.
pub fn globe_with_provenance<T, I>(
    terrain: T,
    imagery: I,
    layer: LayerJson,
    opts: GlobeOptions,
    offload: Arc<dyn Offload>,
) -> GlobeWithProvenance
where
    T: TerrainSource + 'static,
    I: ImageryProvider + 'static,
{
    let provenance = Arc::new(Provenance::default());
    // One growing availability, shared by the tree (reader) and loader (writer):
    // the loader folds in each tile's `metadata` ranges, the tree refines on them.
    let scheme = GeographicTilingScheme::default();
    let availability = Arc::new(Availability::from_layer(
        &layer,
        scheme.root_tiles_x,
        scheme.root_tiles_y,
    ));
    // Divide the terrain as deep as the *imagery* can still be sharp on it, and
    // no deeper.
    //
    // The two are not independent: a tile carries imagery about a level or two
    // finer than its own match, so the only way to show the sharpest photography
    // a provider has is for the tiles under it to be smaller than the terrain
    // data goes. Past the imagery's own maximum, dividing buys smaller tiles and
    // identical pictures.
    //
    // This belongs here rather than in the tree because it is the one place that
    // holds both: the tree knows nothing about imagery, and the imagery provider
    // knows nothing about the quadtree it drapes.
    let deepest_useful = opts
        .max_level
        .unwrap_or_else(|| imagery.tiling_scheme().maximum_level);
    let tree: Box<dyn TileTree> = Box::new(
        TerrainTree::with_availability(layer, Arc::clone(&availability))
            .with_max_level(deepest_useful),
    );
    let heights = Arc::new(TerrainHeights::new(scheme));
    let loader: Arc<dyn TileLoader> = Arc::new(PlanetaryLoader {
        terrain,
        imagery,
        scheme,
        opts,
        cache: Mutex::new(ImageryCache::new(512)),
        meshes: Mutex::new(MeshCache::new(512)),
        fetching: ImageryInFlight::default(),
        building: TerrainInFlight::default(),
        changes: store_log::StoreLog::default(),
        availability,
        provenance: Arc::clone(&provenance),
        heights: Arc::clone(&heights),
        offload,
    });
    (tree, loader, heights, provenance)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader compresses nothing unless its consumer asks.
    #[test]
    fn imagery_is_held_as_rgba8_by_default() {
        assert_eq!(
            GlobeOptions::default().imagery_plan.get(),
            tuile_core::texels::ImageryPlan::RGBA8
        );
        assert_eq!(
            GlobeOptions::default().stored_imagery,
            tuile_core::texels::StoredPlan::OFF
        );
    }

    /// One client choice sets both plans, from the same device; Off leaves
    /// the defaults.
    #[test]
    fn one_choice_sets_both_imagery_plans() {
        use tuile_core::texels::{GpuTextureCaps, ImageryCompression};
        let caps = GpuTextureCaps {
            bc: true,
            astc: false,
            compute: false,
        };
        for choice in [
            ImageryCompression::Off,
            ImageryCompression::Auto,
            ImageryCompression::Format(tuile_core::texels::BlockFormat::Bc7),
        ] {
            let o = GlobeOptions::default().with_imagery_compression(choice, caps);
            assert_eq!(o.imagery_plan.get(), choice.plan(caps), "{choice:?}");
            assert_eq!(o.stored_imagery, choice.stored(caps), "{choice:?}");
        }
        let off = GlobeOptions::default().with_imagery_compression(ImageryCompression::Off, caps);
        assert_eq!(
            off.imagery_plan.get(),
            GlobeOptions::default().imagery_plan.get()
        );
        assert_eq!(off.stored_imagery, GlobeOptions::default().stored_imagery);
    }

    /// An imagery provider that refuses everything and counts being asked.
    ///
    /// Refusing rather than serving is deliberate: this test's whole claim is
    /// that nothing is requested, so a provider that could satisfy a request
    /// would let the test pass for the wrong reason.
    struct CountingImagery(std::sync::atomic::AtomicUsize);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl raster::ImageryProvider for CountingImagery {
        fn tiling_scheme(&self) -> raster::TilingScheme {
            raster::TilingScheme::web_mercator()
        }

        async fn fetch_tile_bytes(
            &self,
            coord: ImageryCoord,
        ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, raster::RasterError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(raster::RasterError::Image(format!(
                "{coord:?} was requested"
            )))
        }
    }

    /// A terrain source that has nothing, says so after a turn of the
    /// executor (so that loads overlap), and counts every request.
    struct Barren(std::sync::atomic::AtomicUsize);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl tuile_terrain::TerrainSource for Barren {
        async fn fetch_tile(
            &self,
            coord: TileCoord,
        ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError>
        {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut turned = false;
            futures_util::future::poll_fn(|cx| {
                if turned {
                    std::task::Poll::Ready(())
                } else {
                    turned = true;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            Err(tuile_terrain::TerrainSourceError::no_such_tile(format!(
                "{coord:?}"
            )))
        }
    }

    /// **One terrain tile is asked for once at a time**, however many loads
    /// need it together: two siblings built from the same ancestors fetch
    /// each ancestor once between them — four requests, not six. A browser
    /// showed one terrain tile requested 23 times, many of them at once.
    #[test]
    fn concurrent_loads_ask_for_a_shared_ancestor_once() {
        let scheme = GeographicTilingScheme::default();
        let loader = PlanetaryLoader {
            terrain: Barren(std::sync::atomic::AtomicUsize::new(0)),
            imagery: CountingImagery(std::sync::atomic::AtomicUsize::new(0)),
            scheme,
            opts: GlobeOptions::default(),
            cache: Mutex::new(ImageryCache::new(8)),
            meshes: Mutex::new(MeshCache::new(8)),
            fetching: ImageryInFlight::default(),
            building: TerrainInFlight::default(),
            changes: store_log::StoreLog::default(),
            availability: Arc::new(tuile_terrain::Availability::default()),
            provenance: Arc::default(),
            heights: Arc::new(TerrainHeights::new(scheme)),
            offload: offload::inline(),
        };
        let (a, b) = futures_executor::block_on(futures_util::future::join(
            loader.terrain_mesh(TileCoord::new(2, 0, 0)),
            loader.terrain_mesh(TileCoord::new(2, 1, 0)),
        ));
        assert!(a.is_err() && b.is_err(), "the source has nothing");
        assert_eq!(
            loader.terrain.0.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "two siblings, their parent and the root, once each"
        );
    }

    /// A terrain source that is never reached: `drape` does not touch it.
    struct NoTerrain;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl tuile_terrain::TerrainSource for NoTerrain {
        async fn fetch_tile(
            &self,
            _coord: TileCoord,
        ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError>
        {
            unreachable!("drape never asks for terrain")
        }
    }

    fn a_loader(held: Option<HeldDrape>) -> PlanetaryLoader<NoTerrain, CountingImagery> {
        let scheme = GeographicTilingScheme::default();
        PlanetaryLoader {
            terrain: NoTerrain,
            imagery: CountingImagery(std::sync::atomic::AtomicUsize::new(0)),
            scheme,
            opts: GlobeOptions {
                deterministic_floor: true,
                composed_at: 2048,
                held_drape: held,
                ..GlobeOptions::default()
            },
            cache: Mutex::new(ImageryCache::new(8)),
            meshes: Mutex::new(MeshCache::new(8)),
            fetching: ImageryInFlight::default(),
            building: TerrainInFlight::default(),
            changes: store_log::StoreLog::default(),
            availability: Arc::new(tuile_terrain::Availability::default()),
            provenance: Arc::default(),
            heights: Arc::new(TerrainHeights::new(scheme)),
            offload: offload::inline(),
        }
    }

    /// The tile `some_ground` is.
    const GROUND: TileCoord = TileCoord {
        level: 8,
        x: 130,
        y: 90,
    };

    fn ground_id() -> TileId {
        TileId::from_terrain(GROUND.level, GROUND.x, GROUND.y)
    }

    fn drape_once(loader: &PlanetaryLoader<NoTerrain, CountingImagery>) -> Option<DrapeContent> {
        futures_executor::block_on(loader.compose(ground_id(), LayerId(0), 9)).ok()
    }

    /// An imagery set that is one flat shade everywhere, on the geographic
    /// grid.
    struct Painted(u8);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl raster::ImageryProvider for Painted {
        fn tiling_scheme(&self) -> raster::TilingScheme {
            raster::TilingScheme::geographic()
        }

        async fn fetch_tile_bytes(
            &self,
            _coord: ImageryCoord,
        ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, raster::RasterError> {
            let picture =
                image::RgbaImage::from_pixel(4, 4, image::Rgba([self.0, self.0, self.0, 255]));
            let mut png = std::io::Cursor::new(Vec::new());
            picture
                .write_to(&mut png, image::ImageFormat::Png)
                .expect("a png");
            Ok(tuile_core::fetch::Fetched::undated(bytes::Bytes::from(
                png.into_inner(),
            )))
        }
    }

    /// Imagery that waits at a gate before it answers, and counts how often
    /// it is asked.
    struct GatedImagery {
        asked: std::sync::atomic::AtomicUsize,
        gate: Arc<futures_util::lock::Mutex<()>>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl raster::ImageryProvider for GatedImagery {
        fn tiling_scheme(&self) -> raster::TilingScheme {
            raster::TilingScheme::geographic()
        }

        async fn fetch_tile_bytes(
            &self,
            coord: ImageryCoord,
        ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, raster::RasterError> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _open = self.gate.lock().await;
            Painted(40).fetch_tile_bytes(coord).await
        }
    }

    fn gated_loader(
        gate: Arc<futures_util::lock::Mutex<()>>,
    ) -> PlanetaryLoader<NoTerrain, GatedImagery> {
        let scheme = GeographicTilingScheme::default();
        PlanetaryLoader {
            terrain: NoTerrain,
            imagery: GatedImagery {
                asked: std::sync::atomic::AtomicUsize::new(0),
                gate,
            },
            scheme,
            opts: GlobeOptions::default(),
            cache: Mutex::new(ImageryCache::new(64)),
            meshes: Mutex::new(MeshCache::new(8)),
            fetching: ImageryInFlight::default(),
            building: TerrainInFlight::default(),
            changes: store_log::StoreLog::default(),
            availability: Arc::new(tuile_terrain::Availability::default()),
            provenance: Arc::default(),
            heights: Arc::new(TerrainHeights::new(scheme)),
            offload: offload::inline(),
        }
    }

    /// **Asking whether a drape is held never waits on the loader's own
    /// cache.** The budget asks the presence oracle from inside its step,
    /// synchronously; choosing a drape's floor reads the decoded-imagery cache
    /// under its lock, and the question once held that same lock around it —
    /// a session that hung on its first step, with nothing in its log.
    #[test]
    fn asking_whether_a_drape_is_held_does_not_deadlock() {
        let gate = Arc::new(futures_util::lock::Mutex::new(()));
        let loader = Arc::new(gated_loader(gate));
        let asking = Arc::clone(&loader);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let answer = asking.holds(ResourceId::drape(ground_id(), LayerId(0), 9));
            let _ = tx.send(answer);
        });
        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the presence query never returned");
        assert_eq!(answer, Presence::Absent, "nothing is stored anywhere");
    }

    const SHARED: ImageryCoord = ImageryCoord {
        level: 5,
        x: 40,
        y: 12,
    };

    /// **One imagery tile wanted by several terrain tiles at once is fetched
    /// once.** The askers that come second wait for the first one's answer,
    /// and all of them get the same texture.
    #[test]
    fn an_imagery_tile_asked_for_at_once_is_fetched_once() {
        let gate = Arc::new(futures_util::lock::Mutex::new(()));
        let loader = gated_loader(Arc::clone(&gate));
        let answers = futures_executor::block_on(async {
            let closed = gate.lock().await;
            let askers =
                futures_util::future::join_all((0..3).map(|_| loader.fetch_imagery(SHARED)));
            // Polled after every asker has been once: they are all waiting.
            let open = async {
                assert_eq!(loader.fetching.len(), 1, "one tile in flight");
                drop(closed);
            };
            futures_util::future::join(askers, open).await.0
        });
        assert_eq!(
            loader
                .imagery
                .asked
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fetched once for three askers"
        );
        let textures: Vec<_> = answers
            .into_iter()
            .map(|a| a.expect("an answer").1)
            .collect();
        assert!(textures.iter().all(|t| Arc::ptr_eq(t, &textures[0])));
        assert_eq!(loader.fetching.len(), 0, "nothing left in flight");
    }

    /// **The loader holds what it decodes as its consumer's plan says**, read
    /// at each decode: RGBA8 until a plan is set, then marked with the target
    /// the consumer's GPU encodes it into.
    #[test]
    fn decoded_imagery_follows_the_consumers_plan() {
        use tuile_core::texels::{ImageryEncode, ImageryFormat, ImageryPlan};
        let gate = Arc::new(futures_util::lock::Mutex::new(()));
        let loader = gated_loader(gate);
        let fetch = |c| {
            futures_executor::block_on(loader.fetch_imagery(c))
                .expect("a texture")
                .1
        };
        assert_eq!(fetch(SHARED).resident_format(), ImageryFormat::Rgba8);
        loader.opts.imagery_plan.set(ImageryPlan {
            target: ImageryFormat::Astc4x4,
            encode: ImageryEncode::Gpu,
        });
        let next = ImageryCoord { x: 41, ..SHARED };
        let planned = fetch(next);
        assert_eq!(planned.resident_format(), ImageryFormat::Astc4x4);
        assert!(planned.rgba8().is_some(), "still RGBA8: the GPU encodes it");
        assert_eq!(
            fetch(SHARED).resident_format(),
            ImageryFormat::Rgba8,
            "what was decoded before keeps its mark"
        );
    }

    /// **The asker that fetches may be cancelled; the others are not left
    /// waiting on a fetch nobody makes.** The next one fetches instead.
    #[test]
    fn a_cancelled_fetch_hands_the_tile_to_the_next_asker() {
        use futures_util::FutureExt;
        let gate = Arc::new(futures_util::lock::Mutex::new(()));
        let loader = gated_loader(Arc::clone(&gate));
        let closed = futures_executor::block_on(gate.lock());
        let mut first = Box::pin(loader.fetch_imagery(SHARED));
        let mut second = Box::pin(loader.fetch_imagery(SHARED));
        assert!(
            first.as_mut().now_or_never().is_none(),
            "the first one fetches"
        );
        assert!(
            second.as_mut().now_or_never().is_none(),
            "the second one waits"
        );
        // The first one's terrain tile left the view.
        drop(first);
        drop(closed);
        let answer = futures_executor::block_on(second);
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(
            loader
                .imagery
                .asked
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the second asker fetched it itself"
        );
        assert_eq!(loader.fetching.len(), 0);
    }

    /// **After a switch, a drape is the new imagery and says so.**
    ///
    /// Two things have to hold, and each was a way for a switch to show
    /// nothing. The decode cache is keyed by tile address, so without turning
    /// it over the second drape is answered from the first one's tiles — the
    /// old picture, served as the new. And a consumer shares textures by
    /// `(source, coord)`, so a layer not stamped with its generation is handed
    /// the old texture still held by every tile not yet refreshed.
    #[test]
    fn a_switch_of_imagery_drapes_the_new_set_under_a_new_identity() {
        let switch = SwitchableImagery::new(Arc::new(Painted(10)));
        let scheme = GeographicTilingScheme::default();
        let loader = PlanetaryLoader {
            terrain: NoTerrain,
            imagery: switch.clone(),
            scheme,
            opts: GlobeOptions::default(),
            cache: Mutex::new(ImageryCache::new(64)),
            meshes: Mutex::new(MeshCache::new(8)),
            fetching: ImageryInFlight::default(),
            building: TerrainInFlight::default(),
            changes: store_log::StoreLog::default(),
            availability: Arc::new(tuile_terrain::Availability::default()),
            provenance: Arc::default(),
            heights: Arc::new(TerrainHeights::new(scheme)),
            offload: offload::inline(),
        };
        let drape = || {
            let content = futures_executor::block_on(loader.compose(ground_id(), LayerId(0), 9))
                .expect("a drape");
            assert!(!content.tiles.is_empty(), "nothing was draped");
            content
                .tiles
                .iter()
                .map(|layer| (layer.source, layer.coord, layer.texture.rgba8().unwrap()[0]))
                .collect::<Vec<_>>()
        };

        let before = drape();
        assert!(before
            .iter()
            .all(|(source, _, shade)| (*source, *shade) == (0, 10)));
        assert_eq!(TileLoader::epoch(&loader), 0);

        assert_eq!(switch.switch_to(Arc::new(Painted(200))), 1);
        assert_eq!(
            TileLoader::epoch(&loader),
            1,
            "the loader's epoch is the imagery's"
        );
        let after = drape();
        // The same ground, so the same tile addresses — and none of the old
        // pixels, and none of the old identities.
        assert_eq!(
            after.iter().map(|(_, coord, _)| *coord).collect::<Vec<_>>(),
            before
                .iter()
                .map(|(_, coord, _)| *coord)
                .collect::<Vec<_>>(),
        );
        for (source, coord, shade) in &after {
            assert_eq!(
                *shade, 200,
                "{coord:?} was served from the old imagery's cache"
            );
            assert_eq!(
                *source, 1,
                "{coord:?} still carries the old imagery's identity"
            );
        }
    }

    /// A drape the consumer already holds costs **nothing**: no request leaves.
    ///
    /// Not "is served from the cache" — not asked for at all. The HTTP cache
    /// already spares the network for a second look at the same imagery tile,
    /// and it still pays the JPEG decode, the reprojection and the mosaic
    /// compose every time. Those are the expensive halves, and on an orbit that
    /// re-selects nearly the same ground every frame they are almost all of the
    /// imagery work.
    #[test]
    fn a_held_drape_is_never_requested() {
        let loader = a_loader(Some(HeldDrape::new(|_, _| true)));
        let content = drape_once(&loader).expect("a withheld drape");
        assert_eq!(
            loader.imagery.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the loader asked for imagery it was told the consumer holds"
        );
        assert!(
            content.withheld,
            "nothing was fetched and nothing says why — this is bare ground"
        );
        assert!(content.tiles.is_empty(), "no layers were attached");
    }

    /// And the same ground, unheld, does ask — otherwise the test above would
    /// pass over a drape that was never going to fetch anything anyway.
    #[test]
    fn an_unheld_drape_is_requested_as_it_always_was() {
        let loader = a_loader(Some(HeldDrape::new(|_, _| false)));
        let content = drape_once(&loader);
        assert!(
            loader.imagery.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "nothing was requested for a drape nobody holds"
        );
        // The provider refuses everything, so the drape fails; it is not
        // reported withheld.
        assert!(
            content.is_none_or(|c| !c.withheld),
            "nothing was withheld, so the flag must stay clear"
        );
    }

    /// The floor a bulk render uses depends on the ground, and on nothing else.
    ///
    /// This is the property a batch render is reproducible *because of*.
    /// `coarse_floor` deliberately consults the imagery cache — a viewer should
    /// spend the sharpest ancestor it already holds rather than blur ground it
    /// could show — and "what it already holds" is a function of the order
    /// concurrent loads finished and of what a bounded LRU evicted. The floor
    /// then changes between runs, the drape key changes with it, and the
    /// composed pixels change underneath an identical selection.
    ///
    /// Measured before this existed, 2026-09-10: two bakes of one scene, one
    /// warm cache, identical settings, selecting exactly the same 80 and 60
    /// tiles — and 16 of the 80 draped differently. Afterwards the two packs
    /// were byte-identical.
    ///
    /// What this does **not** cover: that `drape` calls `floor_under` at all.
    /// That single call site is held only by the end-to-end measurement above
    /// (`tuile-bake` twice, then `cmp`), because covering it in a unit test
    /// would mean a `PlanetaryLoader` over fake quantized-mesh and imagery
    /// sources, which does not exist here yet.
    #[test]
    fn the_bulk_floor_is_a_function_of_the_ground_and_nothing_else() {
        let scheme = raster::TilingScheme::web_mercator();
        let rect = inset(
            &scheme,
            ImageryCoord {
                level: 12,
                x: 2100,
                y: 1500,
            },
        );

        let fixed = fixed_floor(&scheme, &rect);
        assert_eq!(
            fixed.level,
            FLOOR_LEVEL.max(scheme.minimum_level),
            "the bulk floor is the pinned level, which is always resident"
        );
        // An empty cache and a full one must answer the same, because the
        // cache is not consulted at all.
        assert_eq!(fixed, fixed_floor(&scheme, &rect));

        // …and it is an ancestor of the tile, not some other patch of Earth.
        let deepest = scheme.containing_tile(&rect);
        let up = deepest.level - fixed.level;
        assert_eq!((deepest.x >> up, deepest.y >> up), (fixed.x, fixed.y));

        // The interactive floor is the one that moves: given a cache holding a
        // deeper ancestor it answers differently, which is exactly why a bake
        // must not use it.
        let cache = Mutex::new(ImageryCache::new(8));
        let empty_answer = coarse_floor(&scheme, &rect, &cache);
        assert_eq!(empty_answer, fixed, "with nothing cached, both agree");
        let deeper = ImageryCoord {
            level: fixed.level + 3,
            x: deepest.x >> (deepest.level - (fixed.level + 3)),
            y: deepest.y >> (deepest.level - (fixed.level + 3)),
        };
        cache.lock().expect("cache").put(
            deeper,
            Arc::new(DecodedTexture::from_rgba8(1, 1, vec![255, 255, 255, 255])),
            &mut |_, _| {},
        );
        assert_eq!(
            coarse_floor(&scheme, &rect, &cache),
            deeper,
            "the interactive floor follows the cache — the behaviour a viewer \
             wants and a bake cannot have"
        );
        assert_eq!(
            fixed_floor(&scheme, &rect),
            fixed,
            "and the bulk floor does not move when the cache does"
        );

        // The dispatch, which is the behaviour. Proving the two functions do
        // what they say proves nothing about which one a bulk render gets.
        let bulk = GlobeOptions {
            deterministic_floor: true,
            ..GlobeOptions::default()
        };
        let interactive = GlobeOptions::default();
        assert_eq!(bulk.floor_under(&scheme, &rect, &cache), fixed);
        assert_eq!(interactive.floor_under(&scheme, &rect, &cache), deeper);
    }

    /// Draping asks for the mosaic at a level and gets back at most the layer
    /// budget, whatever the level asked for — the coarsening is what keeps the
    /// shader's binding count a promise rather than a hope.
    /// A rect strictly inside `coord`, so tile ranges never pick up the
    /// boundary neighbours.
    fn inset(scheme: &raster::TilingScheme, coord: ImageryCoord) -> GeoRect {
        let r = scheme.tile_rect(coord);
        let (w, h) = (r.width() * 0.01, r.height() * 0.01);
        GeoRect {
            west: r.west + w,
            east: r.east - w,
            // v grows southward: north > south.
            south: r.south + h,
            north: r.north - h,
        }
    }

    #[test]
    fn a_drape_never_asks_for_more_layers_than_a_draw_can_bind() {
        let scheme = tuile_core::raster::TilingScheme::web_mercator();
        let rect = GeoRect {
            west: 0.100,
            south: 0.200,
            east: 0.104,
            north: 0.206,
        };
        // Every budget a device can produce, not one written down here: the
        // floor, the ceiling, and the WebGPU baseline in between. The coarsening
        // has to hold for all of them, because which one is in force is decided
        // by the machine the viewer happens to run on.
        for budget in [
            raster::MIN_IMAGERY_SLOTS,
            raster::imagery_slots(16),
            raster::USEFUL_IMAGERY_SLOTS,
        ] {
            for level in 0..=scheme.maximum_level {
                let mosaic = scheme.mosaic_at_level(&rect, level, budget);
                assert!(
                    mosaic.tile_count() <= u64::from(budget),
                    "budget {budget}, level {level} produced {} layers",
                    mosaic.tile_count()
                );
            }
        }
    }

    /// **The detected budget never exceeds what the device said it would bind.**
    ///
    /// The failure this guards is a validation error at the first draw, on some
    /// machine that is not this one: a shader is generated for `n` slots, the
    /// bind-group layout declares `n` textures, and the device refuses the
    /// pipeline because `n` is more than its stage allows. Nothing about it is
    /// visible on a Metal box reporting 128.
    #[test]
    fn the_budget_fits_inside_what_the_device_reports() {
        // 16 is both the WebGPU baseline and the WebGL2 floor; 128 is Metal.
        //
        // Nothing below 10 is covered, and deliberately: the floor is 9 slots
        // plus the base colour, so a device offering fewer cannot host this
        // renderer at all and the clamp would hand back a number larger than
        // what it said it would bind. No such device is reachable — WebGL2
        // guarantees 16 — so this is a documented edge rather than a case.
        for reported in [16u32, 32, 128, 1024] {
            let slots = raster::imagery_slots(reported);
            assert!(
                slots < reported,
                "device offers {reported} textures, budget asked for {slots} \
                 — and one of them is the tile's own base colour"
            );
            assert!(
                slots >= raster::MIN_IMAGERY_SLOTS,
                "budget {slots} is below the 3x3 a drape needs to be correct"
            );
            assert!(
                slots <= raster::USEFUL_IMAGERY_SLOTS,
                "budget {slots} spends more fetches per fragment than coarsening costs"
            );
        }
    }

    /// A loader over flat imagery and a terrain store, holding `cap`
    /// decoded imagery tiles.
    fn store_loader(cap: usize) -> PlanetaryLoader<StoredTerrain, Painted> {
        let scheme = GeographicTilingScheme::default();
        PlanetaryLoader {
            terrain: StoredTerrain::default(),
            imagery: Painted(40),
            scheme,
            opts: GlobeOptions::default(),
            cache: Mutex::new(ImageryCache::new(cap)),
            meshes: Mutex::new(MeshCache::new(8)),
            fetching: ImageryInFlight::default(),
            building: TerrainInFlight::default(),
            changes: store_log::StoreLog::default(),
            availability: Arc::new(tuile_terrain::Availability::default()),
            provenance: Arc::default(),
            heights: Arc::new(TerrainHeights::new(scheme)),
            offload: offload::inline(),
        }
    }

    /// A terrain store with nothing behind it: a fetch answers that the
    /// tile does not exist, and the store remembers the absence, as
    /// `CachedTerrain` does.
    #[derive(Default)]
    struct StoredTerrain(Mutex<std::collections::HashSet<TileCoord>>);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl tuile_terrain::TerrainSource for StoredTerrain {
        async fn fetch_tile(
            &self,
            coord: TileCoord,
        ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError>
        {
            self.0.lock().expect("store").insert(coord);
            Err(tuile_terrain::TerrainSourceError::no_such_tile(format!(
                "{coord:?}"
            )))
        }

        fn holds(&self, coord: TileCoord) -> bool {
            self.0.lock().expect("store").contains(&coord)
        }
    }

    /// The store events' answer about `asked` (with the answers they were
    /// given) since `since`.
    fn touched<T: tuile_terrain::TerrainSource + 'static, I: ImageryProvider + 'static>(
        loader: &PlanetaryLoader<T, I>,
        since: u64,
        asked: &[(ResourceId, Presence)],
    ) -> Vec<ResourceId> {
        let mut out = Vec::new();
        loader.store_changes(since, asked, &mut out);
        out
    }

    /// **A drape's imagery decoded, then let go, is a store event each time,
    /// naming that drape and not one elsewhere; and the answer follows.**
    /// The cache holds one drape's tiles: composing a drape on the other
    /// side of the globe lets the first one's go.
    #[test]
    fn a_drapes_imagery_kept_and_let_go_are_store_events() {
        let here = ResourceId::drape(ground_id(), LayerId(0), 9);
        let far = TileId::from_terrain(GROUND.level, 3 * GROUND.x, GROUND.y);
        let there = ResourceId::drape(far, LayerId(0), 9);
        let cap = store_loader(1).drape_coords(ground_id(), 9).len();
        let loader = store_loader(cap);
        let start = loader.store_events();
        assert_eq!(loader.holds(here), Presence::Absent);
        assert_eq!(loader.store_events(), start, "asking is no event");
        futures_executor::block_on(loader.compose(ground_id(), LayerId(0), 9)).expect("drape");
        let kept = loader.store_events();
        assert!(
            kept > start,
            "keeping the drape's imagery is no store event"
        );
        assert_eq!(
            touched(
                &loader,
                start,
                &[(here, Presence::Absent), (there, Presence::Absent)]
            ),
            vec![here]
        );
        assert_eq!(loader.holds(here), Presence::Stored);
        futures_executor::block_on(loader.compose(far, LayerId(0), 9)).expect("drape");
        assert!(loader.store_events() > kept);
        assert!(
            touched(
                &loader,
                kept,
                &[(here, Presence::Stored), (there, Presence::Stored)]
            )
            .contains(&here),
            "letting the drape's imagery go is no event about it"
        );
        assert_eq!(loader.holds(here), Presence::Absent);
        assert_eq!(
            touched(&loader, loader.store_events(), &[(here, Presence::Stored)]),
            vec![]
        );
    }

    /// **A terrain tile the store comes to hold is a store event about its
    /// mesh and those built from it, not about a tile that reads its own.**
    /// The source has levels 1 to 3 everywhere and nothing in them, so the
    /// tile and its ancestors to level 1 are fetched (each stored as
    /// absent). A level-5 tile under it climbs to it — its own level and
    /// the one above are not the source's — and is touched; a level-3 tile
    /// elsewhere reads its own tile, and is not.
    #[test]
    fn a_terrain_tile_stored_is_a_store_event_about_the_meshes_built_from_it() {
        let loader = store_loader(8);
        let every = |level: u32| AvailabilityRange {
            start_x: 0,
            start_y: 0,
            end_x: (2 << level) - 1,
            end_y: (1 << level) - 1,
        };
        loader
            .availability
            .add_descendant_ranges(0, &[vec![every(1)], vec![every(2)], vec![every(3)]]);
        let at = TileCoord::new(3, 4, 2);
        let mesh = |c: TileCoord| ResourceId::Mesh(TileId::from_terrain(c.level, c.x, c.y));
        let asked = [
            mesh(at),
            mesh(TileCoord::new(5, 17, 9)),
            mesh(TileCoord::new(3, 12, 2)),
        ];
        let start = loader.store_events();
        assert_eq!(loader.holds(asked[0]), Presence::Absent);
        assert_eq!(loader.holds(asked[1]), Presence::Absent);
        let _ = futures_executor::block_on(loader.terrain_mesh(at));
        assert!(loader.store_events() > start);
        assert_eq!(
            touched(&loader, start, &asked.map(|r| (r, Presence::Absent))),
            asked[..2].to_vec()
        );
        assert_eq!(loader.holds(asked[0]), Presence::Stored);
        assert_eq!(loader.holds(asked[1]), Presence::Stored);
        // The source turns out to have level 4 too: the level-5 tile now
        // climbs to a tile the store does not hold.
        let seen = loader.store_events();
        loader
            .availability
            .add_descendant_ranges(3, &[vec![every(4)]]);
        assert!(
            loader.store_events() > seen,
            "availability revealed is no event"
        );
        let answers = [
            (asked[0], Presence::Stored),
            (asked[1], Presence::Stored),
            (asked[2], Presence::Absent),
        ];
        assert_eq!(touched(&loader, seen, &answers), vec![asked[1]]);
        assert_eq!(loader.holds(asked[1]), Presence::Absent);
        // A mesh decoded at a tile the store holds is no event; one decoded
        // where the store holds nothing touches what climbs to it.
        let quad = || {
            Arc::new(tuile_terrain::QuantizedMesh {
                header: tuile_terrain::Header {
                    center: [0.0; 3],
                    min_height: 0.0,
                    max_height: 1.0,
                    bounding_sphere_center: [0.0; 3],
                    bounding_sphere_radius: 1.0,
                    horizon_occlusion: [0.0; 3],
                },
                u: vec![0.0, 1.0, 0.0],
                v: vec![0.0, 0.0, 1.0],
                height: vec![0.0; 3],
                indices: vec![0, 1, 2],
                normals: None,
                edges: [vec![], vec![], vec![], vec![]],
                metadata_available: None,
                cut: 0,
            })
        };
        let seen = loader.store_events();
        loader.keep_mesh(at, quad());
        assert_eq!(
            loader.store_events(),
            seen,
            "a stored tile decoded is an event"
        );
        loader.keep_mesh(TileCoord::new(4, 8, 4), quad());
        assert!(loader.store_events() > seen);
        let answers = [
            (asked[0], Presence::Stored),
            (asked[1], Presence::Absent),
            (asked[2], Presence::Absent),
        ];
        assert_eq!(touched(&loader, seen, &answers), vec![asked[1]]);
        assert_eq!(loader.holds(asked[1]), Presence::Stored);
    }

    /// Flat imagery behind a store holding the tiles listed.
    struct StoredImagery(Mutex<std::collections::HashSet<ImageryCoord>>);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl raster::ImageryProvider for StoredImagery {
        fn tiling_scheme(&self) -> raster::TilingScheme {
            raster::TilingScheme::geographic()
        }

        async fn fetch_tile_bytes(
            &self,
            coord: ImageryCoord,
        ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, raster::RasterError> {
            Painted(40).fetch_tile_bytes(coord).await
        }

        fn holds(&self, coord: ImageryCoord) -> bool {
            self.0.lock().expect("store").contains(&coord)
        }
    }

    /// **A stored imagery tile decoded is an event only while the floor
    /// under it is not held**: decoded, it may become a drape's floor in
    /// place of one that is not held, or, let go, give the floor back to
    /// it. Under a held floor it changes no answer.
    #[test]
    fn a_stored_imagery_tile_decoded_is_an_event_while_its_floor_is_not_held() {
        let scheme = GeographicTilingScheme::default();
        let loader = PlanetaryLoader {
            terrain: NoTerrain,
            imagery: StoredImagery(Mutex::default()),
            scheme,
            opts: GlobeOptions::default(),
            cache: Mutex::new(ImageryCache::new(8)),
            meshes: Mutex::new(MeshCache::new(8)),
            fetching: ImageryInFlight::default(),
            building: TerrainInFlight::default(),
            changes: store_log::StoreLog::default(),
            availability: Arc::new(tuile_terrain::Availability::default()),
            provenance: Arc::default(),
            heights: Arc::new(TerrainHeights::new(scheme)),
            offload: offload::inline(),
        };
        let imagery = loader.imagery.tiling_scheme();
        let tile = ImageryCoord {
            level: FLOOR_LEVEL + 3,
            x: 40,
            y: 20,
        };
        let floor = ImageryCoord {
            level: FLOOR_LEVEL,
            x: 5,
            y: 2,
        };
        let tex = || Arc::new(DecodedTexture::from_rgba8(1, 1, vec![0, 0, 0, 255]));
        loader.imagery.0.lock().expect("store").insert(tile);
        let seen = loader.store_events();
        loader.keep_imagery(&mut loader.decoded(), &imagery, tile, tex());
        assert!(loader.store_events() > seen, "its floor is not held");
        loader.decoded().map.clear();
        loader.decoded().order.clear();
        loader.imagery.0.lock().expect("store").insert(floor);
        let seen = loader.store_events();
        loader.keep_imagery(&mut loader.decoded(), &imagery, tile, tex());
        assert_eq!(loader.store_events(), seen, "its floor is held");
    }
}
