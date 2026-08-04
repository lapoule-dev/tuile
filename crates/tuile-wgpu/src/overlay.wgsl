// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Screen-space overlay: 2-D triangles given in pixels, painted over the scene.

struct Screen {
    // Viewport size in pixels; zw unused, kept for 16-byte alignment.
    size: vec4f,
}
@group(0) @binding(0) var<uniform> screen: Screen;

struct VsIn {
    @location(0) position: vec2f,
    @location(1) color: vec4f,
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) color: vec4f,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    // Pixels (origin top-left, y down) → clip space (origin centre, y up).
    let ndc = in.position / max(screen.size.xy, vec2f(1.0)) * 2.0 - 1.0;
    out.clip = vec4f(ndc.x, -ndc.y, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // Straight alpha in, premultiplied out: the pipeline blends with One,
    // OneMinusSrcAlpha, which is the form that composites correctly when
    // translucent shapes overlap each other.
    return vec4f(in.color.rgb * in.color.a, in.color.a);
}
