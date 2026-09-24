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
use tuile_tile_server::projection::DEFAULT_TILE_FACTOR;
use tuile_tile_server::{Eye, Footprint, StoreConfig, StoreContent, TileStore};

const DEFAULT_REGION: &str = "auto";
/// Where the scene's projections are written, unless `TUILE_TILES_DIR` says.
const DEFAULT_PROJECTION_DIR: &str = "/tmp/tuile-tiles";
/// A bake has no use for other writers' newest deltas: it reads its scene
/// from its projections, and its own tiles from memory.
const BAKE_MANIFEST_TTL: std::time::Duration = std::time::Duration::from_secs(60);

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
            .block_on(TileStore::open(
                Arc::new(s3),
                StoreConfig { manifest_ttl: BAKE_MANIFEST_TTL, ..StoreConfig::default() },
            ))
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

    /// Projects the part of the store the scene's cameras can use into local
    /// archives, read before the bucket for the rest of the bake.
    pub fn project(&self, poses: &[tuile_tape::Frame]) -> Result<(), String> {
        let eyes = poses.iter().map(|p| {
            let g = tuile_core::geo::ecef_to_geodetic(glam::DVec3::from_array(p.position));
            Eye { lon: g.lon.to_degrees(), lat: g.lat.to_degrees(), height: g.height }
        });
        let factor = std::env::var("TUILE_TILES_FACTOR").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TILE_FACTOR);
        let footprint = Footprint::from_eyes(eyes, factor);
        let dir = std::env::var("TUILE_TILES_DIR").unwrap_or_else(|_| DEFAULT_PROJECTION_DIR.into());
        let report = self
            .runtime
            .block_on(self.store.project(&footprint, std::path::Path::new(&dir)))
            .map_err(|e| format!("projecting the tile store: {e}"))?;
        tracing::info!(
            eyes = footprint.eyes().len(),
            factor,
            zones_seen = report.zones_seen,
            zones = report.zones_projected,
            tiles_listed = report.tiles_listed,
            tiles = report.tiles_kept,
            mb = report.bytes as f64 / 1e6,
            requests = report.requests,
            seconds = report.millis as f64 / 1e3,
            "TILES-PROJECTED"
        );
        Ok(())
    }

    pub fn cache(&self) -> tuile_bake::TileCache {
        tuile_bake::TileCache(Arc::new(StoreContent::new(self.store.clone())))
    }

    /// Publishes whatever is still buffered. Called on the way out, success or
    /// not: tiles fetched by a failed bake are still worth keeping.
    pub fn flush(&self) -> Result<(), String> {
        let stats = self.store.stats();
        tracing::info!(
            buffer = stats.buffer,
            projection = stats.projection,
            projection_misses = stats.projection_misses,
            remote = stats.remote,
            absent = stats.absent,
            "TILES-READS"
        );
        let began = std::time::Instant::now();
        let zones = self.runtime.block_on(self.store.flush_all()).map_err(|e| format!("tile store flush: {e}"))?;
        tracing::info!(zones, seconds = began.elapsed().as_secs_f64(), "TILES-FLUSHED");
        Ok(())
    }
}
