// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A local HTTP API over the farm's packs.
//!
//! ```text
//! GET /packs?prefix=packs/        the farm's packs: [{key, size, cached}]
//! GET /packs/<key>                a pack, by byte range (206)
//! GET /pmtiles?prefix=<zone>/     the tile store's PMTiles archives
//! GET /pmtiles/<key>              an archive, by byte range (206)
//! GET /pmtiles/catalog.json       the tile store's catalog, as stored
//! GET /                           the film bench page, if --www is given
//! ```
//!
//! Both shelves work the same way. An object is fetched from its store once,
//! into the cache directory, and every request after that reads the local file
//! by range. Both kinds are immutable under their keys — a pack's key hashes its
//! inputs, and the tile store names each archive afresh on every compaction —
//! so a cached copy never goes stale. A pack's blob digest is also checked on
//! the way in.
//!
//! A pack is fetched from the store **once**, into the cache directory, and its
//! blob digest checked on the way in; every request after that — every worker
//! of every film rendered from it — reads the local file by range. A browser
//! never holds a whole pack: each worker asks for the bytes of the tiles its
//! frames bring in.
//!
//! Packs come from the farm's store, configured as for `tuile-farm`:
//! `TUILE_STORE_DIR`, or `TUILE_STORE_ENDPOINT`, `TUILE_STORE_BUCKET`,
//! `TUILE_STORE_ACCESS_KEY_ID`, `TUILE_STORE_SECRET_ACCESS_KEY`. PMTiles come
//! from the tile store, as for a bake: `TUILE_TILES_DIR`, or
//! `TUILE_TILES_BUCKET` on the same endpoint and credentials. A shelf with no
//! configuration is simply not served. A `.env` in the working directory is
//! read first. Credentials stay in this process; the page never sees them.
//!
//! ```bash
//! cargo run --release -p tuile-pack-api -- --www examples/film-web/www
//! ```

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, Request};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tuile_farm::{BucketConfig, ObjectRunStore, RunStore, Tuning};

/// One kind of object, from one store, cached in one directory.
struct Shelf {
    name: &'static str,
    store: ObjectRunStore,
    cache: PathBuf,
    suffix: &'static str,
    /// Packs carry a digest of their blob region; archives do not.
    verify: bool,
    /// One lock per key: two workers asking for an object nobody has
    /// fetched yet must wait for one download, not start two.
    fetching: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(serde::Deserialize)]
struct ListQuery {
    prefix: Option<String>,
}

#[derive(serde::Serialize)]
struct Listed {
    key: String,
    size: u64,
    cached: bool,
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    tracing::warn!("{status}: {message}");
    (status, message.to_string()).into_response()
}

/// A key is a path inside the store, never outside the cache.
fn safe(key: &str, suffix: &str) -> bool {
    !key.is_empty()
        && key.ends_with(suffix)
        && !key.starts_with('/')
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

async fn list(shelf: Arc<Shelf>, q: ListQuery) -> Response {
    let api = shelf;
    let prefix = q.prefix.unwrap_or_default();
    match api.store.list(&prefix).await {
        Ok(entries) => Json(
            entries
                .into_iter()
                .filter(|e| e.key.ends_with(api.suffix))
                .map(|e| Listed {
                    cached: api.cache.join(&e.key).exists(),
                    key: e.key,
                    size: e.size,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, e),
    }
}

/// Folds the blob digest over a pack on disk and compares it with its table.
fn verify(path: &Path) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut preamble = [0u8; tuile_pack::PREAMBLE];
    file.read_exact(&mut preamble).map_err(|e| e.to_string())?;
    let start = tuile_pack::blob_start(&preamble).map_err(|e| e.to_string())? as usize;
    let mut head = preamble.to_vec();
    head.resize(start, 0);
    file.read_exact(&mut head[tuile_pack::PREAMBLE..])
        .map_err(|e| e.to_string())?;
    let expected = tuile_pack::Pack::open_table(&head)
        .map_err(|e| e.to_string())?
        .blob_digest();
    let mut digest = tuile_pack::Fnv1a::default();
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    let found = digest.finish();
    if found != expected {
        return Err(format!(
            "blob digest {found:016x}, the table says {expected:016x}"
        ));
    }
    Ok(())
}

/// The object's local copy, fetched (and verified) if this is the first ask.
async fn cached(api: &Shelf, key: &str) -> Result<PathBuf, Response> {
    let path = api.cache.join(key);
    if path.exists() {
        return Ok(path);
    }
    let lock = api
        .fetching
        .lock()
        .await
        .entry(key.to_string())
        .or_default()
        .clone();
    let _held = lock.lock().await;
    if path.exists() {
        return Ok(path);
    }
    let partial = path.with_extension("part");
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    let t = std::time::Instant::now();
    tracing::info!("{}: fetching {key} from {}", api.name, api.store.label());
    let bytes = api.store.get(key, &partial).await.map_err(|e| match e {
        tuile_farm::StoreError::NotFound(_) => error(StatusCode::NOT_FOUND, e),
        e => error(StatusCode::BAD_GATEWAY, e),
    })?;
    if api.verify {
        let check = partial.clone();
        tokio::task::spawn_blocking(move || verify(&check))
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
            .map_err(|e| error(StatusCode::BAD_GATEWAY, format!("{key}: {e}")))?;
    }
    tokio::fs::rename(&partial, &path)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::info!(
        "{key}: {:.1} MB fetched and verified in {:.1} s",
        bytes as f64 / 1e6,
        t.elapsed().as_secs_f64()
    );
    Ok(path)
}

async fn object(api: Arc<Shelf>, request: Request) -> Response {
    let prefix = format!("/{}/", api.name);
    let key = request.uri().path().trim_start_matches(&prefix).to_string();
    if !safe(&key, api.suffix) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("not a {} key: {key}", api.name),
        );
    }
    let path = match cached(&api, &key).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut response = match tower::ServiceExt::oneshot(ServeFile::new(path), request).await {
        Ok(r) => r.into_response(),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // Nothing under a key ever changes (see the module docs).
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}

/// The tile store's catalog, fetched afresh: it is the one object that does
/// change, as archives are added and compacted.
async fn catalog(api: Arc<Shelf>) -> Response {
    let tmp = api.cache.join(".catalog.json.part");
    if let Err(e) = tokio::fs::create_dir_all(&api.cache).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = api.store.get("catalog.json", &tmp).await {
        return error(StatusCode::BAD_GATEWAY, e);
    }
    match tokio::fs::read(&tmp).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// The tile store, as a bake finds it: a directory, or `TUILE_TILES_BUCKET`
/// on the farm's endpoint and credentials.
fn tile_store() -> Result<Option<ObjectRunStore>, tuile_farm::StoreError> {
    let tuning = Tuning::from_env();
    if let Ok(dir) = std::env::var("TUILE_TILES_DIR") {
        return ObjectRunStore::local(Path::new(&dir), tuning).map(Some);
    }
    let Ok(bucket) = std::env::var("TUILE_TILES_BUCKET") else {
        return Ok(None);
    };
    let config = BucketConfig {
        bucket,
        ..BucketConfig::from_env()?
    };
    ObjectRunStore::bucket(&config, tuning).map(Some)
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tuile_pack_api=info".into()),
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
    let mut app = Router::new();
    let packs = match ObjectRunStore::from_env() {
        Ok(store) => Some(store),
        Err(e) => {
            tracing::warn!("no pack store ({e}): /packs is not served");
            None
        }
    };
    let shelves = [
        packs.map(|store| ("packs", store, ".tuilepack", true)),
        tile_store()?.map(|store| ("pmtiles", store, ".pmtiles", false)),
    ];
    for (name, store, suffix, verify) in shelves.into_iter().flatten() {
        tracing::info!(
            "/{name}: {}, cached in {}",
            store.label(),
            cache.join(name).display()
        );
        let shelf = Arc::new(Shelf {
            name,
            store,
            cache: cache.join(name),
            suffix,
            verify,
            fetching: Mutex::default(),
        });
        let (l, o) = (shelf.clone(), shelf.clone());
        app = app
            .route(
                &format!("/{name}"),
                get(move |Query(q): Query<ListQuery>| list(l.clone(), q)),
            )
            .route(
                &format!("/{name}/{{*key}}"),
                get(move |r: Request| object(o.clone(), r)),
            );
        if name == "pmtiles" {
            let c = shelf.clone();
            app = app.route("/pmtiles/catalog.json", get(move || catalog(c.clone())));
        }
    }
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
    fn keys_stay_inside_the_cache() {
        let t = ".tuilepack";
        assert!(safe("packs/24cd8199f4c13cfa/1-2880.tuilepack", t));
        assert!(safe(
            "z6/1759000000-000000000000000000000001-00000000deadbeef.pmtiles",
            ".pmtiles"
        ));
        assert!(!safe("../secrets.tuilepack", t));
        assert!(!safe("packs/../../x.tuilepack", t));
        assert!(!safe("/etc/x.tuilepack", t));
        assert!(!safe("packs/a.mp4", t));
        assert!(!safe("packs//a.tuilepack", t));
    }
}
