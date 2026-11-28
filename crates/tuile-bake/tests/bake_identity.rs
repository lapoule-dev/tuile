// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A bake by identity is the one recorded, bit for bit.**
//!
//! The memory bench's made-up globe — terrain tiles with their skirts'
//! edges and their normals, imagery described and never fetched — flown as
//! a farm's bake flies it: a survey that settles each frame and reads its
//! ground, then the bake's own frames into a pack by reference, on one
//! session that keeps little between frames, so that tiles are let go and
//! delivered again.
//!
//! What it says, one `IDENTITY` line: the scene's digest, each frame's
//! selection as the survey and the bake converged on it, each frame's
//! ground (heights read on a grid round the point under the eye, every
//! bit), and the pack's sha256. The line is compared with the one recorded
//! before the bake's memory was cut down: a change that keeps fewer bytes
//! must keep every one of these.
//!
//! `BAKE_IDENTITY_RECORD=1` prints the line without comparing it, to record
//! it again when what a bake makes is meant to change.

#[path = "bake_memory_support/mod.rs"]
mod flight;
#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use flight::{knob, pose, MadeUp, DRAWN};
use support::{LAT, LON};
use tuile_bake::bake::{bake_frames, BakeFrames, PackContent, PackedMirror, Poses, SceneName};
use tuile_bake::settle::settle_each;
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle};
use tuile_film::ground::Ground;

/// What the probe recorded on the bake as it was before its memory was cut
/// down (stack #218, on #217): the line every change must give back.
///
/// Recorded again once the geometry's `sin`/`cos`/`atan2`/`tan`/`log2` were
/// libm's rather than the platform's: Apple's libm and glibc differ in their
/// last bits, so the line first recorded on Linux had never held on a Mac.
/// The same line now comes out of both, bit for bit.
///
/// Recorded again after the budget's deliberate changes of 2026-11-24 —
/// the screen-space error priced by view depth (#323), a fine tile let go
/// only once its replacement is whole (#324), the compression of what
/// lasts (#312): the survey's and the bake's selections and so the pack
/// move; every frame's ground is the same, bit for bit.
/// And again with the skirts at the references' rule (`LEVELS_OF_REACH`
/// 0): the pack's skirt depths move, so its digest; its size, the
/// selections and every frame's ground are the same.
/// And again with each terrain tile's volume made of its parent's decoded
/// height range rather than the global -1000..9000 m: the
/// eye no longer sits inside the column over the ground under it, so that
/// ground is no longer refined to the bottom of the tree. 248 tiles a frame
/// become about 120, the deepest level 19 becomes 16, the pack 247 472
/// bytes becomes 81 384; the selections, the pack and every frame's ground
/// (read from coarser tiles) move. The same line came out of every run.
const RECORDED: &str = concat!(
    "{\"frames\":12,\"viewport\":[960,540],\"scene\":\"cb6dbb6b0f25b499\",\"pack_sha256\":\"4",
    "4aa812ad2bd9ad44f23062a635a9936e183f58f7cee0a1542ea61aece93fbf2\",\"pack_bytes\":81384,\"",
    "survey_selections\":[\"695dfb50e1629ca4\",\"fa2ea7dfb3590923\",\"d67c0f3a55b4823b\",\"d6",
    "7c0f3a55b4823b\",\"d87843c3f9c106a3\",\"0577ebb7dd71ddfc\",\"d665b9a09809e3a4\",\"40a650",
    "076e0f2011\",\"40a650076e0f2011\",\"3aa047c5e32091d5\",\"107f35b921999aaf\",\"107f35b921",
    "999aaf\"],\"grounds\":[\"caf4264263cf1ea6\",\"461d5c14c2b2ffd9\",\"43358125717c151f\",\"",
    "fb3990667f2e2b51\",\"546e2e370b959614\",\"09cec59def24161f\",\"8519c892c66071de\",\"3165",
    "263121bb4880\",\"8437f7cd2e296373\",\"ff6cbf5dd127a039\",\"ccc00947a070c350\",\"91ec0424",
    "3af1a96c\"],\"bake_selections\":[\"d230e4cf0d49dd86\",\"792210c85473191b\",\"dbc11687c65",
    "471cb\",\"dbc11687c65471cb\",\"0577ebb7dd71ddfc\",\"0577ebb7dd71ddfc\",\"d665b9a09809e3a",
    "4\",\"40a650076e0f2011\",\"40a650076e0f2011\",\"3aa047c5e32091d5\",\"107f35b921999aaf\",",
    "\"107f35b921999aaf\"]}",
);

/// Places a side the ground's height is read at, each frame.
const GRID: u32 = 8;

/// Every bit of the heights of `ground` on a grid round the point under
/// frame `i`'s eye.
fn ground_digest(i: u32, ground: &Ground) -> u64 {
    let lat = LAT + f64::from(i) * 0.00036;
    let f = |n: u32| (f64::from(n) / f64::from(GRID - 1) - 0.5) * 0.004;
    let mut bits = Vec::new();
    for a in 0..GRID {
        for b in 0..GRID {
            let (lon, lat) = ((LON + f(a)).to_radians(), (lat + f(b) + 0.002).to_radians());
            bits.push(ground.height_at(lon, lat).map_or(u64::MAX, f64::to_bits));
        }
    }
    in_order(&bits)
}

/// FNV-1a over `values` in their order (`determinism::digest` sorts).
fn in_order(values: &[u64]) -> u64 {
    values
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

fn hex(digests: &[u64]) -> String {
    digests
        .iter()
        .map(|d| format!("\"{d:016x}\""))
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn a_bake_by_identity_is_the_one_recorded() {
    let frames: u32 = knob("BAKE_IDENTITY_FRAMES", 12);
    let viewport = (
        knob("BAKE_IDENTITY_WIDTH", 960.0),
        knob("BAKE_IDENTITY_HEIGHT", 540.0),
    );
    let packed = PackedMirror::new();
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp { steps: 32 })));
    config.session.traversal.maximum_screen_space_error = 3.0;
    config.session.finish = Finish::Identity;
    config.held_drape = Some(packed.held_drape());
    // Little kept between frames: tiles are let go and delivered again.
    config.session.release_beyond = Some(knob("BAKE_IDENTITY_KEEP_MIB", 48u64) << 20);
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    let mut drawer = GroundDrawer::new();

    // The survey: each frame settled, its ground read.
    let numbers: Vec<u32> = (0..frames).collect();
    let surveyed = settle_each(
        &mut session,
        &mut drawer,
        DRAWN,
        &numbers,
        |i, _: &Session| vec![pose_view(&pose(i), viewport)],
        |i, ground: &Arc<Ground>| ground_digest(i, ground),
        |_| false,
    )
    .expect("settled");
    let survey_selections: Vec<u64> = surveyed.iter().map(|s| s.selection).collect();
    let grounds: Vec<u64> = surveyed.iter().map(|s| s.value).collect();

    // The bake, into a pack by reference.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("probe.tuilepack");
    let poses: Vec<tuile_tape::Frame> = (0..frames).map(pose).collect();
    let content = PackContent {
        content: tuile_pack::Content::References,
        terrain_layer: "terrain".into(),
        imagery_layer: "imagery".into(),
        night_layer: String::new(),
    };
    let outcome = bake_frames(
        &mut session,
        &BakeFrames {
            first: 1,
            last: frames,
            viewport,
            out: &out,
            scene: SceneName::OfPoses {
                settings: "identity probe".into(),
            },
            culling: "full",
            content: &content,
            packed: &packed,
        },
        &mut Poses(&poses),
    )
    .expect("baked");
    let pack = std::fs::read(&out).expect("the pack");
    // Every tile record, one line each, when asked: to tell two packs apart.
    if let Ok(dump) = std::env::var("BAKE_IDENTITY_DUMP") {
        let opened = tuile_pack::Pack::open(&pack).expect("a pack");
        let mut lines = Vec::new();
        for n in 1..=frames {
            for t in opened.frame(n).expect("a frame") {
                lines.push(format!(
                    "{n} {} {:x} {} {} {:?} {:?} {:?}",
                    t.id(),
                    t.drape(),
                    t.vertex_count(),
                    t.index_count(),
                    t.origin_ecef().map(|v| v.iter().collect::<Vec<_>>()),
                    t.base_color_factor().map(|v| v.iter().collect::<Vec<_>>()),
                    tuile_pack::refs_of(&t),
                ));
            }
        }
        std::fs::write(dump, lines.join("\n")).expect("dumped");
    }
    let sha256 = {
        use sha2::Digest;
        sha2::Sha256::digest(&pack)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };

    let line = format!(
        "{{\"frames\":{frames},\"viewport\":[{},{}],\"scene\":\"{}\",\"pack_sha256\":\"{sha256}\",\"pack_bytes\":{},\
\"survey_selections\":[{}],\"grounds\":[{}],\"bake_selections\":[{}]}}",
        viewport.0,
        viewport.1,
        outcome.scene,
        pack.len(),
        hex(&survey_selections),
        hex(&grounds),
        hex(&outcome.selections),
    );
    eprintln!("IDENTITY {line}");
    assert!(
        grounds
            .iter()
            .all(|g| *g != in_order(&[u64::MAX; (GRID * GRID) as usize])),
        "every frame's ground answered"
    );
    let default_flight = frames == 12 && viewport == (960.0, 540.0);
    if std::env::var_os("BAKE_IDENTITY_RECORD").is_none() && default_flight {
        assert_eq!(line, RECORDED, "the bake is not the one recorded");
    }
}
