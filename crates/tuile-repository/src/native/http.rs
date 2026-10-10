// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::time::Duration;

use async_trait::async_trait;

use crate::{Get, Got};

/// A GET under the address of a tile store somebody else serves, with the
/// header that server wants a credential in: the transport of
/// [`crate::RemoteStore`] in a native process.
pub struct HttpGet {
    http: reqwest::Client,
    root: String,
    credential: Option<(String, String)>,
}

const TRIES: u32 = 4;
const FIRST_WAIT: Duration = Duration::from_millis(150);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl HttpGet {
    /// `root` is what the store's routes are under (`<root>/store/live/…`);
    /// `credential` a header's name and the value to send in it.
    pub fn new(root: &str, credential: Option<(String, String)>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            root: root.trim_end_matches('/').to_string(),
            credential,
        })
    }

    /// The store named by `TUILE_TILES_REMOTE`, with the header
    /// `TUILE_TILES_REMOTE_HEADER` names carrying `TUILE_TILES_REMOTE_SECRET`;
    /// `None` when no address is set.
    pub fn from_env() -> Result<Option<Self>, String> {
        let set = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let Some(root) = set("TUILE_TILES_REMOTE") else {
            return Ok(None);
        };
        let credential = match set("TUILE_TILES_REMOTE_HEADER") {
            Some(name) => Some((
                name,
                set("TUILE_TILES_REMOTE_SECRET").ok_or(
                    "TUILE_TILES_REMOTE_HEADER is set and TUILE_TILES_REMOTE_SECRET is not",
                )?,
            )),
            None => None,
        };
        Self::new(&root, credential).map(Some)
    }

    pub fn root(&self) -> &str {
        &self.root
    }
}

impl HttpGet {
    /// One request, tried again while its server is busy: a GET, perhaps
    /// conditional, or — given a body — a POST of it. Either only reads, so
    /// either is safe to send twice.
    async fn asked(
        &self,
        path: &str,
        known: Option<&str>,
        body: Option<&str>,
    ) -> Result<Got, String> {
        let url = format!("{}/{path}", self.root);
        let mut wait = FIRST_WAIT;
        let mut last = String::new();
        for attempt in 1..=TRIES {
            let mut request = match body {
                // As a browser sends it, so one server answers both.
                Some(body) => self
                    .http
                    .post(&url)
                    .header("content-type", "text/plain;charset=UTF-8")
                    .body(body.to_string()),
                None => self.http.get(&url),
            };
            if let Some((name, value)) = &self.credential {
                request = request.header(name, value);
            }
            if let Some(known) = known {
                request = request.header("if-none-match", known);
            }
            match request.send().await {
                // Busy, or a passing failure: worth asking again.
                Ok(response)
                    if matches!(response.status().as_u16(), 429 | 500 | 502 | 503 | 504) =>
                {
                    last = format!("{path}: HTTP {}", response.status().as_u16());
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    let header = |name: &str| {
                        response
                            .headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    let object_size = header("x-object-size").and_then(|v| v.parse().ok());
                    let etag = header("etag");
                    let body = response.bytes().await.map_err(|e| format!("{path}: {e}"))?;
                    return Ok(Got {
                        status,
                        object_size,
                        etag,
                        body: body.to_vec(),
                    });
                }
                Err(e) => last = format!("{path}: {e}"),
            }
            if attempt < TRIES {
                tokio::time::sleep(wait).await;
                wait *= 3;
            }
        }
        Err(format!("{last}, after {TRIES} tries"))
    }
}

#[async_trait]
impl Get for HttpGet {
    async fn get(&self, path: &str) -> Result<Got, String> {
        self.asked(path, None, None).await
    }

    async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
        self.asked(path, known, None).await
    }

    async fn post(&self, path: &str, body: String) -> Result<Got, String> {
        self.asked(path, None, Some(&body)).await
    }
}

/// A transport whose blocks are downloaded once.
///
/// The first reply for a block is handed to a content store — its bytes, the
/// size of its object, which a reader asks for as often as for the bytes, and
/// the validator the server gave it. From then on:
///
/// - **within this transport's life** — a film, a bake — a block that has
///   been read or confirmed is answered from the store without a word to the
///   server, however often and by whichever reader it is asked for;
/// - **the first time a later transport wants it** — another film, another
///   day — the server is asked whether it is still the same, with the
///   validator (`If-None-Match`): a 304 and no bytes when it is, the new
///   block when the object was written again under its key.
///
/// The store is the host's: memory over disk (`tuile-storage-foyer`) for a
/// process that renders or bakes, so a block outlives the readers' own short
/// memory, the film, and the process. Everything that is not a block goes
/// through each time. Only a whole reply (200) is kept, and a block the
/// server gave no validator for is kept for this transport's life only:
/// nothing could say later whether it is still true.
///
/// It sits under the readers, not over them: whatever a reader keeps or
/// forgets of its own, and however it comes to ask — for the bytes, or only
/// to learn the size — a block crosses the network once, for as long as the
/// store holds it. A store too small for a film's blocks gives some up and
/// they are downloaded again: size it for the film.
pub struct Kept<G> {
    inner: G,
    store: std::sync::Arc<dyn tuile_core::storage::ContentStore>,
    /// The blocks read or confirmed by this transport: asked about no more.
    sure: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Blocks downloaded, confirmed unchanged by the server, and answered
    /// from the store without asking.
    asked: std::sync::atomic::AtomicU64,
    confirmed: std::sync::atomic::AtomicU64,
    kept: std::sync::atomic::AtomicU64,
}

/// A block as the store holds it: the object's size, the server's validator
/// (empty when it gave none), the bytes.
fn packed(size: u64, etag: Option<&str>, body: &[u8]) -> Vec<u8> {
    let etag = etag.unwrap_or_default().as_bytes();
    let etag = &etag[..etag.len().min(usize::from(u16::MAX))];
    let mut bytes = Vec::with_capacity(10 + etag.len() + body.len());
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&(etag.len() as u16).to_le_bytes());
    bytes.extend_from_slice(etag);
    bytes.extend_from_slice(body);
    bytes
}

fn unpacked(bytes: &[u8]) -> Option<(u64, Option<&str>, &[u8])> {
    let (size, rest) = bytes.split_first_chunk::<8>()?;
    let (len, rest) = rest.split_first_chunk::<2>()?;
    let (etag, body) = rest.split_at_checked(usize::from(u16::from_le_bytes(*len)))?;
    let etag = std::str::from_utf8(etag).ok()?;
    Some((
        u64::from_le_bytes(*size),
        (!etag.is_empty()).then_some(etag),
        body,
    ))
}

impl<G: Get> Kept<G> {
    pub fn new(inner: G, store: std::sync::Arc<dyn tuile_core::storage::ContentStore>) -> Self {
        Self {
            inner,
            store,
            sure: Default::default(),
            asked: Default::default(),
            confirmed: Default::default(),
            kept: Default::default(),
        }
    }

    /// `(blocks downloaded, blocks the server confirmed unchanged, blocks
    /// answered from the store without asking)`.
    pub fn blocks(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.asked.load(Relaxed),
            self.confirmed.load(Relaxed),
            self.kept.load(Relaxed),
        )
    }

    fn is_block(path: &str) -> bool {
        path.contains(&format!("/{}/", crate::block_segment()))
    }

    /// Where a block is in the store: its path, and the shape it is held in.
    fn place(path: &str) -> String {
        format!("{path}#validated")
    }

    fn is_sure(&self, path: &str) -> bool {
        self.sure.lock().expect("lock").contains(path)
    }

    fn now_sure(&self, path: &str) {
        self.sure.lock().expect("lock").insert(path.to_string());
    }
}

#[async_trait]
impl<G: Get> Get for Kept<G> {
    /// What changes is asked of the server each time, with the caller's
    /// validator; a block is asked about with the one kept beside it.
    async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
        if Self::is_block(path) {
            self.get(path).await
        } else {
            self.inner.get_unless(path, known).await
        }
    }

    /// Nothing posted is a block: it goes through as it is.
    async fn post(&self, path: &str, body: String) -> Result<Got, String> {
        self.inner.post(path, body).await
    }

    async fn get(&self, path: &str) -> Result<Got, String> {
        use std::sync::atomic::Ordering::Relaxed;
        if !Self::is_block(path) {
            return self.inner.get(path).await;
        }
        let place = Self::place(path);
        let held = self.store.get(&place).await;
        let held = held.as_deref().and_then(unpacked);
        let from_store = |(size, etag, body): (u64, Option<&str>, &[u8])| Got {
            status: 200,
            object_size: Some(size),
            etag: etag.map(str::to_string),
            body: body.to_vec(),
        };
        if let Some(held) = held {
            if self.is_sure(path) {
                self.kept.fetch_add(1, Relaxed);
                return Ok(from_store(held));
            }
        }
        // Not yet asked about by this transport. With a validator, the
        // server says whether what is held is still the object's block;
        // without one it can only send the block again.
        let known = held.and_then(|(_, etag, _)| etag);
        let got = match known {
            Some(known) => self.inner.get_unless(path, Some(known)).await?,
            None => self.inner.get(path).await?,
        };
        if let (304, Some(held)) = (got.status, held) {
            self.confirmed.fetch_add(1, Relaxed);
            self.now_sure(path);
            return Ok(from_store(held));
        }
        self.asked.fetch_add(1, Relaxed);
        if let (200, Some(size)) = (got.status, got.object_size) {
            let bytes = packed(size, got.etag.as_deref(), &got.body);
            // No lifetime: what says whether it is still true is the
            // server, asked with the validator.
            self.store.put(&place, bytes.into(), None).await;
            self.now_sure(path);
        }
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::*;

    /// A server of blocks that counts what it sends — whole blocks, and
    /// "you have it" — and whose objects can be written again. Shared by
    /// every transport to it.
    #[derive(Clone, Default)]
    struct Server {
        sent: Arc<AtomicU64>,
        unchanged: Arc<AtomicU64>,
        /// How many times the objects were written: their validator.
        version: Arc<AtomicU64>,
        /// A server that gives no validator.
        silent: bool,
    }

    #[async_trait]
    impl Get for Server {
        async fn get(&self, path: &str) -> Result<Got, String> {
            self.get_unless(path, None).await
        }

        async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
            let version = self.version.load(Ordering::SeqCst);
            let etag = format!("\"v{version}\"");
            if path.ends_with("missing.pmtiles") {
                return Ok(Got {
                    status: 404,
                    object_size: None,
                    etag: None,
                    body: Vec::new(),
                });
            }
            if known == Some(etag.as_str()) {
                self.unchanged.fetch_add(1, Ordering::SeqCst);
                return Ok(Got {
                    status: 304,
                    object_size: None,
                    etag: Some(etag),
                    body: Vec::new(),
                });
            }
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(Got {
                status: 200,
                object_size: Some(4242),
                etag: (!self.silent).then_some(etag),
                body: format!("{path} v{version}").into_bytes(),
            })
        }
    }

    /// The host's store, standing in: what is put is there to be got.
    #[derive(Default)]
    struct Store(std::sync::Mutex<std::collections::HashMap<String, bytes::Bytes>>);

    #[async_trait]
    impl tuile_core::storage::ContentStore for Store {
        async fn get(&self, key: &str) -> Option<bytes::Bytes> {
            self.0.lock().expect("lock").get(key).cloned()
        }
        async fn put(&self, key: &str, value: bytes::Bytes, _: Option<Duration>) {
            self.0.lock().expect("lock").insert(key.to_string(), value);
        }
    }

    fn block(index: u32, name: &str) -> String {
        format!("store/{}/{index}/layer/zone/{name}", crate::block_segment())
    }

    #[tokio::test]
    async fn a_block_is_downloaded_once_whoever_asks_and_however_often() {
        let store: Arc<dyn tuile_core::storage::ContentStore> = Arc::new(Store::default());
        let server = Server::default();
        let sent = || server.sent.load(Ordering::SeqCst);
        let unchanged = || server.unchanged.load(Ordering::SeqCst);
        let a = block(0, "a.pmtiles");

        let first = Kept::new(server.clone(), store.clone());
        let got = first.get(&a).await.expect("got");
        assert_eq!((got.status, got.object_size, sent()), (200, Some(4242), 1));
        // Again from the same reader, for the bytes or only for the size:
        // nothing is asked, not even whether it changed.
        for _ in 0..3 {
            let again = first.get(&a).await.expect("got");
            assert_eq!((again.object_size, &again.body), (Some(4242), &got.body));
        }
        assert_eq!((sent(), unchanged(), first.blocks()), (1, 0, (1, 0, 3)));

        // Another reader — another film, another day — over the same store:
        // the server is asked once whether the block is still the same, says
        // so without sending it, and is not asked again.
        let later = Kept::new(server.clone(), store.clone());
        for _ in 0..3 {
            assert_eq!(later.get(&a).await.expect("got").body, got.body);
        }
        assert_eq!((sent(), unchanged(), later.blocks()), (1, 1, (0, 1, 2)));

        // Another block is another download; what changes is asked each
        // time; what is not there is not kept as if it were.
        later.get(&block(1, "a.pmtiles")).await.expect("got");
        assert_eq!(sent(), 2);
        for _ in 0..2 {
            later
                .get("store/live/layer/zone/manifest.json")
                .await
                .expect("got");
        }
        assert_eq!(sent(), 4);
        let missing = block(0, "missing.pmtiles");
        for _ in 0..2 {
            assert_eq!(later.get(&missing).await.expect("got").status, 404);
        }
        assert_eq!(later.blocks().0, 3);
    }

    #[tokio::test]
    async fn an_archive_written_again_under_its_name_is_downloaded_again() {
        let store: Arc<dyn tuile_core::storage::ContentStore> = Arc::new(Store::default());
        let server = Server::default();
        let a = block(0, "a.pmtiles");

        let first = Kept::new(server.clone(), store.clone());
        let old = first.get(&a).await.expect("got").body;

        // The same key, the same size, other bytes.
        server.version.fetch_add(1, Ordering::SeqCst);
        // The film that was reading it goes on with what it read: one film,
        // one reading of a block.
        assert_eq!(first.get(&a).await.expect("got").body, old);
        // The next one is told, and holds the new block from then on.
        let next = Kept::new(server.clone(), store.clone());
        let new = next.get(&a).await.expect("got").body;
        assert_ne!(new, old);
        assert_eq!(next.get(&a).await.expect("got").body, new);
        assert_eq!(next.blocks(), (1, 0, 1));
        assert_eq!(server.unchanged.load(Ordering::SeqCst), 0);
        // And the one after that only asks.
        let last = Kept::new(server.clone(), store.clone());
        assert_eq!(last.get(&a).await.expect("got").body, new);
        assert_eq!(last.blocks(), (0, 1, 0));
    }

    #[tokio::test]
    async fn a_block_with_no_validator_is_kept_for_one_film_only() {
        let store: Arc<dyn tuile_core::storage::ContentStore> = Arc::new(Store::default());
        let server = Server {
            silent: true,
            ..Server::default()
        };
        let a = block(0, "a.pmtiles");
        let first = Kept::new(server.clone(), store.clone());
        for _ in 0..3 {
            first.get(&a).await.expect("got");
        }
        assert_eq!(first.blocks(), (1, 0, 2));
        // Nothing can say it is still true: the next film reads it again.
        let next = Kept::new(server.clone(), store.clone());
        next.get(&a).await.expect("got");
        assert_eq!(next.blocks(), (1, 0, 0));
    }
}
