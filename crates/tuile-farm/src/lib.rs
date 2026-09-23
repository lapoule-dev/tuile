// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A render farm's object storage, behind one repository.
//!
//! Every byte a farm job moves — the pack in, the segments out, the logs, the
//! traces, the OptiX cache, the finished film — goes through [`RunStore`]. The
//! job holds a bucket's access key and secret and talks to the bucket itself;
//! nothing is presigned, and no launcher stands between the job and its data.
//!
//! # Why a crate and not `curl`
//!
//! `curl -o` on a presigned URL is one TCP stream. Measured on a Cloud Run L4
//! on 21 September 2026: a 2.3 GB pack in **88 seconds**, every task, before a
//! single frame — a single stream to object storage is capped far below what
//! the machine's network can carry. Many ranged reads in flight at once are
//! what the storage is built for, and a multipart upload is the only way to put
//! a film of several gigabytes at all. Both are the client's job, not ours:
//! [`object_store`] does the chunking, the part bookkeeping and the retries.
//!
//! # One implementation, two backends
//!
//! [`ObjectRunStore`] is written once against the `ObjectStore` interface and
//! built either on an S3-compatible bucket ([`ObjectRunStore::bucket`]) or on a
//! local directory ([`ObjectRunStore::local`]). The directory is not a mock:
//! it runs the same parallel reads and the same multipart writes, which is
//! what lets the contract below be checked on a workstation in a second and on
//! the real bucket with the same function.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod assemble;
pub mod concat;
pub mod job;
pub mod jobs;
pub mod launch;

/// What went wrong, in terms a job script can act on.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The key is not there. Distinct from every other failure because a
    /// caller often wants to know it — "is the pack baked yet?" — rather than
    /// die of it.
    #[error("not found: {0}")]
    NotFound(String),
    /// The bytes that arrived are not the bytes that were promised: a short
    /// read, a size that changed underneath a download.
    #[error("{key}: expected {expected} bytes, got {got}")]
    Truncated { key: String, expected: u64, got: u64 },
    #[error("configuration: {0}")]
    Config(String),
    #[error("{key}: {source}")]
    Store {
        key: String,
        #[source]
        source: object_store::Error,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl StoreError {
    fn store(key: &str, source: object_store::Error) -> Self {
        match source {
            object_store::Error::NotFound { .. } => StoreError::NotFound(key.to_string()),
            source => StoreError::Store { key: key.to_string(), source },
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        StoreError::Io { path: path.to_path_buf(), source }
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One object in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub size: u64,
}

/// The repository. Keys are `/`-separated and relative to the store's root.
///
/// Files, not buffers, for the whole objects: a pack is gigabytes, and the
/// question of how much of it sits in memory at once belongs to the
/// implementation (a bounded window of parts), not to every caller.
#[async_trait]
pub trait RunStore: Send + Sync {
    /// Downloads `key` into `dest`, whole or not at all: the bytes land in a
    /// sibling file and are renamed into place once complete and of the
    /// announced size. Returns the size.
    async fn get(&self, key: &str, dest: &Path) -> Result<u64>;

    /// The bytes of `range` within `key`.
    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes>;

    /// Uploads `src` to `key`, replacing whatever was there. Returns the size.
    async fn put(&self, src: &Path, key: &str) -> Result<u64>;

    /// Every object whose key starts with the directory `prefix`, sorted by
    /// key.
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>>;

    /// Whether `key` exists. Any failure other than absence is an error, not
    /// a `false`: a store that cannot be reached must not read as "not baked
    /// yet" and send a bake off for nothing.
    async fn exists(&self, key: &str) -> Result<bool>;
}

/// How a transfer is cut up.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Size of one ranged read and of one uploaded part.
    ///
    /// S3 refuses parts under 5 MiB except the last, and a bucket that holds
    /// parts to an equal size wants them constant — which `WriteMultipart`
    /// guarantees.
    pub part_bytes: usize,
    /// Transfers in flight at once, in each direction. Memory held is about
    /// `part_bytes × concurrency`: 16 × 16 MiB = 256 MiB.
    pub concurrency: usize,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { part_bytes: 16 << 20, concurrency: 16 }
    }
}

impl Tuning {
    /// The defaults, overridden by `TUILE_STORE_PART_MB` and
    /// `TUILE_STORE_CONCURRENCY` — a knob for measuring, not for everyday use.
    pub fn from_env() -> Self {
        let mut t = Tuning::default();
        if let Some(mb) = env_number("TUILE_STORE_PART_MB") {
            t.part_bytes = mb.max(5) << 20;
        }
        if let Some(n) = env_number("TUILE_STORE_CONCURRENCY") {
            t.concurrency = n.max(1);
        }
        t
    }
}

fn env_number(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// [`RunStore`] on any [`ObjectStore`].
pub struct ObjectRunStore {
    store: Arc<dyn ObjectStore>,
    tuning: Tuning,
    /// Printed in progress lines, so a log says where the bytes went.
    label: String,
}

/// Where a bucket is and how to sign for it.
#[derive(Debug, Clone)]
pub struct BucketConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

impl BucketConfig {
    /// From `TUILE_STORE_ENDPOINT`, `TUILE_STORE_BUCKET`,
    /// `TUILE_STORE_ACCESS_KEY_ID`, `TUILE_STORE_SECRET_ACCESS_KEY` and
    /// optionally `TUILE_STORE_REGION` (default `auto`).
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| StoreError::Config(format!("{name} is not set")))
        };
        Ok(BucketConfig {
            endpoint: var("TUILE_STORE_ENDPOINT")?,
            bucket: var("TUILE_STORE_BUCKET")?,
            access_key_id: var("TUILE_STORE_ACCESS_KEY_ID")?,
            secret_access_key: var("TUILE_STORE_SECRET_ACCESS_KEY")?,
            region: std::env::var("TUILE_STORE_REGION").unwrap_or_else(|_| "auto".into()),
        })
    }
}

impl ObjectRunStore {
    /// An S3-compatible bucket.
    pub fn bucket(config: &BucketConfig, tuning: Tuning) -> Result<Self> {
        let store = AmazonS3Builder::new()
            .with_endpoint(&config.endpoint)
            .with_region(&config.region)
            .with_bucket_name(&config.bucket)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .build()
            .map_err(|e| StoreError::Config(e.to_string()))?;
        Ok(ObjectRunStore {
            store: Arc::new(store),
            tuning,
            label: format!("bucket {}", config.bucket),
        })
    }

    /// A directory, created if absent.
    pub fn local(root: &Path, tuning: Tuning) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(|e| StoreError::io(root, e))?;
        let store =
            LocalFileSystem::new_with_prefix(root).map_err(|e| StoreError::Config(e.to_string()))?;
        Ok(ObjectRunStore {
            store: Arc::new(store),
            tuning,
            label: format!("dir {}", root.display()),
        })
    }

    /// `TUILE_STORE_DIR` if set, the bucket from the environment otherwise.
    pub fn from_env() -> Result<Self> {
        let tuning = Tuning::from_env();
        match std::env::var("TUILE_STORE_DIR") {
            Ok(dir) if !dir.is_empty() => Self::local(Path::new(&dir), tuning),
            _ => Self::bucket(&BucketConfig::from_env()?, tuning),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The body of [`RunStore::get`], into `tmp`.
    async fn download(
        &self,
        key: &str,
        path: &ObjectPath,
        meta: &object_store::ObjectMeta,
        tmp: &Path,
    ) -> Result<()> {
        let size = meta.size;
        let part = self.tuning.part_bytes as u64;
        let mut file = tokio::fs::File::create(tmp).await.map_err(|e| StoreError::io(tmp, e))?;

        // Every range is read against the ETag the HEAD returned. An object
        // replaced halfway through a download would otherwise stitch the first
        // half of one pack onto the second half of another, both the right
        // size — a corruption nothing downstream could see.
        let ranges: Vec<Range<u64>> =
            (0..size).step_by(part.max(1) as usize).map(|s| s..(s + part).min(size)).collect();
        let e_tag = meta.e_tag.clone();
        // `buffered`, not `buffer_unordered`: parts arrive in order, so the
        // file is written front to back with at most `concurrency` parts in
        // memory, and never needs preallocating or seeking.
        let mut parts = stream::iter(ranges)
            .map(|range| {
                let store = Arc::clone(&self.store);
                let path = path.clone();
                let e_tag = e_tag.clone();
                async move {
                    let expected = range.end - range.start;
                    let options = GetOptions {
                        range: Some(GetRange::Bounded(range)),
                        if_match: e_tag,
                        ..Default::default()
                    };
                    let bytes = store.get_opts(&path, options).await?.bytes().await?;
                    Ok::<_, object_store::Error>((expected, bytes))
                }
            })
            .buffered(self.tuning.concurrency);

        let mut written = 0u64;
        while let Some(next) = parts.next().await {
            let (expected, bytes) = next.map_err(|e| StoreError::store(key, e))?;
            if bytes.len() as u64 != expected {
                return Err(StoreError::Truncated {
                    key: key.to_string(),
                    expected: written + expected,
                    got: written + bytes.len() as u64,
                });
            }
            file.write_all(&bytes).await.map_err(|e| StoreError::io(tmp, e))?;
            written += bytes.len() as u64;
        }
        file.flush().await.map_err(|e| StoreError::io(tmp, e))?;
        file.sync_all().await.map_err(|e| StoreError::io(tmp, e))?;
        if written != size {
            return Err(StoreError::Truncated { key: key.to_string(), expected: size, got: written });
        }
        Ok(())
    }
}

fn key_path(key: &str) -> Result<ObjectPath> {
    ObjectPath::parse(key).map_err(|e| StoreError::Config(format!("key {key:?}: {e}")))
}

/// `dest` with `.part` appended, in the same directory so the rename is
/// atomic.
fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

fn rate(bytes: u64, started: Instant) -> String {
    let secs = started.elapsed().as_secs_f64().max(1e-3);
    format!("{:.1} MB in {secs:.1} s ({:.0} MB/s)", bytes as f64 / 1e6, bytes as f64 / 1e6 / secs)
}

#[async_trait]
impl RunStore for ObjectRunStore {
    async fn get(&self, key: &str, dest: &Path) -> Result<u64> {
        let started = Instant::now();
        let path = key_path(key)?;
        let meta = self.store.head(&path).await.map_err(|e| StoreError::store(key, e))?;
        let tmp = partial_path(dest);
        if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(dir).await.map_err(|e| StoreError::io(dir, e))?;
        }
        let result = self.download(key, &path, &meta, &tmp).await;
        if result.is_err() {
            // Half a pack must never sit where a retry could mistake it for one.
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result?;
        tokio::fs::rename(&tmp, dest).await.map_err(|e| StoreError::io(dest, e))?;
        eprintln!("GET {key} ← {}: {}", self.label, rate(meta.size, started));
        Ok(meta.size)
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        self.store.get_range(&key_path(key)?, range).await.map_err(|e| StoreError::store(key, e))
    }

    async fn put(&self, src: &Path, key: &str) -> Result<u64> {
        let started = Instant::now();
        let path = key_path(key)?;
        let size = tokio::fs::metadata(src).await.map_err(|e| StoreError::io(src, e))?.len();
        let part = self.tuning.part_bytes;
        let mut file = tokio::fs::File::open(src).await.map_err(|e| StoreError::io(src, e))?;

        if size <= part as u64 {
            // One request is one request: multipart for a log tarball would
            // be three round trips to say the same thing.
            let mut data = Vec::with_capacity(size as usize);
            file.read_to_end(&mut data).await.map_err(|e| StoreError::io(src, e))?;
            self.store
                .put(&path, PutPayload::from(data))
                .await
                .map_err(|e| StoreError::store(key, e))?;
        } else {
            let upload =
                self.store.put_multipart(&path).await.map_err(|e| StoreError::store(key, e))?;
            let mut writer = WriteMultipart::new_with_chunk_size(upload, part);
            let mut buf = vec![0u8; part];
            loop {
                let n = read_full(&mut file, &mut buf).await.map_err(|e| StoreError::io(src, e))?;
                if n == 0 {
                    break;
                }
                // Back-pressure: never more than `concurrency` parts on the
                // wire, whatever the file's size.
                if let Err(e) = writer.wait_for_capacity(self.tuning.concurrency).await {
                    let _ = writer.abort().await;
                    return Err(StoreError::store(key, e));
                }
                writer.write(&buf[..n]);
            }
            writer.finish().await.map_err(|e| StoreError::store(key, e))?;
        }
        eprintln!("PUT {key} → {}: {}", self.label, rate(size, started));
        Ok(size)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        let path = key_path(prefix)?;
        let mut entries: Vec<Entry> = self
            .store
            .list(Some(&path))
            .map_ok(|meta| Entry { key: meta.location.to_string(), size: meta.size })
            .try_collect()
            .await
            .map_err(|e| StoreError::store(prefix, e))?;
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        match self.store.head(&key_path(key)?).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(StoreError::store(key, e)),
        }
    }
}

/// Fills `buf` unless the file ends first. `read` alone may return less than
/// asked in the middle of a file, and a short part in the middle of a
/// multipart upload is refused by the bucket.
async fn read_full(file: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// The behaviour every [`RunStore`] owes its callers, as one function any
/// implementation can be run through — the directory in unit tests, the real
/// bucket in `tests/bucket.rs`.
///
/// `prefix` must be a key directory nothing else writes to; `part_bytes` is
/// the store's part size, so the sizes below straddle it.
#[doc(hidden)]
pub async fn assert_run_store_contract(store: &dyn RunStore, prefix: &str, part_bytes: usize) {
    let work = tempfile::tempdir().expect("tempdir");
    let make = |name: &str, len: usize| {
        // Not a repeating pattern: a part written at the wrong offset must
        // change the bytes, not just move identical ones around.
        let data: Vec<u8> = (0..len).map(|i| ((i * 2_654_435_761) >> 13) as u8).collect();
        let path = work.path().join(name);
        std::fs::write(&path, &data).expect("write fixture");
        (path, data)
    };

    // Sizes around the part boundary, including the empty file and a last
    // part shorter than the others.
    let sizes = [0, 1, part_bytes - 1, part_bytes, part_bytes + 1, 3 * part_bytes + 7];
    for (i, len) in sizes.iter().enumerate() {
        let (src, data) = make(&format!("in{i}"), *len);
        let key = format!("{prefix}/obj{i}.bin");
        assert_eq!(store.put(&src, &key).await.expect("put"), *len as u64, "put size of {len}");
        let dest = work.path().join(format!("out{i}"));
        assert_eq!(store.get(&key, &dest).await.expect("get"), *len as u64, "get size of {len}");
        let back = std::fs::read(&dest).expect("read back");
        assert!(back == data, "round trip of {len} bytes changed the content");
        assert!(!partial_path(&dest).exists(), "a completed get left its .part behind");
    }

    // A range inside, and a range across a part boundary.
    let big = format!("{prefix}/obj5.bin");
    let (_, data) = make("again", 3 * part_bytes + 7);
    let across = (part_bytes as u64 - 3)..(part_bytes as u64 + 5);
    let got = store.get_range(&big, across.clone()).await.expect("get_range");
    assert_eq!(&got[..], &data[across.start as usize..across.end as usize], "get_range bytes");

    // Replacing an object replaces it — a shorter one included.
    let (short, short_data) = make("short", 3);
    store.put(&short, &big).await.expect("overwrite");
    let dest = work.path().join("short-back");
    store.get(&big, &dest).await.expect("get overwritten");
    assert_eq!(std::fs::read(&dest).expect("read"), short_data, "overwrite kept old bytes");

    // Listing: everything under the prefix, sorted, with sizes; nothing from a
    // sibling that merely shares the prefix's first characters.
    let (_, _) = make("sibling", 1);
    store.put(&work.path().join("sibling"), &format!("{prefix}x/stray.bin")).await.expect("put");
    let listed = store.list(prefix).await.expect("list");
    let keys: Vec<&str> = listed.iter().map(|e| e.key.as_str()).collect();
    let expected: Vec<String> = (0..sizes.len()).map(|i| format!("{prefix}/obj{i}.bin")).collect();
    assert_eq!(keys, expected.iter().map(String::as_str).collect::<Vec<_>>(), "list keys");
    assert_eq!(listed[1].size, 1, "list size");
    assert_eq!(listed[5].size, 3, "list size after overwrite");

    assert!(store.exists(&big).await.expect("exists"), "exists on a present key");
    assert!(!store.exists(&format!("{prefix}/absent")).await.expect("exists"), "exists on absent");
    let missing = store.get(&format!("{prefix}/absent"), &work.path().join("never")).await;
    assert!(matches!(missing, Err(StoreError::NotFound(_))), "get of absent key: {missing:?}");
    assert!(!work.path().join("never").exists(), "a failed get left a file at dest");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_directory_keeps_the_contract() {
        let root = tempfile::tempdir().expect("tempdir");
        // Tiny parts, so the multipart and ranged paths run on small files.
        let tuning = Tuning { part_bytes: 4096, concurrency: 3 };
        let store = ObjectRunStore::local(root.path(), tuning).expect("local store");
        assert_run_store_contract(&store, "runs/r1", tuning.part_bytes).await;
    }
}
