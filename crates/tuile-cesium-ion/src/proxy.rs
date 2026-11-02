// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The catalog behind a proxy: ion's asset endpoints, and the tiles behind
//! them, answered for a client that never holds a credential.
//!
//! A page cannot keep a secret: whatever token it is given, anyone who opens
//! it can read. So a page's viewer is pointed at this proxy as its catalog,
//! and the proxy is the one that holds the account token. It speaks the
//! subset of ion's API the connector speaks ([`crate::IonSources`]), so the
//! client is the same code with another API base and no token:
//!
//! | The client asks | The proxy does |
//! |---|---|
//! | `v1/assets/{id}/endpoint` | asks ion with the account token, and answers the endpoint with its URLs pointing back here and **no token in it**: no account token, no asset token, no imagery key |
//! | `assets/{id}/{path}` | fetches `{path}` under the asset's own URL with the asset's short-lived token, which it refreshes itself |
//! | `bing/{id}/REST/v1/Imagery/Metadata/{style}` | asks the imagery service for its metadata with the key ion gave for the asset |
//! | `tiles3d/{id}/{path}?session=…` | for a 3D Tiles asset ion points at on another service: fetches `{path}` from that service with the key ion gave, and answers tileset documents with their references pointing back here |
//!
//! The imagery service's tiles themselves are **not** proxied: the metadata
//! names public tile URLs that carry no key, and the client fetches them from
//! the service's own CDN as it always has — fewer bytes through the proxy,
//! nothing secret on the way.
//!
//! A 3D Tiles service's tiles **are** proxied, unlike the imagery's: every
//! request it answers needs the key, which must not reach the client.
//!
//! **Not an open proxy.** It answers for the assets it was configured with,
//! for these four routes, for relative paths of a narrow alphabet and the
//! two query parameters a terrain request carries; everything else is a 404
//! or a 400 before anything goes upstream. And a reply is never sent if it
//! would carry the account token, an asset token or an imagery key, whatever
//! the upstream put in it.
//!
//! [`Catalog`] is the logic, with no web framework in it, over the same
//! [`IonHttp`] transport the connector uses; [`router`] puts it on `axum`,
//! with the cache and CORS headers.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;
use url::Url;

use crate::{IonHttp, ION_API_BASE};

/// How long an asset's endpoint — and with it its short-lived token — is
/// used before it is asked for again. ion's asset tokens last an hour; a
/// refusal before then is answered by asking again at once.
const ENDPOINT_LIFETIME: Duration = Duration::from_secs(45 * 60);

/// How long a client may keep a tile or a layer description when the
/// upstream said nothing: the catalog's tiles do not change under an asset.
const TILE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a client may keep a rewritten endpoint. It holds no secret, and
/// it points at this proxy, so it is good for as long as the proxy is.
const ENDPOINT_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// The longest relative path an asset request may name.
const LONGEST_PATH: usize = 160;

/// The longest path a 3D Tiles service's request may name: its files are
/// named by long encoded identifiers.
const LONGEST_TILES3D_PATH: usize = 512;

/// The query parameter a 3D Tiles service's references carry, and the only
/// one passed on: the session its root hands out.
const TILES3D_QUERY: &str = "session";

/// What the proxy serves, and with which credential.
#[derive(Clone)]
pub struct ProxyConfig {
    /// The account token. Held here, sent upstream as a bearer, never
    /// logged and never put in a reply.
    pub token: String,
    /// The assets a client may ask for; any other is a 404.
    pub assets: BTreeSet<u64>,
    /// ion's API; the public one unless said.
    pub api_base: Url,
}

impl std::fmt::Debug for ProxyConfig {
    /// Everything but the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("assets", &self.assets)
            .field("api_base", &self.api_base.as_str())
            .finish_non_exhaustive()
    }
}

impl ProxyConfig {
    /// For `assets`, over the public API.
    pub fn new(token: impl Into<String>, assets: impl IntoIterator<Item = u64>) -> Self {
        Self {
            token: token.into(),
            assets: assets.into_iter().collect(),
            api_base: Url::parse(ION_API_BASE).unwrap_or_else(|_| unreachable!("static url")),
        }
    }
}

/// One request, read and checked: nothing else ever goes upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `v1/assets/{id}/endpoint`.
    Endpoint(u64),
    /// `assets/{id}/{path}?{query}`: a file under the asset's URL, with the
    /// query parameters a terrain request carries.
    Asset {
        id: u64,
        path: String,
        query: Vec<(String, String)>,
    },
    /// `bing/{id}/REST/v1/Imagery/Metadata/{style}`.
    ImageryMetadata { id: u64, style: String },
    /// `tiles3d/{id}/{path}?session={session}`: a file of a 3D Tiles
    /// service ion points the asset at.
    Tiles3d {
        id: u64,
        path: String,
        session: Option<String>,
    },
}

/// Why a request is refused before anything goes upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not a route of the proxy, or an asset it does not serve.
    NotFound,
    /// A route, with a path or query it will not pass on.
    BadRequest(&'static str),
}

/// The query parameters an asset request may carry: a terrain layer's
/// version and the extensions asked of its tiles.
const QUERY_KEYS: [&str; 2] = ["v", "extensions"];

fn is_plain(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Route {
    /// Reads `path` — relative to where the proxy is mounted, with no
    /// leading slash — and `query`, as the client sent them.
    pub fn read(path: &str, query: Option<&str>) -> Result<Self, Refusal> {
        let segments: Vec<&str> = path.split('/').collect();
        let asset = |s: &str| s.parse::<u64>().map_err(|_| Refusal::NotFound);
        match segments.as_slice() {
            ["v1", "assets", id, "endpoint"] => Ok(Self::Endpoint(asset(id)?)),
            ["bing", id, "REST", "v1", "Imagery", "Metadata", style] => {
                let plain = !style.is_empty()
                    && style.len() <= 32
                    && style.bytes().all(|b| b.is_ascii_alphabetic());
                if !plain {
                    return Err(Refusal::BadRequest("not a map style"));
                }
                Ok(Self::ImageryMetadata {
                    id: asset(id)?,
                    style: (*style).to_owned(),
                })
            }
            ["tiles3d", id, rest @ ..] if !rest.is_empty() => {
                let id = asset(id)?;
                let path = rest.join("/");
                if path.len() > LONGEST_TILES3D_PATH || !rest.iter().all(|s| is_plain(s)) {
                    return Err(Refusal::BadRequest("not a path the proxy passes on"));
                }
                let mut session = None;
                for (key, value) in url::form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
                    let allowed = key == TILES3D_QUERY
                        && session.is_none()
                        && !value.is_empty()
                        && value.len() <= 512
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
                    if !allowed {
                        return Err(Refusal::BadRequest("not a query the proxy passes on"));
                    }
                    session = Some(value.into_owned());
                }
                Ok(Self::Tiles3d { id, path, session })
            }
            ["assets", id, rest @ ..] if !rest.is_empty() => {
                let id = asset(id)?;
                let path = rest.join("/");
                if path.len() > LONGEST_PATH || !rest.iter().all(|s| is_plain(s)) {
                    return Err(Refusal::BadRequest("not a path the proxy passes on"));
                }
                let mut pairs = Vec::new();
                for (key, value) in url::form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
                    let allowed = QUERY_KEYS.contains(&key.as_ref())
                        && value.len() <= 120
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
                    if !allowed {
                        return Err(Refusal::BadRequest("not a query the proxy passes on"));
                    }
                    pairs.push((key.into_owned(), value.into_owned()));
                }
                Ok(Self::Asset {
                    id,
                    path,
                    query: pairs,
                })
            }
            _ => Err(Refusal::NotFound),
        }
    }

    fn asset(&self) -> u64 {
        match self {
            Self::Endpoint(id)
            | Self::Asset { id, .. }
            | Self::ImageryMetadata { id, .. }
            | Self::Tiles3d { id, .. } => *id,
        }
    }
}

/// What the proxy answers.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Bytes,
    pub content_type: &'static str,
    /// How long a client may keep it; `None` for a reply not to be kept.
    pub max_age: Option<Duration>,
}

impl Reply {
    fn refused(status: u16, why: &str) -> Self {
        Self {
            status,
            body: Bytes::from(why.to_owned()),
            content_type: "text/plain; charset=utf-8",
            max_age: None,
        }
    }
}

/// An asset's endpoint as ion gave it, and when.
#[derive(Clone)]
struct Upstream {
    /// The endpoint, as received: rewritten for each reply.
    json: Value,
    /// The asset's own URL, for an asset hosted by ion.
    url: Option<Url>,
    /// Its short-lived token.
    token: String,
    /// The imagery service's API and key, for imagery ion serves through it.
    imagery: Option<(String, String)>,
    /// The 3D Tiles service's root, key included, for a tileset ion points
    /// at on another service.
    tiles3d: Option<Url>,
    fetched: Instant,
}

/// The proxy's logic: routes in, replies out, the credentials kept here.
pub struct Catalog<H: IonHttp> {
    http: Arc<H>,
    config: ProxyConfig,
    endpoints: Mutex<HashMap<u64, Upstream>>,
}

impl<H: IonHttp> Catalog<H> {
    pub fn new(http: Arc<H>, config: ProxyConfig) -> Self {
        Self {
            http,
            config,
            endpoints: Mutex::default(),
        }
    }

    /// The assets it serves.
    pub fn assets(&self) -> &BTreeSet<u64> {
        &self.config.assets
    }

    /// Answers `route` for a client that reaches the proxy at `public` — the
    /// URL of the proxy's mount as the client sees it, ending in `/`.
    pub async fn answer(&self, route: &Route, public: &Url) -> Reply {
        if !self.config.assets.contains(&route.asset()) {
            return Reply::refused(404, "no such asset");
        }
        let reply = match route {
            Route::Endpoint(id) => self.endpoint(*id, public).await,
            Route::Asset { id, path, query } => self.file(*id, path, query).await,
            Route::ImageryMetadata { id, style } => self.imagery_metadata(*id, style).await,
            Route::Tiles3d { id, path, session } => {
                self.tiles3d_file(*id, path, session.as_deref(), public)
                    .await
            }
        };
        self.withheld(reply)
    }

    /// The reply, unless it would carry a credential: whatever the upstream
    /// wrote, a token never leaves through here.
    fn withheld(&self, reply: Reply) -> Reply {
        let secrets = {
            let endpoints = self.endpoints.lock().unwrap_or_else(|e| e.into_inner());
            let mut secrets = vec![self.config.token.clone()];
            for upstream in endpoints.values() {
                secrets.push(upstream.token.clone());
                if let Some((_, key)) = &upstream.imagery {
                    secrets.push(key.clone());
                }
                if let Some(root) = &upstream.tiles3d {
                    secrets.extend(root.query_pairs().map(|(_, v)| v.into_owned()));
                }
            }
            secrets
        };
        let carries = secrets
            .iter()
            .filter(|s| s.len() >= 8)
            .any(|s| contains(&reply.body, s.as_bytes()));
        if carries {
            tracing::error!("an upstream reply carried a credential; it was not passed on");
            return Reply::refused(502, "the upstream reply could not be passed on");
        }
        reply
    }

    /// The asset's endpoint, from memory while it is fresh.
    async fn upstream(&self, id: u64, fresh: bool) -> Result<Upstream, Reply> {
        if !fresh {
            let endpoints = self.endpoints.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(upstream) = endpoints.get(&id) {
                if upstream.fetched.elapsed() < ENDPOINT_LIFETIME {
                    return Ok(upstream.clone());
                }
            }
        }
        let url = self
            .config
            .api_base
            .join(&format!("v1/assets/{id}/endpoint"))
            .map_err(|_| Reply::refused(500, "no catalog address"))?;
        let response = self
            .http
            .get(&url, Some(&self.config.token))
            .await
            .map_err(|_| Reply::refused(502, "the catalog could not be reached"))?;
        if response.status != 200 {
            tracing::warn!(
                asset = id,
                status = response.status,
                "the catalog refused an endpoint"
            );
            let status = if response.status == 404 { 404 } else { 502 };
            return Err(Reply::refused(status, "the catalog refused the asset"));
        }
        let json: Value = serde_json::from_slice(&response.body)
            .map_err(|_| Reply::refused(502, "the catalog answered something unreadable"))?;
        let text = |key: &str| json.get(key).and_then(Value::as_str).map(str::to_owned);
        let options = json.get("options");
        let option = |key: &str| {
            options
                .and_then(|o| o.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let (imagery, tiles3d) = match text("externalType").as_deref() {
            None => (None, None),
            Some("BING") => (
                Some((
                    option("url").ok_or_else(|| Reply::refused(502, "no imagery service named"))?,
                    option("key").unwrap_or_default(),
                )),
                None,
            ),
            Some("3DTILES") => (
                None,
                Some(
                    option("url")
                        .and_then(|u| Url::parse(&u).ok())
                        .filter(|u| u.scheme() == "https" && !u.path().is_empty())
                        .ok_or_else(|| Reply::refused(502, "no 3D Tiles service named"))?,
                ),
            ),
            Some(_) => return Err(Reply::refused(404, "not an asset the proxy serves")),
        };
        let upstream = Upstream {
            url: text("url").and_then(|u| Url::parse(&u).ok()),
            token: text("accessToken").unwrap_or_default(),
            imagery,
            tiles3d,
            json,
            fetched: Instant::now(),
        };
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, upstream.clone());
        Ok(upstream)
    }

    /// The endpoint, pointing back here and holding no credential.
    async fn endpoint(&self, id: u64, public: &Url) -> Reply {
        let upstream = match self.upstream(id, false).await {
            Ok(upstream) => upstream,
            Err(reply) => return reply,
        };
        let mut json = upstream.json;
        let Some(object) = json.as_object_mut() else {
            return Reply::refused(502, "the catalog answered something unreadable");
        };
        object.insert("accessToken".to_owned(), Value::String(String::new()));
        if let Some(root) = &upstream.tiles3d {
            // The service's root, here, with no key.
            let Ok(here) = public.join(&format!("tiles3d/{id}{}", root.path())) else {
                return Reply::refused(500, "no public address");
            };
            if let Some(options) = object.get_mut("options").and_then(Value::as_object_mut) {
                options.insert("url".to_owned(), Value::String(here.to_string()));
            }
        } else if upstream.imagery.is_some() {
            let Ok(service) = public.join(&format!("bing/{id}")) else {
                return Reply::refused(500, "no public address");
            };
            if let Some(options) = object.get_mut("options").and_then(Value::as_object_mut) {
                options.insert("url".to_owned(), Value::String(service.to_string()));
                options.insert("key".to_owned(), Value::String(String::new()));
            }
        } else if object.contains_key("url") {
            let Ok(own) = public.join(&format!("assets/{id}/")) else {
                return Reply::refused(500, "no public address");
            };
            object.insert("url".to_owned(), Value::String(own.to_string()));
        }
        match serde_json::to_vec(&json) {
            Ok(body) => Reply {
                status: 200,
                body: Bytes::from(body),
                content_type: "application/json",
                max_age: Some(ENDPOINT_MAX_AGE),
            },
            Err(_) => Reply::refused(500, "the endpoint could not be written"),
        }
    }

    /// A file under the asset's URL, with its token; asked again with a new
    /// token once if the old one is refused.
    async fn file(&self, id: u64, path: &str, query: &[(String, String)]) -> Reply {
        for fresh in [false, true] {
            let upstream = match self.upstream(id, fresh).await {
                Ok(upstream) => upstream,
                Err(reply) => return reply,
            };
            let Some(base) = upstream.url.as_ref() else {
                return Reply::refused(404, "the asset has no files");
            };
            let Ok(mut url) = base.join(path) else {
                return Reply::refused(400, "not a path the proxy passes on");
            };
            // Under the asset's own URL, and nowhere else.
            if url.origin() != base.origin() || !url.path().starts_with(base.path()) {
                return Reply::refused(400, "not a path the proxy passes on");
            }
            for (key, value) in query {
                url.query_pairs_mut().append_pair(key, value);
            }
            let response = match self.http.get(&url, Some(&upstream.token)).await {
                Ok(response) => response,
                Err(_) => return Reply::refused(502, "the tile service could not be reached"),
            };
            if matches!(response.status, 401 | 403) && !fresh {
                continue;
            }
            return match response.status {
                200 => Reply {
                    status: 200,
                    content_type: content_type(path),
                    max_age: Some(response.max_age.unwrap_or(TILE_MAX_AGE)),
                    body: response.body,
                },
                404 => Reply::refused(404, "no such tile"),
                status => {
                    tracing::warn!(asset = id, status, "the tile service refused a file");
                    Reply::refused(502, "the tile service refused the file")
                }
            };
        }
        Reply::refused(502, "the tile service refused the asset's token")
    }

    /// A file of the 3D Tiles service the asset points at, asked with the
    /// key ion gave and the session the client carries. The service's root
    /// is asked fresh — it hands out the session, and one kept would hand
    /// out an expired one — and a tileset document is answered with its
    /// references pointing back here.
    async fn tiles3d_file(
        &self,
        id: u64,
        path: &str,
        session: Option<&str>,
        public: &Url,
    ) -> Reply {
        let upstream = match self.upstream(id, false).await {
            Ok(upstream) => upstream,
            Err(reply) => return reply,
        };
        let Some(root) = upstream.tiles3d.as_ref() else {
            return Reply::refused(404, "the asset is not a 3D Tiles service's");
        };
        let mut url = root.clone();
        url.set_path(&format!("/{path}"));
        url.set_query(None);
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in root.query_pairs() {
                query.append_pair(&key, &value);
            }
            if let Some(session) = session {
                query.append_pair(TILES3D_QUERY, session);
            }
        }
        let is_root = url.path() == root.path();
        // Never logged with its URL: the key is in it.
        let response = if is_root {
            self.http.get_fresh(&url, None).await
        } else {
            self.http.get(&url, None).await
        };
        let response = match response {
            Ok(response) => response,
            Err(_) => return Reply::refused(502, "the 3D Tiles service could not be reached"),
        };
        match response.status {
            200 => {}
            404 => return Reply::refused(404, "no such tile"),
            status => {
                tracing::warn!(asset = id, status, "the 3D Tiles service refused a file");
                return Reply::refused(502, "the 3D Tiles service refused the file");
            }
        }
        let body = if path.ends_with(".json") {
            let Ok(here) = public.join(&format!("tiles3d/{id}/")) else {
                return Reply::refused(500, "no public address");
            };
            match rewritten(&response.body, root, &here) {
                Some(body) => Bytes::from(body),
                None => {
                    return Reply::refused(
                        502,
                        "the 3D Tiles service answered something unreadable",
                    )
                }
            }
        } else {
            response.body
        };
        Reply {
            status: 200,
            content_type: content_type(path),
            // As the service says, and not kept at all when it says nothing:
            // the root must never be.
            max_age: if is_root { None } else { response.max_age },
            body,
        }
    }

    /// The imagery service's metadata, asked with the key.
    async fn imagery_metadata(&self, id: u64, style: &str) -> Reply {
        let upstream = match self.upstream(id, false).await {
            Ok(upstream) => upstream,
            Err(reply) => return reply,
        };
        let Some((service, key)) = upstream.imagery.as_ref() else {
            return Reply::refused(404, "the asset is not served by an imagery service");
        };
        let Ok(url) = Url::parse(&tuile_bing::BingMetadata::metadata_url(service, style, key))
        else {
            return Reply::refused(502, "no imagery service address");
        };
        // Never logged with its URL: the key is in it.
        match self.http.get(&url, None).await {
            Ok(response) if response.status == 200 => Reply {
                status: 200,
                body: response.body,
                content_type: "application/json",
                max_age: Some(response.max_age.unwrap_or(ENDPOINT_MAX_AGE)),
            },
            Ok(response) => {
                tracing::warn!(
                    asset = id,
                    status = response.status,
                    "the imagery service refused"
                );
                Reply::refused(502, "the imagery service refused")
            }
            Err(_) => Reply::refused(502, "the imagery service could not be reached"),
        }
    }
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// A 3D Tiles document of the service at `root`, its references pointing at
/// `here` (`…/tiles3d/{id}/`) instead: a reference by absolute path, or by
/// URL on the service's own origin. Relative references already resolve
/// here. `None` for a document that is not JSON.
fn rewritten(body: &[u8], root: &Url, here: &Url) -> Option<Vec<u8>> {
    fn walk(value: &mut Value, root: &Url, here: &Url) {
        match value {
            Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    if matches!(key.as_str(), "uri" | "url") {
                        if let Value::String(reference) = v {
                            if let Some(local) = local(reference, root, here) {
                                *reference = local;
                            }
                            continue;
                        }
                    }
                    walk(v, root, here);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| walk(v, root, here)),
            _ => {}
        }
    }
    fn local(reference: &str, root: &Url, here: &Url) -> Option<String> {
        let resolved = root.join(reference).ok()?;
        let absolute_path = reference.starts_with('/') && !reference.starts_with("//");
        if !absolute_path && Url::parse(reference).is_err() {
            return None;
        }
        if resolved.origin() != root.origin() {
            return None;
        }
        let mut url = here.join(resolved.path().trim_start_matches('/')).ok()?;
        // The service's key never goes back: only what the reference itself
        // carried besides it.
        let keep: Vec<(String, String)> = resolved
            .query_pairs()
            .filter(|(k, v)| !root.query_pairs().any(|(rk, rv)| rk == *k && rv == *v))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.set_query(None);
        if !keep.is_empty() {
            url.query_pairs_mut().extend_pairs(keep);
        }
        Some(url.to_string())
    }
    let mut json: Value = serde_json::from_slice(body).ok()?;
    walk(&mut json, root, here);
    serde_json::to_vec(&json).ok()
}

/// The type of a file, by its name.
fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map_or("", |(_, e)| e);
    match extension {
        "json" => "application/json",
        "terrain" => "application/vnd.quantized-mesh",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "xml" => "application/xml",
        "glb" => "model/gltf-binary",
        _ => "application/octet-stream",
    }
}

#[cfg(feature = "proxy")]
pub use server::{router, Origins};

/// The proxy on `axum`: one route that reads every path under its mount,
/// with the cache and CORS headers a page needs.
#[cfg(feature = "proxy")]
mod server {
    use std::sync::Arc;

    use axum::extract::{Path, RawQuery, State};
    use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use tower_http::cors::{AllowOrigin, CorsLayer};
    use url::Url;

    use super::{Catalog, Refusal, Reply, Route};
    use crate::IonHttp;

    /// Which pages may read the proxy's replies.
    #[derive(Debug, Clone)]
    pub enum Origins {
        /// Any page: for a machine's own development server.
        Any,
        /// These origins only (`https://example.org`).
        Only(Vec<HeaderValue>),
    }

    struct Shared<H: IonHttp> {
        catalog: Catalog<H>,
        /// The proxy's public address, when it is not the one a request
        /// came in on — behind a load balancer, say.
        public: Option<Url>,
    }

    /// The proxy, to be nested where it is served (`/catalog`, say). Its
    /// replies point back at `public` when given, and otherwise at the
    /// address each request came in on, under `mount`.
    pub fn router<H: IonHttp + 'static>(
        catalog: Catalog<H>,
        public: Option<Url>,
        mount: &str,
        origins: Origins,
    ) -> Router {
        let mount = format!("/{}/", mount.trim_matches('/'));
        let shared = Arc::new((Shared { catalog, public }, mount));
        let cors = CorsLayer::new()
            .allow_methods([Method::GET, Method::HEAD])
            .allow_origin(match origins {
                Origins::Any => AllowOrigin::any(),
                Origins::Only(list) => AllowOrigin::list(list),
            })
            .max_age(std::time::Duration::from_secs(24 * 60 * 60));
        Router::new()
            .route("/{*path}", get(answer::<H>))
            .with_state(shared)
            .layer(cors)
    }

    async fn answer<H: IonHttp + 'static>(
        State(shared): State<Arc<(Shared<H>, String)>>,
        Path(path): Path<String>,
        RawQuery(query): RawQuery,
        headers: HeaderMap,
    ) -> Response {
        let (shared, mount) = shared.as_ref();
        let route = match Route::read(&path, query.as_deref()) {
            Ok(route) => route,
            Err(Refusal::NotFound) => return reply(Reply::refused(404, "not found")),
            Err(Refusal::BadRequest(why)) => return reply(Reply::refused(400, why)),
        };
        let Some(public) = shared
            .public
            .clone()
            .or_else(|| from_request(&headers, mount))
        else {
            return reply(Reply::refused(400, "no host"));
        };
        reply(shared.catalog.answer(&route, &public).await)
    }

    /// The mount's address as the client reached it: its `Host`, and the
    /// scheme a proxy in front said it used.
    fn from_request(headers: &HeaderMap, mount: &str) -> Option<Url> {
        let host = headers.get(header::HOST)?.to_str().ok()?;
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .filter(|s| *s == "https")
            .unwrap_or("http");
        let plain = host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
        if !plain {
            return None;
        }
        Url::parse(&format!("{scheme}://{host}{mount}")).ok()
    }

    fn reply(reply: Reply) -> Response {
        let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let cache = match reply.max_age {
            Some(age) => format!("public, max-age={}", age.as_secs()),
            None => "no-store".to_owned(),
        };
        let mut response = (status, reply.body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(reply.content_type),
        );
        if let Ok(cache) = HeaderValue::from_str(&cache) {
            headers.insert(header::CACHE_CONTROL, cache);
        }
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::MockHttp;

    const TOKEN: &str = "account-token-secret";
    const ASSET_TOKEN: &str = "asset-token-secret-1";
    const NEW_ASSET_TOKEN: &str = "asset-token-secret-2";
    const KEY: &str = "imagery-key-secret";
    const API: &str = "https://api.test/";
    const DEPOT: &str = "https://assets.test/depot/1/terrain/";

    fn catalog(http: Arc<MockHttp>) -> Catalog<MockHttp> {
        let mut config = ProxyConfig::new(TOKEN, [1, 2, 3954]);
        config.api_base = Url::parse(API).expect("a url");
        Catalog::new(http, config)
    }

    fn public() -> Url {
        Url::parse("http://192.168.1.20:8091/catalog/").expect("a url")
    }

    fn terrain_endpoint(token: &str) -> String {
        format!(
            r#"{{"type":"TERRAIN","url":"{DEPOT}","accessToken":"{token}",
                "attributions":[{{"html":"© someone","collapsible":false}}]}}"#
        )
    }

    fn bing_endpoint() -> String {
        format!(
            r#"{{"type":"IMAGERY","externalType":"BING",
                "options":{{"url":"https://imagery.test","key":"{KEY}","mapStyle":"Aerial"}},
                "attributions":[]}}"#
        )
    }

    fn block<T>(future: impl std::future::Future<Output = T>) -> T {
        futures_executor::block_on(future)
    }

    fn text(reply: &Reply) -> String {
        String::from_utf8_lossy(&reply.body).into_owned()
    }

    /// The endpoint goes out pointing at the proxy, with no credential in
    /// it, and was asked for with the account token.
    #[test]
    fn an_endpoint_points_back_here_and_carries_no_token() {
        let http = Arc::new(MockHttp::default());
        http.push(
            &format!("{API}v1/assets/1/endpoint"),
            200,
            &terrain_endpoint(ASSET_TOKEN),
        );
        let catalog = catalog(Arc::clone(&http));
        let reply = block(catalog.answer(&Route::Endpoint(1), &public()));
        assert_eq!(reply.status, 200);
        let json: Value = serde_json::from_slice(&reply.body).expect("json");
        assert_eq!(json["url"], "http://192.168.1.20:8091/catalog/assets/1/");
        assert_eq!(json["accessToken"], "");
        assert_eq!(json["attributions"][0]["html"], "© someone");
        let body = text(&reply);
        assert!(
            !body.contains(TOKEN) && !body.contains(ASSET_TOKEN),
            "{body}"
        );
        assert_eq!(http.calls()[0].1.as_deref(), Some(TOKEN));
        assert_eq!(reply.max_age, Some(ENDPOINT_MAX_AGE));
    }

    /// A tile is fetched under the asset's URL with the asset's token, with
    /// the two query parameters passed on and the origin's freshness kept.
    #[test]
    fn a_tile_is_fetched_with_the_asset_token() {
        let http = Arc::new(MockHttp::default());
        http.push(
            &format!("{API}v1/assets/1/endpoint"),
            200,
            &terrain_endpoint(ASSET_TOKEN),
        );
        let tile = format!("{DEPOT}9/541/386.terrain?v=1.2.0&extensions=octvertexnormals");
        http.push(&tile, 200, "mesh");
        let catalog = catalog(Arc::clone(&http));
        let route = Route::read(
            "assets/1/9/541/386.terrain",
            Some("v=1.2.0&extensions=octvertexnormals"),
        )
        .expect("a route");
        let reply = block(catalog.answer(&route, &public()));
        assert_eq!((reply.status, text(&reply).as_str()), (200, "mesh"));
        assert_eq!(reply.content_type, "application/vnd.quantized-mesh");
        assert_eq!(reply.max_age, Some(TILE_MAX_AGE));
        let calls = http.calls();
        assert_eq!(calls[1], (tile, Some(ASSET_TOKEN.to_owned())));
        // The endpoint was asked for once, and kept.
        let again = block(catalog.answer(&route, &public()));
        assert_eq!(again.status, 200);
        assert_eq!(http.calls().len(), 3);
    }

    /// A 3D Tiles service's key, in its root's URL as ion gives it.
    const SERVICE_KEY: &str = "service-key-secret-123";

    fn tiles3d_endpoint() -> String {
        format!(
            r#"{{"type":"3DTILES","externalType":"3DTILES",
                "options":{{"url":"https://tiles3d.test/v1/3dtiles/root.json?key={SERVICE_KEY}"}},
                "attributions":[{{"html":"Cesium ion","collapsible":false}}]}}"#
        )
    }

    fn tiles3d_catalog(http: Arc<MockHttp>) -> Catalog<MockHttp> {
        let mut config = ProxyConfig::new(TOKEN, [2275207]);
        config.api_base = Url::parse(API).expect("a url");
        Catalog::new(http, config)
    }

    /// A 3D Tiles asset on another service: its endpoint points here with
    /// no key; its root is asked fresh with the key and never kept; its
    /// references by absolute path point back here, the session kept and
    /// the key gone; its tiles go through with the key and the session.
    #[test]
    fn a_3d_tiles_service_is_proxied_and_its_key_never_leaves() {
        let http = Arc::new(MockHttp::default());
        http.push(
            &format!("{API}v1/assets/2275207/endpoint"),
            200,
            &tiles3d_endpoint(),
        );
        let root = format!("https://tiles3d.test/v1/3dtiles/root.json?key={SERVICE_KEY}");
        http.push(
            &root,
            200,
            r#"{"asset":{"version":"1.0"},"geometricError":1e7,
                "root":{"geometricError":1e6,"boundingVolume":{"box":[0,0,0,1,0,0,0,1,0,0,0,1]},
                  "children":[{"geometricError":1,"boundingVolume":{"box":[0,0,0,1,0,0,0,1,0,0,0,1]},
                    "content":{"uri":"/v1/3dtiles/datasets/CgA/files/A1.json?session=S-1_x"}},
                    {"geometricError":1,"boundingVolume":{"box":[0,0,0,1,0,0,0,1,0,0,0,1]},
                    "content":{"uri":"https://elsewhere.test/b.glb"}}]}}"#,
        );
        let tile =
            format!("https://tiles3d.test/v1/3dtiles/datasets/CgA/files/B2.glb?key={SERVICE_KEY}&session=S-1_x");
        http.push(&tile, 200, "glTF");
        let catalog = tiles3d_catalog(Arc::clone(&http));

        let reply = block(catalog.answer(&Route::Endpoint(2275207), &public()));
        assert_eq!(reply.status, 200);
        let json: Value = serde_json::from_slice(&reply.body).expect("json");
        assert_eq!(
            json["options"]["url"],
            "http://192.168.1.20:8091/catalog/tiles3d/2275207/v1/3dtiles/root.json"
        );
        assert!(!text(&reply).contains(SERVICE_KEY));

        let route = Route::read("tiles3d/2275207/v1/3dtiles/root.json", None).expect("a route");
        let reply = block(catalog.answer(&route, &public()));
        assert_eq!(reply.status, 200, "{}", text(&reply));
        assert_eq!(reply.max_age, None, "the root is never kept");
        assert_eq!(http.fresh(), vec![root.clone()], "and asked fresh");
        let body = text(&reply);
        assert!(!body.contains(SERVICE_KEY), "{body}");
        let json: Value = serde_json::from_slice(&reply.body).expect("json");
        assert_eq!(
            json["root"]["children"][0]["content"]["uri"],
            "http://192.168.1.20:8091/catalog/tiles3d/2275207/v1/3dtiles/datasets/CgA/files/A1.json?session=S-1_x"
        );
        assert_eq!(
            json["root"]["children"][1]["content"]["uri"], "https://elsewhere.test/b.glb",
            "another origin is not the service's"
        );

        let route = Route::read(
            "tiles3d/2275207/v1/3dtiles/datasets/CgA/files/B2.glb",
            Some("session=S-1_x"),
        )
        .expect("a route");
        let reply = block(catalog.answer(&route, &public()));
        assert_eq!((reply.status, text(&reply).as_str()), (200, "glTF"));
        assert_eq!(reply.content_type, "model/gltf-binary");
        let calls = http.calls();
        assert!(calls.contains(&(tile, None)), "{calls:?}");
        assert!(
            calls.iter().skip(1).all(|(_, bearer)| bearer.is_none()),
            "no ion credential goes to the service"
        );

        // Nothing but the session is passed on.
        for query in ["key=x", "session=a&session=b", "session=a b", "other=1"] {
            assert!(
                Route::read("tiles3d/2275207/v1/x.glb", Some(query)).is_err(),
                "{query}"
            );
        }
        assert!(Route::read("tiles3d/2275207/../x.glb", None).is_err());
    }

    /// An asset token refused is replaced, and the tile asked for again.
    #[test]
    fn a_refused_asset_token_is_renewed_once() {
        let http = Arc::new(MockHttp::default());
        let endpoint = format!("{API}v1/assets/1/endpoint");
        http.push(&endpoint, 200, &terrain_endpoint(ASSET_TOKEN));
        http.push(&endpoint, 200, &terrain_endpoint(NEW_ASSET_TOKEN));
        let layer = format!("{DEPOT}layer.json");
        http.push(&layer, 401, "expired");
        http.push(&layer, 200, "{}");
        let catalog = catalog(Arc::clone(&http));
        let route = Route::read("assets/1/layer.json", None).expect("a route");
        let reply = block(catalog.answer(&route, &public()));
        assert_eq!(reply.status, 200);
        let bearers: Vec<_> = http.calls().into_iter().map(|(_, b)| b).collect();
        assert_eq!(
            bearers,
            [
                Some(TOKEN.to_owned()),
                Some(ASSET_TOKEN.to_owned()),
                Some(TOKEN.to_owned()),
                Some(NEW_ASSET_TOKEN.to_owned()),
            ]
        );
    }

    /// The imagery service's endpoint names the proxy and no key; its
    /// metadata is asked for with the key, and comes back without it.
    #[test]
    fn imagery_metadata_is_asked_with_the_key_the_client_never_sees() {
        let http = Arc::new(MockHttp::default());
        http.push(&format!("{API}v1/assets/2/endpoint"), 200, &bing_endpoint());
        let metadata =
            tuile_bing::BingMetadata::metadata_url("https://imagery.test", "Aerial", KEY);
        http.push(&metadata, 200, r#"{"resourceSets":[]}"#);
        let catalog = catalog(Arc::clone(&http));
        let endpoint = block(catalog.answer(&Route::Endpoint(2), &public()));
        let json: Value = serde_json::from_slice(&endpoint.body).expect("json");
        assert_eq!(
            json["options"]["url"],
            "http://192.168.1.20:8091/catalog/bing/2"
        );
        assert_eq!(json["options"]["key"], "");
        assert!(!text(&endpoint).contains(KEY));
        // What the client then asks, built by the client's own code.
        let asked = tuile_bing::BingMetadata::metadata_url(
            json["options"]["url"].as_str().expect("a url"),
            "Aerial",
            "",
        );
        let asked = Url::parse(&asked).expect("a url");
        let path = asked.path().trim_start_matches("/catalog/");
        let route = Route::read(path, asked.query()).expect("a route");
        let reply = block(catalog.answer(&route, &public()));
        assert_eq!(
            (reply.status, text(&reply).as_str()),
            (200, r#"{"resourceSets":[]}"#)
        );
        assert_eq!(http.calls()[1], (metadata, None));
    }

    /// An upstream reply that carries a credential is not passed on.
    #[test]
    fn a_reply_carrying_a_credential_is_withheld() {
        let http = Arc::new(MockHttp::default());
        http.push(&format!("{API}v1/assets/2/endpoint"), 200, &bing_endpoint());
        let metadata =
            tuile_bing::BingMetadata::metadata_url("https://imagery.test", "Aerial", KEY);
        http.push(&metadata, 200, &format!(r#"{{"echo":"{KEY}"}}"#));
        let catalog = catalog(Arc::clone(&http));
        let route = Route::ImageryMetadata {
            id: 2,
            style: "Aerial".into(),
        };
        let reply = block(catalog.answer(&route, &public()));
        assert_eq!(reply.status, 502);
        assert!(!text(&reply).contains(KEY));
    }

    /// An asset the proxy was not configured with is a 404, and nothing
    /// goes upstream for it.
    #[test]
    fn an_unknown_asset_is_refused_without_asking_upstream() {
        let http = Arc::new(MockHttp::default());
        let catalog = catalog(Arc::clone(&http));
        for route in [
            Route::Endpoint(4),
            Route::read("assets/96188/tileset.json", None).expect("a route"),
        ] {
            assert_eq!(block(catalog.answer(&route, &public())).status, 404);
        }
        assert!(http.calls().is_empty());
    }

    /// Anything but the three routes, a relative path of the narrow
    /// alphabet and the two query parameters is refused before it is a
    /// route at all.
    #[test]
    fn paths_and_queries_outside_the_routes_are_refused() {
        for (path, query) in [
            ("v1/me", None),
            ("v1/assets", None),
            ("v1/assets/1/endpoint/x", None),
            ("assets/1", None),
            ("assets/x/layer.json", None),
            ("https://elsewhere.test/a", None),
            ("tiles/1/2/3.png", None),
        ] {
            assert_eq!(Route::read(path, query), Err(Refusal::NotFound), "{path}");
        }
        for (path, query) in [
            ("assets/1/", None),
            ("assets/1/../2/layer.json", None),
            ("assets/1/./layer.json", None),
            ("assets/1/%2e%2e/layer.json", None),
            ("assets/1//layer.json", None),
            ("assets/1/layer.json", Some("access_token=x")),
            ("assets/1/layer.json", Some("v=1&extensions=a/b")),
            ("bing/2/REST/v1/Imagery/Metadata/Aerial%2F..", None),
        ] {
            assert!(
                matches!(Route::read(path, query), Err(Refusal::BadRequest(_))),
                "{path} {query:?}"
            );
        }
    }

    /// The token stays out of what the configuration prints.
    #[test]
    fn the_configuration_does_not_print_its_token() {
        let printed = format!("{:?}", ProxyConfig::new(TOKEN, [1]));
        assert!(!printed.contains(TOKEN), "{printed}");
    }

    #[cfg(feature = "proxy")]
    mod server {
        use super::*;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        fn app(http: Arc<MockHttp>) -> axum::Router {
            axum::Router::new().nest(
                "/catalog",
                super::super::router(catalog(http), None, "/catalog", Origins::Any),
            )
        }

        async fn body(response: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("a body");
            String::from_utf8_lossy(&bytes).into_owned()
        }

        /// The endpoint, as a page reads it: CORS open, cacheable, pointing
        /// at the address the page came in on, with no token.
        #[tokio::test]
        async fn a_page_reads_the_endpoint_across_origins() {
            let http = Arc::new(MockHttp::default());
            http.push(
                &format!("{API}v1/assets/1/endpoint"),
                200,
                &terrain_endpoint(ASSET_TOKEN),
            );
            let response = app(http)
                .oneshot(
                    Request::get("/catalog/v1/assets/1/endpoint")
                        .header("host", "127.0.0.1:8091")
                        .header("origin", "http://localhost:5173")
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("a response");
            assert_eq!(response.status(), StatusCode::OK);
            let headers = response.headers().clone();
            assert_eq!(headers["access-control-allow-origin"], "*");
            assert_eq!(headers["cache-control"], "public, max-age=600");
            assert_eq!(headers["content-type"], "application/json");
            let text = body(response).await;
            assert!(
                text.contains("http://127.0.0.1:8091/catalog/assets/1/"),
                "{text}"
            );
            assert!(!text.contains(TOKEN) && !text.contains(ASSET_TOKEN));
        }

        /// A preflight from another origin is answered for GET.
        #[tokio::test]
        async fn a_preflight_is_answered() {
            let response = app(Arc::new(MockHttp::default()))
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri("/catalog/assets/1/layer.json")
                        .header("origin", "https://pages.test")
                        .header("access-control-request-method", "GET")
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("a response");
            assert!(response.status().is_success());
            let allowed = response.headers()["access-control-allow-methods"]
                .to_str()
                .expect("text")
                .to_owned();
            assert!(allowed.contains("GET"), "{allowed}");
        }

        /// Refusals keep out of the cache, and a list of origins is kept to.
        #[tokio::test]
        async fn refusals_are_not_cached_and_origins_can_be_listed() {
            let http = Arc::new(MockHttp::default());
            let only = axum::Router::new().nest(
                "/catalog",
                super::super::router(
                    catalog(http),
                    Some(Url::parse("https://tiles.test/catalog/").expect("a url")),
                    "catalog",
                    Origins::Only(vec!["https://pages.test".parse().expect("an origin")]),
                ),
            );
            let response = only
                .oneshot(
                    Request::get("/catalog/v1/assets/77/endpoint")
                        .header("origin", "https://elsewhere.test")
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("a response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(response
                .headers()
                .get("access-control-allow-origin")
                .is_none());
        }
    }
}
