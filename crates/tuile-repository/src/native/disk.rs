// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::ChunkStore;

/// How long a size once asked of the bucket is believed.
const SIZE_IS_BELIEVED: std::time::Duration = std::time::Duration::from_secs(60);

/// Chunks kept in a directory: `<dir>/<key>/<chunk index>`.
pub struct DiskChunks {
    dir: PathBuf,
    sizes: Mutex<HashMap<String, (u64, std::time::Instant)>>,
}

impl DiskChunks {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            sizes: Mutex::default(),
        }
    }

    fn path(&self, key: &str, index: u64) -> PathBuf {
        self.dir.join(key).join(format!("{index:08}"))
    }
}

#[async_trait]
impl ChunkStore for DiskChunks {
    async fn get(&self, key: &str, index: u64) -> Option<Vec<u8>> {
        tokio::fs::read(self.path(key, index)).await.ok()
    }

    /// Written beside and renamed: a reader never sees half a chunk. A chunk
    /// that cannot be written is simply not kept.
    async fn put(&self, key: &str, index: u64, bytes: &[u8]) {
        let path = self.path(key, index);
        let Some(dir) = path.parent() else { return };
        let partial = path.with_extension("part");
        let written = async {
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::write(&partial, bytes).await?;
            tokio::fs::rename(&partial, &path).await
        };
        if let Err(e) = written.await {
            tracing::warn!("{}: chunk not kept ({e})", path.display());
        }
    }

    /// In memory, and for a minute: an object can be replaced under its key,
    /// and its size is how that is noticed.
    async fn size(&self, key: &str) -> Option<u64> {
        let sizes = self.sizes.lock().ok()?;
        let (size, noted) = sizes.get(key)?;
        (noted.elapsed() < SIZE_IS_BELIEVED).then_some(*size)
    }

    async fn note_size(&self, key: &str, size: u64) {
        if let Ok(mut sizes) = self.sizes.lock() {
            sizes.insert(key.to_string(), (size, std::time::Instant::now()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_chunk_comes_back_as_it_was_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiskChunks::new(dir.path());
        assert!(store.get("a/b.tuilepack", 3).await.is_none());
        store.put("a/b.tuilepack", 3, b"chunk three").await;
        assert_eq!(
            store.get("a/b.tuilepack", 3).await.as_deref(),
            Some(&b"chunk three"[..])
        );
        assert!(store.get("a/b.tuilepack", 4).await.is_none());
        // Kept across instances: it is on disk.
        assert_eq!(
            DiskChunks::new(dir.path())
                .get("a/b.tuilepack", 3)
                .await
                .as_deref(),
            Some(&b"chunk three"[..])
        );
    }
}
