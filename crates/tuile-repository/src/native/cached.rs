// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::{Entry, Listing, Objects, RepoError};

/// Four megabytes: a frame's newcomers usually fit in one or two, and a
/// bucket answers a ranged GET of this size in about the time of its latency.
pub const CHUNK: u64 = 4 << 20;

/// Any [`Objects`], with every chunk it has read kept on disk.
///
/// A pack runs to gigabytes and a renderer reads a few megabytes of it per
/// frame, so nothing is downloaded whole: an object is fetched in fixed
/// chunks, on demand, each chunk once — `<dir>/<key>/<chunk index>` — and
/// every later read of those bytes is a local file read.
///
/// Chunks are keyed by the object's key alone, which is sound for what a film
/// is made of: a pack's key hashes its inputs, a run's prefix carries its
/// timestamp, the tile store names every archive afresh. Listings are not
/// cached: what a bucket holds does change.
pub struct Cached {
    inner: Arc<dyn Objects>,
    dir: PathBuf,
    sizes: Mutex<HashMap<String, u64>>,
    /// One lock per chunk: two readers after the same bytes wait for one
    /// fetch rather than starting two.
    fetching: Mutex<HashMap<(String, u64), Arc<Mutex<()>>>>,
}

fn io(path: &std::path::Path, e: std::io::Error) -> RepoError {
    RepoError::Store(format!("{}: {e}", path.display()))
}

/// The chunks a byte range touches.
fn covering(range: &Range<u64>) -> Range<u64> {
    range.start / CHUNK..range.end.div_ceil(CHUNK)
}

impl Cached {
    pub fn new(inner: Arc<dyn Objects>, dir: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            dir: dir.into(),
            sizes: Mutex::default(),
            fetching: Mutex::default(),
        }
    }

    async fn chunk(&self, key: &str, size: u64, index: u64) -> Result<Vec<u8>, RepoError> {
        let path = self.dir.join(key).join(format!("{index:08}"));
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return Ok(bytes);
        }
        let lock = self
            .fetching
            .lock()
            .await
            .entry((key.to_string(), index))
            .or_default()
            .clone();
        let _held = lock.lock().await;
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return Ok(bytes);
        }
        let bytes = self
            .inner
            .read(key, index * CHUNK..((index + 1) * CHUNK).min(size))
            .await?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| io(dir, e))?;
        }
        // Written beside and renamed: a reader never sees half a chunk.
        let partial = path.with_extension("part");
        tokio::fs::write(&partial, &bytes)
            .await
            .map_err(|e| io(&partial, e))?;
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|e| io(&path, e))?;
        Ok(bytes)
    }
}

#[async_trait]
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

    /// Asked of the store once per object.
    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        if let Some(size) = self.sizes.lock().await.get(key) {
            return Ok(*size);
        }
        let size = self.inner.size(key).await?;
        self.sizes.lock().await.insert(key.to_string(), size);
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
    use tuile_farm::{ObjectRunStore, Tuning};

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

    #[test]
    fn a_range_touches_exactly_its_chunks() {
        assert_eq!(covering(&(0..1)), 0..1);
        assert_eq!(covering(&(CHUNK - 1..CHUNK + 1)), 0..2);
        assert_eq!(covering(&(CHUNK..2 * CHUNK)), 1..2);
    }

    /// The cache is an `Objects` like the store it wraps: the same reads give
    /// the same bytes — and, once read, still do with the store gone.
    #[tokio::test]
    async fn it_answers_as_the_store_does_and_fetches_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("store");
        let object = noise(2 * CHUNK as usize + 12_345);
        std::fs::create_dir_all(root.join("a")).expect("mkdir");
        std::fs::write(root.join("a/b.tuilepack"), &object).expect("write");
        let store: Arc<dyn Objects> =
            Arc::new(ObjectRunStore::local(&root, Tuning::default()).expect("store"));
        let cached = Cached::new(store.clone(), dir.path().join("cache"));
        let key = "a/b.tuilepack";
        assert_eq!(cached.size(key).await.expect("size"), object.len() as u64);

        let c = CHUNK as usize;
        for range in [
            0..16,
            c - 3..c + 5,
            2 * c..object.len(),
            100..2 * c + 12_000,
        ] {
            let r = range.start as u64..range.end as u64;
            let got = cached.read(key, r.clone()).await.expect("read");
            assert_eq!(got, object[range.clone()], "range {range:?}");
            assert_eq!(
                got,
                store.read(key, r).await.expect("store"),
                "range {range:?}"
            );
        }
        assert!(cached.read(key, 0..object.len() as u64 + 1).await.is_err());
        assert!(matches!(
            cached.size("a/missing").await,
            Err(RepoError::NotFound(_))
        ));

        std::fs::remove_file(root.join("a/b.tuilepack")).expect("remove");
        let again = cached
            .read(key, 5..object.len() as u64 - 5)
            .await
            .expect("cached");
        assert_eq!(again, object[5..object.len() - 5]);
    }
}
