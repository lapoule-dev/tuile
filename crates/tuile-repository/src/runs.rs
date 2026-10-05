// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::try_join_all;

use crate::films::{frames_of, in_order, line_of, safe};
use crate::{Chunk, Entry, Film, FilmRepository, FilmSummary, Objects, RepoError};

/// How an orchestrator lays a film's packs out in its run directory.
///
/// The engine cuts nothing itself: a job is handed a frame range — "the chunk
/// an orchestrator cut" — and a pack key, and what the orchestrator calls its
/// chunks' packs is its own business. So none of those names is written in
/// this crate: they are data, read from the configuration of whoever points
/// a repository at a bucket.
///
/// Keys are relative to the run directory. A template holds `{index}` or
/// `{index:0N}` (zero-padded to N digits) where a chunk's number goes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLayout {
    /// How many path segments name a run: 2 for `<a>/<b>/…`.
    pub depth: usize,
    /// A chunk's pack, e.g. `chunks/{index:04}.tuilepack`.
    #[serde(default)]
    pub chunk_pack: Option<String>,
    /// The object written last for a chunk: its presence is what makes the
    /// pack a chunk rather than an upload in progress. Its one line, when it
    /// has one, is the chunk's scene digest. Without it, a pack that is
    /// there is taken to be whole.
    #[serde(default)]
    pub chunk_ready: Option<String>,
    /// A pack holding the whole film, for a run that was not cut.
    #[serde(default)]
    pub whole_pack: Option<String>,
    /// Its marker, as `chunk_ready` is a chunk's.
    #[serde(default)]
    pub whole_ready: Option<String>,
}

/// One `{index}` or `{index:0N}` placeholder, between a prefix and a suffix.
struct Template<'a> {
    prefix: &'a str,
    width: usize,
    suffix: &'a str,
}

impl<'a> Template<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let open = text.find("{index")?;
        let close = open + text[open..].find('}')?;
        let width = match &text[open + "{index".len()..close] {
            "" => 0,
            spec => spec.strip_prefix(":0")?.parse().ok()?,
        };
        Some(Self {
            prefix: &text[..open],
            width,
            suffix: &text[close + 1..],
        })
    }

    fn key(&self, index: usize) -> String {
        format!(
            "{}{index:0width$}{}",
            self.prefix,
            self.suffix,
            width = self.width
        )
    }

    /// The index a key was made with, if it was made with this template.
    fn index(&self, key: &str) -> Option<usize> {
        let digits = key.strip_prefix(self.prefix)?.strip_suffix(self.suffix)?;
        let fits = !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && (self.width == 0 || digits.len() == self.width);
        fits.then(|| digits.parse().ok()).flatten()
    }
}

/// Films kept as run directories, read by a configured [`RunLayout`].
///
/// A run is a film cut into chunks — each its own pack, marked ready once it
/// is whole — or a film in one pack, or both, in which case its chunks are
/// the film and the whole pack stays listed with the rest. A chunk's frame
/// range is read from its pack's own table.
pub struct RunFilms {
    objects: Arc<dyn Objects>,
    layout: RunLayout,
}

/// The first `depth` segments of a key that has more than that.
fn run_of(key: &str, depth: usize) -> Option<&str> {
    let (at, _) = key.match_indices('/').nth(depth.checked_sub(1)?)?;
    Some(&key[..at])
}

impl RunFilms {
    /// Fails on a layout that cannot find a pack.
    pub fn new(objects: Arc<dyn Objects>, layout: RunLayout) -> Result<Self, RepoError> {
        let bad = |what: String| RepoError::Malformed {
            key: "run layout".into(),
            what,
        };
        if layout.depth == 0 {
            return Err(bad("depth must be at least 1".into()));
        }
        for template in [&layout.chunk_pack, &layout.chunk_ready]
            .into_iter()
            .flatten()
        {
            if Template::parse(template).is_none() {
                return Err(bad(format!("`{template}` has no {{index}} placeholder")));
            }
        }
        if layout.chunk_pack.is_none() && layout.whole_pack.is_none() {
            return Err(bad(
                "neither chunk_pack nor whole_pack: no pack to find".into()
            ));
        }
        Ok(Self { objects, layout })
    }

    fn template(text: &Option<String>) -> Option<Template<'_>> {
        text.as_deref().and_then(Template::parse)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl FilmRepository for RunFilms {
    fn layout(&self) -> &'static str {
        "runs"
    }

    async fn films(&self) -> Result<Vec<FilmSummary>, RepoError> {
        let entries = self.objects.list("").await?;
        let present: std::collections::HashSet<&str> =
            entries.iter().map(|e| e.key.as_str()).collect();
        let pack = Self::template(&self.layout.chunk_pack);
        let marker = Self::template(&self.layout.chunk_ready);
        // Per run: its ready chunks, and its whole pack.
        let mut runs: BTreeMap<&str, ((usize, u64), Option<u64>)> = BTreeMap::new();
        for entry in &entries {
            let Some(run) = run_of(&entry.key, self.layout.depth) else {
                continue;
            };
            let relative = &entry.key[run.len() + 1..];
            if self.layout.whole_pack.as_deref() == Some(relative) {
                runs.entry(run).or_default().1 = Some(entry.size);
            } else if let Some(i) = pack.as_ref().and_then(|t| t.index(relative)) {
                // Only a chunk `film` would give: a pack without its marker
                // is an upload in progress, and a run of nothing else is not
                // a film yet.
                let ready = marker
                    .as_ref()
                    .is_none_or(|m| present.contains(format!("{run}/{}", m.key(i)).as_str()));
                if ready {
                    let chunks = &mut runs.entry(run).or_default().0;
                    chunks.0 += 1;
                    chunks.1 += entry.size;
                }
            }
        }
        Ok(runs
            .into_iter()
            .filter_map(|(id, (chunks, whole))| {
                let (packs, bytes) = match (chunks, whole) {
                    ((0, _), Some(bytes)) => (1, bytes),
                    ((0, _), None) => return None,
                    (chunks, _) => chunks,
                };
                Some(FilmSummary {
                    id: id.to_string(),
                    layout: FilmRepository::layout(self),
                    packs,
                    bytes,
                })
            })
            .collect())
    }

    async fn film(&self, id: &str) -> Result<Film, RepoError> {
        let id = id.trim_matches('/');
        if !safe(id) || run_of(&format!("{id}/"), self.layout.depth) != Some(id) {
            return Err(RepoError::NotFound(id.to_string()));
        }
        let within = format!("{id}/");
        let files = self.objects.list(id).await?;
        let by_name: BTreeMap<&str, &Entry> = files
            .iter()
            .filter_map(|e| Some((e.key.strip_prefix(&within)?, e)))
            .collect();
        let objects = self.objects.as_ref();
        let layout = &self.layout;

        // Chunks whose pack is in and marked ready, by index.
        let pack = Self::template(&layout.chunk_pack);
        let marker = Self::template(&layout.chunk_ready);
        let ready: Vec<(usize, &Entry)> = by_name
            .iter()
            .filter_map(|(name, entry)| Some((pack.as_ref()?.index(name)?, *entry)))
            .filter(|(i, _)| {
                marker
                    .as_ref()
                    .is_none_or(|m| by_name.contains_key(m.key(*i).as_str()))
            })
            .collect();

        let mut used: Vec<String> = Vec::new();
        let mut chunks = Vec::new();
        if let (false, Some(pack)) = (ready.is_empty(), pack.as_ref()) {
            // Read side by side: a film has dozens of chunks.
            let ranges =
                try_join_all(ready.iter().map(|(_, e)| frames_of(objects, &e.key))).await?;
            for ((i, entry), (first, last)) in ready.iter().zip(ranges) {
                let marker_key = marker.as_ref().map(|m| m.key(*i));
                chunks.push(Chunk {
                    key: entry.key.clone(),
                    first,
                    last,
                    bytes: entry.size,
                    scene: match &marker_key {
                        Some(m) => line_of(objects, &format!("{id}/{m}")).await?,
                        None => None,
                    },
                });
                used.push(pack.key(*i));
                used.extend(marker_key);
            }
        } else if let Some(entry) = layout.whole_pack.as_deref().and_then(|w| by_name.get(w)) {
            let (first, last) = frames_of(objects, &entry.key).await?;
            let marker = layout
                .whole_ready
                .as_deref()
                .filter(|m| by_name.contains_key(m));
            chunks.push(Chunk {
                key: entry.key.clone(),
                first,
                last,
                bytes: entry.size,
                scene: match marker {
                    Some(m) => line_of(objects, &format!("{id}/{m}")).await?,
                    None => None,
                },
            });
            used.extend(layout.whole_pack.clone());
            used.extend(marker.map(str::to_string));
        } else {
            return Err(RepoError::NotFound(id.to_string()));
        }

        Ok(Film {
            id: id.to_string(),
            layout: FilmRepository::layout(self),
            chunks: in_order(id, chunks)?,
            others: by_name
                .iter()
                .filter(|(name, _)| !used.iter().any(|u| u == *name))
                .map(|(_, e)| (*e).clone())
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_gives_its_run() {
        assert_eq!(run_of("a/b/whole.tuilepack", 2), Some("a/b"));
        assert_eq!(run_of("a/b/chunks/0000.tuilepack", 2), Some("a/b"));
        assert_eq!(run_of("stray.tar.gz", 2), None);
        assert_eq!(run_of("a/stray", 2), None);
        assert_eq!(run_of("a/film.tuilepack", 1), Some("a"));
    }

    #[test]
    fn a_template_makes_keys_and_reads_them_back() {
        let t = Template::parse("chunks/c{index:04}.tuilepack").expect("template");
        assert_eq!(t.key(12), "chunks/c0012.tuilepack");
        assert_eq!(t.index("chunks/c0012.tuilepack"), Some(12));
        assert_eq!(
            t.index("chunks/c12.tuilepack"),
            None,
            "not padded as the writer pads"
        );
        assert_eq!(t.index("chunks/c0012.txt"), None);
        assert_eq!(t.index("whole.tuilepack"), None);

        let bare = Template::parse("part-{index}.tuilepack").expect("template");
        assert_eq!(bare.key(7), "part-7.tuilepack");
        assert_eq!(bare.index("part-123.tuilepack"), Some(123));
        assert_eq!(bare.index("part-.tuilepack"), None);

        assert!(Template::parse("no-placeholder.tuilepack").is_none());
        assert!(
            Template::parse("c{index:4}.tuilepack").is_none(),
            "only zero padding"
        );
    }
}
