// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The terrain-source seam: an abstract provider of raw `.terrain` tile bytes,
//! keyed by quadtree coordinate. This crate stays I/O-free — it defines the
//! interface; an implementor (e.g. a Cesium-ion connector, a local file store,
//! an HTTP CDN) supplies the bytes. `tuile-planetary` consumes this trait, so
//! it never depends on any particular terrain backend or transport.

use crate::tiling::TileCoord;
use async_trait::async_trait;

/// Supplies the raw bytes of a quantized-mesh terrain tile. The bytes may be
/// gzipped — [`crate::decode`] handles that. Transport- and backend-agnostic:
/// the implementor decides HTTP, disk, ion bearer auth, etc.
#[async_trait]
pub trait TerrainSource: Send + Sync {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Vec<u8>, TerrainSourceError>;
}

#[derive(Debug, thiserror::Error)]
#[error("terrain source: {0}")]
pub struct TerrainSourceError(pub String);
