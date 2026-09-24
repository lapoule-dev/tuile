// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The service logic, written once, with no HTTP framework in sight.
//!
//! A server — axum, a Worker, anything — is a thin adapter over
//! [`TileService::tile`]: it owns its routes, its headers and its
//! authentication, and asks this for bytes. A tile is served from the store
//! when present; otherwise it is fetched from the layer's upstream **once**,
//! however many requests ask for it at the same moment, stored, and served.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::Serialize;

use crate::grid::Grid;
use crate::store::TileStore;
use crate::upstream::Upstream;
use crate::StoreError;

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

/// A tile, ready for an adapter to answer with.
#[derive(Debug, Clone)]
pub struct TileResponse {
    pub bytes: Bytes,
    pub content_type: String,
    /// A strong validator derived from the bytes: the same tile always has
    /// the same one, on every instance and across restarts.
    pub etag: String,
    /// Served from the store rather than fetched just now.
    pub hit: bool,
}

/// What a client needs to address a layer.
#[derive(Debug, Clone, Serialize)]
pub struct LayerMeta {
    pub name: String,
    pub grid: Grid,
    pub max_level: u8,
    pub content_type: String,
}

type Key = (String, u8, u32, u32);
type Flight = Shared<BoxFuture<'static, Result<Option<Bytes>, ServiceError>>>;

/// The store plus an upstream per layer.
pub struct TileService {
    store: Arc<TileStore>,
    upstreams: HashMap<String, Arc<dyn Upstream>>,
    inflight: Arc<Mutex<HashMap<Key, Flight>>>,
}

impl TileService {
    pub fn new(store: Arc<TileStore>, upstreams: HashMap<String, Arc<dyn Upstream>>) -> Self {
        Self { store, upstreams, inflight: Arc::default() }
    }

    pub fn store(&self) -> &Arc<TileStore> {
        &self.store
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
        if let Some(bytes) = self.store.get(layer, level, x, y).await? {
            return Ok(Some(respond(bytes, content_type, true)));
        }
        let fetched = self.fetch_once(layer, level, x, y).await?;
        Ok(fetched.map(|b| respond(b, content_type, false)))
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
        let k = key.clone();
        let flight = async move {
            let result = async {
                let upstream = upstream.ok_or_else(|| ServiceError::UnknownLayer(k.0.clone()))?;
                metrics::counter!("tuile_tiles_upstream_total", "layer" => k.0.clone()).increment(1);
                let got = upstream.fetch(k.1, k.2, k.3).await.map_err(|e| ServiceError::Upstream(e.0))?;
                if let Some(bytes) = &got {
                    store.put(&k.0, k.1, k.2, k.3, bytes.clone()).await?;
                }
                Ok(got)
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

fn respond(bytes: Bytes, content_type: String, hit: bool) -> TileResponse {
    let etag = format!("\"{:016x}\"", fnv1a(&bytes));
    TileResponse { bytes, content_type, etag, hit }
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
