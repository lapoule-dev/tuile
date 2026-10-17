// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The shared tile store, when the job is given one.
//!
//! `TUILE_TILES_BUCKET` names the bucket every bake and the tile server
//! share; the endpoint and credentials are the job's usual `TUILE_STORE_*`.
//! Unset, a bake fetches every tile from its source, as before.
//! `TUILE_TILES_PREFIX` puts the store under a prefix of the bucket.
//!
//! Source tiles are then read from the store when an earlier run (of any job,
//! or the server) fetched them, and each tile fetched now is offered to it.
//! What is stored is what the source served, so the pack does not depend on
//! whether the store was there.

use std::sync::Arc;

use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use tuile_tile_server::projection::width_m;
use tuile_tile_server::{Eye, Footprint, Grid, LayerDef, StoreConfig, StoreContent, TileStore};

const DEFAULT_REGION: &str = "auto";
/// Projections kept, in GiB, unless `TUILE_TILES_DIR_GIB` says.
const DEFAULT_PROJECTIONS_GIB: u64 = 40;

/// The files under `dir`, oldest first by when they were last written,
/// removed until what is left holds at most `budget` bytes. A projection is
/// written again by every bake that uses it, so the oldest is the one no
/// bake has used for longest. A file still open by another bake is read
/// to its end: removing it only unlinks it. Returns the files removed and
/// their bytes.
fn evict_oldest(dir: &std::path::Path, budget: u64) -> (u64, u64) {
    fn walk(
        dir: &std::path::Path,
        out: &mut Vec<(std::time::SystemTime, u64, std::path::PathBuf)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            // What a projection is being made of, or into: not ours to take.
            if entry.file_name().to_string_lossy().starts_with(".tmp") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), out);
            } else if let Ok(at) = meta.modified() {
                out.push((at, meta.len(), entry.path()));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    let mut held: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    let (mut removed, mut freed) = (0, 0);
    for (_, bytes, path) in files {
        if held <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            held -= bytes;
            removed += 1;
            freed += bytes;
        }
    }
    (removed, freed)
}

/// Where the scene's projections are written, unless `TUILE_TILES_DIR` says.
const DEFAULT_PROJECTION_DIR: &str = "/tmp/tuile-tiles";
/// Imagery is kept eight times farther than terrain: a 2048² drape composes
/// imagery about three levels finer than the terrain tile it covers.
const IMAGERY_OVER_TERRAIN: f64 = 8.0;
/// What the terrain factor keeps beyond the farthest tile a bake can read
/// ([`terrain_tile_factor`]): the fallback counters say whether it is enough.
const FACTOR_MARGIN: f64 = 1.5;

/// What a bake selects with: what sizes the slice of the store it projects.
#[derive(Debug, Clone, Copy)]
pub struct Sight<'a> {
    /// The store's layers of the bake's terrain, and of its imagery.
    pub terrain_layer: &'a str,
    pub imagery_layer: Option<&'a str>,
    /// The traversal the bake selects with, resolved
    /// ([`crate::exact_traversal`]): its screen-space error and hysteresis.
    pub traversal: &'a tuile_core::traversal::Config,
    /// The picture's height, in pixels.
    pub viewport_height_px: f64,
}

/// How many of its own widths from a camera a terrain tile can still be read
/// by a bake, with a margin: the footprint's factor for the terrain.
///
/// A quantized-mesh tile's geometric error is a fixed fraction of its width
/// (about 1/260, [`tuile_terrain::level_geometric_error`]). Its screen-space
/// error at distance `d` is `error × height / (d × 2 tan(fovy / 2))`, and it
/// is refined while that exceeds the threshold — down to `τ (1 − h)` for a
/// tile already refined, the hysteresis: so within
/// `ρ = error / width × height / (τ (1 − h) × 2 tan(fovy / 2))` of its
/// widths. Its four children are then read, the farthest a parent's
/// diagonal beyond it: `2ρ + 2√2` of their widths. A screen-space error of
/// 6 on a 1280-pixel-high picture at 45° reads within about 5.2 widths; of 3
/// on 2880 pixels, within about 13.
pub fn terrain_tile_factor(
    traversal: &tuile_core::traversal::Config,
    viewport_height_px: f64,
    fovy_rad: f64,
) -> f64 {
    let error_per_width = tuile_terrain::level_geometric_error(0, tuile_core::geo::WGS84_A, 2)
        / width_m(Grid::Geographic, 0, 0, 0);
    let threshold =
        traversal.maximum_screen_space_error * (1.0 - traversal.refine_hysteresis.clamp(0.0, 0.9));
    let rho = error_per_width * viewport_height_px / (threshold * 2.0 * (fovy_rad / 2.0).tan());
    FACTOR_MARGIN * (2.0 * rho + 2.0 * std::f64::consts::SQRT_2)
}
/// A bake has no use for other writers' newest deltas: it reads its scene
/// from its projections, and its own tiles from memory.
const BAKE_MANIFEST_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The store, and the runtime its final flush runs on.
pub struct Tiles {
    runtime: tokio::runtime::Runtime,
    store: Arc<TileStore>,
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

impl Tiles {
    /// Opens the store named by the environment, or `None` when there is none.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_env_with(None)
    }

    /// The same, with `layer` declared in the store's catalog first if the
    /// store does not know it yet: a store keeps a source's tiles only once
    /// it has a layer for them. A layer the store already has is left as the
    /// store describes it. Says whether the layer was added.
    pub fn from_env_with(layer: Option<LayerDef>) -> Result<Option<Self>, String> {
        Self::from_env_with_layers(layer.into_iter().collect())
    }

    /// The same, with every one of `layers` declared that the store does not
    /// know yet. The store is the bucket `TUILE_TILES_BUCKET`, or a
    /// directory here, `TUILE_TILES_LOCAL` — the same store, its archives,
    /// manifests and catalog written the same way, on a disk.
    pub fn from_env_with_layers(layers: Vec<LayerDef>) -> Result<Option<Self>, String> {
        let (bucket, s3): (String, Arc<dyn object_store::ObjectStore>) =
            match var("TUILE_TILES_BUCKET") {
                Ok(bucket) => {
                    let s3 = AmazonS3Builder::new()
                        .with_bucket_name(&bucket)
                        .with_endpoint(var("TUILE_STORE_ENDPOINT")?)
                        .with_region(
                            std::env::var("TUILE_STORE_REGION")
                                .unwrap_or_else(|_| DEFAULT_REGION.into()),
                        )
                        .with_access_key_id(var("TUILE_STORE_ACCESS_KEY_ID")?)
                        .with_secret_access_key(var("TUILE_STORE_SECRET_ACCESS_KEY")?)
                        // The store's manifests are replaced by conditional writes only.
                        .with_conditional_put(S3ConditionalPut::ETagMatch)
                        .build()
                        .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
                    (bucket, Arc::new(s3))
                }
                Err(_) => match std::env::var("TUILE_TILES_LOCAL") {
                    Ok(dir) => {
                        // Its manifests replaced on condition, as a bucket's.
                        let local = crate::local_store::LocalStore::new(dir.as_ref())?;
                        (dir, Arc::new(local))
                    }
                    Err(_) => return Ok(None),
                },
            };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("tile store runtime: {e}"))?;
        // A store of its own inside a bucket that holds another: a trial that
        // shares a real bucket without touching what it serves.
        let s3: Arc<dyn object_store::ObjectStore> = match var("TUILE_TILES_PREFIX") {
            Ok(prefix) => Arc::new(object_store::prefix::PrefixStore::new(s3, prefix)),
            Err(_) => s3,
        };
        if !layers.is_empty() {
            runtime
                .block_on(async {
                    // A store with no catalog yet starts with an empty one.
                    let mut catalog = tuile_tile_server::catalog::read(s3.as_ref())
                        .await
                        .unwrap_or_default();
                    let mut added = false;
                    for layer in layers {
                        if catalog.layers.iter().any(|l| l.name == layer.name) {
                            tracing::info!(layer = %layer.name, "TILES-LAYER-KNOWN");
                            continue;
                        }
                        tracing::info!(layer = %layer.name, grid = ?layer.grid, "TILES-LAYER-ADDED");
                        catalog.layers.push(layer);
                        added = true;
                    }
                    if added {
                        tuile_tile_server::catalog::write(s3.as_ref(), &catalog).await?;
                    }
                    Ok::<_, tuile_tile_server::StoreError>(())
                })
                .map_err(|e| format!("tile store {bucket}: its catalog: {e}"))?;
        }
        let store = runtime
            .block_on(TileStore::open(
                s3,
                StoreConfig {
                    manifest_ttl: BAKE_MANIFEST_TTL,
                    ..StoreConfig::default()
                },
            ))
            .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
        tracing::info!(bucket, layers = store.layers().count(), "TILES-OPEN");
        Ok(Some(Self {
            runtime,
            store: Arc::new(store),
        }))
    }

    /// The store on `objects`, whose catalog names its layers: a bucket of
    /// the caller's own (a test's, in memory).
    pub fn over(objects: Arc<dyn object_store::ObjectStore>) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("tile store runtime: {e}"))?;
        let store = runtime
            .block_on(TileStore::open(
                objects,
                StoreConfig {
                    manifest_ttl: BAKE_MANIFEST_TTL,
                    ..StoreConfig::default()
                },
            ))
            .map_err(|e| format!("tile store: {e}"))?;
        Ok(Self {
            runtime,
            store: Arc::new(store),
        })
    }

    /// From now on a projection by reference asks `reader` — the host's
    /// register — which zones a layer has and the copy a reader gets of each
    /// tile, rather than listing the bucket and reading archives: see
    /// [`tuile_tile_server::IndexReader`]. Set before the store is shared.
    pub fn with_index_reader(
        mut self,
        reader: Arc<dyn tuile_tile_server::IndexReader>,
    ) -> Result<Self, String> {
        Arc::get_mut(&mut self.store)
            .ok_or("the tile store is already shared: set its index reader first")?
            .set_index_reader(reader);
        Ok(self)
    }

    /// From now on the store's absences are kept, and read, by `absences`
    /// rather than the bucket: see [`tuile_tile_server::absences`]. Set
    /// before the store is shared.
    pub fn with_absences(
        mut self,
        absences: Arc<dyn tuile_tile_server::Absences>,
    ) -> Result<Self, String> {
        Arc::get_mut(&mut self.store)
            .ok_or("the tile store is already shared: set its absence store first")?
            .set_absences(absences);
        Ok(self)
    }

    /// `with` given the store before it is shared, to set what its host
    /// keeps beside it — a ledger told of what the bake writes, an index
    /// reader, an absence store — from the store's own layers.
    pub fn with_store(
        mut self,
        with: impl FnOnce(&mut tuile_tile_server::TileStore),
    ) -> Result<Self, String> {
        with(
            Arc::get_mut(&mut self.store)
                .ok_or("the tile store is already shared: set it up first")?,
        );
        Ok(self)
    }

    /// Where the store's reads came from so far.
    pub fn stats(&self) -> tuile_tile_server::StatsSnapshot {
        self.store.stats()
    }

    /// Says loudly when a source has no layer in the store: its tiles would
    /// silently not be kept.
    pub fn check(&self, namespaces: &[String]) {
        for ns in namespaces {
            if self.store.layer(ns).is_err() {
                tracing::warn!(layer = %ns, "TILES-NO-LAYER: this source's tiles are fetched but not kept");
            }
        }
    }

    /// Projects the part of the store the scene's cameras can use into local
    /// archives, read before the bucket for the rest of the bake: the
    /// terrain's tiles one by one, as far as `sight` can read them
    /// ([`terrain_tile_factor`]); the imagery's zones whole. A tile left out
    /// is read from the bucket, and counted (`TILES-READS`).
    ///
    /// `TUILE_TILES_FACTOR` and `TUILE_TILES_IMAGERY_FACTOR` override the
    /// factors.
    pub fn project(&self, poses: &[tuile_tape::Frame], sight: &Sight<'_>) -> Result<(), String> {
        self.project_as(poses, sight, false)
    }

    /// The same, for a bake by reference: its imagery is named and never
    /// drawn, so the imagery layer is projected by index — which tiles each
    /// zone holds, and a stamp of each, from the register
    /// ([`Self::with_index_reader`]) — and not a byte of imagery comes down
    /// for a zone the register knows whole. Without a register, or for a
    /// zone it does not know, the imagery is projected from its archives.
    /// The terrain is projected tile by tile all the same.
    pub fn project_by_reference(
        &self,
        poses: &[tuile_tape::Frame],
        sight: &Sight<'_>,
    ) -> Result<(), String> {
        self.project_as(poses, sight, true)
    }

    fn project_as(
        &self,
        poses: &[tuile_tape::Frame],
        sight: &Sight<'_>,
        imagery_by_index: bool,
    ) -> Result<(), String> {
        let eyes = poses.iter().map(|p| {
            let g = tuile_core::geo::ecef_to_geodetic(glam::DVec3::from_array(p.position));
            Eye {
                lon: g.lon.to_degrees(),
                lat: g.lat.to_degrees(),
                height: g.height,
            }
        });
        // The narrowest field of view refines the farthest.
        let fovy = poses
            .iter()
            .map(|p| p.fovy)
            .filter(|f| *f > 0.0)
            .fold(f64::INFINITY, f64::min);
        let derived = terrain_tile_factor(
            sight.traversal,
            sight.viewport_height_px,
            if fovy.is_finite() { fovy } else { 1.0 },
        );
        let factor = std::env::var("TUILE_TILES_FACTOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(derived);
        let imagery_factor = std::env::var("TUILE_TILES_IMAGERY_FACTOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(factor * IMAGERY_OVER_TERRAIN);
        // The layers this bake reads, and no other the catalog holds (a
        // reference imagery is read by a render, never by a bake).
        let mut footprint = Footprint::from_eyes(eyes, factor)
            .with_targeted_layer(sight.terrain_layer)
            .with_only_layers(std::iter::once(sight.terrain_layer).chain(sight.imagery_layer));
        if let Some(name) = sight.imagery_layer {
            footprint = footprint.with_layer_factor(name, imagery_factor);
            if imagery_by_index {
                footprint = footprint.with_layer_by_index(name);
            }
        }
        let dir =
            std::env::var("TUILE_TILES_DIR").unwrap_or_else(|_| DEFAULT_PROJECTION_DIR.into());
        let report = self.project_footprint(&footprint, std::path::Path::new(&dir))?;
        tracing::info!(
            eyes = footprint.eyes().len(),
            factor,
            imagery_factor,
            sse = sight.traversal.maximum_screen_space_error,
            viewport_height = sight.viewport_height_px,
            fovy,
            zones_seen = report.zones_seen,
            zones = report.zones_projected,
            tiles_listed = report.tiles_listed,
            tiles = report.tiles_kept,
            mb = report.bytes as f64 / 1e6,
            whole_mb = report.whole_bytes as f64 / 1e6,
            requests = report.requests,
            zones_indexed = report.zones_indexed,
            tiles_indexed = report.tiles_indexed,
            zones_registered = report.zones_registered,
            tiles_registered = report.tiles_registered,
            register_batches = report.register_batches,
            register_s = report.register_millis as f64 / 1e3,
            layers_listed = report.layers_listed,
            layers_from_register = report.layers_from_register,
            seconds = report.millis as f64 / 1e3,
            "TILES-PROJECTED"
        );
        for (layer, l) in &report.layers {
            tracing::info!(
                layer,
                targeted = l.targeted,
                zones_seen = l.zones_seen,
                zones = l.zones_projected,
                tiles_listed = l.tiles_listed,
                tiles = l.tiles_kept,
                mb = l.bytes as f64 / 1e6,
                whole_mb = l.whole_bytes as f64 / 1e6,
                requests = l.requests,
                top_tiles = l.top_tiles,
                top_mb = l.top_bytes as f64 / 1e6,
                top_requests = l.top_requests,
                zones_indexed = l.zones_indexed,
                tiles_indexed = l.tiles_indexed,
                "TILES-PROJECTED-LAYER"
            );
        }
        Ok(())
    }

    /// Projects what `footprint` keeps into `dir`, the projections there
    /// least recently made evicted first to keep under their budget
    /// (`TUILE_TILES_DIR_GIB`).
    pub fn project_footprint(
        &self,
        footprint: &Footprint,
        dir: &std::path::Path,
    ) -> Result<tuile_tile_server::ProjectionReport, String> {
        // Projections stay after their bake, for the next one over the same
        // ground; the least recently made go first to keep under a budget.
        let budget = std::env::var("TUILE_TILES_DIR_GIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PROJECTIONS_GIB)
            << 30;
        let (evicted, freed) = evict_oldest(dir, budget);
        if evicted > 0 {
            tracing::info!(
                evicted,
                mb = freed as f64 / 1e6,
                budget_gib = budget >> 30,
                "TILES-PROJECTIONS-EVICTED"
            );
        }
        self.runtime
            .block_on(self.store.project(footprint, dir))
            .map_err(|e| format!("projecting the tile store: {e}"))
    }

    /// The grid a layer's tiles are addressed on, if the store has the layer.
    pub fn grid(&self, layer: &str) -> Option<Grid> {
        self.store.layer(layer).ok().map(|l| l.grid)
    }

    pub fn cache(&self) -> crate::TileCache {
        crate::TileCache(Arc::new(StoreContent::new(self.store.clone())))
    }

    /// The store as a cache that is read and never written: for a bake whose
    /// sources keep their own tiles (a host's tile server, see
    /// [`crate::Sources`]). Such a bake has nothing to flush.
    pub fn read_only_cache(&self) -> crate::TileCache {
        crate::TileCache(Arc::new(StoreContent::read_only(self.store.clone())))
    }

    /// The tiles `asks` name, gathered into one archive put in the store's
    /// bucket under `key`: an extract, for a server of the bucket to serve
    /// (`tuile_tile_server::TileStore::extract_into`).
    pub fn extract_into(
        &self,
        asks: &[tuile_tile_server::ExtractAsk],
        key: &str,
    ) -> Result<tuile_tile_server::ExtractReport, String> {
        self.runtime
            .block_on(self.store.extract_into(asks, key))
            .map_err(|e| format!("extract {key}: {e}"))
    }

    /// The same, put only if the store holds every tile `asks` name: for a
    /// render, which draws every tile its pack names. A missing tile is
    /// [`tuile_tile_server::StoreError::Incomplete`], and nothing is put.
    pub fn extract_whole_into(
        &self,
        asks: &[tuile_tile_server::ExtractAsk],
        key: &str,
    ) -> Result<tuile_tile_server::ExtractReport, tuile_tile_server::StoreError> {
        self.runtime
            .block_on(self.store.extract_whole_into(asks, key))
    }

    /// Publishes whatever is still buffered. Called on the way out, success or
    /// not: tiles fetched by a failed bake are still worth keeping.
    pub fn flush(&self) -> Result<(), String> {
        let stats = self.store.stats();
        tracing::info!(
            buffer = stats.buffer,
            projection = stats.projection,
            projection_misses = stats.projection_misses,
            remote = stats.remote,
            absent = stats.absent,
            "TILES-READS"
        );
        let began = std::time::Instant::now();
        let zones = self
            .runtime
            .block_on(self.store.flush_all())
            .map_err(|e| format!("tile store flush: {e}"))?;
        tracing::info!(
            zones,
            seconds = began.elapsed().as_secs_f64(),
            "TILES-FLUSHED"
        );
        Ok(())
    }
}

#[cfg(test)]
mod factor_tests {
    use super::terrain_tile_factor;

    fn traversal(sse: f64, hysteresis: f64) -> tuile_core::traversal::Config {
        tuile_core::traversal::Config {
            maximum_screen_space_error: sse,
            refine_hysteresis: hysteresis,
            ..tuile_core::traversal::Config::default()
        }
    }

    const FOVY_45: f64 = std::f64::consts::FRAC_PI_4;

    #[test]
    fn the_terrain_factor_follows_the_picture_and_its_error() {
        let mobile = terrain_tile_factor(&traversal(6.0, 0.15), 1280.0, FOVY_45);
        let tall = terrain_tile_factor(&traversal(3.0, 0.15), 2880.0, FOVY_45);
        // ρ = 1/260 × 1280 / (6 × 0.85 × 0.828) ≈ 1.17: 1.5 × (2ρ + 2√2).
        assert!((mobile - 7.74).abs() < 0.05, "{mobile}");
        // ρ ≈ 5.25: 1.5 × (2ρ + 2√2).
        assert!((tall - 19.97).abs() < 0.1, "{tall}");
        // Twice the error, or half the picture: half the reach, the sibling
        // term apart.
        let sibling = 1.5 * 2.0 * std::f64::consts::SQRT_2;
        let half = terrain_tile_factor(&traversal(12.0, 0.15), 1280.0, FOVY_45);
        assert!(((mobile - sibling) / (half - sibling) - 2.0).abs() < 1e-9);
        let short = terrain_tile_factor(&traversal(6.0, 0.15), 640.0, FOVY_45);
        assert!((short - half).abs() < 1e-9);
        // A narrower field of view refines farther.
        assert!(terrain_tile_factor(&traversal(6.0, 0.15), 1280.0, 0.5) > mobile);
        // The hysteresis keeps a refined tile refined farther.
        assert!(terrain_tile_factor(&traversal(6.0, 0.0), 1280.0, FOVY_45) < mobile);
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::evict_oldest;

    #[test]
    fn the_oldest_projections_go_until_the_rest_fits() {
        let dir = tempfile::tempdir().expect("dir");
        let zone = |name: &str, bytes: usize, age_s: u64| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, vec![0u8; bytes]).expect("write");
            let at = std::time::SystemTime::now() - std::time::Duration::from_secs(age_s);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(at)
                .expect("time");
            path
        };
        let old = zone("asset-1/zones/z4/1/2.pmtiles", 400, 300);
        let middle = zone("asset-2/zones/z4/1/2.pmtiles", 400, 200);
        let new = zone("asset-2/top.pmtiles", 400, 100);
        // Being written by another bake, however old: left alone.
        let making = zone(".tmpAbC/0.pmtiles", 400, 900);
        let renaming = zone("asset-1/zones/z4/.tmpXyZ", 400, 900);
        assert_eq!(evict_oldest(dir.path(), 1000), (1, 400));
        assert!(!old.exists() && middle.exists() && new.exists());
        assert_eq!(
            evict_oldest(dir.path(), 1000),
            (0, 0),
            "under the budget: nothing goes"
        );
        assert_eq!(evict_oldest(dir.path(), 0), (2, 800));
        assert!(making.exists() && renaming.exists());
        assert_eq!(evict_oldest(&dir.path().join("none"), 0), (0, 0));
    }
}
