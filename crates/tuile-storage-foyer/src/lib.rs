// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-storage-foyer
//!
//! A native [`ContentStore`]: a hybrid memory + disk cache backed by
//! [foyer](https://docs.rs/foyer), so content a session evicted comes back from
//! RAM or from the local disk instead of being fetched and decoded again.
//!
//! This crate exists **because the core may not**. `tuile-core` states the
//! [`ContentStore`] contract and nothing more — it compiles to
//! `wasm32-unknown-unknown`, where foyer (tokio, threads, a filesystem) cannot
//! follow. A browser host implements the same trait over IndexedDB or the Cache
//! API, or passes no store at all; nothing above the trait changes.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use tuile_core::storage::ContentStore;
//! use tuile_storage_foyer::FoyerStore;
//!
//! let store: Arc<dyn ContentStore> = Arc::new(FoyerStore::shared("tiles").await?);
//! let ttl = Some(std::time::Duration::from_secs(3600));
//! store.put("terrain/12/34/56", bytes::Bytes::from_static(b"..."), ttl).await;
//! # Ok(())
//! # }
//! ```
//!
//! Must run inside a tokio runtime: foyer is tokio-native.

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
#[error("content store: {0}")]
pub struct StoreError(String);

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
            .map_err(|e| StoreError(e.to_string()))
    }

    /// Builds a store with an explicit configuration.
    pub async fn with_config(cfg: StoreConfig) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&cfg.dir).map_err(|e| StoreError(e.to_string()))?;
        let device = FsDeviceBuilder::new(&cfg.dir)
            .with_capacity(cfg.disk_bytes)
            .build()
            .map_err(|e| StoreError(e.to_string()))?;
        let cache = HybridCacheBuilder::new()
            .memory(cfg.memory_bytes)
            // Capacity is in bytes: weigh each entry by its payload length,
            // or a few large records would evict everything else.
            .with_weighter(|_k: &String, v: &Vec<u8>| v.len())
            .storage()
            .with_io_engine_config(PsyncIoEngineConfig::new())
            .with_engine_config(BlockEngineConfig::new(device))
            .build()
            .await
            .map_err(|e| StoreError(e.to_string()))?;
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
/// on macOS / `~/.cache` elsewhere, else the temp dir), suffixed `tuile`.
pub fn default_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let home = PathBuf::from(home);
                if cfg!(target_os = "macos") {
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
    #[tokio::test]
    async fn a_store_that_is_never_closed_loses_its_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StoreConfig {
            memory_bytes: 4 << 10,
            ..config(dir.path())
        };
        let payload = Bytes::from(vec![9u8; 512 << 10]);

        let first = FoyerStore::with_config(cfg.clone()).await.expect("store");
        first.put("dropped", payload.clone(), None).await;
        drop(first); // no close: exactly what a killed process leaves behind

        let second = FoyerStore::with_config(cfg).await.expect("reopen");
        assert_eq!(
            second.get("dropped").await,
            None,
            "if this now survives, foyer flushes on drop and close() is optional"
        );
    }

    #[test]
    fn the_default_directory_is_namespaced() {
        assert!(default_cache_dir().ends_with("tuile"));
    }
}
