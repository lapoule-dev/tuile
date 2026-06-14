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
use tuile_core::content::DecodedTexture;
use tuile_core::geo::WGS84_A;
use tuile_core::raster::{self, GeoRect, ImageryCoord, ImageryProvider, OverlayAttachment};
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_terrain::{
    decode, level_geometric_error, to_decoded, GeographicTilingScheme, LayerJson, TerrainSource,
    TerrainTree, TileCoord,
};

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
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobeOptions {
    /// When false, terrain only (no imagery) — the geometry debug view.
    pub no_imagery: bool,
    /// Log each terrain/imagery tile as it loads (batch progress).
    pub log: bool,
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
}

impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> PlanetaryLoader<T, I> {
    async fn fetch_imagery(&self, c: ImageryCoord) -> Result<DecodedTexture, LoadError> {
        if let Some(tex) = self.cache.lock().expect("imagery cache").get(&c) {
            return Ok(tex);
        }
        let tex = self
            .imagery
            .fetch_tile(c)
            .await
            .map_err(|e| LoadError::Failed(format!("imagery {c:?}: {e}")))?;
        if self.opts.log {
            eprintln!("  ⬇ imagery {}/{}/{}", c.level, c.x, c.y);
        }
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
        // Cesium: pick the imagery level whose texel spacing matches the
        // terrain tile's geometric error — imagery exactly as detailed as the
        // geometry it drapes (max at the nadir, coarse toward the horizon).
        let ge = level_geometric_error(terrain_level, WGS84_A, self.scheme.root_tiles_x);
        let lat = 0.5 * (rect.south + rect.north);
        let level = scheme.level_for_texel_spacing(ge, lat);
        let mosaic = scheme.mosaic_at_level(rect, level, 16);
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

#[async_trait]
impl<T: TerrainSource + 'static, I: ImageryProvider + 'static> TileLoader for PlanetaryLoader<T, I> {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let (z, x, y) = id.terrain_coord();
        let coord = TileCoord::new(z, x, y);
        let bytes = self
            .terrain
            .fetch_tile(coord)
            .await
            .map_err(|e| LoadError::Failed(format!("terrain {z}/{x}/{y}: {e}")))?;
        if self.opts.log {
            eprintln!("⬇ terrain {z}/{x}/{y}  ({} KB)", bytes.len() / 1024);
        }
        let qm = decode(&bytes).map_err(|e| LoadError::Failed(e.to_string()))?;
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
/// `(tree, loader)` ready for `tuile_core::runtime::in_process_with`. `layer`
/// is the terrain's `layer.json` (the host resolves it). Pure: the host has
/// already chosen the backends and transport.
pub fn globe<T, I>(
    terrain: T,
    imagery: I,
    layer: LayerJson,
    opts: GlobeOptions,
) -> (Box<dyn TileTree>, Arc<dyn TileLoader>)
where
    T: TerrainSource + 'static,
    I: ImageryProvider + 'static,
{
    let tree = terrain_tree(layer);
    let loader: Arc<dyn TileLoader> = Arc::new(PlanetaryLoader {
        terrain,
        imagery,
        scheme: GeographicTilingScheme::default(),
        opts,
        cache: Mutex::new(ImageryCache::new(512)),
    });
    (tree, loader)
}
