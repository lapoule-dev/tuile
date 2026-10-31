// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground contract on the reference heightfield, in a planar and in a
//! geodetic frame: query semantics, edits, ordering, versions, change
//! reports, water, and determinism properties.

// Helpers outside `#[test]` functions unwrap too: a test says what it
// expects by panicking.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::f64::consts::{FRAC_PI_2, PI};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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
    // `libm`'s cosine, not the platform's: the golden digests depend on
    // these bounds to the bit.
    let lat = 45f64.to_radians();
    let dlat = 4096.0 / EARTH_RADIUS;
    let dlon = dlat / libm::cos(lat);
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
// Poles and the antimeridian.

/// Every longitude, from 8 km south of the north pole up to it: 257
/// samples, 32 m apart in latitude.
fn polar() -> GeodeticFrame {
    GeodeticFrame {
        min: DVec2::new(-PI, FRAC_PI_2 - 8192.0 / EARTH_RADIUS),
        max: DVec2::new(PI, FRAC_PI_2),
        edit_level: 4,
    }
}

/// The same cap, but a window of one radian of longitude: it has edges.
fn polar_window() -> GeodeticFrame {
    GeodeticFrame {
        min: DVec2::new(-0.5, FRAC_PI_2 - 8192.0 / EARTH_RADIUS),
        max: DVec2::new(0.5, FRAC_PI_2),
        edit_level: 4,
    }
}

/// A band around the equator over every longitude: 637 km north to south,
/// samples 156 km apart in longitude.
fn global() -> GeodeticFrame {
    GeodeticFrame {
        min: DVec2::new(-PI, -0.05),
        max: DVec2::new(PI, 0.05),
        edit_level: 4,
    }
}

#[test]
fn a_ball_at_the_pole_carves_a_disc_over_every_longitude() {
    let f = polar();
    let mut g = flat(f, 0.0);
    let r = 2000.0;
    let mut changed = Vec::new();
    g.apply(
        &sphere(1, EditOp::Remove, DVec3::new(0.3, FRAC_PI_2, 0.0), r),
        &mut changed,
    )
    .unwrap();
    // The disc's angular radius: tangent-plane distance R·sin θ ≤ r.
    let theta = libm::asin(r / EARTH_RADIUS);
    let (mut inside, mut outside) = (0, 0);
    for j in 0..257 {
        for i in (0..257).step_by(4) {
            let c = g.sample_column(i, j);
            if !f.contains(c) {
                continue;
            }
            let h = top(&g, c).unwrap();
            let from_pole = FRAC_PI_2 - c.y;
            if from_pole < theta * 0.999 {
                assert!(h < 0.0, "carved at every longitude: {c:?} {h}");
                inside += 1;
            } else if from_pole > theta * 1.001 {
                assert_eq!(h, 0.0, "nothing beyond the disc: {c:?}");
                outside += 1;
            }
        }
    }
    assert!(inside > 1000 && outside > 1000, "{inside} {outside}");
    for x in 0..16 {
        assert!(
            changed
                .iter()
                .any(|c| c.tile == TileKey { level: 4, x, y: 15 }),
            "every tile around the pole is reported: {changed:?}"
        );
    }
}

/// A ball a kilometre from the pole reaches across it to the far side.
#[test]
fn a_ball_near_the_pole_covers_its_far_side() {
    let mut g = flat(polar(), 0.0);
    let centre = DVec3::new(0.0, FRAC_PI_2 - 1000.0 / EARTH_RADIUS, 0.0);
    g.apply(&sphere(1, EditOp::Remove, centre, 2000.0), &mut Vec::new())
        .unwrap();
    // Sample meridian 255 is at longitude π − 0.0245, across the pole.
    // Row 240 is 512 m from the pole (about 1.5 km from the centre), row
    // 219 1184 m (about 2.2 km).
    assert!(top(&g, g.sample_column(255, 240)).unwrap() < 0.0);
    assert_eq!(top(&g, g.sample_column(255, 219)), Some(0.0));
    // A quarter turn away, about 1.1 km from the centre.
    assert!(top(&g, g.sample_column(192, 240)).unwrap() < 0.0);
}

#[test]
fn a_ball_across_the_antimeridian_wraps() {
    let mut g = flat(global(), 0.0);
    let mut changed = Vec::new();
    // 64 km east of 180°, on the equator, 400 km wide.
    g.apply(
        &sphere(
            1,
            EditOp::Remove,
            DVec3::new(-PI + 0.01, 0.0, 0.0),
            400_000.0,
        ),
        &mut changed,
    )
    .unwrap();
    let j = 128; // the equator
    assert!(
        top(&g, g.sample_column(1, j)).unwrap() < 0.0,
        "east of 180°"
    );
    assert!(
        top(&g, g.sample_column(255, j)).unwrap() < 0.0,
        "west of 180°, 220 km from the centre"
    );
    assert_eq!(top(&g, g.sample_column(128, j)), Some(0.0), "the far side");
    let xs: Vec<u32> = changed.iter().map(|c| c.tile.x).collect();
    assert!(
        xs.contains(&0) && xs.contains(&15),
        "both ends reported: {xs:?}"
    );
    // A window with edges clips instead.
    let mut window = flat(
        GeodeticFrame {
            min: DVec2::new(-PI, -0.05),
            max: DVec2::new(0.0, 0.05),
            edit_level: 4,
        },
        0.0,
    );
    window
        .apply(
            &sphere(
                1,
                EditOp::Remove,
                DVec3::new(-PI + 0.01, 0.0, 0.0),
                400_000.0,
            ),
            &mut Vec::new(),
        )
        .unwrap();
    assert!(top(&window, window.sample_column(1, 128)).unwrap() < 0.0);
}

#[test]
fn latitudes_beyond_the_poles_are_no_place() {
    let mut g = flat(polar(), 0.0);
    let mut changed = Vec::new();
    assert_eq!(
        g.apply(
            &sphere(1, EditOp::Remove, DVec3::new(0.0, 1.6, 0.0), 500.0),
            &mut changed
        ),
        Err(GroundError::InvalidBrush)
    );
    assert_eq!(g.last_seq(), EditSeq(1), "consumed");
    assert!(changed.is_empty());
    let capsule = Edit {
        seq: EditSeq(2),
        op: EditOp::Add,
        brush: Brush::Capsule {
            a: DVec3::new(0.0, 1.5, 0.0),
            b: DVec3::new(0.0, -1.7, 0.0),
            radius: 5.0,
        },
    };
    assert_eq!(
        g.apply(&capsule, &mut changed),
        Err(GroundError::InvalidBrush)
    );
}

// ---------------------------------------------------------------------------
// A ground that keeps nothing resident.

/// A test double for a paged or procedural source: it holds its source and
/// its edit log and rebuilds the heightfield from them every third read,
/// dropping whatever it held. The contract says no answer can tell.
#[derive(Debug)]
struct Evicting<F: Frame + Clone> {
    source: Heightfield<F>,
    log: Vec<Edit>,
    last: EditSeq,
    resident: Mutex<Option<Heightfield<F>>>,
    reads: AtomicU64,
}

impl<F: Frame + Clone> Evicting<F> {
    fn new(source: Heightfield<F>) -> Self {
        Evicting {
            source,
            log: Vec::new(),
            last: EditSeq(0),
            resident: Mutex::new(None),
            reads: AtomicU64::new(0),
        }
    }

    fn rebuild(&self) -> Heightfield<F> {
        let mut g = self.source.clone();
        for e in &self.log {
            let _ = g.apply(e, &mut Vec::new());
        }
        g
    }

    fn with<R>(&self, read: impl FnOnce(&Heightfield<F>) -> R) -> R {
        let mut slot = self.resident.lock().unwrap();
        if self.reads.fetch_add(1, Ordering::Relaxed).is_multiple_of(3) {
            *slot = None;
        }
        read(slot.get_or_insert_with(|| self.rebuild()))
    }
}

impl<F: Frame + Clone> Ground for Evicting<F> {
    fn frame(&self) -> &dyn Frame {
        self.source.frame()
    }
    fn version(&self, tile: TileKey) -> Version {
        self.with(|g| g.version(tile))
    }
    fn water_bodies(&self) -> &[WaterBody] {
        self.source.water_bodies()
    }
    fn surface(&self, p: &[DVec3], out: &mut [Option<Surface>]) -> Result<(), BatchMismatch> {
        self.with(|g| g.surface(p, out))
    }
    fn spans(&self, c: &[DVec2], out: &mut SpanBuffer) {
        self.with(|g| g.spans(c, out));
    }
    fn solid(&self, p: &[DVec3], out: &mut [bool]) -> Result<(), BatchMismatch> {
        self.with(|g| g.solid(p, out))
    }
    fn raycast(&self, r: &[Ray], out: &mut [Option<Hit>]) -> Result<(), BatchMismatch> {
        self.with(|g| g.raycast(r, out))
    }
    fn water(&self, c: &[DVec2], out: &mut [Option<WaterAt>]) -> Result<(), BatchMismatch> {
        self.with(|g| g.water(c, out))
    }
}

impl<F: Frame + Clone> EditGround for Evicting<F> {
    fn last_seq(&self) -> EditSeq {
        self.last
    }

    fn apply(&mut self, edit: &Edit, changed: &mut Vec<TileChanged>) -> Result<(), GroundError> {
        let mut g = self.rebuild();
        let result = g.apply(edit, changed);
        if result
            .as_ref()
            .map_or_else(GroundError::consumes_seq, |()| true)
        {
            self.log.push(*edit);
            self.last = edit.seq;
        }
        *self.resident.get_mut().unwrap() = Some(g);
        result
    }

    /// From the source's identity (its digest) and the edit layer, without
    /// loading anything.
    fn digest(&self) -> u64 {
        let mut h = Fnv::default();
        h.u64(self.source.digest());
        for e in &self.log {
            h.u64(e.seq.0);
            h.u64(u64::from(e.op == EditOp::Add));
            let (kind, points, radius) = match e.brush {
                Brush::Sphere { centre, radius } => (0, [centre, centre], radius),
                Brush::Capsule { a, b, radius } => (1, [a, b], radius),
                Brush::Box { min, max } => (2, [min, max], 0.0),
            };
            h.u64(kind);
            for p in points {
                p.to_array().into_iter().for_each(|v| h.u64(v.to_bits()));
            }
            h.u64(radius.to_bits());
        }
        h.0
    }
}

#[derive(Debug)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

// ---------------------------------------------------------------------------
// Properties, generic over any `EditGround` built by a factory: a conformance
// suite a new implementation can run.

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

fn arb_script<F: Frame + Copy + 'static>(frame: F) -> impl Strategy<Value = Vec<Edit>> {
    proptest::collection::vec(arb_spec(), 1..10).prop_map(move |v| script(&frame, &v))
}

/// Edits on a frame over every longitude, in its own coordinates: longitudes
/// often within 0.2 rad of the antimeridian; on a polar frame, distances
/// from the pole often under 400 m or nil; radii scaled to the frame's
/// cells (`scale`, metres); long thin capsules along a parallel too.
fn arb_globe_script(frame: GeodeticFrame, near_pole: bool) -> impl Strategy<Value = Vec<Edit>> {
    let lon = prop_oneof![-PI..PI, (PI - 0.2)..PI, -PI..(-PI + 0.2)];
    let place = if near_pole {
        prop_oneof![0.0f64..9000.0, 0.0f64..400.0, Just(0.0)]
            .prop_map(|d| FRAC_PI_2 - d / EARTH_RADIUS)
            .boxed()
    } else {
        (-0.045f64..0.045).boxed()
    };
    let scale = if near_pole { 1.0 } else { 100.0 };
    let one = (
        lon,
        place,
        -30.0f64..30.0,
        40.0f64..2500.0,
        -1.0f64..1.0,
        any::<bool>(),
        0u8..15,
    );
    proptest::collection::vec(one, 1..8).prop_map(move |v| {
        v.into_iter()
            .enumerate()
            .map(|(k, (lon, lat, dz, r, slope, add, kind))| {
                let r = r * scale;
                let dlat = r / EARTH_RADIUS;
                let p = DVec3::new(lon, lat, dz);
                let brush = match kind {
                    0..=3 => Brush::Sphere {
                        centre: p,
                        radius: r,
                    },
                    4..=7 => Brush::Capsule {
                        a: p,
                        b: DVec3::new(lon + 0.3 * slope / scale, lat - dlat, dz + slope * r),
                        radius: r / 3.0 + 1.0,
                    },
                    8..=10 => Brush::Box {
                        min: DVec3::new(lon - 0.2 / scale, lat - dlat, dz - r),
                        max: DVec3::new(lon + 0.2 / scale, lat, dz),
                    },
                    11 => Brush::Sphere {
                        centre: DVec3::new(lon, 1.6, dz),
                        radius: r,
                    },
                    // A long thin capsule along a parallel: its great circle
                    // bulges toward the pole, far beyond its radius.
                    12 | 13 => Brush::Capsule {
                        a: p,
                        b: DVec3::new(lon + 3.0 * slope / scale, lat, dz),
                        radius: r / 20.0,
                    },
                    _ => Brush::Sphere {
                        centre: DVec3::new(lon, frame.min.y - 10.0 * dlat - 0.01, dz),
                        radius: r,
                    },
                };
                Edit {
                    seq: EditSeq(k as u64 + 1),
                    op: if add { EditOp::Add } else { EditOp::Remove },
                    brush,
                }
            })
            .collect()
    })
}

/// Columns over the whole frame: an irregular grid, and every edit-level
/// tile corner with the columns just before it (where off-by-ones live).
fn probe_columns(frame: &dyn Frame) -> Vec<DVec2> {
    let (lo, hi) = frame.bounds();
    let mut out = Vec::new();
    for i in 0..48 {
        for j in 0..48 {
            let f = DVec2::new((f64::from(i) + 0.37) / 48.0, (f64::from(j) + 0.71) / 48.0);
            out.push(lo + (hi - lo) * f);
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

/// Rays over the whole frame, many starting or ending outside its bounds,
/// some underground: fractions of the bounds and heights relative to the
/// surface at (the clamp of) each end.
fn rays_over(g: &impl Ground, ends: &[(f64, f64, f64, f64, f64, f64)]) -> Vec<Ray> {
    let (lo, hi) = g.frame().bounds();
    let height = |c: DVec2| {
        let inside = c.clamp(lo, DVec2::new(hi.x.next_down(), hi.y.next_down()));
        top(g, inside).unwrap_or(0.0)
    };
    ends.iter()
        .map(|&(u1, v1, z1, u2, v2, z2)| {
            let (a, b) = (
                lo + (hi - lo) * DVec2::new(u1, v1),
                lo + (hi - lo) * DVec2::new(u2, v2),
            );
            Ray {
                from: a.extend(height(a) + z1),
                to: b.extend(height(b) + z2),
            }
        })
        .collect()
}

fn fixed_ray_ends() -> Vec<(f64, f64, f64, f64, f64, f64)> {
    (0..40)
        .map(|k| {
            let t = f64::from(k) / 40.0;
            (
                -0.2 + 1.4 * t,
                1.2 - 1.3 * t,
                30.0 - 50.0 * t,
                1.2 - 1.4 * ((t * 7.0) % 1.0),
                -0.2 + 1.4 * ((t * 3.0) % 1.0),
                -20.0 + 60.0 * ((t * 5.0) % 1.0),
            )
        })
        .collect()
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

/// Hits as bits: `-0.0` and `0.0` differ, as two replicas' bits would.
fn hit_bits(hits: &[Option<Hit>]) -> Vec<Option<[u64; 4]>> {
    hits.iter()
        .map(|h| {
            h.map(|h| {
                [
                    h.fraction.to_bits(),
                    h.at.x.to_bits(),
                    h.at.y.to_bits(),
                    h.at.z.to_bits(),
                ]
            })
        })
        .collect()
}

fn span_bits(b: &SpanBuffer) -> (Vec<[u64; 2]>, Vec<(usize, usize)>) {
    (
        b.spans
            .iter()
            .map(|s| [s.bottom.to_bits(), s.top.to_bits()])
            .collect(),
        b.ranges.clone(),
    )
}

fn hits(g: &impl Ground, rays: &[Ray]) -> Vec<Option<Hit>> {
    let mut out = vec![None; rays.len()];
    g.raycast(rays, &mut out).unwrap();
    out
}

/// A sea over some samples, so water answers move too.
fn wet<F: Frame>(
    g: Heightfield<F>,
    level: f64,
    covered: impl Fn(usize, usize) -> bool,
) -> Heightfield<F> {
    g.with_water(
        vec![WaterBody {
            kind: WaterKind::Sea,
            level,
        }],
        |i, j| covered(i, j).then_some(WaterBodyId(0)),
    )
    .unwrap()
}

/// The ramp, with a sea over its low samples.
fn wet_ramp<F: Frame>(frame: F) -> Heightfield<F> {
    wet(ramp(frame), 100.0, |i, _| i < 100)
}

/// Flat ground with a sea over its southern rows: the same along a pole row
/// and on both seam meridians.
fn wet_flat<F: Frame>(frame: F) -> Heightfield<F> {
    wet(flat(frame, 0.0), 1.0, |_, j| j < 100)
}

fn replicas_agree<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
) -> Result<(), TestCaseError> {
    let (mut a, mut b) = (make(), make());
    let (mut ca, mut cb) = (Vec::new(), Vec::new());
    for e in edits {
        prop_assert_eq!(a.apply(e, &mut ca), b.apply(e, &mut cb));
    }
    prop_assert_eq!(a.last_seq(), EditSeq(edits.len() as u64));
    prop_assert_eq!(a.digest(), b.digest());
    prop_assert_eq!(ca, cb);
    Ok(())
}

/// Versions grow by exactly one on every reported tile, are reported once
/// and in order, and never move on any other tile.
fn versions_bump_by_one<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
) -> Result<(), TestCaseError> {
    let mut g = make();
    let level = g.frame().edit_level();
    let all: Vec<TileKey> = (0..1u32 << level)
        .flat_map(|y| (0..1u32 << level).map(move |x| TileKey { level, x, y }))
        .collect();
    for e in edits {
        let before: BTreeMap<TileKey, Version> = all.iter().map(|k| (*k, g.version(*k))).collect();
        let mut changed = Vec::new();
        let _ = g.apply(e, &mut changed);
        prop_assert!(changed
            .windows(2)
            .all(|w| (w[0].tile.y, w[0].tile.x) < (w[1].tile.y, w[1].tile.x)));
        for k in &all {
            match changed.iter().find(|c| c.tile == *k) {
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
/// every point query bit for bit as before the edit, and a ray whose
/// segment's rectangle meets no reported tile hits where it did.
fn changes_are_local<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
) -> Result<(), TestCaseError> {
    let mut g = make();
    let level = g.frame().edit_level();
    let columns = probe_columns(g.frame());
    let tiles: Vec<Option<TileKey>> = columns
        .iter()
        .map(|c| g.frame().tile_of(*c, level))
        .collect();
    let rays = rays_over(&g, &fixed_ray_ends());
    let ray_tiles: Vec<Vec<TileKey>> = rays
        .iter()
        .map(|r| {
            let (a, b) = (r.from.truncate(), r.to.truncate());
            tiles_over(g.frame(), a.min(b), a.max(b), level).collect()
        })
        .collect();
    let (mut before, mut before_hits) = (answers(&g, &columns), hits(&g, &rays));
    for e in edits {
        let mut changed = Vec::new();
        let _ = g.apply(e, &mut changed);
        let reported = |t: &TileKey| changed.iter().any(|c| c.tile == *t);
        let (after, after_hits) = (answers(&g, &columns), hits(&g, &rays));
        for k in 0..columns.len() {
            if !tiles[k].is_some_and(|t| reported(&t)) {
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
        for k in 0..rays.len() {
            if !ray_tiles[k].iter().any(reported) {
                prop_assert_eq!(
                    before_hits[k].map(|h| (h.fraction.to_bits(), h.at.z.to_bits())),
                    after_hits[k].map(|h| (h.fraction.to_bits(), h.at.z.to_bits())),
                    "ray {:?} crossing no reported tile changed under {:?}",
                    rays[k],
                    e
                );
            }
        }
        (before, before_hits) = (after, after_hits);
    }
    Ok(())
}

/// Whatever order and duplication the edits arrive in, each delivered once
/// in that order, a replica's state is the state after a prefix of the
/// script: it never applies an edit past a missing one.
fn deliveries_leave_a_prefix<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
    delivery: &[usize],
) -> Result<(), TestCaseError> {
    let mut g = make();
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
    let mut reference = make();
    for e in &edits[..applied] {
        let _ = reference.apply(e, &mut Vec::new());
    }
    prop_assert_eq!(g.digest(), reference.digest());
    Ok(())
}

/// A replica that buffers what arrives early and retries it once the gap
/// is filled reaches the script's final state, whatever the delivery order
/// and duplication, once every edit has arrived.
fn a_buffering_replica_reaches_the_script<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
    delivery: &[usize],
) -> Result<(), TestCaseError> {
    let mut g = make();
    let mut early: BTreeMap<EditSeq, Edit> = BTreeMap::new();
    // The delivery, then every edit once more, last first, so nothing is
    // lost and the gaps are at their widest.
    let rest = (0..edits.len()).rev();
    for k in delivery.iter().copied().chain(rest) {
        let e = edits[k % edits.len()];
        match g.apply(&e, &mut Vec::new()) {
            Err(GroundError::Gap { .. }) => {
                early.insert(e.seq, e);
            }
            Err(GroundError::Stale { .. }) => {}
            _ => {
                // Drain what has become next.
                while let Some(next) = early.remove(&EditSeq(g.last_seq().0 + 1)) {
                    let _ = g.apply(&next, &mut Vec::new());
                }
            }
        }
    }
    let mut reference = make();
    for e in edits {
        let _ = reference.apply(e, &mut Vec::new());
    }
    prop_assert_eq!(g.last_seq(), EditSeq(edits.len() as u64));
    prop_assert_eq!(g.digest(), reference.digest());
    Ok(())
}

/// The raycast agrees with the surface: no point of the segment before the
/// hit is under the ground (a segment running underground from where it
/// enters the bounds hits there), and the hit is on the surface or, where
/// the segment enters the ground's domain, below it.
fn rays_agree_with_the_surface<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
    ends: &[(f64, f64, f64, f64, f64, f64)],
) -> Result<(), TestCaseError> {
    let mut g = make();
    for e in edits {
        let _ = g.apply(e, &mut Vec::new());
    }
    let (lo, hi) = g.frame().bounds();
    let tolerance = 1e-6;
    for ray in rays_over(&g, ends) {
        let hit = cast(&g, ray.from, ray.to);
        let end = hit.map_or(1.0, |h| h.fraction);
        for s in 0..400 {
            let t = end * f64::from(s) / 400.0;
            if t >= end {
                break;
            }
            let p = ray.from + (ray.to - ray.from) * t;
            if let Some(h) = top(&g, p.truncate()) {
                prop_assert!(
                    p.z > h - tolerance,
                    "under the ground at {t} before the hit {hit:?}: {} < {h}",
                    p.z
                );
            }
        }
        if let Some(hit) = hit {
            let c = hit.at.truncate();
            let on_edge = c.x == lo.x || c.y == lo.y || c.x == hi.x || c.y == hi.y;
            let inside = c.clamp(lo, DVec2::new(hi.x.next_down(), hi.y.next_down()));
            let h = top(&g, inside).unwrap();
            // Where the segment enters the ground's domain (its start, or the
            // bounds' entry face) it may already be below the surface.
            let entry = hit.fraction == 0.0 || on_edge;
            prop_assert!(
                (hit.at.z - h).abs() < tolerance || (entry && hit.at.z < h + tolerance),
                "the hit is on the surface: {hit:?} vs {h}"
            );
        }
    }
    Ok(())
}

/// The evicting double answers every query, reports and digests like
/// replicas of itself, and answers bit for bit like the heightfield it pages.
fn the_double_answers_like_the_heightfield<F: Frame + Copy>(
    make: impl Fn() -> Heightfield<F>,
    edits: &[Edit],
) -> Result<(), TestCaseError> {
    let (mut plain, mut paged) = (make(), Evicting::new(make()));
    let columns = probe_columns(plain.frame());
    let rays = rays_over(&plain, &fixed_ray_ends());
    for e in edits {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        prop_assert_eq!(plain.apply(e, &mut a), paged.apply(e, &mut b));
        prop_assert_eq!(a, b);
        prop_assert_eq!(answers(&plain, &columns), answers(&paged, &columns));
        prop_assert_eq!(
            hit_bits(&hits(&plain, &rays)),
            hit_bits(&hits(&paged, &rays))
        );
        let (mut sa, mut sb) = (SpanBuffer::default(), SpanBuffer::default());
        plain.spans(&columns, &mut sa);
        paged.spans(&columns, &mut sb);
        prop_assert_eq!(span_bits(&sa), span_bits(&sb));
    }
    Ok(())
}

fn arb_ray_ends() -> impl Strategy<Value = Vec<(f64, f64, f64, f64, f64, f64)>> {
    proptest::collection::vec(
        (
            -0.3f64..1.3,
            -0.3f64..1.3,
            -40.0f64..60.0,
            -0.3f64..1.3,
            -0.3f64..1.3,
            -40.0f64..60.0,
        ),
        1..8,
    )
}

/// Every property on one ground and one script.
fn all_properties<G: EditGround>(
    make: impl Fn() -> G,
    edits: &[Edit],
    delivery: &[usize],
    ends: &[(f64, f64, f64, f64, f64, f64)],
) -> Result<(), TestCaseError> {
    replicas_agree(&make, edits)?;
    versions_bump_by_one(&make, edits)?;
    changes_are_local(&make, edits)?;
    deliveries_leave_a_prefix(&make, edits, delivery)?;
    a_buffering_replica_reaches_the_script(&make, edits, delivery)?;
    rays_agree_with_the_surface(&make, edits, ends)
}

fn arb_delivery() -> impl Strategy<Value = Vec<usize>> {
    proptest::collection::vec(0usize..64, 0..24)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn the_heightfield_on_a_planar_frame(
        edits in arb_script(planar()), delivery in arb_delivery(), ends in arb_ray_ends(),
    ) {
        all_properties(|| wet_ramp(planar()), &edits, &delivery, &ends)?;
    }

    #[test]
    fn the_heightfield_on_a_geodetic_frame(
        edits in arb_script(geodetic()), delivery in arb_delivery(), ends in arb_ray_ends(),
    ) {
        all_properties(|| wet_ramp(geodetic()), &edits, &delivery, &ends)?;
    }

    #[test]
    fn the_heightfield_around_a_pole(
        edits in arb_globe_script(polar(), true), delivery in arb_delivery(), ends in arb_ray_ends(),
    ) {
        all_properties(|| wet_flat(polar()), &edits, &delivery, &ends)?;
        all_properties(|| wet_flat(polar_window()), &edits, &delivery, &ends)?;
    }

    #[test]
    fn the_heightfield_across_the_antimeridian(
        edits in arb_globe_script(global(), false), delivery in arb_delivery(), ends in arb_ray_ends(),
    ) {
        all_properties(|| wet_flat(global()), &edits, &delivery, &ends)?;
    }

    #[test]
    fn a_ground_that_keeps_nothing_resident(
        edits in arb_script(geodetic()), delivery in arb_delivery(), ends in arb_ray_ends(),
    ) {
        all_properties(|| Evicting::new(wet_ramp(geodetic())), &edits, &delivery, &ends)?;
        the_double_answers_like_the_heightfield(|| wet_ramp(geodetic()), &edits)?;
    }

    #[test]
    fn a_ground_that_keeps_nothing_resident_around_a_pole(
        edits in arb_globe_script(polar(), true),
    ) {
        the_double_answers_like_the_heightfield(|| wet_flat(polar()), &edits)?;
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
// Golden digests: a fixed script's state and answers, recorded.
//
// Recorded on macOS (aarch64, Apple silicon) with rustc 1.99.0, by
// running this test with the expected values zeroed and copying what it
// prints. The `test` CI jobs run it on Linux (x86-64) and macOS (aarch64):
// a drift in any arithmetic path on one of them fails here. wasm32 is not
// run yet. Every input is built with `+ − × ÷ sqrt` and `libm`, never the
// platform's transcendentals; a deliberate change of arithmetic re-records
// them in the same commit, saying why. Last re-recorded with the capsule's
// great-circle footprint, pole rows and seams decided once, and normals
// measured at the cell's centre (planar unchanged; geodetic: answers only).

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

/// A fixed script around the pole: balls and capsules at and near it, and
/// across the antimeridian.
fn golden_polar_script() -> Vec<Edit> {
    (0..12u32)
        .map(|k| {
            let kf = f64::from(k);
            let lon = -PI + 0.5 * kf;
            let lat = FRAC_PI_2 - 300.0 * kf / EARTH_RADIUS;
            let p = DVec3::new(lon, lat, 10.0 - 3.0 * kf);
            let brush = if k % 3 == 2 {
                Brush::Capsule {
                    a: p,
                    b: DVec3::new(PI - 0.05, lat - 0.0002, -20.0),
                    radius: 150.0,
                }
            } else {
                Brush::Sphere {
                    centre: p,
                    radius: 200.0 + 90.0 * kf,
                }
            };
            Edit {
                seq: EditSeq(u64::from(k) + 1),
                op: if k % 2 == 0 {
                    EditOp::Remove
                } else {
                    EditOp::Add
                },
                brush,
            }
        })
        .collect()
}

fn golden(mut g: impl EditGround, script: &[Edit]) -> (u64, u64) {
    let (mut changed, mut refused) = (Vec::new(), 0);
    for e in script {
        refused += usize::from(g.apply(e, &mut changed).is_err());
    }
    // The script exercises edits that change the ground.
    assert!(changed.len() > 10, "{} {refused}", changed.len());
    let columns = probe_columns(g.frame());
    let mut h = Fnv::default();
    for a in answers(&g, &columns) {
        a.into_iter().for_each(|v| h.u64(v));
    }
    for hit in hits(&g, &rays_over(&g, &fixed_ray_ends())) {
        h.u64(hit.map_or(u64::MAX, |h| h.fraction.to_bits()));
        h.u64(hit.map_or(u64::MAX, |h| h.at.z.to_bits()));
    }
    (g.digest(), h.0)
}

#[test]
fn golden_digests_are_recorded() {
    let recorded = [
        (
            "planar",
            golden(wet_ramp(planar()), &script(&planar(), &golden_specs())),
            (0x1736_893e_df47_5f1e, 0x2eb5_d769_c49b_c4ab),
        ),
        (
            "geodetic",
            golden(wet_ramp(geodetic()), &script(&geodetic(), &golden_specs())),
            (0x6f92_c77b_6f59_c82e, 0x3b36_808b_a543_db82),
        ),
        (
            "polar",
            golden(wet_flat(polar()), &golden_polar_script()),
            (0x9f19_6aa1_040a_8ff6, 0x047a_89b4_4b88_bfa2),
        ),
    ];
    let wrong: Vec<String> = recorded
        .iter()
        .filter(|(_, got, want)| got != want)
        .map(|(name, got, _)| format!("{name}: ({:#018x}, {:#018x})", got.0, got.1))
        .collect();
    assert!(wrong.is_empty(), "state and answer digests: {wrong:?}");
}

// ---------------------------------------------------------------------------
// Raycast edges and precision, degenerate grids.

#[test]
fn a_ray_entering_through_a_max_edge_below_the_surface_hits_there() {
    let g = flat(planar(), 0.0);
    for (from, to, x, y) in [
        (
            DVec3::new(2100.0, 0.0, -1000.0),
            DVec3::new(-2100.0, 0.0, -1000.0),
            2048.0,
            0.0,
        ),
        (
            DVec3::new(-2100.0, 0.0, -1000.0),
            DVec3::new(2100.0, 0.0, -1000.0),
            -2048.0,
            0.0,
        ),
        (
            DVec3::new(3.0, 2100.0, -1000.0),
            DVec3::new(3.0, -2100.0, -1000.0),
            3.0,
            2048.0,
        ),
    ] {
        let hit = cast(&g, from, to).expect("the segment runs underground from its entry");
        assert_eq!((hit.at.x, hit.at.y), (x, y), "{hit:?}");
    }
}

/// A concave patch (one raised corner) grazed by long rays: the entry is a
/// hair after the cell's corner, where a root computed from the far start
/// of the ray loses its precision or rounds to before the corner.
#[test]
fn a_grazing_entry_into_a_concave_patch_is_found_at_once() {
    let g = Heightfield::new(
        planar(),
        257,
        |i, j| {
            if (i, j) == (129, 128) {
                64.0
            } else {
                0.0
            }
        },
    )
    .unwrap();
    let mut misses = Vec::new();
    for e in 0..60 {
        let eps = 1e-4 * 0.6f64.powi(e);
        for k in 0..60 {
            let start = -2000.0 + 33.7 * f64::from(k);
            for slope in [0.1, 0.5, 2.0, 7.0] {
                // z = eps − slope·x: above the flat ground west of the
                // cell, at the cell's corner a hair above its surface.
                let from = DVec3::new(start, start, eps - slope * start);
                let to = DVec3::new(16.0, 16.0, eps - slope * 16.0);
                let hit = cast(&g, from, to).expect("the ray enters the patch");
                // Along the diagonal, s = x/16: the patch is 64·s·(1 − s)
                // and the gap −64s² + b·s − eps, whose first root is
                // 2·eps / (b + √(b² − 256·eps)).
                let b = 64.0 + 16.0 * slope;
                let x = 16.0 * (2.0 * eps / (b + (b * b - 256.0 * eps).sqrt()));
                if (hit.at.x - x).abs() > 1e-9 {
                    misses.push((eps, start, slope, x, hit.at.x));
                }
            }
        }
    }
    assert!(
        misses.is_empty(),
        "{} late entries, e.g. {:?}",
        misses.len(),
        &misses[..misses.len().min(3)]
    );
}

#[test]
fn cells_too_narrow_to_tell_apart_are_refused() {
    // 2^-1074-wide bounds: every edge rounds onto the next.
    let f = GeodeticFrame {
        min: DVec2::new(0.5, 0.5),
        max: DVec2::new(0.5f64.next_up(), 0.5f64.next_up().next_up()),
        edit_level: 0,
    };
    assert_eq!(
        Heightfield::new(f, 17, |_, _| 0.0).err(),
        Some(InvalidGround::DegenerateCells)
    );
}

// ---------------------------------------------------------------------------
// Great circles, one place at a pole, longitudes modulo a turn.

/// A unit vector's column, longitude and latitude, with `libm`.
fn column_of(v: DVec3) -> DVec2 {
    let horizontal = (v.x * v.x + v.y * v.y).sqrt();
    DVec2::new(libm::atan2(v.y, v.x), libm::atan2(v.z, horizontal))
}

fn unit(c: DVec2) -> DVec3 {
    DVec3::new(
        libm::cos(c.y) * libm::cos(c.x),
        libm::cos(c.y) * libm::sin(c.x),
        libm::sin(c.y),
    )
}

/// A capsule whose two ends sit 3 km from the pole, 3 radians of longitude
/// apart: its axis follows the great circle, which passes about 210 m from
/// the pole, not the parallel of its ends.
#[test]
fn a_capsule_near_the_pole_carves_along_its_great_circle() {
    let mut g = flat(polar(), 0.0);
    let lat = FRAC_PI_2 - 3000.0 / EARTH_RADIUS;
    let capsule = Edit {
        seq: EditSeq(1),
        op: EditOp::Remove,
        brush: Brush::Capsule {
            a: DVec3::new(0.0, lat, 0.0),
            b: DVec3::new(3.0, lat, 0.0),
            radius: 150.0,
        },
    };
    g.apply(&capsule, &mut Vec::new()).unwrap();
    // Meridian 189 is at longitude 1.497, row 250 192 m from the pole: about
    // 20 m from the axis.
    let c = g.sample_column(189, 250);
    assert!(
        capsule.brush.column_extent(g.frame(), c).is_some(),
        "the axis passes by"
    );
    assert!(top(&g, c).unwrap() < 0.0, "carved");
}

/// A 6.4 km capsule along the 60th parallel with a 1 m radius: its great
/// circle bulges 1.4 m north of the parallel, beyond the box of its ends.
#[test]
fn a_capsule_footprint_holds_its_great_circle() {
    let f = GeodeticFrame {
        min: DVec2::new(-0.01, 1.0),
        max: DVec2::new(0.01, 1.1),
        edit_level: 2,
    };
    let lat = 60f64.to_radians();
    let dlon = 6400.0 / (EARTH_RADIUS * libm::cos(lat));
    let (a, b) = (DVec2::new(-dlon / 2.0, lat), DVec2::new(dlon / 2.0, lat));
    let brush = Brush::Capsule {
        a: a.extend(0.0),
        b: b.extend(0.0),
        radius: 1.0,
    };
    let middle = column_of(unit(a) + unit(b));
    assert!(brush.column_extent(&f, middle).is_some());
    let (lo, hi) = brush.footprint(&f);
    assert!(
        middle.cmpge(lo).all() && middle.cmple(hi).all(),
        "{middle:?} in {lo:?}..{hi:?}"
    );
}

/// Whether a column falls in a footprint, a whole period of `u` away
/// included.
fn in_footprint(frame: &dyn Frame, c: DVec2, lo: DVec2, hi: DVec2) -> bool {
    if c.y < lo.y || c.y > hi.y {
        return false;
    }
    match frame.u_period() {
        Some(p) if hi.x - lo.x >= p => true,
        Some(p) => (-3..=3).any(|k| {
            let x = c.x + p * f64::from(k);
            x >= lo.x && x <= hi.x
        }),
        None => c.x >= lo.x && c.x <= hi.x,
    }
}

/// No sample outside a brush's footprint is reached by it: the footprint
/// bounds the samples an edit visits.
fn footprints_hold_their_brushes(
    g: &Heightfield<impl Frame>,
    edits: &[Edit],
) -> Result<(), TestCaseError> {
    let f = g.frame();
    let n = g.samples_per_side();
    for e in edits {
        if !e.brush.is_valid_in(f) {
            continue;
        }
        let (lo, hi) = e.brush.footprint(f);
        for j in 0..n {
            for i in 0..n {
                let c = g.sample_column(i, j);
                if !in_footprint(f, c, lo, hi) {
                    prop_assert!(
                        e.brush.column_extent(f, c).is_none(),
                        "sample ({}, {}) at {:?} outside the footprint {:?}..{:?} of {:?}",
                        i,
                        j,
                        c,
                        lo,
                        hi,
                        e.brush
                    );
                }
            }
        }
    }
    Ok(())
}

/// The samples of one place — a pole row, the two seam meridians of a frame
/// over a whole turn — stay equal to the bit.
fn one_place_stays_one<F: Frame>(
    make: impl Fn() -> Heightfield<F>,
    edits: &[Edit],
) -> Result<(), TestCaseError> {
    let mut g = make();
    let n = g.samples_per_side();
    let (lo, hi) = g.frame().bounds();
    let seam = hi.x - lo.x == core::f64::consts::TAU;
    let mut applied = 0usize;
    for e in edits {
        if g.apply(e, &mut Vec::new()).is_ok() {
            applied += 1;
        }
        for j in [0, n - 1] {
            if g.sample_column(0, j).y.abs() == FRAC_PI_2 {
                let first = g.sample_height(0, j).unwrap().to_bits();
                for i in 0..n {
                    prop_assert_eq!(
                        g.sample_height(i, j).unwrap().to_bits(),
                        first,
                        "pole row {} at {}",
                        j,
                        i
                    );
                }
            }
        }
        if seam {
            for j in 0..n {
                prop_assert_eq!(
                    g.sample_height(0, j).unwrap().to_bits(),
                    g.sample_height(n - 1, j).unwrap().to_bits(),
                    "seam at row {}",
                    j
                );
            }
        }
    }
    // A script none of whose edits applied proves nothing: drawn again.
    prop_assume!(applied > 0, "no edit of the script applied");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn footprints_hold_their_brushes_everywhere(
        planar_edits in arb_script(planar()),
        geodetic_edits in arb_script(geodetic()),
        polar_edits in arb_globe_script(polar(), true),
        global_edits in arb_globe_script(global(), false),
    ) {
        footprints_hold_their_brushes(&flat(planar(), 0.0), &planar_edits)?;
        footprints_hold_their_brushes(&flat(geodetic(), 0.0), &geodetic_edits)?;
        footprints_hold_their_brushes(&flat(polar(), 0.0), &polar_edits)?;
        footprints_hold_their_brushes(&flat(polar_window(), 0.0), &polar_edits)?;
        footprints_hold_their_brushes(&flat(global(), 0.0), &global_edits)?;
    }

    #[test]
    fn pole_rows_and_seams_stay_one_place(
        polar_edits in arb_globe_script(polar(), true),
        global_edits in arb_globe_script(global(), false),
    ) {
        one_place_stays_one(|| wet_flat(polar()), &polar_edits)?;
        one_place_stays_one(|| wet_flat(global()), &global_edits)?;
    }
}

#[test]
fn a_source_must_agree_with_itself_at_one_place() {
    // A pole row that varies with longitude.
    assert_eq!(
        Heightfield::new(polar(), 257, |i, _| i as f64).err(),
        Some(InvalidGround::NotOnePlace { i: 1, j: 256 })
    );
    // Seam meridians that differ.
    assert_eq!(
        Heightfield::new(global(), 257, |i, _| i as f64).err(),
        Some(InvalidGround::NotOnePlace { i: 256, j: 0 })
    );
}

/// Near the south pole a cell's east-west width is measured at its centre,
/// not at the query's latitude, where it vanishes.
#[test]
fn normals_near_a_pole_stay_upright() {
    let f = GeodeticFrame {
        min: DVec2::new(0.0, -FRAC_PI_2),
        max: DVec2::new(0.5, -FRAC_PI_2 + 0.01),
        edit_level: 2,
    };
    // The pole row is flat; the next rows rise gently eastwards.
    let g = Heightfield::new(f, 257, |i, j| 0.1 * i as f64 * j as f64).unwrap();
    let s = surface(&g, DVec3::new(0.25, -FRAC_PI_2 + 1e-9, 1e6)).unwrap();
    assert!(s.normal.z > 0.5, "{:?}", s.normal);
}

#[test]
fn a_ray_leaving_through_a_max_edge_does_not_hit_there() {
    let g = flat(planar(), 0.0);
    let (lo, hi) = (
        DVec3::new(2048.0, 0.0, -10.0),
        DVec3::new(2100.0, 0.0, -10.0),
    );
    assert_eq!(cast(&g, lo, hi), None, "outward from the max edge");
    let mut solid = [true];
    g.solid(&[lo], &mut solid).unwrap();
    assert!(!solid[0], "the max edge is outside the ground");
    let inward = cast(&g, lo, DVec3::new(2000.0, 0.0, -10.0)).expect("inward");
    assert_eq!(inward.fraction, 0.0);
}

/// A longitude and the same plus or minus whole turns name one place: the
/// same samples change, to the rounding of `lon ± k·2π` itself.
#[test]
fn longitudes_are_taken_modulo_a_turn() {
    use core::f64::consts::TAU;
    for (frame, lon) in [(global(), 0.4), (global(), 3.0), (polar_window(), 0.1)] {
        let lat = (frame.min.y + frame.max.y) / 2.0;
        let r = if frame.max.y == FRAC_PI_2 {
            300.0
        } else {
            200_000.0
        };
        let mut reference = flat(frame, 0.0);
        let mut expected = Vec::new();
        reference
            .apply(
                &sphere(1, EditOp::Remove, DVec3::new(lon, lat, 0.0), r),
                &mut expected,
            )
            .unwrap();
        assert!(!expected.is_empty());
        let columns = probe_columns(&frame);
        for turns in [-2.0, -1.0, 1.0, 2.0] {
            let mut g = flat(frame, 0.0);
            let far = DVec3::new(lon + turns * TAU, lat, 0.0);
            let mut changed = Vec::new();
            assert_eq!(
                g.apply(&sphere(1, EditOp::Remove, far, r), &mut changed),
                Ok(()),
                "{far:?}"
            );
            assert_eq!(changed, expected, "{far:?}");
            for c in &columns {
                let (a, b) = (top(&g, *c), top(&reference, *c));
                let close = match (a, b) {
                    (Some(a), Some(b)) => (a - b).abs() < 1e-6,
                    _ => a == b,
                };
                assert!(close, "{far:?} at {c:?}: {a:?} vs {b:?}");
            }
        }
    }
}

/// Found by the Linux CI run: a segment entering through the min edge below
/// the surface, whose entry point rounds a hair inside the face. The hit is
/// on the face exactly.
#[test]
fn an_entry_hit_lies_on_its_face_exactly() {
    let edits = [Edit {
        seq: EditSeq(1),
        op: EditOp::Remove,
        brush: Brush::Sphere {
            centre: DVec3::new(0.000_454_099_586_719_460_3, 0.785_719_260_294_551_6, 256.0),
            radius: 3.0,
        },
    }];
    let ends = [(
        -0.064_963_967_219_121_65,
        0.986_085_637_025_66,
        -39.578_830_339_842_426,
        0.955_298_101_597_490_2,
        0.0,
        0.0,
    )];
    rays_agree_with_the_surface(|| wet_ramp(geodetic()), &edits, &ends).unwrap();
}
