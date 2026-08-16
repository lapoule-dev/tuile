// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The terrain-source seam: an abstract provider of raw `.terrain` tile bytes,
//! keyed by quadtree coordinate. This crate stays I/O-free — it defines the
//! interface; an implementor (e.g. a Cesium-ion connector, a local file store,
//! an HTTP CDN) supplies the bytes. `tuile-planetary` consumes this trait, so
//! it never depends on any particular terrain backend or transport.

use async_trait::async_trait;

use std::sync::Arc;

use bytes::Bytes;

use crate::tiling::TileCoord;
use tuile_core::fetch::Fetched;
use tuile_core::storage::ContentStore;

/// Supplies the raw bytes of a quantized-mesh terrain tile. The bytes may be
/// gzipped — [`crate::decode`](mod@crate::decode) handles that. Transport- and
/// backend-agnostic:
/// the implementor decides HTTP, disk, ion bearer auth, etc.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait TerrainSource: Send + Sync {
    /// The tile's bytes, with the lifetime its origin stated — a decoded mesh
    /// is worth storing, and only the origin knows for how long.
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError>;
}

#[derive(Debug, thiserror::Error)]
#[error("terrain source: {0}")]
pub struct TerrainSourceError(pub String, pub Absence);

/// Whether a failure means "this tile does not exist" or "something went wrong".
///
/// The distinction is not cosmetic: an absence is a **fact about the data** and
/// is worth remembering, while a timeout or a torn connection is a fact about
/// this moment and must never be. Collapsing the two — which a plain string
/// does — means either re-asking for tiles that have never existed, on every
/// run for ever, or caching a network hiccup as though the planet had a hole in
/// it. Measured before this existed: 1630 of 4094 tiles came back missing on a
/// warm-up and *the same 1630* on the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    /// The source answered, and the answer was "there is nothing here".
    NoSuchTile,
    /// Anything else: a transport failure, a malformed payload, an auth error.
    Failed,
}

impl TerrainSourceError {
    /// A failure that says nothing about whether the tile exists.
    pub fn failed(message: impl Into<String>) -> Self {
        Self(message.into(), Absence::Failed)
    }

    /// The source's own statement that this tile does not exist.
    pub fn no_such_tile(message: impl Into<String>) -> Self {
        Self(message.into(), Absence::NoSuchTile)
    }

    /// Whether this is worth writing to a store.
    pub fn is_absence(&self) -> bool {
        self.1 == Absence::NoSuchTile
    }
}

/// Wraps a [`TerrainSource`] with a [`ContentStore`], so a tile served once is
/// not fetched again — across runs, if the store is persistent.
///
/// Caches the `.terrain` payload **as served**, never a decoded mesh. Beyond
/// the size difference, this is what keeps refinement correct: the quantized
/// mesh's `metadata` extension is what reveals a tile's descendants, and it is
/// recovered by decoding. A cache of decoded meshes would hand back geometry
/// with nothing to say about what lies below it, and the traversal would stall
/// at whatever depth the cache was read from.
///
/// Keys are tile coordinates, not URLs. A source whose URLs carry a rotating
/// access token would otherwise miss on every token refresh, for bytes that
/// never changed.
pub struct CachedTerrain<T> {
    inner: T,
    store: Arc<dyn ContentStore>,
    namespace: String,
}

impl<T: TerrainSource> CachedTerrain<T> {
    pub fn new(inner: T, store: Arc<dyn ContentStore>, namespace: impl Into<String>) -> Self {
        Self {
            inner,
            store,
            namespace: namespace.into(),
        }
    }

    fn key(&self, c: TileCoord) -> String {
        let TileCoord { level, x, y } = c;
        format!("mesh/{}/{level}/{x}/{y}", self.namespace)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<T: TerrainSource> TerrainSource for CachedTerrain<T> {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        let key = self.key(coord);
        let m = tuile_core::metrics::metrics();
        if let Some(bytes) = self.store.get(&key).await {
            m.store_hits.inc();
            // An absence remembered is still a hit: it answered without the
            // network, which is why it was stored.
            if tuile_core::storage::is_absent(&bytes) {
                return Err(TerrainSourceError::no_such_tile(format!(
                    "{}/{}/{} is not served",
                    coord.level, coord.x, coord.y
                )));
            }
            m.store_bytes_served.add(bytes.len() as u64);
            // The store already applied the lifetime this was written with.
            return Ok(Fetched::undated(bytes.to_vec()));
        }
        m.store_misses.inc();
        match self.inner.fetch_tile(coord).await {
            Ok(fetched) => {
                m.store_bytes_fetched.add(fetched.value.len() as u64);
                self.store
                    .put(&key, Bytes::from(fetched.value.clone()), fetched.ttl)
                    .await;
                Ok(fetched)
            }
            // The source says there is nothing here. Remembered, or every run
            // asks again — and over a coarse pyramid spanning oceans and poles
            // that is most of what a warm-up does: measured, 1630 of 4094 tiles
            // were missing on the first pass and *the same 1630* on the second.
            Err(e) if e.is_absence() => {
                m.terrain_absent.inc();
                tracing::debug!(z = coord.level, x = coord.x, y = coord.y, "terrain absent");
                self.store
                    .put(
                        &key,
                        tuile_core::storage::ABSENT,
                        Some(tuile_core::storage::ABSENCE_TTL),
                    )
                    .await;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tuile_core::storage::ContentStore;

    #[derive(Default)]
    struct MemStore {
        entries: Mutex<HashMap<String, Bytes>>,
        writes: Mutex<Vec<(String, Option<std::time::Duration>)>>,
    }

    #[async_trait]
    impl ContentStore for MemStore {
        async fn get(&self, key: &str) -> Option<Bytes> {
            self.entries.lock().expect("lock").get(key).cloned()
        }
        async fn put(&self, key: &str, value: Bytes, ttl: Option<std::time::Duration>) {
            self.entries
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value);
            self.writes
                .lock()
                .expect("lock")
                .push((key.to_owned(), ttl));
        }
    }

    struct CountingTerrain {
        calls: Mutex<u32>,
        ttl: Option<std::time::Duration>,
    }

    #[async_trait]
    impl TerrainSource for CountingTerrain {
        async fn fetch_tile(&self, _c: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
            *self.calls.lock().expect("lock") += 1;
            Ok(Fetched {
                value: b"quantized-mesh".to_vec(),
                ttl: self.ttl,
            })
        }
    }

    fn cached(
        store: Arc<MemStore>,
        ttl: Option<std::time::Duration>,
    ) -> CachedTerrain<CountingTerrain> {
        CachedTerrain::new(
            CountingTerrain {
                calls: Mutex::new(0),
                ttl,
            },
            store,
            "test",
        )
    }

    #[test]
    fn a_cached_source_asks_the_origin_once() {
        let store = Arc::new(MemStore::default());
        let source = cached(store, None);
        let c = TileCoord::new(3, 4, 5);

        let first = futures_executor::block_on(source.fetch_tile(c)).expect("first");
        let second = futures_executor::block_on(source.fetch_tile(c)).expect("second");

        assert_eq!(first.value, second.value);
        let calls = *source.inner.calls.lock().expect("lock");
        assert_eq!(calls, 1, "origin asked {calls} times");
    }

    /// The payload must be the `.terrain` bytes as served: decoding them is
    /// what recovers the `metadata` extension, and so what lets the traversal
    /// refine past a cached tile.
    #[test]
    fn a_cached_source_stores_the_payload_as_served_with_its_ttl() {
        let store = Arc::new(MemStore::default());
        let ttl = Some(std::time::Duration::from_secs(3600));
        let source = cached(Arc::clone(&store), ttl);
        futures_executor::block_on(source.fetch_tile(TileCoord::new(3, 4, 5))).expect("fetch");

        let writes = store.writes.lock().expect("lock");
        let (key, written_ttl) = writes.first().expect("one write");
        assert_eq!(*written_ttl, ttl, "the origin's lifetime is forwarded");
        assert_eq!(
            store.entries.lock().expect("lock")[key],
            Bytes::from_static(b"quantized-mesh")
        );
    }

    /// Keyed by coordinate, not URL — a source whose URLs carry a rotating
    /// token would otherwise miss on every refresh, for identical bytes.
    #[test]
    fn keys_are_coordinates_and_namespaced() {
        let store = Arc::new(MemStore::default());
        let source = cached(store, None);
        assert_ne!(
            source.key(TileCoord::new(3, 4, 5)),
            source.key(TileCoord::new(3, 4, 6))
        );
        assert!(source.key(TileCoord::new(3, 4, 5)).contains("test"));
    }
}
