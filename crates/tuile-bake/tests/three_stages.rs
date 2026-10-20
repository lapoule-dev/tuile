// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A bake in three stages is the bake one frame after another, bit for
//! bit.**
//!
//! A made-up globe — a relief measured by each terrain tile, one picture
//! for every imagery tile — whose every tile arrives after a wait of its
//! own, different on every run, so that no two sessions see their loads
//! land in the same order. Over it, a camera flies north a kilometre up.
//!
//! Each stage is compared, on two sessions opened alike, with doing its
//! work frame after frame, written out here as the reference:
//!
//! - settling ([`tuile_bake::settle::settle_each`]): the same selection
//!   every frame, the same heights read off every ground, bit for bit —
//!   views aimed, as a bake aims them, with the heights the session holds;
//! - baking ([`tuile_bake::bake::bake_frames`]): the same pack, byte for
//!   byte, and the same selection every frame;
//! - scouting ahead ([`tuile_bake::scout::Scout`]) over a tile store the
//!   scouts fill first: the same selections and the same heights as
//!   without them.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use glam::DVec3;
use support::{encoded, measured, LAT, LON};
use tuile_bake::bake::{
    bake_frames, digest_of_scene, packed_tile, BakeFrames, PackContent, PackedMirror, Poses,
    SceneName,
};
use tuile_bake::settle::{settle_each, Drawn};
use tuile_bake::{Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle, TileCache};
use tuile_core::fetch::Fetched;
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic};
use tuile_core::ground::GroundHeight;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_core::storage::ContentStore;
use tuile_core::traversal::ViewStateParams;
use tuile_film::ground::{Ground, Prepare};
use tuile_film::smoothing::{Smooth, DEFAULT_BOUND_M};
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};

/// The finest terrain the made-up source has.
const DEEPEST: u32 = 15;
const VIEWPORT: (f64, f64) = (480.0, 360.0);
const SSE: f64 = 6.0;
const FRAMES: u32 = 10;
/// Places a side the height is read at, each frame.
const GRID: u32 = 12;

/// Differs on every source made, so that two sessions wait differently.
static RUNS: AtomicU64 = AtomicU64::new(1);

/// A number from `x`, well mixed (splitmix64).
fn mixed(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The relief, every tile after a wait of up to 12 ms, its own.
struct Relief {
    run: u64,
}

#[async_trait::async_trait]
impl TerrainSource for Relief {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        let wait = mixed(self.run ^ (u64::from(coord.level) << 58) ^ (coord.x << 29) ^ coord.y);
        tokio::time::sleep(Duration::from_micros(wait % 12_000)).await;
        if coord.level > DEEPEST {
            return Err(TerrainSourceError::no_such_tile(format!("{coord:?}")));
        }
        Ok(Fetched::undated(encoded(&measured(coord, 16))))
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
        let run = mixed(RUNS.fetch_add(1, Ordering::SeqCst));
        Ok((layer, Arc::new(Relief { run })))
    }

    async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        Ok(Arc::new(OnePicture))
    }
}

/// A tile store in memory.
#[derive(Default)]
struct Memory(Mutex<HashMap<String, Bytes>>);

#[async_trait::async_trait]
impl ContentStore for Memory {
    async fn get(&self, key: &str) -> Option<Bytes> {
        self.0.lock().expect("not poisoned").get(key).cloned()
    }

    async fn put(&self, key: &str, value: Bytes, _: Option<Duration>) {
        self.0
            .lock()
            .expect("not poisoned")
            .insert(key.to_string(), value);
    }
}

fn config(finish: Finish, store: Option<Arc<Memory>>) -> GlobeConfig {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp)));
    config.session.traversal.maximum_screen_space_error = SSE;
    config.session.finish = finish;
    config.session.bake_max_size = 128;
    config.tile_cache = store.map(|store| TileCache(store as Arc<dyn ContentStore>));
    config
}

/// The place under frame `i`'s eye, degrees.
fn under(i: u32) -> (f64, f64) {
    (LON + f64::from(i) * 0.0003, LAT + f64::from(i) * 0.0009)
}

/// The camera of frame `i`: flying north-north-east a kilometre up,
/// looking ahead and down.
fn pose(i: u32) -> tuile_tape::Frame {
    let (lon, lat) = under(i);
    let (p, l) = (lat.to_radians(), lon.to_radians());
    let up = DVec3::new(p.cos() * l.cos(), p.cos() * l.sin(), p.sin());
    let north = DVec3::new(-p.sin() * l.cos(), -p.sin() * l.sin(), p.cos());
    let eye = geodetic_to_ecef(Geodetic {
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

fn view(f: &tuile_tape::Frame) -> ViewStateParams {
    ViewStateParams {
        position: DVec3::from_array(f.position),
        direction: DVec3::from_array(f.direction),
        up: DVec3::from_array(f.up),
        viewport_px: glam::dvec2(VIEWPORT.0, VIEWPORT.1),
        fovy_rad: f.fovy,
    }
}

/// The views frame `i` is settled on, as a bake settles a camera: its
/// pose, its eye kept above the highest ground the session's heights know
/// under it — so what is aimed depends on what was loaded before — and
/// straight down from it.
fn views(i: u32, session: &Session) -> Vec<ViewStateParams> {
    let mut eye = pose(i);
    let at = ecef_to_geodetic(DVec3::from_array(eye.position));
    if let Some(ground) = session
        .heights()
        .and_then(|h| h.height_at(at.lon, at.lat))
        .filter(|g| *g + 900.0 > at.height)
    {
        eye.position = geodetic_to_ecef(Geodetic {
            height: ground + 900.0,
            ..at
        })
        .to_array();
    }
    let enu = enu_frame(at);
    let nadir = tuile_tape::Frame {
        position: eye.position,
        direction: (-enu.z_axis).to_array(),
        up: enu.y_axis.to_array(),
        fovy: std::f64::consts::FRAC_PI_4,
    };
    vec![view(&eye), view(&nadir)]
}

/// The ground as the render service draws it.
const DRAWN: Drawn = Drawn {
    how: Prepare {
        skirts: true,
        smooth: Smooth::Geometry {
            bound: DEFAULT_BOUND_M,
        },
    },
    stitch: true,
};

/// The heights of `ground` on a grid of places round the point under
/// frame `i`'s eye, bit for bit.
fn heights(i: u32, ground: &Ground) -> Vec<Option<u64>> {
    let (lon, lat) = under(i);
    let f = |n: u32| (f64::from(n) / f64::from(GRID - 1) - 0.5) * 0.004;
    let mut out = Vec::new();
    for a in 0..GRID {
        for b in 0..GRID {
            let (lon, lat) = ((lon + f(a)).to_radians(), (lat + f(b) + 0.002).to_radians());
            out.push(ground.height_at(lon, lat).map(f64::to_bits));
        }
    }
    out
}

/// Settling, frame after frame, on one thread: the reference.
fn settled_in_turn(session: &mut Session) -> Vec<(u64, Vec<Option<u64>>)> {
    session.keep_ground();
    (0..FRAMES)
        .map(|i| {
            let v = views(i, session);
            session.frame(v).expect("a frame");
            let selection = session.selection_digest().expect("live");
            let ground = session.ground(DRAWN.how, DRAWN.stitch).expect("kept");
            (selection, heights(i, &ground))
        })
        .collect()
}

/// Settling in stages, as a bake does.
fn settled_in_stages(session: &mut Session) -> Vec<(u64, Vec<Option<u64>>)> {
    session.keep_ground();
    let numbers: Vec<u32> = (0..FRAMES).collect();
    settle_each(
        session,
        &mut GroundDrawer::new(),
        DRAWN,
        &numbers,
        views,
        heights,
    )
    .expect("settled")
    .into_iter()
    .map(|s| (s.selection, s.value))
    .collect()
}

fn read(settled: &[(u64, Vec<Option<u64>>)]) -> usize {
    settled
        .iter()
        .map(|(_, h)| h.iter().filter(|h| h.is_some()).count())
        .sum()
}

#[test]
fn settling_in_stages_is_settling_frame_after_frame() {
    let in_turn =
        settled_in_turn(&mut Session::globe(config(Finish::Identity, None)).expect("a globe"));
    let in_stages =
        settled_in_stages(&mut Session::globe(config(Finish::Identity, None)).expect("a globe"));
    assert!(
        read(&in_turn) > (FRAMES * GRID * GRID / 2) as usize,
        "the ground answered"
    );
    assert_eq!(in_stages.len(), in_turn.len());
    for (i, (stages, turn)) in in_stages.iter().zip(&in_turn).enumerate() {
        assert_eq!(stages.0, turn.0, "frame {i}: another selection");
        assert!(stages.1 == turn.1, "frame {i}: other heights");
    }
}

/// Baking frame after frame, as `bake_frames` did before it had stages:
/// the reference. The pack's bytes and each frame's selection.
fn baked_in_turn(
    session: &mut Session,
    packed: &PackedMirror,
    out: &std::path::Path,
) -> (Vec<u8>, Vec<u64>) {
    use tuile_pack::{BakedView, PackWriter};
    let poses: Vec<tuile_tape::Frame> = (0..FRAMES).map(pose).collect();
    let mut writer = PackWriter::new(
        String::new(),
        poses[0].position,
        out.with_extension("blob.part"),
    )
    .expect("a writer")
    .culling("full")
    .with_content(tuile_pack::Content::Embedded, "", "")
    .with_night_layer("");
    let mut selections = Vec::new();
    for (n, pose) in poses.iter().enumerate() {
        let frame = session.frame_for_pose(pose, VIEWPORT).expect("a frame");
        selections.push(session.selection_digest().expect("live"));
        let mut open = writer.begin_frame(
            n as u32 + 1,
            BakedView {
                position: pose.position,
                direction: pose.direction,
                up: pose.up,
                viewport_px: [VIEWPORT.0, VIEWPORT.1],
                fovy_rad: pose.fovy,
            },
        );
        for (index, tile) in frame.tiles.iter().enumerate() {
            if open.push_known(tile.tile.0, tile.drape()) {
                continue;
            }
            open.push(
                packed_tile(&frame, index, tile, tuile_pack::Content::Embedded)
                    .expect("a tile for the pack"),
            );
            packed.hold(tile.tile.0, tile.drape());
        }
        open.end();
    }
    writer.set_scene_digest(digest_of_scene(&poses, VIEWPORT, "settings"));
    writer.finish_to(out).expect("written");
    (std::fs::read(out).expect("read"), selections)
}

fn baked_in_stages(
    session: &mut Session,
    packed: &PackedMirror,
    out: &std::path::Path,
) -> (Vec<u8>, Vec<u64>) {
    let poses: Vec<tuile_tape::Frame> = (0..FRAMES).map(pose).collect();
    let outcome = bake_frames(
        session,
        &BakeFrames {
            first: 1,
            last: FRAMES,
            viewport: VIEWPORT,
            out,
            scene: SceneName::OfPoses {
                settings: "settings".into(),
            },
            culling: "full",
            content: &PackContent::embedded(),
            packed,
        },
        &mut Poses(&poses),
    )
    .expect("baked");
    (std::fs::read(out).expect("read"), outcome.selections)
}

/// A session over the made-up globe, its loader told what its bake's pack
/// holds — as a farm's bake is.
fn baking() -> (Session, PackedMirror) {
    let packed = PackedMirror::new();
    let mut config = config(Finish::Pictures, None);
    config.held_drape = Some(packed.held_drape());
    (Session::globe(config).expect("a globe"), packed)
}

#[test]
fn a_bake_in_stages_writes_the_pack_a_bake_frame_after_frame_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (turn, turn_selections) = {
        let (mut session, packed) = baking();
        baked_in_turn(&mut session, &packed, &dir.path().join("in-turn.tuilepack"))
    };
    let (stages, stage_selections) = {
        let (mut session, packed) = baking();
        baked_in_stages(
            &mut session,
            &packed,
            &dir.path().join("in-stages.tuilepack"),
        )
    };
    assert_eq!(stage_selections, turn_selections, "another selection");
    assert!(
        turn.len() > 10_000,
        "a pack was written: {} bytes",
        turn.len()
    );
    assert!(stages == turn, "the packs differ");
}

#[test]
fn scouting_ahead_changes_no_selection_and_no_height() {
    let without = {
        let store = Arc::new(Memory::default());
        settled_in_stages(
            &mut Session::globe(config(Finish::Identity, Some(store))).expect("a globe"),
        )
    };
    let scouted = tuile_core::metrics::metrics().scout_frames.get();
    let with = {
        let store = Arc::new(Memory::default());
        let config = config(Finish::Identity, Some(store));
        let mut session = Session::globe(config.clone()).expect("a globe");
        let scout = tuile_bake::scout::Scout::start(&config);
        // What the scouts converge: the frames' poses, unaimed — near
        // enough to fetch the tiles the bake will want.
        let first = scout.ahead((0..FRAMES).map(|i| {
            let eye = pose(i);
            let at = ecef_to_geodetic(DVec3::from_array(eye.position));
            let enu = enu_frame(at);
            let nadir = tuile_tape::Frame {
                position: eye.position,
                direction: (-enu.z_axis).to_array(),
                up: enu.y_axis.to_array(),
                fovy: std::f64::consts::FRAC_PI_4,
            };
            vec![view(&eye), view(&nadir)]
        }));
        session.keep_ground();
        let numbers: Vec<u32> = (0..FRAMES).collect();
        settle_each(
            &mut session,
            &mut GroundDrawer::new(),
            DRAWN,
            &numbers,
            |i, session| {
                scout.reached(first + i as usize);
                views(i, session)
            },
            heights,
        )
        .expect("settled")
        .into_iter()
        .map(|s| (s.selection, s.value))
        .collect::<Vec<_>>()
    };
    assert!(
        tuile_core::metrics::metrics().scout_frames.get() > scouted,
        "the scouts converged frames"
    );
    assert_eq!(with.len(), without.len());
    for (i, (with, without)) in with.iter().zip(&without).enumerate() {
        assert_eq!(with.0, without.0, "frame {i}: another selection");
        assert!(with.1 == without.1, "frame {i}: other heights");
    }
}
