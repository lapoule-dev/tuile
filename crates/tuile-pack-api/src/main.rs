// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The bench's API, served by a native process.
//!
//! The routes and what they mean are `tuile_repository::bench`'s: this file
//! opens the buckets a configuration names, hands them to a `Bench`, and
//! turns each request into a call to it. A Worker serves the very same
//! routes from the same `Bench`; the two differ in how they reach a bucket
//! and in nothing a client can see.
//!
//! **Nothing is downloaded whole.** Each project's bucket is wrapped in
//! `Cached`: a pack runs to gigabytes and a browser reads a few megabytes of
//! it per frame, so objects are fetched in fixed chunks, on demand, each
//! chunk once.
//!
//! What is served is described by a configuration file (`--config`, default
//! `film-bench.toml`): the projects, each a bucket read through one layout,
//! and the tile store. See `examples/film-web/film-bench.example.toml`. The
//! buckets share one endpoint and one key, taken from the environment as the
//! farm takes them — `TUILE_STORE_ENDPOINT`, `TUILE_STORE_ACCESS_KEY_ID`,
//! `TUILE_STORE_SECRET_ACCESS_KEY` — with a `.env` in the working directory
//! read first. Credentials stay in this process.
//!
//! ```bash
//! cargo run --release -p tuile-pack-api -- --www examples/film-web/www
//! ```

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tuile_farm::{BucketConfig, ObjectRunStore, StoreError, Tuning};
use tuile_repository::{
    ArchivedTiles, Asked, Bench, Cached, Config, DiskChunks, FilmRepository, HttpGet, Layout,
    Objects, Place, Project, RemoteBlocks, RemoteLive, Reply, RunFilms, ScenePacks, StoreObjects,
    TileRepository,
};
use tuile_tile_server::{StoreConfig, TileStore};

/// A `Reply` as this server's response. A body left to be read is read here,
/// whole: this process has the memory for a block.
async fn respond(mut reply: Reply) -> Response {
    if let Some(later) = reply.later.take() {
        match later.read().await {
            Ok(body) => reply.body = body,
            Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        }
    }
    if reply.status >= 400 {
        tracing::warn!("{}: {}", reply.status, String::from_utf8_lossy(&reply.body));
    }
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, reply.body).into_response();
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&reply.content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(reply.cache_control),
    );
    if let Some(Ok(v)) = reply.etag.as_deref().map(HeaderValue::from_str) {
        headers.insert(header::ETAG, v);
    }
    if let Some(size) = reply.object_size {
        headers.insert("x-object-size", HeaderValue::from(size));
    }
    if reply.ranged {
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    if let Some(Ok(v)) = reply.content_range.as_deref().map(HeaderValue::from_str) {
        headers.insert(header::CONTENT_RANGE, v);
    }
    response
}

async fn api(bench: Arc<Bench>, request: Request) -> Response {
    // Owned before the first await: a request is not to be held across it.
    let (range, if_none_match, path, query) = {
        let header_of = |name| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        (
            header_of(header::RANGE),
            header_of(header::IF_NONE_MATCH),
            request.uri().path().to_string(),
            request.uri().query().unwrap_or_default().to_string(),
        )
    };
    drop(request);
    let asked = Asked {
        range: range.as_deref(),
        if_none_match: if_none_match.as_deref(),
    };
    match bench.get(&path, &query, asked).await {
        Some(reply) => respond(reply).await,
        None => (StatusCode::NOT_FOUND, "no such route").into_response(),
    }
}

/// The most a POST's body may be: a list of keys and validators, a few
/// hundred of each.
const POSTED_LIMIT: usize = 1 << 20;

/// The one route that takes a body (`POST /api/store/live`, many of the
/// store's small objects asked about at once), handed to the bench as it
/// came. The content type is not looked at: a browser sends this across
/// origins as `text/plain`, which is what spares it a preflight.
async fn posted(bench: Arc<Bench>, request: Request) -> Response {
    let (path, query) = (
        request.uri().path().to_string(),
        request.uri().query().unwrap_or_default().to_string(),
    );
    let body = match axum::body::to_bytes(request.into_body(), POSTED_LIMIT).await {
        Ok(body) => body,
        Err(e) => return (StatusCode::PAYLOAD_TOO_LARGE, e.to_string()).into_response(),
    };
    match bench.post(&path, &query, &body).await {
        Some(reply) => respond(reply).await,
        None => (StatusCode::NOT_FOUND, "no such route").into_response(),
    }
}

fn open(place: &Place) -> Result<ObjectRunStore, StoreError> {
    let tuning = Tuning::from_env();
    match place {
        Place::Remote { url, .. } => Err(StoreError::NotFound(format!(
            "{url}: an address is read by the store's routes, not as a bucket"
        ))),
        Place::Dir(dir) => ObjectRunStore::local(dir, tuning),
        Place::Bucket(bucket) => ObjectRunStore::bucket(
            &BucketConfig {
                bucket: bucket.clone(),
                ..bucket_base()?
            },
            tuning,
        ),
    }
}

/// The endpoint and key every bucket here shares.
fn bucket_base() -> Result<BucketConfig, StoreError> {
    let var = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| StoreError::Config(format!("{name} is not set")))
    };
    Ok(BucketConfig {
        endpoint: var("TUILE_STORE_ENDPOINT")?,
        bucket: String::new(),
        access_key_id: var("TUILE_STORE_ACCESS_KEY_ID")?,
        secret_access_key: var("TUILE_STORE_SECRET_ACCESS_KEY")?,
        region: std::env::var("TUILE_STORE_REGION").unwrap_or_else(|_| "auto".into()),
    })
}

/// What packs were found to hold, for as long as this server runs: a few
/// bytes a pack, of the packs someone listed.
#[derive(Default)]
struct Notes(std::sync::Mutex<std::collections::HashMap<String, bytes::Bytes>>);

#[async_trait::async_trait]
impl tuile_core::storage::ContentStore for Notes {
    async fn get(&self, key: &str) -> Option<bytes::Bytes> {
        self.0.lock().ok()?.get(key).cloned()
    }

    async fn put(&self, key: &str, value: bytes::Bytes, _: Option<std::time::Duration>) {
        if let Ok(mut notes) = self.0.lock() {
            notes.insert(key.to_string(), value);
        }
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    let addr: SocketAddr = arg(&args, "--addr")
        .unwrap_or_else(|| "127.0.0.1:8090".into())
        .parse()?;
    let cache = PathBuf::from(
        arg(&args, "--cache")
            .or_else(|| std::env::var("TUILE_PACK_CACHE").ok())
            .unwrap_or_else(|| ".pack-cache".into()),
    );
    let config_path = arg(&args, "--config").unwrap_or_else(|| "film-bench.toml".into());
    let config = Config::read(Path::new(&config_path))?;

    let mut projects = Vec::new();
    for project in config.projects {
        // The bucket, then the cache over it: both are `Objects`, and
        // everything from here on is handed the second.
        let objects: Arc<dyn Objects> = Arc::new(Cached::new(
            Arc::new(open(&project.place)?),
            DiskChunks::new(cache.join(&project.name)),
        ));
        let notes = Arc::new(Notes::default());
        let films: Arc<dyn FilmRepository> = match project.layout {
            Layout::Scenes(roots) => {
                Arc::new(ScenePacks::new(objects.clone(), roots).remembering(notes))
            }
            Layout::Runs(layout) => {
                Arc::new(RunFilms::new(objects.clone(), layout)?.remembering(notes))
            }
        };
        tracing::info!(
            "project {}: {} read as {}",
            project.name,
            objects.label(),
            films.layout()
        );
        projects.push(Project {
            name: project.name,
            objects,
            films,
        });
    }
    let mut store_objects = None;
    let mut store_at = None;
    let tiles = match config.tiles {
        Some(Place::Remote {
            url,
            header,
            direct,
            parameter,
        }) => {
            // Somebody else serves the store: its catalog and manifests are
            // asked of it each time, its archives by blocks, each kept here
            // once read.
            let credential = match header {
                Some(name) => Some((
                    name,
                    std::env::var("TUILE_TILES_REMOTE_SECRET").map_err(|_| {
                        "TUILE_TILES_REMOTE_SECRET is not set: [tiles] names a header"
                    })?,
                )),
                None => None,
            };
            if direct {
                store_at = Some(tuile_repository::StoreAt {
                    url: url.clone(),
                    header: match parameter {
                        Some(_) => None,
                        None => credential.as_ref().map(|(name, _)| name.clone()),
                    },
                    parameter: parameter.clone(),
                    credential: std::env::var("TUILE_TILES_REMOTE_PUBLIC_SECRET")
                        .ok()
                        .filter(|v| !v.is_empty()),
                });
            }
            // A block of the remote store is downloaded once and kept,
            // memory over disk.
            let blocks =
                tuile_storage_foyer::FoyerStore::keeping(tuile_storage_foyer::StoreConfig {
                    dir: cache.join("tile-store-remote"),
                    memory_bytes: 256 << 20,
                    disk_bytes: 32 << 30,
                    default_ttl: None,
                })
                .await?;
            let get = Arc::new(tuile_repository::Kept::new(
                HttpGet::new(&url, credential)?,
                Arc::new(blocks),
            ));
            let live: Arc<dyn Objects> = Arc::new(RemoteLive::new(get.clone(), "store"));
            let archives: Arc<dyn Objects> = Arc::new(RemoteBlocks::new(get, "store"));
            store_objects = Some(StoreObjects {
                live: live.clone(),
                archives: archives.clone(),
            });
            let now: tuile_repository::Now = Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });
            match ArchivedTiles::open(live, archives, now).await {
                Ok(t) => {
                    let tiles: Arc<dyn TileRepository> = Arc::new(t);
                    tracing::info!("tiles: {url}, {} layers", tiles.layers().len());
                    Some((url, tiles))
                }
                Err(e) => {
                    tracing::warn!("tiles: {url} cannot be opened ({e}): /api/tiles is not served");
                    None
                }
            }
        }
        Some(place) => {
            let store = open(&place)?;
            let label = store.label().to_string();
            // The same store as objects, for a reader that finds tiles
            // itself: its catalog and manifests straight from the bucket,
            // its archives through the chunk cache.
            let live: Arc<dyn Objects> = Arc::new(open(&place)?);
            store_objects = Some(StoreObjects {
                archives: Arc::new(Cached::new(
                    live.clone(),
                    DiskChunks::new(cache.join("tile-store")),
                )),
                live,
            });
            match TileStore::open(store.object_store(), StoreConfig::default()).await {
                Ok(t) => {
                    let tiles: Arc<dyn TileRepository> = Arc::new(t);
                    tracing::info!("tiles: {label}, {} layers", tiles.layers().len());
                    Some((label, tiles))
                }
                Err(e) => {
                    tracing::warn!(
                        "tiles: {label} cannot be opened ({e}): /api/tiles is not served"
                    );
                    None
                }
            }
        }
        None => None,
    };
    let bench = Arc::new(Bench {
        projects,
        tiles,
        store: store_objects,
        store_at,
    });

    let posting = bench.clone();
    let mut app = Router::new().route(
        "/api/{*rest}",
        axum::routing::get(move |request: Request| api(bench.clone(), request))
            .post(move |request: Request| posted(posting.clone(), request)),
    );
    if let Some(www) = arg(&args, "--www") {
        // The page, its scripts and its wasm are rebuilt together and must be
        // loaded together: a cached script against a fresh module fails at
        // the first call. `no-cache` makes the browser ask each time; an
        // unchanged file still costs only a 304.
        app = app.fallback_service(
            tower::ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("no-cache"),
                ))
                .service(ServeDir::new(www)),
        );
    }
    let cors = CorsLayer::permissive().expose_headers([
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CONTENT_LENGTH,
        // A page served from elsewhere reads these two: a block's reader
        // needs its object's size, and a cache its validator.
        header::ETAG,
        header::HeaderName::from_static("x-object-size"),
    ]);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app.layer(cors)).await?;
    Ok(())
}
