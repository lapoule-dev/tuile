// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`Objects`] on the far side of the bench's API.
//!
//! A reader that is not next to the bucket — a browser, a renderer on
//! another machine — reaches objects through the routes of [`crate::bench`]:
//! blocks for what never changes, whole small objects for what does. These
//! are those two readings as [`Objects`], so that everything written against
//! `Objects` (the tile store's reader first of all) runs there unchanged.
//!
//! How a request leaves the host is [`Get`], and nothing here knows more of
//! it than a path and what came back.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::lock::Mutex as AsyncMutex;

use crate::bench::block_segment;
use crate::{Entry, Listing, Objects, RepoError, BLOCK};

/// What came back from a GET.
#[derive(Debug, Clone, Default)]
pub struct Got {
    pub status: u16,
    /// The `x-object-size` header: the size of the object a block is of.
    pub object_size: Option<u64>,
    pub body: Vec<u8>,
}

/// A GET of a path under the API (`store/live/catalog.json`), by whatever
/// the host has for it.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Get: Send + Sync {
    async fn get(&self, path: &str) -> Result<Got, String>;
}

/// A key as a path: each segment percent-encoded, its slashes kept.
fn encoded(key: &str) -> String {
    key.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn unlistable<T>(what: &str) -> Result<T, RepoError> {
    Err(RepoError::Store(format!(
        "{what}: objects behind the API are read by key, not listed"
    )))
}

async fn fetched(get: &dyn Get, path: &str, key: &str) -> Result<Got, RepoError> {
    let got = get
        .get(path)
        .await
        .map_err(|e| RepoError::Store(format!("{key}: {e}")))?;
    match got.status {
        200 => Ok(got),
        404 => Err(RepoError::NotFound(key.to_string())),
        status => Err(RepoError::Store(format!(
            "{key}: HTTP {status} — {}",
            String::from_utf8_lossy(&got.body)
                .chars()
                .take(200)
                .collect::<String>()
        ))),
    }
}

/// Small objects that change, each read whole: a store's catalog and its
/// manifests, under `<root>/live/`.
pub struct RemoteLive<G> {
    get: Arc<G>,
    root: String,
}

impl<G: Get> RemoteLive<G> {
    /// `root` is the route the objects are under: `store`.
    pub fn new(get: Arc<G>, root: impl Into<String>) -> Self {
        Self {
            get,
            root: root.into(),
        }
    }

    async fn whole(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        let path = format!("{}/live/{}", self.root, encoded(key));
        Ok(fetched(self.get.as_ref(), &path, key).await?.body)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<G: Get> Objects for RemoteLive<G> {
    fn label(&self) -> String {
        format!("{}/live", self.root)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        unlistable(prefix)
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        unlistable(prefix)
    }

    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        Ok(self.whole(key).await?.len() as u64)
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let whole = self.whole(key).await?;
        whole
            .get(range.start as usize..range.end as usize)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                RepoError::Store(format!(
                    "{key}: bytes {}..{} are outside its {}",
                    range.start,
                    range.end,
                    whole.len()
                ))
            })
    }

    /// One request, where the default would make two.
    async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        self.whole(key).await
    }
}

/// How many blocks a reader keeps from its latest reads, unless told
/// otherwise: a tile is found by reading an archive's head, perhaps a leaf
/// of its directory, then the tile — three reads that land in one or two
/// blocks, over and over for every tile of the same archive.
pub const HELD_BLOCKS: usize = 8;

/// What a reader of blocks has asked for and been given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockCounts {
    /// Blocks asked of the API.
    pub fetched: u64,
    pub fetched_bytes: u64,
    /// Blocks answered from the ones held here.
    pub held: u64,
}

struct Held {
    /// Newest last.
    blocks: VecDeque<(String, u64, Arc<Vec<u8>>)>,
    sizes: HashMap<String, u64>,
    counts: BlockCounts,
}

/// Objects that never change, read by the API's fixed blocks, under
/// `<root>/<block segment>/<n>/<key>`.
///
/// Every reader of an object asks for the same blocks at the same addresses,
/// whichever bytes it wanted, so a block read once — by this reader or any
/// other — is answered by a cache from then on.
pub struct RemoteBlocks<G> {
    get: Arc<G>,
    root: String,
    capacity: usize,
    held: Mutex<Held>,
    /// One lock per block being fetched: readers after the same block at the
    /// same moment wait for one request rather than each making their own.
    flights: Mutex<HashMap<(String, u64), Arc<AsyncMutex<()>>>>,
}

impl<G: Get> RemoteBlocks<G> {
    /// `root` is the route the objects are under: `store`, or `p/<project>`.
    pub fn new(get: Arc<G>, root: impl Into<String>) -> Self {
        Self::holding(get, root, HELD_BLOCKS)
    }

    /// As [`Self::new`], keeping `capacity` blocks.
    pub fn holding(get: Arc<G>, root: impl Into<String>, capacity: usize) -> Self {
        Self {
            get,
            root: root.into(),
            capacity: capacity.max(1),
            held: Mutex::new(Held {
                blocks: VecDeque::new(),
                sizes: HashMap::new(),
                counts: BlockCounts::default(),
            }),
            flights: Mutex::new(HashMap::new()),
        }
    }

    pub fn counts(&self) -> BlockCounts {
        self.held.lock().map(|h| h.counts).unwrap_or_default()
    }

    async fn block(&self, key: &str, index: u64) -> Result<Arc<Vec<u8>>, RepoError> {
        if let Some(block) = self.held_block(key, index) {
            return Ok(block);
        }
        let flight = match self.flights.lock() {
            Ok(mut flights) => flights.entry((key.to_string(), index)).or_default().clone(),
            Err(_) => Arc::default(),
        };
        let _landing = flight.lock().await;
        // Whoever held the lock before may have brought the block in.
        if let Some(block) = self.held_block(key, index) {
            return Ok(block);
        }
        let fetched = self.fetch_block(key, index).await;
        if let Ok(mut flights) = self.flights.lock() {
            flights.remove(&(key.to_string(), index));
        }
        fetched
    }

    fn held_block(&self, key: &str, index: u64) -> Option<Arc<Vec<u8>>> {
        let mut held = self.held.lock().ok()?;
        let at = held
            .blocks
            .iter()
            .position(|(k, i, _)| *i == index && k == key)?;
        let entry = held.blocks.remove(at)?;
        let block = entry.2.clone();
        held.blocks.push_back(entry);
        held.counts.held += 1;
        Some(block)
    }

    async fn fetch_block(&self, key: &str, index: u64) -> Result<Arc<Vec<u8>>, RepoError> {
        let path = format!("{}/{}/{index}/{}", self.root, block_segment(), encoded(key));
        let got = fetched(self.get.as_ref(), &path, key).await?;
        let size = got.object_size.ok_or_else(|| {
            RepoError::Store(format!(
                "{key}: block {index} came without its object's size"
            ))
        })?;
        // A block is whole or it is not one: a reply cut short is refused
        // here rather than read as the end of an object.
        let wanted = (index * BLOCK + BLOCK)
            .min(size)
            .saturating_sub(index * BLOCK);
        if got.body.len() as u64 != wanted {
            return Err(RepoError::Store(format!(
                "{key}: block {index} is {} bytes, not {wanted}",
                got.body.len()
            )));
        }
        let block = Arc::new(got.body);
        if let Ok(mut held) = self.held.lock() {
            held.sizes.insert(key.to_string(), size);
            held.counts.fetched += 1;
            held.counts.fetched_bytes += block.len() as u64;
            if held.blocks.len() >= self.capacity {
                held.blocks.pop_front();
            }
            held.blocks
                .push_back((key.to_string(), index, block.clone()));
        }
        Ok(block)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<G: Get> Objects for RemoteBlocks<G> {
    fn label(&self) -> String {
        format!("{}/{}", self.root, block_segment())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        unlistable(prefix)
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        unlistable(prefix)
    }

    /// An object's size comes with any of its blocks; the first is asked
    /// for, which a reader of an archive wants next anyway.
    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        if let Some(size) = self
            .held
            .lock()
            .ok()
            .and_then(|h| h.sizes.get(key).copied())
        {
            return Ok(size);
        }
        self.block(key, 0).await?;
        self.held
            .lock()
            .ok()
            .and_then(|h| h.sizes.get(key).copied())
            .ok_or_else(|| RepoError::Store(format!("{key}: no size")))
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let mut out = Vec::with_capacity((range.end.saturating_sub(range.start)) as usize);
        for index in range.start / BLOCK..range.end.div_ceil(BLOCK) {
            let block = self.block(key, index).await?;
            let base = index * BLOCK;
            let from = range.start.max(base) - base;
            let to = range
                .end
                .min(base + block.len() as u64)
                .saturating_sub(base);
            let part = block.get(from as usize..to as usize).ok_or_else(|| {
                RepoError::Store(format!(
                    "{key}: bytes {}..{} are past its end",
                    range.start, range.end
                ))
            })?;
            out.extend_from_slice(part);
        }
        if out.len() as u64 != range.end.saturating_sub(range.start) {
            return Err(RepoError::Store(format!(
                "{key}: bytes {}..{} are past its end",
                range.start, range.end
            )));
        }
        Ok(out)
    }
}
