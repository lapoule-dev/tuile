// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a render reads: a film's packs and the tile store.
//!
//! Both are `Objects`, and both are read through a cache of chunks on disk
//! (`tuile_repository::Cached`), so a second render of the same film asks
//! the buckets for nothing but what changes; the chunks last read stay in
//! memory, so a tile is not a chunk read from disk. A counter sits on each
//! side of the cache: what the render asked for, and what went out for it.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tuile_core::raster::TilingScheme;
use tuile_film::Pack;
use tuile_radiometry::FilmGrade;
use tuile_repository::tone::{film_grade, layer_is_graded};
use tuile_repository::{
    ArchivedTiles, Cached, DiskChunks, Entry, Listing, Objects, Read, RepoError, Revalidated,
    Revalidations, TileRepository, CHUNK,
};

use tuile_storage_foyer::{FoyerStore, StoreConfig};

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
    async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
        let read = self.inner.read_if_changed(key, known).await?;
        // A question is a read; only an object that came counts its bytes.
        self.count(match &read {
            Read::Changed { bytes, .. } => bytes.len(),
            Read::Unchanged => 0,
        });
        Ok(read)
    }
}

/// Bytes of chunks kept in memory before the oldest are let go. A film's
/// tiles come from a few hundred chunks, read over and over; holding fewer
/// than it uses means reading each from disk, whole, again and again.
const HELD_BYTES: usize = 1 << 30;

/// `Objects` with the chunks last read kept in memory, and ranges cut out
/// of them. A tile is a few kilobytes of a chunk of megabytes: asked of the
/// disk cache each time, every tile costs a chunk read and copied whole,
/// which is most of what reading from a warm cache would otherwise cost.
struct Held {
    behind: Arc<dyn Objects>,
    chunks: std::sync::Mutex<std::collections::VecDeque<((String, u64), Arc<Vec<u8>>)>>,
    /// Sizes, believed for as long as this lives: one render.
    sizes: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl Held {
    async fn chunk(&self, key: &str, size: u64, index: u64) -> Result<Arc<Vec<u8>>, RepoError> {
        let held = self.chunks.lock().ok().and_then(|chunks| {
            chunks
                .iter()
                .find(|(at, _)| at.0 == key && at.1 == index)
                .map(|(_, bytes)| bytes.clone())
        });
        if let Some(bytes) = held {
            return Ok(bytes);
        }
        let range = index * CHUNK..((index + 1) * CHUNK).min(size);
        let bytes = Arc::new(self.behind.read(key, range).await?);
        if let Ok(mut chunks) = self.chunks.lock() {
            let mut held: usize = chunks.iter().map(|c| c.1.len()).sum();
            while held + bytes.len() > HELD_BYTES {
                match chunks.pop_front() {
                    Some(gone) => held -= gone.1.len(),
                    None => break,
                }
            }
            chunks.push_back(((key.to_string(), index), bytes.clone()));
        }
        Ok(bytes)
    }
}

#[async_trait]
impl Objects for Held {
    fn label(&self) -> String {
        self.behind.label()
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        self.behind.list(prefix).await
    }
    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        self.behind.browse(prefix).await
    }
    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        if let Some(size) = self.sizes.lock().ok().and_then(|s| s.get(key).copied()) {
            return Ok(size);
        }
        let size = self.behind.size(key).await?;
        if let Ok(mut sizes) = self.sizes.lock() {
            sizes.insert(key.to_string(), size);
        }
        Ok(size)
    }
    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let size = self.size(key).await?;
        if range.end > size || range.start > range.end {
            return Err(RepoError::Store(format!(
                "{key}: bytes {}..{} are outside its {size}",
                range.start, range.end
            )));
        }
        let mut out = Vec::with_capacity((range.end - range.start) as usize);
        for index in range.start / CHUNK..range.end.div_ceil(CHUNK) {
            let chunk = self.chunk(key, size, index).await?;
            let base = index * CHUNK;
            let from = range.start.max(base) - base;
            let to = (range.end.min(base + chunk.len() as u64)) - base;
            out.extend_from_slice(&chunk[from as usize..to as usize]);
        }
        Ok(out)
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
        let asked = Counting::new(Arc::new(Held {
            behind: cached,
            chunks: std::sync::Mutex::default(),
            sizes: std::sync::Mutex::default(),
        }));
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

    /// Whether an imagery layer is graded at all: its store says.
    pub async fn is_graded(&self, layer: &str) -> Result<bool, Error> {
        Ok(layer_is_graded(self.live.as_ref(), layer).await?)
    }
}

/// Everything a render reads.
pub struct Sources {
    /// The packs' bucket as it is, for what is written again under its
    /// key: a film's grade.
    runs: Arc<dyn Objects>,
    pub packs: Cache,
    pub archives: Cache,
    pub store: Store,
    /// The store's catalog, manifests and tables as the bucket was asked
    /// for them: every question, and the bytes of what came.
    pub live: Arc<Counting>,
    /// The same, as the render was answered: kept between two renders with
    /// the bucket's validators, and asked for again only conditionally.
    revalidated: Arc<Revalidated>,
    keeper: FoyerStore,
}

/// What is kept of the store's small objects between two renders: they are
/// kilobytes each, a few hundred of them a film.
const KEPT_IN_MEMORY: usize = 32 << 20;
const KEPT_ON_DISK: usize = 256 << 20;

impl Sources {
    /// `runs` holds the packs, `tiles` the tile store; chunks of both are
    /// kept under `cache`, and so are the store's catalog, manifests and
    /// tables — those with their validators, since they change: each is
    /// asked for again, and comes again only if it was written again.
    pub async fn open(
        runs: Arc<dyn Objects>,
        tiles: Arc<dyn Objects>,
        cache: &std::path::Path,
    ) -> Result<Self, Error> {
        let packs = Cache::over(runs.clone(), cache.join("packs"));
        let archives = Cache::over(tiles.clone(), cache.join("tiles"));
        let live = Counting::new(tiles);
        let keeper = FoyerStore::with_config(StoreConfig {
            dir: cache.join("live"),
            memory_bytes: KEPT_IN_MEMORY,
            disk_bytes: KEPT_ON_DISK,
            // The validator says when an entry is stale, not a clock.
            default_ttl: None,
        })
        .await?;
        let revalidated = Arc::new(Revalidated::new(live.clone(), Arc::new(keeper.clone())));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let store = ArchivedTiles::open(
            revalidated.clone(),
            archives.objects.clone(),
            Arc::new(move || now),
        )
        .await?;
        Ok(Self {
            runs,
            packs,
            archives,
            store: Store {
                tiles: store,
                live: revalidated.clone(),
            },
            live,
            revalidated,
            keeper,
        })
    }

    /// The grade of a film, from what is kept beside its packs; `None` for
    /// a film that has none.
    pub async fn film_grade(&self, film: &Film) -> Result<Option<FilmGrade>, Error> {
        let keys: Vec<&str> = film.packs.iter().map(|p| p.key.as_str()).collect();
        Ok(film_grade(self.runs.as_ref(), &keys).await?)
    }

    /// What became of the store's small objects: asked about and unchanged,
    /// or come whole.
    pub fn revalidations(&self) -> Revalidations {
        self.revalidated.so_far()
    }

    /// Flushes what was kept to disk. Without it the next render finds
    /// nothing: call it on the way out.
    pub async fn close(&self) -> Result<(), Error> {
        Ok(self.keeper.close().await?)
    }
}
