// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What several instances of a server share besides the bucket.
//!
//! The bucket is all that instances need to share to be right: archives are
//! immutable and a zone's manifest is replaced by a conditional write. But
//! three things stay each instance's own, and cost a source its quota when
//! there are several of them:
//!
//! - a tile one instance has just fetched is in its buffer, where no other
//!   can read it until it is flushed and the others have read the zone's
//!   manifest again;
//! - "one fetch per tile, however many ask" holds within a process only;
//! - a manifest one instance published is seen by another when its cached
//!   copy runs out, not before.
//!
//! [`SharedTiles`] closes the first two — a short-lived buffer every instance
//! reads before it asks the source, and a claim on the fetch — and
//! [`Announce`] the third. Both are for whoever deploys the server to provide
//! (a key-value store with expiry, a publish-subscribe channel): there is no
//! such service in this crate, only [`InMemory`], for tests and for several
//! services in one process.
//!
//! **None of it is needed to be right.** Nothing here returns an error: a
//! shared buffer that cannot be reached is a miss, a claim that cannot be
//! taken is unknown, an announcement that is lost is a manifest read a little
//! later. Without any of it, and with all of it failing, a server behaves as
//! one that never had it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;

use crate::layer::Zone;
use crate::store::TileStore;

/// A tile as instances share it before it is in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedTile {
    /// The tile, or `None` for the source's own "no such tile".
    pub bytes: Option<Bytes>,
    /// When it was fetched from its source, in milliseconds since the Unix
    /// epoch, by the clock of the instance that fetched it. It travels with
    /// the tile: it is what decides, at publication, whether a stored copy
    /// is newer.
    pub fetched_ms: u64,
}

/// What became of a fresh tile handed to a [`FreshTiles`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emitted {
    /// It is on its way to the store: someone else writes it there.
    Kept,
    /// It could not be handed over. The tile is still served; it is not
    /// stored, and will be fetched again one day.
    NotKept,
}

/// Where a serving instance sends the tiles it has just fetched, when it does
/// not write them to the store itself.
///
/// With one, a [`crate::TileService`] keeps no buffer and publishes nothing:
/// each fresh tile, and each "no such tile" of a source, is emitted once, and
/// whoever is behind — a queue, a stream, another process calling
/// [`TileStore::publish_batch`] — puts it in the store. What is emitted is
/// also what the other instances are to see: an implementation shares it as
/// [`SharedTiles::put`] would have.
///
/// As everything here, it returns no error: a tile that could not be emitted
/// is [`Emitted::NotKept`], and the request that fetched it is answered all
/// the same.
#[async_trait]
pub trait FreshTiles: Send + Sync {
    async fn emit(&self, layer: &str, level: u8, x: u32, y: u32, tile: &SharedTile) -> Emitted;
}

/// What became of asking to be the one that fetches a tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// This instance fetches it; the token gives the claim back.
    Mine(String),
    /// Another instance is fetching it.
    Theirs,
    /// Could not be told: fetch it, as if there were no one else.
    Unknown,
}

/// A buffer of freshly fetched tiles shared by every instance, and the claim
/// on fetching one. See the module.
///
/// An entry must outlive the time a tile takes to be read from the store by
/// every instance — a buffer's flush, then the longest a manifest is served
/// stale — and need not live longer: after that the store answers.
#[async_trait]
pub trait SharedTiles: Send + Sync {
    /// The tile, if an instance has shared it and it has not run out.
    async fn get(&self, layer: &str, level: u8, x: u32, y: u32) -> Option<SharedTile>;
    /// Shares a tile just fetched.
    async fn put(&self, layer: &str, level: u8, x: u32, y: u32, tile: &SharedTile);
    /// Asks to be the one instance that fetches this tile. A claim runs out
    /// by itself: an instance that dies holding one stops nobody for long.
    async fn claim(&self, layer: &str, level: u8, x: u32, y: u32) -> Claim;
    /// Gives a claim back, if it is still this token's.
    async fn release(&self, layer: &str, level: u8, x: u32, y: u32, token: &str);
}

/// Tells the other instances that a zone's manifest was published. What an
/// instance does on hearing it is [`TileStore::forget_zone`].
#[async_trait]
pub trait Announce: Send + Sync {
    async fn published(&self, layer: &str, zone: Zone, generation: u64);
}

type Key = (String, u8, u32, u32);

/// [`SharedTiles`] and [`Announce`] within one process: for tests, and for
/// several services that happen to live together.
pub struct InMemory {
    tiles: Mutex<HashMap<Key, (SharedTile, Instant)>>,
    claims: Mutex<HashMap<Key, (String, Instant)>>,
    listeners: Mutex<Vec<Weak<TileStore>>>,
    tile_ttl: Duration,
    claim_ttl: Duration,
    tokens: AtomicU64,
    announced: AtomicU64,
}

impl Default for InMemory {
    fn default() -> Self {
        Self::new(Duration::from_secs(300), Duration::from_secs(15))
    }
}

impl InMemory {
    /// `tile_ttl`: how long a shared tile is kept; `claim_ttl`: how long a
    /// claim holds if it is never given back.
    pub fn new(tile_ttl: Duration, claim_ttl: Duration) -> Self {
        Self {
            tiles: Mutex::default(),
            claims: Mutex::default(),
            listeners: Mutex::default(),
            tile_ttl,
            claim_ttl,
            tokens: AtomicU64::new(0),
            announced: AtomicU64::new(0),
        }
    }

    /// From now on `store` hears what is announced here.
    pub fn listen(&self, store: &Arc<TileStore>) {
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.push(Arc::downgrade(store));
        }
    }

    /// Announcements made so far.
    pub fn announced(&self) -> u64 {
        self.announced.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl SharedTiles for InMemory {
    async fn get(&self, layer: &str, level: u8, x: u32, y: u32) -> Option<SharedTile> {
        let tiles = self.tiles.lock().ok()?;
        let (tile, at) = tiles.get(&(layer.to_string(), level, x, y))?;
        (at.elapsed() < self.tile_ttl).then(|| tile.clone())
    }

    async fn put(&self, layer: &str, level: u8, x: u32, y: u32, tile: &SharedTile) {
        if let Ok(mut tiles) = self.tiles.lock() {
            tiles.insert((layer.to_string(), level, x, y), (tile.clone(), Instant::now()));
        }
    }

    async fn claim(&self, layer: &str, level: u8, x: u32, y: u32) -> Claim {
        let Ok(mut claims) = self.claims.lock() else { return Claim::Unknown };
        let key = (layer.to_string(), level, x, y);
        if claims.get(&key).is_some_and(|(_, at)| at.elapsed() < self.claim_ttl) {
            return Claim::Theirs;
        }
        let token = self.tokens.fetch_add(1, Ordering::Relaxed).to_string();
        claims.insert(key, (token.clone(), Instant::now()));
        Claim::Mine(token)
    }

    async fn release(&self, layer: &str, level: u8, x: u32, y: u32, token: &str) {
        if let Ok(mut claims) = self.claims.lock() {
            let key = (layer.to_string(), level, x, y);
            if claims.get(&key).is_some_and(|(held, _)| held == token) {
                claims.remove(&key);
            }
        }
    }
}

#[async_trait]
impl Announce for InMemory {
    async fn published(&self, layer: &str, zone: Zone, generation: u64) {
        self.announced.fetch_add(1, Ordering::Relaxed);
        let listeners: Vec<Arc<TileStore>> =
            self.listeners.lock().map(|l| l.iter().filter_map(Weak::upgrade).collect()).unwrap_or_default();
        for store in listeners {
            store.forget_zone(layer, zone, generation);
        }
    }
}
