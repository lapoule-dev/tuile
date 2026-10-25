// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The sky: the physical atmosphere on every line of sight, wherever nothing
// stands in it — `sky_seen`, the film's own, with the sun's disc, the Moon in
// its phase and the stars of the instant, graded for the display.
//
// Drawn first, over the whole picture, on the far plane and writing nothing:
// everything drawn after it covers it, and anything a host drew before tuile
// composed into its pass stays in front of it. Prepended here are the
// atmosphere's bindings (group 1), `hillaire.wgsl` and `seen.wgsl`.

struct SkyUniform {
    // `view_proj`, inverted: from a pixel back to the line of sight.
    inverse_view_proj: mat4x4f,
    // x: the angular size of a pixel, radians — what a disc's edge is
    // spread over and a star's image is as wide as; yzw: nothing.
    stars: vec4f,
}
@group(0) @binding(0) var<uniform> sky: SkyUniform;

// The depth of the far plane in the target's convention — 1 forward, 0
// reversed — set from the Rust side when the module is built.
const SKY_DEPTH: f32 = /*SKY_DEPTH*/;

struct SkyOut {
    @builtin(position) clip: vec4f,
    @location(0) ndc: vec2f,
}

// One triangle over the whole viewport: (-1, -1), (3, -1), (-1, 3), on the
// far plane. Tested against depth, it is drawn only where nothing nearer
// already is: everywhere in a frame of tuile's own, which clears to far, and
// never over what a host drew before tuile composed into its pass.
@vertex
fn vs_sky(@builtin(vertex_index) index: u32) -> SkyOut {
    let xy = vec2f(f32((index << 1u) & 2u), f32(index & 2u)) * 2.0 - 1.0;
    var out: SkyOut;
    out.clip = vec4f(xy, SKY_DEPTH, 1.0);
    out.ndc = xy;
    return out;
}

@fragment
fn fs_sky(in: SkyOut) -> @location(0) vec4f {
    // A point on the line of sight between the near and far planes, in
    // either depth convention — finite even under an infinite far plane —
    // from the eye.
    let h = sky.inverse_view_proj * vec4f(in.ndc, 0.5, 1.0);
    let d = h.xyz / h.w - atmosphere.eye.xyz;
    return vec4f(graded(sky_seen(d, sky.stars.x)), 1.0);
}
