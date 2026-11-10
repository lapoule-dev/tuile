// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **What settling holds does not grow with the frames it settles.**
//!
//! The made-up globe of `ground_memory`, flown over north by many more
//! frames than a settle may hold grounds for, each ground read slowly —
//! the way a 4K bake's audits are read, slower than the next ground is
//! built. Built grounds used to pile up in front of their readings, one per
//! frame settled, each some fifteen times its selection; and every frozen
//! selection was held until the last frame was settled, pinning tiles the
//! session had let go. Now at most [`READING`] grounds are out for reading
//! at once, whichever stage is slowest, and only the selections the caller
//! keeps are held. What is read is what reading them in turn reads.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use glam::DVec3;
use support::{encoded, measured, LAT, LON};
use tuile_bake::settle::{read_each, settle_each, Drawn, READING};
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Held, Session, SourcesHandle};
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_film::ground::{Ground, Prepare};
use tuile_film::smoothing::{Smooth, DEFAULT_BOUND_M};
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};

/// Many more frames than grounds may be out for reading.
const FRAMES: u32 = 4 * READING as u32;
/// How long each ground takes to read: far longer than a frame takes to
/// converge and its ground to build.
const READ: Duration = Duration::from_millis(150);

/// The finest terrain the made-up source has.
const DEEPEST: u32 = 15;
/// Spacings a side of each terrain tile: 65 × 65 vertices.
const STEPS: usize = 64;
const VIEWPORT: (f64, f64) = (960.0, 720.0);
const SSE: f64 = 4.0;

struct Relief;

#[async_trait::async_trait]
impl TerrainSource for Relief {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        if coord.level > DEEPEST {
            return Err(TerrainSourceError::no_such_tile(format!("{coord:?}")));
        }
        Ok(Fetched::undated(encoded(&measured(coord, STEPS))))
    }
}

/// A 4 × 4 PNG of one colour, for every imagery tile.
struct OnePicture;

const PICTURE: [u8; 73] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 4, 8, 2, 0,
    0, 0, 38, 147, 9, 41, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 168, 104, 202, 131, 35, 6,
    226, 56, 0, 73, 242, 22, 129, 231, 235, 56, 46, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

#[async_trait::async_trait]
impl ImageryProvider for OnePicture {
    fn tiling_scheme(&self) -> TilingScheme {
        TilingScheme::web_mercator()
    }

    async fn fetch_tile_bytes(
        &self,
        _coord: ImageryCoord,
    ) -> Result<Fetched<bytes::Bytes>, RasterError> {
        Ok(Fetched::undated(bytes::Bytes::from_static(&PICTURE)))
    }
}

struct MadeUp;

#[async_trait::async_trait]
impl tuile_bake::Sources for MadeUp {
    async fn terrain(&self, _asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        let available: Vec<String> = (0..=DEEPEST)
            .map(|level| {
                format!(
                    r#"[{{"startX":0,"startY":0,"endX":{},"endY":{}}}]"#,
                    (2u64 << level) - 1,
                    (1u64 << level) - 1
                )
            })
            .collect();
        let layer = LayerJson::from_slice(
            format!(
                r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
                    "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
                    "bounds":[-180,-90,180,90],"available":[{}]}}"#,
                available.join(",")
            )
            .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        Ok((layer, Arc::new(Relief)))
    }

    async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        Ok(Arc::new(OnePicture))
    }
}

/// The camera of frame `i`: flying north a kilometre up, looking ahead and
/// down.
fn pose(i: u32) -> tuile_tape::Frame {
    let lat = LAT + f64::from(i) * 0.003;
    let (p, l) = (lat.to_radians(), LON.to_radians());
    let up = DVec3::new(p.cos() * l.cos(), p.cos() * l.sin(), p.sin());
    let north = DVec3::new(-p.sin() * l.cos(), -p.sin() * l.sin(), p.cos());
    let eye = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
        lon: l,
        lat: p,
        height: 1_300.0,
    });
    let direction = (north - up * 0.6).normalize();
    let camera_up = direction.cross(north.cross(up)).normalize() * -1.0;
    tuile_tape::Frame {
        position: eye.to_array(),
        direction: direction.to_array(),
        up: camera_up.to_array(),
        fovy: std::f64::consts::FRAC_PI_4,
    }
}

const DRAWN: Drawn = Drawn {
    how: Prepare {
        skirts: true,
        smooth: Smooth::Geometry {
            bound: DEFAULT_BOUND_M,
        },
    },
    stitch: true,
};

fn session() -> Session {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp)));
    config.session.traversal.maximum_screen_space_error = SSE;
    config.session.finish = Finish::Identity;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    session
}

/// What a frame reads off its ground: its height on a line under the eye.
fn heights(i: u32, ground: &Ground) -> Vec<Option<u64>> {
    let (lat, lon) = ((LAT + f64::from(i) * 0.003).to_radians(), LON.to_radians());
    (0..16)
        .map(|k| {
            let d = f64::from(k) * 0.00005;
            ground.height_at(lon + d, lat + d).map(f64::to_bits)
        })
        .collect()
}

/// The only test of this binary: the gauges it reads are the process's.
#[test]
fn grounds_out_for_reading_and_selections_held_are_bounded() {
    let m = tuile_core::metrics::metrics();
    let numbers: Vec<u32> = (0..FRAMES).collect();
    let views = |i: u32, _: &Session| vec![pose_view(&pose(i), VIEWPORT)];

    // The reference: each frame settled, built and read in turn.
    let mut reference_session = session();
    let mut reference_drawer = GroundDrawer::new();
    let reference: Vec<(u64, Vec<Option<u64>>)> = numbers
        .iter()
        .map(|&i| {
            reference_session.frame(views(i, &reference_session)).expect("a frame");
            let selected = reference_session.selected_ground().expect("kept");
            let ground = reference_drawer.ground(&selected, DRAWN.how, DRAWN.stitch);
            (reference_session.selection_digest().expect("live"), heights(i, &ground))
        })
        .collect();

    // Settled in stages, the readings slower than the building; every
    // fourth frame's selection kept.
    let kept = |i: u32| i % 4 == 0;
    let (out, held) = (AtomicU64::new(0), AtomicU64::new(0));
    let mut drawer = GroundDrawer::new();
    let started = std::time::Instant::now();
    let frames = settle_each(
        &mut session(),
        &mut drawer,
        DRAWN,
        &numbers,
        views,
        |i, ground: &Arc<Ground>| {
            std::thread::sleep(READ);
            out.fetch_max(m.ground_reads_pending.get(), Ordering::SeqCst);
            held.fetch_max(m.settled_tiles.get(), Ordering::SeqCst);
            heights(i, ground)
        },
        kept,
    )
    .expect("settled");
    let read: Vec<(u64, Vec<Option<u64>>)> =
        frames.iter().map(|s| (s.selection, s.value.clone())).collect();
    assert_eq!(read, reference, "read as reading them in turn reads");

    let out = out.load(Ordering::SeqCst);
    let mut only_kept = Held::default();
    for s in frames.iter().filter(|s| kept(s.number)) {
        if let Some(selected) = &s.selected {
            only_kept.add(selected);
        }
    }
    let held = held.load(Ordering::SeqCst);
    // One line for the bench's table.
    eprintln!(
        "BENCH {{\"frames\":{FRAMES},\"grounds_out_max\":{out},\"held_tiles_max\":{held},\"kept_tiles\":{},\"seconds\":{:.3}}}",
        only_kept.tiles(),
        started.elapsed().as_secs_f64()
    );
    assert!(out >= 1, "a ground was out");
    assert!(
        out <= READING as u64,
        "{out} grounds out for reading at once, more than {READING}"
    );

    // Only the selections kept come back, and only they were held.
    for s in &frames {
        assert_eq!(s.selected.is_some(), kept(s.number), "frame {}", s.number);
    }
    assert!(
        held <= only_kept.tiles() as u64,
        "{held} tiles held, more than the {} of the selections kept",
        only_kept.tiles()
    );
    assert_eq!(m.settled_tiles.get(), 0, "the selections handed over");

    // Grounds built again from their selections are bounded alike.
    let again: Vec<(u32, &tuile_bake::Selected)> = frames
        .iter()
        .filter_map(|s| Some((s.number, s.selected.as_ref()?)))
        .collect();
    let mut many: Vec<(u32, &tuile_bake::Selected)> = Vec::new();
    for _ in 0..4 {
        many.extend(again.iter().copied());
    }
    let out = AtomicU64::new(0);
    let read = read_each(&mut drawer, DRAWN, &many, |i, ground| {
        std::thread::sleep(READ);
        out.fetch_max(m.ground_reads_pending.get(), Ordering::SeqCst);
        heights(i, ground)
    });
    let out = out.load(Ordering::SeqCst);
    eprintln!("reading again: at most {out} grounds out of {}", many.len());
    assert!(
        out <= READING as u64,
        "{out} grounds out for reading again at once, more than {READING}"
    );
    for ((i, _), (heights, _, _)) in many.iter().zip(read) {
        assert_eq!(heights, reference[*i as usize].1, "frame {i} read again");
    }
    assert_eq!(m.ground_reads_pending.get(), 0);
}
