// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Native HTTP fetchers for tuile.
//!
//! [`NativeHttp`] is the native-target transport for the viewer apps: one
//! pooled `reqwest` client behind a browser-style HTTP cache (foyer — a hybrid
//! in-memory + on-disk cache with bounded capacity and eviction), exposed
//! through both core fetch seams at once:
//!
//! - [`tuile_cesium_ion::IonHttp`] — bearer GET returning status + body, used
//!   by the ion terrain/imagery connectors.
//! - [`tuile_core::fetch::TileFetcher`] — plain GET returning bytes, used by
//!   everything else.
//!
//! A single instance serves ion *and* imagery, so they share one connection
//! pool and one cache. The cache follows HTTP rules ([`CacheMode::ForceCache`]
//! — immutable tiles are stored regardless of headers, for true offline
//! reuse), except for `api.cesium.com` endpoint calls, which carry short-lived
//! tokens and are never cached.
//!
//! This crate is native-only by construction (it pulls reqwest + foyer); it is
//! deliberately outside the render-agnostic, wasm-able core.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    PsyncIoEngineConfig,
};
use http_cache::FoyerManager;
use http_cache_reqwest::{Cache, CacheMode, HttpCache, HttpCacheOptions};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use tuile_cesium_ion::{HttpResponse, IonError, IonHttp};
use tuile_core::fetch::{FetchError, Fetched, TileFetcher};
use url::Url;

/// Host whose responses carry short-lived ion tokens — never cache them.
const ION_API_HOST: &str = "api.cesium.com";

/// Statuses worth trying again.
///
/// The list is short on purpose. Every status *not* here is treated as the
/// origin's final answer, and that is the important half: a 404 retried five
/// times is five times the latency for the same answer, and a 401 retried is a
/// misconfiguration hidden behind a delay.
fn is_retryable_status(status: u16) -> bool {
    matches!(
        status,
        // Request Timeout, Too Early, Too Many Requests.
        408 | 425 | 429
        // Server-side: the request may well succeed unchanged. 501 and 505 are
        // excluded — the server is saying it will never do this.
        | 500 | 502 | 503 | 504 | 507 | 509
    )
}

/// Whether a transport failure is worth trying again.
///
/// A timeout, a refused connection or a dropped socket are the ordinary weather
/// of a long render; a malformed URL or a body-decode failure will fail
/// identically forever.
fn is_retryable_transport(error: &reqwest_middleware::Error) -> bool {
    let reqwest_middleware::Error::Reqwest(e) = error else {
        // A middleware error is ours, not the network's.
        return false;
    };
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// How hard to try before giving up on a request.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Total attempts, including the first. `1` disables retrying.
    pub attempts: u32,
    /// Delay before the second attempt; doubles thereafter.
    pub initial_backoff: Duration,
    /// Ceiling on that doubling.
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            // Three attempts covers the single dropped connection and the brief
            // 503 without turning a genuinely dead endpoint into a long wait.
            attempts: 3,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
        }
    }
}

impl RetryConfig {
    /// No retrying — one attempt, and its answer is the answer.
    pub fn none() -> Self {
        Self {
            attempts: 1,
            ..Self::default()
        }
    }

    /// How long to wait before attempt number `attempt` (1-based, so the first
    /// call is for attempt 2), spread by `spread`.
    ///
    /// The spread is *derived from the URL*, not drawn at random. It serves the
    /// same purpose as jitter — a thousand tiles that failed together must not
    /// retry together — while leaving a single request's timing reproducible,
    /// which is what a farm comparing two renders needs.
    fn backoff(&self, attempt: u32, spread: u64) -> Duration {
        let doublings = attempt.saturating_sub(1).min(16);
        let base = self
            .initial_backoff
            .saturating_mul(1u32 << doublings)
            .min(self.max_backoff);
        // Up to +50%, so waits never bunch on the same millisecond.
        base + base.mul_f64((spread % 500) as f64 / 1000.0)
    }
}

/// A stable per-URL spread, so concurrent retries desynchronise without an RNG.
fn spread_of(url: &Url) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.as_str().hash(&mut hasher);
    hasher.finish()
}

/// `Retry-After`, in seconds or as an HTTP date, when the origin states one.
///
/// Honouring it is not politeness: a 429 retried on our own schedule is how a
/// render farm gets an API key revoked.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // The date form. Without a date parser in this crate's dependencies we
    // cannot compute the delta, so fall back to the configured backoff rather
    // than guess — treating an unparsed date as "retry now" is the one wrong
    // answer.
    None
}

/// A stable per-user cache directory (`$XDG_CACHE_HOME`, else `~/Library/Caches`
/// on macOS / `~/.cache` elsewhere, else the temp dir), suffixed `tuile`.
/// Persistent across runs — the point of an offline cache.
pub fn default_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("tuile");
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let mac = home.join("Library/Caches");
        let base = if mac.is_dir() {
            mac
        } else {
            home.join(".cache")
        };
        return base.join("tuile");
    }
    std::env::temp_dir().join("tuile-cache")
}

/// Failure building a [`NativeHttp`] (cache device or HTTP client setup).
#[derive(Debug, thiserror::Error)]
#[error("native fetcher setup: {0}")]
pub struct NativeFetchError(String);

/// Capacities for the hybrid cache backing a [`NativeHttp`].
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Directory holding the persistent on-disk cache.
    pub dir: PathBuf,
    /// In-memory tier capacity, bytes (entries weighed by payload size).
    pub memory_bytes: usize,
    /// On-disk tier capacity, bytes.
    pub disk_bytes: usize,
}

impl CacheConfig {
    /// Defaults rooted at `dir`: 256 MiB RAM, 4 GiB disk.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            memory_bytes: 256 << 20,
            disk_bytes: 4 << 30,
        }
    }
}

/// Everything a [`NativeHttp`] needs: where to cache, how long to wait, how
/// hard to try.
///
/// Timeout and retry are configurable because the right answer differs by
/// caller and neither default is safe everywhere. An interactive viewer wants
/// to give up quickly and let the next frame ask again; a farm node rendering
/// one frame for minutes wants to wait, because a tile it abandons does not
/// leave a hole — an ancestor stands in, and the frame is silently coarser than
/// the one rendered on the machine next to it.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub cache: CacheConfig,
    /// Ceiling on one attempt, connect through body. `None` waits forever,
    /// which is only ever right when something above imposes its own deadline.
    pub request_timeout: Option<Duration>,
    /// Ceiling on establishing the connection alone. Separate because a
    /// black-holed host is worth abandoning long before a slow-but-live one.
    pub connect_timeout: Option<Duration>,
    pub retry: RetryConfig,
}

impl TransportConfig {
    /// Defaults rooted at `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self::over(CacheConfig::at(dir))
    }

    /// Default timeouts and retry policy over an already-chosen cache.
    pub fn over(cache: CacheConfig) -> Self {
        Self {
            cache,
            request_timeout: Some(Duration::from_secs(30)),
            connect_timeout: Some(Duration::from_secs(10)),
            retry: RetryConfig::default(),
        }
    }
}

/// Pooled native HTTP transport with a hybrid (memory + disk) browser-style
/// cache. Cheap to clone — clones share the pool and cache.
#[derive(Clone)]
pub struct NativeHttp {
    client: ClientWithMiddleware,
    retry: RetryConfig,
}

impl NativeHttp {
    /// Builds the transport with a persistent cache under `cache_dir` and
    /// default capacities. Must run inside a tokio runtime (foyer is
    /// tokio-native).
    pub async fn new(cache_dir: impl AsRef<Path>) -> Result<Self, NativeFetchError> {
        Self::with_config(CacheConfig::at(cache_dir.as_ref())).await
    }

    /// Builds the transport with a persistent cache in the per-user OS cache
    /// directory ([`default_cache_dir`]). The common case for the apps.
    pub async fn shared() -> Result<Self, NativeFetchError> {
        Self::new(default_cache_dir()).await
    }

    /// Builds the transport with an explicit cache configuration and default
    /// timeouts and retry policy.
    pub async fn with_config(cfg: CacheConfig) -> Result<Self, NativeFetchError> {
        Self::with_transport(TransportConfig::over(cfg)).await
    }

    /// Builds the transport with everything stated explicitly.
    pub async fn with_transport(cfg: TransportConfig) -> Result<Self, NativeFetchError> {
        let cache = build_cache(&cfg.cache).await?;
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("tuile/", env!("CARGO_PKG_VERSION")))
            // Saturate the fiber: keep many keep-alive connections per host so
            // parallel tile/imagery requests don't serialize on the pool. 64
            // matches the bulk path's fetch waves — fewer and every wave pays
            // reconnects between bursts.
            .pool_max_idle_per_host(64)
            .cookie_store(true);
        if let Some(timeout) = cfg.request_timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(timeout) = cfg.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        let reqwest_client = builder
            .build()
            .map_err(|e| NativeFetchError(e.to_string()))?;
        let client = ClientBuilder::new(reqwest_client)
            .with(Cache(HttpCache {
                // Immutable tiles: store regardless of headers (offline reuse).
                mode: CacheMode::ForceCache,
                manager: FoyerManager::new(cache),
                options: HttpCacheOptions::default(),
            }))
            .build();
        Ok(Self {
            client,
            retry: cfg.retry,
        })
    }

    /// One GET through the middleware (pool + cache). Token-bearing endpoint
    /// calls bypass the cache.
    async fn get_once(
        &self,
        url: &Url,
        bearer: Option<&str>,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        let mut req = self.client.get(url.clone());
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        if url.host_str() == Some(ION_API_HOST) {
            req = req.with_extension(CacheMode::NoStore);
        }
        req.send().await
    }

    /// A GET, retried per the configured policy.
    ///
    /// Retrying lives here rather than in the callers so both seams —
    /// [`IonHttp`] and [`TileFetcher`] — get it, and so the decision of what is
    /// worth retrying is made once. Callers still see a single response and
    /// classify its status as they always did.
    async fn get(
        &self,
        url: &Url,
        bearer: Option<&str>,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        let spread = spread_of(url);
        let mut attempt = 1;
        loop {
            let outcome = self.get_once(url, bearer).await;
            let last = attempt >= self.retry.attempts;

            let wait = match &outcome {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if !is_retryable_status(status) {
                        return outcome;
                    }
                    tracing::debug!(%url, status, attempt, "retryable status");
                    // The origin's own pacing wins over ours when it states one.
                    retry_after(response.headers())
                        .unwrap_or_else(|| self.retry.backoff(attempt, spread))
                }
                Err(error) => {
                    if !is_retryable_transport(error) {
                        return outcome;
                    }
                    tracing::debug!(%url, attempt, %error, "retryable transport failure");
                    self.retry.backoff(attempt, spread)
                }
            };

            if last {
                // Out of attempts: hand back whatever the last one said, so the
                // caller reports the real status or error rather than a
                // synthesised "gave up".
                return outcome;
            }
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }
}

#[async_trait]
impl IonHttp for NativeHttp {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        let response = NativeHttp::get(self, url, bearer)
            .await
            .map_err(|e| IonError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let max_age = max_age(response.headers());
        let body = response
            .bytes()
            .await
            .map_err(|e| IonError::Transport(e.to_string()))?;
        Ok(HttpResponse {
            status,
            body,
            max_age,
        })
    }
}

#[async_trait]
impl TileFetcher for NativeHttp {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        let response = NativeHttp::get(self, url, None)
            .await
            .map_err(|e| FetchError::Io {
                url: url.clone(),
                message: e.to_string(),
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(FetchError::NotFound(url.clone()));
        }
        if !status.is_success() {
            return Err(FetchError::Status {
                status: status.as_u16(),
                url: url.clone(),
            });
        }
        response.bytes().await.map_err(|e| FetchError::Io {
            url: url.clone(),
            message: e.to_string(),
        })
    }

    async fn fetch_cacheable(&self, url: &Url) -> Result<Fetched<Bytes>, FetchError> {
        let response = NativeHttp::get(self, url, None)
            .await
            .map_err(|e| FetchError::Io {
                url: url.clone(),
                message: e.to_string(),
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(FetchError::NotFound(url.clone()));
        }
        if !status.is_success() {
            return Err(FetchError::Status {
                status: status.as_u16(),
                url: url.clone(),
            });
        }
        let ttl = max_age(response.headers());
        let value = response.bytes().await.map_err(|e| FetchError::Io {
            url: url.clone(),
            message: e.to_string(),
        })?;
        Ok(Fetched { value, ttl })
    }
}

/// `Cache-Control: max-age=<seconds>`, when the response states one.
///
/// Only `max-age` is read, and only to be passed on to whatever stores the
/// body. Full HTTP freshness — `Expires`, `s-maxage`, revalidation — is the
/// middleware's job one layer down; duplicating it here would be a second,
/// disagreeing implementation. `no-store` and `no-cache` are honoured by
/// returning nothing, so a response the origin marked uncacheable never
/// arrives with a lifetime attached.
fn max_age(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let directives = headers
        .get(reqwest::header::CACHE_CONTROL)?
        .to_str()
        .ok()?
        .to_ascii_lowercase();
    if directives
        .split(',')
        .any(|d| matches!(d.trim(), "no-store" | "no-cache"))
    {
        return None;
    }
    directives.split(',').find_map(|directive| {
        let seconds = directive.trim().strip_prefix("max-age=")?;
        seconds.trim().parse().ok().map(Duration::from_secs)
    })
}

/// Assembles the foyer hybrid cache: a payload-weighed, bounded memory tier in
/// front of a persistent on-disk block store.
async fn build_cache(cfg: &CacheConfig) -> Result<HybridCache<String, Vec<u8>>, NativeFetchError> {
    std::fs::create_dir_all(&cfg.dir).map_err(|e| NativeFetchError(e.to_string()))?;
    let device = FsDeviceBuilder::new(&cfg.dir)
        .with_capacity(cfg.disk_bytes)
        .build()
        .map_err(|e| NativeFetchError(e.to_string()))?;
    HybridCacheBuilder::new()
        .memory(cfg.memory_bytes)
        // Capacity is in bytes: weigh each entry by its payload length.
        .with_weighter(|_k: &String, v: &Vec<u8>| v.len())
        .storage()
        .with_io_engine_config(PsyncIoEngineConfig::new())
        .with_engine_config(BlockEngineConfig::new(device))
        .build()
        .await
        .map_err(|e| NativeFetchError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, CACHE_CONTROL};

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CACHE_CONTROL, HeaderValue::from_str(value).expect("header"));
        h
    }

    #[test]
    fn max_age_is_read_from_cache_control() {
        assert_eq!(
            max_age(&headers("max-age=3600")),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn max_age_survives_company_and_casing() {
        assert_eq!(
            max_age(&headers("public, MAX-AGE=60, immutable")),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn an_uncacheable_response_states_no_lifetime() {
        // Even alongside a max-age: the store must not be handed a lifetime
        // for a body the origin said not to keep.
        assert_eq!(max_age(&headers("no-store, max-age=600")), None);
        assert_eq!(max_age(&headers("no-cache")), None);
    }

    #[test]
    fn a_silent_or_unparsable_header_states_nothing() {
        assert_eq!(max_age(&HeaderMap::new()), None);
        assert_eq!(max_age(&headers("public")), None);
        assert_eq!(max_age(&headers("max-age=soon")), None);
    }

    /// The half that matters: a definitive answer must not be asked again.
    #[test]
    fn a_final_answer_is_not_retried() {
        for status in [200, 204, 301, 400, 401, 403, 404, 410, 501, 505] {
            assert!(
                !is_retryable_status(status),
                "{status} is the origin's final answer"
            );
        }
    }

    #[test]
    fn a_transient_failure_is_retried() {
        for status in [408, 425, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(status), "{status} may yet succeed");
        }
    }

    #[test]
    fn backoff_doubles_and_then_stops() {
        let retry = RetryConfig {
            attempts: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
        };
        // Spread 0: the bare schedule, before desynchronisation.
        assert_eq!(retry.backoff(1, 0), Duration::from_millis(100));
        assert_eq!(retry.backoff(2, 0), Duration::from_millis(200));
        assert_eq!(retry.backoff(3, 0), Duration::from_millis(400));
        assert_eq!(
            retry.backoff(9, 0),
            Duration::from_millis(400),
            "the ceiling holds however many attempts precede it"
        );
    }

    /// A large attempt number must not overflow the doubling into a zero or a
    /// panic — that would turn a slow endpoint into a busy loop.
    #[test]
    fn backoff_survives_an_absurd_attempt_count() {
        let retry = RetryConfig {
            attempts: u32::MAX,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        };
        assert_eq!(retry.backoff(u32::MAX, 0), Duration::from_secs(5));
    }

    /// Two URLs that fail together must not retry together.
    #[test]
    fn the_spread_desynchronises_urls() {
        let a = Url::parse("https://example.test/a").expect("url");
        let b = Url::parse("https://example.test/b").expect("url");
        let retry = RetryConfig::default();
        assert_ne!(
            retry.backoff(1, spread_of(&a)),
            retry.backoff(1, spread_of(&b))
        );
        // ...and the same URL always waits the same, so a render is repeatable.
        assert_eq!(
            retry.backoff(1, spread_of(&a)),
            retry.backoff(1, spread_of(&a))
        );
    }

    /// The spread only ever adds, and never more than half again — a retry must
    /// stay recognisably on schedule.
    #[test]
    fn the_spread_stays_within_half() {
        let retry = RetryConfig {
            attempts: 3,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
        };
        for spread in [0u64, 1, 499, 500, 12345, u64::MAX] {
            let wait = retry.backoff(1, spread);
            assert!(wait >= Duration::from_millis(200), "{spread}");
            assert!(wait < Duration::from_millis(300), "{spread}");
        }
    }

    #[test]
    fn retry_after_seconds_are_honoured() {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_static("120"),
        );
        assert_eq!(retry_after(&h), Some(Duration::from_secs(120)));
    }

    /// An unparsable `Retry-After` must fall through to our own backoff, not to
    /// "immediately" — hammering a 429 is how a key gets revoked.
    #[test]
    fn an_unparsable_retry_after_defers_to_the_backoff() {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retrying_can_be_switched_off() {
        assert_eq!(RetryConfig::none().attempts, 1);
    }

    /// A farm node must be able to state its own deadline; the default is a
    /// viewer's, and both must be reachable.
    #[test]
    fn timeouts_are_configurable_and_bounded_by_default() {
        let cfg = TransportConfig::at("/tmp/whatever");
        assert!(cfg.request_timeout.is_some());
        assert!(cfg.connect_timeout.is_some());
        assert!(cfg.connect_timeout < cfg.request_timeout);
    }
}
