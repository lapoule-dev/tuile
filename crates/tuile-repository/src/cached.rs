// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures_util::lock::Mutex;

use crate::{Entry, Listing, Objects, RepoError};

/// Eight megabytes: a frame's newcomers usually fit in one or two, a bucket
/// answers a ranged GET of this size in little more than its latency, and a
/// host with little memory can still hold a few at once.
pub const CHUNK: u64 = 8 << 20;

/// Where chunks that have been read are kept.
///
/// A store may lose what it was given — a cache does — and answer `None` for
/// it later: the chunk is then read again. It must never answer with bytes
/// other than those it was given for that key and index.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait ChunkStore: Send + Sync {
    async fn get(&self, key: &str, index: u64) -> Option<Vec<u8>>;
    async fn put(&self, key: &str, index: u64, bytes: &[u8]);
    /// The object's size, if it was noted.
    async fn size(&self, key: &str) -> Option<u64>;
    async fn note_size(&self, key: &str, size: u64);
}

/// Any [`Objects`], with every chunk it has read kept in a [`ChunkStore`].
///
/// A pack runs to gigabytes and a renderer reads a few megabytes of it per
/// frame, so nothing is downloaded whole: an object is fetched in fixed
/// chunks, on demand, each chunk once, and every later read of those bytes
/// comes from the store — a directory for a native process, the edge cache
/// for a Worker. Whatever range is asked for, and however differently the
/// next client cuts the same film, the same chunks answer.
///
/// Chunks are keyed by the object's key alone, which is sound for what a film
/// is made of: a pack's key hashes its inputs, a run's prefix carries its
/// timestamp, the tile store names every archive afresh. Listings are not
/// cached: what a bucket holds does change.
pub struct Cached {
    inner: Arc<dyn Objects>,
    store: Box<dyn ChunkStore>,
    /// One lock per chunk: two readers after the same bytes wait for one
    /// fetch rather than starting two.
    fetching: StdMutex<HashMap<(String, u64), Arc<Mutex<()>>>>,
}

/// The chunks a byte range touches.
fn covering(range: &Range<u64>) -> Range<u64> {
    range.start / CHUNK..range.end.div_ceil(CHUNK)
}

impl Cached {
    pub fn new(inner: Arc<dyn Objects>, store: impl ChunkStore + 'static) -> Self {
        Self {
            inner,
            store: Box::new(store),
            fetching: StdMutex::default(),
        }
    }

    async fn chunk(&self, key: &str, size: u64, index: u64) -> Result<Vec<u8>, RepoError> {
        // A chunk is kept under its size as well as its key: cut another
        // way, the same number would be other bytes.
        let kept = format!("{}m/{key}", CHUNK >> 20);
        if let Some(bytes) = self.store.get(&kept, index).await {
            return Ok(bytes);
        }
        let lock = match self.fetching.lock() {
            Ok(mut map) => map.entry((key.to_string(), index)).or_default().clone(),
            Err(poisoned) => poisoned
                .into_inner()
                .entry((key.to_string(), index))
                .or_default()
                .clone(),
        };
        let _held = lock.lock().await;
        if let Some(bytes) = self.store.get(&kept, index).await {
            return Ok(bytes);
        }
        let bytes = self
            .inner
            .read(key, index * CHUNK..((index + 1) * CHUNK).min(size))
            .await?;
        self.store.put(&kept, index, &bytes).await;
        Ok(bytes)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Objects for Cached {
    fn label(&self) -> String {
        self.inner.label()
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        self.inner.list(prefix).await
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        self.inner.browse(prefix).await
    }

    /// Asked of the bucket once per object.
    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        if let Some(size) = self.store.size(key).await {
            return Ok(size);
        }
        let size = self.inner.size(key).await?;
        self.store.note_size(key, size).await;
        Ok(size)
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let size = self.size(key).await?;
        if range.start > range.end || range.end > size {
            return Err(RepoError::Store(format!(
                "{key}: bytes {}..{} are outside its {size}",
                range.start, range.end
            )));
        }
        let chunks = covering(&range);
        // Exactly one whole chunk — what an aligned reader asks for — is
        // handed back as it is: no second copy of the chunk.
        if chunks.end - chunks.start == 1
            && range.start == chunks.start * CHUNK
            && range.end == ((chunks.start + 1) * CHUNK).min(size)
        {
            return self.chunk(key, size, chunks.start).await;
        }
        let parts =
            futures_util::future::try_join_all(chunks.clone().map(|i| self.chunk(key, size, i)))
                .await?;
        let mut out = Vec::with_capacity((range.end - range.start) as usize);
        for (index, part) in chunks.zip(parts) {
            let base = index * CHUNK;
            let from = range.start.max(base) - base;
            let to = range.end.min(base + part.len() as u64) - base;
            out.extend_from_slice(&part[from as usize..to as usize]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_range_touches_exactly_its_chunks() {
        assert_eq!(covering(&(0..1)), 0..1);
        assert_eq!(covering(&(CHUNK - 1..CHUNK + 1)), 0..2);
        assert_eq!(covering(&(CHUNK..2 * CHUNK)), 1..2);
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// One object, and a count of every read made of it.
    struct Bucket {
        object: Vec<u8>,
        reads: AtomicUsize,
        sizes: AtomicUsize,
    }

    #[async_trait]
    impl Objects for Bucket {
        fn label(&self) -> String {
            "test".into()
        }
        async fn list(&self, _: &str) -> Result<Vec<Entry>, RepoError> {
            Ok(Vec::new())
        }
        async fn browse(&self, _: &str) -> Result<Listing, RepoError> {
            Ok(Listing::default())
        }
        async fn size(&self, _: &str) -> Result<u64, RepoError> {
            self.sizes.fetch_add(1, Ordering::Relaxed);
            Ok(self.object.len() as u64)
        }
        async fn read(&self, _: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.object[range.start as usize..range.end as usize].to_vec())
        }
    }

    #[derive(Default)]
    struct Memory {
        chunks: StdMutex<HashMap<(String, u64), Vec<u8>>>,
        sizes: StdMutex<HashMap<String, u64>>,
    }

    #[async_trait]
    impl ChunkStore for Memory {
        async fn get(&self, key: &str, index: u64) -> Option<Vec<u8>> {
            self.chunks
                .lock()
                .expect("lock")
                .get(&(key.to_string(), index))
                .cloned()
        }
        async fn put(&self, key: &str, index: u64, bytes: &[u8]) {
            self.chunks
                .lock()
                .expect("lock")
                .insert((key.to_string(), index), bytes.to_vec());
        }
        async fn size(&self, key: &str) -> Option<u64> {
            self.sizes.lock().expect("lock").get(key).copied()
        }
        async fn note_size(&self, key: &str, size: u64) {
            self.sizes
                .lock()
                .expect("lock")
                .insert(key.to_string(), size);
        }
    }

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        futures_executor::block_on(f)
    }

    /// **No byte is read from the bucket twice** — whatever ranges are asked
    /// for, in whatever order, and however they overlap.
    #[test]
    fn every_chunk_is_read_from_the_bucket_once() {
        let object = noise(2 * CHUNK as usize + 12_345);
        let bucket = Arc::new(Bucket {
            object: object.clone(),
            reads: AtomicUsize::new(0),
            sizes: AtomicUsize::new(0),
        });
        let cached = Cached::new(bucket.clone(), Memory::default());
        let c = CHUNK as usize;
        let ranges = [
            0..16,
            c - 3..c + 5,
            2 * c..object.len(),
            100..2 * c + 12_000,
            7..9,
            0..object.len(),
        ];
        for range in ranges {
            let got = block(cached.read("k", range.start as u64..range.end as u64)).expect("read");
            assert_eq!(got, object[range.clone()], "range {range:?}");
        }
        assert_eq!(
            bucket.reads.load(Ordering::Relaxed),
            3,
            "three chunks, three reads, ever"
        );
        assert_eq!(
            bucket.sizes.load(Ordering::Relaxed),
            1,
            "and its size asked once"
        );
        assert!(block(cached.read("k", 0..object.len() as u64 + 1)).is_err());
    }

    /// Readers after the same chunk at the same moment share one fetch.
    #[test]
    fn concurrent_readers_of_a_chunk_fetch_it_once() {
        let object = noise(1000);
        let bucket = Arc::new(Bucket {
            object: object.clone(),
            reads: AtomicUsize::new(0),
            sizes: AtomicUsize::new(0),
        });
        let cached = Cached::new(bucket.clone(), Memory::default());
        let all = block(futures_util::future::join_all(
            (0..8).map(|i| cached.read("k", i..i + 100)),
        ));
        for (i, got) in all.into_iter().enumerate() {
            assert_eq!(got.expect("read"), object[i..i + 100]);
        }
        assert_eq!(bucket.reads.load(Ordering::Relaxed), 1);
    }

    /// A store that forgets is a cache, not a fault: the chunk is read again.
    #[test]
    fn a_store_that_forgets_costs_a_read_and_nothing_else() {
        struct Amnesia;
        #[async_trait]
        impl ChunkStore for Amnesia {
            async fn get(&self, _: &str, _: u64) -> Option<Vec<u8>> {
                None
            }
            async fn put(&self, _: &str, _: u64, _: &[u8]) {}
            async fn size(&self, _: &str) -> Option<u64> {
                None
            }
            async fn note_size(&self, _: &str, _: u64) {}
        }
        let object = noise(500);
        let bucket = Arc::new(Bucket {
            object: object.clone(),
            reads: AtomicUsize::new(0),
            sizes: AtomicUsize::new(0),
        });
        let cached = Cached::new(bucket.clone(), Amnesia);
        for _ in 0..2 {
            assert_eq!(
                block(cached.read("k", 10..20)).expect("read"),
                object[10..20]
            );
        }
        assert_eq!(bucket.reads.load(Ordering::Relaxed), 2);
    }
}
