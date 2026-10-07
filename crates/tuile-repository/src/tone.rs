// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's table of grades, from what a tile store keeps.
//!
//! Grades are an imagery layer's own: what brings one provider's levels to
//! one another says nothing of another provider's, and a layer is graded
//! only if its store says so. The switch is the layer's own table,
//! `<layer>/tone.json`: a layer without one is drawn as stored, whatever
//! else lies beside it.
//!
//! A graded layer keeps grades two ways: one table a place
//! (`tuile_radiometry::region_key`), fitted on that place's own tiles, and
//! the layer's table, for the places nobody has fitted yet. A film's table
//! is made of the tables of the places its imagery lies in, each counting
//! for how much of the film is in it — the place's own where the store has
//! one, the layer's where it has not.
//!
//! Read through [`Objects`], so the same lines serve a native render next
//! to the bucket and a page behind an API.

use std::collections::BTreeMap;

use tuile_radiometry::{
    region_key, Bounds, FieldBounds, FilmGrade, LevelGrades, LookTarget, Observed,
};

use crate::{Objects, RepoError};

/// Places asked of the store at once: most have no table of their own, and
/// each is a request.
const AT_ONCE: usize = 16;

/// Where a layer's own table is kept: the grades of a place without one,
/// and by being there, what says the layer is graded at all.
pub fn layer_tone_key(layer: &str) -> String {
    format!("{layer}/tone.json")
}

/// Where a pack's grade is kept: beside the pack, as its scene digest is.
pub fn pack_tone_key(pack: &str) -> String {
    format!("{pack}.tone.json")
}

/// Whether a layer is graded at all: whether its store keeps
/// `<layer>/tone.json`. Only that it is there is read, not what it says.
pub async fn layer_is_graded(live: &dyn Objects, layer: &str) -> Result<bool, RepoError> {
    match live.size(&layer_tone_key(layer)).await {
        Ok(_) => Ok(true),
        Err(RepoError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

/// Where what a pack's bake saw of its imagery is kept: what a film of
/// several packs is fitted on as one.
pub fn pack_seen_key(pack: &str) -> String {
    format!("{pack}.tone.seen")
}

/// The grade of a film made of these packs. A film of one pack has that
/// pack's own ([`pack_tone_key`]). A film of several is fitted anew on
/// what all of them saw ([`pack_seen_key`]), so that it is one grade from
/// its first frame to its last. `None` for a film none of whose packs has
/// a grade — it is then drawn as it is, with nothing of another film's.
///
/// A pack without a grade is not an error; a grade that cannot be read is,
/// and so is a store that cannot be asked: a render that took either for an
/// absence would draw ungraded and say nothing.
pub async fn film_grade(
    packs: &dyn Objects,
    keys: &[&str],
) -> Result<Option<FilmGrade>, RepoError> {
    async fn whole(packs: &dyn Objects, key: &str) -> Result<Option<Vec<u8>>, RepoError> {
        match packs.read_all(key).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(RepoError::NotFound(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }
    let not = |key: &str, what: &str| RepoError::Store(format!("{key} is not {what}"));
    let mut grades = Vec::new();
    for some in keys.chunks(AT_ONCE) {
        let read = futures_util::future::join_all(some.iter().map(|pack| async move {
            let key = pack_tone_key(pack);
            match whole(packs, &key).await? {
                Some(bytes) => std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(FilmGrade::from_json)
                    .map(|grade| Some((*pack, grade)))
                    .ok_or_else(|| not(&key, "a film's grade")),
                None => Ok(None),
            }
        }))
        .await;
        for grade in read {
            grades.extend(grade?);
        }
    }
    match grades.len() {
        0 => return Ok(None),
        1 => return Ok(grades.pop().map(|(_, grade)| grade)),
        _ => {}
    }
    // Packs fitted together carry the one grade of their film, each a
    // copy of it: it is the film's as it is.
    if grades.windows(2).all(|pair| pair[0].1 == pair[1].1) {
        return Ok(grades.pop().map(|(_, grade)| grade));
    }
    // Several: what each saw, then the film as one.
    let mut seen = Vec::new();
    for (pack, _) in &grades {
        let key = pack_seen_key(pack);
        let bytes = whole(packs, &key)
            .await?
            .ok_or_else(|| RepoError::NotFound(key.clone()))?;
        seen.push(Observed::from_bytes(&bytes).ok_or_else(|| not(&key, "what a bake saw"))?);
    }
    let parts: Vec<(&FilmGrade, &Observed)> = grades.iter().map(|(_, g)| g).zip(&seen).collect();
    Ok(FilmGrade::of_packs(
        &parts,
        &LookTarget::default(),
        &Bounds::default(),
        &FieldBounds::default(),
    ))
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
    /// Whether the layer is graded at all: whether its store keeps a table
    /// for it. Where it does not, nothing was asked of the places.
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
    // The layer's own table first: without it the layer is not graded,
    // and no place is asked anything.
    let Some(of_layer) = table(live, layer_tone_key(layer)).await? else {
        return Ok(FilmTone {
            table: None,
            places: places.len(),
            fitted: 0,
            layer_table: false,
        });
    };
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
    // A place nobody fitted is given the layer's gains and nothing more.
    // How dark one source is against another holds from place to place;
    // how flat and how dull it is does not — it is the ground's, and a
    // contrast and a saturation fitted on moorland burnt a coast out.
    let elsewhere = of_layer.gains_alone();
    let parts: Vec<(&LevelGrades, f32)> = own
        .iter()
        .map(|(t, weight)| (t.as_ref().unwrap_or(&elsewhere), *weight))
        .collect();
    Ok(FilmTone {
        table: LevelGrades::merged(&parts),
        places: own.len(),
        fitted,
        layer_table: true,
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
        with(stops, 1.2)
    }

    fn with(stops: f32, saturation: f32) -> String {
        LevelGrades {
            anchor: 10,
            grades: BTreeMap::from([
                (10, Grade::IDENTITY),
                (
                    13,
                    Grade {
                        gain: [stops.exp2(); 3],
                        saturation,
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
            (
                layer_tone_key("imagery"),
                of(3.0).replace("\"saturation\":1.2000", "\"saturation\":1.9000"),
            ),
        ]));
        let tone = block(film_tone(&files, "imagery", &places)).expect("a tone");
        assert_eq!((tone.places, tone.fitted, tone.layer_table), (2, 1, true));
        // Thirty tiles at one stop and ten at three: a stop and a half.
        let table = tone.table.expect("a table");
        let gain = table.of(13).gain[0].log2();
        assert!((gain - 1.5).abs() < 1e-3, "{gain}");
        // Of the layer's table the unfitted place takes the gain alone: its
        // saturation is another ground's. Three parts of 1.2, one of none.
        assert!(
            (table.of(13).saturation - 1.2f32.powf(0.75)).abs() < 2e-3,
            "{table:?}"
        );

        // Without the layer's table the layer is not graded at all, though
        // a place of it has a table: another imagery's film is left alone.
        files.0.remove(&layer_tone_key("imagery"));
        let tone = block(film_tone(&files, "imagery", &places)).expect("a tone");
        assert_eq!((tone.fitted, tone.layer_table), (0, false));
        assert_eq!(tone.table, None);
        // And a layer the store has never heard of, likewise.
        let tone = block(film_tone(&files, "another", &places)).expect("a tone");
        assert_eq!(tone.table, None);
    }

    /// A pack's imagery: columns `from..to` of a film four tiles high, the
    /// columns before `dark` a capture a stop darker, over a reference
    /// level that is one picture.
    fn seen_of(from: u32, to: u32, dark: u32) -> Observed {
        use tuile_radiometry::{TileSeen, GRID};
        let flat = |tone: f32, usage: f32| TileSeen {
            cells: [[tone; 3]; GRID * GRID],
            edges: [[[tone; 3]; GRID]; 4],
            usage,
            tones: None,
            paired: None,
        };
        let mut observed = Observed::default();
        for y in 6..10 {
            for x in from..to {
                let tone = if x < dark { 0.02 } else { 0.04 };
                observed.tiles.insert((13, x, y), flat(tone, 1.0));
                observed.tiles.insert((12, x / 2, y / 2), flat(0.04, 0.0));
            }
        }
        observed
    }

    fn graded(seen: &Observed) -> FilmGrade {
        FilmGrade::fit(
            seen,
            tuile_radiometry::Measure::Moments,
            1.0,
            &LookTarget::default(),
            &Bounds::default(),
            &FieldBounds::default(),
        )
    }

    /// The gain on light the grade gives the middle of a tile, in stops.
    fn lift(grade: &FilmGrade, x: u32) -> f32 {
        grade.field.at((13, x, 7), 0.5, 0.5).gain[1].log2()
    }

    #[test]
    fn a_films_grade_is_its_packs_own_and_nobody_elses() {
        let seen = seen_of(0, 12, 4);
        let grade = graded(&seen);
        assert!(grade.exposure_ev > 0.0 && lift(&grade, 1) > 0.9);
        let mut files = Files(BTreeMap::from([
            (pack_tone_key("film/1-10.tuilepack"), grade.to_json()),
            // Another film's grade lies in the same store.
            (pack_tone_key("other/1-10.tuilepack"), grade.to_json()),
        ]));
        // One pack of two has a grade: it is the film's, as it is.
        let keys = ["film/1-10.tuilepack", "film/11-20.tuilepack"];
        let found = block(film_grade(&files, &keys)).expect("asked");
        assert_eq!(
            found.as_ref(),
            FilmGrade::from_json(&grade.to_json()).as_ref()
        );
        // Both packs with the same grade — fitted together, each a copy of
        // the film's: it is the film's as it is, with nothing of what each
        // saw asked for (there is none here to ask for).
        let mut both = Files(BTreeMap::from([
            (pack_tone_key("film/1-10.tuilepack"), grade.to_json()),
            (pack_tone_key("film/11-20.tuilepack"), grade.to_json()),
        ]));
        assert_eq!(
            block(film_grade(&both, &keys)).expect("asked").as_ref(),
            FilmGrade::from_json(&grade.to_json()).as_ref()
        );
        // Two that differ are not one grade: what each saw is wanted.
        both.0.insert(
            pack_tone_key("film/11-20.tuilepack"),
            graded(&seen_of(0, 12, 8)).to_json(),
        );
        assert!(block(film_grade(&both, &keys)).is_err());
        // A film without one has none, whatever lies beside it.
        assert_eq!(
            block(film_grade(&files, &["bare/1-10.tuilepack"])).expect("asked"),
            None
        );
        // A grade that cannot be read is an error, not an absence.
        files
            .0
            .insert(pack_tone_key("film/11-20.tuilepack"), "not one".into());
        assert!(block(film_grade(&files, &keys)).is_err());
        files
            .0
            .insert(pack_tone_key("broken/1.tuilepack"), grade.to_json());
        assert!(block(film_grade(&files, &["broken/1.tuilepack"])).is_err());
    }

    /// Files of bytes, for what is not text.
    struct Bytes(BTreeMap<String, Vec<u8>>);

    #[async_trait::async_trait]
    impl Objects for Bytes {
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
            self.0
                .get(key)
                .map(|b| b.len() as u64)
                .ok_or_else(|| RepoError::NotFound(key.to_string()))
        }
        async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
            self.size(key).await?;
            Ok(self.0[key][range.start as usize..range.end as usize].to_vec())
        }
    }

    #[test]
    fn a_film_of_several_packs_is_fitted_as_one_from_what_each_saw() {
        // Two packs, cut where the capture changes: each alone is one
        // capture and gives no tile a gain.
        let (west, east) = (seen_of(0, 4, 4), seen_of(4, 12, 4));
        let (g_west, g_east) = (graded(&west), graded(&east));
        assert!(lift(&g_west, 1).abs() < 0.05 && lift(&g_east, 8).abs() < 0.05);
        let mut files = Bytes(BTreeMap::from([
            (
                pack_tone_key("film/a.tuilepack"),
                g_west.to_json().into_bytes(),
            ),
            (
                pack_tone_key("film/b.tuilepack"),
                g_east.to_json().into_bytes(),
            ),
            (pack_seen_key("film/a.tuilepack"), west.to_bytes()),
            (pack_seen_key("film/b.tuilepack"), east.to_bytes()),
        ]));
        let keys = ["film/a.tuilepack", "film/b.tuilepack"];
        let film = block(film_grade(&files, &keys))
            .expect("asked")
            .expect("a grade");
        // Together they are two captures, and the smaller is brought to
        // the larger across the cut.
        let lifted = lift(&film, 1);
        assert!((lifted - 1.0).abs() < 0.1, "{lifted}");
        assert!(lift(&film, 9).abs() < 0.05);
        // Without what one of them saw the film cannot be fitted, and that
        // is said rather than drawn around.
        files.0.remove(&pack_seen_key("film/b.tuilepack"));
        assert!(block(film_grade(&files, &keys)).is_err());
    }

    #[test]
    fn a_layer_is_graded_if_its_store_says_so() {
        let files = Files(BTreeMap::from([
            (layer_tone_key("imagery"), "{}".into()),
            (layer_tone_key("broken"), "{}".into()),
        ]));
        assert!(block(layer_is_graded(&files, "imagery")).expect("asked"));
        assert!(!block(layer_is_graded(&files, "another")).expect("asked"));
        assert!(block(layer_is_graded(&files, "broken")).is_err());
    }

    #[test]
    fn a_store_that_cannot_be_asked_is_an_error_not_an_absence() {
        let places = BTreeMap::from([((7, 7), 1)]);
        let files = Files(BTreeMap::from([(layer_tone_key("broken"), of(1.0))]));
        assert!(block(film_tone(&files, "broken", &places)).is_err());
        let files = Files(BTreeMap::from([
            (layer_tone_key("imagery"), of(1.0)),
            (region_key("imagery", 7, 7), "not a table".into()),
        ]));
        assert!(block(film_tone(&files, "imagery", &places)).is_err());
    }
}
