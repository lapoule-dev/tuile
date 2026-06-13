// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Abstract I/O: the core never knows where bytes come from.
//!
//! Implementations: [`FsFetcher`] (local files: tests, local viewer),
//! `HttpFetcher` (feature `http`, native), a wasm `fetch` implementation
//! lives with the web backend, and `tuile-cesium-ion` will wrap any of
//! them with endpoint resolution and bearer tokens.

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("not found: {0}")]
    NotFound(Url),
    #[error("unsupported url scheme {scheme:?} for {url}")]
    UnsupportedScheme { scheme: String, url: Url },
    #[error("i/o error fetching {url}: {message}")]
    Io { url: Url, message: String },
    #[error("http status {status} for {url}")]
    Status { status: u16, url: Url },
}

/// Abstract byte source. `Send + Sync` bounds are the native baseline; the
/// single-threaded wasm relaxation (`maybe_send`) is an M2 follow-up noted
/// in `docs/01-architecture.md`.
#[async_trait]
pub trait TileFetcher: Send + Sync {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError>;
}

/// Reads `file://` URLs from the local filesystem. Meant for tests and the
/// local viewer; blocking reads, deliberately simple. Not available on wasm
/// (no filesystem, and `Url::to_file_path` is compiled out there).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Clone)]
pub struct FsFetcher;

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl TileFetcher for FsFetcher {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        if url.scheme() != "file" {
            return Err(FetchError::UnsupportedScheme {
                scheme: url.scheme().to_owned(),
                url: url.clone(),
            });
        }
        let path = url
            .to_file_path()
            .map_err(|()| FetchError::NotFound(url.clone()))?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Bytes::from(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(FetchError::NotFound(url.clone()))
            }
            Err(e) => Err(FetchError::Io {
                url: url.clone(),
                message: e.to_string(),
            }),
        }
    }
}

/// HTTP(S) fetcher backed by reqwest (native targets, feature `http`).
#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
#[derive(Debug, Clone, Default)]
pub struct HttpFetcher {
    client: reqwest::Client,
}

#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
#[async_trait]
impl TileFetcher for HttpFetcher {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        let response = self
            .client
            .get(url.clone())
            .send()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[test]
    fn fs_fetcher_reads_and_misses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tile.glb");
        std::fs::write(&path, b"hello").expect("write");

        let fetcher = FsFetcher;
        let url = Url::from_file_path(&path).expect("url");
        let bytes = fetcher
            .fetch(&url)
            .now_or_never()
            .expect("ready")
            .expect("ok");
        assert_eq!(&bytes[..], b"hello");

        let missing = Url::from_file_path(dir.path().join("nope.glb")).expect("url");
        let err = fetcher.fetch(&missing).now_or_never().expect("ready");
        assert!(matches!(err, Err(FetchError::NotFound(_))));
    }

    #[test]
    fn fs_fetcher_rejects_other_schemes() {
        let fetcher = FsFetcher;
        let url = Url::parse("https://example.com/x.glb").expect("url");
        let err = fetcher.fetch(&url).now_or_never().expect("ready");
        assert!(matches!(err, Err(FetchError::UnsupportedScheme { .. })));
    }
}
