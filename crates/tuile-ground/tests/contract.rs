// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground contract on the reference heightfield, in a planar and in a
//! geodetic frame: query semantics, edits, ordering, versions, change
//! reports, water, and determinism properties.

// Helpers outside `#[test]` functions unwrap too: a test says what it
// expects by panicking.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use glam::{DVec2, DVec3};
use proptest::prelude::*;
use tuile_ground::*;

/// A 4 km abstract world: 257 samples 16 m apart, tiles versioned at level
/// 4 (256 m).
fn planar() -> PlanarFrame {
    PlanarFrame {
        half_extent: 2048.0,
        edit_level: 4,
        anchor: None,
    }
}

/// About 4 km × 4 km of globe around 45° N.
fn geodetic() -> GeodeticFrame {
    let lat = 45f64.to_radians();
    let dlat = 4096.0 / EARTH_RADIUS;
    let dlon = dlat / lat.cos();
    GeodeticFrame {
        min: DVec2::new(0.0, lat),
        max: DVec2::new(dlon, lat + dlat),
        edit_level: 4,
    }
}

fn flat<F: Frame>(frame: F, h: f64) -> Heightfield<F> {
    Heightfield::new(frame, 257, move |_, _| h).unwrap()
}

/// Rises 2 m per sample along `u`: `(east + 2048) / 8` metres.
fn ramp<F: Frame>(frame: F) -> Heightfield<F> {
    Heightfield::new(frame, 257, |i, _| 2.0 * i as f64).unwrap()
}

/// The ramp's height `east` metres from the centre.
fn ramp_height(east: f64) -> f64 {
    (east + 2048.0) / 8.0
}

/// The point `east` metres east and `north` metres north of the frame's
/// centre (measured with the metric there), at `z`.
fn at<F: Frame>(frame: &F, east: f64, north: f64, z: f64) -> DVec3 {
    let (lo, hi) = frame.bounds();
    let c = (lo + hi) / 2.0;
    let m = frame.metres_per_unit(c);
    DVec3::new(c.x + east / m.x, c.y + north / m.y, z)
}

fn surface(g: &impl Ground, p: DVec3) -> Option<Surface> {
    let mut out = [None];
    g.surface(&[p], &mut out).unwrap();
    out[0]
}

fn top(g: &impl Ground, c: DVec2) -> Option<f64> {
    surface(g, c.extend(f64::INFINITY)).map(|s| s.height)
}

fn cast(g: &impl Ground, from: DVec3, to: DVec3) -> Option<Hit> {
    let mut hit = [None];
    g.raycast(&[Ray { from, to }], &mut hit).unwrap();
    hit[0]
}

fn sphere(seq: u64, op: EditOp, centre: DVec3, radius: f64) -> Edit {
    Edit {
        seq: EditSeq(seq),
        op,
        brush: Brush::Sphere { centre, radius },
    }
}

fn a_flat_ground_is_flat_and_up<F: Frame + Copy>(frame: F) {
    let g = flat(frame, 10.0);
    let s = surface(&g, at(&frame, 100.0, -300.0, 50.0)).expect("above the ground");
    assert_eq!(s.height, 10.0);
    assert_eq!(s.normal, DVec3::Z);
    assert_eq!(
        surface(&g, at(&frame, 100.0, -300.0, 10.0)).map(|s| s.height),
        Some(10.0),
        "a probe exactly at the surface stands on it"
    );
    assert_eq!(
        surface(&g, at(&frame, 100.0, -300.0, 5.0)),
        None,
        "a probe inside the ground has no surface below it"
    );
    assert_eq!(
        surface(&g, at(&frame, 9000.0, 0.0, 50.0)),
        None,
        "outside the bounds"
    );
    let (_, hi) = frame.bounds();
    assert_eq!(top(&g, hi), None, "the max edge is outside");
}

#[test]
fn flat_in_both_frames() {
    a_flat_ground_is_flat_and_up(planar());
    a_flat_ground_is_flat_and_up(geodetic());
}

#[test]
fn heights_interpolate_and_normals_lean_downhill() {
    let s = surface(&ramp(planar()), at(&planar(), 8.0, 0.0, 1000.0)).expect("planar");
    // Half way between samples 128 (256 m) and 129 (258 m).
    assert_eq!(s.height, 257.0);
    assert!(
        s.normal.x < -0.1 && s.normal.z > 0.9,
        "the normal leans downhill, toward -u: {:?}",
        s.normal
    );
    // The same ramp on the globe: 2 m per ~16 m too, so the same slope.
    let g = ramp(geodetic());
    let s = surface(&g, at(&geodetic(), 8.0, 0.0, 1000.0)).expect("geodetic");
    assert!(
        (s.normal.x - (-0.124)).abs() < 0.01,
        "a metric slope, not a slope per radian: {:?}",
        s.normal
    );
}

#[test]
fn batches_answer_each_query_and_refuse_mismatched_slices() {
    let g = ramp(planar());
    let probes: Vec<DVec3> = (0..50)
        .map(|k| {
            at(
                &planar(),
                -2000.0 + 80.0 * f64::from(k),
                7.0 * f64::from(k),
                2000.0,
            )
        })
        .collect();
    let mut batch = vec![None; probes.len()];
    g.surface(&probes, &mut batch).unwrap();
    for (p, b) in probes.iter().zip(&batch) {
        assert_eq!(*b, surface(&g, *p));
    }
    let mut short = vec![None; 3];
    assert_eq!(
        g.surface(&probes, &mut short),
        Err(BatchMismatch {
            queries: 50,
            answers: 3
        })
    );
}

#[test]
fn spans_solid_and_raycasts_agree_with_the_surface() {
    let g = flat(planar(), 10.0);
    let mut spans = SpanBuffer::default();
    g.spans(&[DVec2::ZERO, DVec2::new(9000.0, 0.0)], &mut spans);
    assert_eq!(
        spans.of(0),
        &[Span {
            bottom: f64::NEG_INFINITY,
            top: 10.0
        }]
    );
    assert!(spans.of(1).is_empty(), "outside the bounds: no span");

    let mut solid = [false; 3];
    g.solid(
        &[
            DVec3::new(0.0, 0.0, 9.0),
            DVec3::new(0.0, 0.0, 10.0),
            DVec3::new(0.0, 0.0, 11.0),
        ],
        &mut solid,
    )
    .unwrap();
    assert_eq!(solid, [true, true, false], "solid is closed");

    let down = cast(&g, DVec3::new(0.0, 0.0, 50.0), DVec3::new(0.0, 0.0, -50.0))
        .expect("straight down hits");
    assert!((down.at.z - 10.0).abs() < 1e-9, "{down:?}");
    assert!((down.fraction - 0.4).abs() < 1e-12, "{down:?}");
    assert_eq!(
        cast(
            &g,
            DVec3::new(-500.0, 0.0, 20.0),
            DVec3::new(500.0, 0.0, 20.0)
        ),
        None,
        "a level line above the ground is clear: line of sight"
    );
    let inside = cast(&g, DVec3::new(0.0, 0.0, 5.0), DVec3::new(0.0, 0.0, 50.0)).unwrap();
    assert_eq!(inside.fraction, 0.0, "a ray starting in solid hits at once");
    assert_eq!(
        cast(
            &g,
            DVec3::new(5000.0, 0.0, 5.0),
            DVec3::new(6000.0, 0.0, -50.0)
        ),
        None,
        "no ground outside the bounds"
    );
}

#[test]
fn a_ridge_blocks_the_line_of_sight() {
    // A 60 m wall along the sample column at the centre.
    let g = Heightfield::new(planar(), 257, |i, _| if i == 128 { 60.0 } else { 0.0 }).unwrap();
    let hit = cast(
        &g,
        DVec3::new(-300.0, 0.0, 20.0),
        DVec3::new(300.0, 0.0, 20.0),
    );
    assert!(hit.is_some(), "the wall stands in the way");
}

/// A one-sample tent, 60 m high, and a ray at 50 m: the ray meets the tent's
/// flank 16/6 m before its peak. A march at half a cell (8 m) lands 4 m
/// either side of the peak, where the tent is 45 m high, and sees nothing.
#[test]
fn a_one_sample_peak_is_never_stepped_over() {
    let g = Heightfield::new(planar(), 257, |i, _| if i == 128 { 60.0 } else { 0.0 }).unwrap();
    let hit = cast(
        &g,
        DVec3::new(-300.0, 0.0, 50.0),
        DVec3::new(300.0, 0.0, 50.0),
    )
    .expect("the peak stands in the way");
    assert!(
        (hit.at.x - (-16.0 / 6.0)).abs() < 1e-9,
        "the first entry, on the tent's west flank: {hit:?}"
    );
    // Diagonally across a cell whose one corner is raised, the patch is a
    // saddle: the segment meets it where the quadratic says.
    let corner = Heightfield::new(
        planar(),
        257,
        |i, j| {
            if i == 129 && j == 129 {
                64.0
            } else {
                0.0
            }
        },
    )
    .unwrap();
    // Along the diagonal of cell (128, 128), fx = fy = s: h = 64 s².
    // At z = 16: s = 1/2, the cell's centre (8, 8).
    let hit = cast(
        &corner,
        DVec3::new(0.0, 0.0, 16.0),
        DVec3::new(16.0, 16.0, 16.0),
    )
    .expect("the saddle rises to the ray");
    assert!(
        (hit.at.truncate() - DVec2::splat(8.0)).length() < 1e-9,
        "{hit:?}"
    );
}

#[test]
fn water_reports_its_body_level_and_depth() {
    let bodies = vec![
        WaterBody {
            kind: WaterKind::Sea,
            level: 0.0,
        },
        WaterBody {
            kind: WaterKind::Lake,
            level: 40.0,
        },
    ];
    // Sea west of the centre over a floor at -10 m; a lake bed at 30 m east.
    let g = Heightfield::new(planar(), 257, |i, _| if i < 128 { -10.0 } else { 30.0 })
        .unwrap()
        .with_water(bodies, |i, _| {
            Some(if i < 128 {
                WaterBodyId(0)
            } else {
                WaterBodyId(1)
            })
        })
        .unwrap();
    let mut out = [None; 2];
    g.water(
        &[DVec2::new(-1000.0, 0.0), DVec2::new(1000.0, 0.0)],
        &mut out,
    )
    .unwrap();
    assert_eq!(
        out[0],
        Some(WaterAt {
            body: WaterBodyId(0),
            level: 0.0,
            depth: 10.0
        })
    );
    assert_eq!(
        out[1],
        Some(WaterAt {
            body: WaterBodyId(1),
            level: 40.0,
            depth: 10.0
        })
    );
    assert_eq!(
        g.water_bodies()[1].level,
        40.0,
        "the id is the index into the bodies"
    );
    // A dry column: the body's level is under the ground.
    let dry = flat(planar(), 5.0)
        .with_water(
            vec![WaterBody {
                kind: WaterKind::Sea,
                level: 0.0,
            }],
            |_, _| Some(WaterBodyId(0)),
        )
        .unwrap();
    let mut out = [None];
    dry.water(&[DVec2::ZERO], &mut out).unwrap();
    assert_eq!(out[0], None);
}

#[test]
fn grounds_refuse_what_they_cannot_hold() {
    assert_eq!(
        Heightfield::new(planar(), 1, |_, _| 0.0).err(),
        Some(InvalidGround::Samples(1))
    );
    assert_eq!(
        Heightfield::new(planar(), usize::MAX, |_, _| 0.0).err(),
        Some(InvalidGround::Samples(usize::MAX))
    );
    assert_eq!(
        Heightfield::new(planar(), 9, |_, _| 0.0).err(),
        Some(InvalidGround::EditLevelTooFine { level: 4, cells: 8 })
    );
    assert_eq!(
        Heightfield::new(planar(), 17, |i, j| if (i, j) == (3, 4) {
            f64::INFINITY
        } else {
            0.0
        })
        .err(),
        Some(InvalidGround::Height { i: 3, j: 4 })
    );
    let bad_frame = PlanarFrame {
        edit_level: 40,
        ..planar()
    };
    assert_eq!(
        Heightfield::new(bad_frame, 17, |_, _| 0.0).err(),
        Some(InvalidGround::Frame(InvalidFrame::Level(40)))
    );
    let sea = WaterBody {
        kind: WaterKind::Sea,
        level: 0.0,
    };
    assert_eq!(
        flat(planar(), 0.0)
            .with_water(vec![sea], |_, _| Some(WaterBodyId(1)))
            .err(),
        Some(InvalidGround::WaterBody {
            i: 0,
            j: 0,
            body: 1
        })
    );
    assert_eq!(
        flat(planar(), 0.0)
            .with_water(
                vec![WaterBody {
                    level: f64::NAN,
                    ..sea
                }],
                |_, _| None
            )
            .err(),
        Some(InvalidGround::WaterLevel(0))
    );
}

fn edits_carve_raise_and_version<F: Frame + Copy>(frame: F) {
    let mut g = flat(frame, 10.0);
    let mut changed = Vec::new();
    let crater = sphere(1, EditOp::Remove, at(&frame, 0.0, 0.0, 10.0), 20.0);
    g.apply(&crater, &mut changed).unwrap();
    let floor = top(&g, at(&frame, 0.0, 0.0, 0.0).truncate()).unwrap();
    assert!(
        (floor - (-10.0)).abs() < 1e-6,
        "the crater's floor: {floor}"
    );
    assert_eq!(
        top(&g, at(&frame, 300.0, 0.0, 0.0).truncate()),
        Some(10.0),
        "untouched further away"
    );
    // The crater straddles the four tiles around the centre.
    assert_eq!(changed.len(), 4, "{changed:?}");
    assert!(changed
        .iter()
        .all(|c| c.version == Version(1) && g.version(c.tile) == Version(1)));

    let mut changed = Vec::new();
    // Inside one tile (256 m tiles from the min corner: 512..768 m east and
    // north of the centre), more than a cell from its edges.
    let mound = Edit {
        seq: EditSeq(2),
        op: EditOp::Add,
        brush: Brush::Box {
            min: at(&frame, 540.0, 540.0, 0.0),
            max: at(&frame, 600.0, 600.0, 30.0),
        },
    };
    g.apply(&mound, &mut changed).unwrap();
    assert_eq!(
        top(&g, at(&frame, 570.0, 570.0, 0.0).truncate()),
        Some(30.0)
    );
    assert_eq!(changed.len(), 1, "{changed:?}");
}

#[test]
fn edits_in_both_frames() {
    edits_carve_raise_and_version(planar());
    edits_carve_raise_and_version(geodetic());
}

/// A changed sample on a tile edge changes the cell on the other side of
/// the edge too, which interpolates it: that tile is reported even though
/// the brush lies wholly in the first one.
#[test]
fn a_change_reports_every_tile_whose_answers_move() {
    let mut g = flat(planar(), 10.0);
    // Tiles are 256 m from -2048: an edge, and a sample line, at x = -1792.
    let west_column = DVec2::new(-1800.0, 100.0);
    let before = top(&g, west_column);
    let mut changed = Vec::new();
    // A box from the edge eastwards, in the east tile only.
    g.apply(
        &Edit {
            seq: EditSeq(1),
            op: EditOp::Add,
            brush: Brush::Box {
                min: DVec3::new(-1792.0, 90.0, 0.0),
                max: DVec3::new(-1788.0, 110.0, 30.0),
            },
        },
        &mut changed,
    )
    .unwrap();
    assert_ne!(
        before,
        top(&g, west_column),
        "the cell west of the edge moved"
    );
    let west = g.frame().tile_of(west_column, 4).unwrap();
    assert!(
        changed.iter().any(|c| c.tile == west),
        "the tile west of the edge is reported: {changed:?}"
    );
}

#[test]
fn an_edit_that_changes_nothing_reports_nothing() {
    let mut g = flat(planar(), 10.0);
    let before = top(&g, DVec2::ZERO);
    let mut changed = Vec::new();
    // A tunnel under the surface and a floating rock: no column changes.
    g.apply(
        &sphere(1, EditOp::Remove, DVec3::new(0.0, 0.0, -20.0), 5.0),
        &mut changed,
    )
    .unwrap();
    g.apply(
        &sphere(2, EditOp::Add, DVec3::new(0.0, 0.0, 60.0), 5.0),
        &mut changed,
    )
    .unwrap();
    assert_eq!(top(&g, DVec2::ZERO), before);
    assert!(changed.is_empty(), "nothing changed: {changed:?}");
    assert_eq!(g.last_seq(), EditSeq(2), "both numbers consumed");
}

#[test]
fn a_vertical_shaft_carves_its_whole_depth() {
    let mut g = flat(planar(), 0.0);
    g.apply(
        &Edit {
            seq: EditSeq(1),
            op: EditOp::Remove,
            brush: Brush::Capsule {
                a: DVec3::new(0.0, 0.0, 0.0),
                b: DVec3::new(0.0, 0.0, -50.0),
                radius: 2.0,
            },
        },
        &mut Vec::new(),
    )
    .unwrap();
    let mut spans = SpanBuffer::default();
    g.spans(&[DVec2::ZERO], &mut spans);
    assert_eq!(spans.of(0)[0].top, -52.0, "the shaft's rounded bottom");
}

#[test]
fn edits_follow_a_contiguous_order() {
    let mut g = flat(planar(), 0.0);
    let mut changed = Vec::new();
    let e = |seq| sphere(seq, EditOp::Remove, DVec3::ZERO, 30.0);
    assert_eq!(g.last_seq(), EditSeq(0));
    assert_eq!(
        g.apply(&e(2), &mut changed),
        Err(GroundError::Gap {
            expected: EditSeq(1),
            got: EditSeq(2)
        }),
        "a fresh ground starts at 1"
    );
    assert_eq!(g.last_seq(), EditSeq(0), "a gap consumes nothing");
    g.apply(&e(1), &mut changed).unwrap();
    let (digest, n) = (g.digest(), changed.len());
    assert_eq!(
        g.apply(&e(1), &mut changed),
        Err(GroundError::Stale {
            last: EditSeq(1),
            got: EditSeq(1)
        })
    );
    assert_eq!(
        g.apply(&e(0), &mut changed),
        Err(GroundError::Stale {
            last: EditSeq(1),
            got: EditSeq(0)
        })
    );
    assert_eq!(
        g.apply(&e(3), &mut changed),
        Err(GroundError::Gap {
            expected: EditSeq(2),
            got: EditSeq(3)
        }),
        "edit 2 was lost: 3 waits"
    );
    assert_eq!(
        (g.digest(), changed.len()),
        (digest, n),
        "refused out of order: no trace at all"
    );
    for err in [
        GroundError::Stale {
            last: EditSeq(1),
            got: EditSeq(1),
        },
        GroundError::Gap {
            expected: EditSeq(2),
            got: EditSeq(3),
        },
    ] {
        assert!(!err.consumes_seq());
    }
}

/// Refusals for content are the same on every replica: they consume their
/// number, append nothing and change nothing else.
#[test]
fn content_refusals_consume_their_number() {
    let p = DVec3::new(0.0, 0.0, 0.0);
    let brushes = [
        (
            Brush::Sphere {
                centre: DVec3::new(90_000.0, 0.0, 0.0),
                radius: 10.0,
            },
            GroundError::OutOfBounds,
        ),
        (
            Brush::Sphere {
                centre: DVec3::new(f64::NAN, 0.0, 0.0),
                radius: 10.0,
            },
            GroundError::InvalidBrush,
        ),
        (
            Brush::Sphere {
                centre: DVec3::new(0.0, 0.0, f64::NAN),
                radius: 10.0,
            },
            GroundError::InvalidBrush,
        ),
        (
            Brush::Sphere {
                centre: p,
                radius: -5.0,
            },
            GroundError::InvalidBrush,
        ),
        (
            Brush::Box {
                min: p,
                max: DVec3::new(10.0, 10.0, f64::INFINITY),
            },
            GroundError::InvalidBrush,
        ),
        (
            Brush::Box {
                min: DVec3::new(-10.0, -10.0, f64::NEG_INFINITY),
                max: DVec3::new(10.0, 10.0, 5.0),
            },
            GroundError::InvalidBrush,
        ),
        (
            Brush::Box {
                min: DVec3::new(10.0, 10.0, 0.0),
                max: DVec3::new(-10.0, -10.0, 5.0),
            },
            GroundError::InvalidBrush,
        ),
        (
            Brush::Capsule {
                a: p,
                b: DVec3::new(0.0, 0.0, f64::INFINITY),
                radius: 1.0,
            },
            GroundError::InvalidBrush,
        ),
    ];
    for (k, (brush, err)) in brushes.into_iter().enumerate() {
        let mut g = flat(planar(), 0.0);
        let reference = flat(planar(), 0.0);
        let mut changed = Vec::new();
        let edit = Edit {
            seq: EditSeq(1),
            op: EditOp::Add,
            brush,
        };
        assert_eq!(g.apply(&edit, &mut changed), Err(err.clone()), "{k}");
        assert!(err.consumes_seq());
        assert!(changed.is_empty(), "{k}");
        assert_eq!(g.last_seq(), EditSeq(1), "{k}: the number is consumed");
        let probes: Vec<DVec3> = (-20..20)
            .map(|x| DVec3::new(f64::from(x) * 7.0, 3.0, f64::INFINITY))
            .collect();
        let (mut a, mut b) = (vec![None; 40], vec![None; 40]);
        g.surface(&probes, &mut a).unwrap();
        reference.surface(&probes, &mut b).unwrap();
        assert_eq!(a, b, "{k}: nothing else changed");
        // And the next edit goes on.
        g.apply(&sphere(2, EditOp::Add, p, 5.0), &mut changed)
            .unwrap();
    }
}

#[test]
fn the_digest_covers_the_frame_and_the_water() {
    let sea = |level| {
        flat(planar(), 0.0)
            .with_water(
                vec![WaterBody {
                    kind: WaterKind::Sea,
                    level,
                }],
                |_, _| None,
            )
            .unwrap()
    };
    assert_ne!(sea(1.0).digest(), sea(2.0).digest(), "a body's level");
    let lake = flat(planar(), 0.0)
        .with_water(
            vec![WaterBody {
                kind: WaterKind::Lake,
                level: 1.0,
            }],
            |_, _| None,
        )
        .unwrap();
    assert_ne!(sea(1.0).digest(), lake.digest(), "a body's kind");
    let wider = PlanarFrame {
        half_extent: 4096.0,
        ..planar()
    };
    assert_ne!(
        flat(planar(), 0.0).digest(),
        flat(wider, 0.0).digest(),
        "the frame's bounds"
    );
    let coarser = PlanarFrame {
        edit_level: 3,
        ..planar()
    };
    assert_ne!(
        flat(planar(), 0.0).digest(),
        flat(coarser, 0.0).digest(),
        "the edit level"
    );
}

// ---------------------------------------------------------------------------
// Properties, on both frames.

/// An edit in metres around the frame's centre, realised in a frame on the
/// ramp: the brush sits on the ramp's surface, give or take `dz`, so most
/// edits change the ground.
#[derive(Debug, Clone, Copy)]
struct Spec {
    east: f64,
    north: f64,
    dz: f64,
    r: f64,
    slope: f64,
    add: bool,
    kind: u8,
}

/// A coordinate in metres from the centre: anywhere, within a cell or two
/// of a tile edge (every 256 m), or exactly on one — where change reports
/// are decided (a box starting exactly on an edge changes the cell west of
/// it).
fn arb_offset() -> impl Strategy<Value = f64> {
    prop_oneof![
        -1900.0f64..1900.0,
        (-7i32..8, -40.0f64..40.0).prop_map(|(k, d)| 256.0 * f64::from(k) + d),
        (-7i32..8).prop_map(|k| 256.0 * f64::from(k)),
    ]
}

fn arb_spec() -> impl Strategy<Value = Spec> {
    (
        arb_offset(),
        arb_offset(),
        -25.0f64..25.0,
        3.0f64..60.0,
        -1.5f64..1.5,
        any::<bool>(),
        0u8..20,
    )
        .prop_map(|(east, north, dz, r, slope, add, kind)| Spec {
            east,
            north,
            dz,
            r,
            slope,
            add,
            kind,
        })
}

fn realise<F: Frame>(frame: &F, s: Spec, seq: u64) -> Edit {
    let z = ramp_height(s.east) + s.dz;
    let p = at(frame, s.east, s.north, z);
    let brush = match s.kind {
        0..=5 => Brush::Sphere {
            centre: p,
            radius: s.r,
        },
        6..=11 => Brush::Capsule {
            a: p,
            b: at(frame, s.east + s.r, s.north - s.r, z + s.slope * s.r),
            radius: s.r / 3.0 + 1.0,
        },
        12..=17 => Brush::Box {
            min: at(frame, s.east, s.north, z - s.r),
            max: at(frame, s.east + s.r, s.north + s.r / 2.0, z),
        },
        // Refused for content: out of bounds, then not a volume.
        18 => Brush::Sphere {
            centre: at(frame, 9000.0, s.north, z),
            radius: s.r,
        },
        _ => Brush::Sphere {
            centre: p,
            radius: -s.r,
        },
    };
    Edit {
        seq: EditSeq(seq),
        op: if s.add { EditOp::Add } else { EditOp::Remove },
        brush,
    }
}

fn script<F: Frame>(frame: &F, specs: &[Spec]) -> Vec<Edit> {
    specs
        .iter()
        .enumerate()
        .map(|(k, s)| realise(frame, *s, k as u64 + 1))
        .collect()
}

/// Columns over the whole frame: an irregular grid, and every tile corner
/// with the column just before it (where off-by-ones live).
fn probe_columns<F: Frame>(frame: &F) -> Vec<DVec2> {
    let mut out = Vec::new();
    for i in 0..48 {
        for j in 0..48 {
            out.push(
                at(
                    frame,
                    -2040.0 + 85.1 * f64::from(i),
                    -2040.0 + 84.7 * f64::from(j),
                    0.0,
                )
                .truncate(),
            );
        }
    }
    let level = frame.edit_level();
    for x in 0..1u32 << level {
        for y in 0..1u32 << level {
            let (lo, _) = frame.tile_bounds(TileKey { level, x, y }).unwrap();
            out.push(lo);
            out.push(DVec2::new(lo.x.next_down(), lo.y.next_down()));
            out.push(DVec2::new(lo.x, lo.y.next_down()));
        }
    }
    out
}

/// Every answer a column gives to point queries, as bits.
fn answers(g: &impl Ground, columns: &[DVec2]) -> Vec<[u64; 6]> {
    let n = columns.len();
    let high: Vec<DVec3> = columns.iter().map(|c| c.extend(f64::INFINITY)).collect();
    let mut s = vec![None; n];
    g.surface(&high, &mut s).unwrap();
    let mid: Vec<DVec3> = columns.iter().map(|c| c.extend(200.0)).collect();
    let mut solid = vec![false; n];
    g.solid(&mid, &mut solid).unwrap();
    let mut w = vec![None; n];
    g.water(columns, &mut w).unwrap();
    (0..n)
        .map(|k| {
            let (h, nx, ny) = s[k].map_or((u64::MAX, 0, 0), |s| {
                (
                    s.height.to_bits(),
                    s.normal.x.to_bits(),
                    s.normal.y.to_bits(),
                )
            });
            [
                h,
                nx,
                ny,
                u64::from(solid[k]),
                w[k].map_or(u64::MAX, |w| w.depth.to_bits()),
                w[k].map_or(u64::MAX, |w| u64::from(w.body.0)),
            ]
        })
        .collect()
}

/// The ramp with a sea over its low half, so water answers move too.
fn wet_ramp<F: Frame>(frame: F) -> Heightfield<F> {
    ramp(frame)
        .with_water(
            vec![WaterBody {
                kind: WaterKind::Sea,
                level: 100.0,
            }],
            |i, _| (i < 100).then_some(WaterBodyId(0)),
        )
        .unwrap()
}

fn replicas_agree<F: Frame + Copy>(frame: F, specs: &[Spec]) -> Result<(), TestCaseError> {
    let edits = script(&frame, specs);
    let (mut a, mut b) = (ramp(frame), ramp(frame));
    let (mut ca, mut cb) = (Vec::new(), Vec::new());
    for e in &edits {
        prop_assert_eq!(a.apply(e, &mut ca), b.apply(e, &mut cb));
    }
    prop_assert_eq!(a.last_seq(), EditSeq(edits.len() as u64));
    prop_assert_eq!(a.digest(), b.digest());
    prop_assert_eq!(ca, cb);
    Ok(())
}

/// Versions grow by exactly one on every reported tile, are reported once
/// and in order, and never move on any other tile.
fn versions_bump_by_one<F: Frame + Copy>(frame: F, specs: &[Spec]) -> Result<(), TestCaseError> {
    let mut g = ramp(frame);
    let level = frame.edit_level();
    let all: Vec<TileKey> = (0..1u32 << level)
        .flat_map(|y| (0..1u32 << level).map(move |x| TileKey { level, x, y }))
        .collect();
    for e in &script(&frame, specs) {
        let before: BTreeMap<TileKey, Version> = all.iter().map(|k| (*k, g.version(*k))).collect();
        let mut changed = Vec::new();
        let _ = g.apply(e, &mut changed);
        prop_assert!(changed
            .windows(2)
            .all(|w| (w[0].tile.y, w[0].tile.x) < (w[1].tile.y, w[1].tile.x)));
        for k in &all {
            let reported = changed.iter().find(|c| c.tile == *k);
            match reported {
                Some(c) => {
                    prop_assert_eq!(c.version.0, before[k].0 + 1);
                    prop_assert_eq!(g.version(*k), c.version);
                }
                None => prop_assert_eq!(g.version(*k), before[k]),
            }
        }
    }
    Ok(())
}

/// The change report is complete: a column in an unreported tile answers
/// every point query bit for bit as before the edit.
fn changes_are_local<F: Frame + Copy>(frame: F, specs: &[Spec]) -> Result<(), TestCaseError> {
    let mut g = wet_ramp(frame);
    let columns = probe_columns(&frame);
    let tiles: Vec<Option<TileKey>> = columns
        .iter()
        .map(|c| frame.tile_of(*c, frame.edit_level()))
        .collect();
    let mut before = answers(&g, &columns);
    for e in &script(&frame, specs) {
        let mut changed = Vec::new();
        let _ = g.apply(e, &mut changed);
        let after = answers(&g, &columns);
        for k in 0..columns.len() {
            let reported = tiles[k].is_some_and(|t| changed.iter().any(|c| c.tile == t));
            if !reported {
                prop_assert_eq!(
                    before[k],
                    after[k],
                    "column {:?} of unreported tile {:?} changed under {:?}",
                    columns[k],
                    tiles[k],
                    e
                );
            }
        }
        before = after;
    }
    Ok(())
}

/// Whatever order and duplication the edits arrive in, each delivered once
/// in that order, a replica's state is the state after a prefix of the
/// script: it never applies an edit past a missing one.
fn deliveries_leave_a_prefix<F: Frame + Copy>(
    frame: F,
    specs: &[Spec],
    delivery: &[usize],
) -> Result<(), TestCaseError> {
    let edits = script(&frame, specs);
    let mut g = ramp(frame);
    for &k in delivery {
        let e = &edits[k % edits.len()];
        let (last, digest) = (g.last_seq(), g.digest());
        match g.apply(e, &mut Vec::new()) {
            Err(GroundError::Stale { .. } | GroundError::Gap { .. }) => {
                prop_assert_eq!((g.last_seq(), g.digest()), (last, digest));
            }
            _ => prop_assert_eq!(g.last_seq(), EditSeq(last.0 + 1)),
        }
    }
    let applied = usize::try_from(g.last_seq().0).unwrap();
    let mut reference = ramp(frame);
    for e in &edits[..applied] {
        let _ = reference.apply(e, &mut Vec::new());
    }
    prop_assert_eq!(g.digest(), reference.digest());
    Ok(())
}

/// The raycast agrees with the surface: no point of the segment before the
/// hit is under the ground, the hit is on it, and a clear segment stays
/// above it all the way.
fn rays_agree_with_the_surface<F: Frame + Copy>(
    frame: F,
    specs: &[Spec],
    rays: &[(f64, f64, f64, f64, f64, f64)],
) -> Result<(), TestCaseError> {
    let mut g = ramp(frame);
    for e in &script(&frame, specs) {
        let _ = g.apply(e, &mut Vec::new());
    }
    let tolerance = 1e-6;
    for &(e1, n1, z1, e2, n2, z2) in rays {
        let from = at(&frame, e1, n1, ramp_height(e1) + z1);
        let to = at(&frame, e2, n2, ramp_height(e2) + z2);
        let hit = cast(&g, from, to);
        let end = hit.map_or(1.0, |h| h.fraction);
        // Strictly before the hit (a segment starting in solid hits at 0).
        for s in 0..400 {
            let t = end * f64::from(s) / 400.0;
            if t >= end {
                break;
            }
            let p = from + (to - from) * t;
            if let Some(h) = top(&g, p.truncate()) {
                prop_assert!(
                    p.z > h - tolerance,
                    "under the ground at {t} before the hit {hit:?}: {} < {h}",
                    p.z
                );
            }
        }
        if let Some(hit) = hit {
            if hit.fraction > 0.0 {
                let h = top(&g, hit.at.truncate()).unwrap();
                prop_assert!(
                    (hit.at.z - h).abs() < tolerance,
                    "the hit is on the surface: {hit:?} vs {h}"
                );
            }
        }
    }
    Ok(())
}

fn arb_specs() -> impl Strategy<Value = Vec<Spec>> {
    proptest::collection::vec(arb_spec(), 1..10)
}

fn arb_rays() -> impl Strategy<Value = Vec<(f64, f64, f64, f64, f64, f64)>> {
    proptest::collection::vec(
        (
            -2000.0f64..2000.0,
            -2000.0f64..2000.0,
            -5.0f64..60.0,
            -2000.0f64..2000.0,
            -2000.0f64..2000.0,
            -40.0f64..60.0,
        ),
        1..8,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Two replicas applying the same edits end with the same bits.
    #[test]
    fn replicas_agree_in_both_frames(specs in arb_specs()) {
        replicas_agree(planar(), &specs)?;
        replicas_agree(geodetic(), &specs)?;
    }

    #[test]
    fn versions_bump_by_one_in_both_frames(specs in arb_specs()) {
        versions_bump_by_one(planar(), &specs)?;
        versions_bump_by_one(geodetic(), &specs)?;
    }

    #[test]
    fn changes_are_local_in_both_frames(specs in arb_specs()) {
        changes_are_local(planar(), &specs)?;
        changes_are_local(geodetic(), &specs)?;
    }

    #[test]
    fn deliveries_leave_a_prefix_in_both_frames(
        specs in arb_specs(),
        delivery in proptest::collection::vec(0usize..64, 0..24),
    ) {
        deliveries_leave_a_prefix(planar(), &specs, &delivery)?;
        deliveries_leave_a_prefix(geodetic(), &specs, &delivery)?;
    }

    #[test]
    fn rays_agree_with_the_surface_in_both_frames(specs in arb_specs(), rays in arb_rays()) {
        rays_agree_with_the_surface(planar(), &specs, &rays)?;
        rays_agree_with_the_surface(geodetic(), &specs, &rays)?;
    }

    /// Edits on columns far apart commute: their order only shows in the
    /// sequence numbers, not in the heights.
    #[test]
    fn far_apart_edits_commute(a in arb_spec(), b in arb_spec()) {
        let f = planar();
        let (ea, eb) = (realise(&f, a, 1), realise(&f, b, 2));
        prop_assume!(ea.brush.is_valid() && eb.brush.is_valid());
        let (la, ha) = ea.brush.footprint(&f);
        let (lb, hb) = eb.brush.footprint(&f);
        let gap = 32.0;
        prop_assume!(ha.x + gap < lb.x || hb.x + gap < la.x || ha.y + gap < lb.y || hb.y + gap < la.y);
        let (mut one, mut two) = (ramp(f), ramp(f));
        let mut c = Vec::new();
        let _ = one.apply(&ea, &mut c);
        let _ = one.apply(&eb, &mut c);
        let _ = two.apply(&Edit { seq: EditSeq(1), ..eb }, &mut c);
        let _ = two.apply(&Edit { seq: EditSeq(2), ..ea }, &mut c);
        let columns = probe_columns(&f);
        prop_assert_eq!(answers(&one, &columns), answers(&two, &columns));
    }
}

// ---------------------------------------------------------------------------
// Golden digests: a fixed script's state and answers, recorded. The same
// test runs on every CI target (Linux and macOS today); a drift in any
// arithmetic path on one of them fails here.

/// A deterministic script of edits from a fixed linear congruential
/// sequence.
fn golden_specs() -> Vec<Spec> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (x >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..24)
        .map(|k| Spec {
            east: -1800.0 + 3600.0 * next(),
            north: -1800.0 + 3600.0 * next(),
            dz: -20.0 + 40.0 * next(),
            r: 4.0 + 50.0 * next(),
            slope: -1.0 + 2.0 * next(),
            add: next() < 0.5,
            kind: (k * 7 % 20) as u8,
        })
        .collect()
}

fn golden<F: Frame + Copy>(frame: F) -> (u64, u64) {
    let mut g = wet_ramp(frame);
    let (mut changed, mut refused) = (Vec::new(), 0);
    for e in &script(&frame, &golden_specs()) {
        refused += usize::from(g.apply(e, &mut changed).is_err());
    }
    // The script exercises edits that change the ground and refusals.
    assert!(
        changed.len() > 10 && refused >= 2,
        "{} {refused}",
        changed.len()
    );
    let columns = probe_columns(&frame);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut write = |v: u64| {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for a in answers(&g, &columns) {
        a.into_iter().for_each(&mut write);
    }
    for k in 0..64 {
        let e = -2000.0 + 62.5 * f64::from(k);
        let from = at(&frame, e, -1500.0, ramp_height(e) + 30.0);
        let to = at(&frame, -e, 1500.0, ramp_height(-e) - 10.0);
        let hit = cast(&g, from, to);
        write(hit.map_or(u64::MAX, |h| h.fraction.to_bits()));
        write(hit.map_or(u64::MAX, |h| h.at.z.to_bits()));
    }
    (g.digest(), h)
}

#[test]
fn golden_digests_are_recorded() {
    let recorded = [
        (
            "planar",
            golden(planar()),
            (0xbcf0_845d_1feb_cb69, 0xc09e_1967_da7d_c764),
        ),
        (
            "geodetic",
            golden(geodetic()),
            (0xf82b_4095_c2e2_abf8, 0x0929_7c73_20d1_59a4),
        ),
    ];
    for (name, got, want) in recorded {
        assert_eq!(got, want, "{name}: state and answer digests ({got:#018x?})");
    }
}
