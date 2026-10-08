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
//! Each archive is fetched once however many readers want it. A small archive
//! — a typical zone — is waited for: its first reader pays one whole-object
//! request instead of the range reads it would otherwise make *besides* the
//! copy, and every reader behind it is local. A large one (the coarse `top`
//! zone) is copied in the background, one at a time, while reads go to the
//! bucket by ranges as before.

use std::collections::HashMap;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use object_store::ObjectStore;

use crate::archive::{self, LocalReader};
use crate::lru::Lru;
use crate::store::download;
use crate::StoreError;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// Default for [`DiskCacheConfig::budget_bytes`].
pub const DEFAULT_DISK_BUDGET: u64 = 8 * GIB;
/// Default for [`DiskCacheConfig::downloads_in_flight`]: small copies are
/// what the first readers of their zones wait on, as many as the requests a
/// burst of new zones brings.
pub const DEFAULT_DOWNLOADS_IN_FLIGHT: usize = 16;
/// Default for [`DiskCacheConfig::await_copy_bytes`]: a zone of a few
/// megabytes comes down in about the time of the two or three range reads a
/// single tile of it costs.
pub const DEFAULT_AWAIT_COPY_BYTES: u64 = 4 * MIB;
/// Archives at least this large are copied one at a time, on a queue of their
/// own that yields to the small copies: a coarse zone of tens of megabytes
/// must neither take the bandwidth readers are waiting on nor hold up the
/// copies of the small zones behind it.
pub const LARGE_ARCHIVE_BYTES: u64 = 16 * MIB;
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
    /// Archives smaller than [`LARGE_ARCHIVE_BYTES`] downloaded at once;
    /// larger ones go one at a time, besides.
    pub downloads_in_flight: usize,
    /// Archives larger than this (by the tile bytes their manifest states)
    /// are never copied whole.
    pub max_archive_bytes: u64,
    /// When a zone is first read from the bucket, also copy the zones around
    /// it at the same level: a camera that moves reaches them next.
    pub prefetch_neighbours: bool,
    /// A read waits for the copy of an archive up to this size rather than
    /// reading it by ranges alongside the copy. `0`: never wait.
    pub await_copy_bytes: u64,
}

impl DiskCacheConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            budget_bytes: DEFAULT_DISK_BUDGET,
            downloads_in_flight: DEFAULT_DOWNLOADS_IN_FLIGHT,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
            prefetch_neighbours: false,
            await_copy_bytes: DEFAULT_AWAIT_COPY_BYTES,
        }
    }
}

/// The local copies, by archive key.
pub(crate) struct ArchiveCache {
    pub(crate) cfg: DiskCacheConfig,
    readers: Mutex<Lru<String, Arc<LocalReader>>>,
    inflight: Mutex<HashMap<String, ArchiveCopy>>,
    /// The number of downloads in flight, so a caller can wait for quiet.
    pending: tokio::sync::watch::Sender<usize>,
    permits: Arc<tokio::sync::Semaphore>,
    large_permits: Arc<tokio::sync::Semaphore>,
    /// Small copies in flight or queued: a large copy starts only at zero.
    small: tokio::sync::watch::Sender<usize>,
}

/// Counts a small copy for as long as it lives.
struct SmallCopy<'a>(&'a tokio::sync::watch::Sender<usize>);

impl<'a> SmallCopy<'a> {
    fn new(small: &'a tokio::sync::watch::Sender<usize>) -> Self {
        small.send_modify(|n| *n += 1);
        Self(small)
    }
}

impl Drop for SmallCopy<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// A copy in flight: resolves to whether the archive is now local.
pub(crate) type ArchiveCopy = Shared<BoxFuture<'static, bool>>;

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
        let (small, _) = tokio::sync::watch::channel(0);
        Ok(Arc::new(Self {
            permits: Arc::new(tokio::sync::Semaphore::new(cfg.downloads_in_flight.max(1))),
            large_permits: Arc::new(tokio::sync::Semaphore::new(1)),
            cfg,
            readers: Mutex::new(lru),
            inflight: Mutex::default(),
            pending,
            small,
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

    /// The copy of an archive, started unless it is on its way: `None` when
    /// it is already here or is never copied (too large, odd key, no
    /// runtime). The flag says whether this call started it. The copy runs to
    /// its end whether or not anyone awaits it.
    pub(crate) fn copy(self: &Arc<Self>, objects: Arc<dyn ObjectStore>, key: &str, bytes: u64) -> Option<(ArchiveCopy, bool)> {
        if bytes > self.cfg.max_archive_bytes || !safe_relative(key) || self.has(key) {
            return None;
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let mut inflight = self.inflight.lock().ok()?;
        if let Some(c) = inflight.get(key) {
            return Some((c.clone(), false));
        }
        self.pending.send_modify(|n| *n += 1);
        let this = self.clone();
        let k = key.to_string();
        let copy = async move {
            let ok = match this.fetch(objects.as_ref(), &k, bytes).await {
                Ok(_) => true,
                Err(e) => {
                    tracing::debug!(key = k, error = %e, "archive copy failed; reads stay remote");
                    false
                }
            };
            if let Ok(mut inflight) = this.inflight.lock() {
                inflight.remove(&k);
            }
            this.pending.send_modify(|n| *n = n.saturating_sub(1));
            ok
        }
        .boxed()
        .shared();
        inflight.insert(key.to_string(), copy.clone());
        runtime.spawn(copy.clone());
        Some((copy, true))
    }

    /// Copies an archive now, and waits for it. `bytes` is the size its
    /// manifest states, which picks the queue.
    pub(crate) async fn fetch(&self, objects: &dyn ObjectStore, key: &str, bytes: u64) -> Result<Arc<LocalReader>, StoreError> {
        if let Some(r) = self.reader(key) {
            return Ok(r);
        }
        if !safe_relative(key) {
            return Err(StoreError::Corrupt(format!("{key}: not a relative key")));
        }
        let large = bytes >= LARGE_ARCHIVE_BYTES;
        let _small = (!large).then(|| SmallCopy::new(&self.small));
        let _permit = if large {
            let permit = self.large_permits.acquire().await.map_err(|_| StoreError::Poisoned)?;
            // Behind every small copy: those are what readers wait on.
            let mut quiet = self.small.subscribe();
            let _ = quiet.wait_for(|n| *n == 0).await;
            permit
        } else {
            self.permits.acquire().await.map_err(|_| StoreError::Poisoned)?
        };
        // Copied while this one waited its turn.
        if let Some(r) = self.reader(key) {
            return Ok(r);
        }
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

    /// Drops the copies under `prefix` that `named` does not hold: what a
    /// zone's manifest no longer names is never read through it again, and
    /// would otherwise sit in the budget until the least recently used of
    /// all. Returns how many were dropped.
    pub(crate) fn keep_only(&self, prefix: &str, named: &[&str]) -> usize {
        let Ok(mut readers) = self.readers.lock() else { return 0 };
        let gone: Vec<String> =
            readers.keys().filter(|k| k.starts_with(prefix) && !named.contains(&k.as_str())).cloned().collect();
        for key in &gone {
            readers.remove(key);
            // A reader still holding the mapping keeps it until it is done.
            let _ = std::fs::remove_file(self.cfg.dir.join(key));
        }
        gone.len()
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
