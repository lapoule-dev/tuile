// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A pyramid whose truth is known.
//!
//! One picture of ground, cut into a parent and its four children; the
//! children are then given the faults a capture has — another exposure,
//! another white balance, a tone that drifts across the tile — and the field
//! computed from them is asked to undo exactly that.

use tuile_radiometry::{
    apply, linear_of, transfer, BlockStats, GainField, Params, BLOCKS, LATTICE,
};

/// Ground with tone and detail: slow hills of colour, fine texture on top.
/// Linear light, as a function of place, so any level can be cut from it.
fn ground(x: f32, y: f32) -> [f32; 3] {
    let tone = 0.5 + 0.25 * (x * 3.1).sin() * (y * 2.3).cos();
    let detail = 0.06 * ((x * 180.0).sin() * (y * 150.0).sin());
    let base = (tone + detail).clamp(0.05, 0.9);
    [base * 0.45, base * 0.55, base * 0.30]
}

fn byte_of(linear: f32) -> u8 {
    let c = linear.clamp(0.0, 1.0);
    let encoded = if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5) as u8
}

/// A `side`² tile over the square `[x0, x0 + span] × [y0, y0 + span]`, each
/// texel multiplied by `fault(u, v)` — a capture's own way of seeing.
fn tile(side: u32, x0: f32, y0: f32, span: f32, fault: impl Fn(f32, f32) -> [f32; 3]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for row in 0..side {
        for col in 0..side {
            let (u, v) = (
                (col as f32 + 0.5) / side as f32,
                (row as f32 + 0.5) / side as f32,
            );
            let colour = ground(x0 + u * span, y0 + v * span);
            let gain = fault(u, v);
            for c in 0..3 {
                rgba.push(byte_of(colour[c] * gain[c]));
            }
            rgba.push(255);
        }
    }
    rgba
}

const SIDE: u32 = 256;

fn parent() -> Vec<u8> {
    tile(SIDE, 0.0, 0.0, 2.0, |_, _| [1.0; 3])
}

/// The child in the top-right quadrant of the parent.
fn child(fault: impl Fn(f32, f32) -> [f32; 3]) -> Vec<u8> {
    tile(SIDE, 1.0, 0.0, 1.0, fault)
}

const QUADRANT: (u32, u32) = (1, 0);

fn field_of(child_rgba: &[u8], parent_rgba: &[u8], params: &Params) -> GainField {
    let found = transfer(
        &BlockStats::of(child_rgba, SIDE, SIDE),
        &BlockStats::of_quadrant(parent_rgba, SIDE, SIDE, QUADRANT),
        params,
    );
    GainField::toward_parent(&found, params)
}

/// How far a child's tone is from its parent's, in stops: the largest gap
/// between their block means, over blocks and channels.
fn tone_gap(child_rgba: &[u8], parent_rgba: &[u8]) -> f32 {
    let (c, p) = (
        BlockStats::of(child_rgba, SIDE, SIDE),
        BlockStats::of_quadrant(parent_rgba, SIDE, SIDE, QUADRANT),
    );
    let mut worst = 0.0f32;
    for b in 0..BLOCKS * BLOCKS {
        for k in 0..3 {
            worst = worst.max((p.mean[b][k] / c.mean[b][k]).log2().abs());
        }
    }
    worst
}

#[test]
fn a_capture_with_another_exposure_and_white_balance_is_brought_back() {
    let params = Params::default();
    let parent = parent();
    // A stop darker, and warm: red up, blue well down.
    let fault = [0.5 * 1.25, 0.5, 0.5 * 0.7];
    let mut seen = child(|_, _| fault);
    assert!(
        tone_gap(&seen, &parent) > 0.9,
        "the fault is there to begin with"
    );

    let found = transfer(
        &BlockStats::of(&seen, SIDE, SIDE),
        &BlockStats::of_quadrant(&parent, SIDE, SIDE, QUADRANT),
        &params,
    );
    assert!(!found.same_source);
    for k in 0..3 {
        let wanted = (1.0 / fault[k]).log2();
        assert!(
            (found.overall[k] - wanted).abs() < 0.03,
            "channel {k}: {} stops found, {wanted} applied",
            found.overall[k]
        );
    }
    let field = GainField::toward_parent(&found, &params);
    apply(&mut seen, SIDE, SIDE, &field, 1.0);
    let after = tone_gap(&seen, &parent);
    assert!(after < 0.06, "{after} stops of tone still apart");
}

#[test]
fn a_tone_that_drifts_across_the_tile_is_followed() {
    let params = Params::default();
    let parent = parent();
    // Half a stop darker on the left, half a stop brighter on the right.
    let mut seen = child(|u, _| [2.0f32.powf(u - 0.5); 3]);
    let before = tone_gap(&seen, &parent);
    let field = field_of(&seen, &parent, &params);
    // The field leans the other way, and smoothly.
    let (left, right) = (field.at(0.05, 0.5)[1], field.at(0.95, 0.5)[1]);
    assert!(left > 0.3 && right < -0.3, "left {left}, right {right}");
    apply(&mut seen, SIDE, SIDE, &field, 1.0);
    let after = tone_gap(&seen, &parent);
    assert!(after < before / 4.0, "{before} stops before, {after} after");
}

#[test]
fn the_same_source_resampled_is_left_exactly_as_it_is() {
    let params = Params::default();
    let parent = parent();
    let mut seen = child(|_, _| [1.0; 3]);
    let untouched = seen.clone();
    let found = transfer(
        &BlockStats::of(&seen, SIDE, SIDE),
        &BlockStats::of_quadrant(&parent, SIDE, SIDE, QUADRANT),
        &params,
    );
    assert!(
        found.same_source,
        "agreement {}, gain {:?}",
        found.agreement, found.overall
    );
    let field = GainField::toward_parent(&found, &params);
    assert!(field.is_identity());
    apply(&mut seen, SIDE, SIDE, &field, 1.0);
    assert_eq!(seen, untouched, "not one byte may move");
}

#[test]
fn detail_is_kept_and_only_tone_moves() {
    let params = Params::default();
    let parent = parent();
    let mut seen = child(|_, _| [0.6, 0.6, 0.6]);
    // The fine texture, as the spread of a texel around its block's mean.
    let detail = |rgba: &[u8]| {
        let stats = BlockStats::of(rgba, SIDE, SIDE);
        let mut energy = 0.0f64;
        for row in 0..SIDE {
            for col in 0..SIDE {
                let block = (row as usize * BLOCKS / SIDE as usize) * BLOCKS
                    + col as usize * BLOCKS / SIDE as usize;
                let g = linear_of(rgba[((row * SIDE + col) * 4 + 1) as usize]);
                // Relative, so that a gain alone does not count as detail.
                energy += f64::from((g / stats.mean[block][1] - 1.0).powi(2));
            }
        }
        energy
    };
    let before = detail(&seen);
    let field = field_of(&seen, &parent, &params);
    apply(&mut seen, SIDE, SIDE, &field, 1.0);
    let after = detail(&seen);
    assert!(
        (after / before - 1.0).abs() < 0.05,
        "detail went from {before} to {after}"
    );
}

#[test]
fn ground_that_changed_does_not_drag_the_tile_with_it() {
    let params = Params::default();
    let parent = parent();
    // The capture is half a stop dark everywhere — and a corner of it is
    // something else entirely: snow, where the parent has grass.
    let mut seen = child(|u, v| {
        if u > 0.8 && v > 0.8 {
            [6.0; 3]
        } else {
            [0.7; 3]
        }
    });
    let field = field_of(&seen, &parent, &params);
    // Away from the corner the field is the half stop, not pulled off it.
    let wanted = (1.0f32 / 0.7).log2();
    let centre = field.at(0.3, 0.3)[1];
    assert!(
        (centre - wanted).abs() < 0.08,
        "{centre} stops at the centre, {wanted} wanted"
    );
    // In the corner itself the field is still the tile's — filled in from
    // around it — and not the three stops down the snow asks for: the snow
    // is ground that changed, and is left snow.
    let corner = field.at(0.93, 0.93)[1];
    assert!(
        (corner - wanted).abs() < 0.35,
        "{corner} stops in the changed corner, {wanted} around it"
    );
    // And nowhere does it go past its bound, whatever the corner asks.
    apply(&mut seen, SIDE, SIDE, &field, 1.0);
    assert!(field
        .nodes
        .iter()
        .flatten()
        .all(|n| (f32::from(*n) / 1024.0).abs() <= params.clamp_stops + 1e-3));
}

#[test]
fn a_field_chained_to_its_parents_reaches_the_anchor() {
    let params = Params::default();
    // The parent is itself a stop off its own parent; the child half a stop
    // off the parent. Chained, the child is a stop and a half off the anchor.
    let mut of_parent = GainField::identity();
    of_parent.nodes.iter_mut().for_each(|n| *n = [1024; 3]);
    let mut of_child = GainField::identity();
    of_child.nodes.iter_mut().for_each(|n| *n = [512; 3]);
    let chained = of_child.chained(&of_parent, (1, 1), &params);
    assert!(chained.nodes.iter().all(|n| *n == [1536; 3]));
    // Past the bound it stops at the bound.
    let far = chained.chained(&of_parent, (0, 0), &params);
    assert!(far.nodes.iter().all(|n| *n == [1536; 3]));
    // An identity parent changes nothing; and a parent's gradient is read
    // over the child's own quadrant, edge to edge.
    assert_eq!(
        of_child.chained(&GainField::identity(), (0, 1), &params),
        of_child
    );
    let mut sloped = GainField::identity();
    for j in 0..LATTICE {
        for i in 0..LATTICE {
            sloped.nodes[j * LATTICE + i] = [(i * 64) as i16; 3];
        }
    }
    let right = GainField::identity().chained(&sloped, (1, 0), &params);
    assert_eq!(
        right.nodes[0][0], 512,
        "the child's left edge is the parent's middle"
    );
    assert_eq!(
        right.nodes[LATTICE - 1][0],
        1024,
        "its right edge is the parent's"
    );
}

#[test]
fn a_field_is_its_bytes_and_the_same_every_time() {
    let params = Params::default();
    let parent = parent();
    let seen = child(|u, v| [0.5 + 0.3 * u, 0.6, 0.9 - 0.2 * v]);
    let field = field_of(&seen, &parent, &params);
    assert_eq!(field, field_of(&seen, &parent, &params));
    let bytes = field.to_bytes();
    assert_eq!(bytes.len(), LATTICE * LATTICE * 6);
    assert_eq!(GainField::from_bytes(&bytes), Some(field));
    assert_eq!(GainField::from_bytes(&bytes[1..]), None);
}

#[test]
fn half_the_strength_is_half_the_stops_and_none_is_nothing() {
    let mut whole = GainField::identity();
    whole.nodes.iter_mut().for_each(|n| *n = [1024, 0, -1024]);
    let grey = |v: u8| vec![v, v, v, 255];
    let mut none = grey(128);
    apply(&mut none, 1, 1, &whole, 0.0);
    assert_eq!(none, grey(128));
    let (mut half, mut full) = (grey(128), grey(128));
    apply(&mut half, 1, 1, &whole, 0.5);
    apply(&mut full, 1, 1, &whole, 1.0);
    let stops = |after: u8| (linear_of(after) / linear_of(128)).log2();
    assert!((stops(full[0]) - 1.0).abs() < 0.03 && (stops(half[0]) - 0.5).abs() < 0.03);
    assert_eq!(
        (full[1], half[1]),
        (128, 128),
        "a channel with no gain keeps its byte"
    );
    assert!((stops(full[2]) + 1.0).abs() < 0.03);
    assert_eq!(full[3], 255);
}
