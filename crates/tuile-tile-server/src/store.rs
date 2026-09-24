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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use rand::Rng;
use tokio::io::AsyncReadExt;

use crate::archive::{self, RemoteReader};
use crate::layer::{Layer, Zone};
use crate::manifest::{self, now_secs, ArchiveRef, Manifest, Retired, Versioned};
use crate::StoreError;

/// A source of the current time, so tests can move it.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

const MIB: usize = 1 << 20;

// Defaults of [`StoreConfig`], one place each.

/// A zone's buffer at this size is frozen: large enough that deltas are few,
/// small enough that a crash loses little and memory stays modest.
pub const DEFAULT_FLUSH_BYTES: usize = 32 * MIB;
/// A buffer this old is frozen even if small, so a quiet zone is not left in
/// memory only.
pub const DEFAULT_FLUSH_AGE: Duration = Duration::from_secs(30);
/// How stale a manifest may be; the bound on cross-instance visibility.
pub const DEFAULT_MANIFEST_TTL: Duration = Duration::from_secs(2);
/// Deltas in the current epoch before a compaction: each is one more range
/// read on a miss.
pub const DEFAULT_COMPACT_MIN_ARCHIVES: usize = 8;
/// A retired archive outlives any reader still holding an older manifest.
pub const DEFAULT_RETIRE_GRACE: Duration = Duration::from_secs(10 * 60);
/// No upload takes this long, so an unreferenced archive this old is an orphan.
pub const DEFAULT_ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);
/// Open archives kept, each holding its header and directories.
pub const DEFAULT_READER_CACHE: usize = 512;
/// Conditional publications tried before giving up on a contended zone.
pub const DEFAULT_PUBLISH_ATTEMPTS: usize = 64;

// Transfer and merge tuning.

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
    /// …or once its oldest tile is this old (see [`TileStore::flush_due`]).
    pub flush_age: Duration,
    /// How long a manifest read is trusted. This bounds how late one instance
    /// sees another's publication.
    pub manifest_ttl: Duration,
    /// A zone is compacted after a flush once its current epoch has this many
    /// archives.
    pub compact_min_archives: usize,
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
            flush_age: DEFAULT_FLUSH_AGE,
            manifest_ttl: DEFAULT_MANIFEST_TTL,
            compact_min_archives: DEFAULT_COMPACT_MIN_ARCHIVES,
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

#[derive(Default)]
struct Buffer {
    pending: BTreeMap<u64, Bytes>,
    bytes: usize,
    since: Option<Instant>,
    /// Frozen tiles being published; still readable.
    flushing: Option<Arc<BTreeMap<u64, Bytes>>>,
}

/// Tiles of every configured layer, kept in an object store.
pub struct TileStore {
    store: Arc<dyn ObjectStore>,
    layers: HashMap<String, Layer>,
    cfg: StoreConfig,
    clock: Clock,
    buffers: Mutex<HashMap<ZoneKey, Buffer>>,
    manifests: Mutex<HashMap<ZoneKey, (Instant, Arc<Manifest>)>>,
    readers: Mutex<HashMap<String, Arc<RemoteReader>>>,
    zone_locks: Mutex<HashMap<ZoneKey, Arc<tokio::sync::Mutex<()>>>>,
    fault: AtomicU8,
    gate: Mutex<Option<Arc<Gate>>>,
}

impl TileStore {
    pub fn new(store: Arc<dyn ObjectStore>, layers: Vec<Layer>, cfg: StoreConfig) -> Self {
        Self::with_clock(store, layers, cfg, Arc::new(SystemTime::now))
    }

    pub fn with_clock(store: Arc<dyn ObjectStore>, layers: Vec<Layer>, cfg: StoreConfig, clock: Clock) -> Self {
        Self {
            store,
            layers: layers.into_iter().map(|l| (l.name.clone(), l)).collect(),
            cfg,
            clock,
            buffers: Mutex::default(),
            manifests: Mutex::default(),
            readers: Mutex::default(),
            zone_locks: Mutex::default(),
            fault: AtomicU8::new(Fault::None as u8),
            gate: Mutex::default(),
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
            metrics::counter!("tuile_tiles_hits_total", "layer" => l.name.clone(), "from" => "buffer").increment(1);
            return Ok(Some(bytes));
        }

        let now = (self.clock)();
        let manifest = self.manifest(&key, l).await?;
        for archive in manifest.archives.iter().rev() {
            if l.is_expired(&archive.epoch, now) {
                continue;
            }
            match self.read_tile(&archive.key, id).await {
                Ok(Some(bytes)) => {
                    metrics::counter!("tuile_tiles_hits_total", "layer" => l.name.clone(), "from" => "archive").increment(1);
                    return Ok(Some(bytes));
                }
                Ok(None) => {}
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
        metrics::counter!("tuile_tiles_misses_total", "layer" => l.name.clone()).increment(1);
        Ok(None)
    }

    fn buffered(&self, key: &ZoneKey, id: u64) -> Option<Bytes> {
        let buffers = self.buffers.lock().ok()?;
        let b = buffers.get(key)?;
        b.pending.get(&id).or_else(|| b.flushing.as_ref().and_then(|f| f.get(&id))).cloned()
    }

    async fn manifest(&self, key: &ZoneKey, layer: &Layer) -> Result<Arc<Manifest>, StoreError> {
        if let Ok(cache) = self.manifests.lock() {
            if let Some((at, m)) = cache.get(key) {
                if at.elapsed() < self.cfg.manifest_ttl {
                    return Ok(m.clone());
                }
            }
        }
        let read = manifest::read(self.store.as_ref(), &layer.zone_prefix(key.1)).await?;
        let m = Arc::new(read.manifest);
        self.remember_manifest(key, m.clone());
        Ok(m)
    }

    fn remember_manifest(&self, key: &ZoneKey, m: Arc<Manifest>) {
        if let Ok(mut cache) = self.manifests.lock() {
            cache.insert(key.clone(), (Instant::now(), m));
        }
    }

    fn forget_manifest(&self, key: &ZoneKey) {
        if let Ok(mut cache) = self.manifests.lock() {
            cache.remove(key);
        }
    }

    async fn read_tile(&self, archive_key: &str, id: u64) -> Result<Option<Bytes>, StoreError> {
        let reader = self.reader(archive_key).await?;
        Ok(reader.get_tile(pmtiles::TileId::new(id)?).await?)
    }

    async fn reader(&self, archive_key: &str) -> Result<Arc<RemoteReader>, StoreError> {
        if let Some(r) = self.readers.lock().ok().and_then(|c| c.get(archive_key).cloned()) {
            return Ok(r);
        }
        let r = Arc::new(archive::open_remote(self.store.clone(), archive_key).await?);
        if let Ok(mut cache) = self.readers.lock() {
            if cache.len() >= self.cfg.reader_cache {
                // Archives are immutable: dropping any of them only costs a
                // header read the next time.
                cache.clear();
            }
            cache.insert(archive_key.to_string(), r.clone());
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
                cache.remove(archive_key);
            }
        }
        gone
    }

    // ── Writing ──────────────────────────────────────────────────────────

    /// Stores a tile. It is readable immediately; it reaches the object store
    /// when its zone's buffer is flushed.
    pub async fn put(&self, layer: &str, level: u8, x: u32, y: u32, bytes: Bytes) -> Result<(), StoreError> {
        let (l, zone, id) = self.locate(layer, level, x, y)?;
        let key = (l.name.clone(), zone);
        let full = {
            let mut buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            let b = buffers.entry(key.clone()).or_default();
            b.bytes += bytes.len();
            if let Some(old) = b.pending.insert(id, bytes) {
                b.bytes -= old.len();
            }
            b.since.get_or_insert_with(Instant::now);
            b.bytes >= self.cfg.flush_bytes
        };
        if full {
            self.flush_zone(&key).await?;
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

    /// Flushes every non-empty buffer.
    pub async fn flush_all(&self) -> Result<usize, StoreError> {
        let keys: Vec<ZoneKey> = {
            let buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            buffers.iter().filter(|(_, b)| !b.pending.is_empty()).map(|(k, _)| k.clone()).collect()
        };
        for key in &keys {
            self.flush_zone(key).await?;
        }
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

        let result = self.publish_delta(&layer, key, &frozen).await;
        {
            let mut buffers = self.buffers.lock().map_err(|_| StoreError::Poisoned)?;
            let b = buffers.entry(key.clone()).or_default();
            b.flushing = None;
            match &result {
                Ok(()) => {}
                Err(StoreError::Fault(_)) => {} // the simulated crash lost them
                Err(_) => {
                    // Put back what was not published, never over a newer write.
                    for (id, bytes) in frozen.iter() {
                        if !b.pending.contains_key(id) {
                            b.bytes += bytes.len();
                            b.pending.insert(*id, bytes.clone());
                        }
                    }
                    if !b.pending.is_empty() {
                        b.since.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
        result?;

        // A zone that keeps receiving deltas is compacted before reads have to
        // walk too many archives.
        let m = self.manifest(key, &layer).await?;
        let epoch = layer.epoch((self.clock)());
        if m.archives.iter().filter(|a| a.epoch == epoch).count() >= self.cfg.compact_min_archives {
            self.compact_locked(&layer, key.1).await?;
        }
        Ok(())
    }

    async fn publish_delta(&self, layer: &Layer, key: &ZoneKey, tiles: &BTreeMap<u64, Bytes>) -> Result<(), StoreError> {
        let now = (self.clock)();
        let epoch = layer.epoch(now);
        let prefix = layer.zone_prefix(key.1);
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

        let entry = ArchiveRef { key: object, epoch, created: now_secs(now), tiles: count };
        let published = self
            .publish(&prefix, |m| {
                let mut next = m.clone();
                next.archives.push(entry.clone());
                Some(next)
            })
            .await?;
        if let Some(m) = published {
            self.remember_manifest(key, Arc::new(m));
        }
        metrics::counter!("tuile_tiles_deltas_total", "layer" => layer.name.clone()).increment(1);
        Ok(())
    }

    /// Derives the next manifest from the current one and publishes it,
    /// retrying on a lost race. `derive` returning `None` abandons.
    async fn publish<F>(&self, prefix: &str, derive: F) -> Result<Option<Manifest>, StoreError>
    where
        F: Fn(&Manifest) -> Option<Manifest>,
    {
        for attempt in 0..self.cfg.publish_attempts {
            let current: Versioned = manifest::read(self.store.as_ref(), prefix).await?;
            let Some(mut next) = derive(&current.manifest) else { return Ok(None) };
            next.generation = current.manifest.generation + 1;
            if manifest::replace(self.store.as_ref(), prefix, &current, &next).await? {
                return Ok(Some(next));
            }
            metrics::counter!("tuile_tiles_publish_conflicts_total").increment(1);
            let backoff = rand::rng().random_range(0..(BACKOFF_BASE_MS << attempt.min(BACKOFF_MAX_DOUBLINGS)));
            tokio::time::sleep(Duration::from_millis(backoff)).await;
        }
        Err(StoreError::Contended(prefix.to_string()))
    }

    // ── Compaction ───────────────────────────────────────────────────────

    /// Merges the archives of the zone's current epoch into one, drops the
    /// expired ones, and removes retired and orphaned objects past their grace.
    pub async fn compact(&self, layer: &str, zone: Zone) -> Result<Compaction, StoreError> {
        let l = self.layer(layer)?.clone();
        let lock = self.zone_lock(&(l.name.clone(), zone));
        let _held = lock.lock().await;
        self.compact_locked(&l, zone).await
    }

    async fn compact_locked(&self, layer: &Layer, zone: Zone) -> Result<Compaction, StoreError> {
        let prefix = layer.zone_prefix(zone);
        let now = (self.clock)();
        let epoch = layer.epoch(now);
        let current = manifest::read(self.store.as_ref(), &prefix).await?.manifest;
        let inputs: Vec<ArchiveRef> = current.archives.iter().filter(|a| a.epoch == epoch).cloned().collect();
        if inputs.len() < 2 {
            self.cleanup(layer, zone).await?;
            return Ok(Compaction::Nothing);
        }

        // Inputs are read from local copies: one sequential download each,
        // then every tile from memory-mapped files.
        let dir = tempfile::tempdir()?;
        let mut readers = Vec::with_capacity(inputs.len());
        for (i, a) in inputs.iter().enumerate() {
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

        // Stream the k-way merge into the writer through a bounded channel:
        // memory holds a few tiles, not the zone.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u64, Bytes), StoreError>>(MERGE_QUEUE);
        let l = layer.clone();
        let e = epoch.clone();
        let writer =
            tokio::task::spawn_blocking(move || archive::write(&l, &e, zooms, std::iter::from_fn(move || rx.blocking_recv())));
        // The writer is synchronous (`Write + Seek`): the stream feeds it
        // through a bounded channel, the only bridge between the two.
        // A failed tile is sent on like any other: the writer stops on it and
        // reports it, so the error surfaces from one place.
        let mut tiles = std::pin::pin!(merged(readers));
        while let Some(tile) = futures_util::StreamExt::next(&mut tiles).await {
            let failed = tile.is_err();
            if tx.send(tile).await.is_err() || failed {
                break;
            }
        }
        drop(tx);
        let (file, count) = writer.await.map_err(|e| StoreError::Corrupt(format!("archive writer: {e}")))??;

        let object = archive_key(&prefix, &epoch, now);
        upload(self.store.as_ref(), file.path(), &object).await?;
        let entry = ArchiveRef { key: object.clone(), epoch: epoch.clone(), created: now_secs(now), tiles: count };
        let input_keys: Vec<String> = inputs.iter().map(|a| a.key.clone()).collect();
        let now_s = now_secs(now);

        let published = self
            .publish(&prefix, |m| {
                // Every input must still be there, or someone else compacted.
                let positions: Vec<usize> = input_keys
                    .iter()
                    .filter_map(|k| m.archives.iter().position(|a| &a.key == k))
                    .collect();
                if positions.len() != input_keys.len() {
                    return None;
                }
                let first = positions.iter().copied().min().unwrap_or(0);
                let mut next = Manifest { generation: m.generation, archives: Vec::new(), retired: m.retired.clone() };
                for (i, a) in m.archives.iter().enumerate() {
                    if i == first {
                        next.archives.push(entry.clone());
                    }
                    // Merged into the new archive, or past its expiry.
                    if input_keys.contains(&a.key) || layer.is_expired(&a.epoch, now) {
                        next.retired.push(Retired { key: a.key.clone(), retired: now_s });
                    } else {
                        next.archives.push(a.clone());
                    }
                }
                Some(next)
            })
            .await?;

        let key = (layer.name.clone(), zone);
        match published {
            Some(m) => {
                self.remember_manifest(&key, Arc::new(m));
                metrics::counter!("tuile_tiles_compactions_total", "layer" => layer.name.clone()).increment(1);
                self.cleanup(layer, zone).await?;
                Ok(Compaction::Merged { merged: inputs.len(), tiles: count })
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
            self.publish(&prefix, |m| {
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
async fn download(store: &dyn ObjectStore, key: &str, to: &std::path::Path) -> Result<(), StoreError> {
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
