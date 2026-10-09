// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::storage::ContentStore;
use tuile_repository::bench::names;
use tuile_repository::s3::{Http, HttpReply, S3Config, S3Objects, Signed};
use tuile_repository::{
    block_segment, ArchivedTiles, Asked, Bench, Cached, ChunkStore, Config, FilmRepository, Get,
    Got, Layout, Now, Objects, Place, Project, RemoteBlocks, RemoteLive, Reply, RunFilms,
    ScenePacks, StoreObjects, TileRepository,
};
use worker::{
    console_log, event, Cache, Context, Date, Delay, Env, Fetch, Headers, Method, Request,
    RequestInit, Response, Result,
};

/// `fetch`, as the transport the S3 protocol is sent with.
struct FetchHttp;

#[async_trait(?Send)]
impl Http for FetchHttp {
    fn now(&self) -> u64 {
        Date::now().as_millis() / 1000
    }

    async fn pause(&self, milliseconds: u32) {
        Delay::from(std::time::Duration::from_millis(u64::from(milliseconds))).await;
    }

    async fn send(&self, request: &Signed) -> std::result::Result<HttpReply, String> {
        let headers = Headers::new();
        for (name, value) in &request.headers {
            headers.set(name, value).map_err(|e| e.to_string())?;
        }
        let mut init = RequestInit::new();
        init.with_method(if request.method == "HEAD" {
            Method::Head
        } else {
            Method::Get
        })
        .with_headers(headers);
        let outgoing = Request::new_with_init(&request.url, &init).map_err(|e| e.to_string())?;
        let mut response = Fetch::Request(outgoing)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let content_length = response
            .headers()
            .get("content-length")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok());
        let etag = response.headers().get("etag").ok().flatten();
        Ok(HttpReply {
            status: response.status_code(),
            content_length,
            etag,
            body: response.bytes().await.map_err(|e| e.to_string())?,
        })
    }
}

/// `fetch`, as the way to a tile store somebody else serves: a GET under its
/// address, with the header that server wants a credential in.
struct RemoteGet {
    root: String,
    credential: Option<(String, String)>,
}

#[async_trait(?Send)]
impl Get for RemoteGet {
    async fn get(&self, path: &str) -> std::result::Result<Got, String> {
        const TRIES: u32 = 4;
        let url = format!("{}/{path}", self.root);
        let mut wait = 150;
        let mut last = String::new();
        for attempt in 1..=TRIES {
            let headers = Headers::new();
            if let Some((name, value)) = &self.credential {
                headers.set(name, value).map_err(|e| e.to_string())?;
            }
            let mut init = RequestInit::new();
            init.with_method(Method::Get).with_headers(headers);
            let outgoing = Request::new_with_init(&url, &init).map_err(|e| e.to_string())?;
            match Fetch::Request(outgoing).send().await {
                // Busy, or a passing failure: worth asking again.
                Ok(response) if matches!(response.status_code(), 429 | 500 | 502 | 503 | 504) => {
                    last = format!("{path}: HTTP {}", response.status_code());
                }
                Ok(mut response) => {
                    let object_size = response
                        .headers()
                        .get("x-object-size")
                        .ok()
                        .flatten()
                        .and_then(|v| v.parse().ok());
                    return Ok(Got {
                        status: response.status_code(),
                        object_size,
                        body: response.bytes().await.map_err(|e| e.to_string())?,
                    });
                }
                Err(e) => last = format!("{path}: {e}"),
            }
            if attempt < TRIES {
                Delay::from(std::time::Duration::from_millis(wait)).await;
                wait *= 3;
            }
        }
        Err(format!("{last}, after {TRIES} tries"))
    }
}

/// Chunks kept in the edge cache of the point of presence that read them.
///
/// The cache is keyed by URL, so a chunk is given one: a name under a host
/// that does not exist, never fetched, only looked up. An entry is immutable
/// and kept for as long as the cache will have it; if it is evicted, the
/// chunk is read from the bucket again and nothing else changes.
struct EdgeChunks {
    project: String,
    /// Whether chunks read are kept here. Not for a request whose whole
    /// reply is kept by the cache in front: the same megabytes would be
    /// copied and kept twice, and an isolate serving several blocks at once
    /// runs out of memory on the copies (the runtime's log: "Worker
    /// exceeded memory limit").
    keeping: bool,
}

/// Percent-encoded as a path: a key may hold anything.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What this point of presence's cache holds under an address.
async fn lookup(url: String) -> Option<Vec<u8>> {
    let mut hit = Cache::default().get(url, false).await.ok()??;
    hit.bytes().await.ok()
}

/// Keeps bytes under an address in this point of presence's cache.
async fn keep(url: String, bytes: Vec<u8>, cache_control: &str) {
    let headers = Headers::new();
    let _ = headers.set("cache-control", cache_control);
    let _ = headers.set("content-type", "application/octet-stream");
    if let Ok(response) = Response::from_bytes(bytes) {
        // What the cache will not take is simply not kept.
        let _ = Cache::default()
            .put(url, response.with_headers(headers))
            .await;
    }
}

impl EdgeChunks {
    fn url(&self, key: &str, part: &str) -> String {
        format!(
            "https://chunks.invalid/{}/{}/{part}",
            encode(&self.project),
            encode(key)
        )
    }

    async fn lookup(&self, url: String) -> Option<Vec<u8>> {
        lookup(url).await
    }

    async fn keep(&self, url: String, bytes: Vec<u8>, cache_control: &str) {
        keep(url, bytes, cache_control).await;
    }
}

/// Small things worth remembering — what a pack was found to hold — kept in
/// the edge cache of the point of presence that worked them out, under an
/// address that is never fetched. The cache is one per point of presence: a
/// listing is slow the first time each sees a pack, and not after.
struct EdgeNotes {
    project: String,
}

/// How long a note is kept when whoever wrote it did not say.
const NOTE_KEPT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

impl EdgeNotes {
    fn url(&self, key: &str) -> String {
        format!(
            "https://notes.invalid/{}/{}",
            encode(&self.project),
            encode(key)
        )
    }
}

#[async_trait(?Send)]
impl ContentStore for EdgeNotes {
    async fn get(&self, key: &str) -> Option<Bytes> {
        lookup(self.url(key)).await.map(Bytes::from)
    }

    async fn put(&self, key: &str, value: Bytes, ttl: Option<std::time::Duration>) {
        let seconds = ttl.unwrap_or(NOTE_KEPT).as_secs();
        keep(
            self.url(key),
            value.to_vec(),
            &format!("public, max-age={seconds}"),
        )
        .await;
    }
}

/// What an object's size is kept under. Not `size`: an earlier build kept
/// sizes under that name for a year, and what it kept is still in the cache
/// and would still be believed — of objects that have since been replaced.
const SIZE_NOTE: &str = "size-for-a-minute";

#[async_trait(?Send)]
impl ChunkStore for EdgeChunks {
    async fn get(&self, key: &str, index: u64) -> Option<Vec<u8>> {
        self.lookup(self.url(key, &format!("{index:08}"))).await
    }

    async fn put(&self, key: &str, index: u64, bytes: &[u8]) {
        if !self.keeping {
            return;
        }
        // A chunk is kept under its object's size as well as its key, so it
        // is what it is for good.
        self.keep(
            self.url(key, &format!("{index:08}")),
            bytes.to_vec(),
            "public, max-age=31536000, immutable",
        )
        .await;
    }

    async fn size(&self, key: &str) -> Option<u64> {
        let bytes = self.lookup(self.url(key, SIZE_NOTE)).await?;
        std::str::from_utf8(&bytes).ok()?.parse().ok()
    }

    async fn note_size(&self, key: &str, size: u64) {
        // A size is believed for a minute: an object can be replaced under
        // its key, and its size is how that is noticed.
        self.keep(
            self.url(key, SIZE_NOTE),
            size.to_string().into_bytes(),
            "public, max-age=60",
        )
        .await;
    }
}

/// The bench this deployment serves, from its environment. The tile store is
/// opened — its catalog read — only for a request that is about tiles.
async fn bench(env: &Env, with_tiles: bool, keeping: bool) -> std::result::Result<Bench, String> {
    let var = |name: &str| {
        env.secret(name)
            .map(|s| s.to_string())
            .or_else(|_| env.var(name).map(|v| v.to_string()))
            .map_err(|_| format!("{name} is not set"))
    };
    let config = Config::parse(&var("BENCH_CONFIG")?).map_err(|e| format!("BENCH_CONFIG: {e}"))?;
    let (endpoint, key, secret) = (
        var("TUILE_STORE_ENDPOINT")?,
        var("TUILE_STORE_ACCESS_KEY_ID")?,
        var("TUILE_STORE_SECRET_ACCESS_KEY")?,
    );
    let mut projects = Vec::new();
    for project in config.projects {
        let Place::Bucket(bucket) = project.place else {
            return Err(format!(
                "project {}: a Worker reads a project from a bucket",
                project.name
            ));
        };
        // The bucket over signed HTTP, then the edge cache over it: a chunk
        // is read from the bucket once per point of presence, and every
        // range after that is cut from what the cache holds.
        let bucket: Arc<dyn Objects> = Arc::new(S3Objects::new(
            S3Config {
                endpoint: endpoint.clone(),
                bucket,
                access_key_id: key.clone(),
                secret_access_key: secret.clone(),
                region: "auto".into(),
            },
            FetchHttp,
        ));
        let objects: Arc<dyn Objects> = Arc::new(Cached::new(
            bucket,
            EdgeChunks {
                project: project.name.clone(),
                keeping,
            },
        ));
        // What a pack holds is worked out once per point of presence, and
        // remembered there: a film is listed without its packs being asked.
        let notes: Arc<dyn ContentStore> = Arc::new(EdgeNotes {
            project: project.name.clone(),
        });
        let films: Arc<dyn FilmRepository> = match project.layout {
            Layout::Scenes(roots) => {
                Arc::new(ScenePacks::new(objects.clone(), roots).remembering(notes))
            }
            Layout::Runs(layout) => Arc::new(
                RunFilms::new(objects.clone(), layout)
                    .map_err(|e| e.to_string())?
                    .remembering(notes),
            ),
        };
        projects.push(Project {
            name: project.name,
            objects,
            films,
        });
    }
    // The tile store, as objects and as tiles. As objects it costs nothing
    // to offer: nothing is read until a route asks.
    let store = match &config.tiles {
        Some(Place::Bucket(bucket)) => {
            let live: Arc<dyn Objects> = Arc::new(S3Objects::new(
                S3Config {
                    endpoint,
                    bucket: bucket.clone(),
                    access_key_id: key,
                    secret_access_key: secret,
                    region: "auto".into(),
                },
                FetchHttp,
            ));
            // The catalog and the manifests change as tiles are added, and
            // are read from the bucket; an archive never does, and is read
            // through the edge cache.
            let archives: Arc<dyn Objects> = Arc::new(Cached::new(
                live.clone(),
                EdgeChunks {
                    project: format!("tiles-{bucket}"),
                    keeping,
                },
            ));
            Some((bucket.clone(), StoreObjects { live, archives }))
        }
        Some(Place::Remote { url, header }) => {
            // Somebody else serves the store: its catalog and manifests are
            // asked of it each time, its archives by blocks — the very
            // blocks this Worker's own `store/b8/…` route is asked for, so
            // one of them is one request there, kept here by the cache in
            // front of this route.
            let credential = match header {
                Some(name) => Some((name.clone(), var("TUILE_TILES_REMOTE_SECRET")?)),
                None => None,
            };
            let get = Arc::new(RemoteGet {
                root: url.clone(),
                credential,
            });
            let live: Arc<dyn Objects> = Arc::new(RemoteLive::new(get.clone(), "store"));
            let archives: Arc<dyn Objects> = Arc::new(RemoteBlocks::new(get, "store"));
            Some((url.clone(), StoreObjects { live, archives }))
        }
        Some(Place::Dir(_)) => return Err("tiles: a Worker reads buckets, not directories".into()),
        None => None,
    };
    let tiles = match &store {
        Some((bucket, objects)) if with_tiles => {
            let now: Now = Arc::new(|| Date::now().as_millis() / 1000);
            match ArchivedTiles::open(objects.live.clone(), objects.archives.clone(), now).await {
                Ok(tiles) => {
                    let tiles: Arc<dyn TileRepository> = Arc::new(tiles);
                    Some((bucket.clone(), tiles))
                }
                Err(e) => {
                    // The films are still worth serving without it.
                    console_log!("tiles: {bucket} cannot be opened: {e}");
                    None
                }
            }
        }
        _ => None,
    };
    Ok(Bench {
        projects,
        tiles,
        store: store.map(|(_, objects)| objects),
    })
}

async fn respond(mut reply: Reply) -> Result<(Vec<u8>, u16, Headers)> {
    let headers = Headers::new();
    headers.set("content-type", &reply.content_type)?;
    headers.set("cache-control", reply.cache_control)?;
    if let Some(etag) = &reply.etag {
        headers.set("etag", etag)?;
    }
    if let Some(size) = reply.object_size {
        headers.set("x-object-size", &size.to_string())?;
    }
    if reply.ranged {
        headers.set("accept-ranges", "bytes")?;
    }
    if let Some(range) = &reply.content_range {
        headers.set("content-range", range)?;
    }
    let body = match reply.later.take() {
        // A block is one chunk of the cache below: read whole, and handed
        // over as it is.
        Some(later) => later
            .read()
            .await
            .map_err(|e| worker::Error::RustError(e.to_string()))?,
        None => std::mem::take(&mut reply.body),
    };
    Ok((body, reply.status, headers))
}

/// A response of these bytes. A reply kept in the cache is its own
/// response, not a clone of the one sent — a cloned body is a stream read
/// from two ends, and a request whose client had finished before the cache
/// had was found hung by the runtime.
fn response(body: Vec<u8>, status: u16, headers: &Headers) -> Result<Response> {
    Ok(Response::from_bytes(body)?
        .with_status(status)
        .with_headers(headers.clone()))
}

/// Whether a path is a block of an object: `/api/p/<project>/b8/<n>/<key>`.
/// A block is a whole, immutable reply — the one kind the edge cache keeps.
fn is_block(path: &str) -> bool {
    let blocks = format!("{}/", block_segment());
    // A project's object, or one of the tile store's archives.
    path.strip_prefix("/api/store/")
        .is_some_and(|rest| rest.starts_with(&blocks))
        || path
            .strip_prefix("/api/p/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, rest)| rest.starts_with(&blocks))
}

#[event(fetch)]
pub async fn main(request: Request, env: Env, ctx: Context) -> Result<Response> {
    let url = request.url()?;
    if !url.path().starts_with("/api/") {
        return env.assets("ASSETS")?.fetch_request(request).await;
    }
    if request.method() != Method::Get {
        return Response::error("the API is read-only", 405);
    }
    // A block that this point of presence has served before is served again
    // from its cache, whole, without a line of this crate's logic running:
    // no bucket, no chunk read, nothing held in memory.
    // …provided the address says which object it is a block of: a pack's
    // block names its pack's size (`?s=`), because a pack can be replaced
    // under its key and an address kept for a year must not then answer
    // with the old one. An archive of the tile store is never rewritten.
    let named = url.query_pairs().any(|(name, _)| name == "s");
    let cacheable = is_block(url.path()) && (url.path().starts_with("/api/store/") || named);
    let if_none_match = request.headers().get("if-none-match")?;
    let cache = Cache::default();
    let key = url.to_string();
    if cacheable {
        if let Ok(Some(hit)) = cache.get(&key, false).await {
            // A client that already holds this block is told so, and sent
            // nothing: the cache is asked by address, and does not see
            // what the request holds.
            let held = if_none_match
                .as_deref()
                .zip(hit.headers().get("etag")?)
                .is_some_and(|(theirs, ours)| names(theirs, &ours));
            if held {
                let headers = Headers::new();
                for name in ["etag", "cache-control", "x-object-size"] {
                    if let Some(value) = hit.headers().get(name)? {
                        headers.set(name, &value)?;
                    }
                }
                return Ok(Response::empty()?.with_status(304).with_headers(headers));
            }
            return Ok(hit);
        }
    }
    let with_tiles = url.path().starts_with("/api/tiles/") || url.path() == "/api/projects";
    let bench = match bench(&env, with_tiles, !cacheable).await {
        Ok(b) => b,
        Err(e) => return Response::error(format!("misconfigured: {e}"), 500),
    };
    let range = request.headers().get("range")?;
    let reply = match bench
        .get(
            url.path(),
            url.query().unwrap_or_default(),
            Asked {
                range: range.as_deref(),
                if_none_match: if_none_match.as_deref(),
            },
        )
        .await
    {
        Some(reply) => reply,
        None => return Response::error("no such route", 404),
    };
    let stored = cacheable && reply.status == 200;
    let (body, status, headers) = respond(reply).await?;
    if stored {
        // Kept after the reply has gone: the client does not wait for it.
        // One copy of the block and no more: several blocks are in flight
        // in one isolate at a time, and its memory is what gives first.
        let copy = response(body.clone(), status, &headers)?;
        ctx.wait_until(async move {
            let _ = cache.put(&key, copy).await;
        });
    }
    response(body, status, &headers)
}
