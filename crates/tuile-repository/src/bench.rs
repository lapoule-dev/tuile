// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The bench's API, as a function from a request to a reply.
//!
//! ```text
//! GET /api/projects                      the projects, their layout, the tile store
//! GET /api/p/<project>/films             the films the project's bucket holds
//! GET /api/p/<project>/films/<id>        one film: its packs (chunks), in order
//! GET /api/p/<project>/b8/<n>/<key>       block n of an object: a whole reply (200)
//! GET /api/p/<project>/o/<key>           an object, by byte range (206)
//! GET /api/p/<project>/ls?prefix=        one level of the bucket, as it lies
//! GET /api/tiles/catalog                 the tile store's layers
//! GET /api/tiles/<layer>/<z>/<x>/<y>     one source tile, as stored
//! ```
//!
//! No server is in here and no transport: a native process and a Worker both
//! serve these routes, and each is an adapter of a few lines that turns its
//! own request into a call to [`Bench::get`] and the [`Reply`] back into its
//! own response. What a route means — which range a header asks for, what a
//! missing film answers — is decided once.
//!
//! **A reader of packs asks for blocks, not ranges.** An object is cut into
//! fixed blocks of [`BLOCK`] bytes and each has its own URL, answered whole —
//! a plain 200, immutable. That is the one shape every cache keeps without
//! being argued with: a browser stores it like any file, and an edge cache
//! keys it by its URL. Ranged replies are neither stored nor matched
//! reliably by either, and a range is whatever its asker computed, so no two
//! askers share one. Blocks are the same for every frame, every slice of a
//! film and every reader of it. The ranged route stays, for a media element
//! and a curious human.
//!
//! Every route is a GET and nothing writes to a bucket.

use std::ops::Range;
use std::sync::Arc;

use percent_encoding::percent_decode_str;

use crate::{FilmRepository, Objects, RepoError, TileRepository, CHUNK};

/// The size of a block: what one request for a pack brings back.
///
/// It is a trade. A frame reads a few megabytes scattered over its pack, so a
/// small block wastes little and costs a request each; a large one brings
/// bytes nobody asked for and saves the requests. And a block is held whole
/// by whoever serves it — several at once, when a film's workers ask
/// together, in a Worker that has 128 MB for everything. So a block is one
/// chunk of the cache below, read and handed over without being assembled
/// or copied, and eight megabytes is as large as that stays comfortable.
pub const BLOCK: u64 = CHUNK;

/// The path segment that names a block: `b8` for eight-megabyte blocks.
/// The size is in the URL because a block is immutable there: cut another
/// way, the same number would be other bytes under an address every cache
/// has been told never to ask about again.
pub fn block_segment() -> String {
    format!("b{}", BLOCK >> 20)
}

/// How much an open-ended range (`bytes=N-`) is answered with. A media
/// element asks that way and then asks again from where the answer stopped.
pub const OPEN_RANGE: u64 = 8 << 20;
/// The largest object served without a range.
pub const WHOLE_LIMIT: u64 = 32 << 20;
/// The most one request is answered with. A reply is held whole while it is
/// put together, and a host may have little memory to hold it in — a Worker
/// has 128 MB for everything, shared by the requests it is serving at that
/// moment. A client reads a large span as several.
pub const RANGE_LIMIT: u64 = 32 << 20;

pub struct Project {
    pub name: String,
    pub objects: Arc<dyn Objects>,
    pub films: Arc<dyn FilmRepository>,
}

/// Everything a bench serves.
pub struct Bench {
    pub projects: Vec<Project>,
    /// The tile store and where it is, when there is one.
    pub tiles: Option<(String, Arc<dyn TileRepository>)>,
}

/// An HTTP response, before any server has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    /// `Content-Range`, on a partial reply.
    pub content_range: Option<String>,
    /// Whether the body is bytes of an object, which a client may ask for by
    /// range.
    pub ranged: bool,
    /// A strong validator for the object, on a reply that carries its bytes.
    pub etag: Option<String>,
    /// The whole object's size, on a block: its reader cannot tell a short
    /// last block from a truncated one otherwise.
    pub object_size: Option<u64>,
    /// `Cache-Control`.
    pub cache_control: &'static str,
    pub body: Vec<u8>,
    /// The body, when it has not been read: `body` is then empty, and whoever
    /// sends the reply reads this as it sends.
    pub later: Option<Later>,
}

/// A body not read yet: ranges of one object, to be sent in order.
///
/// A block is megabytes, and whoever serves it decides how to hold them: a
/// reply names the pieces — each one chunk of the cache below, so each is
/// read without being assembled or copied — and its sender reads them when
/// it sends.
#[derive(Clone)]
pub struct Later {
    objects: Arc<dyn Objects>,
    pub key: String,
    pub ranges: Vec<Range<u64>>,
}

impl Later {
    /// How many bytes the whole body is.
    pub fn len(&self) -> u64 {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One piece of the body.
    pub async fn part(&self, index: usize) -> Result<Vec<u8>, RepoError> {
        match self.ranges.get(index) {
            Some(range) => self.objects.read(&self.key, range.clone()).await,
            None => Ok(Vec::new()),
        }
    }

    /// The whole body, for a server with the memory to hold it.
    pub async fn read(&self) -> Result<Vec<u8>, RepoError> {
        let mut out = Vec::with_capacity(self.len() as usize);
        for index in 0..self.ranges.len() {
            out.extend(self.part(index).await?);
        }
        Ok(out)
    }
}

impl std::fmt::Debug for Later {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:?}", self.key, self.ranges)
    }
}

impl PartialEq for Later {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.ranges == other.ranges
    }
}

impl Eq for Later {}

/// What a listing or an error is sent with: asked for again each time. What
/// a bucket holds changes.
pub const FRESH: &str = "no-cache";

/// What an object's bytes are sent with.
///
/// Everything served here is immutable under its key — a pack's key hashes
/// its inputs, a run's prefix carries its timestamp — so a browser may keep
/// what it has read for as long as it likes, without asking again. With the
/// strong validator beside it, its HTTP cache stores each ranged reply and
/// answers the same range later from disk or memory: a 206 that never
/// leaves the machine. A film rendered twice reads its pack from the
/// network once.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The validator of an object: its key and its size, which is all that
/// identifies it and all that is known without reading it.
fn etag(key: &str, size: u64) -> String {
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("\"{hash:016x}-{size:x}\"")
}

impl Reply {
    fn text(status: u16, message: impl std::fmt::Display) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".into(),
            content_range: None,
            ranged: false,
            etag: None,
            object_size: None,
            cache_control: FRESH,
            body: message.to_string().into_bytes(),
            later: None,
        }
    }

    fn json(value: &impl serde::Serialize) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self {
                status: 200,
                content_type: "application/json".into(),
                content_range: None,
                ranged: false,
                etag: None,
                object_size: None,
                cache_control: FRESH,
                body,
                later: None,
            },
            Err(e) => Self::text(500, e),
        }
    }

    /// The body, whole: read now if it had not been.
    pub async fn whole(self) -> Result<Vec<u8>, RepoError> {
        match self.later {
            Some(later) => later.read().await,
            None => Ok(self.body),
        }
    }

    fn of(e: RepoError) -> Self {
        let status = match e {
            RepoError::NotFound(_) => 404,
            RepoError::Malformed { .. } => 422,
            RepoError::Store(_) => 502,
        };
        Self::text(status, e)
    }
}

/// A key is a path inside the bucket, never a way out of it.
pub fn safe(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn content_type(key: &str) -> &'static str {
    match key.rsplit('.').next().unwrap_or_default() {
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "txt" | "scene" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `bytes=a-b`, `bytes=a-` or nothing, resolved against the object's size:
/// the bytes to send, and whether that is a partial reply. `None` is a range
/// that cannot be satisfied.
pub fn resolve(range: Option<&str>, size: u64) -> Option<(Range<u64>, bool)> {
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

fn decoded(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

/// The value of `name` in a query string.
fn param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| decoded(&v.replace('+', " ")))
}

impl Bench {
    fn project(&self, name: &str) -> Result<&Project, Reply> {
        self.projects
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| Reply::text(404, format!("no project {name}")))
    }

    /// Answers a GET. `path` is as the request line has it, still
    /// percent-encoded; `range` is the `Range` header. `None` is a path this
    /// API does not own, for the caller to serve something else on.
    pub async fn get(&self, path: &str, query: &str, range: Option<&str>) -> Option<Reply> {
        let rest = path.strip_prefix("/api/")?;
        Some(match self.route(rest, query, range).await {
            Ok(reply) | Err(reply) => reply,
        })
    }

    async fn route(&self, rest: &str, query: &str, range: Option<&str>) -> Result<Reply, Reply> {
        if rest == "projects" {
            return Ok(Reply::json(&serde_json::json!({
                "projects": self.projects.iter().map(|p| serde_json::json!({
                    "name": p.name, "store": p.objects.label(), "layout": p.films.layout(),
                })).collect::<Vec<_>>(),
                "tiles": self.tiles.as_ref().map(|(label, _)| label),
            })));
        }
        if let Some(rest) = rest.strip_prefix("tiles/") {
            return self.tiles_route(rest).await;
        }
        let (name, rest) = rest
            .strip_prefix("p/")
            .and_then(|r| r.split_once('/'))
            .ok_or_else(|| Reply::text(404, "no such route"))?;
        let project = self.project(&decoded(name))?;
        if rest == "films" {
            return Ok(Reply::json(
                &project.films.films().await.map_err(Reply::of)?,
            ));
        }
        if let Some(id) = rest.strip_prefix("films/") {
            return Ok(Reply::json(
                &project.films.film(&decoded(id)).await.map_err(Reply::of)?,
            ));
        }
        if rest == "ls" {
            let prefix = param(query, "prefix").unwrap_or_default();
            return Ok(Reply::json(
                &project.objects.browse(&prefix).await.map_err(Reply::of)?,
            ));
        }
        if let Some(key) = rest.strip_prefix("o/") {
            return self.object(project, &decoded(key), range).await;
        }
        if let Some((index, key)) = rest
            .strip_prefix(&format!("{}/", block_segment()))
            .and_then(|r| r.split_once('/'))
        {
            let index = index.parse().map_err(|_| {
                Reply::text(
                    400,
                    format!("a block is {}/<number>/<key>", block_segment()),
                )
            })?;
            return self.block(project, &decoded(key), index).await;
        }
        Err(Reply::text(404, "no such route"))
    }

    async fn object(
        &self,
        project: &Project,
        key: &str,
        range: Option<&str>,
    ) -> Result<Reply, Reply> {
        if !safe(key) {
            return Err(Reply::text(400, format!("not a key: {key}")));
        }
        let size = project.objects.size(key).await.map_err(Reply::of)?;
        let Some((bytes, partial)) = resolve(range, size) else {
            return Err(match range {
                Some(_) => Reply {
                    content_range: Some(format!("bytes */{size}")),
                    ..Reply::text(416, "range not satisfiable")
                },
                None => Reply::text(400, format!("{key} is {size} bytes: ask for a range")),
            });
        };
        if bytes.end - bytes.start > RANGE_LIMIT {
            return Err(Reply::text(
                400,
                format!(
                    "{} bytes asked for in one range; {RANGE_LIMIT} at most — read it as several",
                    bytes.end - bytes.start
                ),
            ));
        }
        let body = project
            .objects
            .read(key, bytes.clone())
            .await
            .map_err(Reply::of)?;
        Ok(Reply {
            status: if partial { 206 } else { 200 },
            content_type: content_type(key).into(),
            content_range: partial
                .then(|| format!("bytes {}-{}/{size}", bytes.start, bytes.end - 1)),
            ranged: true,
            etag: Some(etag(key, size)),
            object_size: Some(size),
            cache_control: IMMUTABLE,
            body,
            later: None,
        })
    }

    /// Block `index` of an object: bytes `index × BLOCK` up to the next
    /// block, or the object's end. Whole, immutable, and the same for whoever
    /// asks.
    async fn block(&self, project: &Project, key: &str, index: u64) -> Result<Reply, Reply> {
        if !safe(key) {
            return Err(Reply::text(400, format!("not a key: {key}")));
        }
        let size = project.objects.size(key).await.map_err(Reply::of)?;
        let start = index.saturating_mul(BLOCK);
        if start >= size {
            return Err(Reply::text(404, format!("{key} has no block {index}")));
        }
        // Not read here: cut along the cache's chunks, for whoever sends it
        // to read a piece at a time.
        let end = (start + BLOCK).min(size);
        let ranges = (start..end)
            .step_by(CHUNK as usize)
            .map(|at| at..(at + CHUNK).min(end))
            .collect();
        Ok(Reply {
            status: 200,
            content_type: "application/octet-stream".into(),
            content_range: None,
            ranged: false,
            etag: Some(format!(
                "{}-{index}\"",
                etag(key, size).trim_end_matches('"')
            )),
            object_size: Some(size),
            cache_control: IMMUTABLE,
            body: Vec::new(),
            later: Some(Later {
                objects: project.objects.clone(),
                key: key.to_string(),
                ranges,
            }),
        })
    }

    async fn tiles_route(&self, rest: &str) -> Result<Reply, Reply> {
        let Some((_, tiles)) = &self.tiles else {
            return Err(Reply::text(404, "no tile store configured"));
        };
        if rest == "catalog" {
            return Ok(Reply::json(&tiles.layers()));
        }
        let parts: Vec<&str> = rest.split('/').collect();
        let [layer, z, x, y] = parts.as_slice() else {
            return Err(Reply::text(404, "no such route"));
        };
        let (Ok(z), Ok(x), Ok(y)) = (z.parse::<u8>(), x.parse::<u32>(), y.parse::<u32>()) else {
            return Err(Reply::text(400, "a tile is <layer>/<z>/<x>/<y>"));
        };
        let layer = decoded(layer);
        match tiles.tile(&layer, z, x, y).await.map_err(Reply::of)? {
            Some(tile) => Ok(Reply {
                status: 200,
                content_type: tile.content_type,
                content_range: None,
                ranged: false,
                etag: None,
                object_size: None,
                // A tile can be refetched and replaced in the store.
                cache_control: FRESH,
                body: tile.bytes,
                later: None,
            }),
            None => Err(Reply::text(
                404,
                format!("{layer}/{z}/{x}/{y} is not in the store"),
            )),
        }
    }
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

    #[test]
    fn a_range_too_large_for_one_reply_is_refused_before_it_is_read() {
        use crate::{Entry, Listing, Objects, RepoError};
        use std::sync::Arc;

        /// A huge object nobody may read: reading it fails the test.
        struct Huge;
        #[async_trait::async_trait]
        impl Objects for Huge {
            fn label(&self) -> String {
                "huge".into()
            }
            async fn list(&self, _: &str) -> Result<Vec<Entry>, RepoError> {
                Ok(Vec::new())
            }
            async fn browse(&self, _: &str) -> Result<Listing, RepoError> {
                Ok(Listing::default())
            }
            async fn size(&self, _: &str) -> Result<u64, RepoError> {
                Ok(3 << 30)
            }
            async fn read(&self, _: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
                assert!(
                    range.end - range.start <= RANGE_LIMIT,
                    "a read past the limit reached the bucket"
                );
                Ok(vec![0; (range.end - range.start) as usize])
            }
        }
        let objects: Arc<dyn Objects> = Arc::new(Huge);
        let bench = Bench {
            projects: vec![Project {
                name: "p".into(),
                films: Arc::new(crate::ScenePacks::new(objects.clone(), ["packs"])),
                objects,
            }],
            tiles: None,
        };
        let get = |range: &'static str| {
            futures_executor::block_on(bench.get("/api/p/p/o/a.tuilepack", "", Some(range)))
        };
        let too_much = get("bytes=2137432-163204141").expect("reply");
        assert_eq!(too_much.status, 400);
        assert!(String::from_utf8_lossy(&too_much.body).contains("read it as several"));
        assert_eq!(get("bytes=0-4194303").expect("reply").status, 206);
        // A block of it is cut into pieces and none of them is read here.
        let block = futures_executor::block_on(bench.get(
            &format!("/api/p/p/{}/3/a.tuilepack", block_segment()),
            "",
            None,
        ))
        .expect("reply");
        assert_eq!(block.status, 200);
        assert!(block.body.is_empty());
        let later = block.later.expect("a block is read as it is sent");
        assert_eq!(later.len(), BLOCK);
        assert_eq!(later.ranges.len(), (BLOCK / CHUNK) as usize);
        assert_eq!(later.ranges[0].start, 3 * BLOCK);
        assert!(later
            .ranges
            .iter()
            .all(|r| r.start % CHUNK == 0 && r.end - r.start == CHUNK));
        // The whole of it, with no range, is refused too.
        let whole = futures_executor::block_on(bench.get("/api/p/p/o/a.tuilepack", "", None))
            .expect("reply");
        assert_eq!(whole.status, 400);
    }

    #[test]
    fn a_query_gives_its_parameter_decoded() {
        assert_eq!(
            param("prefix=packs%2Fabc&x=1", "prefix").as_deref(),
            Some("packs/abc")
        );
        assert_eq!(param("a=1", "prefix"), None);
        assert_eq!(param("", "prefix"), None);
    }
}
