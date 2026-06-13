// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-cesium-ion
//!
//! Source connector for Cesium ion: resolves asset endpoints
//! (`/v1/assets/{id}/endpoint`), carries bearer tokens, refreshes them on
//! 401/403, and exposes ion-hosted **3D Tiles** as a plain
//! [`tuile_core::fetch::TileFetcher`] — the core never knows ion exists.
//!
//! **Imagery** assets resolve the same way ([`ImageryEndpoint`]); this
//! crate is their *transport* (tile URLs + auth). Draping imagery onto
//! geometry is the core's raster-overlay module (see roadmap) — not this
//! crate's business.
//!
//! Tokens are never stored in any repository; pass them in (CLI flag, env
//! var read by the caller). Attribution strings returned by ion must be
//! displayed by consumers ([`Attribution`]).

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::lock::Mutex;
use serde::Deserialize;
use std::sync::Arc;
use tuile_core::fetch::{FetchError, TileFetcher};
use url::Url;

pub const ION_API_BASE: &str = "https://api.cesium.com/";

#[derive(Debug, thiserror::Error)]
pub enum IonError {
    #[error("http transport: {0}")]
    Transport(String),
    #[error("ion api status {status} for {url}")]
    Status { status: u16, url: Url },
    #[error("invalid endpoint response: {0}")]
    InvalidEndpoint(#[from] serde_json::Error),
    #[error("asset {asset_id} has unsupported endpoint type {kind:?}")]
    UnsupportedAssetType { asset_id: u64, kind: String },
    #[error("invalid url: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

/// Minimal HTTP transport seam (lets tests run without network and hosts
/// pick their client). `bearer` goes out as `Authorization: Bearer …`.
#[async_trait]
pub trait IonHttp: Send + Sync {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError>;
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Bytes,
}

/// An attribution that consumers must display (ion terms of use).
#[derive(Debug, Clone, Deserialize)]
pub struct Attribution {
    pub html: String,
    #[serde(default)]
    pub collapsible: bool,
}

/// Raw shape of `/v1/assets/{id}/endpoint`.
#[derive(Debug, Clone, Deserialize)]
struct EndpointJson {
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "externalType")]
    external_type: Option<String>,
    #[serde(default)]
    attributions: Vec<Attribution>,
}

/// A resolved 3D Tiles asset endpoint.
#[derive(Debug, Clone)]
pub struct Tiles3dEndpoint {
    /// URL of the asset's tileset.json.
    pub url: Url,
    /// Short-lived asset-scoped token (NOT the account token).
    pub access_token: String,
    pub attributions: Vec<Attribution>,
}

/// A resolved imagery asset endpoint. Transport-level only: build tile URLs
/// with [`ImageryEndpoint::tile_url`] and fetch them with the same bearer.
#[derive(Debug, Clone)]
pub struct ImageryEndpoint {
    /// Base URL of the imagery service.
    pub url: Url,
    pub access_token: String,
    /// Set when ion proxies an external provider (e.g. `"BING"`); tile URL
    /// layouts differ per provider then.
    pub external_type: Option<String>,
    pub attributions: Vec<Attribution>,
}

impl ImageryEndpoint {
    /// Slippy-map tile URL (`{base}/{z}/{x}/{y}.{ext}`) for ion-hosted
    /// imagery (`external_type == None`).
    pub fn tile_url(&self, z: u32, x: u64, y: u64, ext: &str) -> Result<Url, IonError> {
        Ok(self.url.join(&format!("{z}/{x}/{y}.{ext}"))?)
    }
}

#[derive(Debug, Clone)]
pub enum AssetEndpoint {
    Tiles3d(Tiles3dEndpoint),
    Imagery(ImageryEndpoint),
}

/// Talks to the ion REST API with an account token.
pub struct IonClient<H: IonHttp> {
    http: Arc<H>,
    api_base: Url,
    account_token: String,
}

impl<H: IonHttp> IonClient<H> {
    pub fn new(http: Arc<H>, account_token: impl Into<String>) -> Self {
        Self {
            http,
            api_base: Url::parse(ION_API_BASE).unwrap_or_else(|_| unreachable!("static url")),
            account_token: account_token.into(),
        }
    }

    /// Overrides the API base (self-hosted ion, tests).
    pub fn with_api_base(mut self, base: Url) -> Self {
        self.api_base = base;
        self
    }

    /// Resolves `/v1/assets/{id}/endpoint` into a typed endpoint.
    pub async fn asset_endpoint(&self, asset_id: u64) -> Result<AssetEndpoint, IonError> {
        let url = self
            .api_base
            .join(&format!("v1/assets/{asset_id}/endpoint"))?;
        let response = self.http.get(&url, Some(&self.account_token)).await?;
        if response.status != 200 {
            return Err(IonError::Status {
                status: response.status,
                url,
            });
        }
        let json: EndpointJson = serde_json::from_slice(&response.body)?;
        let endpoint_url = |u: &Option<String>| -> Result<Url, IonError> {
            Ok(Url::parse(u.as_deref().unwrap_or_default())?)
        };
        match json.kind.as_str() {
            "3DTILES" => Ok(AssetEndpoint::Tiles3d(Tiles3dEndpoint {
                url: endpoint_url(&json.url)?,
                access_token: json.access_token.unwrap_or_default(),
                attributions: json.attributions,
            })),
            "IMAGERY" => Ok(AssetEndpoint::Imagery(ImageryEndpoint {
                url: endpoint_url(&json.url)?,
                access_token: json.access_token.unwrap_or_default(),
                external_type: json.external_type,
                attributions: json.attributions,
            })),
            other => Err(IonError::UnsupportedAssetType {
                asset_id,
                kind: other.to_owned(),
            }),
        }
    }
}

/// A [`TileFetcher`] for one ion-hosted 3D Tiles asset: resolves the
/// endpoint lazily, attaches the asset-scoped bearer to every fetch, and
/// re-resolves once on 401/403 (token expiry).
pub struct IonTileFetcher<H: IonHttp> {
    client: IonClient<H>,
    asset_id: u64,
    endpoint: Mutex<Option<Tiles3dEndpoint>>,
}

impl<H: IonHttp> IonTileFetcher<H> {
    pub fn new(client: IonClient<H>, asset_id: u64) -> Self {
        Self {
            client,
            asset_id,
            endpoint: Mutex::new(None),
        }
    }

    /// The asset's tileset.json URL (resolves the endpoint if needed).
    /// Hand this to `Tileset::from_json_bytes` after fetching it through
    /// this fetcher.
    pub async fn tileset_url(&self) -> Result<Url, IonError> {
        Ok(self.resolved().await?.url)
    }

    /// Attributions to display for this asset.
    pub async fn attributions(&self) -> Result<Vec<Attribution>, IonError> {
        Ok(self.resolved().await?.attributions)
    }

    async fn resolved(&self) -> Result<Tiles3dEndpoint, IonError> {
        let mut guard = self.endpoint.lock().await;
        if let Some(e) = guard.as_ref() {
            return Ok(e.clone());
        }
        let e = match self.client.asset_endpoint(self.asset_id).await? {
            AssetEndpoint::Tiles3d(e) => e,
            AssetEndpoint::Imagery(_) => {
                return Err(IonError::UnsupportedAssetType {
                    asset_id: self.asset_id,
                    kind: "IMAGERY (use ImageryEndpoint, not a TileFetcher)".into(),
                })
            }
        };
        *guard = Some(e.clone());
        Ok(e)
    }

    async fn refresh(&self) -> Result<Tiles3dEndpoint, IonError> {
        let mut guard = self.endpoint.lock().await;
        *guard = None;
        drop(guard);
        self.resolved().await
    }
}

fn ion_to_fetch_error(e: IonError, url: &Url) -> FetchError {
    FetchError::Io {
        url: url.clone(),
        message: e.to_string(),
    }
}

#[async_trait]
impl<H: IonHttp> TileFetcher for IonTileFetcher<H> {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        let endpoint = self
            .resolved()
            .await
            .map_err(|e| ion_to_fetch_error(e, url))?;
        let mut token = endpoint.access_token;
        for attempt in 0..2 {
            let response = self
                .client
                .http
                .get(url, Some(&token))
                .await
                .map_err(|e| ion_to_fetch_error(e, url))?;
            match response.status {
                200 => return Ok(response.body),
                404 => return Err(FetchError::NotFound(url.clone())),
                // Asset token expired: re-resolve the endpoint once.
                401 | 403 if attempt == 0 => {
                    token = self
                        .refresh()
                        .await
                        .map_err(|e| ion_to_fetch_error(e, url))?
                        .access_token;
                }
                status => {
                    return Err(FetchError::Status {
                        status,
                        url: url.clone(),
                    })
                }
            }
        }
        Err(FetchError::Status {
            status: 401,
            url: url.clone(),
        })
    }
}

/// reqwest-backed [`IonHttp`] (native targets, feature `reqwest`).
#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
#[derive(Debug, Clone, Default)]
pub struct ReqwestHttp {
    client: reqwest::Client,
}

#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
#[async_trait]
impl IonHttp for ReqwestHttp {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        let mut req = self.client.get(url.clone());
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        let response = req
            .send()
            .await
            .map_err(|e| IonError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|e| IonError::Transport(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// Scripted transport: per-URL response queues + a call log.
    #[derive(Default)]
    struct MockHttp {
        responses: StdMutex<HashMap<String, Vec<HttpResponse>>>,
        log: StdMutex<Vec<(String, Option<String>)>>,
    }

    impl MockHttp {
        fn push(&self, url: &str, status: u16, body: &str) {
            self.responses
                .lock()
                .expect("lock")
                .entry(url.to_owned())
                .or_default()
                .push(HttpResponse {
                    status,
                    body: Bytes::from(body.to_owned()),
                });
        }
        fn calls(&self) -> Vec<(String, Option<String>)> {
            self.log.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl IonHttp for MockHttp {
        async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
            self.log
                .lock()
                .expect("lock")
                .push((url.to_string(), bearer.map(str::to_owned)));
            let mut map = self.responses.lock().expect("lock");
            let queue = map
                .get_mut(url.as_str())
                .unwrap_or_else(|| unreachable!("unexpected request to {url}"));
            Ok(if queue.len() > 1 {
                queue.remove(0)
            } else {
                queue[0].clone()
            })
        }
    }

    const ENDPOINT_URL: &str = "https://api.cesium.com/v1/assets/1415/endpoint";

    fn endpoint_body(token: &str) -> String {
        format!(
            r#"{{ "type": "3DTILES",
                  "url": "https://assets.ion.cesium.com/1415/tileset.json",
                  "accessToken": "{token}",
                  "attributions": [{{ "html": "<span>Data © Example</span>", "collapsible": true }}] }}"#
        )
    }

    #[test]
    fn resolves_3dtiles_endpoint_and_attaches_asset_bearer() {
        let http = Arc::new(MockHttp::default());
        http.push(ENDPOINT_URL, 200, &endpoint_body("asset-token-1"));
        http.push("https://assets.ion.cesium.com/1415/tileset.json", 200, "{}");

        let fetcher = IonTileFetcher::new(IonClient::new(Arc::clone(&http), "account-token"), 1415);
        futures_executor::block_on(async {
            let url = fetcher.tileset_url().await.expect("resolve");
            assert_eq!(
                url.as_str(),
                "https://assets.ion.cesium.com/1415/tileset.json"
            );
            let body = fetcher.fetch(&url).await.expect("fetch");
            assert_eq!(&body[..], b"{}");
            let attributions = fetcher.attributions().await.expect("attributions");
            assert_eq!(attributions.len(), 1);
        });

        let calls = http.calls();
        // Endpoint resolution uses the ACCOUNT token…
        assert_eq!(calls[0].1.as_deref(), Some("account-token"));
        // …tile fetches use the asset-scoped token.
        assert_eq!(calls[1].1.as_deref(), Some("asset-token-1"));
    }

    #[test]
    fn refreshes_token_once_on_401() {
        let http = Arc::new(MockHttp::default());
        http.push(ENDPOINT_URL, 200, &endpoint_body("expired"));
        http.push(ENDPOINT_URL, 200, &endpoint_body("fresh"));
        let tile = "https://assets.ion.cesium.com/1415/a.glb";
        http.push(tile, 401, "");
        http.push(tile, 200, "GLB");

        let fetcher = IonTileFetcher::new(IonClient::new(Arc::clone(&http), "account-token"), 1415);
        let url = Url::parse(tile).expect("url");
        let body = futures_executor::block_on(fetcher.fetch(&url)).expect("fetch after refresh");
        assert_eq!(&body[..], b"GLB");

        let calls = http.calls();
        let tile_calls: Vec<_> = calls.iter().filter(|(u, _)| u == tile).collect();
        assert_eq!(tile_calls.len(), 2);
        assert_eq!(tile_calls[0].1.as_deref(), Some("expired"));
        assert_eq!(
            tile_calls[1].1.as_deref(),
            Some("fresh"),
            "retried with the re-resolved token"
        );
    }

    #[test]
    fn resolves_imagery_endpoint_and_builds_tile_urls() {
        let http = Arc::new(MockHttp::default());
        http.push(
            "https://api.cesium.com/v1/assets/2/endpoint",
            200,
            r#"{ "type": "IMAGERY",
                 "url": "https://assets.ion.cesium.com/2/",
                 "accessToken": "imagery-token",
                 "attributions": [] }"#,
        );
        let client = IonClient::new(Arc::clone(&http), "account-token");
        let endpoint = futures_executor::block_on(client.asset_endpoint(2)).expect("resolve");
        let AssetEndpoint::Imagery(imagery) = endpoint else {
            unreachable!("imagery expected");
        };
        assert_eq!(imagery.access_token, "imagery-token");
        let tile = imagery.tile_url(3, 5, 7, "jpg").expect("tile url");
        assert_eq!(tile.as_str(), "https://assets.ion.cesium.com/2/3/5/7.jpg");
    }

    #[test]
    fn terrain_assets_are_a_typed_error() {
        let http = Arc::new(MockHttp::default());
        http.push(
            "https://api.cesium.com/v1/assets/3/endpoint",
            200,
            r#"{ "type": "TERRAIN", "url": "https://x/", "accessToken": "t" }"#,
        );
        let client = IonClient::new(Arc::clone(&http), "account-token");
        let err = futures_executor::block_on(client.asset_endpoint(3)).expect_err("unsupported");
        assert!(matches!(err, IonError::UnsupportedAssetType { .. }));
    }
}
