// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use async_trait::async_trait;

use crate::RepoError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LayerInfo {
    pub name: String,
    /// `geographic` or `web-mercator`.
    pub grid: String,
    pub max_level: u8,
    pub content_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// The source tiles a bake reads: terrain and imagery, as their sources
/// served them.
///
/// # Contract
///
/// - `tile` addresses a layer's own grid: level, column, row, as its source
///   numbers them.
/// - `Ok(None)` is a tile the store does not hold. It is never fetched from
///   the source on the way: reading does not fill the store.
/// - A layer `layers()` does not name is [`RepoError::NotFound`].
/// - Nothing is written.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait TileRepository: Send + Sync {
    fn layers(&self) -> Vec<LayerInfo>;
    async fn tile(&self, layer: &str, level: u8, x: u32, y: u32)
        -> Result<Option<Tile>, RepoError>;
}
