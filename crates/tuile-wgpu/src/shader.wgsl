// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Minimal PBR-ish tile shader: base color texture × factor, lambert + ambient.

struct ViewUniform {
    view_proj: mat4x4f,
    // xyz = direction the light travels (normalized), w unused.
    sun_dir: vec4f,
    // x = ambient amount, yzw unused.
    params: vec4f,
}
@group(0) @binding(0) var<uniform> view: ViewUniform;

struct TileUniform {
    model: mat4x4f,
}
@group(1) @binding(0) var<uniform> tile: TileUniform;

struct MaterialUniform {
    base_color: vec4f,
}
@group(2) @binding(0) var<uniform> material: MaterialUniform;
@group(2) @binding(1) var base_tex: texture_2d<f32>;
@group(2) @binding(2) var base_samp: sampler;

// Imagery draped over the tile, blended on top of the base colour.
//
// A layer is two vec4, so that the whole set is one uniform array with the
// natural 16-byte stride:
//   [2i]     coverage:  u_min, v_min, u_max, v_max — in the TILE's uv space
//   [2i + 1] placement: translation.xy, scale.xy
// and `texture_uv = tile_uv * scale + translation`.
//
// The slot count must equal `tuile_core::raster::MAX_IMAGERY_LAYERS`; a test in
// this crate reads it back out of this file and asserts they agree.
const IMAGERY_LAYERS: u32 = 12u;
struct ImageryUniform {
    layers: array<vec4f, 24>,
}
@group(2) @binding(3) var<uniform> imagery: ImageryUniform;
@group(2) @binding(4) var img0: texture_2d<f32>;
@group(2) @binding(5) var img1: texture_2d<f32>;
@group(2) @binding(6) var img2: texture_2d<f32>;
@group(2) @binding(7) var img3: texture_2d<f32>;
@group(2) @binding(8) var img4: texture_2d<f32>;
@group(2) @binding(9) var img5: texture_2d<f32>;
@group(2) @binding(10) var img6: texture_2d<f32>;
@group(2) @binding(11) var img7: texture_2d<f32>;
@group(2) @binding(12) var img8: texture_2d<f32>;
@group(2) @binding(13) var img9: texture_2d<f32>;
@group(2) @binding(14) var img10: texture_2d<f32>;
@group(2) @binding(15) var img11: texture_2d<f32>;

struct VsIn {
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) normal: vec3f,
    @location(1) uv: vec2f,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let world = tile.model * vec4f(in.position, 1.0);
    out.clip = view.view_proj * world;
    out.normal = (tile.model * vec4f(in.normal, 0.0)).xyz;
    out.uv = in.uv;
    return out;
}

/// Blends one imagery layer over what is already there.
///
/// Branch-free on purpose, twice over. The coverage test is `step` rather than
/// an `if` because a discarded layer must still not diverge the control flow
/// `textureSample` sits in; and an *unused* slot needs no test of its own,
/// because it carries an empty coverage rectangle and so masks itself out. That
/// is why the sampler reads a 1×1 white texture in unused slots rather than
/// nothing: it costs a guaranteed cache hit and keeps this straight-line.
fn blend_layer(dst: vec3f, tex: texture_2d<f32>, uv: vec2f, slot: u32) -> vec3f {
    let coverage = imagery.layers[2u * slot];
    let placement = imagery.layers[2u * slot + 1u];
    let above = step(coverage.xy, uv);
    let below = step(uv, coverage.zw);
    let mask = above.x * above.y * below.x * below.y;
    // Outside the coverage rectangle this reads past the layer's own edge and
    // the clamping sampler smears it — which never shows, because that is
    // exactly where the mask is zero.
    let texel = textureSample(tex, base_samp, uv * placement.zw + placement.xy);
    return mix(dst, texel.rgb, mask * texel.a);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    var albedo = textureSample(base_tex, base_samp, in.uv) * material.base_color;
    // Content that owns its texturing has no layers, and terrain has no base
    // colour texture — the two never both contribute, but nothing here needs to
    // know which case it is in.
    var ground = albedo.rgb;
    ground = blend_layer(ground, img0, in.uv, 0u);
    ground = blend_layer(ground, img1, in.uv, 1u);
    ground = blend_layer(ground, img2, in.uv, 2u);
    ground = blend_layer(ground, img3, in.uv, 3u);
    ground = blend_layer(ground, img4, in.uv, 4u);
    ground = blend_layer(ground, img5, in.uv, 5u);
    ground = blend_layer(ground, img6, in.uv, 6u);
    ground = blend_layer(ground, img7, in.uv, 7u);
    ground = blend_layer(ground, img8, in.uv, 8u);
    ground = blend_layer(ground, img9, in.uv, 9u);
    ground = blend_layer(ground, img10, in.uv, 10u);
    ground = blend_layer(ground, img11, in.uv, 11u);
    albedo = vec4f(ground, albedo.a);

    let n = normalize(in.normal);
    let lambert = max(dot(n, -view.sun_dir.xyz), 0.0);
    let ambient = view.params.x;
    let lit = albedo.rgb * (ambient + (1.0 - ambient) * lambert);
    return vec4f(lit, albedo.a);
}
