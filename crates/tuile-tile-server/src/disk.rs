// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Archives kept on local disk, so a zone's second tile costs no round trip.
//!
//! Reading a tile from an archive in the bucket costs up to three sequential
//! round trips (header and root directory, leaf directory, tile) per archive
//! consulted. A zone is a few megabytes: fetching all of its archives in one
//! request each, the first time any tile of it is asked for, makes every later
//! read of that zone a memory-mapped lookup.
//!
//! Archives are immutable, so a copy is never stale: there is nothing to
//! invalidate, only a budget to keep. A compaction publishes a new key and the
//! archives it replaced simply stop being read, until the least recently used
//! order evicts them. The directory survives restarts: it is indexed again on
//! open, so a restarted server starts warm.
//!
//! Downloads run in the background, a bounded number at a time, and each
//! archive is fetched once however many readers want it. Until it lands, reads
//! go to the bucket as before.

use std::collections::HashSet;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use object_store::ObjectStore;

use crate::archive::{self, LocalReader};
use crate::lru::Lru;
use crate::store::download;
use crate::StoreError;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// Default for [`DiskCacheConfig::budget_bytes`].
pub const DEFAULT_DISK_BUDGET: u64 = 8 * GIB;
/// Default for [`DiskCacheConfig::downloads_in_flight`]: enough to fetch a
/// zone's handful of archives and its neighbours' at once, few enough not to
/// crowd the reads a request is waiting on.
pub const DEFAULT_DOWNLOADS_IN_FLIGHT: usize = 8;
/// Default for [`DiskCacheConfig::max_archive_bytes`]: past this, an archive
/// is read by ranges rather than copied whole.
pub const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 256 * MIB;

/// Suffix of a download not yet complete; such files are removed on open.
const PARTIAL: &str = "part";

/// Where and how much to keep on disk.
#[derive(Debug, Clone)]
pub struct DiskCacheConfig {
    pub dir: PathBuf,
    /// Bytes of archives kept; past it, the least recently read go first.
    pub budget_bytes: u64,
    /// Archives downloaded at once.
    pub downloads_in_flight: usize,
    /// Archives larger than this (by the tile bytes their manifest states)
    /// are never copied whole.
    pub max_archive_bytes: u64,
    /// When a zone is first read from the bucket, also copy the zones around
    /// it at the same level: a camera that moves reaches them next.
    pub prefetch_neighbours: bool,
}

impl DiskCacheConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            budget_bytes: DEFAULT_DISK_BUDGET,
            downloads_in_flight: DEFAULT_DOWNLOADS_IN_FLIGHT,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
            prefetch_neighbours: false,
        }
    }
}

/// The local copies, by archive key.
pub(crate) struct ArchiveCache {
    pub(crate) cfg: DiskCacheConfig,
    readers: Mutex<Lru<String, Arc<LocalReader>>>,
    inflight: Mutex<HashSet<String>>,
    /// The number of downloads in flight, so a caller can wait for quiet.
    pending: tokio::sync::watch::Sender<usize>,
    permits: Arc<tokio::sync::Semaphore>,
}

/// Is this archive key a plain relative path, safe to join under the cache
/// directory?
fn safe_relative(key: &str) -> bool {
    !key.is_empty() && FsPath::new(key).components().all(|c| matches!(c, Component::Normal(_)))
}

impl ArchiveCache {
    /// Opens the cache directory, indexing the archives already in it (oldest
    /// first by modification time, so the recently fetched are kept longest).
    pub(crate) async fn open(cfg: DiskCacheConfig) -> Result<Arc<Self>, StoreError> {
        std::fs::create_dir_all(&cfg.dir)?;
        let mut found: Vec<(SystemTime, String, u64)> = Vec::new();
        let mut stack = vec![cfg.dir.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == PARTIAL) {
                    let _ = std::fs::remove_file(&path);
                } else if path.extension().is_some_and(|e| e == "pmtiles") {
                    if let Ok(rel) = path.strip_prefix(&cfg.dir) {
                        let key = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
                        found.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), key, meta.len()));
                    }
                }
            }
        }
        found.sort();
        let mut lru = Lru::new(cfg.budget_bytes);
        for (_, key, len) in found {
            let path = cfg.dir.join(&key);
            match archive::open_local(&path).await {
                Ok(r) => {
                    for (gone, _) in lru.insert(key, Arc::new(r), len) {
                        let _ = std::fs::remove_file(cfg.dir.join(gone));
                    }
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let (pending, _) = tokio::sync::watch::channel(0);
        Ok(Arc::new(Self {
            permits: Arc::new(tokio::sync::Semaphore::new(cfg.downloads_in_flight.max(1))),
            cfg,
            readers: Mutex::new(lru),
            inflight: Mutex::default(),
            pending,
        }))
    }

    /// The local copy of an archive, if there is one.
    pub(crate) fn reader(&self, key: &str) -> Option<Arc<LocalReader>> {
        self.readers.lock().ok()?.get(&key.to_string())
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.readers.lock().map(|r| r.contains(&key.to_string())).unwrap_or(false)
    }

    /// Bytes on disk.
    pub(crate) fn bytes(&self) -> u64 {
        self.readers.lock().map(|r| r.weight()).unwrap_or(0)
    }

    /// Copies an archive in the background unless it is here, on its way, or
    /// too large. Returns whether a download was started.
    pub(crate) fn schedule(self: &Arc<Self>, objects: Arc<dyn ObjectStore>, key: &str, bytes: u64) -> bool {
        if bytes > self.cfg.max_archive_bytes || !safe_relative(key) || self.has(key) {
            return false;
        }
        {
            let Ok(mut inflight) = self.inflight.lock() else { return false };
            if !inflight.insert(key.to_string()) {
                return false;
            }
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime to run it on: nothing is copied.
            self.done(key);
            return false;
        };
        self.pending.send_modify(|n| *n += 1);
        let this = self.clone();
        let key = key.to_string();
        runtime.spawn(async move {
            if let Err(e) = this.fetch(objects.as_ref(), &key).await {
                tracing::debug!(key, error = %e, "archive copy failed; reads stay remote");
            }
            this.done(&key);
            this.pending.send_modify(|n| *n = n.saturating_sub(1));
        });
        true
    }

    fn done(&self, key: &str) {
        if let Ok(mut inflight) = self.inflight.lock() {
            inflight.remove(key);
        }
    }

    /// Copies an archive now, and waits for it.
    pub(crate) async fn fetch(&self, objects: &dyn ObjectStore, key: &str) -> Result<Arc<LocalReader>, StoreError> {
        if let Some(r) = self.reader(key) {
            return Ok(r);
        }
        if !safe_relative(key) {
            return Err(StoreError::Corrupt(format!("{key}: not a relative key")));
        }
        let _permit = self.permits.acquire().await.map_err(|_| StoreError::Poisoned)?;
        let path = self.cfg.dir.join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Unique per download: a scheduled copy and an awaited one may race;
        // each renames its own complete file into place.
        let salt: u64 = rand::Rng::random(&mut rand::rng());
        let partial = path.with_extension(format!("{salt:016x}.{PARTIAL}"));
        if let Err(e) = download(objects, key, &partial).await {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        std::fs::rename(&partial, &path)?;
        let len = std::fs::metadata(&path)?.len();
        let reader = Arc::new(archive::open_local(&path).await?);
        metrics::counter!("tuile_tiles_archive_copies_total").increment(1);
        metrics::counter!("tuile_tiles_archive_copy_bytes_total").increment(len);
        let evicted = self.readers.lock().map_err(|_| StoreError::Poisoned)?.insert(key.to_string(), reader.clone(), len);
        for (gone, _) in evicted {
            // A reader still holding the old mapping keeps it until it is done.
            let _ = std::fs::remove_file(self.cfg.dir.join(gone));
        }
        Ok(reader)
    }

    /// Waits until no download is in flight.
    pub(crate) async fn settle(&self) {
        let mut rx = self.pending.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }
}

#[cfg(test)]
mod tests {
    use super::safe_relative;

    #[test]
    fn only_plain_relative_keys_land_on_disk() {
        assert!(safe_relative("asset-2/zones/z10/522/373/202609-1-a.pmtiles"));
        assert!(!safe_relative("../etc/passwd"));
        assert!(!safe_relative("/abs/path.pmtiles"));
        assert!(!safe_relative(""));
    }
}
