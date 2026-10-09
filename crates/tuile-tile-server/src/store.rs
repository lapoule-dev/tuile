// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The store: zones of immutable archives, a mutable buffer in front.
//!
//! # Writing
//!
//! A tile is first written to its zone's **buffer**, in memory, where it is
//! readable at once. When the buffer is large or old enough it is **frozen**
//! into a delta archive: written to a local file in tile-id order, uploaded,
//! then made part of the zone by a conditional replacement of the zone's
//! manifest. Until that replacement succeeds the frozen tiles stay readable
//! from memory; if it fails they go back into the buffer.
//!
//! # Many writers
//!
//! Nothing here assumes it is alone. Two instances flushing the same zone both
//! upload their archive (under unique keys) and race on the manifest: one wins,
//! the other re-reads and appends on top. A compaction publishes the same way
//! and gives up if the archives it merged are no longer all there. An upload
//! whose publication never happened — a crash between the two — is an orphan:
//! no manifest names it, readers never see it, and a cleanup removes it once it
//! is old enough not to be an upload still in flight.
//!
//! # Reading
//!
//! Buffer first, then the zone's archives newest first. An archive that has
//! vanished (expired by the bucket's lifecycle, removed after a compaction) is
//! a miss, never an error: the caller fetches the tile again from its source.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::{StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use rand::Rng;
use tokio::io::AsyncReadExt;

use crate::archive::{self, LocalReader, RemoteReader};
use crate::catalog::ABSENT_SUFFIX;
use crate::disk::{ArchiveCache, DiskCacheConfig};
use crate::layer::{Layer, Zone};
use crate::lru::Lru;
use crate::manifest::{self, now_secs, ArchiveRef, Manifest, Retired, Versioned};
use crate::peers::Announce;
use crate::tiering::{self, Tiering};
use crate::StoreError;

/// A source of the current time, so tests can move it.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

const MIB: usize = 1 << 20;

// Defaults of [`StoreConfig`], one place each.

/// A zone's buffer at this size is frozen: large enough that deltas are few,
/// small enough that a crash loses little and memory stays modest.
pub const DEFAULT_FLUSH_BYTES: usize = 32 * MIB;
/// All buffers together never hold more than this: past it, the largest zone
/// is frozen. A bake touches many zones, none of which may fill up on its own.
pub const DEFAULT_MAX_BUFFERED_BYTES: usize = 256 * MIB;
/// A buffer this old is frozen even if small, so a quiet zone is not left in
/// memory only.
pub const DEFAULT_FLUSH_AGE: Duration = Duration::from_secs(30);
/// How stale a manifest may be; the bound on cross-instance visibility.
pub const DEFAULT_MANIFEST_TTL: Duration = Duration::from_secs(2);
/// How much older than [`DEFAULT_MANIFEST_TTL`] a manifest may still be
/// served while a fresh copy is fetched in the background.
pub const DEFAULT_MANIFEST_MAX_STALE: Duration = Duration::from_secs(60);
/// Archives merged at once by a tiered compaction.
pub const DEFAULT_TIER_FANOUT: usize = 4;
/// Archives of one run are within this size factor of each other.
pub const DEFAULT_TIER_RATIO: u64 = 4;
/// Past this many archives in an epoch, the cheapest run is merged anyway:
/// each archive is one more lookup on a miss.
pub const DEFAULT_MAX_ARCHIVES: usize = 12;
/// A retired archive outlives any reader still holding an older manifest.
pub const DEFAULT_RETIRE_GRACE: Duration = Duration::from_secs(10 * 60);
/// No upload takes this long, so an unreferenced archive this old is an orphan.
pub const DEFAULT_ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);
/// Open archives kept, each holding its header and directories.
pub const DEFAULT_READER_CACHE: usize = 512;
/// Conditional publications tried before giving up on a contended zone.
pub const DEFAULT_PUBLISH_ATTEMPTS: usize = 64;

// Transfer and merge tuning.

/// Zones flushed at once: each is an upload and a conditional publication,
/// mostly waiting on the network.
pub const FLUSH_IN_FLIGHT: usize = 16;
/// Part size of a multipart upload.
const UPLOAD_PART: usize = 8 * MIB;
/// Parts of one upload in flight at once.
const UPLOAD_PARTS_IN_FLIGHT: usize = 8;
/// Read size when streaming a local archive to the bucket.
const UPLOAD_READ: usize = MIB;
/// Tiles queued between the merge stream and the synchronous writer.
const MERGE_QUEUE: usize = 64;
/// Retry backoff after a lost publication: random in `[0, base << n)` ms,
/// `n` growing with each attempt up to the cap.
const BACKOFF_BASE_MS: u64 = 2;
const BACKOFF_MAX_DOUBLINGS: usize = 6;

/// When buffers freeze, how long things are cached and kept.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// A zone's buffer is frozen once it holds this many bytes.
    pub flush_bytes: usize,
    /// …and the largest one once all of them together hold this many.
    pub max_buffered_bytes: usize,
    /// …or once its oldest tile is this old (see [`TileStore::flush_due`]).
    pub flush_age: Duration,
    /// How long a manifest read is trusted. This bounds how late one instance
    /// sees another's publication.
    pub manifest_ttl: Duration,
    /// Past its TTL, a manifest is still answered with for this long while a
    /// fresh one is read in the background: a reader never waits on a manifest
    /// it has already seen. Beyond it, the read waits. `ZERO`: always wait.
    ///
    /// Serving a stale list only ever misses archives published since, and a
    /// miss is fetched from the source: never a wrong tile, at worst a
    /// duplicate, which publication then drops (see [`TileStore::put`]).
    pub manifest_max_stale: Duration,
    /// What a tiered compaction merges ([`TileStore::compact_due`]).
    pub tiering: Tiering,
    /// How long a retired archive stays, for readers with an older manifest.
    pub retire_grace: Duration,
    /// How old an unreferenced archive must be before it is an orphan rather
    /// than an upload still being published.
    pub orphan_grace: Duration,
    /// Open archives kept, each with its header and directories.
    pub reader_cache: usize,
    /// Attempts at publishing a manifest before giving up.
    pub publish_attempts: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            flush_bytes: DEFAULT_FLUSH_BYTES,
            max_buffered_bytes: DEFAULT_MAX_BUFFERED_BYTES,
            flush_age: DEFAULT_FLUSH_AGE,
            manifest_ttl: DEFAULT_MANIFEST_TTL,
            manifest_max_stale: DEFAULT_MANIFEST_MAX_STALE,
            tiering: Tiering {
                fanout: DEFAULT_TIER_FANOUT,
                ratio: DEFAULT_TIER_RATIO,
                max_archives: DEFAULT_MAX_ARCHIVES,
            },
            retire_grace: DEFAULT_RETIRE_GRACE,
            orphan_grace: DEFAULT_ORPHAN_GRACE,
            reader_cache: DEFAULT_READER_CACHE,
            publish_attempts: DEFAULT_PUBLISH_ATTEMPTS,
        }
    }
}

/// What a compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compaction {
    /// Fewer than two archives in the current epoch.
    Nothing,
    /// `merged` archives became one of `tiles` tiles.
    Merged { merged: usize, tiles: u64 },
    /// Another writer changed the archives first; nothing was published.
    Superseded,
}

/// A failure injected on purpose by the tests.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Fault {
    None = 0,
    /// The next flush uploads its archive, then fails before publishing it,
    /// as a process killed at that instant would. Its tiles are dropped.
    CrashAfterUpload = 1,
    /// The next flush uploads its archive, then fails to publish it as a
    /// transient error would (the process lives on): its tiles must go back
    /// into the buffer, readable, and reach the store on a later flush.
    FailAfterUpload = 2,
}

type ZoneKey = (String, Zone);

/// A pause point in a publication, for tests: the flush signals `reached`
/// once its archive is uploaded, then waits for `go`.
#[doc(hidden)]
#[derive(Default)]
pub struct Gate {
    pub reached: tokio::sync::Notify,
    pub go: tokio::sync::Notify,
}

/// A buffered tile and when its bytes were fetched, in milliseconds since
/// the Unix epoch: what decides, at publication, whether a copy the store
/// already holds is newer.
#[derive(Debug, Clone)]
struct Pending {
    bytes: Bytes,
    fetched_ms: u64,
}

/// A tile fetched from a source and not yet in the store, for
/// [`TileStore::publish_batch`]. An absence is offered as the absence
/// sibling's own marker ([`crate::catalog::ABSENT_MARKER`]), in that layer.
#[derive(Debug, Clone)]
pub struct Fresh {
    pub level: u8,
    pub x: u32,
    pub y: u32,
    pub bytes: Bytes,
    /// When it was fetched, in milliseconds since the Unix epoch.
    pub fetched_ms: u64,
}

/// What a zone is made of, counted: see [`TileStore::census`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Census {
    /// Archives a reader goes through (those of expired epochs left out).
    pub archives: usize,
    /// Tiles those archives hold, each copy counted.
    pub copies: u64,
    /// Tiles a reader can get: each tile counted once.
    pub tiles: u64,
    /// Tiles held by more than one archive.
    pub repeated: u64,
}

#[derive(Default)]
struct Buffer {
    pending: BTreeMap<u64, Pending>,
    bytes: usize,
    since: Option<Instant>,
    /// Frozen tiles being published; still readable.
    flushing: Option<Arc<BTreeMap<u64, Pending>>>,
}

type ManifestFlight = Shared<BoxFuture<'static, Result<Arc<Manifest>, String>>>;

/// Manifests as last read, and the reads in flight: one per zone however
/// many requests want it.
#[derive(Default)]
struct Manifests {
    cache: Mutex<HashMap<ZoneKey, (Instant, Arc<Manifest>)>>,
    flights: Mutex<HashMap<ZoneKey, ManifestFlight>>,
}

/// Zones remembered as already prefetched, so a busy zone does not schedule
/// its neighbours on every read.
const PREFETCHED_ZONES: u64 = 100_000;
/// Tiles of one publication checked against the store at once.
const DEDUP_IN_FLIGHT: usize = 32;

/// Tiles of every configured layer, kept in an object store.
pub struct TileStore {
    store: Arc<dyn ObjectStore>,
    layers: HashMap<String, Layer>,
    cfg: StoreConfig,
    clock: Clock,
    buffers: Mutex<HashMap<ZoneKey, Buffer>>,
    manifests: Arc<Manifests>,
    readers: Mutex<Lru<String, Arc<RemoteReader>>>,
    /// Whole archives copied to local disk, if configured.
    disk: Option<Arc<ArchiveCache>>,
    prefetched: Mutex<Lru<ZoneKey, ()>>,
    zone_locks: Mutex<HashMap<ZoneKey, Arc<tokio::sync::Mutex<()>>>>,
    fault: AtomicU8,
    gate: Mutex<Option<Arc<Gate>>>,
    /// Local projections of remote zones, read before the bucket.
    projections: Mutex<HashMap<ZoneKey, Arc<archive::LocalReader>>>,
    /// Told of every manifest this store publishes, if anyone is to be.
    announcer: Option<Arc<dyn Announce>>,
    stats: Stats,
}

/// Where reads were answered from, since the store was opened.
#[derive(Debug, Default)]
pub struct Stats {
    pub buffer: std::sync::atomic::AtomicU64,
    pub projection: std::sync::atomic::AtomicU64,
    /// Asked of a projected zone and not in its projection.
    pub projection_misses: std::sync::atomic::AtomicU64,
    pub remote: std::sync::atomic::AtomicU64,
    pub absent: std::sync::atomic::AtomicU64,
    /// Answered from an archive copied to local disk.
    pub disk: AtomicU64,
    /// Whole archives scheduled for a local copy.
    pub copies: AtomicU64,
    /// Buffered tiles not published because the store already held the same
    /// bytes, or a copy fetched after them.
    pub dedup_identical: AtomicU64,
    pub dedup_newer: AtomicU64,
}

/// A copy of [`Stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub buffer: u64,
    pub projection: u64,
    pub projection_misses: u64,
    pub remote: u64,
    pub absent: u64,
    pub disk: u64,
    pub copies: u64,
    pub dedup_identical: u64,
    pub dedup_newer: u64,
}

impl TileStore {
    pub fn new(store: Arc<dyn ObjectStore>, layers: Vec<Layer>, cfg: StoreConfig) -> Self {
        Self::with_clock(store, layers, cfg, Arc::new(SystemTime::now))
    }

    pub fn with_clock(store: Arc<dyn ObjectStore>, layers: Vec<Layer>, cfg: StoreConfig, clock: Clock) -> Self {
        let reader_cache = cfg.reader_cache as u64;
        Self {
            store,
            layers: layers.into_iter().map(|l| (l.name.clone(), l)).collect(),
            cfg,
            clock,
            buffers: Mutex::default(),
            manifests: Arc::default(),
            readers: Mutex::new(Lru::new(reader_cache)),
            disk: None,
            prefetched: Mutex::new(Lru::new(PREFETCHED_ZONES)),
            zone_locks: Mutex::default(),
            fault: AtomicU8::new(Fault::None as u8),
            gate: Mutex::default(),
            projections: Mutex::default(),
            announcer: None,
            stats: Stats::default(),
        }
    }

    pub fn layer(&self, name: &str) -> Result<&Layer, StoreError> {
        self.layers.get(name).ok_or_else(|| StoreError::UnknownLayer(name.to_string()))
    }

    pub fn layers(&self) -> impl Iterator<Item = &Layer> {
        self.layers.values()
    }

    pub fn object_store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    pub fn object_store_config(&self) -> &StoreConfig {
        &self.cfg
    }

    /// The time, by this store's clock.
    pub fn now(&self) -> SystemTime {
        (self.clock)()
    }

    /// Where reads were answered from.
    pub fn stats(&self) -> StatsSnapshot {
        let g = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        StatsSnapshot {
            buffer: g(&self.stats.buffer),
            projection: g(&self.stats.projection),
            projection_misses: g(&self.stats.projection_misses),
            remote: g(&self.stats.remote),
            absent: g(&self.stats.absent),
            disk: g(&self.stats.disk),
            copies: g(&self.stats.copies),
            dedup_identical: g(&self.stats.dedup_identical),
            dedup_newer: g(&self.stats.dedup_newer),
        }
    }

    /// Keeps whole archives on local disk under `cfg.dir` from now on (see
    /// [`crate::disk`]). What the directory already holds is served at once.
    pub async fn with_disk_cache(mut self, cfg: DiskCacheConfig) -> Result<Self, StoreError> {
        self.disk = Some(ArchiveCache::open(cfg).await?);
        Ok(self)
    }

    /// From now on every manifest this store publishes — a delta, a merge,
    /// an expiry, a cleanup — is announced through `announcer`, for the other
    /// instances to drop their copy of it at once: see [`crate::peers`].
    pub fn with_announcer(mut self, announcer: Arc<dyn Announce>) -> Self {
        self.announcer = Some(announcer);
        self
    }

    /// What an instance does on hearing that a zone's manifest was published
    /// at `generation`: its own copy, if older, is dropped, and the next read
    /// of the zone reads the manifest again. A copy as new as the
    /// announcement — this instance's own publication — is kept.
    pub fn forget_zone(&self, layer: &str, zone: Zone, generation: u64) {
        let key = (layer.to_string(), zone);
        if let Ok(mut cache) = self.manifests.cache.lock() {
            if cache.get(&key).is_some_and(|(_, kept)| kept.generation < generation) {
                cache.remove(&key);
            }
        }
    }

    /// Bytes of archives on local disk.
    pub fn disk_bytes(&self) -> u64 {
        self.disk.as_ref().map_or(0, |d| d.bytes())
    }

    /// Waits for the background copies in flight (tests, benchmarks).
    pub async fn settle(&self) {
        if let Some(d) = &self.disk {
            d.settle().await;
        }
    }

    /// Reads the zone through a local projection from now on.
    pub(crate) async fn attach_projection(&self, layer: &Layer, zone: Zone, path: &std::path::Path) -> Result<(), StoreError> {
        let reader = Arc::new(archive::open_local(path).await?);
        if let Ok(mut p) = self.projections.lock() {
            p.insert((layer.name.clone(), zone), reader);
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn inject_fault(&self, fault: Fault) {
        self.fault.store(fault as u8, Ordering::SeqCst);
    }

    /// Pauses the next publication after its upload (tests only).
    #[doc(hidden)]
    pub fn pause_next_publication(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        if let Ok(mut g) = self.gate.lock() {
            *g = Some(gate.clone());
        }
        gate
    }

    fn locate(&self, layer: &str, level: u8, x: u32, y: u32) -> Result<(&Layer, Zone, u64), StoreError> {
        let l = self.layer(layer)?;
        let zone = l.zone_of(level, x, y)?;
        let id = l.grid.to_id(level, x, y)?;
        Ok((l, zone, id))
    }

    // ── Reading ──────────────────────────────────────────────────────────

    /// The stored bytes of a tile, exactly as they were put.
    pub async fn get(&self, layer: &str, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, StoreError> {
        let (l, zone, id) = self.locate(layer, level, x, y)?;
        let key = (l.name.clone(), zone);

        if let Some(bytes) = self.buffered(&key, id) {
            self.stats.buffer.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("tuile_tiles_hits_total", "layer" => l.name.clone(), "from" => "buffer").increment(1);
            return Ok(Some(bytes));
        }

        let projection = self.projections.lock().ok().and_then(|p| p.get(&key).cloned());
        if let Some(local) = projection {
            if let Some(bytes) = local.get_tile(pmtiles::TileId::new(id)?).await? {
                self.stats.projection.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("tuile_tiles_hits_total", "layer" => l.name.clone(), "from" => "projection").increment(1);
                return Ok(Some(bytes));
            }
            // Not in the projection: the footprint left it out, or it is
            // truly absent. The bucket decides.
            self.stats.projection_misses.fetch_add(1, Ordering::Relaxed);
        }

        let now = (self.clock)();
        let manifest = self.manifest(&key, l).await?;
        let live: Vec<&ArchiveRef> = manifest.archives.iter().rev().filter(|a| !l.is_expired(&a.epoch, now)).collect();
        if !live.iter().all(|a| self.disk_reader(&a.key).is_some()) {
            // A read of this zone from the bucket: copy it whole, so the next
            // read of any of its tiles is local — and this one too, when the
            // archives are small enough to be worth waiting for.
            let waits = self.copy_zone(l, zone, &live, now);
            if !waits.is_empty() {
                futures_util::future::join_all(waits).await;
            }
        }
        // The newest archive first, as it usually holds the tile; on a miss,
        // all the older ones at once, so a tile held only by the oldest costs
        // two round trips, not one per archive — without asking every
        // archive for every tile. Local copies answer in microseconds.
        let mut results = Vec::with_capacity(live.len());
        if let Some(newest) = live.first() {
            results.push(self.read_archive(&newest.key, id).await);
        }
        if !matches!(results.first(), Some(Ok((Some(_), _)))) && live.len() > 1 {
            results.extend(futures_util::future::join_all(live[1..].iter().map(|a| self.read_archive(&a.key, id))).await);
        }
        for (archive, result) in live.iter().zip(results) {
            match result {
                Ok((Some(bytes), local)) => {
                    let from = if local {
                        self.stats.disk.fetch_add(1, Ordering::Relaxed);
                        "disk"
                    } else {
                        self.stats.remote.fetch_add(1, Ordering::Relaxed);
                        "archive"
                    };
                    metrics::counter!("tuile_tiles_hits_total", "layer" => l.name.clone(), "from" => from).increment(1);
                    return Ok(Some(bytes));
                }
                Ok((None, _)) => {}
                Err(e) => {
                    if self.vanished(&archive.key).await {
                        // Expired or compacted away under an older manifest:
                        // a miss, and the next read must not trust this list.
                        self.forget_manifest(&key);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        self.stats.absent.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("tuile_tiles_misses_total", "layer" => l.name.clone()).increment(1);
        Ok(None)
    }

    fn buffered(&self, key: &ZoneKey, id: u64) -> Option<Bytes> {
        let buffers = self.buffers.lock().ok()?;
        let b = buffers.get(key)?;
        b.pending.get(&id).or_else(|| b.flushing.as_ref().and_then(|f| f.get(&id))).map(|p| p.bytes.clone())
    }

    fn disk_reader(&self, archive_key: &str) -> Option<Arc<LocalReader>> {
        self.disk.as_ref()?.reader(archive_key)
    }

    /// A tile of one archive, from its local copy if there is one. The flag
    /// says whether it was local.
    async fn read_archive(&self, archive_key: &str, id: u64) -> Result<(Option<Bytes>, bool), StoreError> {
        if let Some(local) = self.disk_reader(archive_key) {
            return Ok((local.get_tile(pmtiles::TileId::new(id)?).await?, true));
        }
        Ok((self.read_tile(archive_key, id).await?, false))
    }

    /// Starts local copies of a zone's archives and, if configured, of the
    /// zones around it. Returns the copies worth waiting for.
    fn copy_zone(&self, layer: &Layer, zone: Zone, live: &[&ArchiveRef], now: SystemTime) -> Vec<crate::disk::ArchiveCopy> {
        let Some(disk) = &self.disk else { return Vec::new() };
        let mut waits = Vec::new();
        for a in live {
            if let Some((copy, started)) = disk.copy(self.store.clone(), &a.key, a.bytes) {
                if started {
                    self.stats.copies.fetch_add(1, Ordering::Relaxed);
                }
                if a.bytes <= disk.cfg.await_copy_bytes {
                    waits.push(copy);
                }
            }
        }
        let key = (layer.name.clone(), zone);
        let first = match self.prefetched.lock() {
            Ok(mut seen) => !seen.contains(&key) && seen.insert(key, (), 1).is_empty(),
            Err(_) => false,
        };
        if first && disk.cfg.prefetch_neighbours {
            for near in neighbours(layer, zone) {
                self.prefetch_zone(layer, near, now);
            }
        }
        waits
    }

    /// Copies a zone's archives in the background, reading its manifest
    /// first. Nothing waits on it.
    fn prefetch_zone(&self, layer: &Layer, zone: Zone, now: SystemTime) {
        let Some(disk) = self.disk.clone() else { return };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let objects = self.store.clone();
        let layer = layer.clone();
        runtime.spawn(async move {
            let Ok(read) = manifest::read(objects.as_ref(), &layer.zone_prefix(zone)).await else { return };
            for a in read.manifest.archives.iter().rev().filter(|a| !layer.is_expired(&a.epoch, now)) {
                disk.copy(objects.clone(), &a.key, a.bytes);
            }
        });
    }

    async fn manifest(&self, key: &ZoneKey, layer: &Layer) -> Result<Arc<Manifest>, StoreError> {
        let cached = self.manifests.cache.lock().ok().and_then(|c| c.get(key).cloned());
        if let Some((at, m)) = cached {
            let age = at.elapsed();
            if age < self.cfg.manifest_ttl {
                return Ok(m);
            }
            if age < self.cfg.manifest_ttl.saturating_add(self.cfg.manifest_max_stale) {
                // Answer with what we have; the fresh copy replaces it for the
                // next reader.
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(self.manifest_flight(key, layer));
                    return Ok(m);
                }
            }
        }
        self.manifest_flight(key, layer).await.map_err(StoreError::Remote)
    }

    /// The one read of a zone's manifest in flight, started if there is none.
    fn manifest_flight(&self, key: &ZoneKey, layer: &Layer) -> ManifestFlight {
        let mut flights = match self.manifests.flights.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(f) = flights.get(key) {
            return f.clone();
        }
        let objects = self.store.clone();
        let manifests = self.manifests.clone();
        let disk = self.disk.clone();
        let prefix = layer.zone_prefix(key.1);
        let k = key.clone();
        let flight = async move {
            let result = manifest::read(objects.as_ref(), &prefix).await.map(|v| Arc::new(v.manifest));
            if let Ok(m) = &result {
                if manifests.remember(&k, m.clone()) {
                    drop_unnamed(disk.as_deref(), &prefix, m);
                }
            }
            if let Ok(mut f) = manifests.flights.lock() {
                f.remove(&k);
            }
            result.map_err(|e| e.to_string())
        }
        .boxed()
        .shared();
        flights.insert(key.clone(), flight.clone());
        flight
    }

    fn remember_manifest(&self, key: &ZoneKey, m: Arc<Manifest>) {
        if self.manifests.remember(key, m.clone()) {
            if let Ok(layer) = self.layer(&key.0) {
                drop_unnamed(self.disk.as_deref(), &layer.zone_prefix(key.1), &m);
            }
        }
    }

    fn forget_manifest(&self, key: &ZoneKey) {
        if let Ok(mut cache) = self.manifests.cache.lock() {
            cache.remove(key);
        }
    }

    async fn read_tile(&self, archive_key: &str, id: u64) -> Result<Option<Bytes>, StoreError> {
        let reader = self.reader(archive_key).await?;
        Ok(reader.get_tile(pmtiles::TileId::new(id)?).await?)
    }

    async fn reader(&self, archive_key: &str) -> Result<Arc<RemoteReader>, StoreError> {
        let key = archive_key.to_string();
        if let Some(r) = self.readers.lock().ok().and_then(|mut c| c.get(&key)) {
            return Ok(r);
        }
        let r = Arc::new(archive::open_remote(self.store.clone(), archive_key).await?);
        if let Ok(mut cache) = self.readers.lock() {
            // Archives are immutable: dropping any of them only costs a
            // header read the next time.
            cache.insert(key, r.clone(), 1);
        }
        Ok(r)
    }

    async fn vanished(&self, archive_key: &str) -> bool {
        let gone = matches!(
            self.store.head(&Path::from(archive_key)).await,
            Err(object_store::Error::NotFound { .. })
        );
        if gone {
            if let Ok(mut cache) = self.readers.lock() {
                cache.remove(&archive_key.to_string());
            }
        }
        gone
    }

    // ── Writing ──────────────────────────────────────────────────────────

    /// Stores a tile fetched just now. It is readable immediately; it reaches
    /// the object store when its zone's buffer is flushed.
    ///
    /// Publication keeps the store free of duplicates: a buffered tile whose
    /// bytes the store already holds, or of which it holds a copy fetched
    /// after it, is dropped rather than published (see [`TileStore::put_fetched`]).
    pub async fn put(&self, layer: &str, level: u8, x: u32, y: u32, bytes: Bytes) -> Result<(), StoreError> {
        let now = (self.clock)();
        self.put_fetched(layer, level, x, y, bytes, now).await
    }

    /// Stores a tile its source delivered at `fetched`.
    ///
    /// When its zone is published, the tile is compared with the copy a reader
    /// would get from the store at that moment: identical bytes, or a stored
    /// copy from an archive whose tiles were all fetched after `fetched`, and
    /// this one is dropped. Otherwise it is published last, so it wins every
    /// later read — the newest fetch wins. (A stored copy from an archive whose
    /// fetch times span `fetched` cannot be ordered per tile; the tile being
    /// published, being at least as recent as part of that archive, wins.)
    pub async fn put_fetched(
        &self,
        layer: &str,
        level: u8,
        x: u32,
        y: u32,
        bytes: Bytes,
        fetched: SystemTime,
    ) -> Result<(), StoreError> {
        let (l, zone, id) = self.locate(layer, level, x, y)?;
        let key = (l.name.clone(), zone);
        let fetched_ms = manifest::now_ms(fetched);
        let to_flush = {
            let mut buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            let b = buffers.entry(key.clone()).or_default();
            b.bytes += bytes.len();
            if let Some(old) = b.pending.insert(id, Pending { bytes, fetched_ms }) {
                b.bytes -= old.bytes.len();
            }
            b.since.get_or_insert_with(Instant::now);
            if b.bytes >= self.cfg.flush_bytes {
                Some(key.clone())
            } else if buffers.values().map(|b| b.bytes).sum::<usize>() >= self.cfg.max_buffered_bytes {
                buffers.iter().max_by_key(|(_, b)| b.bytes).map(|(k, _)| k.clone())
            } else {
                None
            }
        };
        if let Some(zone) = to_flush {
            self.flush_zone(&zone).await?;
        }
        Ok(())
    }

    /// Flushes every buffer that is full or old enough. Meant to be called
    /// periodically.
    pub async fn flush_due(&self) -> Result<usize, StoreError> {
        let due: Vec<ZoneKey> = {
            let buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            buffers
                .iter()
                .filter(|(_, b)| {
                    !b.pending.is_empty()
                        && (b.bytes >= self.cfg.flush_bytes
                            || b.since.is_some_and(|s| s.elapsed() >= self.cfg.flush_age))
                })
                .map(|(k, _)| k.clone())
                .collect()
        };
        for key in &due {
            self.flush_zone(key).await?;
        }
        Ok(due.len())
    }

    /// Flushes every non-empty buffer, [`FLUSH_IN_FLIGHT`] zones at a time.
    pub async fn flush_all(&self) -> Result<usize, StoreError> {
        let keys: Vec<ZoneKey> = {
            let buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            buffers.iter().filter(|(_, b)| !b.pending.is_empty()).map(|(k, _)| k.clone()).collect()
        };
        let results: Vec<Result<(), StoreError>> = futures_util::stream::iter(keys.clone())
            .map(|key| async move { self.flush_zone(&key).await })
            .buffer_unordered(FLUSH_IN_FLIGHT)
            .collect()
            .await;
        // Every zone was attempted; the first failure is the one reported.
        results.into_iter().collect::<Result<Vec<()>, _>>()?;
        Ok(keys.len())
    }

    fn zone_lock(&self, key: &ZoneKey) -> Arc<tokio::sync::Mutex<()>> {
        match self.zone_locks.lock() {
            Ok(mut locks) => locks.entry(key.clone()).or_default().clone(),
            Err(_) => Arc::default(),
        }
    }

    async fn flush_zone(&self, key: &ZoneKey) -> Result<(), StoreError> {
        let lock = self.zone_lock(key);
        let _held = lock.lock().await;
        let layer = self.layer(&key.0)?.clone();

        let frozen = {
            let mut buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            let Some(b) = buffers.get_mut(key) else { return Ok(()) };
            if b.pending.is_empty() {
                return Ok(());
            }
            let frozen = Arc::new(std::mem::take(&mut b.pending));
            b.bytes = 0;
            b.since = None;
            b.flushing = Some(frozen.clone());
            frozen
        };

        let result = self.publish_delta(&layer, key, &frozen).await.map(|_| ());
        {
            let mut buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            let b = buffers.entry(key.clone()).or_default();
            b.flushing = None;
            match &result {
                Ok(()) => {}
                Err(StoreError::Fault(_)) => {} // the simulated crash lost them
                Err(_) => {
                    // Put back what was not published, never over a newer write.
                    for (id, p) in frozen.iter() {
                        if !b.pending.contains_key(id) {
                            b.bytes += p.bytes.len();
                            b.pending.insert(*id, p.clone());
                        }
                    }
                    if !b.pending.is_empty() {
                        b.since.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
        // No compaction here: a flush runs on a writer's path (a request, a
        // bake), and merging is the business of a long-lived process calling
        // [`TileStore::compact_due`].
        result
    }

    /// Where a fresh tile is stored: its layer and its zone. A source's "no
    /// such tile" (`absent`) goes to the layer's absence sibling, and nowhere
    /// (`None`) when the layer keeps none.
    pub fn place(&self, layer: &str, level: u8, x: u32, y: u32, absent: bool) -> Result<Option<(String, Zone)>, StoreError> {
        let name = if absent { format!("{layer}{ABSENT_SUFFIX}") } else { layer.to_string() };
        if absent && self.layer(&name).is_err() {
            // Checked against the real layer all the same: a tile nobody
            // could address is an error, not a quiet nothing.
            self.locate(layer, level, x, y)?;
            return Ok(None);
        }
        let (_, zone, _) = self.locate(&name, level, x, y)?;
        Ok(Some((name, zone)))
    }

    /// Publishes a batch of fresh tiles of one zone, now, as a writer that
    /// keeps no buffer does: a process fed by a queue or a stream of what
    /// serving instances fetched (see [`crate::peers::FreshTiles`]).
    ///
    /// `layer` and `zone` are where [`Self::place`] put the tiles; one that
    /// belongs elsewhere fails the whole batch, nothing written. Exactly what
    /// a flush does from there on: tiles the store already holds as they are,
    /// or holds a later copy of, are dropped; the rest become one delta,
    /// published by a conditional write of the zone's manifest and announced.
    /// Says how many tiles were written. On an error nothing was published
    /// and the batch is the caller's to offer again.
    pub async fn publish_batch(&self, layer: &str, zone: Zone, tiles: Vec<Fresh>) -> Result<usize, StoreError> {
        let l = self.layer(layer)?.clone();
        let key = (l.name.clone(), zone);
        let mut batch: BTreeMap<u64, Pending> = BTreeMap::new();
        for tile in tiles {
            let (_, at, id) = self.locate(layer, tile.level, tile.x, tile.y)?;
            if at != zone {
                return Err(StoreError::Corrupt(format!(
                    "{layer} {}/{}/{} is not a tile of the zone it was offered for",
                    tile.level, tile.x, tile.y
                )));
            }
            let pending = Pending { bytes: tile.bytes, fetched_ms: tile.fetched_ms };
            // The same tile twice in a batch: the later fetch is the tile.
            match batch.get(&id) {
                Some(kept) if kept.fetched_ms > pending.fetched_ms => {}
                _ => {
                    batch.insert(id, pending);
                }
            }
        }
        if batch.is_empty() {
            return Ok(0);
        }
        let lock = self.zone_lock(&key);
        let _held = lock.lock().await;
        self.publish_delta(&l, &key, &batch).await
    }

    /// Publishes a zone's tiles as a delta; says how many it wrote (the rest
    /// were in the store already).
    async fn publish_delta(&self, layer: &Layer, key: &ZoneKey, frozen: &BTreeMap<u64, Pending>) -> Result<usize, StoreError> {
        let now = (self.clock)();
        let epoch = layer.epoch(now);
        let prefix = layer.zone_prefix(key.1);

        let kept = self.drop_duplicates(layer, key, frozen, now).await?;
        if kept.is_empty() {
            // Everything is already in the store: nothing to publish.
            return Ok(0);
        }
        let oldest_fetch_ms = kept.values().map(|p| p.fetched_ms).min();
        let newest_fetch_ms = kept.values().map(|p| p.fetched_ms).max();
        let tiles: BTreeMap<u64, Bytes> = kept.into_iter().map(|(id, p)| (id, p.bytes)).collect();
        let object = archive_key(&prefix, &epoch, now);

        let l = layer.clone();
        let (e, list) = (epoch.clone(), tiles.clone());
        let zooms = archive::zoom_range(list.keys());
        let (file, count) = tokio::task::spawn_blocking(move || archive::write(&l, &e, zooms, list.into_iter().map(Ok)))
            .await
            .map_err(|e| StoreError::Corrupt(format!("archive writer: {e}")))??;
        upload(self.store.as_ref(), file.path(), &object).await?;

        let gate = self.gate.lock().ok().and_then(|mut g| g.take());
        if let Some(gate) = gate {
            gate.reached.notify_one();
            gate.go.notified().await;
        }
        match self.fault.swap(Fault::None as u8, Ordering::SeqCst) {
            f if f == Fault::CrashAfterUpload as u8 => return Err(StoreError::Fault("crash after upload")),
            f if f == Fault::FailAfterUpload as u8 => return Err(StoreError::Contended(format!("{prefix} (injected)"))),
            _ => {}
        }

        let bytes = tiles.values().map(|b| b.len() as u64).sum();
        let entry =
            ArchiveRef { key: object, epoch, created: now_secs(now), tiles: count, bytes, oldest_fetch_ms, newest_fetch_ms };
        let published = self
            .publish(key, &prefix, |m| {
                let mut next = m.clone();
                next.archives.push(entry.clone());
                Some(next)
            })
            .await?;
        if let Some(m) = published {
            self.remember_manifest(key, Arc::new(m));
        }
        metrics::counter!("tuile_tiles_deltas_total", "layer" => layer.name.clone()).increment(1);
        Ok(count as usize)
    }

    /// The buffered tiles worth publishing: those the store does not already
    /// hold as they are, nor hold a copy of fetched after them. Compared with
    /// the zone's manifest as it is now, not as cached. A tile that cannot be
    /// compared (a read failed) is kept: a duplicate is harmless, a lost tile
    /// is not.
    async fn drop_duplicates(
        &self,
        layer: &Layer,
        key: &ZoneKey,
        frozen: &BTreeMap<u64, Pending>,
        now: SystemTime,
    ) -> Result<BTreeMap<u64, Pending>, StoreError> {
        let current = manifest::read(self.store.as_ref(), &layer.zone_prefix(key.1)).await?.manifest;
        let current = Arc::new(current);
        self.remember_manifest(key, current.clone());
        let live: Vec<&ArchiveRef> = current.archives.iter().rev().filter(|a| !layer.is_expired(&a.epoch, now)).collect();
        if live.is_empty() {
            return Ok(frozen.clone());
        }
        // With a disk cache, the zone comes down whole once and every
        // comparison is local; otherwise tiles are compared a batch at a time.
        if let Some(disk) = &self.disk {
            for a in &live {
                if a.bytes <= disk.cfg.max_archive_bytes {
                    if let Err(e) = disk.fetch(self.store.as_ref(), &a.key, a.bytes).await {
                        tracing::debug!(key = a.key, error = %e, "archive copy for comparison failed; read remotely");
                    }
                }
            }
        }
        let live = &live;
        let owned: Vec<(u64, Pending)> = frozen.iter().map(|(id, p)| (*id, p.clone())).collect();
        let verdicts: Vec<(u64, bool)> = futures_util::stream::iter(owned)
            .map(|(id, p)| async move {
                let keep = match self.stored_copy(live, id).await {
                    Some((stored, _)) if stored == p.bytes => {
                        self.stats.dedup_identical.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                    Some((_, archive)) if archive.oldest_fetch_ms.is_some_and(|t| t > p.fetched_ms) => {
                        self.stats.dedup_newer.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                    _ => true,
                };
                (id, keep)
            })
            .buffer_unordered(DEDUP_IN_FLIGHT)
            .collect()
            .await;
        let dropped = verdicts.iter().filter(|(_, keep)| !keep).count() as u64;
        if dropped > 0 {
            metrics::counter!("tuile_tiles_dedup_total", "layer" => layer.name.clone()).increment(dropped);
        }
        Ok(verdicts
            .into_iter()
            .filter(|(_, keep)| *keep)
            .filter_map(|(id, _)| frozen.get(&id).map(|p| (id, p.clone())))
            .collect())
    }

    /// The copy of a tile a reader would get from these archives (newest
    /// first), and the archive it comes from. `None` when absent, or when it
    /// cannot be read.
    async fn stored_copy<'a>(&self, live: &[&'a ArchiveRef], id: u64) -> Option<(Bytes, &'a ArchiveRef)> {
        for a in live {
            match self.read_archive(&a.key, id).await {
                Ok((Some(bytes), _)) => return Some((bytes, *a)),
                Ok((None, _)) => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// Derives the next manifest from the current one and publishes it,
    /// retrying on a lost race. `derive` returning `None` abandons.
    async fn publish<F>(&self, key: &ZoneKey, prefix: &str, derive: F) -> Result<Option<Manifest>, StoreError>
    where
        F: Fn(&Manifest) -> Option<Manifest>,
    {
        for attempt in 0..self.cfg.publish_attempts {
            let current: Versioned = manifest::read(self.store.as_ref(), prefix).await?;
            let Some(mut next) = derive(&current.manifest) else { return Ok(None) };
            next.generation = current.manifest.generation + 1;
            if manifest::replace(self.store.as_ref(), prefix, &current, &next).await? {
                if let Some(announcer) = &self.announcer {
                    announcer.published(&key.0, key.1, next.generation).await;
                    metrics::counter!("tuile_tiles_announced_total", "layer" => key.0.clone()).increment(1);
                }
                return Ok(Some(next));
            }
            metrics::counter!("tuile_tiles_publish_conflicts_total").increment(1);
            let backoff = rand::rng().random_range(0..(BACKOFF_BASE_MS << attempt.min(BACKOFF_MAX_DOUBLINGS)));
            tokio::time::sleep(Duration::from_millis(backoff)).await;
        }
        Err(StoreError::Contended(prefix.to_string()))
    }

    // ── Compaction ───────────────────────────────────────────────────────

    /// Merges **all** the archives of the zone's current epoch into one, now.
    /// For maintenance and tests; a server calls [`TileStore::compact_due`].
    pub async fn compact(&self, layer: &str, zone: Zone) -> Result<Compaction, StoreError> {
        let l = self.layer(layer)?.clone();
        let lock = self.zone_lock(&(l.name.clone(), zone));
        let _held = lock.lock().await;
        let epoch = l.epoch((self.clock)());
        let m = manifest::read(self.store.as_ref(), &l.zone_prefix(zone)).await?.manifest;
        // The last contiguous run of the current epoch.
        let end = m.archives.iter().rposition(|a| a.epoch == epoch).map_or(0, |i| i + 1);
        let start = m.archives[..end].iter().rposition(|a| a.epoch != epoch).map_or(0, |i| i + 1);
        self.merge_run(&l, zone, &m.archives[start..end]).await
    }

    /// Merges the next tiered run of the zone, if the policy picks one.
    pub async fn compact_tiered(&self, layer: &str, zone: Zone) -> Result<Compaction, StoreError> {
        let l = self.layer(layer)?.clone();
        let lock = self.zone_lock(&(l.name.clone(), zone));
        let _held = lock.lock().await;
        let now = (self.clock)();
        let m = manifest::read(self.store.as_ref(), &l.zone_prefix(zone)).await?.manifest;
        // The epoch being written first, then those before it that have not
        // expired: the deltas an epoch's last hours left behind would
        // otherwise stay as they are until they expire. Each epoch on its
        // own — a run never spans two.
        let mut epochs = vec![l.epoch(now)];
        for a in &m.archives {
            if !epochs.contains(&a.epoch) && !l.is_expired(&a.epoch, now) {
                epochs.push(a.epoch.clone());
            }
        }
        for epoch in &epochs {
            if let Some(run) = tiering::select(&m.archives, epoch, self.cfg.tiering) {
                return self.merge_run(&l, zone, &m.archives[run]).await;
            }
        }
        Ok(Compaction::Nothing)
    }

    /// Merges a given run, as a compaction planned on an older manifest would
    /// (tests only).
    #[doc(hidden)]
    pub async fn merge_run_planned(&self, layer: &str, zone: Zone, run: &[ArchiveRef]) -> Result<Compaction, StoreError> {
        let l = self.layer(layer)?.clone();
        self.merge_run(&l, zone, run).await
    }

    /// Every zone of every layer that has a manifest, found by listing.
    pub async fn zones(&self) -> Result<Vec<(String, Zone)>, StoreError> {
        let mut out = Vec::new();
        for layer in self.layers.values() {
            let listed: Vec<object_store::ObjectMeta> =
                self.store.list(Some(&Path::from(layer.name.as_str()))).try_collect().await?;
            for meta in listed {
                if let Some(zone) = parse_manifest_key(layer, meta.location.as_ref()) {
                    out.push((layer.name.clone(), zone));
                }
            }
        }
        Ok(out)
    }

    /// One pass over the whole store: in every zone, drops expired archives,
    /// merges tiered runs until none is due, and cleans up. Meant to be called
    /// periodically by one long-lived process; any number may, safely.
    pub async fn compact_due(&self) -> Result<Vec<(String, Zone, Compaction)>, StoreError> {
        let mut done = Vec::new();
        for (layer, zone) in self.zones().await? {
            for outcome in self.maintain(&layer, zone).await? {
                done.push((layer.clone(), zone, outcome));
            }
        }
        Ok(done)
    }

    /// Everything one zone is due, now: its expired archives retired, its
    /// tiered runs merged until none is left to merge, its retired and
    /// orphaned archives removed past their grace. What [`Self::compact_due`]
    /// does to every zone, for a process that knows which zone has just been
    /// written to, or that goes round the store a few zones at a time.
    /// Returns the merges it made, and a `Superseded` if a writer got in
    /// first — the zone is then due again.
    pub async fn maintain(&self, layer: &str, zone: Zone) -> Result<Vec<Compaction>, StoreError> {
        let l = self.layer(layer)?.clone();
        let mut done = Vec::new();
        self.prune_expired(&l, zone).await?;
        loop {
            match self.compact_tiered(layer, zone).await? {
                Compaction::Nothing => break,
                c @ Compaction::Superseded => {
                    done.push(c);
                    break;
                }
                c => done.push(c),
            }
        }
        self.cleanup(&l, zone).await?;
        Ok(done)
    }

    /// Counts a zone's tiles archive by archive, from the bucket: how many a
    /// reader can get, and how many are held more than once. A tile in two
    /// archives is not an error — the newest wins — but it is space, and the
    /// measure of how often two writers stored the same thing.
    pub async fn census(&self, layer: &str, zone: Zone) -> Result<Census, StoreError> {
        let l = self.layer(layer)?.clone();
        let now = (self.clock)();
        let m = manifest::read(self.store.as_ref(), &l.zone_prefix(zone)).await?.manifest;
        let mut held: HashMap<u64, u32> = HashMap::new();
        let mut census = Census::default();
        for a in m.archives.iter().filter(|a| !l.is_expired(&a.epoch, now)) {
            census.archives += 1;
            for id in archive::ids(self.reader(&a.key).await?).await? {
                census.copies += 1;
                *held.entry(id).or_default() += 1;
            }
        }
        census.tiles = held.len() as u64;
        census.repeated = held.values().filter(|n| **n > 1).count() as u64;
        Ok(census)
    }

    /// Retires the archives of expired epochs without merging anything.
    async fn prune_expired(&self, layer: &Layer, zone: Zone) -> Result<(), StoreError> {
        let prefix = layer.zone_prefix(zone);
        let now = (self.clock)();
        let m = manifest::read(self.store.as_ref(), &prefix).await?.manifest;
        if !m.archives.iter().any(|a| layer.is_expired(&a.epoch, now)) {
            return Ok(());
        }
        let now_s = now_secs(now);
        self.publish(&(layer.name.clone(), zone), &prefix, |m| {
            let mut next = m.clone();
            let (gone, kept): (Vec<_>, Vec<_>) = m.archives.iter().cloned().partition(|a| layer.is_expired(&a.epoch, now));
            next.archives = kept;
            next.retired.extend(gone.into_iter().map(|a| Retired { key: a.key, retired: now_s }));
            Some(next)
        })
        .await?;
        self.forget_manifest(&(layer.name.clone(), zone));
        Ok(())
    }

    /// Merges a contiguous run of archives into one and publishes it in the
    /// run's place, provided the run is still there, whole and contiguous.
    async fn merge_run(&self, layer: &Layer, zone: Zone, run: &[ArchiveRef]) -> Result<Compaction, StoreError> {
        if run.len() < 2 {
            return Ok(Compaction::Nothing);
        }
        let prefix = layer.zone_prefix(zone);
        let now = (self.clock)();
        let epoch = run[0].epoch.clone();

        // Only the run is copied locally — disk use is bounded by what is
        // merged, not by the zone. One sequential download each, then every
        // tile from memory-mapped files.
        let dir = tempfile::tempdir()?;
        let mut readers = Vec::with_capacity(run.len());
        for (i, a) in run.iter().enumerate() {
            let local = dir.path().join(format!("{i}.pmtiles"));
            download(self.store.as_ref(), &a.key, &local).await?;
            let r = Arc::new(archive::open_local(&local).await?);
            let ids = archive::ids(r.clone()).await?;
            readers.push((r, ids));
        }
        let zooms = readers
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(r, _)| (r.get_header().min_zoom, r.get_header().max_zoom))
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)));

        // The writer is synchronous (`Write + Seek`): the merge stream feeds
        // it through a bounded channel, the only bridge between the two. A
        // failed tile is sent on like any other: the writer stops on it and
        // reports it, so the error surfaces from one place.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u64, Bytes), StoreError>>(MERGE_QUEUE);
        let l = layer.clone();
        let e = epoch.clone();
        let writer =
            tokio::task::spawn_blocking(move || archive::write(&l, &e, zooms, std::iter::from_fn(move || rx.blocking_recv())));
        let mut tiles = std::pin::pin!(merged(readers));
        let mut bytes = 0u64;
        while let Some(tile) = futures_util::StreamExt::next(&mut tiles).await {
            let failed = tile.is_err();
            if let Ok((_, b)) = &tile {
                bytes += b.len() as u64;
            }
            if tx.send(tile).await.is_err() || failed {
                break;
            }
        }
        drop(tx);
        let (file, count) = writer.await.map_err(|e| StoreError::Corrupt(format!("archive writer: {e}")))??;

        let object = archive_key(&prefix, &epoch, now);
        upload(self.store.as_ref(), file.path(), &object).await?;
        // A merged archive's fetch times span its inputs'; unknown if any
        // input's are.
        let fetch_bounds: Option<Vec<(u64, u64)>> =
            run.iter().map(|a| Some((a.oldest_fetch_ms?, a.newest_fetch_ms?))).collect();
        let oldest_fetch_ms = fetch_bounds.as_ref().and_then(|b| b.iter().map(|x| x.0).min());
        let newest_fetch_ms = fetch_bounds.as_ref().and_then(|b| b.iter().map(|x| x.1).max());
        let entry = ArchiveRef {
            key: object.clone(),
            epoch,
            created: now_secs(now),
            tiles: count,
            bytes,
            oldest_fetch_ms,
            newest_fetch_ms,
        };
        let run_keys: Vec<String> = run.iter().map(|a| a.key.clone()).collect();
        let now_s = now_secs(now);

        let published = self
            .publish(&(layer.name.clone(), zone), &prefix, |m| {
                // The run must still be there, whole, contiguous, in order;
                // otherwise someone else compacted part of it first.
                let first = m.archives.iter().position(|a| a.key == run_keys[0])?;
                let here = m.archives.get(first..first + run_keys.len())?;
                if here.iter().map(|a| &a.key).ne(run_keys.iter()) {
                    return None;
                }
                let mut next = m.clone();
                next.archives.splice(first..first + run_keys.len(), [entry.clone()]);
                next.retired.extend(run_keys.iter().map(|k| Retired { key: k.clone(), retired: now_s }));
                Some(next)
            })
            .await?;

        let key = (layer.name.clone(), zone);
        match published {
            Some(m) => {
                self.remember_manifest(&key, Arc::new(m));
                metrics::counter!("tuile_tiles_compactions_total", "layer" => layer.name.clone()).increment(1);
                metrics::counter!("tuile_tiles_compacted_bytes_total", "layer" => layer.name.clone()).increment(bytes);
                self.cleanup(layer, zone).await?;
                Ok(Compaction::Merged { merged: run.len(), tiles: count })
            }
            None => {
                let _ = self.store.delete(&Path::from(object)).await;
                Ok(Compaction::Superseded)
            }
        }
    }

    /// Deletes retired archives past their grace, and unreferenced archives
    /// old enough to be orphans. Returns how many objects were removed.
    pub async fn cleanup(&self, layer: &Layer, zone: Zone) -> Result<usize, StoreError> {
        let prefix = layer.zone_prefix(zone);
        let now = (self.clock)();
        let now_s = now_secs(now);
        let grace = self.cfg.retire_grace.as_secs();

        let expired: Vec<String> = {
            let m = manifest::read(self.store.as_ref(), &prefix).await?.manifest;
            m.retired.iter().filter(|r| now_s.saturating_sub(r.retired) >= grace).map(|r| r.key.clone()).collect()
        };
        if !expired.is_empty() {
            self.publish(&(layer.name.clone(), zone), &prefix, |m| {
                let mut next = m.clone();
                next.retired.retain(|r| !expired.contains(&r.key));
                Some(next)
            })
            .await?;
        }
        let mut removed = 0;
        for k in &expired {
            match self.store.delete(&Path::from(k.as_str())).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => removed += 1,
                Err(e) => return Err(e.into()),
            }
        }

        // Orphans: archives under the zone that no manifest mentions.
        let m = manifest::read(self.store.as_ref(), &prefix).await?.manifest;
        let known: Vec<&str> =
            m.archives.iter().map(|a| a.key.as_str()).chain(m.retired.iter().map(|r| r.key.as_str())).collect();
        let listed: Vec<object_store::ObjectMeta> =
            self.store.list(Some(&Path::from(prefix.as_str()))).try_collect().await?;
        for meta in listed {
            let k = meta.location.to_string();
            if !k.ends_with(".pmtiles") || known.contains(&k.as_str()) {
                continue;
            }
            let modified: SystemTime = meta.last_modified.into();
            let old = now.duration_since(modified).map(|a| a >= self.cfg.orphan_grace).unwrap_or(false);
            if old {
                self.store.delete(&meta.location).await?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

impl Manifests {
    /// Keeps a manifest unless a newer generation is already kept: a read
    /// that started before our own publication must not undo it.
    /// Says whether it was news: no copy was kept, or an older one.
    fn remember(&self, key: &ZoneKey, m: Arc<Manifest>) -> bool {
        let Ok(mut cache) = self.cache.lock() else { return false };
        let kept = cache.get(key).map(|(_, kept)| kept.generation);
        if kept.is_some_and(|kept| kept > m.generation) {
            return false;
        }
        cache.insert(key.clone(), (Instant::now(), m.clone()));
        kept != Some(m.generation)
    }
}

/// A zone as it now is: the local copies of what it was made of before
/// (merged, expired) go at once, rather than when the disk budget is short.
fn drop_unnamed(disk: Option<&ArchiveCache>, zone_prefix: &str, m: &Manifest) {
    let Some(disk) = disk else { return };
    let named: Vec<&str> = m.archives.iter().map(|a| a.key.as_str()).collect();
    let dropped = disk.keep_only(&format!("{zone_prefix}/"), &named);
    if dropped > 0 {
        metrics::counter!("tuile_tiles_archive_copies_dropped_total").increment(dropped as u64);
    }
}

/// The zones around a zone at its level, within the grid. `top` has none.
fn neighbours(layer: &Layer, zone: Zone) -> Vec<Zone> {
    let Zone::Cell { x, y } = zone else { return Vec::new() };
    let (w, h) = layer.grid.size(layer.zone_level);
    let mut out = Vec::with_capacity(8);
    for dy in -1i64..=1 {
        for dx in -1i64..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            // The grid wraps east–west, not north–south.
            let nx = (i64::from(x) + dx).rem_euclid(w as i64);
            let ny = i64::from(y) + dy;
            if ny < 0 || ny >= h as i64 {
                continue;
            }
            out.push(Zone::Cell { x: nx as u32, y: ny as u32 });
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The zone a `…/manifest.json` key belongs to, if it is one of `layer`'s.
fn parse_manifest_key(layer: &Layer, key: &str) -> Option<Zone> {
    let rest = key.strip_prefix(layer.name.as_str())?.strip_prefix('/')?.strip_suffix("/manifest.json")?;
    if rest == "top" {
        return Some(Zone::Top);
    }
    let mut parts = rest.strip_prefix("zones/")?.split('/');
    let level = parts.next()?.strip_prefix('z')?.parse::<u8>().ok()?;
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    (level == layer.zone_level && parts.next().is_none()).then_some(Zone::Cell { x, y })
}

/// A new archive's key: unique, and sortable by time within the zone.
fn archive_key(prefix: &str, epoch: &str, now: SystemTime) -> String {
    let nanos = now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let salt: u64 = rand::rng().random();
    format!("{prefix}/{epoch}-{nanos:024}-{salt:016x}.pmtiles")
}

/// The k-way merge, as a stream: every tile id once, in increasing order, with
/// the bytes of the newest input that has it.
fn merged(
    inputs: Vec<(Arc<archive::LocalReader>, Vec<u64>)>,
) -> impl futures_util::Stream<Item = Result<(u64, Bytes), StoreError>> {
    let heads = vec![0usize; inputs.len()];
    futures_util::stream::unfold((inputs, heads), |(inputs, mut heads)| async move {
        let id = inputs.iter().enumerate().filter_map(|(i, (_, ids))| ids.get(heads[i]).copied()).min()?;
        let mut newest = 0;
        for (i, (_, ids)) in inputs.iter().enumerate() {
            if ids.get(heads[i]) == Some(&id) {
                newest = i;
                heads[i] += 1;
            }
        }
        let tile = match pmtiles::TileId::new(id) {
            Ok(tid) => match inputs[newest].0.get_tile(tid).await {
                Ok(Some(bytes)) => Ok((id, bytes)),
                Ok(None) => Err(StoreError::Corrupt(format!("tile {id} listed but absent"))),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        };
        Some((tile, (inputs, heads)))
    })
}

/// Uploads a local file in parts.
async fn upload(store: &dyn ObjectStore, path: &std::path::Path, key: &str) -> Result<(), StoreError> {
    let upload = store.put_multipart(&Path::from(key)).await?;
    let mut writer = WriteMultipart::new_with_chunk_size(upload, UPLOAD_PART);
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; UPLOAD_READ];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.wait_for_capacity(UPLOAD_PARTS_IN_FLIGHT).await?;
        writer.write(&buf[..n]);
    }
    writer.finish().await?;
    Ok(())
}

/// Downloads an object to a local file.
pub(crate) async fn download(store: &dyn ObjectStore, key: &str, to: &std::path::Path) -> Result<(), StoreError> {
    use tokio::io::AsyncWriteExt;
    let got = store.get(&Path::from(key)).await?;
    let mut stream = got.into_stream();
    let mut out = tokio::fs::File::create(to).await?;
    while let Some(chunk) = stream.try_next().await? {
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    Ok(())
}
