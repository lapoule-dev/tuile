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
const RECORDED: &str = concat!(
    "{\"frames\":12,\"viewport\":[960,540],\"scene\":\"cb6dbb6b0f25b499\",\"pack_sha256\":\"c82c",
    "cb38cad7e7375929fe0b2967ccb46a4af6434c6be97a6735cf963e9abeb1\",\"pack_bytes\":27640",
    "8,\"survey_selections\":[\"19adb3d41723caa0\",\"b4879f3f9e6c2af7\",\"dd2d755e5c18b770\",",
    "\"d118ab7d241ba867\",\"4671d8fab96d1d5d\",\"962255ab19c23e10\",\"a24e598ad5f01791\",\"88b",
    "c520ac19b797a\",\"d5237d54142d4696\",\"d59163f861c4ae79\",\"729331bcffe91daf\",\"8e87029",
    "f9110e4bd\"],\"grounds\":[\"41ef35e9ac9beed8\",\"738744d005755792\",\"aa5b0e1dcd8a3983\",",
    "\"240f09ea8bd30951\",\"2fced8c5bf42aa5c\",\"b6254a798081b476\",\"9558ac90b68d9947\",\"278",
    "c9f78edeec081\",\"351af89c7229f063\",\"2f6cd9f30ead5e27\",\"fd0b1ade1ed9e442\",\"97e3da5",
    "c25d8da6b\"],\"bake_selections\":[\"37cf57d6f04843d4\",\"b5d21a375aae8bb4\",\"50db4c96cd",
    "ed2843\",\"e38bfeae4d803f20\",\"cb77927e21ed0c4f\",\"62cb517f577e915a\",\"e3d0e6538429dc",
    "d3\",\"62e6c88cf844d638\",\"76bd5dd09034afd8\",\"15d924223b1980a3\",\"729331bcffe91daf\",",
    "\"8e87029f9110e4bd\"]}",
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
