// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The ground a camera is kept over, its weight and its heights.**
//!
//! A made-up globe — a relief measured by each terrain tile on a 65 × 65
//! grid, as a heightmap source states it — flown over north, a kilometre
//! up, by a session finishing by identity and keeping the ground: the way a
//! production bake places its camera. Each frame, the ground is asked for as
//! the render service draws it (skirts kept, smoothed within 2 m, stitched)
//! and as drawn plainly, and its height read on a grid of places around the
//! eye.
//!
//! The heights are compared, bit for bit, with those of the ground drawn
//! the way it was before the meshes were kept small — from every mesh as it
//! came, prepared and stitched afresh — on the same session, the same
//! selection, in the same run: whatever order the tiles arrive in, the two
//! are drawn from one resident set. A camera's placement is a function of
//! these numbers and a film of its placement, so a ground kept more cheaply
//! has to answer exactly the same ones.
//!
//! What the ground takes, frame by frame, is printed (`--nocapture`).

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::DVec3;
use support::{encoded, measured, LAT, LON};
use tuile_bake::settle::{read_each, settle_each, Drawn};
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Held, Session, SourcesHandle};
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_film::ground::Prepare;
use tuile_film::smoothing::{Smooth, DEFAULT_BOUND_M};
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};

/// The finest terrain the made-up source has.
const DEEPEST: u32 = 15;
/// Spacings a side of each terrain tile: 65 × 65 vertices.
const STEPS: usize = 64;
const VIEWPORT: (f64, f64) = (960.0, 720.0);
const SSE: f64 = 4.0;
const FRAMES: u32 = 6;
/// Places a side the height is read at, each frame.
const GRID: u32 = 24;

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

/// The ground as the render service draws it, and as drawn plainly.
const DRAWN: [(Prepare, bool); 2] = [
    (
        Prepare {
            skirts: true,
            smooth: Smooth::Geometry {
                bound: DEFAULT_BOUND_M,
            },
        },
        true,
    ),
    (
        Prepare {
            skirts: false,
            smooth: Smooth::Off,
        },
        false,
    ),
];

fn mib(bytes: usize) -> f64 {
    bytes as f64 / f64::from(1u32 << 20)
}

#[test]
fn the_ground_kept_small_answers_the_heights_of_the_meshes_as_they_came() {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp)));
    config.session.traversal.maximum_screen_space_error = SSE;
    config.session.finish = Finish::Identity;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground_and_meshes();

    let (mut read, mut differing) = (0usize, 0usize);
    for i in 0..FRAMES {
        let frame = session.frame_for_pose(&pose(i), VIEWPORT).expect("a frame");
        for (k, (how, stitch)) in DRAWN.iter().enumerate() {
            let ground = session.ground(*how, *stitch).expect("kept");
            let as_it_came = session
                .ground_as_it_came(*how, *stitch)
                .expect("the meshes kept");
            assert_eq!(ground.len(), as_it_came.len(), "frame {i} ground {k}");
            assert!(!ground.is_empty(), "frame {i} ground {k}");
            let memory = session.ground_memory().expect("kept");
            eprintln!(
                "frame {i} ground {k}: {} tiles drawn of {} resident | kept {:.2} MiB, \
                 prepared and stitched {:.2} MiB ({} tiles), drawn {:.2} MiB",
                frame.tiles.len(),
                memory.kept_tiles,
                mib(memory.kept_bytes),
                mib(memory.prepared_bytes),
                memory.prepared_tiles,
                mib(memory.drawn_bytes),
            );
            // A grid of places round the point under the eye: near, where
            // the ground is fine, and up to a few kilometres off.
            let (lat0, lon0) = ((LAT + f64::from(i) * 0.003).to_radians(), LON.to_radians());
            let f = |n: u32| (f64::from(n) / f64::from(GRID - 1) - 0.5) * 0.0012;
            for a in 0..GRID {
                for b in 0..GRID {
                    let (lon, lat) = (lon0 + f(a) * 1.6, lat0 + f(b) + 0.0004);
                    let (now, then) = (ground.height_at(lon, lat), as_it_came.height_at(lon, lat));
                    read += usize::from(then.is_some());
                    differing += usize::from(now.map(f64::to_bits) != then.map(f64::to_bits));
                }
            }
        }
    }
    assert!(read > 0, "the ground answered somewhere");
    assert_eq!(
        differing, 0,
        "{differing} heights differ from those of the meshes as they came ({read} read)"
    );
    eprintln!("{read} heights read, every one as from the meshes as they came");
}

/// The only test of this binary to settle: the numbers it reads are the
/// process's, and a second settle beside it would move them.
#[test]
fn what_settling_holds_is_counted_while_held_and_let_go_after() {
    let m = tuile_core::metrics::metrics();
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp)));
    config.session.traversal.maximum_screen_space_error = SSE;
    config.session.finish = Finish::Identity;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    let mut drawer = GroundDrawer::new();
    let drawn = Drawn {
        how: DRAWN[0].0,
        stitch: DRAWN[0].1,
    };
    let numbers: Vec<u32> = (0..FRAMES).collect();

    let (reads, read_bytes, settled_bytes) =
        (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let frames = settle_each(
        &mut session,
        &mut drawer,
        drawn,
        &numbers,
        |i, _| vec![pose_view(&pose(i), VIEWPORT)],
        |i, ground| {
            reads.fetch_max(m.ground_reads_pending.get(), Ordering::SeqCst);
            let bytes = m.ground_reads_pending_bytes.get();
            assert!(bytes >= ground.heap_bytes() as u64, "frame {i}: {bytes}");
            read_bytes.fetch_max(bytes, Ordering::SeqCst);
            // The frames before the last are read by now or about to be.
            let deadline = Instant::now() + Duration::from_secs(10);
            while i == FRAMES - 1 && m.settled_bytes.get() == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            settled_bytes.fetch_max(m.settled_bytes.get(), Ordering::SeqCst);
        },
        |_| true,
    )
    .expect("settled");
    assert!(reads.load(Ordering::SeqCst) >= 1, "a ground waited");
    assert!(read_bytes.load(Ordering::SeqCst) > 0, "and weighed");
    assert!(
        settled_bytes.load(Ordering::SeqCst) > 0,
        "the selections settled were counted"
    );
    assert_eq!(m.ground_reads_pending.get(), 0, "every ground read");
    assert_eq!(m.ground_reads_pending_bytes.get(), 0, "every ground let go");
    assert_eq!(m.settled_tiles.get(), 0, "the selections handed over");
    assert_eq!(m.settled_bytes.get(), 0, "and their bytes");

    // A tile shared by neighbouring frames is held, and counted, once.
    let mut held = Held::default();
    let kept: Vec<&tuile_bake::Selected> = frames.iter().filter_map(|s| s.selected.as_ref()).collect();
    assert_eq!(kept.len(), frames.len(), "every selection kept");
    for s in &kept {
        held.add(s);
    }
    let selected: usize = kept.iter().map(|s| s.len()).sum();
    let widest = kept.iter().map(|s| s.len()).max().unwrap_or(0);
    assert!(
        (widest..selected).contains(&held.tiles()),
        "{} tiles of {selected} selected, the widest frame {widest}",
        held.tiles()
    );
    assert!(held.bytes() > 0);

    // The staged path draws on its own drawer: the session's has nothing to
    // say about it, and that drawer is where the figures are.
    let memory = session.ground_memory().expect("kept");
    assert!(memory.kept_bytes > 0);
    assert_eq!(memory.prepared_bytes, 0, "the session drew nothing");
    assert!(drawer.prepared().1 > 0, "the drawer prepared the tiles");
    assert!(drawer.built() > 0, "and built the grounds");

    // Grounds built again from their selections wait for their reads alike.
    let frozen: Vec<(u32, &tuile_bake::Selected)> =
        frames.iter().zip(&kept).map(|(s, k)| (s.number, *k)).collect();
    let waited = AtomicU64::new(0);
    read_each(&mut drawer, drawn, &frozen, |_, _| {
        waited.fetch_max(m.ground_reads_pending.get(), Ordering::SeqCst);
    });
    assert!(waited.load(Ordering::SeqCst) >= 1, "a ground waited again");
    assert_eq!(m.ground_reads_pending.get(), 0);
    assert_eq!(m.ground_reads_pending_bytes.get(), 0);
}
