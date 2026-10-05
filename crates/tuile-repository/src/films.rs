// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use async_trait::async_trait;
use tuile_pack::{blob_start, frame_range_by, PREAMBLE};

use crate::{Entry, Objects, RepoError};

/// One pack of a film: the chunk an orchestrator cut — a contiguous,
/// inclusive range of the film's frames, baked and rendered on its own. A
/// chunk is self-contained: its own tiles, its own imagery, nothing shared
/// with its neighbours. A film nobody cut is one chunk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Chunk {
    pub key: String,
    pub first: u32,
    pub last: u32,
    pub bytes: u64,
    /// The scene digest the bake named this chunk by, when it left one.
    pub scene: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FilmSummary {
    pub id: String,
    pub layout: &'static str,
    /// Packs found, baked or not yet marked ready.
    pub packs: usize,
    pub bytes: u64,
}

/// A pack that is there and cannot be read as one by this build: an older
/// format, a name that disagrees with its table, a damaged head.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Unreadable {
    pub key: String,
    pub bytes: u64,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Film {
    pub id: String,
    pub layout: &'static str,
    /// The film's packs, in film order.
    pub chunks: Vec<Chunk>,
    /// The packs that would be chunks and cannot be read.
    pub unreadable: Vec<Unreadable>,
    /// Everything else in the film's directory.
    pub others: Vec<Entry>,
}

/// The films a bucket holds, and what each is made of.
///
/// # Contract
///
/// Every implementation reads one key layout, and a caller never needs to
/// know which:
///
/// - `film(id)` succeeds for every `id` that `films()` returned, and fails
///   with [`RepoError::NotFound`] for an id naming no film.
/// - A film's `chunks` are sorted by `first`, never overlap, and each has
///   `first <= last`. Frame numbers are the film's own: a frame is in at most
///   one chunk.
/// - Every chunk is a pack that is there now, readable through the
///   [`Objects`] the repository was built on: `bytes` is its size, and its
///   table reports exactly `first..=last`. The range is always the pack's
///   own: nothing but a pack says which frames it holds.
/// - A pack that cannot be read is never a chunk and never an error: it is
///   in `unreadable`, with why. A bucket outlives the formats written to it,
///   and one old pack must not take a film's listing down. A film has at
///   least one pack, readable or not.
/// - A pack that is still being written is not a chunk.
/// - Nothing is written.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait FilmRepository: Send + Sync {
    /// The layout this repository reads, as a short name.
    fn layout(&self) -> &'static str;
    async fn films(&self) -> Result<Vec<FilmSummary>, RepoError>;
    async fn film(&self, id: &str) -> Result<Film, RepoError>;
}

/// A pack's frame range, or — when the pack is there but is not one this
/// build reads — why not. Only a failure to reach the bucket is an error.
pub(crate) async fn range_of_pack(
    objects: &dyn Objects,
    key: &str,
) -> Result<Result<(u32, u32), String>, RepoError> {
    match frames_of(objects, key).await {
        Ok(range) => Ok(Ok(range)),
        Err(RepoError::Malformed { what, .. }) => Ok(Err(what)),
        Err(e) => Err(e),
    }
}

/// The frame range a pack's own table reports.
async fn frames_of(objects: &dyn Objects, key: &str) -> Result<(u32, u32), RepoError> {
    let malformed = |e: tuile_pack::PackError| RepoError::Malformed {
        key: key.to_string(),
        what: e.to_string(),
    };
    // An object too short to hold what it claims is not a pack; asking the
    // store for bytes it does not have would be a store error instead.
    let size = objects.size(key).await?;
    if size < PREAMBLE as u64 {
        return Err(malformed(tuile_pack::PackError::NotAPack));
    }
    let preamble = objects.read(key, 0..PREAMBLE as u64).await?;
    let start = blob_start(&preamble).map_err(malformed)?;
    if start > size {
        return Err(malformed(tuile_pack::PackError::Truncated {
            what: "table",
        }));
    }
    // Not the table: it can be larger than the memory there is to read it
    // into, and the range is two numbers at a known place in it.
    frame_range_by(start, |range| objects.read(key, range))
        .await?
        .map_err(malformed)
}

/// A one-line object — a digest, a marker — trimmed.
pub(crate) async fn line_of(objects: &dyn Objects, key: &str) -> Result<Option<String>, RepoError> {
    match objects.read_all(key).await {
        Ok(bytes) => {
            Ok(Some(String::from_utf8_lossy(&bytes).trim().to_string()).filter(|s| !s.is_empty()))
        }
        Err(RepoError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A film id is a directory inside the bucket, never a way out of it.
pub(crate) fn safe(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('/')
        && id
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

pub(crate) fn name_of(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

pub(crate) fn parent_of(key: &str) -> &str {
    key.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Sorts chunks into film order and refuses a layout that broke the
/// contract: two packs claiming the same frame.
pub(crate) fn in_order(id: &str, mut chunks: Vec<Chunk>) -> Result<Vec<Chunk>, RepoError> {
    chunks.sort_by_key(|p| p.first);
    for pair in chunks.windows(2) {
        if pair[1].first <= pair[0].last {
            return Err(RepoError::Malformed {
                key: id.to_string(),
                what: format!(
                    "{} ({}–{}) and {} ({}–{}) overlap",
                    name_of(&pair[0].key),
                    pair[0].first,
                    pair[0].last,
                    name_of(&pair[1].key),
                    pair[1].first,
                    pair[1].last
                ),
            });
        }
    }
    Ok(chunks)
}
