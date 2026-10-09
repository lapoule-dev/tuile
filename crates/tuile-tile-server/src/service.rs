// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The service logic, written once, with no HTTP framework in sight.
//!
//! A server — axum, a Worker, anything — is a thin adapter over
//! [`TileService::tile`]: it owns its routes, its headers and its
//! authentication, and asks this for bytes. A tile is served from the store
//! when present; otherwise it is fetched from the layer's upstream **once**,
//! however many requests ask for it at the same moment, stored, and served.
//!
//! In front of the store, the tiles served last are kept in memory, with their
//! validator already computed: a map a thousand people look at asks for the
//! same few hundred tiles, and those must not cost a lookup each. A source's
//! "no such tile" is remembered too — in memory and in the layer's absence
//! sibling in the store — so a hole over the sea is asked of the source once,
//! not on every request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::Serialize;

use crate::catalog::{ABSENT_MARKER, ABSENT_SUFFIX};
use crate::grid::Grid;
use crate::lru::Lru;
use crate::peers::{Claim, Emitted, FreshTiles, SharedTile, SharedTiles};
use crate::store::TileStore;
use crate::upstream::Upstream;
use crate::StoreError;

const MIB: u64 = 1 << 20;

/// Default for [`ServiceConfig::hot_bytes`].
pub const DEFAULT_HOT_BYTES: u64 = 256 * MIB;
/// Default for [`ServiceConfig::hot_ttl`]: a tile changes only when its
/// source is asked again, which a store hit never does; this only bounds how
/// long another instance's newer copy stays unseen.
pub const DEFAULT_HOT_TTL: Duration = Duration::from_secs(10 * 60);
/// Default for [`ServiceConfig::shared_wait`]: longer than a source takes to
/// answer, far shorter than a client waits.
pub const DEFAULT_SHARED_WAIT: Duration = Duration::from_secs(5);
/// Default for [`ServiceConfig::shared_poll`].
pub const DEFAULT_SHARED_POLL: Duration = Duration::from_millis(50);
/// What an absence weighs in the hot budget: its key and bookkeeping.
const ABSENCE_WEIGHT: u64 = 64;

/// A failure while serving a tile.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ServiceError {
    #[error("unknown layer {0}")]
    UnknownLayer(String),
    #[error("{0}")]
    OutOfGrid(String),
    #[error("store: {0}")]
    Store(String),
    #[error("upstream: {0}")]
    Upstream(String),
}

impl From<StoreError> for ServiceError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::UnknownLayer(l) => ServiceError::UnknownLayer(l),
            StoreError::OutOfGrid(g) => ServiceError::OutOfGrid(g.to_string()),
            other => ServiceError::Store(other.to_string()),
        }
    }
}

/// Where a served tile came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// The hot tiles kept in memory.
    Memory,
    /// The store: its buffer, a local copy, or the bucket.
    Store,
    /// Fetched from the layer's source just now.
    Upstream,
    /// Fetched by another instance moments ago, and not yet in the store:
    /// read from what the instances share ([`crate::peers::SharedTiles`]).
    Shared,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Memory => "memory",
            Source::Store => "store",
            Source::Upstream => "upstream",
            Source::Shared => "shared",
        }
    }
}

/// A tile, ready for an adapter to answer with.
#[derive(Debug, Clone)]
pub struct TileResponse {
    pub bytes: Bytes,
    pub content_type: String,
    /// A strong validator derived from the bytes: the same tile always has
    /// the same one, on every instance and across restarts.
    pub etag: String,
    /// Served without asking the source (memory or store).
    pub hit: bool,
    pub source: Source,
}

impl TileResponse {
    /// Whether an `If-None-Match` header value names this tile.
    pub fn matches(&self, if_none_match: &str) -> bool {
        if_none_match.split(',').any(|t| {
            let t = t.trim();
            t == "*" || t == self.etag || t.strip_prefix("W/") == Some(self.etag.as_str())
        })
    }
}

/// What a client needs to address a layer.
#[derive(Debug, Clone, Serialize)]
pub struct LayerMeta {
    pub name: String,
    pub grid: Grid,
    pub max_level: u8,
    pub content_type: String,
}

/// How much the service keeps in memory.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Bytes of hot tiles kept; `0` keeps none.
    pub hot_bytes: u64,
    /// How long a hot tile, or a remembered absence, is served without
    /// asking the store again.
    pub hot_ttl: Duration,
    /// With tiles shared between instances: how long this one waits for a
    /// tile another is fetching before it fetches it itself…
    pub shared_wait: Duration,
    /// …and how often it looks for it meanwhile.
    pub shared_poll: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            hot_bytes: DEFAULT_HOT_BYTES,
            hot_ttl: DEFAULT_HOT_TTL,
            shared_wait: DEFAULT_SHARED_WAIT,
            shared_poll: DEFAULT_SHARED_POLL,
        }
    }
}

/// Where served tiles came from, since the service started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ServiceStats {
    pub memory: u64,
    pub store: u64,
    pub upstream: u64,
    /// Answered "no such tile" without asking the source.
    pub absent: u64,
    /// Answered with what another instance had just fetched.
    pub shared: u64,
    /// Tiles taken from another instance ahead of any request.
    pub learned: u64,
}

#[derive(Default)]
struct Counters {
    memory: AtomicU64,
    store: AtomicU64,
    upstream: AtomicU64,
    absent: AtomicU64,
    shared: AtomicU64,
    learned: AtomicU64,
}

type Key = (String, u8, u32, u32);
/// What a flight brings back: the tile (or the source's "no such tile"), and
/// whether it was this instance's own fetch or another's, shared.
type Fetched = (Option<Bytes>, Source);
type Flight = Shared<BoxFuture<'static, Result<Fetched, ServiceError>>>;

/// A hot entry: a tile with its validator, or a remembered absence.
#[derive(Clone)]
struct Hot {
    tile: Option<(Bytes, String)>,
    at: Instant,
}

/// The store plus an upstream per layer.
pub struct TileService {
    store: Arc<TileStore>,
    upstreams: HashMap<String, Arc<dyn Upstream>>,
    inflight: Arc<Mutex<HashMap<Key, Flight>>>,
    cfg: ServiceConfig,
    hot: Mutex<Lru<Key, Hot>>,
    counters: Counters,
    /// What this instance shares with the others, if anything.
    shared: Option<Arc<dyn SharedTiles>>,
    sink: Option<Arc<dyn FreshTiles>>,
}

impl TileService {
    pub fn new(store: Arc<TileStore>, upstreams: HashMap<String, Arc<dyn Upstream>>) -> Self {
        Self::with_config(store, upstreams, ServiceConfig::default())
    }

    pub fn with_config(store: Arc<TileStore>, upstreams: HashMap<String, Arc<dyn Upstream>>, cfg: ServiceConfig) -> Self {
        Self {
            store,
            upstreams,
            inflight: Arc::default(),
            hot: Mutex::new(Lru::new(cfg.hot_bytes)),
            cfg,
            counters: Counters::default(),
            shared: None,
            sink: None,
        }
    }

    /// From now on, a tile this service is about to fetch is first looked
    /// for among those other instances have just fetched, and fetched by one
    /// instance only: see [`crate::peers`].
    pub fn with_shared(mut self, shared: Arc<dyn SharedTiles>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// From now on, a tile this service has just fetched is emitted to
    /// `sink` and **not** written to the store by this service: whoever is
    /// behind the sink writes it. See [`FreshTiles`].
    pub fn with_sink(mut self, sink: Arc<dyn FreshTiles>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Takes a tile another instance has fetched, as if this one had: it is
    /// served from memory from now on, for as long as a hot tile is. Nothing
    /// is written, and nothing is asked of a source.
    pub fn learn(&self, layer: &str, level: u8, x: u32, y: u32, tile: SharedTile) {
        let key = (layer.to_string(), level, x, y);
        self.hot_put(key, tile.bytes.map(|bytes| {
            let etag = etag_of(&bytes);
            (bytes, etag)
        }));
        self.counters.learned.fetch_add(1, Ordering::Relaxed);
    }

    pub fn store(&self) -> &Arc<TileStore> {
        &self.store
    }

    pub fn stats(&self) -> ServiceStats {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ServiceStats {
            memory: g(&self.counters.memory),
            store: g(&self.counters.store),
            upstream: g(&self.counters.upstream),
            absent: g(&self.counters.absent),
            shared: g(&self.counters.shared),
            learned: g(&self.counters.learned),
        }
    }

    pub fn meta(&self, layer: &str) -> Result<LayerMeta, ServiceError> {
        let l = self.store.layer(layer)?;
        Ok(LayerMeta {
            name: l.name.clone(),
            grid: l.grid,
            max_level: l.grid.max_level(),
            content_type: l.content_type.clone(),
        })
    }

    /// A tile, or `None` when neither the store nor the source has it.
    pub async fn tile(&self, layer: &str, level: u8, x: u32, y: u32) -> Result<Option<TileResponse>, ServiceError> {
        let l = self.store.layer(layer)?;
        let content_type = l.content_type.clone();
        l.grid.check(level, x, y).map_err(|e| ServiceError::OutOfGrid(e.to_string()))?;
        let key: Key = (layer.to_string(), level, x, y);

        if let Some(hot) = self.hot_get(&key) {
            return Ok(match hot {
                Some((bytes, etag)) => {
                    self.counters.memory.fetch_add(1, Ordering::Relaxed);
                    Some(TileResponse { bytes, content_type, etag, hit: true, source: Source::Memory })
                }
                None => {
                    self.counters.absent.fetch_add(1, Ordering::Relaxed);
                    None
                }
            });
        }

        // The tile and its absence are looked up together: on a miss, the
        // second lookup costs no extra wait.
        let absence = format!("{layer}{ABSENT_SUFFIX}");
        let (stored, absent) = if self.store.layer(&absence).is_ok() {
            let (t, a) = futures_util::join!(self.store.get(layer, level, x, y), self.store.get(&absence, level, x, y));
            // A failed absence lookup is only a lost shortcut.
            (t?, a.ok().flatten().is_some())
        } else {
            (self.store.get(layer, level, x, y).await?, false)
        };
        if let Some(bytes) = stored {
            self.counters.store.fetch_add(1, Ordering::Relaxed);
            let etag = etag_of(&bytes);
            self.hot_put(key, Some((bytes.clone(), etag.clone())));
            return Ok(Some(TileResponse { bytes, content_type, etag, hit: true, source: Source::Store }));
        }
        if absent {
            self.counters.absent.fetch_add(1, Ordering::Relaxed);
            self.hot_put(key, None);
            return Ok(None);
        }

        let (fetched, source) = self.fetch_once(layer, level, x, y).await?;
        // A tile another instance fetched is one the source was not asked
        // for: a hit, from here.
        let hit = source == Source::Shared;
        let counter = if hit { &self.counters.shared } else { &self.counters.upstream };
        counter.fetch_add(1, Ordering::Relaxed);
        match fetched {
            Some(bytes) => {
                let etag = etag_of(&bytes);
                self.hot_put(key, Some((bytes.clone(), etag.clone())));
                Ok(Some(TileResponse { bytes, content_type, etag, hit, source }))
            }
            None => {
                self.hot_put(key, None);
                Ok(None)
            }
        }
    }

    /// `Some(entry)` when a fresh hot entry exists; the entry itself is
    /// `None` for a remembered absence.
    #[allow(clippy::option_option)]
    fn hot_get(&self, key: &Key) -> Option<Option<(Bytes, String)>> {
        if self.cfg.hot_bytes == 0 {
            return None;
        }
        let mut hot = self.hot.lock().ok()?;
        let entry = hot.get(key)?;
        if entry.at.elapsed() >= self.cfg.hot_ttl {
            hot.remove(key);
            return None;
        }
        Some(entry.tile)
    }

    fn hot_put(&self, key: Key, tile: Option<(Bytes, String)>) {
        if self.cfg.hot_bytes == 0 {
            return;
        }
        let weight = tile.as_ref().map_or(ABSENCE_WEIGHT, |(b, _)| b.len() as u64 + ABSENCE_WEIGHT);
        if let Ok(mut hot) = self.hot.lock() {
            hot.insert(key, Hot { tile, at: Instant::now() }, weight);
        }
    }

    fn fetch_once(&self, layer: &str, level: u8, x: u32, y: u32) -> Flight {
        let key = (layer.to_string(), level, x, y);
        let mut inflight = match self.inflight.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(f) = inflight.get(&key) {
            return f.clone();
        }
        let upstream = self.upstreams.get(layer).cloned();
        let store = self.store.clone();
        let table = self.inflight.clone();
        let shared = self.shared.clone();
        let sink = self.sink.clone();
        let (wait, poll) = (self.cfg.shared_wait, self.cfg.shared_poll);
        let k = key.clone();
        let flight = async move {
            let result = async {
                let upstream = upstream.ok_or_else(|| ServiceError::UnknownLayer(k.0.clone()))?;
                let Some(shared) = shared else {
                    return fetch(&upstream, &store, sink.as_ref(), None, &k).await;
                };
                let seen = |outcome: &'static str| {
                    metrics::counter!("tuile_tiles_shared_total", "layer" => k.0.clone(), "outcome" => outcome).increment(1);
                };
                // Another instance may have fetched it moments ago.
                if let Some(tile) = shared.get(&k.0, k.1, k.2, k.3).await {
                    seen("hit");
                    return Ok((tile.bytes, Source::Shared));
                }
                match shared.claim(&k.0, k.1, k.2, k.3).await {
                    Claim::Mine(token) => {
                        // Between the look and the claim, the one that held
                        // the claim may have shared the tile and let go.
                        let got = match shared.get(&k.0, k.1, k.2, k.3).await {
                            Some(tile) => {
                                seen("hit");
                                Ok((tile.bytes, Source::Shared))
                            }
                            None => {
                                seen("claimed");
                                fetch(&upstream, &store, sink.as_ref(), Some(&shared), &k).await
                            }
                        };
                        shared.release(&k.0, k.1, k.2, k.3, &token).await;
                        got
                    }
                    Claim::Theirs => {
                        // Wait for the other's tile — never for ever: it may
                        // have died, or its source may be slower than a
                        // client should wait behind.
                        let until = tokio::time::Instant::now() + wait;
                        while tokio::time::Instant::now() < until {
                            tokio::time::sleep(poll).await;
                            if let Some(tile) = shared.get(&k.0, k.1, k.2, k.3).await {
                                seen("waited");
                                return Ok((tile.bytes, Source::Shared));
                            }
                        }
                        seen("gave_up");
                        fetch(&upstream, &store, sink.as_ref(), Some(&shared), &k).await
                    }
                    Claim::Unknown => {
                        seen("miss");
                        fetch(&upstream, &store, sink.as_ref(), Some(&shared), &k).await
                    }
                }
            }
            .await;
            if let Ok(mut t) = table.lock() {
                t.remove(&k);
            }
            result
        }
        .boxed()
        .shared();
        inflight.insert(key, flight.clone());
        flight
    }
}

/// Asks the source for a tile and hands it on: to the sink when there is
/// one — and then that is all — or else to the other instances, if there is
/// something to share it through, and to the store.
async fn fetch(
    upstream: &Arc<dyn Upstream>,
    store: &Arc<TileStore>,
    sink: Option<&Arc<dyn FreshTiles>>,
    shared: Option<&Arc<dyn SharedTiles>>,
    k: &Key,
) -> Result<Fetched, ServiceError> {
    metrics::counter!("tuile_tiles_upstream_total", "layer" => k.0.clone()).increment(1);
    let got = upstream.fetch(k.1, k.2, k.3).await.map_err(|e| ServiceError::Upstream(e.0))?;
    let fetched = store.now();
    let fetched_ms = fetched.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    if let Some(sink) = sink {
        // Emitted, and no more: the tile is the sink's to share and to store.
        // Not kept is not an error — the tile was fetched and is served.
        let emitted = sink.emit(&k.0, k.1, k.2, k.3, &SharedTile { bytes: got.clone(), fetched_ms }).await;
        let outcome = match emitted {
            Emitted::Kept => "kept",
            Emitted::NotKept => "not_kept",
        };
        metrics::counter!("tuile_tiles_emitted_total", "layer" => k.0.clone(), "outcome" => outcome).increment(1);
        return Ok((got, Source::Upstream));
    }
    // Shared first: whoever waits for it is waiting now, and the store's own
    // write may take an upload.
    if let Some(shared) = shared {
        shared.put(&k.0, k.1, k.2, k.3, &SharedTile { bytes: got.clone(), fetched_ms }).await;
    }
    match &got {
        Some(bytes) => store.put_fetched(&k.0, k.1, k.2, k.3, bytes.clone(), fetched).await?,
        None => {
            // The source's own "no such tile" is data: remembered in the
            // absence sibling when the layer has one.
            let absence = format!("{}{ABSENT_SUFFIX}", k.0);
            if store.layer(&absence).is_ok() {
                store.put_fetched(&absence, k.1, k.2, k.3, Bytes::from_static(ABSENT_MARKER), fetched).await?;
            }
        }
    }
    Ok((got, Source::Upstream))
}

/// The strong validator of a tile's bytes.
pub fn etag_of(bytes: &[u8]) -> String {
    format!("\"{:016x}\"", fnv1a(bytes))
}

/// FNV-1a, 64 bits: stable across builds, unlike the standard hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    /// The 64-bit parameters of the FNV specification.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET_BASIS;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}
