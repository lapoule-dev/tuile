// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Screen-space overlay, its one textured quad: a coverage mask (a line of
// text) drawn a texel to a pixel, inked in one colour.

struct Screen {
    // Viewport size in pixels; zw unused, kept for 16-byte alignment.
    size: vec4f,
}
@group(0) @binding(0) var<uniform> screen: Screen;

struct Mask {
    // Left, top, width, height, in pixels.
    rect: vec4f,
    // Straight RGBA.
    color: vec4f,
}
@group(1) @binding(0) var<uniform> mask: Mask;
@group(1) @binding(1) var coverage: texture_2d<f32>;
@group(1) @binding(2) var nearest: sampler;

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // Two triangles: (0,0) (1,0) (0,1) / (0,1) (1,0) (1,1).
    var corners = array<vec2f, 6>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
        vec2f(0.0, 1.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0),
    );
    let corner = corners[index];
    let pixel = mask.rect.xy + corner * mask.rect.zw;
    let ndc = pixel / max(screen.size.xy, vec2f(1.0)) * 2.0 - 1.0;
    var out: VsOut;
    out.clip = vec4f(ndc.x, -ndc.y, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let a = mask.color.a * textureSample(coverage, nearest, in.uv).r;
    // Premultiplied out, as the triangles are.
    return vec4f(mask.color.rgb * a, a);
}
