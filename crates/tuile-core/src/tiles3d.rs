// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The 3D Tiles binding of the source seam: a [`Tileset`] arena driven as a
//! [`TileTree`] + [`TileLoader`] pair for the [`crate::runtime::GeometryServer`].
//!
//! The two share one `Arc<RwLock<Tileset>>`: [`TilesetTree`] reads it every
//! frame for the traversal, [`TilesetLoader`] fetches + decodes content and —
//! for an external tileset — *grafts* its root into the same arena (a brief
//! write lock). The traversal then sees the new children. No lock is ever held
//! across an `.await`: the loader reads what it needs, releases, fetches, then
//! re-locks to decode or graft.

use crate::content::{self, ContentHints, DecodedTileContent};
use crate::fetch::TileFetcher;
use crate::source::{LoadError, Loaded, TileId, TileLoader, TileProperties, TileTree};
use crate::tileset::{ContentKind, Tileset};
use async_trait::async_trait;
use std::sync::{Arc, RwLock};

/// Shared 3D Tiles arena: cheaply cloned, read by the tree, grown by the loader.
pub type SharedTileset = Arc<RwLock<Tileset>>;

/// A [`Tileset`] arena exposed as a [`TileTree`]. Read-locks the arena for each
/// topology query; the arena may grow under it as external tilesets graft in.
pub struct TilesetTree {
    arena: SharedTileset,
}

impl TilesetTree {
    pub fn new(arena: SharedTileset) -> Self {
        Self { arena }
    }
}

impl TileTree for TilesetTree {
    fn roots(&self) -> Vec<TileId> {
        self.arena.read().expect("tileset lock").roots()
    }

    fn children(&self, id: TileId) -> Vec<TileId> {
        self.arena.read().expect("tileset lock").children(id)
    }

    fn properties(&self, id: TileId) -> TileProperties {
        self.arena.read().expect("tileset lock").properties(id)
    }

    /// Forwarded, like everything else here. Omitting it left the arena's own
    /// parent links unreachable and the server unable to pin an ancestor chain
    /// — see [`Tileset::parent`] for what that cost.
    fn parent(&self, id: TileId) -> Option<TileId> {
        self.arena.read().expect("tileset lock").parent(id)
    }

    fn level(&self, id: TileId) -> u32 {
        self.arena.read().expect("tileset lock").level(id)
    }
}

/// Fetches + decodes 3D Tiles content for the shared arena. Binary content
/// (glb/b3dm) decodes to [`Loaded::Content`]; an external tileset is grafted
/// into the arena and reported as [`Loaded::Expanded`].
pub struct TilesetLoader<F: TileFetcher> {
    arena: SharedTileset,
    fetcher: Arc<F>,
}

impl<F: TileFetcher> TilesetLoader<F> {
    pub fn new(arena: SharedTileset, fetcher: Arc<F>) -> Self {
        Self { arena, fetcher }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<F: TileFetcher + 'static> TileLoader for TilesetLoader<F> {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        // Read the content ref, then drop the guard before awaiting.
        let content = self
            .arena
            .read()
            .expect("tileset lock")
            .tile(id)
            .content
            .clone();
        let Some(content) = content else {
            // Traversal only requests tiles with content; a None here means
            // the tile was consumed (grafted) between request and load.
            return Ok(Loaded::Expanded);
        };

        let bytes = self
            .fetcher
            .fetch(&content.url)
            .await
            .map_err(|e| LoadError::Failed(e.to_string()))?;

        match content.kind {
            ContentKind::ExternalTileset => {
                let json =
                    serde_json::from_slice(&bytes).map_err(|e| LoadError::Failed(e.to_string()))?;
                self.arena
                    .write()
                    .expect("tileset lock")
                    .graft(id, json, &content.url)
                    .map_err(|e| LoadError::Failed(e.to_string()))?;
                Ok(Loaded::Expanded)
            }
            ContentKind::Binary => {
                let decoded = self.decode_binary(id, &bytes)?;
                Ok(Loaded::Content(decoded))
            }
        }
    }
}

impl<F: TileFetcher> TilesetLoader<F> {
    fn decode_binary(&self, id: TileId, bytes: &[u8]) -> Result<DecodedTileContent, LoadError> {
        let hints = {
            let g = self.arena.read().expect("tileset lock");
            let t = g.tile(id);
            ContentHints {
                world_transform: t.world_transform,
                origin_ecef: t.bounding_volume.center(),
            }
        };
        content::decode(bytes, &hints).map_err(|e| LoadError::Failed(e.to_string()))
    }
}
