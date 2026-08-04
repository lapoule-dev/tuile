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

/// Pooled native HTTP transport with a hybrid (memory + disk) browser-style
/// cache. Cheap to clone — clones share the pool and cache.
#[derive(Clone)]
pub struct NativeHttp {
    client: ClientWithMiddleware,
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

    /// Builds the transport with an explicit cache configuration.
    pub async fn with_config(cfg: CacheConfig) -> Result<Self, NativeFetchError> {
        let cache = build_cache(&cfg).await?;
        let reqwest_client = reqwest::Client::builder()
            .user_agent(concat!("tuile/", env!("CARGO_PKG_VERSION")))
            // Saturate the fiber: keep many keep-alive connections per host so
            // parallel tile/imagery requests don't serialize on the pool.
            .pool_max_idle_per_host(32)
            .cookie_store(true)
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
        Ok(Self { client })
    }

    /// One GET through the middleware (pool + cache). Token-bearing endpoint
    /// calls bypass the cache.
    async fn get(
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
}
