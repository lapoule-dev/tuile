// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// A host's shapes: coloured triangles through the frame's own camera, into
// a target of their own at the supersampled size. Their vertices come in
// eye-relative already — the origin of each mesh met the eye in f64 before
// anything was narrowed — and their colours display-linear and
// premultiplied, which is how they are blended here and laid over the
// picture by `present`.

@group(0) @binding(0) var<uniform> frame: Frame;

struct Out {
    @builtin(position) clip: vec4f,
    @location(0) color: vec4f,
}

@vertex
fn vs(@location(0) position: vec3f, @location(1) color: vec4f) -> Out {
    var out: Out;
    out.clip = frame.view_proj * vec4f(position, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4f {
    return in.color;
}
