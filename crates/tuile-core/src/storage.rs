// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The content-store seam: a byte-oriented cache a loader can consult before
//! doing expensive work, and populate after.
//!
//! This is a **trait only**. The core neither opens files nor allocates a cache
//! — it states the contract and leaves the tiers to the host, exactly as it
//! does for [`TileFetcher`](crate::fetch::TileFetcher). Native hosts back it
//! with memory + disk (`tuile-storage-foyer`); a browser host can back the same
//! trait with IndexedDB or the Cache API, or simply pass no store at all.
//!
//! # Why bytes
//!
//! Callers cache payloads **as served** — an encoded image, a quantized-mesh
//! tile — not decoded ones. Decoded is an order of magnitude larger (a 20 KiB
//! JPEG becomes 256 KiB of RGBA) and it drops what decoding reveals: a terrain
//! tile's `metadata` extension is what tells a traversal its descendants exist,
//! and a cache of decoded meshes would hand back geometry with nothing to say
//! about what lies below it. Storing the bytes and decoding on the way out
//! costs microseconds and keeps both properties.
//!
//! So the store stays dumb: opaque bytes under caller-chosen keys. That is what
//! makes it pluggable — see `tuile_core::raster::CachedImagery` and
//! `tuile_terrain::CachedTerrain` for the two callers that use it.

use bytes::Bytes;
use std::time::Duration;

/// A byte cache keyed by opaque strings.
///
/// Implementations are caches, not databases: a `get` may miss at any time (an
/// eviction, a cold start, a store that silently dropped the entry), and every
/// caller must be able to recompute the value. Failures are misses — a store
/// that cannot answer must not fail the load that consulted it.
///
/// Keys are namespaced by the caller. A persistent store outlives the dataset
/// that filled it, so include enough to keep two datasets apart (the source,
/// the layer version, the tile coordinate).
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait ContentStore: Send + Sync {
    /// The stored bytes, or `None` on any miss.
    async fn get(&self, key: &str) -> Option<Bytes>;

    /// Offers bytes to the store, to be served for at most `ttl`.
    ///
    /// Best effort: a store may decline (too large, disk full, read-only) and
    /// the caller carries on regardless. `ttl` is the origin's own statement,
    /// forwarded unchanged — see [`Fetched`](crate::fetch::Fetched). `None`
    /// means the origin said nothing, and the store applies its own policy;
    /// it does not mean "do not store".
    ///
    /// Expiry is the store's business, not the core's: reading a clock needs a
    /// platform, and this crate compiles to wasm.
    async fn put(&self, key: &str, value: Bytes, ttl: Option<Duration>);
}

/// How long an origin's "there is nothing here" is believed.
///
/// A week, the same as real content. A basemap's coverage does not change on a
/// shorter horizon than its pixels do, and the levels this actually bites at —
/// the coarse pyramid over oceans and poles — will not be published at all.
/// Treating an absence as more perishable than a presence would only mean
/// re-asking for tiles that have never existed.
pub const ABSENCE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The sentinel an absence is stored as.
///
/// Empty, which is unambiguous: no imagery tile and no quantized mesh is zero
/// bytes, so a stored empty value can only mean one thing. Storing it at all is
/// the point — a 404 that is not remembered is asked again on every run, for
/// ever. Measured over a warm-up of the coarse pyramid: 1630 of 4094 tiles came
/// back missing on the first pass and *the same 1630* on the second, because
/// only successes were being kept.
pub const ABSENT: Bytes = Bytes::from_static(b"");

/// Whether a stored value is the absence marker rather than content.
pub fn is_absent(value: &Bytes) -> bool {
    value.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStore {
        entries: Mutex<HashMap<String, Bytes>>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl ContentStore for MemStore {
        async fn get(&self, key: &str) -> Option<Bytes> {
            self.entries.lock().expect("lock").get(key).cloned()
        }
        async fn put(&self, key: &str, value: Bytes, _ttl: Option<Duration>) {
            self.entries
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value);
        }
    }

    #[test]
    fn the_trait_is_object_safe() {
        // The whole point of the seam: a host swaps one implementation for
        // another (foyer on native, IndexedDB in a browser) behind one Arc.
        let store: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(MemStore::default());
        futures_executor::block_on(async {
            assert!(store.get("absent").await.is_none());
            store.put("k", Bytes::from_static(b"v"), None).await;
            assert_eq!(store.get("k").await, Some(Bytes::from_static(b"v")));
        });
    }
}
