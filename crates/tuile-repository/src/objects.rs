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

    /// The object's size and the validator its store gives it: what says
    /// whether the object under a key is still the one a reader holds bytes
    /// of. A key can be written again, so a size alone does not.
    ///
    /// A store that has no validator to give answers `None`, which is what
    /// this default does.
    async fn stat(&self, key: &str) -> Result<(u64, Option<String>), RepoError> {
        Ok((self.size(key).await?, None))
    }

    /// A whole object. For the small ones: a marker, a manifest, a tape.
    async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        let size = self.size(key).await?;
        self.read(key, 0..size).await
    }

    /// A whole object, unless it is still the one `known` names: for the
    /// small ones that change, read again and again, of which the same bytes
    /// need not travel twice.
    ///
    /// `known` is a validator a previous call handed back. A store that has
    /// none to give answers `Changed` with no validator every time, which is
    /// what this default does: correct, and no saving.
    async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
        let _ = known;
        Ok(Read::Changed {
            bytes: self.read_all(key).await?,
            etag: None,
        })
    }

    /// [`Self::read_if_changed`] for many objects at once — each a key and
    /// the validator held for it, if any — answered in the order asked, one
    /// answer per key. One that fails, or is not there, is its own answer
    /// and takes nothing from the others.
    ///
    /// A film reads a manifest per zone, several hundred of them, each a
    /// kilobyte: one by one that is several hundred round trips to bring a
    /// few hundred kilobytes. A store that can answer many in one exchange
    /// overrides this; this default asks one by one, a few at a time.
    async fn read_many_if_changed(
        &self,
        asked: &[(String, Option<String>)],
    ) -> Vec<Result<Read, RepoError>> {
        each_if_changed(self, asked).await
    }

    /// Many whole objects at once, in the order asked: [`Self::read_all`]
    /// of each, through whatever [`Self::read_many_if_changed`] saves.
    async fn read_many(&self, keys: &[String]) -> Vec<Result<Vec<u8>, RepoError>> {
        let asked: Vec<(String, Option<String>)> =
            keys.iter().map(|key| (key.clone(), None)).collect();
        self.read_many_if_changed(&asked)
            .await
            .into_iter()
            .zip(keys)
            .map(|(read, key)| match read? {
                Read::Changed { bytes, .. } => Ok(bytes),
                // Said of nothing named: not an answer.
                Read::Unchanged => Err(RepoError::Store(format!(
                    "{key}: said unchanged, and nothing was held"
                ))),
            })
            .collect()
    }
}

/// How many objects are asked for at a time by a reader that asks one by
/// one: enough that several hundred do not wait in line, and no burden on
/// a bucket. A browser lets fewer through to one host and queues the rest.
const AT_ONCE: usize = 16;

/// Many objects asked one by one, [`AT_ONCE`] at a time: what
/// [`Objects::read_many_if_changed`] is where nothing answers many at once.
pub(crate) async fn each_if_changed<O: Objects + ?Sized>(
    objects: &O,
    asked: &[(String, Option<String>)],
) -> Vec<Result<Read, RepoError>> {
    let mut out = Vec::with_capacity(asked.len());
    for some in asked.chunks(AT_ONCE) {
        out.extend(
            futures_util::future::join_all(
                some.iter()
                    .map(|(key, known)| objects.read_if_changed(key, known.as_deref())),
            )
            .await,
        );
    }
    out
}

/// What [`Objects::read_if_changed`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// The object is the one the caller already holds.
    Unchanged,
    /// The object, and the validator its store gives it: opaque, and changed
    /// whenever the object is written again, even to the same size.
    Changed {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
}
