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
use tuile_core::raster::{self, GeoRect, ImageryCoord, ImageryProvider, RasterError};
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_terrain::{
    decode, level_geometric_error, to_decoded, Availability, AvailabilityRange,
    GeographicTilingScheme, LayerJson, TerrainHeights, TerrainSource, TerrainTree, TileCoord,
};

use tuile_core::raster::MAX_IMAGERY_LAYERS;
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
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobeOptions {
    /// When true, terrain only (no imagery) — the geometry debug view.
    pub no_imagery: bool,
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
        match self.imagery.fetch_tile(c).await {
            Ok(fetched) => {
                tracing::debug!(z = c.level, x = c.x, y = c.y, "imagery tile");
                let tex = Arc::new(
                    raster::reproject_tile_to_geographic(&fetched.value, &scheme, c)
                        .unwrap_or(fetched.value),
                );
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
    fn terrain_mesh<'a>(
        &'a self,
        coord: TileCoord,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Arc<tuile_terrain::QuantizedMesh>, LoadError>>
                + Send
                + 'a,
        >,
    > {
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
                match self.terrain.fetch_tile(coord).await {
                    Ok(fetched) => {
                        tracing::debug!(
                            z = coord.level,
                            x = coord.x,
                            y = coord.y,
                            kib = fetched.value.len() / 1024,
                            "terrain tile"
                        );
                        let decoded =
                            decode(&fetched.value).map_err(|e| LoadError::Failed(e.to_string()))?;
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
        // textures one draw binds — see [`MAX_IMAGERY_LAYERS`] — where it used
        // to be bounded by a per-tile memory cost that no longer exists.
        let level = match self.detail.target() {
            Some(texel) => scheme.level_for_texel_spacing(texel, lat).max(base),
            None => base,
        }
        .min(scheme.maximum_level);
        let mosaic = scheme.mosaic_at_level(rect, level, MAX_IMAGERY_LAYERS);
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

        let rows = scheme.tiles_at(mosaic.level).1;
        let mut layers = Vec::with_capacity(coords.len());
        for (requested, got) in coords.iter().zip(fetched) {
            let (served, texture) = got?;
            let layer = raster::ImageryLayer::substituted(
                served,
                texture,
                rect,
                &scheme.tile_rect(served),
                &to_the_pole(scheme.tile_rect(*requested), *requested, rows),
            );
            // A tile the mosaic's bounding box included but the rectangle only
            // touches contributes no pixels and would still cost a binding.
            if layer.is_visible() {
                layers.push(layer);
            }
        }

        // The uv set is the TILE's own space, and the mesh already stated it —
        // `to_decoded` carries it straight from the quantized mesh. It used to
        // be recovered here instead, by projecting every vertex back to a
        // longitude and measuring it against the tile's rectangle, which is
        // slower and tears at the antimeridian: see `tuile_terrain::surface_uvs`.
        // Every layer maps out of that space by its own affine transform, so the
        // vertices carry no imagery level and a layer can be swapped without
        // touching the geometry.
        for mesh in &mut content.meshes {
            // Terrain owns no base-colour texture; the layers are the ground.
            mesh.material.base_color_texture = None;
        }
        content.imagery = layers;
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
        let qm = self.terrain_mesh(coord).await?;
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
        availability,
        detail: detail.clone(),
        heights: Arc::clone(&heights),
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
        for level in 0..=scheme.maximum_level {
            let mosaic = scheme.mosaic_at_level(&rect, level, MAX_IMAGERY_LAYERS);
            assert!(
                mosaic.tile_count() <= MAX_IMAGERY_LAYERS,
                "level {level} produced {} layers",
                mosaic.tile_count()
            );
        }
    }
}
