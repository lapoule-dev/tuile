// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// One imagery layer, blended over what is already there.
//
// The GPU half of `imagery_layer_table`, and it lives beside it because the
// two are one contract: the packing, the empty-coverage sentinel and the
// masking rule have to agree exactly, and a renderer that disagrees loses its
// sharpest layers in silence.
//
// That contract used to be checked by a test that read the backend's `.wgsl`
// file with a regular expression. Adjacency is a better check than a regex.
//
// Depends on nothing but its arguments — no bindings, no globals. Reading the
// table out of a binding is the one thing a backend cannot share, so it is the
// one thing left to the backend.

/// Blends one imagery layer over what is already there.
///
/// Branch-free on purpose, twice over. The coverage test is `step` rather than
/// an `if` because a discarded layer must still not diverge the control flow
/// `textureSample` sits in; and an *unused* slot needs no test of its own,
/// because it carries an empty coverage rectangle and so masks itself out. That
/// is why the sampler reads a 1×1 white texture in unused slots rather than
/// nothing: it costs a guaranteed cache hit and keeps this straight-line.
fn blend_layer(
    dst: vec4f,
    tex: texture_2d<f32>,
    samp: sampler,
    uv: vec2f,
    coverage: vec4f,
    placement: vec4f,
) -> vec4f {
    let above = step(coverage.xy, uv);
    let below = step(uv, coverage.zw);
    let mask = above.x * above.y * below.x * below.y;
    // Outside the coverage rectangle this reads past the layer's own edge and
    // the clamping sampler smears it — which never shows, because that is
    // exactly where the mask is zero.
    let texel = textureSample(tex, samp, uv * placement.zw + placement.xy);
    // `w` accumulates how many layers actually covered this fragment. It costs
    // one add and it is the only way to tell, at a pixel, "no imagery reached
    // here" from "imagery reached here and is dark" — the two look identical
    // and have nothing in common. `DIAGNOSTIC_COVERAGE` reads it.
    return vec4f(mix(dst.rgb, texel.rgb, mask * texel.a), dst.a + mask);
}

/// One slot, read out of the bound table.
///
/// The globals live here rather than inside [`blend_layer`] on purpose: reading
/// a binding is the one thing a second backend cannot share, so it is the one
