// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The mist pass: one triangle over the picture, the field marched along
// every line of sight up to the ground the frame's depth says is there.
// Prepended are the field's uniform (`mist`, group 0 binding 0) and the
// medium (`tuile_atmosphere::mist::WGSL`).

@group(0) @binding(1) var frame_depth: /*DEPTH_TEXTURE*/;

struct MistOut {
    @builtin(position) clip: vec4f,
    @location(0) ndc: vec2f,
}

// One triangle over the whole viewport: (-1, -1), (3, -1), (-1, 3).
@vertex
fn vs_mist(@builtin(vertex_index) index: u32) -> MistOut {
    let xy = vec2f(f32((index << 1u) & 2u), f32(index & 2u)) * 2.0 - 1.0;
    var out: MistOut;
    out.clip = vec4f(xy, 0.5, 1.0);
    out.ndc = xy;
    return out;
}

@fragment
fn fs_mist(in: MistOut) -> @location(0) vec4f {
    // The frame's depth here, forward: 1 is the far plane, where nothing
    // was drawn and the march goes as far as the field reaches. Of the
    // samples of a pixel, the first: a veil's edge needs no finer.
    let depth = textureLoad(frame_depth, vec2i(in.clip.xy), 0);
    // The line of sight: a point on it between the near and far planes,
    // from the eye.
    let mid = mist.inverse_view_proj * vec4f(in.ndc, 0.5, 1.0);
    let direction = normalize(mid.xyz / mid.w - mist.eye.xyz);
    var farthest = mist.reach.x;
    if (depth != /*FAR_DEPTH*/) {
        let hit = mist.inverse_view_proj * vec4f(in.ndc, depth, 1.0);
        farthest = min(length(hit.xyz / hit.w - mist.eye.xyz), farthest);
    }
    return mist_march(mist.eye.xyz, direction, farthest, mist_jitter(in.clip.xy));
}
