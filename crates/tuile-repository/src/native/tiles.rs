// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use async_trait::async_trait;
use tuile_tile_server::{StoreError, TileStore};

use crate::{LayerInfo, RepoError, Tile, TileRepository};

/// The tile store, read straight: `TileStore::get` and nothing else. No
/// upstream is involved, so a miss stays a miss and the bucket is not
/// written to.
#[async_trait]
impl TileRepository for TileStore {
    fn layers(&self) -> Vec<LayerInfo> {
        TileStore::layers(self)
            .map(|l| LayerInfo {
                name: l.name.clone(),
                grid: serde_json::to_value(l.grid)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                max_level: l.grid.max_level(),
                content_type: l.content_type.clone(),
            })
            .collect()
    }

    async fn tile(
        &self,
        layer: &str,
        level: u8,
        x: u32,
        y: u32,
    ) -> Result<Option<Tile>, RepoError> {
        let content_type = match self.layer(layer) {
            Ok(l) => l.content_type.clone(),
            Err(_) => return Err(RepoError::NotFound(format!("layer {layer}"))),
        };
        match self.get(layer, level, x, y).await {
            Ok(Some(bytes)) => Ok(Some(Tile {
                bytes: bytes.to_vec(),
                content_type,
            })),
            Ok(None) => Ok(None),
            Err(e @ StoreError::OutOfGrid(_)) => Err(RepoError::NotFound(e.to_string())),
            Err(e) => Err(RepoError::Store(e.to_string())),
        }
    }
}
