// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::ops::Range;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("{0}: not found")]
    NotFound(String),
    #[error("{0}")]
    Store(String),
    /// The bucket holds something a layout does not allow.
    #[error("{key}: {what}")]
    Malformed { key: String, what: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Entry {
    pub key: String,
    pub size: u64,
}

/// One level of a bucket.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Listing {
    /// The "directories" directly under the prefix, as full prefixes.
    pub dirs: Vec<String>,
    pub files: Vec<Entry>,
}

/// Objects in a bucket, read-only.
///
/// # Contract
///
/// - Keys are `/`-separated, never start with `/`, and name the same object
///   in every method.
/// - `list` and `browse` return keys sorted, and only objects that `size` and
///   `read` then find.
/// - `read(key, a..b)` returns exactly `b - a` bytes — the object's bytes at
///   those offsets — or an error. Never fewer: a short read is an error, not
///   a shorter answer. The range must lie within `size(key)`.
/// - A missing key is [`RepoError::NotFound`] from `size` and `read`.
/// - Nothing is ever written.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Objects: Send + Sync {
    /// Where the bytes are, for a log line or a page.
    fn label(&self) -> String;
    /// Every object below `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError>;
    /// One level below `prefix` (empty for the root).
    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError>;
    async fn size(&self, key: &str) -> Result<u64, RepoError>;
    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError>;

    /// A whole object. For the small ones: a marker, a manifest, a tape.
    async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        let size = self.size(key).await?;
        self.read(key, 0..size).await
    }
}
