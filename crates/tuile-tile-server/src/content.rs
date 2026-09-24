// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The store as the engine's tile cache.
//!
//! The engine caches source tiles through [`ContentStore`], wrapping a terrain
//! source in `CachedTerrain` and an imagery provider in `CachedImagery`; both
//! key by tile address — `mesh/{namespace}/{z}/{x}/{y}` and
//! `img/{namespace}/{z}/{x}/{y}`. Here the namespace **is** the layer name, so
//! a bake that caches through this writes the very archives a tile server
//! serves, and reads what any other process stored before it.
//!
//! An absence (the engine's empty value) goes to the layer's absence sibling;
//! a failure is a miss, never an error, as the trait requires. Nothing reaches
//! the bucket until the store is flushed: whoever owns the [`TileStore`] must
//! call [`TileStore::flush_all`] before it exits.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::storage::{is_absent, ContentStore, ABSENT};

use crate::catalog::{ABSENT_MARKER, ABSENT_SUFFIX};
use crate::store::TileStore;

/// A [`ContentStore`] over a [`TileStore`].
pub struct StoreContent {
    store: Arc<TileStore>,
}

impl StoreContent {
    pub fn new(store: Arc<TileStore>) -> Self {
        Self { store }
    }
}

/// `mesh/{layer}/{z}/{x}/{y}` or `img/{layer}/{z}/{x}/{y}` → its parts.
fn parse(key: &str) -> Option<(&str, u8, u32, u32)> {
    let mut parts = key.split('/');
    let kind = parts.next()?;
    if kind != "mesh" && kind != "img" {
        return None;
    }
    let layer = parts.next()?;
    let z = parts.next()?.parse().ok()?;
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((layer, z, x, y))
}

#[async_trait]
impl ContentStore for StoreContent {
    async fn get(&self, key: &str) -> Option<Bytes> {
        let (layer, z, x, y) = parse(key)?;
        match self.store.get(layer, z, x, y).await {
            Ok(Some(bytes)) => return Some(bytes),
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(key, error = %e, "tile store read failed; treated as a miss");
                return None;
            }
        }
        let absent = format!("{layer}{ABSENT_SUFFIX}");
        match self.store.get(&absent, z, x, y).await {
            Ok(Some(_)) => Some(ABSENT),
            _ => None,
        }
    }

    async fn put(&self, key: &str, value: Bytes, _ttl: Option<Duration>) {
        // The layer's own lifetime governs, not the origin's TTL: a layer
        // expires by epoch, as the catalog says.
        let Some((layer, z, x, y)) = parse(key) else { return };
        let result = if is_absent(&value) {
            let absent = format!("{layer}{ABSENT_SUFFIX}");
            self.store.put(&absent, z, x, y, Bytes::from_static(ABSENT_MARKER)).await
        } else {
            self.store.put(layer, z, x, y, value).await
        };
        if let Err(e) = result {
            tracing::debug!(key, error = %e, "tile store write declined");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn keys_are_the_engine_s_own() {
        assert_eq!(parse("mesh/asset-1/12/4100/2900"), Some(("asset-1", 12, 4100, 2900)));
        assert_eq!(parse("img/asset-2/14/8352/5968"), Some(("asset-2", 14, 8352, 5968)));
        assert_eq!(parse("other/asset-2/14/1/1"), None);
        assert_eq!(parse("img/asset-2/14/1"), None);
        assert_eq!(parse("img/asset-2/14/1/1/extra"), None);
        assert_eq!(parse("img/asset-2/99999/1/1"), None, "level out of range");
    }
}
