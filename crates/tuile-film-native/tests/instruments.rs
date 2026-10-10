// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The parts around the render that need no GPU and no bucket: what is
//! counted of a read, what the meter makes of what it is shown, and the
//! film a sink writes.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use tuile_film::TileKey;
use tuile_film_native::source::Cache;
use tuile_film_native::{
    Av1Film, FrameOut, ImageryIn, LightMeter, Observer, Origin, Sink, TileIn, Timings,
};
use tuile_repository::{Entry, Listing, Objects, RepoError};

/// One object of 20 MiB whose every byte says where it is.
struct Origin20;

fn byte_at(i: u64) -> u8 {
    (i % 251) as u8
}

#[async_trait]
impl Objects for Origin20 {
    fn label(&self) -> String {
        "test".into()
    }
    async fn list(&self, _: &str) -> Result<Vec<Entry>, RepoError> {
        Ok(Vec::new())
    }
    async fn browse(&self, _: &str) -> Result<Listing, RepoError> {
        Ok(Listing::default())
    }
    async fn size(&self, _: &str) -> Result<u64, RepoError> {
        Ok(20 << 20)
    }
    async fn read(&self, _: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        Ok(range.map(byte_at).collect())
    }
}

#[tokio::test]
async fn what_was_read_once_is_not_fetched_again_and_both_sides_are_counted() {
    let dir = tempfile::tempdir().expect("dir");
    let cache = Cache::over(Arc::new(Origin20), dir.path());
    let range = (9u64 << 20)..(9u64 << 20) + 1000;
    let wanted: Vec<u8> = range.clone().map(byte_at).collect();

    assert_eq!(
        cache.objects.read("a", range.clone()).await.expect("read"),
        wanted
    );
    let (asked, fetched) = cache.reads();
    assert_eq!((asked.reads, asked.bytes), (1, 1000));
    // A thousand bytes asked for; the chunk they are in fetched, whole.
    assert_eq!(fetched.reads, 1);
    assert!(fetched.bytes > 1000);

    assert_eq!(
        cache.objects.read("a", range.clone()).await.expect("read"),
        wanted
    );
    let (asked, again) = cache.reads();
    assert_eq!(asked.reads, 2);
    assert_eq!(again, fetched, "the second read went to the origin");

    // And from the directory alone, by a cache that has nothing in memory.
    let cold = Cache::over(Arc::new(Origin20), dir.path());
    assert_eq!(cold.objects.read("a", range).await.expect("read"), wanted);
    assert_eq!(
        cold.reads().1.bytes,
        0,
        "what was on disk was fetched again"
    );
}

/// A flat imagery tile of one stored grey, as a PNG.
fn flat(grey: u8) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(64, 64, image::Rgba([grey, grey, grey, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("png");
    bytes.into_inner()
}

#[test]
fn the_meter_finds_the_step_between_two_sources_and_which_levels_are_one() {
    let mut meter = LightMeter::default();
    let mut show = |level: u8, x: u32, y: u32, grey: u8| {
        meter.imagery(&ImageryIn {
            level,
            x,
            y,
            bytes: &flat(grey),
            renewed: false,
            grade: Default::default(),
        });
    };
    // Levels 10 to 12 one source; 13 and 14 another, darker.
    show(10, 0, 0, 160);
    show(11, 1, 1, 160);
    show(12, 2, 2, 160);
    for (x, y) in [(4, 4), (5, 4), (4, 5), (5, 5)] {
        show(13, x, y, 90);
    }
    show(14, 8, 8, 90);
    meter.tile(&TileIn {
        frame: 1,
        key: TileKey { id: 1, drape: 0 },
        origin: Origin::Store,
        imagery: &[(13, 4, 4), (14, 8, 8)],
    });

    let solved = meter.solve(10);
    assert!(solved.of(11).is_identity());
    assert!(solved.of(12).is_identity());
    // Stored 90 against stored 160 is 1.78 stops of light: flat tiles have
    // nothing but a gain to say.
    let found = solved.of(13);
    let lifted = (found.apply([0.1022; 3])[1] / 0.1022).log2();
    assert!((1.6..1.95).contains(&lifted), "{found:?}");
    assert_eq!(solved.of(14), found);
    assert!(solved.sources[0].before.0 > 1.5 && solved.sources[0].after.0 < 0.1);

    let dir = tempfile::tempdir().expect("dir");
    let report = meter.write(dir.path(), "a film", &solved).expect("write");
    assert!(report.contains("| 13 | 4 |"), "{report}");
    let tiles = std::fs::read_to_string(dir.path().join("tiles.csv")).expect("tiles.csv");
    // The tile in a drape is counted in it; the others are not.
    assert!(tiles.lines().any(|l| l.starts_with("13,4,4,1,")), "{tiles}");
    assert!(tiles.lines().any(|l| l.starts_with("13,5,5,0,")), "{tiles}");
    assert!(dir.path().join("tone.json").exists());
}

#[test]
fn the_meter_reads_a_picture_in_stops_and_by_thirds() {
    // Top half white, bottom half mid grey (stored 188 is half the light).
    let (width, height) = (16u32, 12u32);
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|i| {
            let v = if i / width < height / 2 { 255 } else { 188 };
            [v, v, v, 255]
        })
        .collect();
    let mut meter = LightMeter::default();
    meter.frame(&FrameOut {
        frame: 7,
        index: 0,
        width,
        height,
        rgba: &rgba,
        tiles: 3,
        entered: 3,
        layers: &[(12, 2), (13, 1)],
        timings: Timings::default(),
    });
    let light = meter.pictures()[0];
    assert_eq!(light.frame, 7);
    assert!(light.bands[0].abs() < 0.01, "{:?}", light.bands);
    assert!((light.bands[2] + 1.0).abs() < 0.03, "{:?}", light.bands);
    // Three quarters of white, on average.
    assert!((light.mean - 0.75f32.log2()).abs() < 0.02, "{}", light.mean);
    assert!(light.red.abs() < 1e-3 && light.blue.abs() < 1e-3);
}

#[test]
fn a_film_of_pictures_is_an_mp4_with_every_one_of_them() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("film.mp4");
    let (width, height) = (64usize, 48usize);
    let mut film = Av1Film::at(&path, 500_000);
    assert!(film.wants_i420());
    film.open(width as u32, height as u32, 30).expect("open");
    for n in 0..5u32 {
        let mut i420 = vec![(40 + n * 30) as u8; width * height];
        i420.resize(width * height * 3 / 2, 128);
        film.picture(n, n + 1, &[], &i420).expect("picture");
    }
    // A picture of the wrong size is refused, not encoded as something.
    assert!(film.picture(5, 6, &[], &[0; 10]).is_err());
    film.close().expect("close");

    let bytes = std::fs::read(&path).expect("film");
    let has = |tag: &[u8]| bytes.windows(tag.len()).any(|w| w == tag);
    assert!(
        has(b"ftyp") && has(b"moov") && has(b"av01"),
        "not an AV1 mp4"
    );
    // The sample table counts five samples: `stsz`, version and flags, a
    // sample size of 0 (sizes differ), then the count.
    let stsz = bytes
        .windows(4)
        .position(|w| w == b"stsz")
        .expect("a sample size table");
    let count = u32::from_be_bytes(bytes[stsz + 12..stsz + 16].try_into().expect("four bytes"));
    assert_eq!(count, 5);
}

/// The machine's own encoder, where there is one: every picture given
/// comes back, in an mp4 a player opens.
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
#[test]
fn the_machines_encoder_writes_an_h264_mp4_with_every_picture() {
    use tuile_film_native::H264Film;

    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("film.mp4");
    let (width, height) = (320usize, 240usize);
    let mut film = H264Film::at(&path, 2_000_000);
    film.open(width as u32, height as u32, 30).expect("open");
    for n in 0..12u32 {
        let mut i420 = vec![(30 + n * 15) as u8; width * height];
        i420.resize(width * height * 3 / 2, 128);
        film.picture(n, n + 1, &[], &i420).expect("picture");
    }
    assert!(film.picture(12, 13, &[], &[0; 10]).is_err());
    film.close().expect("close");

    let bytes = std::fs::read(&path).expect("film");
    let has = |tag: &[u8]| bytes.windows(tag.len()).any(|w| w == tag);
    assert!(
        has(b"ftyp") && has(b"moov") && has(b"avc1") && has(b"avcC"),
        "not an H.264 mp4"
    );
    let stsz = bytes
        .windows(4)
        .position(|w| w == b"stsz")
        .expect("a sample size table");
    let count = u32::from_be_bytes(bytes[stsz + 12..stsz + 16].try_into().expect("four bytes"));
    assert_eq!(count, 12);
}

/// A film is the packs under a prefix, or the packs named and no other: a
/// part's pack lies beside the other parts', and is opened alone.
#[tokio::test]
async fn a_pack_named_is_opened_without_its_neighbours() {
    use tuile_film::BakedView;
    use tuile_film_native::Film;
    use tuile_pack::PackWriter;

    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("packs")).expect("packs");
    for part in 0..3u32 {
        let mut w = PackWriter::new("s", [0.0; 3], dir.path().join("blob")).expect("writer");
        for frame in 1 + 3 * part..=3 + 3 * part {
            let view = BakedView {
                position: [7_000_000.0, 0.0, 0.0],
                direction: [-1.0, 0.0, 0.0],
                up: [0.0, 0.0, 1.0],
                viewport_px: [640.0, 480.0],
                fovy_rad: 1.0,
            };
            w.frame(frame, view, []);
        }
        w.finish_to(dir.path().join(format!("packs/c{part:04}.tuilepack")))
            .expect("finish");
    }
    let objects =
        tuile_farm::ObjectRunStore::local(dir.path(), tuile_farm::Tuning::from_env()).expect("dir");

    let whole = Film::open(&objects, "packs").await.expect("open");
    assert_eq!((whole.packs.len(), whole.frames()), (3, (1, 9)));

    let part = Film::of(&objects, &["packs/c0001.tuilepack"])
        .await
        .expect("of");
    assert_eq!(part.packs.len(), 1);
    assert_eq!(part.packs[0].key, "packs/c0001.tuilepack");
    assert_eq!(part.frames(), (4, 6));
    assert_eq!(part.packs[0].head, whole.packs[1].head);

    // Named out of order, they are a film in the order of their frames.
    let two = Film::of(
        &objects,
        &["packs/c0002.tuilepack", "packs/c0000.tuilepack"],
    )
    .await
    .expect("of");
    assert_eq!((two.packs[0].first, two.packs[1].first), (1, 7));
    assert!(Film::of(&objects, &[]).await.is_err());
    assert!(Film::of(&objects, &["packs/c0009.tuilepack"])
        .await
        .is_err());
}
