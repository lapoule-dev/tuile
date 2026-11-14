// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Archives downloaded once, then kept in tiers in front of their origin.
//!
//! A store's archives are immutable: each is written once under a key of its
//! own (`<epoch>-<nanos>-<salt>.pmtiles`, [`is_archive_key`]) and never
//! rewritten, so a copy of one is never stale. [`Tiered`] is an object store
//! that answers for its origin and keeps such archives in tiers read before
//! it, nearest first — a disk on this machine, a bucket the machines of a
//! region share, any object store. Reads go tier by tier, then to the
//! origin; what the origin gives is written into every tier that missed it,
//! so the next reader anywhere behind those tiers does not go to the origin.
//!
//! - **Content keyed.** Only archive keys are tiered. A manifest, the
//!   catalog, an extract — keys that are rewritten — always go to the origin.
//! - **Single flight.** However many readers of this process miss the same
//!   archive at once, it is downloaded once.
//! - **Written once.** A tier is written with [`PutMode::Create`]: an archive
//!   another process put there first is left as it is (the same bytes).
//! - **Ranges.** A range of an archive no tier holds brings the whole archive
//!   down, up to [`DEFAULT_WHOLE_BELOW`], and is served from it: the next
//!   range is in a tier. A larger archive is read from the origin by ranges,
//!   as without tiers.
//! - **Kept while used.** A tier that forgets what is not used — a bucket
//!   whose lifecycle deletes an object some days after its custom time —
//!   is given a [`Touch`]: an archive written into it is touched, and so is
//!   one read from it or from a nearer tier, at most once every
//!   [`TOUCH_EVERY`] for each archive this process reads. A tier without
//!   one is never touched.
//!
//! Without tiers it is its origin: every call passes through.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::stream::{BoxStream, StreamExt};
use object_store::path::Path;
use object_store::{
    Attributes, CopyOptions, Extensions, GetOptions, GetRange, GetResult, GetResultPayload,
    ListResult, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// Archives up to this size are brought down whole on a range miss.
pub const DEFAULT_WHOLE_BELOW: u64 = 256 << 20;

/// Whether `key` names a store archive: `…/<epoch>-<24 digits>-<16 hex>.pmtiles`,
/// a key written once and never again.
pub fn is_archive_key(key: &str) -> bool {
    let name = key.rsplit('/').next().unwrap_or(key);
    let Some(stem) = name.strip_suffix(".pmtiles") else {
        return false;
    };
    let mut parts = stem.rsplitn(3, '-');
    let (Some(salt), Some(nanos), Some(epoch)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !epoch.is_empty()
        && nanos.len() == 24
        && nanos.bytes().all(|b| b.is_ascii_digit())
        && salt.len() == 16
        && salt.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An archive of a tier touched again no sooner than this after the last
/// time this process touched it: a bucket that forgets objects by their
/// custom time counts in days, and each touch is a write.
pub const TOUCH_EVERY: Duration = Duration::from_secs(24 * 3600);

/// Tells a tier that one of its objects is in use, `at` this time: on a
/// bucket, its custom time moved forward, which its lifecycle counts an
/// object's idleness from. See [`Tier::with_touch`].
#[async_trait]
pub trait Touch: fmt::Debug + Send + Sync {
    async fn touch(&self, location: &Path, at: SystemTime) -> Result<()>;
}

/// One tier: a name for the counters, where it keeps its copies, and what
/// tells it a copy is in use, if it needs telling.
#[derive(Debug, Clone)]
pub struct Tier {
    pub name: String,
    pub objects: Arc<dyn ObjectStore>,
    pub touch: Option<Arc<dyn Touch>>,
}

impl Tier {
    pub fn new(name: impl Into<String>, objects: Arc<dyn ObjectStore>) -> Self {
        Self {
            name: name.into(),
            objects,
            touch: None,
        }
    }

    /// The tier, its archives touched when written and when read: once on
    /// writing, then at most once every [`TOUCH_EVERY`] for each.
    pub fn with_touch(mut self, touch: Arc<dyn Touch>) -> Self {
        self.touch = Some(touch);
        self
    }
}

/// The time, as a [`Tiered`] reads it.
pub type TierClock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// What the tiers answered, since they were made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TieredStats {
    /// Per tier, nearest first: `(name, reads answered, bytes answered)`.
    pub tiers: Vec<(String, u64, u64)>,
    /// Archive reads that went to the origin, and the bytes it gave.
    pub origin_requests: u64,
    pub origin_bytes: u64,
    /// Archives written into a tier, and found there already.
    pub written: u64,
    pub already_there: u64,
    /// Ranges of archives too large to bring down whole, read by range.
    pub ranges_passed: u64,
    /// Archives touched in a tier ([`Touch`]), and touches that failed.
    pub touched: u64,
    pub touch_failures: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: Vec<AtomicU64>,
    hit_bytes: Vec<AtomicU64>,
    origin_requests: AtomicU64,
    origin_bytes: AtomicU64,
    written: AtomicU64,
    already_there: AtomicU64,
    ranges_passed: AtomicU64,
    touched: AtomicU64,
    touch_failures: AtomicU64,
}

/// A download's failure, shared by everyone waiting on it.
#[derive(Debug, Clone)]
enum Failed {
    NotFound(String),
    Other(String),
}

impl Failed {
    fn of(path: &Path, e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { .. } => Self::NotFound(path.to_string()),
            e => Self::Other(e.to_string()),
        }
    }

    fn error(self) -> object_store::Error {
        match self {
            Self::NotFound(path) => object_store::Error::NotFound {
                path,
                source: "not in the origin".into(),
            },
            Self::Other(said) => object_store::Error::Generic {
                store: "tiered",
                source: said.into(),
            },
        }
    }
}

type Download = Shared<BoxFuture<'static, std::result::Result<(Bytes, ObjectMeta), Failed>>>;

struct Inner {
    origin: Arc<dyn ObjectStore>,
    tiers: Vec<Tier>,
    whole_below: u64,
    downloads: Mutex<HashMap<String, Download>>,
    counters: Counters,
    clock: TierClock,
    /// When this process last touched an archive, by tier and key.
    touched: Mutex<HashMap<(usize, String), SystemTime>>,
}

/// An origin with tiers in front of it: see the [module](self).
#[derive(Clone)]
pub struct Tiered {
    inner: Arc<Inner>,
}

impl fmt::Debug for Tiered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tiered")
            .field("origin", &self.inner.origin)
            .field("tiers", &self.inner.tiers)
            .finish()
    }
}

impl fmt::Display for Tiered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tiered({}", self.inner.origin)?;
        for t in &self.inner.tiers {
            write!(f, " < {}", t.name)?;
        }
        write!(f, ")")
    }
}

impl Tiered {
    /// `origin` behind `tiers`, nearest first.
    pub fn new(origin: Arc<dyn ObjectStore>, tiers: Vec<Tier>) -> Self {
        Self::with_whole_below(origin, tiers, DEFAULT_WHOLE_BELOW)
    }

    /// The same, archives up to `whole_below` bytes brought down whole on a
    /// range miss.
    pub fn with_whole_below(
        origin: Arc<dyn ObjectStore>,
        tiers: Vec<Tier>,
        whole_below: u64,
    ) -> Self {
        let n = tiers.len();
        Self {
            inner: Arc::new(Inner {
                origin,
                tiers,
                whole_below,
                downloads: Mutex::default(),
                counters: Counters {
                    hits: (0..n).map(|_| AtomicU64::new(0)).collect(),
                    hit_bytes: (0..n).map(|_| AtomicU64::new(0)).collect(),
                    ..Counters::default()
                },
                clock: Arc::new(SystemTime::now),
                touched: Mutex::default(),
            }),
        }
    }

    /// The same, its time read from `clock`: for a test that moves it.
    pub fn with_clock(mut self, clock: TierClock) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.clock = clock,
            None => tracing::warn!("a tiered store already shared keeps its clock"),
        }
        self
    }

    pub fn origin(&self) -> &Arc<dyn ObjectStore> {
        &self.inner.origin
    }

    /// An object the tiers keep and the origin never holds — one written by
    /// [`Self::keep_in_tiers`] — from the nearest tier that has it, touched
    /// there and farther as an archive read is; `None` when no tier has it.
    /// The origin is never asked: it would only answer that it has none.
    pub async fn get_kept(&self, location: &Path) -> Result<Option<GetResult>> {
        self.get_kept_opts(location, GetOptions::default()).await
    }

    /// The same, read with `options`: a range of it, for one.
    pub async fn get_kept_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> Result<Option<GetResult>> {
        let inner = &self.inner;
        for (i, tier) in inner.tiers.iter().enumerate() {
            match tier.objects.get_opts(location, options.clone()).await {
                Ok(got) => {
                    inner.hit(i, got.range.end - got.range.start);
                    inner.touch_from(i, location).await;
                    return Ok(Some(got));
                }
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(error) => {
                    tracing::warn!(tier = %tier.name, key = %location, %error, "TIER-READ-FAILED");
                    continue;
                }
            }
        }
        Ok(None)
    }

    /// `bytes` kept under `location` in every tier that does not hold it
    /// yet, never in the origin: for an object made from archives — named by
    /// what it was made from, so that the same key is the same content —
    /// that the next reader behind these tiers then reads whole instead of
    /// making it again. Written once ([`PutMode::Create`]), touched as an
    /// archive written is; a tier that holds it already is not sent the
    /// bytes again. A tier that fails is told in the log.
    pub async fn keep_in_tiers(&self, location: &Path, bytes: Bytes) {
        let inner = &self.inner;
        for i in 0..inner.tiers.len() {
            if inner.tiers[i].objects.head(location).await.is_ok() {
                inner.counters.already_there.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            inner.keep_one(i, location, &bytes).await;
        }
    }

    pub fn stats(&self) -> TieredStats {
        let c = &self.inner.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TieredStats {
            tiers: self
                .inner
                .tiers
                .iter()
                .enumerate()
                .map(|(i, t)| (t.name.clone(), g(&c.hits[i]), g(&c.hit_bytes[i])))
                .collect(),
            origin_requests: g(&c.origin_requests),
            origin_bytes: g(&c.origin_bytes),
            written: g(&c.written),
            already_there: g(&c.already_there),
            ranges_passed: g(&c.ranges_passed),
            touched: g(&c.touched),
            touch_failures: g(&c.touch_failures),
        }
    }

    /// The archive from the origin, written into every tier: once however
    /// many ask at the same time.
    async fn download(&self, location: &Path) -> Result<(Bytes, ObjectMeta)> {
        let key = location.to_string();
        let flight = {
            let mut downloads =
                self.inner
                    .downloads
                    .lock()
                    .map_err(|_| object_store::Error::Generic {
                        store: "tiered",
                        source: "a lock was poisoned".into(),
                    })?;
            match downloads.get(&key) {
                Some(d) => d.clone(),
                None => {
                    let inner = self.inner.clone();
                    let at = location.clone();
                    let d: Download = async move {
                        let got = inner.fetch_and_keep(&at).await;
                        if let Ok(mut d) = inner.downloads.lock() {
                            d.remove(at.as_ref());
                        }
                        got
                    }
                    .boxed()
                    .shared();
                    downloads.insert(key, d.clone());
                    d
                }
            }
        };
        flight.await.map_err(Failed::error)
    }
}

impl Inner {
    async fn fetch_and_keep(&self, at: &Path) -> std::result::Result<(Bytes, ObjectMeta), Failed> {
        // Kept by a download that ended since this reader missed the tiers.
        for (i, tier) in self.tiers.iter().enumerate() {
            if let Ok(got) = tier.objects.get(at).await {
                let meta = got.meta.clone();
                if let Ok(bytes) = got.bytes().await {
                    self.hit(i, bytes.len() as u64);
                    self.touch_from(i, at).await;
                    return Ok((bytes, meta));
                }
            }
        }
        let got = self.origin.get(at).await.map_err(|e| Failed::of(at, e))?;
        let meta = got.meta.clone();
        let bytes = got.bytes().await.map_err(|e| Failed::of(at, e))?;
        self.counters
            .origin_requests
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .origin_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        metrics::counter!("tuile_tiles_tier_origin_bytes_total").increment(bytes.len() as u64);
        self.keep(self.tiers.len(), at, &bytes).await;
        Ok((bytes, meta))
    }

    /// `bytes` written into the `nearest` tiers, unless one holds them
    /// already. A tier that fails to keep them is told in the log: the
    /// read goes on.
    async fn keep(&self, nearest: usize, at: &Path, bytes: &Bytes) {
        for i in 0..nearest {
            self.keep_one(i, at, bytes).await;
        }
    }

    /// `bytes` written into tier `i`, unless it holds them already.
    async fn keep_one(&self, i: usize, at: &Path, bytes: &Bytes) {
        let tier = &self.tiers[i];
        let opts = PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        match tier
            .objects
            .put_opts(at, PutPayload::from_bytes(bytes.clone()), opts)
            .await
        {
            Ok(_) => {
                self.counters.written.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("tuile_tiles_tier_writes_total", "tier" => tier.name.clone())
                    .increment(1);
                // A copy written now is in use now.
                self.touch_if_due(i, at, true).await;
            }
            Err(object_store::Error::AlreadyExists { .. }) => {
                self.counters.already_there.fetch_add(1, Ordering::Relaxed);
                self.touch_if_due(i, at, false).await;
            }
            Err(error) => {
                tracing::warn!(tier = %tier.name, key = %at, %error, "TIER-WRITE-FAILED");
                metrics::counter!("tuile_tiles_tier_write_failures_total", "tier" => tier.name.clone())
                    .increment(1);
            }
        }
    }

    /// An archive read from tier `i`: that tier and every farther one told
    /// it is in use, if they are to be told. A read answered near is a use
    /// of the copy kept far, which the machines of the region share.
    async fn touch_from(&self, i: usize, at: &Path) {
        for j in i..self.tiers.len() {
            self.touch_if_due(j, at, false).await;
        }
    }

    /// Tier `i` told that `at` is in use, if it is to be told: always when
    /// `written`, else when this process has not touched it in the last
    /// [`TOUCH_EVERY`]. A touch that fails is told in the log, and tried
    /// again on the next read: the read goes on.
    async fn touch_if_due(&self, i: usize, at: &Path, written: bool) {
        let Some(touch) = &self.tiers[i].touch else {
            return;
        };
        let now = (self.clock)();
        {
            let Ok(mut touched) = self.touched.lock() else {
                return;
            };
            let key = (i, at.to_string());
            let due = written
                || touched.get(&key).is_none_or(|last| {
                    now.duration_since(*last).is_ok_and(|idle| idle >= TOUCH_EVERY)
                });
            if !due {
                return;
            }
            // Before the request: readers at the same time touch it once.
            touched.insert(key, now);
        }
        let tier = &self.tiers[i].name;
        match touch.touch(at, now).await {
            Ok(()) => {
                self.counters.touched.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("tuile_tiles_tier_touches_total", "tier" => tier.clone())
                    .increment(1);
            }
            Err(error) => {
                if let Ok(mut touched) = self.touched.lock() {
                    touched.remove(&(i, at.to_string()));
                }
                self.counters.touch_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(tier = %tier, key = %at, %error, "TIER-TOUCH-FAILED");
                metrics::counter!("tuile_tiles_tier_touch_failures_total", "tier" => tier.clone())
                    .increment(1);
            }
        }
    }

    fn hit(&self, i: usize, bytes: u64) {
        self.counters.hits[i].fetch_add(1, Ordering::Relaxed);
        self.counters.hit_bytes[i].fetch_add(bytes, Ordering::Relaxed);
        metrics::counter!("tuile_tiles_tier_hits_total", "tier" => self.tiers[i].name.clone())
            .increment(1);
        metrics::counter!("tuile_tiles_tier_hit_bytes_total", "tier" => self.tiers[i].name.clone())
            .increment(bytes);
    }
}

/// Whether a read asks for anything but the object as it is.
fn conditional(o: &GetOptions) -> bool {
    o.if_match.is_some()
        || o.if_none_match.is_some()
        || o.if_modified_since.is_some()
        || o.if_unmodified_since.is_some()
        || o.version.is_some()
}

/// `bytes`, the whole object, answered as a read of `range` of it.
fn served(meta: ObjectMeta, bytes: Bytes, range: Option<&GetRange>) -> Result<GetResult> {
    let whole = bytes.len() as u64;
    let r: Range<u64> = match range {
        Some(r) => r
            .as_range(whole)
            .map_err(|e| object_store::Error::Generic {
                store: "tiered",
                source: e.to_string().into(),
            })?,
        None => 0..whole,
    };
    let body = bytes.slice(r.start as usize..r.end as usize);
    Ok(GetResult {
        payload: GetResultPayload::Stream(
            futures_util::stream::once(async move { Ok(body) }).boxed(),
        ),
        meta,
        range: r,
        attributes: Attributes::default(),
        extensions: Extensions::default(),
    })
}

#[async_trait]
impl ObjectStore for Tiered {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.inner.origin.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.origin.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let inner = &self.inner;
        if inner.tiers.is_empty() || conditional(&options) || !is_archive_key(location.as_ref()) {
            return inner.origin.get_opts(location, options).await;
        }
        for (i, tier) in inner.tiers.iter().enumerate() {
            match tier.objects.get_opts(location, options.clone()).await {
                Ok(got) => {
                    if i > 0 && options.range.is_none() && !options.head {
                        // Found farther away: kept nearer too.
                        let meta = got.meta.clone();
                        let bytes = got.bytes().await?;
                        inner.hit(i, bytes.len() as u64);
                        inner.touch_from(i, location).await;
                        inner.keep(i, location, &bytes).await;
                        return served(meta, bytes, None);
                    }
                    inner.hit(i, got.range.end - got.range.start);
                    if !options.head {
                        inner.touch_from(i, location).await;
                    }
                    return Ok(got);
                }
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(error) => {
                    tracing::warn!(tier = %tier.name, key = %location, %error, "TIER-READ-FAILED");
                    continue;
                }
            }
        }
        if options.head {
            return inner.origin.get_opts(location, options).await;
        }
        if options.range.is_some() {
            let meta = inner.origin.head(location).await?;
            if meta.size > inner.whole_below {
                inner.counters.ranges_passed.fetch_add(1, Ordering::Relaxed);
                return inner.origin.get_opts(location, options).await;
            }
        }
        let (bytes, meta) = self.download(location).await?;
        served(meta, bytes, options.range.as_ref())
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        // A tier's copies stay: their keys are never written again, and
        // tiers keep what they keep by their own rules.
        self.inner.origin.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.origin.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.origin.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.origin.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    const ARCHIVE: &str =
        "imagery/zones/z4/1/2/202611-176279040000000000000000-00000000deadbeef.pmtiles";

    /// An object store that counts the reads it answers, by key.
    #[derive(Debug, Default)]
    struct Counted {
        inner: InMemory,
        gets: Mutex<HashMap<String, u64>>,
    }

    impl Counted {
        fn gets(&self, key: &str) -> u64 {
            self.gets.lock().unwrap().get(key).copied().unwrap_or(0)
        }
    }

    impl fmt::Display for Counted {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "Counted")
        }
    }

    #[async_trait]
    impl ObjectStore for Counted {
        async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
            self.inner.put_opts(l, p, o).await
        }
        async fn put_multipart_opts(
            &self,
            l: &Path,
            o: PutMultipartOptions,
        ) -> Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(l, o).await
        }
        async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
            if !o.head {
                *self.gets.lock().unwrap().entry(l.to_string()).or_default() += 1;
            }
            // A slow origin: concurrent readers overlap.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.inner.get_opts(l, o).await
        }
        fn delete_stream(
            &self,
            l: BoxStream<'static, Result<Path>>,
        ) -> BoxStream<'static, Result<Path>> {
            self.inner.delete_stream(l)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
            self.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
            self.inner.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
            self.inner.copy_opts(f, t, o).await
        }
    }

    fn bytes() -> Bytes {
        Bytes::from((0..=255u8).cycle().take(10_000).collect::<Vec<u8>>())
    }

    async fn origin_with_archive() -> Arc<Counted> {
        let origin = Arc::new(Counted::default());
        origin
            .inner
            .put(&Path::from(ARCHIVE), bytes().into())
            .await
            .unwrap();
        origin
            .inner
            .put(
                &Path::from("imagery/zones/z4/1/2/manifest.json"),
                "{}".into(),
            )
            .await
            .unwrap();
        origin
    }

    #[test]
    fn only_archive_keys_are_tiered() {
        assert!(is_archive_key(ARCHIVE));
        assert!(is_archive_key(
            "terrain/top/1-2-176279040000000000000000-0123456789abcdef.pmtiles"
        ));
        assert!(!is_archive_key("imagery/zones/z4/1/2/manifest.json"));
        assert!(!is_archive_key("catalog.json"));
        assert!(!is_archive_key("_extracts/run/c0001.pmtiles"));
        assert!(!is_archive_key(
            "x/-176279040000000000000000-0123456789abcdef.pmtiles"
        ));
    }

    #[tokio::test]
    async fn a_miss_fills_every_tier_and_the_next_read_never_reaches_the_origin() {
        let origin = origin_with_archive().await;
        let (local, global) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let tiered = Tiered::new(
            origin.clone(),
            vec![
                Tier::new("local", local.clone()),
                Tier::new("global", global.clone()),
            ],
        );
        let at = Path::from(ARCHIVE);
        assert_eq!(
            tiered.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(origin.gets(ARCHIVE), 1);
        assert_eq!(
            local.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(
            global.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        // Again, whole and by range: the nearest tier answers.
        assert_eq!(
            tiered.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(
            tiered.get_range(&at, 100..200).await.unwrap(),
            bytes().slice(100..200)
        );
        assert_eq!(origin.gets(ARCHIVE), 1, "the origin was read once");
        let s = tiered.stats();
        assert_eq!((s.origin_requests, s.written), (1, 2));
        assert_eq!(s.tiers[0].1, 2);
    }

    #[tokio::test]
    async fn a_far_tier_hit_fills_the_near_one_and_spares_the_origin() {
        let origin = origin_with_archive().await;
        let (local, global) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let at = Path::from(ARCHIVE);
        // Another machine of the region brought it down already.
        global.put(&at, bytes().into()).await.unwrap();
        let tiered = Tiered::new(
            origin.clone(),
            vec![
                Tier::new("local", local.clone()),
                Tier::new("global", global),
            ],
        );
        assert_eq!(
            tiered.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(origin.gets(ARCHIVE), 0);
        assert_eq!(
            local.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
    }

    #[tokio::test]
    async fn concurrent_readers_of_one_archive_download_it_once() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let tiered = Tiered::new(origin.clone(), vec![Tier::new("global", global)]);
        let at = Path::from(ARCHIVE);
        let reads = (0..8).map(|i| {
            let (t, at) = (tiered.clone(), at.clone());
            async move {
                if i % 2 == 0 {
                    t.get(&at).await.unwrap().bytes().await.unwrap()
                } else {
                    t.get_range(&at, 0..10_000).await.unwrap()
                }
            }
        });
        for got in futures_util::future::join_all(reads).await {
            assert_eq!(got, bytes());
        }
        assert_eq!(origin.gets(ARCHIVE), 1, "one download for eight readers");
        assert_eq!(tiered.stats().written, 1, "and one write");
    }

    #[tokio::test]
    async fn what_is_rewritten_always_goes_to_the_origin() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let tiered = Tiered::new(origin.clone(), vec![Tier::new("global", global.clone())]);
        let manifest = "imagery/zones/z4/1/2/manifest.json";
        for _ in 0..2 {
            tiered.get(&Path::from(manifest)).await.unwrap();
        }
        assert_eq!(origin.gets(manifest), 2);
        assert!(global.head(&Path::from(manifest)).await.is_err());
    }

    #[tokio::test]
    async fn an_archive_absent_everywhere_is_not_found() {
        let origin = Arc::new(Counted::default());
        let tiered = Tiered::new(origin, vec![Tier::new("global", Arc::new(InMemory::new()))]);
        let got = tiered.get(&Path::from(ARCHIVE)).await;
        assert!(
            matches!(got, Err(object_store::Error::NotFound { .. })),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn a_large_archive_is_read_by_ranges_from_the_origin() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let tiered = Tiered::with_whole_below(
            origin.clone(),
            vec![Tier::new("global", global.clone())],
            1000,
        );
        let at = Path::from(ARCHIVE);
        assert_eq!(
            tiered.get_range(&at, 0..10).await.unwrap(),
            bytes().slice(0..10)
        );
        assert!(global.head(&at).await.is_err(), "not brought down whole");
        assert_eq!(tiered.stats().ranges_passed, 1);
    }

    /// The touches a tier is given, in order.
    #[derive(Debug, Default)]
    struct Touches(Mutex<Vec<(String, SystemTime)>>);

    impl Touches {
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Touch for Touches {
        async fn touch(&self, location: &Path, at: SystemTime) -> Result<()> {
            self.0.lock().unwrap().push((location.to_string(), at));
            Ok(())
        }
    }

    /// A clock a test moves.
    fn clock() -> (Arc<Mutex<SystemTime>>, TierClock) {
        let now = Arc::new(Mutex::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
        ));
        let read = now.clone();
        (now, Arc::new(move || *read.lock().unwrap()))
    }

    #[tokio::test]
    async fn a_touched_tier_is_touched_when_written_then_at_most_once_a_day() {
        let origin = origin_with_archive().await;
        let (local, global) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let touches = Arc::new(Touches::default());
        let (now, clock) = clock();
        let tiered = Tiered::new(
            origin.clone(),
            vec![
                // A tier that forgets nothing it is not told to: no touch.
                Tier::new("local", local.clone()),
                Tier::new("global", global.clone()).with_touch(touches.clone()),
            ],
        )
        .with_clock(clock);
        let at = Path::from(ARCHIVE);
        tiered.get(&at).await.unwrap().bytes().await.unwrap();
        assert_eq!(touches.count(), 1, "written into the global tier: touched");
        assert_eq!(touches.0.lock().unwrap()[0].0, ARCHIVE);
        // Read again and again, whole and by range, from the local tier:
        // the global copy is in use, and touched once a day at most.
        for _ in 0..3 {
            tiered.get(&at).await.unwrap().bytes().await.unwrap();
            tiered.get_range(&at, 0..10).await.unwrap();
        }
        assert_eq!(touches.count(), 1, "not again within the day");
        *now.lock().unwrap() += Duration::from_secs(23 * 3600);
        tiered.get_range(&at, 0..10).await.unwrap();
        assert_eq!(touches.count(), 1, "23 hours on");
        *now.lock().unwrap() += Duration::from_secs(2 * 3600);
        tiered.get_range(&at, 0..10).await.unwrap();
        tiered.get_range(&at, 0..10).await.unwrap();
        assert_eq!(touches.count(), 2, "a day on: once more, and once only");
        let s = tiered.stats();
        assert_eq!((s.touched, s.touch_failures), (2, 0));
        assert_eq!(origin.gets(ARCHIVE), 1);
    }

    #[tokio::test]
    async fn a_global_hit_from_another_machine_s_download_is_touched() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let at = Path::from(ARCHIVE);
        global.put(&at, bytes().into()).await.unwrap();
        let touches = Arc::new(Touches::default());
        let (_, clock) = clock();
        let tiered = Tiered::new(
            origin.clone(),
            vec![Tier::new("global", global).with_touch(touches.clone())],
        )
        .with_clock(clock);
        tiered.get_range(&at, 0..10).await.unwrap();
        tiered.get(&at).await.unwrap().bytes().await.unwrap();
        assert_eq!(touches.count(), 1);
        assert_eq!(origin.gets(ARCHIVE), 0);
        // What is rewritten is never tiered, so never touched.
        tiered
            .get(&Path::from("imagery/zones/z4/1/2/manifest.json"))
            .await
            .unwrap();
        assert_eq!(touches.count(), 1);
    }
}
