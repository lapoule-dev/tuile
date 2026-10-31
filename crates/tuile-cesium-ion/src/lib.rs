// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::lock::Mutex;
use serde::Deserialize;
use std::sync::Arc;
use tuile_core::fetch::{FetchError, TileFetcher};
use url::Url;

// The catalog behind a proxy: a server's, never a page's.
#[cfg(not(target_arch = "wasm32"))]
pub mod proxy;
pub mod sources;
mod terrain;
#[cfg(test)]
mod tests_support;
pub mod tms;

pub use sources::IonSources;
pub use terrain::IonTerrainSource;

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
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait IonHttp: Send + Sync {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError>;

    /// [`IonHttp::get`], answered by the origin and never by a cache, and
    /// not kept by one.
    ///
    /// For an answer that hands out a short-lived credential: ion's endpoint
    /// with its asset token, and a tileset root that hands out the session
    /// its content is fetched under. A cache that kept one would answer with
    /// a credential long expired, and every request made under it would be
    /// refused. A transport with no cache needs nothing more than `get`.
    async fn get_fresh(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        self.get(url, bearer).await
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Bytes,
    /// `Cache-Control: max-age`, when the origin stated one. Carried so callers
    /// that store what they fetch can honour the origin's own freshness rather
    /// than inventing a lifetime for it.
    pub max_age: Option<std::time::Duration>,
}

/// An attribution that consumers must display (ion terms of use).
#[derive(Debug, Clone, Deserialize)]
pub struct Attribution {
    pub html: String,
    #[serde(default)]
    pub collapsible: bool,
}

impl Attribution {
    /// The credit this attribution asks for, out of its HTML: the words it
    /// shows, the logo it shows, and where it leads.
    ///
    /// ion's attributions are short fragments — a span, a link, an image —
    /// so this reads exactly those and nothing more: the text between the
    /// tags with the common entities decoded, the first `<img>`'s `src` and
    /// `alt`, the first `<a>`'s `href`. A logo with no words around it is
    /// credited by its `alt`.
    pub fn credit(&self) -> tuile_planetary::Credit {
        let html = self.html.as_str();
        let mut text = String::new();
        let mut logo = None;
        let mut alt = None;
        let mut link = None;
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            text.push_str(&rest[..open]);
            let Some(close) = rest[open..].find('>') else {
                rest = "";
                break;
            };
            let tag = &rest[open + 1..open + close];
            let name = tag
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if name == "img" && logo.is_none() {
                logo = attribute(tag, "src").and_then(|s| Url::parse(&s).ok());
                alt = attribute(tag, "alt");
            } else if name == "a" && link.is_none() {
                link = attribute(tag, "href").and_then(|s| Url::parse(&s).ok());
            }
            rest = &rest[open + close + 1..];
        }
        text.push_str(rest);
        let text = decode_entities(&text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let text = if text.is_empty() {
            alt.unwrap_or_default()
        } else {
            text
        };
        tuile_planetary::Credit { text, logo, link }
    }
}

/// The value of `name="…"` (or `'…'`) in a tag's inside, entities decoded.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(name) {
        let start = from + at;
        from = start + name.len();
        let before = lower[..start].chars().next_back();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let after = lower[from..].trim_start();
        let Some(after) = after.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        let offset = tag.len() - after.len();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let value = &tag[offset + 1..];
        let end = value.find(quote)?;
        return Some(decode_entities(&value[..end]));
    }
    None
}

/// The entities an attribution is likely to carry.
fn decode_entities(s: &str) -> String {
    s.replace("&copy;", "©")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
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
    options: Option<ImageryOptions>,
    #[serde(default)]
    attributions: Vec<Attribution>,
}

/// Provider-specific options of an external imagery endpoint (e.g. Bing:
/// `key`, `url`, `mapStyle`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageryOptions {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "mapStyle", default)]
    pub map_style: Option<String>,
}

/// A resolved 3D Tiles asset endpoint.
#[derive(Debug, Clone)]
pub struct Tiles3dEndpoint {
    /// URL of the asset's tileset.json — ion's own, or another service's
    /// when ion points there, with what that service wants on it.
    pub url: Url,
    /// Short-lived asset-scoped token (NOT the account token). Empty for a
    /// tileset ion does not host: then no bearer is sent at all.
    pub access_token: String,
    pub attributions: Vec<Attribution>,
}

impl Tiles3dEndpoint {
    /// The bearer its requests carry, if any.
    fn bearer(&self) -> Option<&str> {
        Some(self.access_token.as_str()).filter(|t| !t.is_empty())
    }
}

/// A resolved terrain asset endpoint (quantized-mesh). Transport-level: the
/// `layer.json` and `.terrain` tiles are fetched relative to `url` with the
/// asset-scoped bearer. Parsing `layer.json` is `tuile-terrain`'s job.
#[derive(Debug, Clone)]
pub struct TerrainEndpoint {
    /// Base URL of the terrain depot (layer.json + tiles are relative to it).
    pub url: Url,
    /// Short-lived asset-scoped token (NOT the account token).
    pub access_token: String,
    pub attributions: Vec<Attribution>,
}

/// A resolved imagery asset endpoint. Transport-level only.
///
/// `url` is `None` for external providers (e.g. Bing), where the tile layout
/// lives in [`ImageryEndpoint::options`] instead; it is `Some` for
/// ion-hosted slippy-map imagery.
#[derive(Debug, Clone)]
pub struct ImageryEndpoint {
    /// Base URL of an ion-hosted imagery service (`None` for external types).
    pub url: Option<Url>,
    pub access_token: String,
    /// Set when ion proxies an external provider (e.g. `"BING"`); tile URL
    /// layouts differ per provider then.
    pub external_type: Option<String>,
    /// Provider-specific options (Bing: key, url, mapStyle).
    pub options: ImageryOptions,
    pub attributions: Vec<Attribution>,
}

impl ImageryEndpoint {
    /// Slippy-map tile URL (`{base}/{z}/{x}/{y}.{ext}`) for ion-hosted
    /// imagery (`external_type == None`). Errors for external providers,
    /// which have no ion base URL (use their dedicated connector instead).
    pub fn tile_url(&self, z: u32, x: u64, y: u64, ext: &str) -> Result<Url, IonError> {
        let base = self
            .url
            .as_ref()
            .ok_or_else(|| IonError::Transport("imagery endpoint has no base url".into()))?;
        Ok(base.join(&format!("{z}/{x}/{y}.{ext}"))?)
    }
}

#[derive(Debug, Clone)]
pub enum AssetEndpoint {
    Tiles3d(Tiles3dEndpoint),
    Terrain(TerrainEndpoint),
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
            // A tileset ion does not host itself names it in its options,
            // with whatever the other service wants on its URL, and carries
            // no ion token: nothing of ion's is sent there.
            "3DTILES" => Ok(AssetEndpoint::Tiles3d(Tiles3dEndpoint {
                url: match json.external_type.as_deref() {
                    Some(_) => endpoint_url(&json.options.and_then(|o| o.url))?,
                    None => endpoint_url(&json.url)?,
                },
                access_token: json.access_token.unwrap_or_default(),
                attributions: json.attributions,
            })),
            "TERRAIN" => Ok(AssetEndpoint::Terrain(TerrainEndpoint {
                url: endpoint_url(&json.url)?,
                access_token: json.access_token.unwrap_or_default(),
                attributions: json.attributions,
            })),
            "IMAGERY" => Ok(AssetEndpoint::Imagery(ImageryEndpoint {
                url: match json.url.as_deref() {
                    Some(u) if !u.is_empty() => Some(Url::parse(u)?),
                    _ => None,
                },
                access_token: json.access_token.unwrap_or_default(),
                external_type: json.external_type,
                options: json.options.unwrap_or_default(),
                attributions: json.attributions,
            })),
            other => Err(IonError::UnsupportedAssetType {
                asset_id,
                kind: other.to_owned(),
            }),
        }
    }

    /// Resolves a TERRAIN asset endpoint (errors on any other asset type).
    pub async fn terrain_endpoint(&self, asset_id: u64) -> Result<TerrainEndpoint, IonError> {
        match self.asset_endpoint(asset_id).await? {
            AssetEndpoint::Terrain(t) => Ok(t),
            AssetEndpoint::Tiles3d(_) | AssetEndpoint::Imagery(_) => {
                Err(IonError::UnsupportedAssetType {
                    asset_id,
                    kind: "expected TERRAIN".into(),
                })
            }
        }
    }

    /// The underlying HTTP transport (used by source helpers).
    pub fn http(&self) -> &H {
        &self.http
    }

    pub fn account_token(&self) -> &str {
        &self.account_token
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
            AssetEndpoint::Terrain(_) => {
                return Err(IonError::UnsupportedAssetType {
                    asset_id: self.asset_id,
                    kind: "TERRAIN (use IonTerrainSource, not a TileFetcher)".into(),
                })
            }
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

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<H: IonHttp> TileFetcher for IonTileFetcher<H> {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        let endpoint = self
            .resolved()
            .await
            .map_err(|e| ion_to_fetch_error(e, url))?;
        // The root document is asked of the origin every time: it may hand
        // out the session everything beneath is fetched under.
        let root = *url == endpoint.url;
        let mut endpoint = endpoint;
        for attempt in 0..2 {
            let http = &self.client.http;
            let response = if root {
                http.get_fresh(url, endpoint.bearer()).await
            } else {
                http.get(url, endpoint.bearer()).await
            }
            .map_err(|e| ion_to_fetch_error(e, url))?;
            match response.status {
                200 => return Ok(response.body),
                404 => return Err(FetchError::NotFound(url.clone())),
                // Asset token expired: re-resolve the endpoint once.
                401 | 403 if attempt == 0 => {
                    endpoint = self
                        .refresh()
                        .await
                        .map_err(|e| ion_to_fetch_error(e, url))?;
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
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
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
        let max_age = response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .and_then(max_age);
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

/// `max-age` of a `Cache-Control` value, unless the value forbids keeping
/// the response at all.
#[cfg_attr(not(feature = "reqwest"), allow(dead_code))]
fn max_age(cache_control: &str) -> Option<std::time::Duration> {
    let directives: Vec<&str> = cache_control.split(',').map(str::trim).collect();
    if directives
        .iter()
        .any(|d| d.eq_ignore_ascii_case("no-store") || d.eq_ignore_ascii_case("no-cache"))
    {
        return None;
    }
    directives.iter().find_map(|d| {
        let (name, value) = d.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("max-age")
            .then(|| value.trim().parse().ok())
            .flatten()
            .map(std::time::Duration::from_secs)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::MockHttp;

    #[test]
    fn max_age_is_read_and_no_store_wins() {
        assert_eq!(
            max_age("public, max-age=86400"),
            Some(std::time::Duration::from_secs(86_400))
        );
        assert_eq!(max_age("max-age=60, no-store"), None);
        assert_eq!(max_age("public"), None);
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

    const EXTERNAL_ENDPOINT: &str = include_str!("../fixtures/external-3dtiles-endpoint.json");
    const EXTERNAL_ROOT: &str = include_str!("../fixtures/external-3dtiles-root.json");
    const EXTERNAL_ENDPOINT_URL: &str = "https://api.cesium.com/v1/assets/2275207/endpoint";
    const EXTERNAL_ROOT_URL: &str = "https://tile.googleapis.com/v1/3dtiles/root.json?key=FAKE_KEY";

    /// A 3D Tiles asset ion does not host answers with no `url` and no
    /// token: the tileset is at `options.url`, which carries its own key.
    /// It resolves to that URL, its credits come with it, and nothing is
    /// sent to the other service with an ion bearer.
    #[test]
    fn an_external_3dtiles_asset_resolves_to_the_url_in_its_options() {
        let http = Arc::new(MockHttp::default());
        http.push(EXTERNAL_ENDPOINT_URL, 200, EXTERNAL_ENDPOINT);
        http.push(EXTERNAL_ROOT_URL, 200, EXTERNAL_ROOT);
        let fetcher =
            IonTileFetcher::new(IonClient::new(Arc::clone(&http), "account-token"), 2_275_207);
        let (url, root, credits) = futures_executor::block_on(async {
            let url = fetcher.tileset_url().await.expect("resolve");
            let root = fetcher.fetch(&url).await.expect("root");
            (url, root, fetcher.attributions().await.expect("credits"))
        });
        assert_eq!(url.as_str(), EXTERNAL_ROOT_URL);
        assert_eq!(&root[..], EXTERNAL_ROOT.as_bytes());
        assert_eq!(credits.len(), 2);
        let calls = http.calls();
        assert_eq!(calls[0].1.as_deref(), Some("account-token"), "the endpoint");
        assert_eq!(calls[1].0, EXTERNAL_ROOT_URL);
        assert_eq!(calls[1].1, None, "no ion bearer to another service");
        assert_eq!(
            http.fresh(),
            vec![EXTERNAL_ROOT_URL.to_owned()],
            "the root document, which hands out the session, is never answered from a cache"
        );
    }

    /// The key on the root URL and the session in the root's content reach
    /// every request below, through the external tilesets too: the tree is
    /// parsed by the core like any other, and the fetcher asks exactly the
    /// URLs it is given.
    #[test]
    fn the_key_and_the_session_reach_every_request_below_the_root() {
        use tuile_core::tileset::Tileset;
        let http = Arc::new(MockHttp::default());
        http.push(EXTERNAL_ENDPOINT_URL, 200, EXTERNAL_ENDPOINT);
        let fetcher =
            IonTileFetcher::new(IonClient::new(Arc::clone(&http), "account-token"), 2_275_207);
        let url = futures_executor::block_on(fetcher.tileset_url()).expect("resolve");
        let tileset = Tileset::from_json_bytes(EXTERNAL_ROOT.as_bytes(), &url).expect("parse");
        let mut urls = Vec::new();
        for i in 0..tileset.len() {
            let id = tuile_core::source::TileId(i as u64);
            if let Some(c) = &tileset.tile(id).content {
                urls.push(c.url.clone());
            }
        }
        assert_eq!(urls.len(), 2);
        for u in &urls {
            let q: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
            assert!(q.contains(&("session".into(), "FAKE_SESSION".into())), "{u}");
            assert!(q.contains(&("key".into(), "FAKE_KEY".into())), "{u}");
        }
        let glb = urls.iter().find(|u| u.path().ends_with(".glb")).expect("a glb");
        http.push(glb.as_str(), 200, "glTF");
        let body = futures_executor::block_on(fetcher.fetch(glb)).expect("content");
        assert_eq!(&body[..], b"glTF");
        let last = http.calls().pop().expect("a call");
        assert_eq!(last, (glb.to_string(), None));
        assert!(http.fresh().is_empty(), "content is cached like any tile");
    }

    /// An attribution's HTML gives the words to show, the logo to show and
    /// where it leads: a logo alone is credited by its `alt`, and a plain
    /// sentence keeps its words with its entities decoded.
    #[test]
    fn an_attribution_reads_as_words_a_logo_and_a_link() {
        let endpoint: EndpointJson = serde_json::from_str(EXTERNAL_ENDPOINT).expect("fixture");
        let credits: Vec<_> = endpoint.attributions.iter().map(Attribution::credit).collect();
        assert_eq!(credits[0].text, "Cesium ion");
        assert_eq!(
            credits[0].logo.as_ref().map(Url::as_str),
            Some("https://assets.ion.cesium.com/ion-credit.png")
        );
        assert_eq!(
            credits[0].link.as_ref().map(Url::as_str),
            Some("https://cesium.com/")
        );
        assert_eq!(credits[1].text, "Google");
        assert!(credits[1]
            .logo
            .as_ref()
            .is_some_and(|u| u.path().ends_with("google-credit.png")));
        assert_eq!(credits[1].link, None);
        let words = Attribution {
            html: "<span>Data &copy; Example &amp; Co,\n  2026</span>".into(),
            collapsible: true,
        }
        .credit();
        assert_eq!(words.text, "Data © Example & Co, 2026");
        assert_eq!((words.logo, words.link), (None, None));
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
    fn resolves_terrain_endpoint() {
        let http = Arc::new(MockHttp::default());
        http.push(
            "https://api.cesium.com/v1/assets/1/endpoint",
            200,
            r#"{ "type": "TERRAIN",
                 "url": "https://assets.ion.cesium.com/1/CesiumWorldTerrain/v1.2/",
                 "accessToken": "terrain-token",
                 "attributions": [{ "html": "<span>USGS</span>", "collapsible": true }] }"#,
        );
        let client = IonClient::new(Arc::clone(&http), "account-token");
        let endpoint = futures_executor::block_on(client.asset_endpoint(1)).expect("resolve");
        let AssetEndpoint::Terrain(terrain) = endpoint else {
            unreachable!("terrain expected");
        };
        assert_eq!(terrain.access_token, "terrain-token");
        assert!(terrain.url.as_str().ends_with("/v1.2/"));
        assert_eq!(terrain.attributions.len(), 1);
    }

    #[test]
    fn unknown_asset_type_is_a_typed_error() {
        let http = Arc::new(MockHttp::default());
        http.push(
            "https://api.cesium.com/v1/assets/3/endpoint",
            200,
            r#"{ "type": "CZML", "url": "https://x/", "accessToken": "t" }"#,
        );
        let client = IonClient::new(Arc::clone(&http), "account-token");
        let err = futures_executor::block_on(client.asset_endpoint(3)).expect_err("unsupported");
        assert!(matches!(err, IonError::UnsupportedAssetType { .. }));
    }
}
