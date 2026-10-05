// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The bench's API, as a function from a request to a reply.
//!
//! ```text
//! GET /api/projects                      the projects, their layout, the tile store
//! GET /api/p/<project>/films             the films the project's bucket holds
//! GET /api/p/<project>/films/<id>        one film: its packs (chunks), in order
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
//! Every route is a GET and nothing writes to a bucket.

use std::ops::Range;
use std::sync::Arc;

use percent_encoding::percent_decode_str;

use crate::{FilmRepository, Objects, RepoError, TileRepository};

/// How much an open-ended range (`bytes=N-`) is answered with. A media
/// element asks that way and then asks again from where the answer stopped.
pub const OPEN_RANGE: u64 = 8 << 20;
/// The largest object served without a range.
pub const WHOLE_LIMIT: u64 = 64 << 20;

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
    /// `Cache-Control`.
    pub cache_control: &'static str,
    pub body: Vec<u8>,
}

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
            cache_control: FRESH,
            body: message.to_string().into_bytes(),
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
                cache_control: FRESH,
                body,
            },
            Err(e) => Self::text(500, e),
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
            cache_control: IMMUTABLE,
            body,
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
                // A tile can be refetched and replaced in the store.
                cache_control: FRESH,
                body: tile.bytes,
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
    fn a_query_gives_its_parameter_decoded() {
        assert_eq!(
            param("prefix=packs%2Fabc&x=1", "prefix").as_deref(),
            Some("packs/abc")
        );
        assert_eq!(param("a=1", "prefix"), None);
        assert_eq!(param("", "prefix"), None);
    }
}
