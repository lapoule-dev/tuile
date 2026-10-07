// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::films::{in_order, line_of, name_of, parent_of, safe, Outlines};
use crate::{Chunk, Film, FilmRepository, FilmSummary, Objects, RepoError, Unreadable};

/// The engine's layout: packs keyed by the scene they are the bake of.
///
/// ```text
/// <root>/<scene>/<first>-<last>.tuilepack        a bake of that frame range
/// <root>/<scene>/<first>-<last>.tuilepack.scene  its scene digest, one line
/// ```
///
/// `<root>/<scene>` is what the launcher's `pack_key` builds — `packs/` and a
/// hash of the trajectory, viewport and settings — and a film is one such
/// directory: every range baked of the same scene.
pub struct ScenePacks {
    objects: Arc<dyn Objects>,
    roots: Vec<String>,
    outlines: Outlines,
}

const PACK: &str = ".tuilepack";

/// `<first>-<last>.tuilepack` → the range.
fn range_of(name: &str) -> Option<(u32, u32)> {
    let (a, b) = name.strip_suffix(PACK)?.split_once('-')?;
    let (a, b) = (a.parse().ok()?, b.parse().ok()?);
    (a <= b).then_some((a, b))
}

impl ScenePacks {
    /// `roots` are the prefixes scenes are kept under, e.g. `packs`.
    pub fn new(
        objects: Arc<dyn Objects>,
        roots: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            objects,
            roots: roots
                .into_iter()
                .map(|r| r.into().trim_matches('/').to_string())
                .collect(),
            outlines: Outlines::default(),
        }
    }

    /// Remembers in `keeper` what each pack was found to hold, so a film is
    /// listed without asking its packs again.
    pub fn remembering(mut self, keeper: Arc<dyn tuile_core::storage::ContentStore>) -> Self {
        self.outlines = Outlines(Some(keeper));
        self
    }

    fn holds(&self, id: &str) -> bool {
        safe(id) && self.roots.iter().any(|r| parent_of(id) == r)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl FilmRepository for ScenePacks {
    fn layout(&self) -> &'static str {
        "scene-packs"
    }

    async fn films(&self) -> Result<Vec<FilmSummary>, RepoError> {
        let mut found: BTreeMap<String, (usize, u64)> = BTreeMap::new();
        for root in &self.roots {
            for entry in self.objects.list(root).await? {
                let dir = parent_of(&entry.key);
                if range_of(name_of(&entry.key)).is_some() && parent_of(dir) == root {
                    let film = found.entry(dir.to_string()).or_default();
                    film.0 += 1;
                    film.1 += entry.size;
                }
            }
        }
        Ok(found
            .into_iter()
            .map(|(id, (packs, bytes))| FilmSummary {
                id,
                layout: self.layout(),
                packs,
                bytes,
            })
            .collect())
    }

    async fn film(&self, id: &str) -> Result<Film, RepoError> {
        let id = id.trim_matches('/');
        if !self.holds(id) {
            return Err(RepoError::NotFound(id.to_string()));
        }
        let listing = self.objects.browse(id).await?;
        let has = |key: &str| listing.files.iter().find(|f| f.key == key);
        let mut chunks = Vec::new();
        let mut unreadable = Vec::new();
        let mut others = Vec::new();
        for file in &listing.files {
            let name = name_of(&file.key);
            if let Some((first, last)) = range_of(name) {
                // The name is the launcher's claim; the table is the pack's.
                let outline = match self.outlines.of(self.objects.as_ref(), file).await? {
                    Ok(found) if (found.first, found.last) == (first, last) => Ok(found),
                    Ok(found) => Err(format!(
                        "named {first}–{last}, holds {}–{}",
                        found.first, found.last
                    )),
                    Err(why) => Err(why),
                };
                let outline = match outline {
                    Ok(outline) => outline,
                    Err(why) => {
                        unreadable.push(Unreadable {
                            key: file.key.clone(),
                            bytes: file.size,
                            why,
                        });
                        continue;
                    }
                };
                let scene_key = format!("{}.scene", file.key);
                let scene = match has(&scene_key) {
                    Some(_) => line_of(self.objects.as_ref(), &scene_key).await?,
                    None => None,
                };
                chunks.push(Chunk::of(file, outline, scene));
            } else if !name.ends_with(".tuilepack.scene") {
                others.push(file.clone());
            }
        }
        if chunks.is_empty() && unreadable.is_empty() {
            return Err(RepoError::NotFound(id.to_string()));
        }
        // Two bakes of one scene may cover the same frames (a short probe and
        // the full film): they are alternatives, not a sequence. The longest
        // run of non-overlapping ranges, widest first, is the film; the rest
        // stay listed.
        chunks.sort_by_key(|p| (std::cmp::Reverse(p.last - p.first), p.first));
        let mut kept: Vec<Chunk> = Vec::new();
        for chunk in chunks {
            if kept
                .iter()
                .all(|k| chunk.last < k.first || chunk.first > k.last)
            {
                kept.push(chunk);
            } else {
                others.push(crate::Entry {
                    key: chunk.key,
                    size: chunk.bytes,
                });
            }
        }
        Ok(Film {
            id: id.to_string(),
            layout: self.layout(),
            chunks: in_order(id, kept)?,
            unreadable,
            others,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pack_name_gives_its_range() {
        assert_eq!(range_of("1-2880.tuilepack"), Some((1, 2880)));
        assert_eq!(range_of("5-4.tuilepack"), None);
        assert_eq!(range_of("scene.tuilepack"), None);
        assert_eq!(range_of("1-2880.tuilepack.mcap"), None);
    }
}
