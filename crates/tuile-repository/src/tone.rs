// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's table of grades, from what a tile store keeps.
//!
//! A store keeps grades two ways, beside the imagery layer they are of: one
//! table a place (`tuile_radiometry::region_key`), fitted on that place's
//! own tiles, and one table for the layer as a whole (`<layer>/tone.json`),
//! for the places nobody has fitted yet. A film's table is made of the
//! tables of the places its imagery lies in, each counting for how much of
//! the film is in it — the place's own where the store has one, the
//! layer's where it has not.
//!
//! Read through [`Objects`], so the same lines serve a native render next
//! to the bucket and a page behind an API.

use std::collections::BTreeMap;

use tuile_radiometry::{region_key, LevelGrades};

use crate::{Objects, RepoError};

/// Places asked of the store at once: most have no table of their own, and
/// each is a request.
const AT_ONCE: usize = 16;

/// Where a layer's own table is kept: the grades of a place without one.
pub fn layer_tone_key(layer: &str) -> String {
    format!("{layer}/tone.json")
}

/// A film's table, and what it was made of.
#[derive(Debug, Clone, PartialEq)]
pub struct FilmTone {
    /// The table; `None` if the store keeps nothing for this film.
    pub table: Option<LevelGrades>,
    /// Places the film's imagery lies in.
    pub places: usize,
    /// Of those, the places the store keeps a table of their own for.
    pub fitted: usize,
    /// Whether the others were given the layer's table.
    pub layer_table: bool,
}

async fn table(live: &dyn Objects, key: String) -> Result<Option<LevelGrades>, RepoError> {
    match live.read_all(&key).await {
        Ok(bytes) => std::str::from_utf8(&bytes)
            .ok()
            .and_then(LevelGrades::from_json)
            .map(Some)
            .ok_or_else(|| RepoError::Store(format!("{key} is not a table of grades"))),
        // A store that has not fitted this place. A failure to ask is a
        // failure, not an absence: a render that took the one for the other
        // would draw ungraded and say nothing.
        Err(RepoError::NotFound(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

/// The table of a film whose imagery, of `layer`, lies in `places` — each
/// with how many of the film's imagery tiles are in it (a pack's
/// `imagery_regions`, summed over the film's packs).
pub async fn film_tone(
    live: &dyn Objects,
    layer: &str,
    places: &BTreeMap<(u32, u32), u32>,
) -> Result<FilmTone, RepoError> {
    let asked: Vec<(&(u32, u32), &u32)> = places.iter().collect();
    let mut own: Vec<(Option<LevelGrades>, f32)> = Vec::with_capacity(asked.len());
    for some in asked.chunks(AT_ONCE) {
        let read = futures_util::future::join_all(
            some.iter()
                .map(|((x, y), _)| table(live, region_key(layer, *x, *y))),
        )
        .await;
        for ((_, tiles), found) in some.iter().zip(read) {
            own.push((found?, **tiles as f32));
        }
    }
    let fitted = own.iter().filter(|(t, _)| t.is_some()).count();
    // The layer's table, only if some place needs it.
    let of_layer = if fitted < own.len() {
        table(live, layer_tone_key(layer)).await?
    } else {
        None
    };
    let parts: Vec<(&LevelGrades, f32)> = own
        .iter()
        .filter_map(|(t, weight)| t.as_ref().or(of_layer.as_ref()).map(|t| (t, *weight)))
        .collect();
    Ok(FilmTone {
        table: LevelGrades::merged(&parts),
        places: own.len(),
        fitted,
        layer_table: of_layer.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Listing};
    use std::ops::Range;
    use tuile_radiometry::Grade;

    /// A store of a few small files, one of which cannot be read.
    struct Files(BTreeMap<String, String>);

    #[async_trait::async_trait]
    impl Objects for Files {
        fn label(&self) -> String {
            "test".into()
        }
        async fn list(&self, _: &str) -> Result<Vec<Entry>, RepoError> {
            Ok(Vec::new())
        }
        async fn browse(&self, _: &str) -> Result<Listing, RepoError> {
            Ok(Listing::default())
        }
        async fn size(&self, key: &str) -> Result<u64, RepoError> {
            match self.0.get(key) {
                Some(_) if key.contains("broken") => Err(RepoError::Store("unreachable".into())),
                Some(text) => Ok(text.len() as u64),
                None => Err(RepoError::NotFound(key.to_string())),
            }
        }
        async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
            self.size(key).await?;
            Ok(self.0[key].as_bytes()[range.start as usize..range.end as usize].to_vec())
        }
    }

    fn of(stops: f32) -> String {
        LevelGrades {
            anchor: 10,
            grades: BTreeMap::from([
                (10, Grade::IDENTITY),
                (
                    13,
                    Grade {
                        gain: [stops.exp2(); 3],
                        ..Grade::IDENTITY
                    },
                ),
            ]),
            sources: Vec::new(),
        }
        .to_json()
    }

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        futures_executor::block_on(f)
    }

    #[test]
    fn a_place_without_a_table_is_given_the_layers() {
        let places = BTreeMap::from([((1, 1), 30), ((2, 1), 10)]);
        // The first place is fitted: one stop. The layer says three.
        let mut files = Files(BTreeMap::from([
            (region_key("imagery", 1, 1), of(1.0)),
            (layer_tone_key("imagery"), of(3.0)),
        ]));
        let tone = block(film_tone(&files, "imagery", &places)).expect("a tone");
        assert_eq!((tone.places, tone.fitted, tone.layer_table), (2, 1, true));
        // Thirty tiles at one stop and ten at three: a stop and a half.
        let gain = tone.table.expect("a table").of(13).gain[0].log2();
        assert!((gain - 1.5).abs() < 1e-3, "{gain}");

        // Without the layer's table, the fitted place alone speaks.
        files.0.remove(&layer_tone_key("imagery"));
        let tone = block(film_tone(&files, "imagery", &places)).expect("a tone");
        assert_eq!((tone.fitted, tone.layer_table), (1, false));
        assert!((tone.table.expect("a table").of(13).gain[0].log2() - 1.0).abs() < 1e-3);

        // Nothing kept at all: no table, and no error.
        files.0.clear();
        let tone = block(film_tone(&files, "imagery", &places)).expect("a tone");
        assert_eq!(tone.table, None);
    }

    #[test]
    fn a_store_that_cannot_be_asked_is_an_error_not_an_absence() {
        let places = BTreeMap::from([((7, 7), 1)]);
        let files = Files(BTreeMap::from([(region_key("broken", 7, 7), of(1.0))]));
        assert!(block(film_tone(&files, "broken", &places)).is_err());
        let files = Files(BTreeMap::from([(
            region_key("imagery", 7, 7),
            "not a table".into(),
        )]));
        assert!(block(film_tone(&files, "imagery", &places)).is_err());
    }
}
