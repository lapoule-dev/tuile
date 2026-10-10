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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::lock::Mutex as AsyncMutex;

use crate::bench::{block_segment, LiveAnswers, LiveAsk, LiveAsked, LIVE_MANY};
use crate::objects::each_if_changed;
use crate::{Entry, Listing, Objects, Read, RepoError, BLOCK};

/// What came back from a GET.
#[derive(Debug, Clone, Default)]
pub struct Got {
    pub status: u16,
    /// The `x-object-size` header: the size of the object a block is of.
    pub object_size: Option<u64>,
    /// The reply's validator (`ETag`), when its server gave one.
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

/// A GET of a path under the API (`store/live/catalog.json`), by whatever
/// the host has for it.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Get: Send + Sync {
    async fn get(&self, path: &str) -> Result<Got, String>;

    /// The same GET, unless what is there is still what `known` names: a
    /// conditional request (`If-None-Match`), answered 304 with no body by a
    /// server — or a cache in front of it — that honours one. A transport
    /// that cannot ask conditionally asks plainly, which is this default:
    /// correct, and no saving.
    async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
        let _ = known;
        self.get(path).await
    }

    /// A POST of `body` to a path under the API: how many small objects are
    /// asked about in one request (`store/live`, see
    /// [`crate::Bench::post`]). The body is JSON and is sent as JSON
    /// (`application/json`). From a browser, to another origin, that costs
    /// one preflight for the route's address — one, kept by the browser for
    /// as long as the server allows — and not one per request.
    ///
    /// A transport that cannot post answers as a server without the route
    /// does — `405` —, which is this default: its reader asks one by one.
    async fn post(&self, path: &str, body: String) -> Result<Got, String> {
        let _ = (path, body);
        Ok(Got {
            status: 405,
            ..Got::default()
        })
    }
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
    /// Whether the server was found not to answer for many objects at once
    /// (`POST <root>/live`): an older one. Found once and remembered, so it
    /// is asked one by one from then on rather than refused at every turn.
    alone: AtomicBool,
}

impl<G: Get> RemoteLive<G> {
    /// `root` is the route the objects are under: `store`.
    pub fn new(get: Arc<G>, root: impl Into<String>) -> Self {
        Self {
            get,
            root: root.into(),
            alone: AtomicBool::new(false),
        }
    }

    /// Up to [`LIVE_MANY`] objects asked about in one request. `None` is a
    /// server that has no such route: nothing was learnt of the objects.
    async fn together(
        &self,
        asked: &[(String, Option<String>)],
    ) -> Option<Vec<Result<Read, RepoError>>> {
        let all = |why: String| -> Vec<Result<Read, RepoError>> {
            asked
                .iter()
                .map(|(key, _)| Err(RepoError::Store(format!("{key}: {why}"))))
                .collect()
        };
        let body = serde_json::to_string(&LiveAsked {
            objects: asked
                .iter()
                .map(|(key, etag)| LiveAsk {
                    key: key.clone(),
                    etag: etag.clone(),
                })
                .collect(),
        });
        let body = match body {
            Ok(body) => body,
            Err(e) => return Some(all(e.to_string())),
        };
        let got = match self.get.post(&format!("{}/live", self.root), body).await {
            Ok(got) => got,
            // A request that failed says nothing of the route.
            Err(e) => return Some(all(e)),
        };
        let answers = match got.status {
            200 => match serde_json::from_slice::<LiveAnswers>(&got.body) {
                Ok(answers) if answers.objects.len() == asked.len() => answers.objects,
                Ok(answers) => {
                    return Some(all(format!(
                        "{} answers to {} questions",
                        answers.objects.len(),
                        asked.len()
                    )))
                }
                Err(e) => return Some(all(format!("not a reply to many: {e}"))),
            },
            404 | 405 => return None,
            status => return Some(all(format!("HTTP {status}"))),
        };
        let mut out = Vec::with_capacity(asked.len());
        for ((key, known), answer) in asked.iter().zip(answers) {
            out.push(match answer.status {
                // Answers come in the order asked: one under another key
                // is an answer to something else.
                _ if answer.key != *key => Err(RepoError::Store(format!(
                    "{key}: answered as {}",
                    answer.key
                ))),
                200 => match answer.body {
                    Some(body) => Ok(Read::Changed {
                        etag: answer.etag,
                        bytes: body.into_bytes(),
                    }),
                    None => Err(RepoError::Store(format!("{key}: sent without its body"))),
                },
                304 if known.is_some() => Ok(Read::Unchanged),
                404 => Err(RepoError::NotFound(key.clone())),
                // Left out of a reply that was full: asked for alone.
                413 => self.read_if_changed(key, known.as_deref()).await,
                status => Err(RepoError::Store(format!(
                    "{key}: HTTP {status} — {}",
                    answer.error.unwrap_or_default()
                ))),
            });
        }
        Some(out)
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

    /// Asked with the validator the caller holds: a server that still has
    /// that object says so and sends nothing.
    async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
        let path = format!("{}/live/{}", self.root, encoded(key));
        let got = self
            .get
            .get_unless(&path, known)
            .await
            .map_err(|e| RepoError::Store(format!("{key}: {e}")))?;
        match got.status {
            304 if known.is_some() => Ok(Read::Unchanged),
            200 => Ok(Read::Changed {
                etag: got.etag,
                bytes: got.body,
            }),
            404 => Err(RepoError::NotFound(key.to_string())),
            status => Err(RepoError::Store(format!("{key}: HTTP {status}"))),
        }
    }

    /// Asked together, in one request for every [`LIVE_MANY`] of them: a
    /// server answers each as it would have alone. One that has no such
    /// route — an older one — is asked one by one, this time and from then
    /// on.
    async fn read_many_if_changed(
        &self,
        asked: &[(String, Option<String>)],
    ) -> Vec<Result<Read, RepoError>> {
        let mut out = Vec::with_capacity(asked.len());
        for some in asked.chunks(LIVE_MANY) {
            // One object is one GET, which a cache in front may answer.
            let together = if some.len() > 1 && !self.alone.load(Ordering::Relaxed) {
                self.together(some).await
            } else {
                None
            };
            match together {
                Some(answers) => out.extend(answers),
                None => {
                    if some.len() > 1 {
                        self.alone.store(true, Ordering::Relaxed);
                    }
                    out.extend(each_if_changed(self, some).await);
                }
            }
        }
        out
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

/// A tile store behind another server's routes, as one set of objects: what
/// changes — its catalog, its manifests, its tables, every `.json` — read
/// whole each time ([`RemoteLive`]), an archive by blocks ([`RemoteBlocks`]).
/// For a reader that is handed a store as a bucket would be.
pub struct RemoteStore<G> {
    live: RemoteLive<G>,
    blocks: RemoteBlocks<G>,
}

impl<G: Get> RemoteStore<G> {
    /// `root` is the route the store is under: `store`.
    pub fn new(get: Arc<G>, root: impl Into<String>) -> Self {
        let root = root.into();
        Self {
            live: RemoteLive::new(get.clone(), root.clone()),
            blocks: RemoteBlocks::new(get, root),
        }
    }

    fn changes(key: &str) -> bool {
        key.ends_with(".json")
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<G: Get> Objects for RemoteStore<G> {
    fn label(&self) -> String {
        self.blocks.label()
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        unlistable(prefix)
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        unlistable(prefix)
    }

    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        if Self::changes(key) {
            self.live.size(key).await
        } else {
            self.blocks.size(key).await
        }
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        if Self::changes(key) {
            self.live.read(key, range).await
        } else {
            self.blocks.read(key, range).await
        }
    }

    /// One request for what changes, where the default would make two.
    async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        if Self::changes(key) {
            self.live.read_all(key).await
        } else {
            let size = self.blocks.size(key).await?;
            self.blocks.read(key, 0..size).await
        }
    }

    async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
        if Self::changes(key) {
            self.live.read_if_changed(key, known).await
        } else {
            Ok(Read::Changed {
                bytes: self.read_all(key).await?,
                etag: None,
            })
        }
    }

    /// What changes is asked for together; an archive, should one be among
    /// them, by its blocks as ever.
    async fn read_many_if_changed(
        &self,
        asked: &[(String, Option<String>)],
    ) -> Vec<Result<Read, RepoError>> {
        if asked.iter().all(|(key, _)| Self::changes(key)) {
            self.live.read_many_if_changed(asked).await
        } else {
            each_if_changed(self, asked).await
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A server of one small file with one validator, that notes what each
    /// request was conditional on.
    #[derive(Default)]
    struct Server(Mutex<Vec<Option<String>>>);

    #[async_trait]
    impl Get for Server {
        async fn get(&self, path: &str) -> Result<Got, String> {
            self.get_unless(path, None).await
        }

        async fn get_unless(&self, _: &str, known: Option<&str>) -> Result<Got, String> {
            self.0.lock().expect("lock").push(known.map(str::to_string));
            // Compared weakly, as a server or a cache in front of it does.
            let same = known.is_some_and(|k| k.trim_start_matches("W/") == "\"v1\"");
            Ok(Got {
                status: if same { 304 } else { 200 },
                object_size: None,
                etag: Some("W/\"v1\"".into()),
                body: if same { Vec::new() } else { b"{}".to_vec() },
            })
        }
    }

    #[tokio::test]
    async fn what_changes_is_asked_with_the_validator_held_and_not_sent_again() {
        let server = Arc::new(Server::default());
        let store = RemoteStore::new(server.clone(), "store");
        let key = "layer/zone/manifest.json";

        // Nothing held: the file, and its validator as the server gave it.
        let first = store.read_if_changed(key, None).await.expect("read");
        let Read::Changed { bytes, etag } = first else {
            panic!("{first:?}")
        };
        assert_eq!(
            (bytes.as_slice(), etag.as_deref()),
            (&b"{}"[..], Some("W/\"v1\""))
        );
        // Held: asked with it, and nothing comes back.
        assert_eq!(
            store
                .read_if_changed(key, etag.as_deref())
                .await
                .expect("read"),
            Read::Unchanged
        );
        // Another validator than the server's: the file again.
        assert!(matches!(
            store
                .read_if_changed(key, Some("\"v0\""))
                .await
                .expect("read"),
            Read::Changed { .. }
        ));
        assert_eq!(
            *server.0.lock().expect("lock"),
            [
                None,
                Some("W/\"v1\"".to_string()),
                Some("\"v0\"".to_string())
            ]
        );
    }

    /// A bench behind a transport that notes every request made of it —
    /// and, made `old`, has no route for many objects at once, as a server
    /// from before there was one.
    struct Api {
        bench: crate::Bench,
        old: bool,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Get for Api {
        async fn get(&self, path: &str) -> Result<Got, String> {
            self.get_unless(path, None).await
        }

        async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
            self.asked.lock().expect("lock").push(format!("GET {path}"));
            let asked = crate::Asked {
                range: None,
                if_none_match: known,
            };
            let reply = self
                .bench
                .get(&format!("/api/{path}"), "", asked)
                .await
                .ok_or("no route")?;
            Ok(Got {
                status: reply.status,
                object_size: reply.object_size,
                etag: reply.etag.clone(),
                body: reply.body,
            })
        }

        async fn post(&self, path: &str, body: String) -> Result<Got, String> {
            self.asked
                .lock()
                .expect("lock")
                .push(format!("POST {path}"));
            if self.old {
                return Ok(Got {
                    status: 405,
                    ..Got::default()
                });
            }
            let reply = self
                .bench
                .post(&format!("/api/{path}"), "", body.as_bytes())
                .await
                .ok_or("no route")?;
            Ok(Got {
                status: reply.status,
                object_size: None,
                etag: None,
                body: reply.body,
            })
        }
    }

    /// Manifests `0..n` of a store, each `{"zone":i}`, written once.
    struct Zones(usize);

    #[async_trait]
    impl Objects for Zones {
        fn label(&self) -> String {
            "zones".into()
        }
        async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
            unlistable(prefix)
        }
        async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
            unlistable(prefix)
        }
        async fn size(&self, key: &str) -> Result<u64, RepoError> {
            Err(RepoError::NotFound(key.to_string()))
        }
        async fn read(&self, key: &str, _: Range<u64>) -> Result<Vec<u8>, RepoError> {
            Err(RepoError::NotFound(key.to_string()))
        }
        async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
            let zone = key
                .strip_suffix("/manifest.json")
                .and_then(|zone| zone.parse::<usize>().ok())
                .filter(|zone| *zone < self.0)
                .ok_or_else(|| RepoError::NotFound(key.to_string()))?;
            let etag = format!("\"z{zone}\"");
            if known == Some(etag.as_str()) {
                return Ok(Read::Unchanged);
            }
            Ok(Read::Changed {
                bytes: format!("{{\"zone\":{zone}}}").into_bytes(),
                etag: Some(etag),
            })
        }
    }

    fn api(zones: usize, old: bool) -> Arc<Api> {
        let objects: Arc<dyn Objects> = Arc::new(Zones(zones));
        Arc::new(Api {
            bench: crate::Bench {
                projects: Vec::new(),
                tiles: None,
                store: Some(crate::StoreObjects {
                    live: objects.clone(),
                    archives: objects,
                }),
                store_at: None,
            },
            old,
            asked: Mutex::default(),
        })
    }

    /// Zones `0..n`: the even ones held under their validator, the odd ones
    /// not held — and the last of them a zone the store has not.
    fn questions(n: usize) -> Vec<(String, Option<String>)> {
        (0..n)
            .map(|zone| {
                (
                    format!("{zone}/manifest.json"),
                    (zone % 2 == 0).then(|| format!("\"z{zone}\"")),
                )
            })
            .collect()
    }

    fn check(answers: &[Result<Read, RepoError>], n: usize) {
        assert_eq!(answers.len(), n);
        for (zone, answer) in answers.iter().enumerate() {
            match answer {
                Ok(Read::Unchanged) => assert!(zone % 2 == 0 && zone < n - 1, "{zone}"),
                Ok(Read::Changed { bytes, etag }) => {
                    assert!(zone % 2 == 1 && zone < n - 1, "{zone}");
                    assert_eq!(bytes, format!("{{\"zone\":{zone}}}").as_bytes());
                    assert_eq!(etag.as_deref(), Some(format!("\"z{zone}\"").as_str()));
                }
                Err(RepoError::NotFound(key)) => {
                    assert_eq!(
                        (zone, key.as_str()),
                        (n - 1, format!("{zone}/manifest.json").as_str())
                    );
                }
                Err(other) => panic!("{zone}: {other}"),
            }
        }
    }

    #[tokio::test]
    async fn many_manifests_are_one_request_each_answered_as_it_would_be_alone() {
        let n = 347;
        let server = api(n - 1, false);
        // Through the transport that keeps blocks, as a render reads.
        let kept = Arc::new(crate::Kept::new(Arced(server.clone()), Arc::new(Nowhere)));
        let store = RemoteStore::new(kept, "store");
        check(&store.read_many_if_changed(&questions(n)).await, n);
        assert_eq!(*server.asked.lock().expect("lock"), ["POST store/live"]);

        // More than one request may ask about: as few requests as hold them.
        let n = LIVE_MANY * 2 + 2;
        let server = api(n - 1, false);
        let live = RemoteLive::new(server.clone(), "store");
        check(&live.read_many_if_changed(&questions(n)).await, n);
        assert_eq!(*server.asked.lock().expect("lock"), ["POST store/live"; 3]);

        // One object alone is the GET a cache in front may answer.
        server.asked.lock().expect("lock").clear();
        let alone = live.read_many(&["1/manifest.json".to_string()]).await;
        assert_eq!(alone[0].as_deref().expect("read"), b"{\"zone\":1}");
        assert_eq!(
            *server.asked.lock().expect("lock"),
            ["GET store/live/1/manifest.json"]
        );
    }

    #[tokio::test]
    async fn an_older_server_is_asked_for_many_once_and_one_by_one_from_then_on() {
        let n = 40;
        let server = api(n - 1, true);
        let live = RemoteLive::new(server.clone(), "store");
        for round in 0..3 {
            check(&live.read_many_if_changed(&questions(n)).await, n);
            let asked = std::mem::take(&mut *server.asked.lock().expect("lock"));
            let posts = asked.iter().filter(|a| a.starts_with("POST")).count();
            // Found out once, and not put to the question again.
            assert_eq!(posts, usize::from(round == 0), "round {round}");
            assert_eq!(asked.len() - posts, n, "round {round}");
        }
    }

    /// The transport above, shared: a test counts what crossed it.
    struct Arced(Arc<Api>);

    #[async_trait]
    impl Get for Arced {
        async fn get(&self, path: &str) -> Result<Got, String> {
            self.0.get(path).await
        }
        async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
            self.0.get_unless(path, known).await
        }
        async fn post(&self, path: &str, body: String) -> Result<Got, String> {
            self.0.post(path, body).await
        }
    }

    /// A store that keeps nothing: no block is read here.
    struct Nowhere;

    #[async_trait]
    impl tuile_core::storage::ContentStore for Nowhere {
        async fn get(&self, _: &str) -> Option<bytes::Bytes> {
            None
        }
        async fn put(&self, _: &str, _: bytes::Bytes, _: Option<std::time::Duration>) {}
    }
}
