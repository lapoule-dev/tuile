// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The shared tile store, when the job is given one.
//!
//! `TUILE_TILES_BUCKET` names the bucket every bake and the tile server
//! share; the endpoint and credentials are the job's usual `TUILE_STORE_*`.
//! Unset, a bake fetches every tile from its source, as before.
//!
//! Source tiles are then read from the store when an earlier run (of any job,
//! or the server) fetched them, and each tile fetched now is offered to it.
//! What is stored is what the source served, so the pack does not depend on
//! whether the store was there.

use std::sync::Arc;

use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use tuile_tile_server::{StoreConfig, StoreContent, TileStore};

const DEFAULT_REGION: &str = "auto";

/// The store, and the runtime its final flush runs on.
pub struct Tiles {
    runtime: tokio::runtime::Runtime,
    store: Arc<TileStore>,
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name).ok().filter(|v| !v.is_empty()).ok_or_else(|| format!("{name} is not set"))
}

impl Tiles {
    /// Opens the store named by the environment, or `None` when there is none.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Ok(bucket) = var("TUILE_TILES_BUCKET") else { return Ok(None) };
        let s3 = AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_endpoint(var("TUILE_STORE_ENDPOINT")?)
            .with_region(std::env::var("TUILE_STORE_REGION").unwrap_or_else(|_| DEFAULT_REGION.into()))
            .with_access_key_id(var("TUILE_STORE_ACCESS_KEY_ID")?)
            .with_secret_access_key(var("TUILE_STORE_SECRET_ACCESS_KEY")?)
            // The store's manifests are replaced by conditional writes only.
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("tile store runtime: {e}"))?;
        let store = runtime
            .block_on(TileStore::open(Arc::new(s3), StoreConfig::default()))
            .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
        tracing::info!(bucket, layers = store.layers().count(), "TILES-OPEN");
        Ok(Some(Self { runtime, store: Arc::new(store) }))
    }

    /// Says loudly when a source has no layer in the store: its tiles would
    /// silently not be kept.
    pub fn check(&self, namespaces: &[String]) {
        for ns in namespaces {
            if self.store.layer(ns).is_err() {
                tracing::warn!(layer = %ns, "TILES-NO-LAYER: this source's tiles are fetched but not kept");
            }
        }
    }

    pub fn cache(&self) -> tuile_bake::TileCache {
        tuile_bake::TileCache(Arc::new(StoreContent::new(self.store.clone())))
    }

    /// Publishes whatever is still buffered. Called on the way out, success or
    /// not: tiles fetched by a failed bake are still worth keeping.
    pub fn flush(&self) -> Result<(), String> {
        let zones = self.runtime.block_on(self.store.flush_all()).map_err(|e| format!("tile store flush: {e}"))?;
        tracing::info!(zones, "TILES-FLUSHED");
        Ok(())
    }
}
