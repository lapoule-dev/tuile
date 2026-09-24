// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a tile comes from the first time.

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::fetch::{FetchError, TileFetcher};
use url::Url;

/// A source failure. `Ok(None)` is not one: it is a tile the source does not
/// have, which a server answers with a 404.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct UpstreamError(pub String);

/// The origin of a layer's tiles.
#[async_trait]
pub trait Upstream: Send + Sync {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError>;
}

const HTTP_NOT_FOUND: u16 = 404;

/// A source addressed by a URL template with `{z}`, `{x}` and `{y}`.
pub struct TemplateUpstream<F> {
    template: String,
    fetcher: F,
}

impl<F: TileFetcher> TemplateUpstream<F> {
    pub fn new(template: impl Into<String>, fetcher: F) -> Self {
        Self { template: template.into(), fetcher }
    }

    pub fn url(&self, level: u8, x: u32, y: u32) -> Result<Url, UpstreamError> {
        let s = self
            .template
            .replace("{z}", &level.to_string())
            .replace("{x}", &x.to_string())
            .replace("{y}", &y.to_string());
        Url::parse(&s).map_err(|e| UpstreamError(format!("{s}: {e}")))
    }
}

#[async_trait]
impl<F: TileFetcher> Upstream for TemplateUpstream<F> {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        let url = self.url(level, x, y)?;
        match self.fetcher.fetch(&url).await {
            Ok(bytes) if bytes.is_empty() => Ok(None),
            Ok(bytes) => Ok(Some(bytes)),
            Err(FetchError::NotFound(_)) | Err(FetchError::Status { status: HTTP_NOT_FOUND, .. }) => Ok(None),
            Err(e) => Err(UpstreamError(e.to_string())),
        }
    }
}
