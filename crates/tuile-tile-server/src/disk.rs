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

use futures_util::future::{BoxFuture, FutureExt, Shared, WeakShared};
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

/// Suffix of a download not yet complete; such files are removed on open
/// once they are older than [`PARTIAL_STALE`].
const PARTIAL: &str = "part";
/// A partial download this old is left over by a process that died: the
/// directory may be shared by the processes of a machine, and a younger one
/// is another's download in flight.
const PARTIAL_STALE: std::time::Duration = std::time::Duration::from_secs(3600);

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
    /// The copies in flight, held weakly: a copy lives as long as its task
    /// or a reader awaiting it, never because it is listed here. Held
    /// strongly, a copy whose task ended with its runtime — a bake's —
    /// stayed here unpolled and counted as a small copy for ever, and every
    /// large copy waited on it ([`InFlight`]).
    inflight: Mutex<HashMap<String, Listed>>,
    /// Numbers the copies, so that one ending takes its own entry only.
    copies: std::sync::atomic::AtomicU64,
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

/// A copy as the table of copies in flight lists it: its number, and a
/// handle that does not keep it alive.
type Listed = (u64, WeakShared<BoxFuture<'static, bool>>);

/// A copy counted in flight for as long as its future lives: dropped when the
/// copy ends, and when it is abandoned unfinished (its task dropped with its
/// runtime, no reader awaiting it), so that it is then neither counted
/// ([`ArchiveCache::pending`]) nor listed — the archive can be asked for again.
struct InFlight {
    cache: std::sync::Weak<ArchiveCache>,
    key: String,
    id: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        if let Ok(mut inflight) = cache.inflight.lock() {
            // Only this copy's entry: a newer copy of the key, started once
            // this one could no longer be reached, stays.
            if inflight
                .get(&self.key)
                .is_some_and(|(id, _)| *id == self.id)
            {
                inflight.remove(&self.key);
            }
        }
        cache.pending.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Is this archive key a plain relative path, safe to join under the cache
/// directory?
fn safe_relative(key: &str) -> bool {
    !key.is_empty()
        && FsPath::new(key)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
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
                    let stale = meta
                        .modified()
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_none_or(|age| age >= PARTIAL_STALE);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                    }
                } else if path.extension().is_some_and(|e| e == "pmtiles") {
                    if let Ok(rel) = path.strip_prefix(&cfg.dir) {
                        let key = rel
                            .components()
                            .map(|c| c.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/");
                        found.push((
                            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                            key,
                            meta.len(),
                        ));
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
            copies: std::sync::atomic::AtomicU64::new(0),
            pending,
            small,
        }))
    }

    /// The local copy of an archive, if there is one.
    pub(crate) fn reader(&self, key: &str) -> Option<Arc<LocalReader>> {
        self.readers.lock().ok()?.get(&key.to_string())
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.readers
            .lock()
            .map(|r| r.contains(&key.to_string()))
            .unwrap_or(false)
    }

    /// Bytes on disk.
    pub(crate) fn bytes(&self) -> u64 {
        self.readers.lock().map(|r| r.weight()).unwrap_or(0)
    }

    /// The copy of an archive, started unless it is on its way: `None` when
    /// it is already here or is never copied (too large, odd key, no
    /// runtime). The flag says whether this call started it. The copy runs to
    /// its end whether or not anyone awaits it.
    pub(crate) fn copy(
        self: &Arc<Self>,
        objects: Arc<dyn ObjectStore>,
        key: &str,
        bytes: u64,
    ) -> Option<(ArchiveCopy, bool)> {
        if bytes > self.cfg.max_archive_bytes || !safe_relative(key) || self.has(key) {
            return None;
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let mut inflight = self.inflight.lock().ok()?;
        if let Some(c) = inflight.get(key).and_then(|(_, w)| w.upgrade()) {
            return Some((c, false));
        }
        self.pending.send_modify(|n| *n += 1);
        // Counted until the copy's future is dropped, finished or not.
        let id = self
            .copies
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let flight = InFlight {
            cache: Arc::downgrade(self),
            key: key.to_string(),
            id,
        };
        let this = self.clone();
        let k = key.to_string();
        let copy = async move {
            let _flight = flight;
            match this.fetch(objects.as_ref(), &k, bytes).await {
                Ok(_) => true,
                Err(e) => {
                    tracing::debug!(key = k, error = %e, "archive copy failed; reads stay remote");
                    false
                }
            }
        }
        .boxed()
        .shared();
        // A future not yet polled always has a weak handle.
        if let Some(weak) = copy.downgrade() {
            inflight.insert(key.to_string(), (id, weak));
        }
        // Released before anything can drop the copy: its end takes the lock.
        drop(inflight);
        runtime.spawn(copy.clone());
        Some((copy, true))
    }

    /// Copies an archive now, and waits for it. `bytes` is the size its
    /// manifest states, which picks the queue.
    pub(crate) async fn fetch(
        &self,
        objects: &dyn ObjectStore,
        key: &str,
        bytes: u64,
    ) -> Result<Arc<LocalReader>, StoreError> {
        if let Some(r) = self.reader(key) {
            return Ok(r);
        }
        if !safe_relative(key) {
            return Err(StoreError::Corrupt(format!("{key}: not a relative key")));
        }
        let large = bytes >= LARGE_ARCHIVE_BYTES;
        let _small = (!large).then(|| SmallCopy::new(&self.small));
        let _permit = if large {
            let permit = self
                .large_permits
                .acquire()
                .await
                .map_err(|_| StoreError::Poisoned)?;
            // Behind every small copy: those are what readers wait on.
            let mut quiet = self.small.subscribe();
            let _ = quiet.wait_for(|n| *n == 0).await;
            permit
        } else {
            self.permits
                .acquire()
                .await
                .map_err(|_| StoreError::Poisoned)?
        };
        // Copied while this one waited its turn.
        if let Some(r) = self.reader(key) {
            return Ok(r);
        }
        let path = self.cfg.dir.join(key);
        // Copied by another process sharing the directory: an archive is
        // never rewritten, so the file there is the one this would download.
        if let Ok(local) = archive::open_local(&path).await {
            let len = std::fs::metadata(&path)?.len();
            let reader = Arc::new(local);
            metrics::counter!("tuile_tiles_archive_copies_shared_total").increment(1);
            let evicted = self
                .readers
                .lock()
                .map_err(|_| StoreError::Poisoned)?
                .insert(key.to_string(), reader.clone(), len);
            for (gone, _) in evicted {
                let _ = std::fs::remove_file(self.cfg.dir.join(gone));
            }
            return Ok(reader);
        }
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
        let evicted = self
            .readers
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .insert(key.to_string(), reader.clone(), len);
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
        let Ok(mut readers) = self.readers.lock() else {
            return 0;
        };
        let gone: Vec<String> = readers
            .keys()
            .filter(|k| k.starts_with(prefix) && !named.contains(&k.as_str()))
            .cloned()
            .collect();
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

    mod orphaned {
        use std::fmt;
        use std::sync::Arc;
        use std::time::Duration;

        use async_trait::async_trait;
        use futures_util::stream::BoxStream;
        use object_store::memory::InMemory;
        use object_store::path::Path;
        use object_store::{
            CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
            ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
            Result,
        };

        use super::super::{ArchiveCache, DiskCacheConfig, LARGE_ARCHIVE_BYTES};

        /// An origin whose `small` archive never finishes coming down: a
        /// copy of it is in flight for as long as it lives.
        #[derive(Debug, Default)]
        struct Stuck {
            inner: InMemory,
        }

        const SMALL: &str = "asset-2/zones/z10/1/2/small.pmtiles";
        const LARGE: &str = "asset-2/zones/top/large.pmtiles";

        impl fmt::Display for Stuck {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "Stuck")
            }
        }

        #[async_trait]
        impl ObjectStore for Stuck {
            async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
                self.inner.put_opts(l, p, o).await
            }
            async fn put_multipart_opts(
                &self,
                l: &Path,
                o: PutMultipartOptions,
            ) -> Result<Box<dyn MultipartUpload>> {
                self.inner.put_multipart_opts(l, o).await
            }
            async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
                if l.as_ref() == SMALL {
                    std::future::pending::<()>().await;
                }
                self.inner.get_opts(l, o).await
            }
            fn delete_stream(
                &self,
                l: BoxStream<'static, Result<Path>>,
            ) -> BoxStream<'static, Result<Path>> {
                self.inner.delete_stream(l)
            }
            fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
                self.inner.list(p)
            }
            async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
                self.inner.list_with_delimiter(p).await
            }
            async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
                self.inner.copy_opts(f, t, o).await
            }
        }

        /// The bug of 13 November 2026 (videos 220 and 221): a small copy
        /// started on a bake's runtime, the bake ended and its runtime with
        /// it, and the copy's future stayed in the table of copies in
        /// flight, unpolled, counted as a small copy forever. Every large
        /// copy waits for the small ones to be done: the next bake of the
        /// process that read a large archive (a film's first or last chunk,
        /// its coarse `top` zone) waited on it for ever, at no CPU.
        ///
        /// A copy whose runtime ended is abandoned whole: it no longer
        /// counts, and the archive can be asked for again.
        #[test]
        fn a_copy_whose_runtime_ended_holds_up_no_other() {
            let dir = tempfile::tempdir().expect("a directory");
            let origin: Arc<dyn ObjectStore> = Arc::new(Stuck::default());
            let large_bytes = LARGE_ARCHIVE_BYTES + 1;

            // The bake's runtime: it starts the small copy, which gets as
            // far as its download, then ends.
            let bake = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("a runtime");
            let cache = bake
                .block_on(ArchiveCache::open(DiskCacheConfig::new(dir.path())))
                .expect("the cache");
            bake.block_on(async {
                let (_copy, started) = cache.copy(origin.clone(), SMALL, 1024).expect("a copy");
                assert!(started, "this call started it");
                // Long enough for the copy to reach its download.
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
            drop(bake);

            // Another runtime, as the next bake or the tile store's flush:
            // a large copy is not held up by the abandoned small one.
            let next = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("a runtime");
            next.block_on(async {
                origin
                    .put(&Path::from(LARGE), vec![0u8; 64].into())
                    .await
                    .expect("put");
                let fetched = tokio::time::timeout(
                    Duration::from_secs(5),
                    cache.fetch(origin.as_ref(), LARGE, large_bytes),
                )
                .await;
                // Not a valid archive, so an error: what matters is that it
                // answered at all.
                assert!(
                    fetched.is_ok(),
                    "a large copy waited on a small one abandoned with its runtime"
                );
                // And the small archive can be asked for again: a new copy,
                // not the dead one.
                let (_again, started) = cache.copy(origin.clone(), SMALL, 1024).expect("a copy");
                assert!(started, "the abandoned copy is no longer in flight");
            });
        }

        /// The other half of the same bug: an abandoned copy also held one of
        /// the permits small copies download under, for ever. With all of
        /// them held, the small copies readers wait on (a zone's first read,
        /// the tile store's flush) never started.
        #[test]
        fn a_copy_whose_runtime_ended_holds_no_download_permit() {
            let dir = tempfile::tempdir().expect("a directory");
            let origin: Arc<dyn ObjectStore> = Arc::new(Stuck::default());
            let mut cfg = DiskCacheConfig::new(dir.path());
            cfg.downloads_in_flight = 1;
            let bake = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("a runtime");
            let cache = bake.block_on(ArchiveCache::open(cfg)).expect("the cache");
            bake.block_on(async {
                let _ = cache.copy(origin.clone(), SMALL, 1024).expect("a copy");
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
            drop(bake);

            let next = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("a runtime");
            next.block_on(async {
                let other = "asset-2/zones/z10/5/6/other.pmtiles";
                origin
                    .put(&Path::from(other), vec![0u8; 64].into())
                    .await
                    .expect("put");
                let (copy, _) = cache.copy(origin.clone(), other, 64).expect("a copy");
                let done = tokio::time::timeout(Duration::from_secs(5), copy).await;
                assert!(
                    done.is_ok(),
                    "a small copy waited for a permit an abandoned copy held"
                );
            });
        }

        /// A copy that ends leaves the table and the count, however many
        /// readers still hold it.
        #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
        async fn a_finished_copy_is_no_longer_in_flight() {
            let dir = tempfile::tempdir().expect("a directory");
            let origin: Arc<dyn ObjectStore> = Arc::new(Stuck::default());
            let key = "asset-2/zones/z10/3/4/gone.pmtiles";
            let cache = ArchiveCache::open(DiskCacheConfig::new(dir.path()))
                .await
                .expect("the cache");
            // Not in the origin: the copy ends at once, unsuccessful.
            let (copy, started) = cache.copy(origin.clone(), key, 1024).expect("a copy");
            assert!(started);
            assert!(!copy.clone().await, "nothing to copy");
            assert!(
                cache.inflight.lock().unwrap().is_empty(),
                "its entry is gone though a reader holds it"
            );
            assert_eq!(*cache.pending.borrow(), 0, "and it is no longer counted");
            let (_again, started) = cache.copy(origin.clone(), key, 1024).expect("a copy");
            assert!(started, "asked again, it is copied again");
        }
    }

    #[test]
    fn only_plain_relative_keys_land_on_disk() {
        assert!(safe_relative(
            "asset-2/zones/z10/522/373/202609-1-a.pmtiles"
        ));
        assert!(!safe_relative("../etc/passwd"));
        assert!(!safe_relative("/abs/path.pmtiles"));
        assert!(!safe_relative(""));
    }
}
