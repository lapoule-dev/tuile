// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use async_trait::async_trait;
use tuile_pack::{blob_start, Pack, PREAMBLE};

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Film {
    pub id: String,
    pub layout: &'static str,
    /// The film's packs, in film order.
    pub chunks: Vec<Chunk>,
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
/// - A film has at least one chunk.
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

/// The frame range a pack's own table reports.
pub(crate) async fn frames_of(objects: &dyn Objects, key: &str) -> Result<(u32, u32), RepoError> {
    let malformed = |e: tuile_pack::PackError| RepoError::Malformed {
        key: key.to_string(),
        what: e.to_string(),
    };
    let preamble = objects.read(key, 0..PREAMBLE as u64).await?;
    let start = blob_start(&preamble).map_err(malformed)?;
    let head = objects.read(key, 0..start).await?;
    Ok(Pack::open_table(&head).map_err(malformed)?.frame_range())
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
