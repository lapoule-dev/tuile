// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Small objects that change, read again only when they have.
//!
//! [`Cached`](crate::Cached) keeps blocks of what never changes. A store's
//! catalog, its manifests and its tables do change, so they cannot be kept
//! and believed — but they change rarely, and a reader that asks for the
//! same manifest on every run mostly asks for bytes it already had.
//! [`Revalidated`] keeps each object with the validator its store gave it,
//! and asks with that validator ([`Objects::read_if_changed`]): an object
//! that has not been written since costs a question and no answer.
//!
//! What it keeps goes in a [`ContentStore`] — memory, a disk, a browser's
//! cache —, which is a cache and may lose anything: an entry lost is read
//! again, once.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::storage::ContentStore;

use crate::{Entry, Listing, Objects, Read, RepoError};

/// What a [`Revalidated`] has done so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Revalidations {
    /// Objects asked for that their store said were still the ones kept:
    /// no body travelled.
    pub unchanged: u64,
    /// Objects that came whole: new, written again, or lost by the keeper.
    pub fetched: u64,
    pub fetched_bytes: u64,
    /// Objects their store does not have.
    pub absent: u64,
    /// Reads answered from what was kept, without asking: the object had
    /// been asked for already by this reader.
    pub kept: u64,
}

/// `Objects` whose whole objects are kept with their validators and asked
/// for again conditionally: see the module.
pub struct Revalidated {
    inner: Arc<dyn Objects>,
    keeper: Arc<dyn ContentStore>,
    /// Keys asked of the store by this reader: what it said then holds for
    /// as long as this reader lives — one render, one page. A reader that
    /// must see later writes is another reader.
    asked: Mutex<HashSet<String>>,
    missing: Mutex<HashSet<String>>,
    counts: [AtomicU64; 5],
}

/// An entry as kept: the validator's length, the validator, the body.
fn entry(etag: &str, body: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(4 + etag.len() + body.len());
    out.extend_from_slice(&(etag.len() as u32).to_le_bytes());
    out.extend_from_slice(etag.as_bytes());
    out.extend_from_slice(body);
    Bytes::from(out)
}

fn parts(kept: &[u8]) -> Option<(&str, &[u8])> {
    let len = u32::from_le_bytes(kept.get(..4)?.try_into().ok()?) as usize;
    let etag = std::str::from_utf8(kept.get(4..4 + len)?).ok()?;
    Some((etag, &kept[4 + len..]))
}

impl Revalidated {
    pub fn new(inner: Arc<dyn Objects>, keeper: Arc<dyn ContentStore>) -> Self {
        Self {
            inner,
            keeper,
            asked: Mutex::default(),
            missing: Mutex::default(),
            counts: Default::default(),
        }
    }

    pub fn so_far(&self) -> Revalidations {
        let at = |i: usize| self.counts[i].load(Ordering::Relaxed);
        Revalidations {
            unchanged: at(0),
            fetched: at(1),
            fetched_bytes: at(2),
            absent: at(3),
            kept: at(4),
        }
    }

    fn count(&self, which: usize, by: u64) {
        self.counts[which].fetch_add(by, Ordering::Relaxed);
    }

    fn was(set: &Mutex<HashSet<String>>, key: &str) -> bool {
        set.lock().is_ok_and(|set| set.contains(key))
    }

    fn note(set: &Mutex<HashSet<String>>, key: &str) {
        if let Ok(mut set) = set.lock() {
            set.insert(key.to_string());
        }
    }

    async fn whole(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        if Self::was(&self.missing, key) {
            return Err(RepoError::NotFound(key.to_string()));
        }
        let kept = self.keeper.get(key).await;
        let held = kept.as_deref().and_then(parts);
        if let Some((_, body)) = held.filter(|_| Self::was(&self.asked, key)) {
            self.count(4, 1);
            return Ok(body.to_vec());
        }
        let read = match self.inner.read_if_changed(key, held.map(|h| h.0)).await {
            Ok(read) => read,
            Err(RepoError::NotFound(what)) => {
                // Nothing there: remembered, so it is asked once. A failure
                // to ask is not remembered — it is not an absence.
                Self::note(&self.missing, key);
                self.count(3, 1);
                return Err(RepoError::NotFound(what));
            }
            Err(other) => return Err(other),
        };
        Self::note(&self.asked, key);
        match (read, held) {
            (Read::Unchanged, Some((_, body))) => {
                self.count(0, 1);
                Ok(body.to_vec())
            }
            // A store that says "unchanged" of nothing named: ask plainly.
            (Read::Unchanged, None) => {
                let bytes = self.inner.read_all(key).await?;
                self.count(1, 1);
                self.count(2, bytes.len() as u64);
                Ok(bytes)
            }
            (Read::Changed { bytes, etag }, _) => {
                self.count(1, 1);
                self.count(2, bytes.len() as u64);
                // Without a validator there is nothing to ask with next
                // time: kept for this reader only, under an empty one.
                self.keeper
                    .put(key, entry(etag.as_deref().unwrap_or(""), &bytes), None)
                    .await;
                Ok(bytes)
            }
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Objects for Revalidated {
    fn label(&self) -> String {
        self.inner.label()
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        self.inner.list(prefix).await
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        self.inner.browse(prefix).await
    }

    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        Ok(self.whole(key).await?.len() as u64)
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let whole = self.whole(key).await?;
        whole
            .get(range.start as usize..range.end as usize)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                RepoError::Store(format!(
                    "{key}: bytes {}..{} are outside its {}",
                    range.start,
                    range.end,
                    whole.len()
                ))
            })
    }

    async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
        self.whole(key).await
    }

    async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
        self.inner.read_if_changed(key, known).await
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    /// A keeper that keeps, and can be made to forget.
    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, Bytes>>);

    #[async_trait]
    impl ContentStore for Memory {
        async fn get(&self, key: &str) -> Option<Bytes> {
            self.0.lock().expect("lock").get(key).cloned()
        }
        async fn put(&self, key: &str, value: Bytes, _: Option<Duration>) {
            self.0.lock().expect("lock").insert(key.to_string(), value);
        }
    }

    /// A store of versioned objects that counts the bodies it sends, and
    /// can be made to fail.
    #[derive(Default)]
    struct Versioned {
        objects: Mutex<HashMap<String, (u32, Vec<u8>)>>,
        bodies: AtomicU64,
        asked: AtomicU64,
        broken: Mutex<bool>,
    }

    impl Versioned {
        fn write(&self, key: &str, body: &[u8]) {
            let mut objects = self.objects.lock().expect("lock");
            let version = objects.get(key).map_or(1, |o| o.0 + 1);
            objects.insert(key.to_string(), (version, body.to_vec()));
        }
    }

    #[async_trait]
    impl Objects for Versioned {
        fn label(&self) -> String {
            "test".into()
        }
        async fn list(&self, _: &str) -> Result<Vec<Entry>, RepoError> {
            Ok(Vec::new())
        }
        async fn browse(&self, _: &str) -> Result<Listing, RepoError> {
            Ok(Listing::default())
        }
        async fn size(&self, key: &str) -> Result<u64, RepoError> {
            Ok(self.read_all(key).await?.len() as u64)
        }
        async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
            Ok(self.read_all(key).await?[range.start as usize..range.end as usize].to_vec())
        }
        async fn read_all(&self, key: &str) -> Result<Vec<u8>, RepoError> {
            match self.read_if_changed(key, None).await? {
                Read::Changed { bytes, .. } => Ok(bytes),
                Read::Unchanged => unreachable!("nothing was known"),
            }
        }
        async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            if *self.broken.lock().expect("lock") {
                return Err(RepoError::Store("unreachable".into()));
            }
            let objects = self.objects.lock().expect("lock");
            let (version, body) = objects
                .get(key)
                .ok_or_else(|| RepoError::NotFound(key.to_string()))?;
            let etag = format!("\"v{version}\"");
            if known == Some(etag.as_str()) {
                return Ok(Read::Unchanged);
            }
            self.bodies.fetch_add(1, Ordering::Relaxed);
            Ok(Read::Changed {
                bytes: body.clone(),
                etag: Some(etag),
            })
        }
    }

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        futures_executor::block_on(f)
    }

    #[test]
    fn an_object_kept_is_asked_about_and_not_sent_again() {
        let store = Arc::new(Versioned::default());
        store.write("zone/manifest.json", b"one");
        let keeper: Arc<Memory> = Arc::default();
        let bodies = || store.bodies.load(Ordering::Relaxed);
        let asked = || store.asked.load(Ordering::Relaxed);

        // A first reader: the object comes, and is kept.
        let first = Revalidated::new(store.clone(), keeper.clone());
        assert_eq!(
            block(first.read_all("zone/manifest.json")).expect("read"),
            b"one"
        );
        assert_eq!((bodies(), asked()), (1, 1));
        // The same reader again: not even a question.
        assert_eq!(
            block(first.read_all("zone/manifest.json")).expect("read"),
            b"one"
        );
        assert_eq!(
            block(first.read("zone/manifest.json", 1..3)).expect("read"),
            b"ne"
        );
        assert_eq!((bodies(), asked()), (1, 1));
        assert_eq!(first.so_far().kept, 2);

        // Another reader, later, over what was kept: a question, no body.
        let second = Revalidated::new(store.clone(), keeper.clone());
        assert_eq!(
            block(second.read_all("zone/manifest.json")).expect("read"),
            b"one"
        );
        assert_eq!((bodies(), asked()), (1, 2));
        assert_eq!(
            second.so_far(),
            Revalidations {
                unchanged: 1,
                ..Revalidations::default()
            }
        );

        // Written again: a third reader is sent the new one, and keeps it.
        store.write("zone/manifest.json", b"two");
        let third = Revalidated::new(store.clone(), keeper.clone());
        assert_eq!(
            block(third.read_all("zone/manifest.json")).expect("read"),
            b"two"
        );
        assert_eq!(bodies(), 2);
        assert_eq!(
            (third.so_far().fetched, third.so_far().fetched_bytes),
            (1, 3)
        );
        let fourth = Revalidated::new(store.clone(), keeper.clone());
        assert_eq!(
            block(fourth.read_all("zone/manifest.json")).expect("read"),
            b"two"
        );
        assert_eq!(bodies(), 2);

        // A keeper that lost it: read again, once, without a word.
        keeper.0.lock().expect("lock").clear();
        let fifth = Revalidated::new(store.clone(), keeper);
        assert_eq!(
            block(fifth.read_all("zone/manifest.json")).expect("read"),
            b"two"
        );
        assert_eq!(bodies(), 3);
    }

    #[test]
    fn what_is_not_there_is_asked_once_and_a_failure_is_not_an_absence() {
        let store = Arc::new(Versioned::default());
        let reader = Revalidated::new(store.clone(), Arc::<Memory>::default());
        for _ in 0..3 {
            assert!(matches!(
                block(reader.read_all("layer/tone/9/1/1.json")),
                Err(RepoError::NotFound(_))
            ));
        }
        assert_eq!(store.asked.load(Ordering::Relaxed), 1);
        assert_eq!(reader.so_far().absent, 1);

        // A store that cannot answer: an error each time, and when it
        // answers again the object is there.
        *store.broken.lock().expect("lock") = true;
        store.write("catalog.json", b"{}");
        assert!(matches!(
            block(reader.read_all("catalog.json")),
            Err(RepoError::Store(_))
        ));
        *store.broken.lock().expect("lock") = false;
        assert_eq!(block(reader.read_all("catalog.json")).expect("read"), b"{}");
    }
}
