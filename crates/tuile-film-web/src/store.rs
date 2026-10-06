// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile store, read from a browser.
//!
//! Nothing here asks a server for a tile. The store's catalog and manifests
//! are fetched as the small files they are, its archives by the API's fixed
//! blocks — whole, immutable replies the browser's cache and the edge's both
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

/// GETs under the API, by the scope's own `fetch` — a page's or a worker's.
pub struct FetchGet {
    /// `https://host/api`.
    api: String,
}

async fn once(url: &str) -> Result<Got, String> {
    let response = settled(call(&js_sys::global(), "fetch", &[url.into()]).map_err(text)?)
        .await
        .map_err(text)?;
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
        body: Uint8Array::new(&buffer).to_vec(),
    })
}

#[async_trait(?Send)]
impl Get for FetchGet {
    async fn get(&self, path: &str) -> Result<Got, String> {
        let url = format!("{}/{path}", self.api);
        let mut wait = 150;
        let mut last = String::new();
        for attempt in 1..=TRIES {
            match once(&url).await {
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
}

/// A store opened through the API.
pub struct Store {
    pub tiles: ArchivedTiles,
    live: Arc<RemoteLive<FetchGet>>,
    blocks: Arc<RemoteBlocks<FetchGet>>,
}

impl Store {
    /// Reads the store's catalog. `api` is `https://host/api`.
    pub async fn open(api: &str) -> Result<Self, RepoError> {
        let fetch = Arc::new(FetchGet {
            api: api.to_string(),
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
