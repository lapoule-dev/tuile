// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A local, read-only HTTP API over the buckets a film is made from.
//!
//! ```text
//! GET /api/projects                      the projects, their layout, the tile store
//! GET /api/p/<project>/films             the films the project's bucket holds
//! GET /api/p/<project>/films/<id>        one film: its packs (chunks), in order
//! GET /api/p/<project>/o/<key>           an object, by byte range (206)
//! GET /api/p/<project>/ls?prefix=        one level of the bucket, as it lies
//! GET /api/tiles/catalog                 the tile store's layers
//! GET /api/tiles/<layer>/<z>/<x>/<y>     one source tile, as stored
//! GET /                                  the bench page, if --www is given
//! ```
//!
//! This is an adapter and nothing more. What a film is made of and where it
//! lies is `tuile-repository`'s to know: every route here talks to a
//! `dyn FilmRepository`, a `dyn Objects` or a `dyn TileRepository`, and no
//! key is built in this file. A Worker serving the same routes over an R2
//! binding would differ in its `Objects` and in nothing else.
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
//! Every route is a GET and nothing here writes to a bucket.
//!
//! ```bash
//! cargo run --release -p tuile-pack-api -- --www examples/film-web/www
//! ```

mod config;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tuile_farm::{BucketConfig, ObjectRunStore, StoreError, Tuning};
use tuile_repository::{
    Cached, FilmRepository, Objects, RepoError, RunFilms, ScenePacks, TileRepository,
};

use config::{Config, Layout, Place};
use tuile_tile_server::{StoreConfig, TileStore};

/// How much an open-ended range (`bytes=N-`) is answered with. A media
/// element asks that way and then asks again from where the answer stopped.
const OPEN_RANGE: u64 = 8 << 20;
/// The largest object served without a range.
const WHOLE_LIMIT: u64 = 64 << 20;

struct Project {
    name: String,
    objects: Arc<dyn Objects>,
    films: Arc<dyn FilmRepository>,
}

struct Api {
    projects: Vec<Arc<Project>>,
    tiles: Option<(String, Arc<dyn TileRepository>)>,
}

impl Api {
    fn project(&self, name: &str) -> Result<Arc<Project>, Response> {
        self.projects
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("no project {name}")))
    }
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    tracing::warn!("{status}: {message}");
    (status, message.to_string()).into_response()
}

fn repo_error(e: RepoError) -> Response {
    match e {
        RepoError::NotFound(_) => error(StatusCode::NOT_FOUND, e),
        RepoError::Malformed { .. } => error(StatusCode::UNPROCESSABLE_ENTITY, e),
        RepoError::Store(_) => error(StatusCode::BAD_GATEWAY, e),
    }
}

/// A key is a path inside the bucket and inside the cache, never out of it.
fn safe(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn content_type(key: &str) -> &'static str {
    match key.rsplit('.').next().unwrap_or_default() {
        "mp4" => "video/mp4",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "txt" | "scene" | "usda" | "log" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `bytes=a-b`, `bytes=a-` or nothing, resolved against the object's size.
/// `None` is a range that cannot be satisfied.
fn resolve(range: Option<&str>, size: u64) -> Option<(std::ops::Range<u64>, bool)> {
    let Some(spec) = range else {
        return (size <= WHOLE_LIMIT).then_some((0..size, false));
    };
    let (a, b) = spec.strip_prefix("bytes=")?.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end = match b.trim() {
        "" => (start + OPEN_RANGE).min(size),
        b => b.parse::<u64>().ok()?.checked_add(1)?.min(size),
    };
    (start < end).then_some((start..end, true))
}

#[derive(serde::Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    prefix: String,
}

async fn projects(api: Arc<Api>) -> Response {
    Json(serde_json::json!({
        "projects": api.projects.iter().map(|p| serde_json::json!({
            "name": p.name, "store": p.objects.label(), "layout": p.films.layout(),
        })).collect::<Vec<_>>(),
        "tiles": api.tiles.as_ref().map(|(label, _)| label),
    }))
    .into_response()
}

async fn ls(api: Arc<Api>, project: String, q: ListQuery) -> Response {
    let project = match api.project(&project) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match project.objects.browse(&q.prefix).await {
        Ok(listing) => Json(listing).into_response(),
        Err(e) => repo_error(e),
    }
}

async fn films(api: Arc<Api>, project: String) -> Response {
    let project = match api.project(&project) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match project.films.films().await {
        Ok(films) => Json(films).into_response(),
        Err(e) => repo_error(e),
    }
}

async fn film(api: Arc<Api>, project: String, id: String) -> Response {
    let project = match api.project(&project) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match project.films.film(&id).await {
        Ok(film) => Json(film).into_response(),
        Err(e) => repo_error(e),
    }
}

async fn object(api: Arc<Api>, project: String, key: String, headers: HeaderMap) -> Response {
    let project = match api.project(&project) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !safe(&key) {
        return error(StatusCode::BAD_REQUEST, format!("not a key: {key}"));
    }
    let size = match project.objects.size(&key).await {
        Ok(s) => s,
        Err(e) => return repo_error(e),
    };
    let wanted = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let Some((range, partial)) = resolve(wanted, size) else {
        return match wanted {
            Some(_) => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response(),
            None => error(
                StatusCode::BAD_REQUEST,
                format!("{key} is {size} bytes: ask for a range"),
            ),
        };
    };
    let bytes = match project.objects.read(&key, range.clone()).await {
        Ok(b) => b,
        Err(e) => return repo_error(e),
    };
    let mut response = (
        if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        },
        bytes,
    )
        .into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&key)),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if partial {
        if let Ok(v) =
            HeaderValue::from_str(&format!("bytes {}-{}/{size}", range.start, range.end - 1))
        {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    response
}

async fn tile_catalog(api: Arc<Api>) -> Response {
    match &api.tiles {
        Some((_, tiles)) => Json(tiles.layers()).into_response(),
        None => error(StatusCode::NOT_FOUND, "no tile store configured"),
    }
}

async fn tile(api: Arc<Api>, layer: String, z: u8, x: u32, y: u32) -> Response {
    let Some((_, tiles)) = &api.tiles else {
        return error(StatusCode::NOT_FOUND, "no tile store configured");
    };
    match tiles.tile(&layer, z, x, y).await {
        Ok(Some(t)) => ([(header::CONTENT_TYPE, t.content_type)], t.bytes).into_response(),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            format!("{layer}/{z}/{x}/{y} is not in the store"),
        ),
        Err(e) => repo_error(e),
    }
}

fn open(place: &Place) -> Result<ObjectRunStore, StoreError> {
    let tuning = Tuning::from_env();
    match place {
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

    let mut projects_open = Vec::new();
    for project in config.projects {
        // The bucket, then the cache over it: both are `Objects`, and
        // everything from here on is handed the second.
        let objects: Arc<dyn Objects> = Arc::new(Cached::new(
            Arc::new(open(&project.place)?),
            cache.join(&project.name),
        ));
        let films: Arc<dyn FilmRepository> = match project.layout {
            Layout::Scenes(roots) => Arc::new(ScenePacks::new(objects.clone(), roots)),
            Layout::Runs(layout) => Arc::new(RunFilms::new(objects.clone(), layout)?),
        };
        tracing::info!(
            "project {}: {} read as {}",
            project.name,
            objects.label(),
            films.layout()
        );
        projects_open.push(Arc::new(Project {
            name: project.name,
            objects,
            films,
        }));
    }
    let tiles = match config.tiles {
        Some(place) => {
            let store = open(&place)?;
            let label = store.label().to_string();
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
    let api = Arc::new(Api {
        projects: projects_open,
        tiles,
    });

    let (a, b, c, d, f, g, h) = (
        api.clone(),
        api.clone(),
        api.clone(),
        api.clone(),
        api.clone(),
        api.clone(),
        api,
    );
    let mut app = Router::new()
        .route("/api/projects", get(move || projects(a.clone())))
        .route(
            "/api/p/{project}/ls",
            get(move |UrlPath(p): UrlPath<String>, Query(q): Query<ListQuery>| ls(b.clone(), p, q)),
        )
        .route(
            "/api/p/{project}/films",
            get(move |UrlPath(p): UrlPath<String>| films(c.clone(), p)),
        )
        .route(
            "/api/p/{project}/films/{*id}",
            get(move |UrlPath((p, id)): UrlPath<(String, String)>| film(h.clone(), p, id)),
        )
        .route(
            "/api/p/{project}/o/{*key}",
            get(
                move |UrlPath((p, k)): UrlPath<(String, String)>, h: HeaderMap| {
                    object(d.clone(), p, k, h)
                },
            ),
        )
        .route("/api/tiles/catalog", get(move || tile_catalog(f.clone())))
        .route(
            "/api/tiles/{layer}/{z}/{x}/{y}",
            get(
                move |UrlPath((l, z, x, y)): UrlPath<(String, u8, u32, u32)>| {
                    tile(g.clone(), l, z, x, y)
                },
            ),
        );
    if let Some(www) = arg(&args, "--www") {
        app = app.fallback_service(ServeDir::new(www));
    }
    let cors = CorsLayer::permissive().expose_headers([
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CONTENT_LENGTH,
    ]);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app.layer(cors)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stay_inside_the_bucket() {
        assert!(safe("packs/24cd8199f4c13cfa/1-2880.tuilepack"));
        assert!(safe("owner/run/chunks/0003.tuilepack"));
        assert!(!safe("../secrets"));
        assert!(!safe("packs/../../x"));
        assert!(!safe("/etc/passwd"));
        assert!(!safe("packs//a"));
        assert!(!safe(""));
    }

    #[test]
    fn ranges_resolve_against_the_size() {
        assert_eq!(resolve(Some("bytes=0-15"), 100), Some((0..16, true)));
        assert_eq!(resolve(Some("bytes=90-200"), 100), Some((90..100, true)));
        assert_eq!(resolve(Some("bytes=100-"), 100), None);
        assert_eq!(resolve(Some("bytes=5-4"), 100), None);
        // Open-ended: a window, never the rest of a multi-gigabyte pack.
        let big = 11 << 30;
        assert_eq!(resolve(Some("bytes=0-"), big), Some((0..OPEN_RANGE, true)));
        // No range: only for what is small enough to send whole.
        assert_eq!(resolve(None, 10), Some((0..10, false)));
        assert_eq!(resolve(None, big), None);
    }
}
