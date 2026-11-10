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
//!
//! **Nothing downloaded twice.** A process opens the store once
//! ([`SharedTiles`]) and every bake and converge of it reads through that one
//! store: its catalog read once (and checked by its e-tag before each use),
//! its archives' listings and its projections made once. The store's
//! archives are immutable, so they are kept by key in tiers read before the
//! bucket ([`tuile_tile_server::tiered`]):
//!
//! - a disk here, `TUILE_TILES_CACHE_DIR` (`TUILE_TILES_CACHE_GIB`, 8 by
//!   default): whole archives, the least recently read evicted first. The
//!   processes of one machine may share it;
//! - a bucket shared by every machine, `TUILE_TILES_GLOBAL`: `gs://bucket/
//!   prefix` (credentials as `object_store` reads them from the environment)
//!   or `file:///dir`. What the bucket gives is written there once. On GCS,
//!   an archive's custom time is set when it is written and moved forward
//!   when it is read, once a day at most ([`GcsCustomTime`]): the bucket's
//!   lifecycle deletes what has not been used for its days. A directory has
//!   no custom time and is never touched.
//!
//! The projections, under `TUILE_TILES_DIR`, are reused whenever the zone's
//! archives are the ones they were made from.
//!
//! **Its I/O on the host's runtime.** The bucket and the global tier are
//! read over HTTP by clients that pool their connections, and a connection
//! lives on the runtime that opened it. A shared store is read from the
//! runtime of every bake, each ending with its bake: a connection a bake
//! opened, handed by the pool to a converge, dies under the converge's
//! request when the bake ends (`error sending request`, which is not
//! retried). So every request of these clients runs on a runtime the host
//! passes in ([`tokio::runtime::Handle`]), one that lives as long as the
//! process and has threads of its own (a multi-threaded runtime): the
//! store's own runtime, and each bake's, only wait for the answers.

use std::sync::{Arc, Mutex};

use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::client::SpawnedReqwestConnector;
use tuile_tile_server::projection::width_m;
use tuile_tile_server::{
    DiskCacheConfig, Eye, Footprint, Grid, LayerDef, StoreConfig, StoreContent, Tier, Tiered,
    TileStore,
};

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

/// Worker threads of the store's runtime: what `TUILE_TILES_THREADS` says,
/// or the threads available to the process. Read when a store is opened,
/// which a bake does once.
///
/// It was two. Every lookup a bake makes against the store, absences
/// included, runs on this runtime, and two workers are the prime suspect
/// for bakes starving on a farm worker (not yet measured).
fn runtime_threads() -> usize {
    crate::session::count_knob("TUILE_TILES_THREADS")
        .unwrap_or_else(crate::session::available_threads)
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

/// Archives kept on the disk tier, in GiB, unless `TUILE_TILES_CACHE_GIB` says.
const DEFAULT_CACHE_GIB: u64 = 8;

/// A store's own runtime. Dropped where a runtime may not block — the last
/// handle on a shared store let go inside a task — it is shut down without
/// waiting rather than panicking.
struct OwnRuntime(Option<tokio::runtime::Runtime>);

impl std::ops::Deref for OwnRuntime {
    type Target = tokio::runtime::Runtime;
    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("a store's runtime lives as long as the store")
    }
}

impl Drop for OwnRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                runtime.shutdown_background();
            }
        }
    }
}

/// The store, and the runtime its final flush runs on.
pub struct Tiles {
    runtime: OwnRuntime,
    store: Arc<TileStore>,
    /// The catalog's e-tag when the store read it: see [`SharedTiles`].
    catalog_tag: Option<String>,
    /// The tiers before the bucket, when there are any.
    tiered: Option<Tiered>,
}

/// The tiers the environment names, nearest first: the disk's, then the
/// global one. See the [module](self).
#[derive(Debug, Default)]
pub struct Tiers {
    pub disk: Option<DiskCacheConfig>,
    pub global: Option<Tier>,
}

impl Tiers {
    /// Their clients' requests run on `io`: see the [module](self).
    pub fn from_env(io: &tokio::runtime::Handle) -> Result<Self, String> {
        let disk = var("TUILE_TILES_CACHE_DIR").ok().map(|dir| {
            let gib = std::env::var("TUILE_TILES_CACHE_GIB")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_CACHE_GIB);
            DiskCacheConfig {
                budget_bytes: gib << 30,
                ..DiskCacheConfig::new(dir)
            }
        });
        let global = match var("TUILE_TILES_GLOBAL") {
            Ok(url) => {
                let (objects, touch) = global_tier(&url, io)?;
                let tier = Tier::new("global", objects);
                Some(match touch {
                    Some(touch) => tier.with_touch(touch),
                    None => tier,
                })
            }
            Err(_) => None,
        };
        Ok(Self { disk, global })
    }
}

/// A global tier's objects, and what touches them when it has a custom
/// time.
type GlobalTier = (
    Arc<dyn object_store::ObjectStore>,
    Option<Arc<dyn tuile_tile_server::Touch>>,
);

/// The global tier at `url`: `gs://bucket[/prefix]` or `file:///dir`, a
/// bucket's requests (its objects' and its custom times') run on `io`.
fn global_tier(url: &str, io: &tokio::runtime::Handle) -> Result<GlobalTier, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("TUILE_TILES_GLOBAL {url}: {e}"))?;
    let prefix = parsed.path().trim_matches('/').to_string();
    let (objects, touch): GlobalTier = match parsed.scheme() {
        "gs" => {
            let bucket = parsed
                .host_str()
                .ok_or_else(|| format!("TUILE_TILES_GLOBAL {url}: no bucket"))?;
            let gcs = object_store::gcp::GoogleCloudStorageBuilder::from_env()
                .with_http_connector(SpawnedReqwestConnector::new(io.clone()))
                .with_bucket_name(bucket)
                .build()
                .map_err(|e| format!("TUILE_TILES_GLOBAL {url}: {e}"))?;
            let touch = GcsCustomTime::new(bucket, &prefix, gcs.credentials().clone(), io.clone())?;
            (Arc::new(gcs), Some(Arc::new(touch)))
        }
        "file" => {
            let dir = parsed
                .to_file_path()
                .map_err(|()| format!("TUILE_TILES_GLOBAL {url}: not a path"))?;
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            // A directory keeps no custom time: nothing to touch.
            return Ok((
                Arc::new(
                    object_store::local::LocalFileSystem::new_with_prefix(&dir)
                        .map_err(|e| format!("TUILE_TILES_GLOBAL {url}: {e}"))?,
                ),
                None,
            ));
        }
        other => {
            return Err(format!(
                "TUILE_TILES_GLOBAL {url}: {other}:// is not a tier"
            ))
        }
    };
    let objects = if prefix.is_empty() {
        objects
    } else {
        Arc::new(object_store::prefix::PrefixStore::new(objects, prefix))
    };
    Ok((objects, touch))
}

/// The JSON API's objects, where an object's metadata is patched.
const GCS_OBJECTS_API: &str = "https://storage.googleapis.com/storage/v1/b";

/// Moves a GCS object's custom time forward: a `PATCH` of its metadata
/// through the JSON API, `{"customTime": …}`. `object_store` 0.14 has
/// neither a custom time to put nor a way to update an object's metadata,
/// so this one request is made here, with the bucket's own credentials.
///
/// Each touch is a Class A operation: [`tuile_tile_server::Tiered`] touches
/// an archive once when it writes it and at most once a day after.
struct GcsCustomTime {
    bucket: String,
    prefix: String,
    credentials: object_store::gcp::GcpCredentialProvider,
    http: reqwest::Client,
    /// Where its requests run: the process's runtime, never a bake's (whose
    /// end would kill the client's pooled connections under a request).
    io: tokio::runtime::Handle,
}

impl std::fmt::Debug for GcsCustomTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GcsCustomTime(gs://{}/{})", self.bucket, self.prefix)
    }
}

impl GcsCustomTime {
    fn new(
        bucket: &str,
        prefix: &str,
        credentials: object_store::gcp::GcpCredentialProvider,
        io: tokio::runtime::Handle,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("the global tier's client: {e}"))?;
        Ok(Self {
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            credentials,
            http,
            io,
        })
    }

    /// The object's URL in the JSON API: its name one path segment,
    /// slashes and all percent-encoded, and only its custom time answered.
    fn url(&self, location: &object_store::path::Path) -> Result<url::Url, String> {
        let name = if self.prefix.is_empty() {
            location.to_string()
        } else {
            format!("{}/{location}", self.prefix)
        };
        let mut url = url::Url::parse(GCS_OBJECTS_API).map_err(|e| e.to_string())?;
        url.path_segments_mut()
            .map_err(|()| "the API's URL has no path".to_string())?
            .push(&self.bucket)
            .push("o")
            .push(&name);
        url.query_pairs_mut().append_pair("fields", "customTime");
        Ok(url)
    }
}

/// `at` as RFC 3339, to the millisecond, in UTC: what GCS reads a custom
/// time as.
fn rfc3339(at: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[async_trait::async_trait]
impl tuile_tile_server::Touch for GcsCustomTime {
    async fn touch(
        &self,
        location: &object_store::path::Path,
        at: std::time::SystemTime,
    ) -> object_store::Result<()> {
        let failed = |said: String| object_store::Error::Generic {
            store: "gcs custom time",
            source: said.into(),
        };
        let url = self.url(location).map_err(failed)?;
        let token = self.credentials.get_credential().await?;
        let request = self
            .http
            .patch(url)
            .bearer_auth(&token.bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"customTime":"{}"}}"#, rfc3339(at)));
        // On the process's runtime, as the bucket's own requests are.
        let answered = self
            .io
            .spawn(async move {
                let answer = request.send().await.map_err(|e| e.to_string())?;
                let status = answer.status();
                let body = if status.is_success() {
                    String::new()
                } else {
                    answer.text().await.unwrap_or_default()
                };
                Ok::<_, String>((status, body))
            })
            .await
            .map_err(|e| failed(format!("{location}: {e}")))?;
        let (status, body) = answered.map_err(|e| failed(format!("{location}: {e}")))?;
        if status.is_success() {
            return Ok(());
        }
        Err(failed(format!("{location}: {status}: {body}")))
    }
}

/// One tile store for every bake and converge of a process: opened on first
/// use, and opened again only when the bucket's catalog changed (its e-tag,
/// checked by one small request before each use).
///
/// A store shared this way reads each archive's listing once, makes each
/// projection once — two bakes that want the same zone at the same time
/// download it once, the second waiting for the first — and keeps the
/// archives it downloads in its tiers. What a host sets up beside the store
/// (a ledger, an index reader, an absence store) is set up by `open`, before
/// the store is shared.
///
/// [`Self::get`] is called off any async runtime: a store that is replaced
/// is dropped with its own runtime when its last bake lets it go.
pub struct SharedTiles {
    open: Box<dyn Fn() -> Result<Option<Tiles>, String> + Send + Sync>,
    current: Mutex<Option<Arc<Tiles>>>,
    opened: std::sync::atomic::AtomicU64,
}

impl SharedTiles {
    /// The store `open` opens, shared.
    pub fn new(open: impl Fn() -> Result<Option<Tiles>, String> + Send + Sync + 'static) -> Self {
        Self {
            open: Box::new(open),
            current: Mutex::new(None),
            opened: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The store the environment names ([`Tiles::from_env`]), its requests
    /// on `io` — a runtime that lives as long as the process — and each
    /// opening set up by `set_up`.
    pub fn from_env(
        io: tokio::runtime::Handle,
        set_up: impl Fn(Tiles) -> Result<Tiles, String> + Send + Sync + 'static,
    ) -> Self {
        Self::new(move || Tiles::from_env(&io)?.map(&set_up).transpose())
    }

    /// The store, `None` when the environment names none.
    pub fn get(&self) -> Result<Option<Arc<Tiles>>, String> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| "the shared tile store's lock was poisoned".to_string())?;
        if let Some(tiles) = current.as_ref() {
            if tiles.catalog_unchanged() {
                return Ok(Some(tiles.clone()));
            }
            tracing::info!("TILES-CATALOG-CHANGED: the store is opened again");
        }
        let fresh = (self.open)()?.map(Arc::new);
        self.opened
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *current = fresh.clone();
        Ok(fresh)
    }

    /// How many times the store was opened.
    pub fn opened(&self) -> u64 {
        self.opened.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The store's bucket, before it is told where it is: its requests on `io`.
fn s3_builder(io: &tokio::runtime::Handle) -> AmazonS3Builder {
    AmazonS3Builder::new()
        .with_http_connector(SpawnedReqwestConnector::new(io.clone()))
        // The store's manifests are replaced by conditional writes only.
        .with_conditional_put(S3ConditionalPut::ETagMatch)
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

/// The catalog's e-tag, `None` when it cannot be read.
async fn catalog_tag(objects: &dyn object_store::ObjectStore) -> Option<String> {
    use object_store::ObjectStoreExt;
    let at = object_store::path::Path::from(tuile_tile_server::catalog::CATALOG_KEY);
    objects.head(&at).await.ok().and_then(|meta| meta.e_tag)
}

impl Tiles {
    /// Opens the store named by the environment, or `None` when there is
    /// none. Its bucket's and its tiers' requests run on `io`, a runtime that
    /// lives as long as the process: see the [module](self).
    pub fn from_env(io: &tokio::runtime::Handle) -> Result<Option<Self>, String> {
        Self::from_env_with(io, None)
    }

    /// The same, with `layer` declared in the store's catalog first if the
    /// store does not know it yet: a store keeps a source's tiles only once
    /// it has a layer for them. A layer the store already has is left as the
    /// store describes it. Says whether the layer was added.
    pub fn from_env_with(
        io: &tokio::runtime::Handle,
        layer: Option<LayerDef>,
    ) -> Result<Option<Self>, String> {
        Self::from_env_with_layers(io, layer.into_iter().collect())
    }

    /// The same, with every one of `layers` declared that the store does not
    /// know yet. The store is the bucket `TUILE_TILES_BUCKET`, or a
    /// directory here, `TUILE_TILES_LOCAL` — the same store, its archives,
    /// manifests and catalog written the same way, on a disk.
    pub fn from_env_with_layers(
        io: &tokio::runtime::Handle,
        layers: Vec<LayerDef>,
    ) -> Result<Option<Self>, String> {
        let (bucket, s3): (String, Arc<dyn object_store::ObjectStore>) =
            match var("TUILE_TILES_BUCKET") {
                Ok(bucket) => {
                    let s3 = s3_builder(io)
                        .with_bucket_name(&bucket)
                        .with_endpoint(var("TUILE_STORE_ENDPOINT")?)
                        .with_region(
                            std::env::var("TUILE_STORE_REGION")
                                .unwrap_or_else(|_| DEFAULT_REGION.into()),
                        )
                        .with_access_key_id(var("TUILE_STORE_ACCESS_KEY_ID")?)
                        .with_secret_access_key(var("TUILE_STORE_SECRET_ACCESS_KEY")?)
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
        // A store of its own inside a bucket that holds another: a trial that
        // shares a real bucket without touching what it serves.
        let s3: Arc<dyn object_store::ObjectStore> = match var("TUILE_TILES_PREFIX") {
            Ok(prefix) => Arc::new(object_store::prefix::PrefixStore::new(s3, prefix)),
            Err(_) => s3,
        };
        Self::open(&bucket, s3, layers, Tiers::from_env(io)?).map(Some)
    }

    /// The store on `origin` (named `name` in what is said of it), `layers`
    /// declared that its catalog lacks, read through `tiers`.
    pub fn open(
        name: &str,
        origin: Arc<dyn object_store::ObjectStore>,
        layers: Vec<LayerDef>,
        tiers: Tiers,
    ) -> Result<Self, String> {
        let bucket = name;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(runtime_threads())
            .enable_all()
            .build()
            .map_err(|e| format!("tile store runtime: {e}"))?;
        let tiered = tiers
            .global
            .map(|global| Tiered::new(origin.clone(), vec![global]));
        let s3: Arc<dyn object_store::ObjectStore> = match &tiered {
            Some(t) => Arc::new(t.clone()),
            None => origin,
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
        let catalog_tag = runtime.block_on(catalog_tag(s3.as_ref()));
        let store = runtime
            .block_on(async {
                let store = TileStore::open(
                    s3,
                    StoreConfig {
                        manifest_ttl: BAKE_MANIFEST_TTL,
                        ..StoreConfig::default()
                    },
                )
                .await?;
                match tiers.disk {
                    Some(disk) => store.with_disk_cache(disk).await,
                    None => Ok(store),
                }
            })
            .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
        tracing::info!(
            bucket,
            layers = store.layers().count(),
            tiers = tiered.as_ref().map(|t| t.to_string()),
            disk_mb = store.disk_bytes() as f64 / 1e6,
            "TILES-OPEN"
        );
        Ok(Self {
            runtime: OwnRuntime(Some(runtime)),
            store: Arc::new(store),
            catalog_tag,
            tiered,
        })
    }

    /// Whether the catalog is still the one the store read: one small
    /// request. A request that fails says yes — the store goes on as it is.
    pub fn catalog_unchanged(&self) -> bool {
        let now = self
            .runtime
            .block_on(catalog_tag(self.store.object_store().as_ref()));
        now.is_none() || now == self.catalog_tag
    }

    /// What the tiers before the bucket answered, when there are any.
    pub fn tiers(&self) -> Option<tuile_tile_server::TieredStats> {
        self.tiered.as_ref().map(Tiered::stats)
    }

    /// The store on `objects`, whose catalog names its layers: a bucket of
    /// the caller's own (a test's, in memory).
    pub fn over(objects: Arc<dyn object_store::ObjectStore>) -> Result<Self, String> {
        Self::open("tile store", objects, Vec::new(), Tiers::default())
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
            disk = stats.disk,
            "TILES-READS"
        );
        if let Some(t) = self.tiers() {
            tracing::info!(
                tiers = ?t.tiers,
                origin_requests = t.origin_requests,
                origin_mb = t.origin_bytes as f64 / 1e6,
                written = t.written,
                already_there = t.already_there,
                ranges_passed = t.ranges_passed,
                "TILES-TIERS"
            );
        }
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

#[cfg(test)]
mod tier_tests {
    use object_store::{ObjectStoreExt, PutMode, PutOptions};

    /// A global tier on a disk keeps the first copy of an archive: a second
    /// writer is told it is there, as a bucket's conditional write tells it.
    #[test]
    fn a_tier_on_a_disk_is_written_once() {
        let dir = tempfile::tempdir().expect("dir");
        let url = url::Url::from_directory_path(dir.path()).expect("url");
        let io = tokio::runtime::Runtime::new().expect("io");
        let (tier, _) = super::global_tier(url.as_str(), io.handle()).expect("tier");
        let at = object_store::path::Path::from(
            "layer/top/1-000000000000000000000001-0123456789abcdef.pmtiles",
        );
        let create = || PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                tier.put_opts(&at, "first".into(), create())
                    .await
                    .expect("first");
                let second = tier.put_opts(&at, "second".into(), create()).await;
                assert!(
                    matches!(second, Err(object_store::Error::AlreadyExists { .. })),
                    "{second:?}"
                );
                let kept = tier
                    .get(&at)
                    .await
                    .expect("get")
                    .bytes()
                    .await
                    .expect("bytes");
                assert_eq!(&kept[..], b"first");
            });
    }
}

#[cfg(test)]
mod runtime_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Arc;
    use std::time::Duration;

    use object_store::ObjectStoreExt;

    /// How long the bucket below takes to answer for `slow`.
    const SLOW: Duration = Duration::from_millis(600);

    /// A bucket over HTTP/1.1 that keeps its connections open, on threads of
    /// its own (no runtime): every GET is answered its key, the key `slow`
    /// only after [`SLOW`].
    fn keep_alive_bucket() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let at = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                    let mut writer = stream;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        // The headers, to the blank line; no request has a body.
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                                return;
                            }
                            if header == "\r\n" {
                                break;
                            }
                        }
                        // `GET /bucket/key HTTP/1.1`
                        let key = line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|path| path.rsplit('/').next())
                            .unwrap_or_default()
                            .to_string();
                        if key == "slow" {
                            std::thread::sleep(SLOW);
                        }
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\netag: \"1\"\r\n\
                             last-modified: Wed, 11 Nov 2026 00:00:00 GMT\r\n\
                             connection: keep-alive\r\n\r\n",
                            key.len()
                        );
                        if writer.write_all(head.as_bytes()).is_err()
                            || writer.write_all(key.as_bytes()).is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        at
    }

    /// A store shared by a process outlives the runtime of each bake that
    /// reads through it, and a converge reads through it while bakes end
    /// beside it. The connection a bake opened is the one the client's pool
    /// hands the converge next; were its I/O on the bake's runtime, it would
    /// die under the converge's request when the bake ends (`error sending
    /// request`). Every request of the store runs on the runtime the host
    /// gives it, which lives as long as the process.
    #[test]
    fn a_reader_outlives_the_runtime_that_opened_its_connection() {
        let endpoint = keep_alive_bucket();
        // The process's own runtime, which the host passes in.
        let process = tokio::runtime::Runtime::new().expect("the process's runtime");
        let bucket: Arc<dyn object_store::ObjectStore> = Arc::new(
            super::s3_builder(process.handle())
                .with_bucket_name("tiles")
                .with_endpoint(&endpoint)
                .with_allow_http(true)
                .with_region("auto")
                .with_access_key_id("key")
                .with_secret_access_key("secret")
                .build()
                .expect("bucket"),
        );
        let read = |bucket: Arc<dyn object_store::ObjectStore>, key: &'static str| async move {
            let got = bucket.get(&object_store::path::Path::from(key)).await?;
            got.bytes().await
        };
        // A bake reads, on its own runtime: one connection, then idle.
        let bake = tokio::runtime::Runtime::new().expect("a bake's runtime");
        let got = bake.block_on(read(bucket.clone(), "first"));
        assert_eq!(&got.expect("the bake reads")[..], b"first");
        // A converge reads, on a runtime of its own, through that connection…
        let converge = {
            let bucket = bucket.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new()
                    .expect("the converge's runtime")
                    .block_on(read(bucket, "slow"))
            })
        };
        // …and the bake ends before it is answered.
        std::thread::sleep(SLOW / 4);
        drop(bake);
        let got = converge.join().expect("the converge");
        assert!(
            matches!(&got, Ok(bytes) if &bytes[..] == b"slow"),
            "the converge, once the bake that opened its connection ended: {got:?}"
        );
    }
}

#[cfg(test)]
mod custom_time_tests {
    use super::*;

    /// The object's name is one segment of the JSON API's path, under the
    /// tier's prefix, its slashes encoded; the time is GCS's RFC 3339.
    #[test]
    fn a_touch_names_the_object_and_the_time_as_gcs_reads_them() {
        let credentials: object_store::gcp::GcpCredentialProvider =
            Arc::new(object_store::StaticCredentialProvider::new(
                object_store::gcp::GcpCredential { bearer: "t".into() },
            ));
        let io = tokio::runtime::Runtime::new().expect("io");
        let touch = GcsCustomTime::new("tiles-eu", "store", credentials, io.handle().clone())
            .expect("made");
        let at = object_store::path::Path::from("imagery/zones/z4/1/2/a b.pmtiles");
        assert_eq!(
            touch.url(&at).expect("url").as_str(),
            "https://storage.googleapis.com/storage/v1/b/tiles-eu/o/store%2Fimagery%2Fzones%2Fz4%2F1%2F2%2Fa%20b.pmtiles?fields=customTime"
        );
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_790_000_000_123);
        assert_eq!(rfc3339(when), "2026-09-21T14:13:20.123Z");
    }

    /// A directory as the global tier has no custom time: never touched.
    #[test]
    fn a_directory_tier_is_never_touched() {
        let dir = tempfile::tempdir().expect("dir");
        let url = url::Url::from_directory_path(dir.path()).expect("url");
        let io = tokio::runtime::Runtime::new().expect("io");
        let (_, touch) = global_tier(url.as_str(), io.handle()).expect("tier");
        assert!(touch.is_none());
    }
}
