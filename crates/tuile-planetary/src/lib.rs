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
//! [`PlanetaryLoader`] is the geometry↔imagery crossing point: per terrain
//! tile it decodes the quantized mesh and drapes the **matched-LOD mosaic** of
//! imagery covering it, reprojected Mercator→geographic.

use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tuile_core::fetch::FetchError;
use tuile_core::geo::WGS84_A;
use tuile_core::content::DecodedTexture;
use tuile_core::raster::{
    self, GeoRect, ImageryCoord, ImageryProvider, OverlayAttachment, RasterError,
};
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_terrain::{
    decode, level_geometric_error, to_decoded, Availability, AvailabilityRange,
    GeographicTilingScheme, LayerJson, TerrainHeights, TerrainSource, TerrainTree, TileCoord,
};

/// Absolute backstop on imagery tiles stitched per terrain tile: 4×4·256² = 1K²,
/// ~4 MiB of RGBA per drape. The mosaic helper coarsens a level at a time if a
/// tile's rectangle would need more than this.
///
/// This is the last line of defence, not the working limit —
/// [`levels_above_terrain`] normally keeps a drape well under it. Left in place
/// because the mosaic size also depends on how a tile's rectangle happens to
/// straddle the imagery grid, which no per-level rule can bound exactly.
const IMAGERY_MOSAIC_CAP: u32 = 16;

/// How many imagery levels above its own geometric error a terrain tile may be
/// draped at.
///
/// Each level down is 4× the texels, and the mosaic reaches
/// [`IMAGERY_MOSAIC_CAP`] at just two levels above the match — so without a
/// bound, *every* tile ends up carrying the same 1024² image whatever its size.
/// That is what saturated the resident budget: 657 tiles × 4 MiB over one orbit,
/// against a 1.5 GiB budget, and the second half-turn then spent itself
/// reloading what the first half had paid for.
///
/// The allowance is graded by depth because the tiles differ in what they are
/// for. A coarse tile covers a continent and is only ever drawn as a stand-in
/// under finer tiles that are not ready; sharpening it buys nothing and costs
/// the most, since it is also pinned as an ancestor for as long as anything
/// below it is visible. A deep tile is what the eye actually reads, and past
/// the terrain's own limit (Cesium World Terrain runs out around z13 over
/// Europe) extra imagery levels are exactly how the ground stays sharp on a
/// mesh that cannot refine any further.
fn levels_above_terrain(terrain_level: u32) -> u32 {
    match terrain_level {
        // Continental stand-ins: match the geometry, nothing more.
        0..=7 => 0,
        8..=11 => 1,
        // Deep enough that this is what is being looked at.
        _ => 2,
    }
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

/// A small FIFO cache of decoded imagery tiles, so the many terrain tiles that
/// share a coarse tile (ancestors, horizon) don't each re-decode it. (The
/// network cache — disk, in-memory — belongs to the injected provider.)
struct ImageryCache {
    map: HashMap<ImageryCoord, DecodedTexture>,
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

    fn get(&self, c: &ImageryCoord) -> Option<DecodedTexture> {
        self.map.get(c).cloned()
    }

    fn put(&mut self, c: ImageryCoord, tex: DecodedTexture) {
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
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobeOptions {
    /// When true, terrain only (no imagery) — the geometry debug view.
    pub no_imagery: bool,
}

/// Loads a terrain tile and drapes the matched-LOD imagery mosaic covering it.
/// Generic over the terrain source `T` and imagery provider `I` — no ion, no
/// Bing, no transport.
pub struct PlanetaryLoader<T: TerrainSource, I: ImageryProvider> {
    terrain: T,
    imagery: I,
    scheme: GeographicTilingScheme,
    opts: GlobeOptions,
    cache: Mutex<ImageryCache>,
    /// Shared with the tree: each terrain tile's `metadata` extension reveals
    /// the availability of its descendants, folded in here so traversal can
    /// keep refining toward the finest LOD.
    availability: Arc<Availability>,
    /// Host-updated imagery detail target (drives imagery level by altitude).
    detail: ImageryDetail,
    /// Shared with the host's camera: each decoded tile's relief, so the eye can
    /// be kept above the ground rather than above the ellipsoid.
    heights: Arc<TerrainHeights>,
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

    /// One decoded imagery tile. The small in-process cache spares the *decode*
    /// for the many terrain tiles that share a coarse tile; persistence across
    /// runs is the provider's business, not this one's.
    async fn fetch_imagery(&self, c: ImageryCoord) -> Result<DecodedTexture, LoadError> {
        if let Some(tex) = self.cache.lock().expect("imagery cache").get(&c) {
            return Ok(tex);
        }
        let tex = match self.imagery.fetch_tile(c).await {
            Ok(fetched) => {
                tracing::debug!(z = c.level, x = c.x, y = c.y, "imagery tile");
                fetched.value
            }
            // Imagery absent at this zoom: upsample from the nearest available
            // ancestor (a quadrant of the parent texture, scaled up). Lets us
            // chase the finest imagery the provider has, falling back tile-by-
            // tile where it runs out instead of failing the whole drape.
            Err(RasterError::Fetch(FetchError::NotFound(_))) if c.level > 0 => {
                let parent = ImageryCoord {
                    level: c.level - 1,
                    x: c.x / 2,
                    y: c.y / 2,
                };
                let ptex = Box::pin(self.fetch_imagery(parent)).await?;
                tracing::debug!(
                    z = c.level,
                    x = c.x,
                    y = c.y,
                    "imagery upsampled (parent fallback)"
                );
                raster::upsample_quadrant(&ptex, (c.x & 1) as u32, (c.y & 1) as u32)
            }
            Err(e) => return Err(LoadError::Failed(format!("imagery {c:?}: {e}"))),
        };
        self.cache
            .lock()
            .expect("imagery cache")
            .put(c, tex.clone());
        Ok(tex)
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
        // …then refine the imagery with the camera ALTITUDE, beyond the terrain
        // LOD, up to the provider's max. Where terrain data runs out (Cesium
        // World Terrain caps ~z13 over Europe) the mesh stays coarse but the
        // ground stays sharp: a giant fine-imagery mosaic draped on it. Missing
        // deep tiles upsample from their parent (see fetch_imagery).
        // …but only so far. Left unbounded, the altitude target overshoots the
        // mosaic cap for nearly every tile at low altitude, and they all end up
        // with the same 1024² image — see [`levels_above_terrain`].
        let level = match self.detail.target() {
            Some(texel) => scheme.level_for_texel_spacing(texel, lat).max(base),
            None => base,
        }
        .min(base + levels_above_terrain(terrain_level))
        .min(scheme.maximum_level);
        let mosaic = scheme.mosaic_at_level(rect, level, IMAGERY_MOSAIC_CAP);
        let coords = mosaic.tiles();
        let fetched =
            futures_util::future::join_all(coords.iter().map(|c| self.fetch_imagery(*c))).await;
        let mut texs = Vec::with_capacity(coords.len());
        for tex in fetched {
            texs.push(tex?);
        }
        let stitched = raster::stitch_mosaic(&texs, mosaic.cols, mosaic.rows, scheme.tile_size);
        let ext = scheme.mosaic_extent(&mosaic);
        // Reproject the mosaic to geographic (no pole pinwheel), drape with
        // linear lon/lat UVs.
        let geographic = raster::reproject_to_geographic(&stitched, ext, rect, scheme.projection);
        let uvs = content
            .meshes
            .iter()
            .map(|m| raster::uvs_geographic(&m.positions, content.local_origin_ecef, rect))
            .collect();
        raster::drape_single(
            content,
            &OverlayAttachment {
                imagery: ImageryCoord {
                    level: mosaic.level,
                    x: mosaic.x0,
                    y: mosaic.y0,
                },
                uvs,
            },
            geographic,
        );
        Ok(())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> TileLoader
    for PlanetaryLoader<T, I>
{
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let (z, x, y) = id.terrain_coord();
        let coord = TileCoord::new(z, x, y);
        let fetched = self
            .terrain
            .fetch_tile(coord)
            .await
            .map_err(|e| LoadError::Failed(format!("terrain {z}/{x}/{y}: {e}")))?;
        tracing::debug!(z, x, y, kib = fetched.value.len() / 1024, "terrain tile");
        let qm = decode(&fetched.value).map_err(|e| LoadError::Failed(e.to_string()))?;
        // Decoding is also what reveals which descendants exist — the source may
        // have served these bytes from a cache, but the ranges still reach the
        // shared availability, so refinement never stalls at a cached level.
        self.reveal(coord, qm.metadata_available.as_deref());
        // The header already carries the tile's relief, so the surface a camera
        // is clamped against sharpens for free as the globe refines.
        self.heights.record(coord, qm.header.max_height);
        let rect = self.scheme.tile_rect(coord);
        // Skirts dropped: same-level neighbours share edges; skirts would show
        // as textured smears at the many LOD boundaries an SSE-2 globe makes.
        let mut content = to_decoded(&qm, &rect, 0.0);
        if !self.opts.no_imagery {
            let georect = GeoRect {
                west: rect.west,
                south: rect.south,
                east: rect.east,
                north: rect.north,
            };
            self.drape(&mut content, &georect, z).await?;
        }
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
    // One growing availability, shared by the tree (reader) and loader (writer):
    // the loader folds in each tile's `metadata` ranges, the tree refines on them.
    let scheme = GeographicTilingScheme::default();
    let availability = Arc::new(Availability::from_layer(
        &layer,
        scheme.root_tiles_x,
        scheme.root_tiles_y,
    ));
    let tree: Box<dyn TileTree> = Box::new(TerrainTree::with_availability(
        layer,
        Arc::clone(&availability),
    ));
    let detail = ImageryDetail::default();
    let heights = Arc::new(TerrainHeights::new(scheme));
    let loader: Arc<dyn TileLoader> = Arc::new(PlanetaryLoader {
        terrain,
        imagery,
        scheme,
        opts,
        cache: Mutex::new(ImageryCache::new(512)),
        availability,
        detail: detail.clone(),
        heights: Arc::clone(&heights),
    });
    (tree, loader, detail, heights)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound exists so a tile's drape scales with what the tile is for. A
    /// coarse stand-in and a tile the eye actually reads must not end up with
    /// the same image, which is what happened when only the camera's altitude
    /// decided.
    #[test]
    fn a_coarse_tile_is_draped_more_cheaply_than_a_deep_one() {
        assert!(levels_above_terrain(4) < levels_above_terrain(14));
    }

    /// The allowance may never decrease with depth: a tile drawn under closer
    /// scrutiny cannot be given less imagery than its own ancestor.
    #[test]
    fn the_allowance_never_decreases_with_depth() {
        let mut previous = levels_above_terrain(0);
        for level in 1..=22 {
            let current = levels_above_terrain(level);
            assert!(
                current >= previous,
                "level {level} allows {current}, less than {previous} above it"
            );
            previous = current;
        }
    }

    /// Two levels above the match already fills the mosaic cap (4×4). Allowing
    /// more would be a bound in name only — the coarsening in `mosaic_at_level`
    /// would silently claw it back.
    #[test]
    fn the_allowance_stays_within_the_mosaic_cap() {
        let widest = (0..=22).map(levels_above_terrain).max().expect("levels");
        let tiles_per_side = 1u32 << widest;
        assert!(
            tiles_per_side * tiles_per_side <= IMAGERY_MOSAIC_CAP,
            "{widest} levels needs {}×{} tiles, over the {IMAGERY_MOSAIC_CAP} cap",
            tiles_per_side,
            tiles_per_side
        );
    }
}
