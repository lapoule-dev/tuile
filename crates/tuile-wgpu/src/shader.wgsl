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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let albedo = textureSample(base_tex, base_samp, in.uv) * material.base_color;
    let n = normalize(in.normal);
    let lambert = max(dot(n, -view.sun_dir.xyz), 0.0);
    let ambient = view.params.x;
    let lit = albedo.rgb * (ambient + (1.0 - ambient) * lambert);
    return vec4f(lit, albedo.a);
}
