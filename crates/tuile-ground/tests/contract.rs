// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground contract on the reference heightfield, in a planar and in a
//! geodetic frame: query semantics, edits, ordering, versions, water, and
//! determinism properties.

// Helpers outside `#[test]` functions unwrap too: a test says what it
// expects by panicking.
#![allow(clippy::unwrap_used, clippy::panic)]

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
    Heightfield::new(frame, 257, move |_, _| h)
}

/// Rises 2 m per sample along `u`.
fn ramp<F: Frame>(frame: F) -> Heightfield<F> {
    Heightfield::new(frame, 257, |i, _| 2.0 * i as f64)
}

/// The centre of the frame, `k` metres-ish east and `m` north of it, at `z`.
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

fn a_flat_ground_is_flat_and_up<F: Frame + Copy>(frame: F) {
    let g = flat(frame, 10.0);
    let s = surface(&g, at(&frame, 100.0, -300.0, 50.0)).expect("above the ground");
    assert_eq!(s.height, 10.0);
    assert_eq!(s.normal, DVec3::Z);
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
}

#[test]
fn flat_in_both_frames() {
    a_flat_ground_is_flat_and_up(planar());
    a_flat_ground_is_flat_and_up(geodetic());
}

#[test]
fn heights_interpolate_and_normals_lean_downhill() {
    for (h, n) in [(ramp(planar()), "planar")]
        .iter()
        .map(|(g, n)| (surface(g, at(&planar(), 8.0, 0.0, 1000.0)), *n))
    {
        // Half way between samples 128 (256 m) and 129 (258 m).
        let s = h.expect(n);
        assert_eq!(s.height, 257.0, "{n}");
        assert!(
            s.normal.x < -0.1 && s.normal.z > 0.9,
            "{n}: the normal leans downhill, toward -u: {:?}",
            s.normal
        );
    }
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
    let f = planar();
    let g = flat(f, 10.0);
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

    let mut solid = [false; 2];
    g.solid(
        &[DVec3::new(0.0, 0.0, 9.0), DVec3::new(0.0, 0.0, 11.0)],
        &mut solid,
    )
    .unwrap();
    assert_eq!(solid, [true, false]);

    let mut hits = [None; 2];
    let rays = [
        Ray {
            from: DVec3::new(0.0, 0.0, 50.0),
            to: DVec3::new(0.0, 0.0, -50.0),
        },
        Ray {
            from: DVec3::new(-500.0, 0.0, 20.0),
            to: DVec3::new(500.0, 0.0, 20.0),
        },
    ];
    g.raycast(&rays, &mut hits).unwrap();
    let down = hits[0].expect("straight down hits");
    assert!(down.at.z <= 10.0 && down.at.z > -50.0, "{down:?}");
    assert_eq!(
        hits[1], None,
        "a level line above the ground is clear: line of sight"
    );
}

#[test]
fn a_ridge_blocks_the_line_of_sight() {
    // A 60 m wall along the sample column at the centre.
    let g = Heightfield::new(planar(), 257, |i, _| if i == 128 { 60.0 } else { 0.0 });
    let mut hit = [None];
    g.raycast(
        &[Ray {
            from: DVec3::new(-300.0, 0.0, 20.0),
            to: DVec3::new(300.0, 0.0, 20.0),
        }],
        &mut hit,
    )
    .unwrap();
    assert!(hit[0].is_some(), "the wall stands in the way");
}

#[test]
fn water_reports_its_body_level_and_depth() {
    let bodies = vec![
        WaterBody {
            id: WaterBodyId(0),
            kind: WaterKind::Sea,
            level: 0.0,
        },
        WaterBody {
            id: WaterBodyId(1),
            kind: WaterKind::Lake,
            level: 40.0,
        },
    ];
    // Sea west of the centre over a floor at -10 m; a lake bed at 30 m east.
    let g = Heightfield::new(planar(), 257, |i, _| if i < 128 { -10.0 } else { 30.0 }).with_water(
        bodies,
        |i, _| {
            Some(if i < 128 {
                WaterBodyId(0)
            } else {
                WaterBodyId(1)
            })
        },
    );
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
    // A dry column: the body's level is under the ground.
    let dry = flat(planar(), 5.0).with_water(
        vec![WaterBody {
            id: WaterBodyId(0),
            kind: WaterKind::Sea,
            level: 0.0,
        }],
        |_, _| Some(WaterBodyId(0)),
    );
    let mut out = [None];
    dry.water(&[DVec2::ZERO], &mut out).unwrap();
    assert_eq!(out[0], None);
}

fn edits_carve_raise_and_version<F: Frame + Copy>(frame: F) {
    let mut g = flat(frame, 10.0);
    let mut changed = Vec::new();
    let crater = Edit {
        seq: EditSeq(1),
        op: EditOp::Remove,
        brush: Brush::Sphere {
            centre: at(&frame, 0.0, 0.0, 10.0),
            radius: 20.0,
        },
    };
    g.apply(&crater, &mut changed).unwrap();
    let floor = surface(&g, at(&frame, 0.0, 0.0, 100.0)).unwrap().height;
    assert!(
        (floor - (-10.0)).abs() < 1e-6,
        "the crater's floor: {floor}"
    );
    assert_eq!(
        surface(&g, at(&frame, 300.0, 0.0, 100.0)).unwrap().height,
        10.0,
        "untouched further away"
    );
    // The sphere straddles the four tiles around the centre.
    assert_eq!(changed.len(), 4, "{changed:?}");
    assert!(changed
        .iter()
        .all(|c| c.version == Version(1) && g.version(c.tile) == Version(1)));

    let mut changed = Vec::new();
    // Inside one tile (256 m tiles from the min corner: 512..768 m east and
    // north of the centre).
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
        surface(&g, at(&frame, 570.0, 570.0, 100.0)).unwrap().height,
        30.0
    );
    assert_eq!(changed.len(), 1, "{changed:?}");
}

#[test]
fn edits_in_both_frames() {
    edits_carve_raise_and_version(planar());
    edits_carve_raise_and_version(geodetic());
}

#[test]
fn a_heightfield_says_what_it_cannot_hold() {
    let mut g = flat(planar(), 10.0);
    let before = surface(&g, DVec3::new(0.0, 0.0, 100.0));
    let mut changed = Vec::new();
    // A tunnel under the surface and a floating rock: no column changes.
    g.apply(
        &Edit {
            seq: EditSeq(1),
            op: EditOp::Remove,
            brush: Brush::Sphere {
                centre: DVec3::new(0.0, 0.0, -20.0),
                radius: 5.0,
            },
        },
        &mut changed,
    )
    .unwrap();
    g.apply(
        &Edit {
            seq: EditSeq(2),
            op: EditOp::Add,
            brush: Brush::Sphere {
                centre: DVec3::new(0.0, 0.0, 60.0),
                radius: 5.0,
            },
        },
        &mut changed,
    )
    .unwrap();
    assert_eq!(surface(&g, DVec3::new(0.0, 0.0, 100.0)), before);
}

#[test]
fn edits_out_of_order_or_out_of_bounds_change_nothing() {
    let mut g = flat(planar(), 0.0);
    let mut changed = Vec::new();
    let e = |seq| Edit {
        seq: EditSeq(seq),
        op: EditOp::Remove,
        brush: Brush::Sphere {
            centre: DVec3::ZERO,
            radius: 30.0,
        },
    };
    g.apply(&e(5), &mut changed).unwrap();
    let (digest, n) = (g.digest(), changed.len());
    assert_eq!(
        g.apply(&e(5), &mut changed),
        Err(GroundError::OutOfOrder {
            last: EditSeq(5),
            got: EditSeq(5)
        })
    );
    assert_eq!(
        g.apply(&e(3), &mut changed),
        Err(GroundError::OutOfOrder {
            last: EditSeq(5),
            got: EditSeq(3)
        })
    );
    let far = Edit {
        seq: EditSeq(9),
        op: EditOp::Add,
        brush: Brush::Sphere {
            centre: DVec3::new(90_000.0, 0.0, 0.0),
            radius: 10.0,
        },
    };
    assert_eq!(g.apply(&far, &mut changed), Err(GroundError::OutOfBounds));
    let nan = Edit {
        seq: EditSeq(10),
        op: EditOp::Add,
        brush: Brush::Sphere {
            centre: DVec3::new(f64::NAN, 0.0, 0.0),
            radius: 10.0,
        },
    };
    assert_eq!(g.apply(&nan, &mut changed), Err(GroundError::NotFinite));
    assert_eq!(
        (g.digest(), changed.len()),
        (digest, n),
        "refused edits leave no trace"
    );
}

fn arb_edit(seq: u64) -> impl Strategy<Value = Edit> {
    (
        -1500.0f64..1500.0,
        -1500.0f64..1500.0,
        -40.0f64..60.0,
        1.0f64..80.0,
        any::<bool>(),
        0u8..3,
    )
        .prop_map(move |(x, y, z, r, add, kind)| {
            let op = if add { EditOp::Add } else { EditOp::Remove };
            let p = DVec3::new(x, y, z);
            let brush = match kind {
                0 => Brush::Sphere {
                    centre: p,
                    radius: r,
                },
                1 => Brush::Capsule {
                    a: p,
                    b: p + DVec3::new(r, -r, 0.0),
                    radius: r / 2.0 + 1.0,
                },
                _ => Brush::Box {
                    min: p - DVec3::new(0.0, 0.0, r),
                    max: p + DVec3::new(r, r, 0.0),
                },
            };
            Edit {
                seq: EditSeq(seq),
                op,
                brush,
            }
        })
}

fn arb_edits() -> impl Strategy<Value = Vec<Edit>> {
    proptest::collection::vec(arb_edit(0), 1..12).prop_map(|mut v| {
        for (k, e) in v.iter_mut().enumerate() {
            e.seq = EditSeq(k as u64 + 1);
        }
        v
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Two replicas applying the same edits end with the same bits.
    #[test]
    fn replicas_agree(edits in arb_edits()) {
        let (mut a, mut b) = (ramp(planar()), ramp(planar()));
        let (mut ca, mut cb) = (Vec::new(), Vec::new());
        for e in &edits {
            prop_assert_eq!(a.apply(e, &mut ca).is_ok(), b.apply(e, &mut cb).is_ok());
        }
        prop_assert_eq!(a.digest(), b.digest());
        prop_assert_eq!(ca, cb);
    }

    /// Versions only grow, and a reported version is never ahead of the
    /// tile's current one.
    #[test]
    fn versions_are_monotonic(edits in arb_edits()) {
        let mut g = ramp(planar());
        let mut changed = Vec::new();
        for e in &edits {
            let start = changed.len();
            if g.apply(e, &mut changed).is_ok() {
                for c in &changed[start..] {
                    prop_assert!(c.version <= g.version(c.tile));
                    prop_assert!(c.version.0 >= 1);
                }
            }
        }
    }

    /// Edits on columns far apart commute: their order only shows in the
    /// sequence numbers, not in the heights.
    #[test]
    fn far_apart_edits_commute(a in arb_edit(1), b in arb_edit(2)) {
        let f = planar();
        let (la, ha) = a.brush.footprint(&f);
        let (lb, hb) = b.brush.footprint(&f);
        let gap = 32.0;
        prop_assume!(ha.x + gap < lb.x || hb.x + gap < la.x || ha.y + gap < lb.y || hb.y + gap < la.y);
        let (mut one, mut two) = (ramp(f), ramp(f));
        let mut c = Vec::new();
        one.apply(&a, &mut c).unwrap();
        one.apply(&b, &mut c).unwrap();
        two.apply(&Edit { seq: EditSeq(1), ..b }, &mut c).unwrap();
        two.apply(&Edit { seq: EditSeq(2), ..a }, &mut c).unwrap();
        let probes: Vec<DVec3> = (-15..15).flat_map(|i| (-15..15).map(move |j| DVec3::new(100.0 * f64::from(i), 100.0 * f64::from(j), 5000.0))).collect();
        let (mut s1, mut s2) = (vec![None; probes.len()], vec![None; probes.len()]);
        one.surface(&probes, &mut s1).unwrap();
        two.surface(&probes, &mut s2).unwrap();
        prop_assert_eq!(s1, s2);
    }
}
