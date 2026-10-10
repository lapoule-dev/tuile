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
//! GET /api/store/live/<key>               the store's catalog, or a manifest
//! POST /api/store/live                    many of those, in one reply
//! GET /api/store/b8/<n>/<key>             block n of one of the store's archives
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
//! a plain 200 with a validator. That is the one shape every cache keeps
//! without being argued with: a browser stores it like any file, and an edge
//! cache keys it by its URL. An object can be written again under its key, so
//! a block is kept a moment and then asked about, by its validator: a block
//! that has not changed costs a 304 and no bytes. Ranged replies are neither stored nor matched
//! reliably by either, and a range is whatever its asker computed, so no two
//! askers share one. Blocks are the same for every frame, every slice of a
//! film and every reader of it. The ranged route stays, for a media element
//! and a curious human.
//!
//! Every route is a GET and nothing writes to a bucket — but one, which is
//! a POST and writes nothing either: `store/live` with no key is asked for
//! many small objects at once, and a list of several hundred keys and their
//! validators does not fit an address. See [`Bench::post`].

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use percent_encoding::percent_decode_str;

use crate::{FilmRepository, Objects, Read, RepoError, TileRepository, CHUNK};

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
/// The size is in the URL because a block's number means nothing without
/// it: cut another way, the same number would be other bytes under the same
/// address and the same validator.
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

/// The tile store as the objects it is made of, for a reader that looks
/// tiles up itself.
///
/// A store is a catalog, a manifest per zone, and archives. A reader that
/// is handed those — rather than tiles, one request each — finds a tile the
/// way the store's own reader does, and what crosses the network is blocks
/// of archives: few, large, validated, and the same for whoever asks, which
/// is the one shape every cache keeps.
pub struct StoreObjects {
    /// The catalog and the manifests: rewritten as tiles are added.
    pub live: Arc<dyn Objects>,
    /// The archives: read by blocks, each with its object's validator.
    pub archives: Arc<dyn Objects>,
}

/// Everything a bench serves.
pub struct Bench {
    pub projects: Vec<Project>,
    /// The tile store and where it is, when there is one.
    pub tiles: Option<(String, Arc<dyn TileRepository>)>,
    /// The same store, as objects.
    pub store: Option<StoreObjects>,
    /// Where a reader finds the store when it is not behind this API: see
    /// [`StoreAt`].
    pub store_at: Option<StoreAt>,
}

/// A tile store served somewhere else than behind this API, as its readers
/// are told (`GET store/at`): what its `store/live/…` and `store/b8/…` routes
/// are under, and the header — its name and its value — that server wants.
/// The value is handed to every reader: it is one a host means to be public.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoreAt {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The query parameter the credential goes in instead of a header: a
    /// request with no header of its own is not preflighted by a browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
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

/// What a store's catalog and manifests are sent with: kept a moment, so a
/// film's workers do not each ask for the same manifest, and asked for again
/// soon, since tiles are added.
pub const BRIEF: &str = "public, max-age=30";

/// The largest catalog or manifest served: they are kilobytes.
const LIVE_LIMIT: u64 = 4 << 20;

/// What the reply to a POST is sent with: kept by nobody. It answers a body
/// — which keys, held under which validators — that no cache keys by, so a
/// reply kept would be the answer to somebody else's question; and it has
/// no validator of its own to be asked about with, which is all [`FRESH`]
/// would allow a cache to do with it. What is kept of it is kept by its
/// reader, object by object, under each one's validator.
pub const UNKEPT: &str = "no-store";

/// The most keys one `POST store/live` asks about. A film's stretch of
/// frames lies in a few hundred zones; a reader with more to ask asks
/// again. It bounds what one request makes its server read.
pub const LIVE_MANY: usize = 512;

/// The most bytes of objects one `POST store/live` is answered with: the
/// reply is held whole while it is put together, in a host that may have
/// little memory to hold it in. Manifests are a kilobyte or two, so this is
/// never met by what the route is for; an object that would pass it is not
/// read, and its asker is told to ask for it alone (`413`).
pub const LIVE_REPLY: u64 = 16 << 20;

/// How many of a `POST store/live`'s objects are read from the bucket at a
/// time. Those in flight when the reply fills are still read: the bound on
/// the reply is passed by that many objects at most.
const LIVE_AT_ONCE: usize = 8;

/// The body of a `POST store/live`: the objects asked about.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveAsked {
    pub objects: Vec<LiveAsk>,
}

/// One object asked about: its key, as `store/live/<key>` would name it
/// (not percent-encoded), and the validator its asker holds it under — the
/// `ETag` it was last sent with, as it was sent — or none.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveAsk {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// The reply to a `POST store/live`: one answer per object asked about, in
/// the order asked.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveAnswers {
    pub objects: Vec<LiveAnswer>,
}

/// What became of one object asked about, told as `GET store/live/<key>`
/// would have told it, by a status:
///
/// - `200` — the object: `body` is it, a JSON document as the string it is
///   stored as, byte for byte; `etag` its validator where its store has one;
/// - `304` — it is still the one held under the validator asked with:
///   `etag` is that validator, and there is no body;
/// - `404` — the store has no such object;
/// - `400` — not a key this route serves, or an object too large for it;
/// - `413` — not read: the reply is full ([`LIVE_REPLY`]). Ask for it alone;
/// - anything else — the store could not be asked (`502`), or what it holds
///   is not text (`422`).
///
/// `error` says why, for every status that is not `200` or `304`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveAnswer {
    pub key: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl LiveAnswer {
    fn refused(key: &str, status: u16, why: impl std::fmt::Display) -> Self {
        Self {
            key: key.to_string(),
            status,
            error: Some(why.to_string()),
            ..Self::default()
        }
    }
}

/// Whether `store/live` serves a key: only what a store rewrites, which is
/// small — its catalog, its manifests, its tables. Everything else is an
/// archive, read by blocks; and a key is never a way out of the bucket.
fn is_live(key: &str) -> bool {
    safe(key) && key.ends_with(".json")
}

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

/// What a request asks beside its path: the headers this API reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Asked<'a> {
    /// The `Range` header.
    pub range: Option<&'a str>,
    /// The `If-None-Match` header: the validators of what the client holds.
    pub if_none_match: Option<&'a str>,
}

impl<'a> From<Option<&'a str>> for Asked<'a> {
    /// A request with a `Range` header, or none, and nothing else.
    fn from(range: Option<&'a str>) -> Self {
        Self {
            range,
            if_none_match: None,
        }
    }
}

/// The validators an `If-None-Match` header lists, as their opaque tags:
/// weakness (`W/`) dropped, quotes kept.
fn tags(if_none_match: &str) -> impl Iterator<Item = &str> {
    if_none_match
        .split(',')
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .filter(|tag| !tag.is_empty())
}

/// Whether an `If-None-Match` header names `etag`: by the weak comparison,
/// which is the one a GET is revalidated with — an intermediary that
/// re-encodes a body weakens its validator, and it is still the same body
/// to whoever holds it. `*` names anything.
pub fn names(if_none_match: &str, etag: &str) -> bool {
    let etag = etag.trim_start_matches("W/");
    tags(if_none_match).any(|tag| tag == "*" || tag == etag)
}

/// The validator of a body that has no other: what it is, and how long.
fn body_tag(body: &[u8]) -> String {
    let hash = body.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("\"{hash:016x}-{:x}\"", body.len())
}

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
    /// This reply as the answer to a client that already holds it: its
    /// validator and how long to keep it, and no body.
    fn not_modified(self) -> Self {
        Reply {
            status: 304,
            content_range: None,
            ranged: false,
            body: Vec::new(),
            later: None,
            ..self
        }
    }

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
    /// percent-encoded; `asked` is what its headers ask — a `Range` alone
    /// may be given as it is. `None` is a path this API does not own, for
    /// the caller to serve something else on.
    ///
    /// Every reply that carries a body carries a validator, and a request
    /// that names it is answered 304 with no body: for a block, without a
    /// byte of it having been read.
    pub async fn get<'a>(
        &self,
        path: &str,
        query: &str,
        asked: impl Into<Asked<'a>>,
    ) -> Option<Reply> {
        let asked = asked.into();
        let rest = path.strip_prefix("/api/")?;
        let mut reply = match self.route(rest, query, &asked).await {
            Ok(reply) | Err(reply) => reply,
        };
        if reply.status == 200 && reply.etag.is_none() && reply.later.is_none() {
            reply.etag = Some(body_tag(&reply.body));
        }
        let held = matches!(reply.status, 200 | 206)
            && asked
                .if_none_match
                .zip(reply.etag.as_deref())
                .is_some_and(|(theirs, ours)| names(theirs, ours));
        if held {
            reply = reply.not_modified();
        }
        Some(reply)
    }

    /// Answers a POST: `path` and `query` as [`Self::get`] takes them, and
    /// the request's body. `None` is a path this API does not own.
    ///
    /// One route takes one: **`POST /api/store/live`**, which answers for
    /// many of the store's small objects — its catalog, its manifests — what
    /// `GET /api/store/live/<key>` answers for one. A film's frames lie in
    /// several hundred zones, each with a manifest of a kilobyte: asked one
    /// by one that is several hundred round trips, conditional or not, to
    /// learn that nothing changed. Asked here it is one.
    ///
    /// The body is a [`LiveAsked`], as JSON: `{"objects":[{"key":
    /// "imagery/top/manifest.json","etag":"\"abc\""},{"key":"catalog.json"}]}`
    /// — at most [`LIVE_MANY`] objects, each with the validator its asker
    /// holds or none. The reply is a [`LiveAnswers`], `application/json`:
    /// one [`LiveAnswer`] per object, in the order asked, each with the
    /// status the GET would have had. What goes wrong with one object is
    /// that object's answer; only a body that cannot be read, or asks for
    /// too many, fails the request (`400`).
    ///
    /// **The body is JSON and says so** (`application/json`). A browser
    /// that sends it to another origin asks first — an `OPTIONS` for this
    /// one address — and keeps the answer for as long as the server lets it
    /// (`Access-Control-Max-Age`), so a film pays one preflight, not one per
    /// request: whoever serves this route across origins answers that
    /// `OPTIONS`, allowing the `content-type` header, with a long max-age.
    /// The credential still travels in the query string, as it does for the
    /// GETs, so the address — and with it the kept preflight — is the same
    /// for every request. The content type is not required here: a body
    /// that reads as the JSON above is answered whatever it was labelled.
    ///
    /// Any other path is answered `405`: the rest of the API is read with
    /// GETs. A reader takes that, or a `404`, from an older server as "not
    /// here", and goes back to asking one by one.
    pub async fn post(&self, path: &str, query: &str, body: &[u8]) -> Option<Reply> {
        // Nothing of the query is this route's: whoever serves it reads a
        // credential there.
        let _ = query;
        let rest = path.strip_prefix("/api/")?;
        if rest != "store/live" {
            return Some(Reply::text(405, "the API is read with GETs"));
        }
        let Some(store) = &self.store else {
            return Some(Reply::text(404, "no tile store configured"));
        };
        let asked: LiveAsked = match serde_json::from_slice(body) {
            Ok(asked) => asked,
            Err(e) => return Some(Reply::text(400, format!("not a list of objects: {e}"))),
        };
        if asked.objects.len() > LIVE_MANY {
            return Some(Reply::text(
                400,
                format!(
                    "{} objects asked about in one request; {LIVE_MANY} at most — ask again for the rest",
                    asked.objects.len()
                ),
            ));
        }
        // What the bodies read so far come to. Reads are made a few at a
        // time and answered in order; once the reply is full nothing more
        // is read.
        let spent = AtomicU64::new(0);
        let mut objects = Vec::with_capacity(asked.objects.len());
        for some in asked.objects.chunks(LIVE_AT_ONCE) {
            objects.extend(
                futures_util::future::join_all(
                    some.iter().map(|ask| live_answer(store, ask, &spent)),
                )
                .await,
            );
        }
        Some(Reply {
            cache_control: UNKEPT,
            ..Reply::json(&LiveAnswers { objects })
        })
    }

    async fn route(&self, rest: &str, query: &str, asked: &Asked<'_>) -> Result<Reply, Reply> {
        let range = asked.range;
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
        if let Some(rest) = rest.strip_prefix("store/") {
            return self.store_route(rest, asked).await;
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
            let reply = block(&project.objects, &decoded(key), index).await?;
            // `?s=<size>` says which object the asker means. A pack can be
            // replaced under its key; an asker holding the old one's plan
            // must be told, not handed blocks of another pack.
            if let Some(meant) = param(query, "s") {
                if reply.object_size.map(|size| size.to_string()) != Some(meant.clone()) {
                    return Err(Reply::text(
                        409,
                        format!(
                            "{} is no longer the {meant} bytes it was: it has been replaced",
                            decoded(key)
                        ),
                    ));
                }
            }
            return Ok(reply);
        }
        Err(Reply::text(404, "no such route"))
    }

    /// The store's objects: `store/live/<key>` for what changes,
    /// `store/b8/<n>/<key>` for blocks of what does not.
    async fn store_route(&self, rest: &str, asked: &Asked<'_>) -> Result<Reply, Reply> {
        // Where the store is, for a reader that goes to it itself. Asked
        // before anything else of the store, and answered whether or not
        // this API holds the store too.
        if rest == "at" {
            return match &self.store_at {
                Some(at) => Ok(Reply {
                    cache_control: BRIEF,
                    ..Reply::json(at)
                }),
                None => Err(Reply {
                    cache_control: BRIEF,
                    ..Reply::text(404, "the store is behind this API")
                }),
            };
        }
        let Some(store) = &self.store else {
            return Err(Reply::text(404, "no tile store configured"));
        };
        if let Some(key) = rest.strip_prefix("live/") {
            let key = decoded(key);
            if !is_live(&key) {
                return Err(Reply::text(400, format!("not a live object: {key}")));
            }
            // The condition goes to the bucket, with its own validator:
            // an object the client still holds is not read to find that out.
            let known = asked.if_none_match.and_then(|theirs| tags(theirs).next());
            let read = store.live.read_if_changed(&key, known).await.map_err(|e| {
                match e {
                    // Nothing there is an answer, and kept as long as one:
                    // what a store does not have is asked for by every film.
                    RepoError::NotFound(_) => Reply {
                        cache_control: BRIEF,
                        ..Reply::of(e)
                    },
                    e => Reply::of(e),
                }
            })?;
            let live = |status, etag: Option<String>, body: Vec<u8>| Reply {
                status,
                content_type: "application/json".into(),
                content_range: None,
                ranged: false,
                etag,
                object_size: None,
                cache_control: BRIEF,
                body,
                later: None,
            };
            return match read {
                Read::Unchanged => Ok(live(304, known.map(str::to_string), Vec::new())),
                Read::Changed { bytes, .. } if bytes.len() as u64 > LIVE_LIMIT => {
                    Err(Reply::text(400, format!("{key} is {} bytes", bytes.len())))
                }
                Read::Changed { bytes, etag } => Ok(Reply {
                    object_size: Some(bytes.len() as u64),
                    ..live(200, etag, bytes)
                }),
            };
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
            return block(&store.archives, &decoded(key), index).await;
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

/// One object of a `POST store/live`, as `GET store/live/<key>` answers it.
async fn live_answer(store: &StoreObjects, ask: &LiveAsk, spent: &AtomicU64) -> LiveAnswer {
    let key = ask.key.as_str();
    if !is_live(key) {
        return LiveAnswer::refused(key, 400, format!("not a live object: {key}"));
    }
    if spent.load(Ordering::Relaxed) >= LIVE_REPLY {
        return LiveAnswer::refused(key, 413, "the reply is full: ask for it alone");
    }
    // The condition goes to the bucket, with its own validator, as the GET
    // sends it: an object still held is not read to find that out.
    let known = ask.etag.as_deref().and_then(|theirs| tags(theirs).next());
    match store.live.read_if_changed(key, known).await {
        Ok(Read::Unchanged) => LiveAnswer {
            key: key.to_string(),
            status: 304,
            etag: known.map(str::to_string),
            ..LiveAnswer::default()
        },
        Ok(Read::Changed { bytes, .. }) if bytes.len() as u64 > LIVE_LIMIT => {
            LiveAnswer::refused(key, 400, format!("{key} is {} bytes", bytes.len()))
        }
        Ok(Read::Changed { bytes, etag }) => {
            spent.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            match String::from_utf8(bytes) {
                Ok(body) => LiveAnswer {
                    key: key.to_string(),
                    status: 200,
                    etag,
                    body: Some(body),
                    error: None,
                },
                Err(_) => LiveAnswer::refused(key, 422, format!("{key} is not text")),
            }
        }
        Err(e) => {
            let status = match e {
                RepoError::NotFound(_) => 404,
                RepoError::Malformed { .. } => 422,
                RepoError::Store(_) => 502,
            };
            LiveAnswer::refused(key, status, e)
        }
    }
}

/// The validator of block `index` of an object: the store's validator for
/// the object where it gives one, the key and the size where it does not.
fn block_tag(key: &str, size: u64, tag: Option<&str>, index: u64) -> String {
    match tag {
        Some(tag) => {
            let tag = tag.trim_start_matches("W/").trim_matches('"');
            format!("\"{tag}-{size:x}-{index}\"")
        }
        None => format!("{}-{index}\"", etag(key, size).trim_end_matches('"')),
    }
}

/// Block `index` of an object: bytes `index × BLOCK` up to the next
/// block, or the object's end. Whole, and the same for whoever asks.
///
/// Its validator is the object's own, from the store that holds it, and the
/// block's number: an object written again under its key is another object,
/// and every block of it another block, even where the size did not move.
/// Kept a moment and then asked about — never for good.
async fn block(objects: &Arc<dyn Objects>, key: &str, index: u64) -> Result<Reply, Reply> {
    if !safe(key) {
        return Err(Reply::text(400, format!("not a key: {key}")));
    }
    let (size, tag) = objects.stat(key).await.map_err(Reply::of)?;
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
        etag: Some(block_tag(key, size, tag.as_deref(), index)),
        object_size: Some(size),
        cache_control: BRIEF,
        body: Vec::new(),
        later: Some(Later {
            objects: objects.clone(),
            key: key.to_string(),
            ranges,
        }),
    })
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
    fn a_block_carries_the_validator_of_the_object_it_is_of() {
        use crate::{Entry, Listing, Objects, RepoError};
        use std::sync::{Arc, Mutex};

        /// An object of a fixed size whose store says which writing of it
        /// this is, or says nothing.
        struct Written(Mutex<Option<String>>);
        #[async_trait::async_trait]
        impl Objects for Written {
            fn label(&self) -> String {
                "written".into()
            }
            async fn list(&self, _: &str) -> Result<Vec<Entry>, RepoError> {
                Ok(Vec::new())
            }
            async fn browse(&self, _: &str) -> Result<Listing, RepoError> {
                Ok(Listing::default())
            }
            async fn size(&self, _: &str) -> Result<u64, RepoError> {
                Ok(100)
            }
            async fn stat(&self, _: &str) -> Result<(u64, Option<String>), RepoError> {
                Ok((100, self.0.lock().expect("lock").clone()))
            }
            async fn read(&self, _: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
                Ok(vec![7; (range.end - range.start) as usize])
            }
        }
        let written = Arc::new(Written(Mutex::new(Some("\"first\"".into()))));
        let objects: Arc<dyn Objects> = written.clone();
        let bench = Bench {
            projects: vec![Project {
                name: "p".into(),
                films: Arc::new(crate::ScenePacks::new(objects.clone(), ["packs"])),
                objects,
            }],
            tiles: None,
            store: None,
            store_at: None,
        };
        let path = format!("/api/p/p/{}/0/zone/a.pmtiles", block_segment());
        let get = |held: Option<&str>| {
            let asked = Asked {
                range: None,
                if_none_match: held,
            };
            futures_executor::block_on(bench.get(&path, "", asked)).expect("reply")
        };
        let first = get(None);
        let tag = first.etag.clone().expect("a validator");
        assert_eq!(first.status, 200);
        // Kept a moment, then asked about: never for good.
        assert!(
            !first.cache_control.contains("immutable"),
            "{}",
            first.cache_control
        );
        assert!(
            first.cache_control.contains("max-age="),
            "{}",
            first.cache_control
        );
        // Whoever holds it is told so, and sent nothing.
        let held = get(Some(&tag));
        assert_eq!((held.status, held.later.is_none()), (304, true));
        // Written again under its key, to the same size: another block.
        *written.0.lock().expect("lock") = Some("W/\"second\"".into());
        let again = get(Some(&tag));
        assert_eq!(again.status, 200);
        let second = again.etag.expect("a validator");
        assert_ne!(second, tag);
        assert!(
            second.starts_with('"') && second.ends_with("-0\""),
            "{second}"
        );
        assert_eq!(get(Some(&second)).status, 304);
        // A store with no validator to give: the key and the size still do.
        *written.0.lock().expect("lock") = None;
        let plain = get(None).etag.expect("a validator");
        assert!(plain != tag && plain != second);
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
            store: None,
            store_at: None,
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

    /// A store's small objects, each with the validator of its writing —
    /// and one the bucket cannot be asked for. It notes every key read.
    struct Small {
        objects: std::collections::HashMap<String, (String, Vec<u8>)>,
        read: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::Objects for Small {
        fn label(&self) -> String {
            "small".into()
        }
        async fn list(&self, _: &str) -> Result<Vec<crate::Entry>, RepoError> {
            Ok(Vec::new())
        }
        async fn browse(&self, _: &str) -> Result<crate::Listing, RepoError> {
            Ok(crate::Listing::default())
        }
        async fn size(&self, key: &str) -> Result<u64, RepoError> {
            Err(RepoError::NotFound(key.to_string()))
        }
        async fn read(&self, key: &str, _: Range<u64>) -> Result<Vec<u8>, RepoError> {
            Err(RepoError::NotFound(key.to_string()))
        }
        async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
            self.read.lock().expect("lock").push(key.to_string());
            if key == "broken/manifest.json" {
                return Err(RepoError::Store("the bucket did not answer".into()));
            }
            let (etag, bytes) = self
                .objects
                .get(key)
                .ok_or_else(|| RepoError::NotFound(key.to_string()))?;
            if known == Some(etag.as_str()) {
                return Ok(Read::Unchanged);
            }
            Ok(Read::Changed {
                bytes: bytes.clone(),
                etag: Some(etag.clone()),
            })
        }
    }

    fn small(objects: &[(&str, &str, Vec<u8>)]) -> (Arc<Small>, Bench) {
        let small = Arc::new(Small {
            objects: objects
                .iter()
                .map(|(key, etag, body)| (key.to_string(), (etag.to_string(), body.clone())))
                .collect(),
            read: Default::default(),
        });
        let bench = Bench {
            projects: Vec::new(),
            tiles: None,
            store: Some(StoreObjects {
                live: small.clone(),
                archives: small.clone(),
            }),
            store_at: None,
        };
        (small, bench)
    }

    fn ask(key: &str, etag: Option<&str>) -> LiveAsk {
        LiveAsk {
            key: key.into(),
            etag: etag.map(str::to_string),
        }
    }

    fn post(bench: &Bench, objects: Vec<LiveAsk>) -> Reply {
        let body = serde_json::to_vec(&LiveAsked { objects }).expect("json");
        futures_executor::block_on(bench.post("/api/store/live", "k=secret", &body)).expect("reply")
    }

    #[test]
    fn many_small_objects_are_answered_in_one_reply_each_as_it_would_be_alone() {
        let (small, bench) = small(&[
            ("catalog.json", "\"c1\"", b"{\"layers\":[]}".to_vec()),
            (
                "a/manifest.json",
                "\"a1\"",
                "{\"archives\":[\"\u{e9}\"]}".into(),
            ),
            ("b/manifest.json", "\"b1\"", b"{}".to_vec()),
            ("a/archive.pmtiles", "\"p\"", vec![0; 8]),
            ("not/text.json", "\"t\"", vec![0xff, 0xfe]),
        ]);
        let reply = post(
            &bench,
            vec![
                ask("catalog.json", None),
                // Held, as an intermediary may have weakened it: unchanged.
                ask("a/manifest.json", Some("W/\"a1\"")),
                // Held under another writing's validator: sent again.
                ask("b/manifest.json", Some("\"b0\"")),
                ask("c/manifest.json", None),
                // Not what this route serves: an archive, a way out.
                ask("a/archive.pmtiles", None),
                ask("../secrets.json", None),
                ask("/catalog.json", None),
                // What fails is its own answer and nobody else's.
                ask("broken/manifest.json", None),
                ask("not/text.json", None),
                ask("a/manifest.json", None),
            ],
        );
        assert_eq!(
            (reply.status, reply.content_type.as_str()),
            (200, "application/json")
        );
        // The answer to one asker's question: kept by nobody on the way.
        assert_eq!(reply.cache_control, "no-store");
        assert_eq!(reply.etag, None);
        let answers: LiveAnswers = serde_json::from_slice(&reply.body).expect("answers");
        let told: Vec<(&str, u16, Option<&str>, Option<&str>)> = answers
            .objects
            .iter()
            .map(|a| {
                (
                    a.key.as_str(),
                    a.status,
                    a.etag.as_deref(),
                    a.body.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            told,
            [
                ("catalog.json", 200, Some("\"c1\""), Some("{\"layers\":[]}")),
                ("a/manifest.json", 304, Some("\"a1\""), None),
                ("b/manifest.json", 200, Some("\"b1\""), Some("{}")),
                ("c/manifest.json", 404, None, None),
                ("a/archive.pmtiles", 400, None, None),
                ("../secrets.json", 400, None, None),
                ("/catalog.json", 400, None, None),
                ("broken/manifest.json", 502, None, None),
                ("not/text.json", 422, None, None),
                // Byte for byte what the store holds.
                (
                    "a/manifest.json",
                    200,
                    Some("\"a1\""),
                    Some("{\"archives\":[\"\u{e9}\"]}")
                ),
            ]
        );
        assert!(answers
            .objects
            .iter()
            .all(|a| a.error.is_some() == !matches!(a.status, 200 | 304)));
        // What was refused never reached the bucket.
        let read = small.read.lock().expect("lock").clone();
        assert!(
            !read
                .iter()
                .any(|k| k.contains("secrets") || k.ends_with(".pmtiles") || k.starts_with('/')),
            "{read:?}"
        );
        // Each is what the GET answers for it alone.
        for answer in &answers.objects {
            let held = match answer.status {
                304 => Some("\"a1\""),
                _ => None,
            };
            let alone = futures_executor::block_on(bench.get(
                &format!("/api/store/live/{}", answer.key),
                "",
                Asked {
                    range: None,
                    if_none_match: held,
                },
            ))
            .expect("reply");
            // Text or not, the GET sends what is there: only the many say 422.
            if answer.status != 422 {
                assert_eq!(alone.status, answer.status, "{}", answer.key);
            }
            if answer.status == 200 {
                assert_eq!(
                    Some(alone.body),
                    answer.body.clone().map(String::into_bytes)
                );
                assert_eq!(alone.etag, answer.etag);
            }
        }
    }

    #[test]
    fn a_post_asks_for_no_more_than_its_bounds_and_only_of_the_one_route() {
        // As large as the route serves one, and a reply holds four.
        let big = vec![b' '; LIVE_LIMIT as usize];
        assert_eq!(LIVE_REPLY, 4 * LIVE_LIMIT);
        let (small, bench) = small(&[
            ("catalog.json", "\"c\"", b"{}".to_vec()),
            ("1/manifest.json", "\"1\"", big.clone()),
            ("2/manifest.json", "\"2\"", big.clone()),
            ("3/manifest.json", "\"3\"", big.clone()),
            ("4/manifest.json", "\"4\"", big),
            (
                "huge/manifest.json",
                "\"h\"",
                vec![b' '; LIVE_LIMIT as usize + 1],
            ),
        ]);
        // As many as allowed are answered; one more and none is read.
        let many = |n: usize| {
            (0..n)
                .map(|_| ask("catalog.json", None))
                .collect::<Vec<_>>()
        };
        let full = post(&bench, many(LIVE_MANY));
        assert_eq!(full.status, 200);
        let answers: LiveAnswers = serde_json::from_slice(&full.body).expect("answers");
        assert_eq!(answers.objects.len(), LIVE_MANY);
        small.read.lock().expect("lock").clear();
        let too_many = post(&bench, many(LIVE_MANY + 1));
        assert_eq!(too_many.status, 400);
        assert!(String::from_utf8_lossy(&too_many.body).contains("ask again"));
        assert!(small.read.lock().expect("lock").is_empty());

        // A reply that is full says so of what it left out, and leaves it
        // unread: the first reads' turn is over before the last one's.
        let mut asked: Vec<LiveAsk> = many(LIVE_AT_ONCE - 4);
        asked.extend(["1", "2", "3", "4"].map(|n| ask(&format!("{n}/manifest.json"), None)));
        asked.push(ask("huge/manifest.json", None));
        asked.push(ask("catalog.json", None));
        small.read.lock().expect("lock").clear();
        let reply = post(&bench, asked);
        let answers: LiveAnswers = serde_json::from_slice(&reply.body).expect("answers");
        let statuses: Vec<u16> = answers.objects[LIVE_AT_ONCE - 4..]
            .iter()
            .map(|a| a.status)
            .collect();
        assert_eq!(statuses, [200, 200, 200, 200, 413, 413]);
        assert!(!small
            .read
            .lock()
            .expect("lock")
            .iter()
            .any(|k| k.starts_with("huge")));
        // Alone, and too large for this route at all: refused as the GET does.
        let huge = post(&bench, vec![ask("huge/manifest.json", None)]);
        let answers: LiveAnswers = serde_json::from_slice(&huge.body).expect("answers");
        assert_eq!(answers.objects[0].status, 400);

        // Not a list of objects; not the route; not this API; no store.
        let post_to = |bench: &Bench, path: &str, body: &[u8]| {
            futures_executor::block_on(bench.post(path, "", body)).map(|r| r.status)
        };
        assert_eq!(
            post_to(&bench, "/api/store/live", b"keys=catalog.json"),
            Some(400)
        );
        let one = serde_json::to_vec(&LiveAsked { objects: many(1) }).expect("json");
        assert_eq!(post_to(&bench, "/api/store/live", &one), Some(200));
        assert_eq!(
            post_to(&bench, "/api/store/live/catalog.json", &one),
            Some(405)
        );
        assert_eq!(post_to(&bench, "/api/projects", &one), Some(405));
        assert_eq!(post_to(&bench, "/index.html", &one), None);
        let none = Bench {
            projects: Vec::new(),
            tiles: None,
            store: None,
            store_at: None,
        };
        assert_eq!(post_to(&none, "/api/store/live", &one), Some(404));
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
