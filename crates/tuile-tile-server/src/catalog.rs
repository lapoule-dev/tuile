// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The layers of a bucket, written in the bucket.
//!
//! Every process that reads or writes a store — a server, a bake job — must
//! agree on what its layers are: grid, zone level, lifetime. Copies of that in
//! each program's configuration would drift. So a store describes itself: a
//! `catalog.json` at its root, read by [`TileStore::open`], and written once
//! when a layer is created.
//!
//! Each layer also gets an **absence** sibling, derived, never listed: a
//! source's "no such tile" is worth remembering (a coarse pyramid over oceans
//! is mostly holes, asked again on every run otherwise), but an archive cannot
//! hold an empty tile. The sibling holds a one-byte marker per absent tile, so
//! the layer's own archives stay pure images or meshes that any viewer reads.

use std::sync::Arc;
use std::time::Duration;

use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde::{Deserialize, Serialize};

use crate::grid::Grid;
use crate::layer::Layer;
use crate::store::{StoreConfig, TileStore};
use crate::{Compression, StoreError, TileType};

/// Where the catalog lives in a bucket.
pub const CATALOG_KEY: &str = "catalog.json";
/// The suffix of a layer's absence sibling.
pub const ABSENT_SUFFIX: &str = ".absent";
/// What an absence is stored as.
pub const ABSENT_MARKER: &[u8] = &[0];

const SECONDS_PER_DAY: u64 = 24 * 3600;

/// One layer, as the catalog writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerDef {
    pub name: String,
    pub grid: Grid,
    /// `jpeg`, `png`, `webp`, `avif`, `mvt`, or `other`.
    pub tile_type: String,
    /// `none`, `gzip`, `brotli` or `zstd`: how the stored bytes are compressed.
    pub tile_compression: String,
    pub zone_level: u8,
    /// Days before a tile is fetched again; absent means never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_days: Option<u64>,
    pub content_type: String,
}

/// A bucket's layers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub layers: Vec<LayerDef>,
}

fn tile_type(s: &str) -> Result<TileType, StoreError> {
    Ok(match s {
        "jpeg" => TileType::Jpeg,
        "png" => TileType::Png,
        "webp" => TileType::Webp,
        "avif" => TileType::Avif,
        "mvt" => TileType::Mvt,
        "other" => TileType::Unknown,
        other => return Err(StoreError::Corrupt(format!("catalog: unknown tile type {other}"))),
    })
}

fn compression(s: &str) -> Result<Compression, StoreError> {
    Ok(match s {
        "none" => Compression::None,
        "gzip" => Compression::Gzip,
        "brotli" => Compression::Brotli,
        "zstd" => Compression::Zstd,
        other => return Err(StoreError::Corrupt(format!("catalog: unknown compression {other}"))),
    })
}

impl LayerDef {
    pub fn to_layer(&self) -> Result<Layer, StoreError> {
        Ok(Layer {
            name: self.name.clone(),
            grid: self.grid,
            tile_type: tile_type(&self.tile_type)?,
            tile_compression: compression(&self.tile_compression)?,
            zone_level: self.zone_level,
            expiry: self.expiry_days.map(|d| Duration::from_secs(d * SECONDS_PER_DAY)),
            content_type: self.content_type.clone(),
        })
    }
}

/// A layer's absence sibling: same grid, zones and lifetime.
pub fn absence_of(layer: &Layer) -> Layer {
    Layer {
        name: format!("{}{ABSENT_SUFFIX}", layer.name),
        tile_type: TileType::Unknown,
        tile_compression: Compression::None,
        content_type: "application/octet-stream".into(),
        ..layer.clone()
    }
}

/// Every layer a catalog implies: each listed layer and its absence sibling.
pub fn layers(catalog: &Catalog) -> Result<Vec<Layer>, StoreError> {
    let mut out = Vec::new();
    for def in &catalog.layers {
        if def.name.ends_with(ABSENT_SUFFIX) {
            return Err(StoreError::Corrupt(format!("catalog: {} uses a reserved suffix", def.name)));
        }
        let layer = def.to_layer()?;
        out.push(absence_of(&layer));
        out.push(layer);
    }
    Ok(out)
}

/// Reads a bucket's catalog.
pub async fn read(store: &dyn ObjectStore) -> Result<Catalog, StoreError> {
    let got = store.get(&Path::from(CATALOG_KEY)).await?;
    let bytes = got.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt(format!("{CATALOG_KEY}: {e}")))
}

/// Writes a bucket's catalog. Replaces the whole list: layers are few and
/// created by hand.
pub async fn write(store: &dyn ObjectStore, catalog: &Catalog) -> Result<(), StoreError> {
    let body = serde_json::to_vec_pretty(catalog).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    store.put(&Path::from(CATALOG_KEY), PutPayload::from(body)).await?;
    Ok(())
}

impl TileStore {
    /// Opens a store on the layers its bucket's catalog lists.
    pub async fn open(store: Arc<dyn ObjectStore>, cfg: StoreConfig) -> Result<Self, StoreError> {
        let catalog = read(store.as_ref()).await?;
        Ok(TileStore::new(store, layers(&catalog)?, cfg))
    }
}
