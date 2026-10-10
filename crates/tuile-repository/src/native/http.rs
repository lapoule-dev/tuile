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

#[async_trait]
impl Get for HttpGet {
    async fn get(&self, path: &str) -> Result<Got, String> {
        self.get_unless(path, None).await
    }

    async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
        let url = format!("{}/{path}", self.root);
        let mut wait = FIRST_WAIT;
        let mut last = String::new();
        for attempt in 1..=TRIES {
            let mut request = self.http.get(&url);
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

/// A transport whose blocks are downloaded once.
///
/// A block of an archive never changes, so the first reply for one is handed
/// to a content store — its bytes and the size of its object, which a reader
/// asks for as often as for the bytes — and every later request for it is
/// answered from there without a word to the server. The store is the
/// host's: memory over disk (`tuile-storage-foyer`) for a process that
/// renders or bakes, so a block outlives the readers' own short memory, the
/// film, and the process. Everything that is not a block — what changes —
/// goes through each time. Only a whole reply (200) is kept.
///
/// It sits under the readers, not over them: whatever a reader keeps or
/// forgets of its own, and however it comes to ask — for the bytes, or only
/// to learn the size — a block crosses the network once, for as long as the
/// store holds it. A store too small for a film's blocks gives some up and
/// they are downloaded again: size it for the film.
pub struct Kept<G> {
    inner: G,
    store: std::sync::Arc<dyn tuile_core::storage::ContentStore>,
    /// Requests answered by the server, and from the store.
    asked: std::sync::atomic::AtomicU64,
    kept: std::sync::atomic::AtomicU64,
}

impl<G: Get> Kept<G> {
    pub fn new(inner: G, store: std::sync::Arc<dyn tuile_core::storage::ContentStore>) -> Self {
        Self {
            inner,
            store,
            asked: Default::default(),
            kept: Default::default(),
        }
    }

    /// `(blocks asked of the server, blocks answered from the store)`.
    pub fn blocks(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.asked.load(Relaxed), self.kept.load(Relaxed))
    }

    fn is_block(path: &str) -> bool {
        path.contains(&format!("/{}/", crate::block_segment()))
    }
}

#[async_trait]
impl<G: Get> Get for Kept<G> {
    /// What changes is asked of the server each time, with the caller's
    /// validator; a block has none to offer, being kept for good.
    async fn get_unless(&self, path: &str, known: Option<&str>) -> Result<Got, String> {
        if Self::is_block(path) {
            self.get(path).await
        } else {
            self.inner.get_unless(path, known).await
        }
    }

    async fn get(&self, path: &str) -> Result<Got, String> {
        use std::sync::atomic::Ordering::Relaxed;
        if !Self::is_block(path) {
            return self.inner.get(path).await;
        }
        // The block's path is its identity. Eight bytes of the object's
        // size, then the block.
        if let Some(bytes) = self.store.get(path).await {
            if let Some((size, body)) = bytes.split_first_chunk::<8>() {
                self.kept.fetch_add(1, Relaxed);
                return Ok(Got {
                    status: 200,
                    object_size: Some(u64::from_le_bytes(*size)),
                    etag: None,
                    body: body.to_vec(),
                });
            }
        }
        let got = self.inner.get(path).await?;
        self.asked.fetch_add(1, Relaxed);
        if let (200, Some(size)) = (got.status, got.object_size) {
            let mut bytes = Vec::with_capacity(8 + got.body.len());
            bytes.extend_from_slice(&size.to_le_bytes());
            bytes.extend_from_slice(&got.body);
            // No lifetime: an archive's block is the same for ever.
            self.store.put(path, bytes.into(), None).await;
        }
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::*;

    /// A server that counts what it is asked, shared by every transport to it.
    #[derive(Clone, Default)]
    struct Server(Arc<AtomicU64>);

    #[async_trait]
    impl Get for Server {
        async fn get(&self, path: &str) -> Result<Got, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(match path {
                p if p.ends_with("missing.pmtiles") => Got {
                    status: 404,
                    object_size: None,
                    etag: None,
                    body: Vec::new(),
                },
                p => Got {
                    status: 200,
                    object_size: Some(4242),
                    etag: None,
                    body: p.as_bytes().to_vec(),
                },
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

    #[tokio::test]
    async fn a_block_is_downloaded_once_whoever_asks_and_however_often() {
        let store: Arc<dyn tuile_core::storage::ContentStore> = Arc::new(Store::default());
        let server = Server::default();
        let asked = || server.0.load(Ordering::SeqCst);
        let block = format!("store/{}/0/layer/zone/a.pmtiles", crate::block_segment());

        let first = Kept::new(server.clone(), store.clone());
        let got = first.get(&block).await.expect("got");
        assert_eq!((got.status, got.object_size, asked()), (200, Some(4242), 1));
        // Again from the same reader, for the bytes or only for the size:
        // nothing is asked.
        for _ in 0..3 {
            let again = first.get(&block).await.expect("got");
            assert_eq!((again.object_size, &again.body), (Some(4242), &got.body));
        }
        assert_eq!(asked(), 1);
        // Another reader — another film, another day — over the same
        // store: still nothing.
        let later = Kept::new(server.clone(), store.clone());
        assert_eq!(later.get(&block).await.expect("got").body, got.body);
        assert_eq!((asked(), later.blocks()), (1, (0, 1)));

        // Another block is another download; what changes is asked each
        // time; what is not there is not kept as if it were.
        let other = format!("store/{}/1/layer/zone/a.pmtiles", crate::block_segment());
        later.get(&other).await.expect("got");
        assert_eq!(asked(), 2);
        for _ in 0..2 {
            later
                .get("store/live/layer/zone/manifest.json")
                .await
                .expect("got");
        }
        assert_eq!(asked(), 4);
        let missing = format!(
            "store/{}/0/layer/zone/missing.pmtiles",
            crate::block_segment()
        );
        for _ in 0..2 {
            assert_eq!(later.get(&missing).await.expect("got").status, 404);
        }
        assert_eq!(asked(), 6);
    }
}
