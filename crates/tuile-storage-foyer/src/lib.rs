// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    PsyncIoEngineConfig,
};
use tuile_core::storage::ContentStore;

/// The store could not be built. Once running, the store never fails a caller:
/// misses and declined writes are normal, not errors.
#[derive(Debug, thiserror::Error)]
#[error("content store: {message}")]
pub struct StoreError {
    message: String,
    transient: bool,
}

impl StoreError {
    /// From any error, judged by the I/O error at the bottom of its chain.
    fn from_error(error: &(dyn std::error::Error + 'static)) -> Self {
        let mut io = None;
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(e) = cause {
            if let Some(found) = e.downcast_ref::<std::io::Error>() {
                io = Some(found);
            }
            cause = e.source();
        }
        Self {
            message: error.to_string(),
            transient: io.is_some_and(is_transient),
        }
    }

    /// Whether the failure says nothing about the store's files: another
    /// process holding them, the process out of descriptors or memory, a full
    /// disk, a call interrupted or timed out. Such a store is fine and will
    /// open again; **clearing it to start afresh would throw away a good
    /// cache over a passing condition**. Anything else — a file where the
    /// directory should be, data that does not read back — is damage.
    pub fn is_transient(&self) -> bool {
        self.transient
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::from_error(&error)
    }
}

fn is_transient(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{
        Interrupted, OutOfMemory, PermissionDenied, QuotaExceeded, ResourceBusy, StorageFull,
        TimedOut, WouldBlock,
    };
    // ENFILE, EMFILE — the system or the process out of descriptors — which
    // carry no kind of their own. The same numbers on macOS, iOS and Linux.
    const OUT_OF_DESCRIPTORS: [i32; 2] = [23, 24];
    matches!(
        error.kind(),
        Interrupted
            | WouldBlock
            | TimedOut
            | ResourceBusy
            | OutOfMemory
            | StorageFull
            | QuotaExceeded
            | PermissionDenied
    ) || error
        .raw_os_error()
        .is_some_and(|code| OUT_OF_DESCRIPTORS.contains(&code))
}

/// Capacities and location of a [`FoyerStore`].
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub dir: PathBuf,
    /// In-memory tier, bytes. Serves the tiles the camera keeps circling back
    /// to without touching the disk.
    pub memory_bytes: usize,
    /// On-disk tier, bytes. Survives the process, so a second run starts warm.
    pub disk_bytes: usize,
    /// Lifetime for entries whose origin stated none. Bounds how stale a silent
    /// origin's content can get; `None` keeps such entries until the tiers
    /// evict them on capacity alone.
    pub default_ttl: Option<Duration>,
}

impl StoreConfig {
    /// Defaults sized for a globe session: a modest memory tier over a large
    /// disk tier, since the point of the disk is to be bigger than RAM.
    pub fn at(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
            memory_bytes: 256 << 20,
            disk_bytes: 4 << 30,
            // A week: terrain meshes and imagery tiles are effectively
            // immutable, but a persistent store should not promise forever to
            // an origin that never said anything.
            default_ttl: Some(Duration::from_secs(7 * 24 * 60 * 60)),
        }
    }
}

/// A hybrid memory + disk [`ContentStore`]. Cheap to clone — clones share the
/// same cache.
#[derive(Clone)]
pub struct FoyerStore {
    cache: HybridCache<String, Vec<u8>>,
    default_ttl: Option<Duration>,
}

impl FoyerStore {
    /// Builds a store under `dir` with [`StoreConfig::at`] defaults.
    pub async fn new(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with_config(StoreConfig::at(dir.as_ref())).await
    }

    /// Builds a store in the per-user OS cache directory, under `name`. The
    /// common case for apps: persistent across runs, which is the point.
    pub async fn shared(name: &str) -> Result<Self, StoreError> {
        Self::new(default_cache_dir().join(name)).await
    }

    /// Flushes the disk tier and shuts the store down.
    ///
    /// **Required for anything to persist.** Writes are buffered, so a store
    /// that is merely dropped leaves the disk as cold as it found it — a
    /// process killed without calling this keeps none of its cache, however
    /// long it ran. Hosts should call it on their way out.
    pub async fn close(&self) -> Result<(), StoreError> {
        self.cache
            .close()
            .await
            .map_err(|e| StoreError::from_error(&e))
    }

    /// Builds a store with an explicit configuration.
    ///
    /// An entry reaches the disk when the memory tier lets go of it: what is
    /// still in memory when the store is closed is not written. Right for a
    /// cache of what can be asked for again at no cost; see
    /// [`FoyerStore::keeping`] for what must not be.
    pub async fn with_config(cfg: StoreConfig) -> Result<Self, StoreError> {
        Self::build(cfg, foyer::HybridCachePolicy::WriteOnEviction).await
    }

    /// As [`FoyerStore::with_config`], but every entry is written to the disk
    /// as it is put, whether or not the memory tier still holds it: after
    /// [`FoyerStore::close`], all of it is there for the next process. For
    /// what is costly to fetch again and never changes — an archive's blocks.
    pub async fn keeping(cfg: StoreConfig) -> Result<Self, StoreError> {
        Self::build(cfg, foyer::HybridCachePolicy::WriteOnInsertion).await
    }

    async fn build(cfg: StoreConfig, policy: foyer::HybridCachePolicy) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&cfg.dir).map_err(|e| StoreError::from_error(&e))?;
        let device = FsDeviceBuilder::new(&cfg.dir)
            .with_capacity(cfg.disk_bytes)
            .build()
            .map_err(|e| StoreError::from_error(&e))?;
        // Writes to the disk go through a queue, and foyer DROPS what arrives
        // while that queue is over its threshold (32 MiB by default): right
        // for a cache that may forget, and the reason a store asked to keep
        // would otherwise keep a tenth of a film's blocks (measured: 23 of
        // 530 blocks of 700 kB). A keeping store never drops: its queue has
        // no threshold worth the name, and holds what the disk has not yet
        // taken.
        let engine = match policy {
            foyer::HybridCachePolicy::WriteOnInsertion => BlockEngineConfig::new(device)
                .with_buffer_pool_size(64 << 20)
                .with_submit_queue_size_threshold(usize::MAX / 2),
            foyer::HybridCachePolicy::WriteOnEviction => BlockEngineConfig::new(device),
        };
        let cache = HybridCacheBuilder::new()
            .with_policy(policy)
            .memory(cfg.memory_bytes)
            // Capacity is in bytes: weigh each entry by its payload length,
            // or a few large records would evict everything else.
            .with_weighter(|_k: &String, v: &Vec<u8>| v.len())
            .storage()
            .with_io_engine_config(PsyncIoEngineConfig::new())
            .with_engine_config(engine)
            .build()
            .await
            .map_err(|e| StoreError::from_error(&e))?;
        Ok(Self {
            cache,
            default_ttl: cfg.default_ttl,
        })
    }
}

#[async_trait]
impl ContentStore for FoyerStore {
    async fn get(&self, key: &str) -> Option<Bytes> {
        let stored = match self.cache.get(&key.to_owned()).await {
            Ok(Some(entry)) => entry.value().clone(),
            Ok(None) => return None,
            // A store that cannot answer is a miss: the caller recomputes.
            // Failing the load instead would turn a cold disk into a broken
            // globe.
            Err(e) => {
                tracing::debug!(key, error = %e, "content store read failed, treating as miss");
                return None;
            }
        };
        match Stamped::split(&stored) {
            Some((deadline, payload)) if !deadline.has_passed(now_millis()) => {
                Some(Bytes::copy_from_slice(payload))
            }
            // Expired: drop it now rather than re-reading it every frame until
            // capacity happens to reclaim it.
            Some(_) => {
                self.cache.remove(&key.to_owned());
                None
            }
            None => {
                tracing::debug!(key, "malformed store entry, treating as miss");
                None
            }
        }
    }

    async fn put(&self, key: &str, value: Bytes, ttl: Option<Duration>) {
        let deadline = Deadline::in_(ttl.or(self.default_ttl), now_millis());
        self.cache
            .insert(key.to_owned(), Stamped::join(deadline, &value));
    }

    /// The memory tier and the disk tier's index, without a read: an entry
    /// expired but not yet dropped, or a hash collision on the disk, answers
    /// yes — a hint, as the contract allows, and a `get` then misses.
    fn holds(&self, key: &str) -> bool {
        self.cache.contains(key)
    }
}

/// A [`FoyerStore`] that can be closed and opened again behind one handle.
///
/// It is a [keeping](FoyerStore::keeping) store: every entry is written to
/// the disk as it is put. A store that writes on eviction keeps its most
/// recent entries in memory only, and closing it pours them all into the
/// disk queue at once, which drops most of them: measured, 240 of a
/// session's 1 537 tiles found by the next launch after a clean flush, and
/// the rest fetched again over a store the index rightly said did not hold
/// them. A desktop closes it on the way out. A phone's application is never shut down: it goes to the
/// background and is ended there without a word, and whatever the memory tier
/// held is lost — measured on a device: a session's tiles, none of them found
/// by the next launch. So the host suspends the store when it leaves the
/// screen, which closes it and writes everything down, and resumes it when it
/// comes back.
///
/// Callers never see the difference: while suspended a read is a miss and a
/// write is declined, which is what a store is allowed to answer at any time.
pub struct SuspendableStore {
    config: StoreConfig,
    live: std::sync::Mutex<Option<FoyerStore>>,
}

impl SuspendableStore {
    /// Opens the store with `config`.
    pub async fn open(config: StoreConfig) -> Result<Self, StoreError> {
        let store = FoyerStore::keeping(config.clone()).await?;
        Ok(Self {
            config,
            live: std::sync::Mutex::new(Some(store)),
        })
    }

    fn current(&self) -> Option<FoyerStore> {
        self.live.lock().ok().and_then(|live| live.clone())
    }

    /// Writes everything down and closes the store. Reads miss and writes
    /// are declined until [`Self::resume`]. Harmless when already suspended.
    pub async fn suspend(&self) -> Result<(), StoreError> {
        let taken = self.live.lock().ok().and_then(|mut live| live.take());
        match taken {
            Some(store) => store.close().await,
            None => Ok(()),
        }
    }

    /// Opens the store again, from what is on disk. Harmless when open.
    pub async fn resume(&self) -> Result<(), StoreError> {
        if self.current().is_some() {
            return Ok(());
        }
        let store = FoyerStore::keeping(self.config.clone()).await?;
        if let Ok(mut live) = self.live.lock() {
            if live.is_none() {
                *live = Some(store);
            }
        }
        Ok(())
    }

    /// Whether the store is suspended.
    pub fn is_suspended(&self) -> bool {
        self.current().is_none()
    }

    /// Writes everything down and closes the store, for good: see
    /// [`FoyerStore::close`].
    pub async fn close(&self) -> Result<(), StoreError> {
        self.suspend().await
    }
}

#[async_trait]
impl ContentStore for SuspendableStore {
    async fn get(&self, key: &str) -> Option<Bytes> {
        self.current()?.get(key).await
    }

    async fn put(&self, key: &str, value: Bytes, ttl: Option<Duration>) {
        if let Some(store) = self.current() {
            store.put(key, value, ttl).await;
        }
    }

    fn holds(&self, key: &str) -> bool {
        self.current().is_some_and(|store| store.holds(key))
    }
}

/// Milliseconds since the Unix epoch. A clock that reads before the epoch is
/// not worth branching on: treat it as the epoch, which expires everything and
/// degrades to a cold store rather than serving content forever.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// When an entry stops being servable, as milliseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Deadline(u64);

impl Deadline {
    /// The sentinel for "no deadline". Zero rather than a flag byte: an entry
    /// can never legitimately expire at the epoch.
    const NEVER: Self = Self(0);

    fn in_(ttl: Option<Duration>, now: u64) -> Self {
        match ttl {
            // Saturating: a TTL far enough out to overflow is indistinguishable
            // from no deadline, and must not wrap into an instant expiry.
            Some(d) => Self(now.saturating_add(d.as_millis().try_into().unwrap_or(u64::MAX))),
            None => Self::NEVER,
        }
    }

    fn has_passed(self, now: u64) -> bool {
        self != Self::NEVER && now >= self.0
    }
}

/// Entries carry their deadline in a fixed-width little-endian prefix.
///
/// foyer stores opaque bytes, so the expiry has to live in the value. A prefix
/// rather than a serde envelope keeps the payload contiguous — reading it costs
/// one slice, and the byte weigher still sees a length that matches what the
/// entry occupies.
struct Stamped;

impl Stamped {
    const PREFIX: usize = std::mem::size_of::<u64>();

    fn join(deadline: Deadline, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::PREFIX + payload.len());
        out.extend_from_slice(&deadline.0.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn split(stored: &[u8]) -> Option<(Deadline, &[u8])> {
        let (head, payload) = stored.split_at_checked(Self::PREFIX)?;
        let deadline = Deadline(u64::from_le_bytes(head.try_into().ok()?));
        Some((deadline, payload))
    }
}

/// A stable per-user cache directory (`$XDG_CACHE_HOME`, else `~/Library/Caches`
/// on macOS and iOS / `~/.cache` elsewhere, else the temp dir), suffixed
/// `tuile`.
///
/// iOS included, and it matters there more than anywhere: an application's
/// `$HOME` is its container, `Library/Caches` is the one directory in it the
/// system may purge when space runs short — and the only right place for a
/// cache of hundreds of megabytes — while `~/.cache` at the container's root
/// is neither purgeable nor a place an application is meant to write.
pub fn default_cache_dir() -> PathBuf {
    cache_dir_for(
        std::env::consts::OS,
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// [`default_cache_dir`], for the operating system `os` (as
/// [`std::env::consts::OS`] names it) and the given variables.
fn cache_dir_for(os: &str, xdg: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    let apple = matches!(os, "macos" | "ios");
    let base = xdg
        .or_else(|| {
            home.map(|home| {
                if apple {
                    home.join("Library/Caches")
                } else {
                    home.join(".cache")
                }
            })
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("tuile")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a suspended store held is on disk for whoever opens it next —
    /// the next launch — and the same handle serves again once resumed.
    /// While suspended it misses and declines, as a store may.
    #[test]
    fn a_suspended_store_has_written_everything_down() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        // Room for a few blocks: a store of one block cannot reclaim, and
        // waits for ever on close.
        let config = |dir: &Path| StoreConfig {
            disk_bytes: 64 << 20,
            ..config(dir)
        };
        runtime.block_on(async {
            let store = SuspendableStore::open(config(dir.path()))
                .await
                .expect("open");
            for i in 0..20 {
                store
                    .put(&format!("tile/{i}"), Bytes::from(vec![i as u8; 1000]), None)
                    .await;
            }
            store.suspend().await.expect("suspended");
            assert!(store.is_suspended());
            assert_eq!(
                store.get("tile/3").await,
                None,
                "a suspended store answered"
            );
            store.put("tile/late", Bytes::from_static(b"x"), None).await;

            // The next process, as it were: a store opened on the same files.
            let next = FoyerStore::with_config(config(dir.path()))
                .await
                .expect("reopened");
            let found = next.get("tile/3").await;
            next.close().await.expect("closed");
            assert_eq!(found.as_deref(), Some(&[3u8; 1000][..]), "lost on suspend");

            store.resume().await.expect("resumed");
            assert_eq!(store.get("tile/7").await.as_deref(), Some(&[7u8; 1000][..]));
            assert_eq!(
                store.get("tile/late").await,
                None,
                "a write while suspended"
            );
            store.close().await.expect("closed");
        });
    }

    /// A passing condition is told from damage: out of descriptors, busy or
    /// a full disk is transient; a file where the store's directory should be
    /// is not.
    #[test]
    fn a_store_error_says_whether_it_will_pass() {
        use std::io::{Error, ErrorKind};
        for passing in [
            Error::from_raw_os_error(24),
            Error::from_raw_os_error(23),
            Error::from(ErrorKind::ResourceBusy),
            Error::from(ErrorKind::WouldBlock),
            Error::from(ErrorKind::StorageFull),
            Error::from(ErrorKind::PermissionDenied),
        ] {
            assert!(StoreError::from(passing).is_transient());
        }
        assert!(!StoreError::from(Error::from(ErrorKind::InvalidData)).is_transient());

        let dir = tempfile::tempdir().expect("a temporary directory");
        let in_the_way = dir.path().join("store");
        std::fs::write(&in_the_way, b"not a directory").expect("a file");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let error = runtime
            .block_on(FoyerStore::with_config(config(&in_the_way)))
            .err()
            .expect("a file where the store goes");
        assert!(!error.is_transient(), "{error}");
    }

    /// A phone's container keeps its caches where a Mac does, under
    /// `Library/Caches` — the purgeable directory — and nowhere else does.
    #[test]
    fn the_cache_lives_in_library_caches_on_apple_systems() {
        let home = || Some(PathBuf::from("/home/x"));
        for os in ["macos", "ios"] {
            assert_eq!(
                cache_dir_for(os, None, home()),
                PathBuf::from("/home/x/Library/Caches/tuile"),
                "{os}"
            );
        }
        assert_eq!(
            cache_dir_for("linux", None, home()),
            PathBuf::from("/home/x/.cache/tuile")
        );
        assert_eq!(
            cache_dir_for("ios", Some("/xdg".into()), home()),
            PathBuf::from("/xdg/tuile"),
            "XDG_CACHE_HOME wins everywhere"
        );
    }

    fn config(dir: &Path) -> StoreConfig {
        StoreConfig {
            dir: dir.to_path_buf(),
            memory_bytes: 1 << 20,
            disk_bytes: 16 << 20,
            default_ttl: None,
        }
    }

    #[tokio::test]
    async fn a_stored_value_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FoyerStore::with_config(config(dir.path()))
            .await
            .expect("store");
        store
            .put("terrain/1/2/3", Bytes::from_static(b"payload"), None)
            .await;
        assert_eq!(
            store.get("terrain/1/2/3").await.as_deref(),
            Some(&b"payload"[..])
        );
    }

    #[tokio::test]
    async fn an_absent_key_is_a_miss_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FoyerStore::with_config(config(dir.path()))
            .await
            .expect("store");
        assert!(store.get("never/written").await.is_none());
    }

    #[tokio::test]
    async fn keys_do_not_collide() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FoyerStore::with_config(config(dir.path()))
            .await
            .expect("store");
        store.put("a", Bytes::from_static(b"first"), None).await;
        store.put("b", Bytes::from_static(b"second"), None).await;
        assert_eq!(store.get("a").await.as_deref(), Some(&b"first"[..]));
        assert_eq!(store.get("b").await.as_deref(), Some(&b"second"[..]));
    }

    #[tokio::test]
    async fn an_expired_entry_is_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FoyerStore::with_config(config(dir.path()))
            .await
            .expect("store");
        // Already stale when written: no sleeping in tests, and the boundary
        // is what matters.
        store
            .put("stale", Bytes::from_static(b"old"), Some(Duration::ZERO))
            .await;
        assert!(store.get("stale").await.is_none());
    }

    #[tokio::test]
    async fn a_live_entry_outlives_its_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FoyerStore::with_config(config(dir.path()))
            .await
            .expect("store");
        store
            .put(
                "fresh",
                Bytes::from_static(b"new"),
                Some(Duration::from_secs(3600)),
            )
            .await;
        assert_eq!(store.get("fresh").await.as_deref(), Some(&b"new"[..]));
    }

    #[tokio::test]
    async fn a_default_ttl_covers_origins_that_state_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FoyerStore::with_config(StoreConfig {
            default_ttl: Some(Duration::ZERO),
            ..config(dir.path())
        })
        .await
        .expect("store");
        // The origin said nothing, so the store's own policy applies.
        store.put("silent", Bytes::from_static(b"x"), None).await;
        assert!(store.get("silent").await.is_none());
    }

    #[test]
    fn a_deadline_never_wraps_into_an_instant_expiry() {
        let far = Deadline::in_(Some(Duration::from_secs(u64::MAX)), 1_000);
        assert!(!far.has_passed(u64::MAX - 1), "a huge ttl must not expire");
        assert_eq!(Deadline::in_(None, 1_000), Deadline::NEVER);
        assert!(!Deadline::NEVER.has_passed(u64::MAX));
    }

    #[test]
    fn the_stamp_round_trips_and_rejects_a_short_entry() {
        let stored = Stamped::join(Deadline(42), b"payload");
        let (deadline, payload) = Stamped::split(&stored).expect("split");
        assert_eq!((deadline, payload), (Deadline(42), &b"payload"[..]));
        assert!(Stamped::split(b"short").is_none(), "truncated entry");
    }

    /// The reason the disk tier exists: a second run must start warm. A store
    /// that only ever answered from its memory tier would pass every other test
    /// here and still lose everything on restart.
    #[tokio::test]
    async fn entries_survive_reopening_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A memory tier far smaller than the payload, so serving it back cannot
        // be RAM answering: it has to come off the disk.
        let cfg = StoreConfig {
            memory_bytes: 4 << 10,
            ..config(dir.path())
        };
        let payload = Bytes::from(vec![7u8; 512 << 10]);

        let first = FoyerStore::with_config(cfg.clone()).await.expect("store");
        first.put("persisted", payload.clone(), None).await;
        first.close().await.expect("flush");
        drop(first);

        let second = FoyerStore::with_config(cfg).await.expect("reopen");
        assert_eq!(
            second.get("persisted").await,
            Some(payload),
            "the disk tier did not survive a reopen"
        );
    }

    /// Why [`FoyerStore::close`] exists at all. A dropped store loses whatever
    /// had not reached the disk yet — which is the fate of any process killed
    /// rather than shut down, and was silently the fate of the viewer until it
    /// learned to flush on exit.
    ///
    /// The kill is modelled, not raced. Foyer's flusher is a task on the
    /// runtime that built the store, and it outlives the store's handles:
    /// once the last one is dropped it drains its queue and shuts down
    /// gracefully, so a store merely dropped inside a runtime that keeps
    /// running does reach the disk — whenever the flusher is next scheduled,
    /// sometimes before the reopen, sometimes not, which is how this test
    /// once failed on CI. Nothing awaits that drain, though: a process that
    /// is killed, or that returns from `main` and tears its runtime down,
    /// never gets it. So the first store lives on a runtime of its own, the
    /// put is the last thing that runtime polls (nothing awaits after it, so
    /// no task runs in between), and the runtime is then torn down with the
    /// flusher's work still queued. The same kill after `close()` keeps the entry: what is lost
    /// is lost for want of `close`, not to the way the process died.
    #[test]
    fn a_store_that_is_never_closed_loses_its_entries() {
        fn found_after_a_kill(closed: bool) -> Option<Bytes> {
            let dir = tempfile::tempdir().expect("tempdir");
            let cfg = StoreConfig {
                memory_bytes: 4 << 10,
                ..config(dir.path())
            };
            let payload = Bytes::from(vec![9u8; 512 << 10]);
            let runtime = || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime")
            };

            let dying = runtime();
            dying.block_on(async {
                let first = FoyerStore::with_config(cfg.clone()).await.expect("store");
                first.put("dropped", payload.clone(), None).await;
                if closed {
                    first.close().await.expect("flush");
                }
            });
            // The kill: every task still queued on the runtime, the flusher
            // among them, is dropped without being polled again.
            drop(dying);

            runtime().block_on(async {
                let second = FoyerStore::with_config(cfg).await.expect("reopen");
                let found = second.get("dropped").await;
                assert!(
                    found.is_none() || found.as_ref() == Some(&payload),
                    "a torn entry read back"
                );
                found
            })
        }

        assert!(
            found_after_a_kill(true).is_some(),
            "closed before the kill: the entry must be on the disk"
        );
        assert_eq!(
            found_after_a_kill(false),
            None,
            "if this now survives a kill, foyer writes as it goes and close() is optional"
        );
    }

    #[test]
    fn the_default_directory_is_namespaced() {
        assert!(default_cache_dir().ends_with("tuile"));
    }

    /// A film's worth of blocks — hundreds of entries of several hundred
    /// kilobytes, more in all than the memory tier — put as fast as they
    /// come, the store closed, and reopened: how many are still there.
    async fn blocks_found_after_reopening(keeping: bool, count: usize, each: usize) -> usize {
        let dir = tempfile::tempdir().expect("dir");
        let cfg = || StoreConfig {
            dir: dir.path().to_path_buf(),
            memory_bytes: 256 << 20,
            disk_bytes: 4 << 30,
            default_ttl: None,
        };
        let first = if keeping {
            FoyerStore::keeping(cfg()).await
        } else {
            FoyerStore::with_config(cfg()).await
        }
        .expect("store");
        for i in 0..count {
            first
                .put(
                    &format!("block/{i}"),
                    Bytes::from(vec![i as u8; each]),
                    None,
                )
                .await;
            // As a reader's blocks arrive: one at a time, the runtime free
            // in between to write what it was handed.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        first.close().await.expect("close");
        drop(first);
        let second = FoyerStore::with_config(cfg()).await.expect("store");
        let mut found = 0;
        for i in 0..count {
            if second
                .get(&format!("block/{i}"))
                .await
                .is_some_and(|b| b.len() == each && b[0] == i as u8)
            {
                found += 1;
            }
        }
        found
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_keeping_store_has_every_block_after_it_is_reopened() {
        assert_eq!(
            blocks_found_after_reopening(true, 530, 700 << 10).await,
            530
        );
    }

    /// A warm relaunch over a viewer's store: a session's tiles — 1 537, the
    /// count a warm relaunch measured fetching again — put as they arrive,
    /// all of them still in the memory tier when the store is flushed on the
    /// way out, then the store opened again by the next launch. Every tile is
    /// there: what the index answers absent the next launch fetches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flushed_viewer_store_has_every_tile_on_the_next_launch() {
        const TILES: usize = 1_537;
        const EACH: usize = 64 << 10;
        let dir = tempfile::tempdir().expect("dir");
        let cfg = || StoreConfig {
            dir: dir.path().to_path_buf(),
            memory_bytes: 256 << 20,
            disk_bytes: 4 << 30,
            default_ttl: None,
        };
        let first = SuspendableStore::open(cfg()).await.expect("store");
        for i in 0..TILES {
            first
                .put(&format!("tile/{i}"), Bytes::from(vec![i as u8; EACH]), None)
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        first.close().await.expect("flushed");
        drop(first);
        let next = SuspendableStore::open(cfg()).await.expect("reopened");
        let mut held = 0;
        let mut found = 0;
        for i in 0..TILES {
            let key = format!("tile/{i}");
            held += usize::from(next.holds(&key));
            if next
                .get(&key)
                .await
                .is_some_and(|b| b.len() == EACH && b[0] == i as u8)
            {
                found += 1;
            }
        }
        next.close().await.expect("closed");
        assert_eq!((held, found), (TILES, TILES), "(indexed, read back)");
    }

    /// Measured, not asserted: what the default store keeps of the same.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn what_a_store_that_writes_on_eviction_keeps_of_the_same() {
        let found = blocks_found_after_reopening(false, 530, 700 << 10).await;
        println!("write on eviction: {found} of 530 found after reopening");
    }
}
