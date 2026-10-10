// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile store, read from a browser.
//!
//! Nothing here asks a server for a tile. The store's catalog and manifests
//! are fetched as the small files they are, its archives by the API's fixed
//! blocks — whole, validated replies the browser's cache and the edge's both
//! keep — and the tile is found here, by the same reader a native process
//! runs next to the bucket (`tuile_repository::ArchivedTiles`).

use std::sync::Arc;

use async_trait::async_trait;
use js_sys::Uint8Array;
use tuile_repository::{
    ArchivedTiles, BlockCounts, Get, Got, Now, Objects, RemoteBlocks, RemoteLive, RepoError,
};

use crate::js::{call, get, number, settled, sleep, text};

/// Tries at a request before giving up: a server under a film's load fails
/// one now and then, and the same request a moment later goes through.
const TRIES: u32 = 5;

/// How a request says who asks.
enum Credential {
    /// In a header: a request a browser preflights when it crosses origins.
    Header(String, String),
    /// In the address: no header of its own, so no preflight.
    Parameter(String, String),
}

/// GETs under an address, by the scope's own `fetch` — a page's or a
/// worker's.
pub struct FetchGet {
    /// `https://host/api`, or the address of a store served elsewhere.
    api: String,
    credential: Option<Credential>,
}

// One block of an archive, from the network once and from then on from the
// browser's cache storage — which is on disk, outlives the page, and is one
// for the page and every one of its workers.
//
// An archive can be written again under its key, so a block that is there is
// asked about once by each scope that reads it — a HEAD, which crosses
// origins without a preflight and brings the block's validator and no body —
// and compared with the validator of what is kept. The same: the kept block
// is read, now and for the rest of this scope's life, with nothing more
// asked. Another, or none to compare: the block is downloaded and replaces
// the one that was there. A lock named after the block makes its first
// readers one: workers that want the same block at the same moment wait for
// a single download.
//
// `key` is the block's address without what says who asks: the same block
// under another credential is the same block.
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
const KEPT = "tuile-store-blocks-v2";
const SURE = new Set();
const bare = (tag) => (tag || "").replace(/^W\//, "");
export async function tuile_block(url, key, header, value) {
  const init = header ? { headers: { [header]: value } } : {};
  const ask = () => fetch(url, init);
  if (!globalThis.caches) return [await ask(), false];
  const once = async () => {
    const cache = await caches.open(KEPT);
    const kept = await cache.match(key);
    if (kept) {
      if (SURE.has(key)) return [kept, true];
      const held = bare(kept.headers.get("etag"));
      if (held) {
        try {
          const now = await fetch(url, { ...init, method: "HEAD", cache: "no-store" });
          if (now.ok && bare(now.headers.get("etag")) === held) {
            SURE.add(key);
            return [kept, true];
          }
        } catch (_) { /* not answered: read it again below */ }
      }
    }
    const response = await ask();
    if (response.status === 200) {
      try {
        await cache.put(key, response.clone());
        SURE.add(key);
      } catch (_) { /* full: read, not kept */ }
    }
    return [response, false];
  };
  const locks = globalThis.navigator && globalThis.navigator.locks;
  return locks ? locks.request("tuile-block:" + key, once) : once();
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(catch)]
    async fn tuile_block(
        url: &str,
        key: &str,
        header: Option<String>,
        value: Option<String>,
    ) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
}

thread_local! {
    /// Blocks this scope took from the network, and from the browser's
    /// cache storage: what says whether a block was read twice.
    static BLOCKS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// `(from the network, from what the browser kept)`, for this page or worker.
pub fn blocks_read() -> (u64, u64) {
    BLOCKS.with(std::cell::Cell::get)
}

async fn read(response: wasm_bindgen::JsValue) -> Result<Got, String> {
    let status = number(&response, "status") as u16;
    let size = call(&get(&response, "headers"), "get", &["x-object-size".into()])
        .ok()
        .and_then(|v| v.as_string())
        .and_then(|v| v.parse().ok());
    let buffer = settled(call(&response, "arrayBuffer", &[]).map_err(text)?)
        .await
        .map_err(text)?;
    Ok(Got {
        status,
        object_size: size,
        etag: None,
        body: Uint8Array::new(&buffer).to_vec(),
    })
}

impl FetchGet {
    /// The address asked, and the same without what says who asks.
    fn addresses(&self, path: &str) -> (String, String) {
        let plain = format!("{}/{path}", self.api);
        match &self.credential {
            Some(Credential::Parameter(name, value)) => {
                let mark = if plain.contains('?') { '&' } else { '?' };
                (format!("{plain}{mark}{name}={value}"), plain)
            }
            _ => (plain.clone(), plain),
        }
    }

    fn header(&self) -> Option<(String, String)> {
        match &self.credential {
            Some(Credential::Header(name, value)) => Some((name.clone(), value.clone())),
            _ => None,
        }
    }

    async fn once(&self, path: &str) -> Result<Got, String> {
        let (url, key) = self.addresses(path);
        let header = self.header();
        // A block of an archive is read once and kept; a scope that finds
        // one kept asks once whether it is still the object's. Everything
        // else is asked each time.
        if path.contains(&format!("/{}/", tuile_repository::block_segment())) {
            let (name, value) = header.unzip();
            let answer = tuile_block(&url, &key, name, value).await.map_err(text)?;
            let kept = get(&answer, "1").as_bool().unwrap_or(false);
            BLOCKS.with(|b| {
                let (network, held) = b.get();
                b.set(if kept {
                    (network, held + 1)
                } else {
                    (network + 1, held)
                });
            });
            return read(get(&answer, "0")).await;
        }
        once(&url, header.as_ref(), None).await
    }
}

/// One `fetch`: a GET, or — given a body — a POST of it.
///
/// The body is a string, and nothing says what it is: `fetch` labels a
/// string `text/plain;charset=UTF-8` by itself, which is one of the content
/// types a browser sends to another origin without asking first. With the
/// credential in the address, a POST then costs one request, as a GET does;
/// naming the content type `application/json` would cost a preflight each.
async fn once(
    url: &str,
    header: Option<&(String, String)>,
    body: Option<&str>,
) -> Result<Got, String> {
    let asked = if header.is_none() && body.is_none() {
        call(&js_sys::global(), "fetch", &[url.into()])
    } else {
        let init = js_sys::Object::new();
        if let Some((name, value)) = header {
            let headers = js_sys::Object::new();
            js_sys::Reflect::set(&headers, &name.as_str().into(), &value.as_str().into())
                .map_err(text)?;
            js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(text)?;
        }
        if let Some(body) = body {
            js_sys::Reflect::set(&init, &"method".into(), &"POST".into()).map_err(text)?;
            js_sys::Reflect::set(&init, &"body".into(), &body.into()).map_err(text)?;
        }
        call(&js_sys::global(), "fetch", &[url.into(), init.into()])
    };
    read(settled(asked.map_err(text)?).await.map_err(text)?).await
}

/// A request tried again while its server is busy, or fails in passing.
/// Everything asked here only reads, so anything is safe to send twice.
async fn tried<F: std::future::Future<Output = Result<Got, String>>>(
    once: impl Fn() -> F,
) -> Result<Got, String> {
    let mut wait = 150;
    let mut last = String::new();
    for attempt in 1..=TRIES {
        match once().await {
            // Busy, or a passing failure: worth asking again.
            Ok(got) if matches!(got.status, 429 | 500 | 502 | 503 | 504) => {
                last = format!("HTTP {}", got.status);
            }
            Ok(got) => return Ok(got),
            Err(why) => last = why,
        }
        if attempt < TRIES {
            sleep(wait).await;
            wait *= 3;
        }
    }
    Err(format!("{last}, after {TRIES} tries"))
}

#[async_trait(?Send)]
impl Get for FetchGet {
    async fn get(&self, path: &str) -> Result<Got, String> {
        tried(|| self.once(path)).await
    }

    /// Many small objects asked about in one request: see [`once`] for what
    /// keeps it from being preflighted.
    async fn post(&self, path: &str, body: String) -> Result<Got, String> {
        let (url, _) = self.addresses(path);
        let header = self.header();
        tried(|| once(&url, header.as_ref(), Some(&body))).await
    }
}

/// A store opened through the API.
pub struct Store {
    pub tiles: ArchivedTiles,
    live: Arc<RemoteLive<FetchGet>>,
    blocks: Arc<RemoteBlocks<FetchGet>>,
}

impl Store {
    /// Reads the store's catalog. `api` is `https://host/api`.
    ///
    /// The API is asked first where the store is (`store/at`): behind the
    /// API itself, which is the answer of one that says nothing, or at an
    /// address of its own, which this reader then goes to directly — with
    /// the credential that address wants, in a header or in the address.
    pub async fn open(api: &str) -> Result<Self, RepoError> {
        let elsewhere = match once(&format!("{api}/store/at"), None, None).await {
            Ok(got) if got.status == 200 => {
                serde_json::from_slice::<tuile_repository::StoreAt>(&got.body).ok()
            }
            _ => None,
        };
        let fetch = Arc::new(match elsewhere {
            Some(at) => FetchGet {
                api: at.url.trim_end_matches('/').to_string(),
                credential: match (at.parameter, at.header, at.credential) {
                    (Some(name), _, Some(value)) => Some(Credential::Parameter(name, value)),
                    (None, Some(name), Some(value)) => Some(Credential::Header(name, value)),
                    _ => None,
                },
            },
            None => FetchGet {
                api: api.to_string(),
                credential: None,
            },
        });
        let blocks = Arc::new(RemoteBlocks::new(fetch.clone(), "store"));
        let now: Now = Arc::new(|| (js_sys::Date::now() / 1000.0) as u64);
        let live = Arc::new(RemoteLive::new(fetch, "store"));
        let tiles = ArchivedTiles::open(live.clone(), blocks.clone(), now).await?;
        Ok(Self {
            tiles,
            live,
            blocks,
        })
    }

    /// One of the store's small files, whole — or `None` if the store has
    /// no such file. A failure to ask is a failure, not an absence: a
    /// renderer that took the one for the other would draw without what it
    /// was meant to draw with, and say nothing.
    pub async fn small(&self, key: &str) -> Result<Option<Vec<u8>>, RepoError> {
        match self.live.read_all(key).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(RepoError::NotFound(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// The store's small files, as objects: what
    /// `tuile_repository::tone::film_tone` reads a film's grades through.
    pub fn live(&self) -> &dyn Objects {
        self.live.as_ref()
    }

    /// Blocks of archives asked of the API, and blocks answered from memory.
    pub fn counts(&self) -> BlockCounts {
        self.blocks.counts()
    }
}
