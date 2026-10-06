// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a render reads: a film's packs and the tile store.
//!
//! Both are `Objects`, and both are read through a cache of chunks on disk
//! (`tuile_repository::Cached`), so a second render of the same film asks
//! the buckets for nothing but what changes. A counter sits on each side of
//! the cache: what the render asked for, and what went out for it.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tuile_core::raster::TilingScheme;
use tuile_film::Pack;
use tuile_radiometry::LevelGains;
use tuile_repository::{
    ArchivedTiles, Cached, ChunkStore, DiskChunks, Entry, Listing, Objects, RepoError,
    TileRepository,
};

use crate::Error;

/// Reads made and bytes read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reads {
    pub reads: u64,
    pub bytes: u64,
}

/// `Objects` that counts what is read through it.
pub struct Counting {
    inner: Arc<dyn Objects>,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    pub fn new(inner: Arc<dyn Objects>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    pub fn so_far(&self) -> Reads {
        Reads {
            reads: self.reads.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn count(&self, bytes: usize) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

#[async_trait]
impl Objects for Counting {
    fn label(&self) -> String {
        self.inner.label()
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        self.inner.list(prefix).await
    }
    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        self.inner.browse(prefix).await
    }
    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        self.inner.size(key).await
    }
    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let bytes = self.inner.read(key, range).await?;
        self.count(bytes.len());
        Ok(bytes)
    }
    async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        let bytes = self.inner.read_all(key).await?;
        self.count(bytes.len());
        Ok(bytes)
    }
}

/// Chunks kept in memory before they are let go: a frame's tiles come from
/// a handful of archives, read over and over.
const WARM: usize = 24;

/// A chunk store with its latest chunks kept in memory in front of it: a
/// tile is a few kilobytes of a chunk of megabytes, and reading the whole
/// chunk from disk for each one is most of a render's reading.
struct Warm<S> {
    behind: S,
    held: std::sync::Mutex<std::collections::VecDeque<((String, u64), Arc<Vec<u8>>)>>,
}

impl<S> Warm<S> {
    fn keep(&self, key: &str, index: u64, bytes: Arc<Vec<u8>>) {
        if let Ok(mut held) = self.held.lock() {
            if held.len() >= WARM {
                held.pop_front();
            }
            held.push_back(((key.to_string(), index), bytes));
        }
    }
}

#[async_trait]
impl<S: ChunkStore> ChunkStore for Warm<S> {
    async fn get(&self, key: &str, index: u64) -> Option<Vec<u8>> {
        let warm = self.held.lock().ok().and_then(|held| {
            held.iter()
                .find(|(at, _)| at.0 == key && at.1 == index)
                .map(|(_, bytes)| bytes.clone())
        });
        if let Some(bytes) = warm {
            return Some(bytes.as_ref().clone());
        }
        let bytes = self.behind.get(key, index).await?;
        self.keep(key, index, Arc::new(bytes.clone()));
        Some(bytes)
    }
    async fn put(&self, key: &str, index: u64, bytes: &[u8]) {
        self.keep(key, index, Arc::new(bytes.to_vec()));
        self.behind.put(key, index, bytes).await;
    }
    async fn size(&self, key: &str) -> Option<u64> {
        self.behind.size(key).await
    }
    async fn note_size(&self, key: &str, size: u64) {
        self.behind.note_size(key, size).await;
    }
}

/// One side of what a render reads, counted before and after its cache.
pub struct Cache {
    /// What the render reads from.
    pub objects: Arc<dyn Objects>,
    asked: Arc<Counting>,
    fetched: Arc<Counting>,
}

impl Cache {
    /// `origin`, behind a cache of chunks kept under `dir`.
    pub fn over(origin: Arc<dyn Objects>, dir: impl Into<std::path::PathBuf>) -> Self {
        let fetched = Counting::new(origin);
        let cached: Arc<dyn Objects> = Arc::new(Cached::new(fetched.clone(), DiskChunks::new(dir)));
        let asked = Counting::new(cached);
        Self {
            objects: asked.clone(),
            asked,
            fetched,
        }
    }

    /// What was asked of the cache, and what the cache had to go and get.
    pub fn reads(&self) -> (Reads, Reads) {
        (self.asked.so_far(), self.fetched.so_far())
    }
}

/// One pack of a film: its key, its size, and its table.
pub struct PackFile {
    pub key: String,
    pub size: u64,
    /// The file up to where the payloads begin.
    pub head: Vec<u8>,
    pub first: u32,
    pub last: u32,
}

/// A film: the packs under one prefix, in the order of their frames.
pub struct Film {
    pub packs: Vec<PackFile>,
}

impl Film {
    /// Reads the table of every pack under `prefix` — a run's `packs/`, or
    /// the run itself.
    pub async fn open(objects: &dyn Objects, prefix: &str) -> Result<Self, Error> {
        let prefix = prefix.trim_end_matches('/');
        let mut listed = objects.list(&format!("{prefix}/")).await?;
        listed.retain(|e| e.key.ends_with(".tuilepack"));
        if listed.is_empty() {
            return Err(format!("no pack under {prefix}/").into());
        }
        let mut packs = Vec::new();
        for entry in listed {
            // Magic, the table's length, then the table.
            let lead = objects.read(&entry.key, 0..16.min(entry.size)).await?;
            let table = lead
                .get(8..16)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
                .ok_or_else(|| format!("{} is too short to be a pack", entry.key))?;
            let head = objects
                .read(&entry.key, 0..(16 + table).min(entry.size))
                .await?;
            let (first, last) = Pack::open_table(&head)?.frame_range();
            packs.push(PackFile {
                key: entry.key,
                size: entry.size,
                head,
                first,
                last,
            });
        }
        packs.sort_by_key(|p| p.first);
        Ok(Self { packs })
    }

    pub fn frames(&self) -> (u32, u32) {
        (
            self.packs.first().map_or(0, |p| p.first),
            self.packs.last().map_or(0, |p| p.last),
        )
    }
}

/// The tile store as a render reads it.
pub struct Store {
    pub tiles: ArchivedTiles,
    live: Arc<dyn Objects>,
}

impl Store {
    /// The grid an imagery layer is cut on.
    pub fn scheme_of(&self, layer: &str) -> TilingScheme {
        match self.tiles.layers().iter().find(|l| l.name == layer) {
            Some(l) if l.grid == "geographic" => TilingScheme::geographic(),
            _ => TilingScheme::web_mercator(),
        }
    }

    /// The tone correction the store holds for an imagery layer, if any.
    /// A failure to ask is a failure, not an absence.
    pub async fn tone_of(&self, layer: &str) -> Result<Option<LevelGains>, Error> {
        let key = format!("{layer}/tone.json");
        match self.live.read_all(&key).await {
            Ok(bytes) => Ok(Some(
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(LevelGains::from_json)
                    .ok_or_else(|| format!("{key} is not a tone table"))?,
            )),
            Err(RepoError::NotFound(_)) => Ok(None),
            Err(other) => Err(other.into()),
        }
    }
}

/// Everything a render reads.
pub struct Sources {
    pub packs: Cache,
    pub archives: Cache,
    pub store: Store,
}

impl Sources {
    /// `runs` holds the packs, `tiles` the tile store; chunks of both are
    /// kept under `cache`. The store's catalog and manifests change, and are
    /// read from `tiles` each time.
    pub async fn open(
        runs: Arc<dyn Objects>,
        tiles: Arc<dyn Objects>,
        cache: &std::path::Path,
    ) -> Result<Self, Error> {
        let packs = Cache::over(runs, cache.join("packs"));
        let archives = Cache::over(tiles.clone(), cache.join("tiles"));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let store = ArchivedTiles::open(
            tiles.clone(),
            archives.objects.clone(),
            Arc::new(move || now),
        )
        .await?;
        Ok(Self {
            packs,
            archives,
            store: Store {
                tiles: store,
                live: tiles,
            },
        })
    }
}
