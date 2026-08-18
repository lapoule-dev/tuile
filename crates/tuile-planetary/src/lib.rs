// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-planetary
//!
//! Planetary scene assembly: crosses a **terrain source** with an **imagery
//! provider** into the [`TileTree`] + [`TileLoader`] pair the geometry server
//! drives, so the globe streams through the same runtime as any other source.
//!
//! It is generic over [`tuile_terrain::TerrainSource`] and
//! [`tuile_core::raster::ImageryProvider`] — it knows **nothing** about ion,
//! Bing, HTTP or any transport. The host decides those at injection time
//! (resolve the sources, then call [`globe`]); a wasm host and a native host
//! plug in different backends behind the same two traits. This crate is pure
//! and wasm-able: no I/O of its own beyond the injected sources.
//!
//! [`PlanetaryLoader`] is the geometry↔imagery crossing point: per terrain tile
//! it decodes the quantized mesh and names the imagery tiles that cover it, each
//! reprojected once onto its own rectangle and **shared** with every other
//! terrain tile it drapes.

use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tuile_core::content::DecodedTexture;
use tuile_core::fetch::FetchError;
use tuile_core::geo::WGS84_A;
use tuile_core::offload::{self, Offload};
use tuile_core::raster::{self, GeoRect, ImageryCoord, ImageryProvider, RasterError};
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_terrain::{
    decode, level_geometric_error, to_decoded, Availability, AvailabilityRange,
    GeographicTilingScheme, LayerJson, TerrainHeights, TerrainSource, TerrainTree, TileCoord,
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
/// `wgpu-viewer`'s `TUILE_PIN_LEVEL` defaults to 4 to match.
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
fn is_opaque_black(tex: &DecodedTexture) -> bool {
    const SAMPLES: usize = 100;
    let texels = tex.rgba8.len() / 4;
    if texels == 0 {
        return false;
    }
    let step = (texels / SAMPLES).max(1);
    (0..texels)
        .step_by(step)
        .all(|i| tex.rgba8[i * 4] == 0 && tex.rgba8[i * 4 + 1] == 0 && tex.rgba8[i * 4 + 2] == 0)
}

/// What a tile is painted when no imagery covers it.
///
/// A light green nothing in an aerial photograph is: not vegetation, which is
/// darker and never uniform; not haze; not snow. Seeing it on the globe names
/// the fault immediately instead of leaving a pale patch to be argued about.
const MISSING_IMAGERY_COLOUR: [f32; 4] = [0.55, 0.95, 0.55, 1.0];

/// What shows under a mosaic wherever no layer reaches.
///
/// A muted slate, close to deep water and to ground in shadow — the two things
/// most of a globe is. Distinct from [`MISSING_IMAGERY_COLOUR`], which marks a
/// tile that got *no* imagery at all and is meant to be seen.
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

/// Shared, host-updated imagery detail target: the desired ground texel
/// spacing (metres per texel) for draped imagery, which the app recomputes from
/// the camera each frame (≈ `2·altitude·tan(fovy/2) / viewport_height`). The
/// loader reads it so imagery refines with **altitude** — a giant mosaic up
/// close, coarse from orbit — decoupled from the terrain LOD. Default (unset)
/// falls back to matching the terrain's geometric error.
#[derive(Clone, Default)]
pub struct ImageryDetail(Arc<std::sync::atomic::AtomicU64>);

impl ImageryDetail {
    /// Sets the desired ground texel spacing (metres/texel). Smaller ⇒ finer
    /// imagery. Call each frame from the host with the current camera.
    pub fn set_target_texel_spacing(&self, metres: f64) {
        self.0
            .store(metres.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    fn target(&self) -> Option<f64> {
        let bits = self.0.load(std::sync::atomic::Ordering::Relaxed);
        let v = f64::from_bits(bits);
        (bits != 0 && v.is_finite() && v > 0.0).then_some(v)
    }
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

    fn put(&mut self, c: TileCoord, mesh: Arc<tuile_terrain::QuantizedMesh>) {
        if self.map.insert(c, mesh).is_none() {
            self.order.push_back(c);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
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
}

impl ImageryCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Whether this tile is already decoded, without taking a reference.
    fn has(&self, c: &ImageryCoord) -> bool {
        self.map.contains_key(c)
    }

    fn get(&self, c: &ImageryCoord) -> Option<Arc<DecodedTexture>> {
        self.map.get(c).map(Arc::clone)
    }

    fn put(&mut self, c: ImageryCoord, tex: Arc<DecodedTexture>) {
        if self.map.insert(c, tex).is_none() {
            self.order.push_back(c);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
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
#[derive(Debug, Clone, Default)]
pub struct GlobeOptions {
    /// When true, terrain only (no imagery) — the geometry debug view.
    pub no_imagery: bool,
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
    /// Meshes built for **stand-ins**, kept apart from [`Self::meshes`] on
    /// purpose.
    ///
    /// A stand-in's mesh is an upsample of an ancestor — the same surface, over
    /// the missing tile's own ground. Putting it in the real cache would make
    /// [`Self::terrain_mesh`] return it on the next request and never fetch the
    /// tile it stands in for: the approximation would become permanent, and the
    /// globe would stop refining at exactly the tiles a camera lingers on.
    ///
    /// Its own cache, so the rungs of a walk are still shared between siblings —
    /// which is what keeps a burst of stand-ins from re-clipping the same
    /// ancestor once per tile.
    fill_meshes: Mutex<MeshCache>,
    /// Shared with the tree: each terrain tile's `metadata` extension reveals
    /// the availability of its descendants, folded in here so traversal can
    /// keep refining toward the finest LOD.
    availability: Arc<Availability>,
    /// Host-updated imagery detail target (drives imagery level by altitude).
    detail: ImageryDetail,
    /// Shared with the host's camera: each decoded tile's relief, so the eye can
    /// be kept above the ground rather than above the ellipsoid.
    heights: Arc<TerrainHeights>,
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
        if let Some(tex) = self.cache.lock().expect("imagery cache").get(&c) {
            return Ok((c, tex));
        }
        let scheme = self.imagery.tiling_scheme();
        // As above: the cache hit is already gone, and the recursion onto the
        // parent below counts itself when it reaches this line.
        let m = tuile_core::metrics::metrics();
        m.texture_fetches.inc();
        let outstanding =
            tuile_core::metrics::InFlight::new(&m.textures_in_flight, &m.texture_fetch_seconds);
        // Bytes, not the decoded texture: decoding is CPU work, and taking it
        // here rather than inside the provider is what lets it leave this
        // thread together with the resampling below, in one hop.
        let fetched = self.imagery.fetch_tile_bytes(c).await;
        drop(outstanding);
        match fetched {
            Ok(fetched) => {
                tracing::debug!(z = c.level, x = c.x, y = c.y, "imagery tile");
                // Decode, resample and inspect in a single offloaded job. All
                // three are pure CPU over the same buffer, so splitting them
                // would only pay the hop three times and undo the locality.
                let off = Arc::clone(&self.offload);
                let (tex, black) = offload::run(off.as_ref(), move || {
                    let tex = raster::decode_and_reproject(&fetched.value, &scheme, c)?;
                    let black = is_opaque_black(&tex);
                    Ok::<_, RasterError>((Arc::new(tex), black))
                })
                .await
                .map_err(|e| LoadError::Failed(format!("imagery {c:?}: {e}")))?;
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
                self.cache
                    .lock()
                    .expect("imagery cache")
                    .put(c, Arc::clone(&tex));
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
    /// A stand-in's mesh: the nearest ancestor already in memory, upsampled onto
    /// `coord`'s own rectangle.
    ///
    /// **The whole point is that it is not a plane.** A stand-in used to be four
    /// corner heights with a ruled surface between them, and that is what made
    /// the mechanism unusable: where some tiles get a stand-in and others fall
    /// back to an ancestor — which happens whenever this returns `None` — the
    /// ancestor's *real relief* and the neighbours' *flat* stand-ins occupy the
    /// same ground, and the depth test picks a winner per pixel. On screen: large
    /// flat patches of coarse colour with ridges punching through them in ragged
    /// outlines that follow the terrain rather than the tile grid.
    ///
    /// [`tuile_terrain::upsample`] gives "a mesh of exactly the same surface over
    /// exactly the child's ground", so there is no relief for anything to punch
    /// through: the stand-in and the ancestor it came from are the same surface.
    ///
    /// Synchronous and cache-only by contract — this runs inside the traversal.
    /// One level at a time, each rung kept in [`Self::fill_meshes`], so a burst
    /// of siblings clips their shared parent once rather than once each.
    fn stand_in_mesh(&self, coord: TileCoord) -> Option<Arc<tuile_terrain::QuantizedMesh>> {
        // The real thing, if it happens to be decoded: a stand-in for a tile
        // whose mesh is already here needs no approximation at all.
        if let Some(mesh) = self.meshes.lock().ok()?.get(coord) {
            return Some(mesh);
        }
        if let Some(mesh) = self.fill_meshes.lock().ok()?.get(coord) {
            return Some(mesh);
        }
        if coord.level == 0 {
            return None;
        }
        let parent = TileCoord::new(coord.level - 1, coord.x / 2, coord.y / 2);
        let from = self.stand_in_mesh(parent)?;
        let built = Arc::new(tuile_terrain::upsample(&from, parent, coord)?);
        self.fill_meshes.lock().ok()?.put(coord, Arc::clone(&built));
        Some(built)
    }

    fn terrain_mesh<'a>(&'a self, coord: TileCoord) -> MeshFuture<'a> {
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
                let fetched = self.terrain.fetch_tile(coord).await;
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
                        // Decoding a quantized mesh is the same kind of work as
                        // decoding a JPEG and belongs off this thread for the
                        // same reason: nothing in it awaits, and holding the
                        // poller for its duration stalls every other load.
                        let off = Arc::clone(&self.offload);
                        let decoded = offload::run(off.as_ref(), move || decode(&fetched.value))
                            .await
                            .map_err(|e| LoadError::Failed(e.to_string()))?;
                        // Decoding is also what reveals which descendants exist —
                        // the source may have served these bytes from a cache, but
                        // the ranges still reach the shared availability, so
                        // refinement never stalls at a cached level.
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

            self.meshes
                .lock()
                .expect("mesh cache")
                .put(coord, Arc::clone(&mesh));
            Ok(mesh)
        })
    }

    async fn drape(
        &self,
        content: &mut tuile_core::DecodedTileContent,
        rect: &GeoRect,
        terrain_level: u32,
    ) -> Result<(), LoadError> {
        let scheme = self.imagery.tiling_scheme();
        // Start from the imagery level whose texel spacing matches the terrain
        // tile's geometric error (Cesium's getLevelWithMaximumTexelSpacing)…
        let ge = level_geometric_error(terrain_level, WGS84_A, self.scheme.root_tiles_x);
        let lat = 0.5 * (rect.south + rect.north);
        let base = scheme.level_for_texel_spacing(ge, lat);
        // …then refine with the camera ALTITUDE, beyond the terrain LOD, up to
        // the provider's max. Where terrain data runs out (Cesium World Terrain
        // caps around z13 over Europe) the mesh stays coarse but the ground
        // stays sharp. How far that can go is now bounded only by how many
        // textures one *tile* binds across its passes — `imagery_layer_budget`
        // of `GlobeOptions::imagery_slots` — where it used to be bounded by a
        // per-tile memory cost that no longer exists.
        let level = match self.detail.target() {
            Some(texel) => scheme.level_for_texel_spacing(texel, lat).max(base),
            None => base,
        }
        .min(scheme.maximum_level);
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
        // The four numbers that decide how sharp the ground gets, in the order
        // they constrain each other: what the terrain's own error asks for, what
        // the camera's altitude asks for, what was requested, and what survived
        // the layer budget. A gap between the last two is the budget coarsening
        // the request back, which is the thing to watch — it is the only reason
        // imagery cannot go deeper than the mesh it drapes.
        tracing::debug!(
            terrain_level,
            matched = base,
            wanted = level,
            got = mosaic.level,
            tiles = mosaic.tile_count(),
            "imagery level"
        );
        let coords = mosaic.tiles();
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
        let floor = coarse_floor(&scheme, rect, &self.cache);
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
        // and painted marker green. Here the tile *has* a layer, so every
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
                "NO IMAGERY over this tile: drawn in marker green"
            );
            tuile_core::metrics::metrics().tiles_without_imagery.inc();
        }
        for mesh in &mut content.meshes {
            // Terrain owns no base-colour texture; the layers are the ground.
            mesh.material.base_color_texture = None;
            mesh.material.base_color_factor = if layers.is_empty() {
                MISSING_IMAGERY_COLOUR
            } else {
                // What shows wherever the mosaic does not reach.
                //
                // It was white, by default, and white is the worst possible
                // choice: a fragment that no layer covers — a hairline between
                // two layers' masks, a corner the mosaic's budget cut — came out
                // as a **bright** line over the imagery. On screen it read as a
                // glowing grid drawn on the ground, and it was reported as one.
                //
                // The reference implementation has the same fallback and never
                // shows it, because `Globe.baseColor` is a dark blue. Same
                // reasoning here: whatever is not covered should be the colour
                // of unremarkable ground, so a gap of one pixel costs a pixel
                // nobody notices instead of announcing itself.
                //
                // This makes a gap invisible; it does not close one. `D` cycles
                // to the coverage view, which paints magenta wherever no layer
                // reaches, and that is where to look for the cause.
                UNCOVERED_GROUND
            };
        }
        content.imagery = layers;

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
        if !content.imagery.is_empty() {
            let (mut umin, mut vmin) = (f32::INFINITY, f32::INFINITY);
            let (mut umax, mut vmax) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
            for l in &content.imagery {
                umin = umin.min(l.coverage[0]);
                vmin = vmin.min(l.coverage[1]);
                umax = umax.max(l.coverage[2]);
                vmax = vmax.max(l.coverage[3]);
            }
            if umin > 0.0 || vmin > 0.0 || umax < 1.0 || vmax < 1.0 {
                tracing::warn!(
                    z = terrain_level,
                    layers = content.imagery.len(),
                    umin,
                    vmin,
                    umax,
                    vmax,
                    "COVERAGE SHORT: the layers span u {umin}..{umax}, v {vmin}..{vmax} \
                     of a tile that is 0..1 in both — the rest draws UNCOVERED_GROUND"
                );
            }
        }
        Ok(())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> TileLoader
    for PlanetaryLoader<T, I>
{
    /// A stand-in surface for a tile that is selected and has not arrived.
    ///
    /// Built entirely from what is already in memory — the shared relief for the
    /// corner heights, the decoded-imagery cache for the coarse layer under it —
    /// so it costs no request and answers in the same breath it was asked. That
    /// is the requirement: it stands in for the frames between "the camera is
    /// looking here" and "the tile arrived", and anything that waits has missed
    /// them.
    ///
    /// The heights are the four corners of the tile's own rectangle, sampled
    /// from whatever terrain has already streamed in — usually a coarser
    /// ancestor covering the same ground, which is exactly the surface the
    /// stand-in should sit on. Where nothing is known the ellipsoid is used;
    /// sea level is wrong for a mountain and wrong by less than the tile is
    /// wide, which is all the accuracy a surface living for three frames needs.
    ///
    /// The imagery is the same coarse floor the real tile will be draped with,
    /// **taken from the cache and never fetched**. A miss simply means no layer:
    /// the fill then draws marker green, which is the honest colour for ground
    /// nobody has a picture of, and still an improvement on the two things it
    /// replaces — black, or an ancestor overlapping its own descendants.
    fn fill(&self, id: TileId) -> Option<tuile_core::DecodedTileContent> {
        let (z, x, y) = id.terrain_coord();
        let coord = TileCoord::new(z, x, y);
        let rect = self.scheme.tile_rect(coord);
        // The surface of the nearest ancestor already in memory, restated over
        // this tile's own rectangle. Never a plane — see `stand_in_mesh`.
        let mesh = self.stand_in_mesh(coord)?;
        let mut content = to_decoded(&mesh, &rect, tuile_terrain::skirt_height(&rect));

        let geo = GeoRect {
            west: rect.west,
            south: rect.south,
            east: rect.east,
            north: rect.north,
        };
        let scheme = self.imagery.tiling_scheme();
        // Cache only, and never a fetch: `fetch_imagery` would be correct and
        // would also await a network round trip, which is the one thing this
        // function may not do.
        //
        // **The sharpest mosaic every tile of which is already decoded**, not one
        // coarse layer. A single ancestor tile was the old answer, and it is what
        // made a stand-in read as a flat rectangle of uniform colour: a level-4
        // image sampled over the ground of a level-16 tile is one colour, and one
        // flat colour laid over real terrain is as noticeable as the artefact it
        // was replacing. The mosaic is the same construction a real drape uses —
        // same coverage chaining, same `substituted` placement — so a stand-in
        // differs from the tile it stands for in sharpness and in nothing else.
        //
        // Sharpest first: the loop stops at the first level whose every tile is
        // in hand, and the coarsest levels are a handful of tiles for the whole
        // planet, so it always terminates on something.
        let budget = self.opts.imagery_slots.get().saturating_sub(1).max(1);
        let deepest = scheme.containing_tile(&geo);
        let picked = self.cache.lock().ok().and_then(|c| {
            let from = (deepest.level + 2).min(scheme.maximum_level);
            (scheme.minimum_level..=from).rev().find_map(|level| {
                let mosaic = scheme.mosaic_at_level(&geo, level, budget);
                let textures: Option<Vec<_>> = mosaic
                    .tiles()
                    .into_iter()
                    .map(|coord| c.get(&coord).map(|texture| (coord, texture)))
                    .collect();
                textures.map(|textures| (mosaic, textures))
            })
        });
        if let Some((mosaic, textures)) = picked {
            for ((served, texture), covers) in
                textures.into_iter().zip(mosaic.coverage(&scheme, &geo))
            {
                let layer = raster::ImageryLayer::substituted(
                    served,
                    texture,
                    &geo,
                    &to_the_pole(
                        scheme.tile_rect(served),
                        served,
                        scheme.tiles_at(served.level).1,
                    ),
                    covers,
                );
                if layer.is_visible() {
                    content.imagery.push(layer);
                }
            }
        }
        // No picture anywhere on the way up: refuse rather than emit a flat
        // marker-green patch. A stand-in exists to be less noticeable than what
        // it replaces, and a bright green rectangle over real terrain is not —
        // measured on screen, and worse than the ancestor it displaced. The
        // consumer keeps whatever it had for one more frame.
        if content.imagery.is_empty() {
            return None;
        }
        // **The one imagery path with no instrument on it.**
        //
        // `drape` counts five different ways its layers can come up short;
        // this counted none, so a stand-in that covered its ground with a
        // single very coarse image looked, from every log and every metric,
        // exactly like one that covered it properly. On screen it is a flat
        // rectangle of one colour with a *finely tessellated* grid inside it —
        // the upsampled mesh — which is why it reads as a deep tile that failed
        // rather than as the approximation it is.
        //
        // The mosaic here is cache-only by contract, so it settles on the
        // sharpest level every tile of which is already decoded. When that is
        // several levels above the ground being covered, one texel is stretched
        // across the whole tile.
        if content.imagery.len() < 4 {
            tracing::warn!(
                z,
                x,
                y,
                layers = content.imagery.len(),
                "THIN STAND-IN: covered by {} imagery layer(s), so the ground is \
                 close to one flat colour",
                content.imagery.len()
            );
        }
        Some(content)
    }

    /// Pulls the coarse pyramid into the store before the camera asks for it.
    ///
    /// Terrain and imagery both, level by level from the top, because the
    /// fallback chain needs both: a mesh with no imagery draws marker green,
    /// imagery with no mesh draws nothing at all.
    ///
    /// Availability is consulted first for terrain, so ground the source does
    /// not cover costs no request. Imagery has no such oracle, and a level the
    /// provider declines is simply a miss — which the store remembers as
    /// cheaply as a hit.
    ///
    /// Bounded concurrency: this competes with the loads the picture is waiting
    /// on, and a warm-up that starves the first frame has defeated itself.
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
                    let _ = self.terrain.fetch_tile(c).await;
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
                    let _ = self.imagery.fetch_tile_bytes(c).await;
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
        // Skirts, at the size both reference implementations use.
        //
        // They were switched on once before and produced broad horizontal smears
        // across every slope, and were switched off again with the size blamed.
        // The size was not the fault. `mesh.edges` arrives in whatever order the
        // server wrote it — the format guarantees none — and the first version
        // stitched each wall out of the list as given, joining whatever happened
        // to be adjacent *in the list*. That draws triangles clean across the
        // tile. The reference sorts each edge before stitching, for exactly this
        // reason, and so does `append_skirts` now; three tests hold it, the
        // winding, and the outward splay that keeps two neighbours' walls from
        // landing in the same plane.
        //
        // A wall is textured by the column of texels above it and lit by the
        // same normal, so where one does show through it is the colour of the
        // ground it hangs from rather than a stripe of something else.
        let mut content = to_decoded(&qm, &rect, tuile_terrain::skirt_height(&rect));
        if !self.opts.no_imagery {
            let georect = GeoRect {
                west: rect.west,
                south: rect.south,
                east: rect.east,
                north: rect.north,
            };
            self.drape(&mut content, &georect, z).await?;
        }
        tuile_core::metrics::metrics()
            .load_seconds
            .record(started.elapsed());
        Ok(Loaded::Content(content))
    }
}

/// The terrain quadtree of a parsed `layer.json`, as a boxed [`TileTree`].
pub fn terrain_tree(layer: LayerJson) -> Box<dyn TileTree> {
    Box::new(TerrainTree::new(layer))
}

/// Crosses an already-resolved terrain source and imagery provider into a
/// `(tree, loader, detail, heights)` ready for `tuile_core::runtime::in_process_with`.
/// `layer` is the terrain's `layer.json` (the host resolves it). The returned
/// [`ImageryDetail`] is the host's handle to drive imagery resolution by camera
/// altitude each frame; the returned [`TerrainHeights`] is the surface to hand
/// a camera controller so it stays above the ground. Pure: the host has already
/// chosen backends + transport.
pub fn globe<T, I>(
    terrain: T,
    imagery: I,
    layer: LayerJson,
    opts: GlobeOptions,
) -> (
    Box<dyn TileTree>,
    Arc<dyn TileLoader>,
    ImageryDetail,
    Arc<TerrainHeights>,
)
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
) -> (
    Box<dyn TileTree>,
    Arc<dyn TileLoader>,
    ImageryDetail,
    Arc<TerrainHeights>,
)
where
    T: TerrainSource + 'static,
    I: ImageryProvider + 'static,
{
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
    let deepest_useful = imagery.tiling_scheme().maximum_level;
    let tree: Box<dyn TileTree> = Box::new(
        TerrainTree::with_availability(layer, Arc::clone(&availability))
            .with_max_level(deepest_useful),
    );
    let detail = ImageryDetail::default();
    let heights = Arc::new(TerrainHeights::new(scheme));
    let loader: Arc<dyn TileLoader> = Arc::new(PlanetaryLoader {
        terrain,
        imagery,
        scheme,
        opts,
        cache: Mutex::new(ImageryCache::new(512)),
        meshes: Mutex::new(MeshCache::new(512)),
        // Smaller than the real cache: these are approximations with a short
        // life, replaced the moment the tile they stand in for arrives.
        fill_meshes: Mutex::new(MeshCache::new(256)),
        availability,
        detail: detail.clone(),
        heights: Arc::clone(&heights),
        offload,
    });
    (tree, loader, detail, heights)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draping asks for the mosaic at a level and gets back at most the layer
    /// budget, whatever the level asked for — the coarsening is what keeps the
    /// shader's binding count a promise rather than a hope.
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
}
