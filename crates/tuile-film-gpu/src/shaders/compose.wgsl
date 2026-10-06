// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// One layer of a drape, written into the tile's texture.
//
// A drape is a stack of imagery tiles, each covering a rectangle of the
// terrain tile's uv and placed in it by a scale and a translation. Composing
// it is, texel by texel: which layer covers this texel, and what does that
// layer hold there. This is the bake's own arithmetic (`bake_layers`), one
// dispatch per layer, bottom layer first — so a later layer writes over an
// earlier one exactly where the bake let it win.
//
// Nothing is read back from the destination: storage textures of this format
// are write-only on the web, and none of it is needed. An imagery tile is
// opaque, so a layer replaces what is under it; a stack with a translucent
// layer is not given to this pass.
//
// Values are the stored, encoded ones, filtered as stored: that is what the
// bake does, and matching the bake is the point.

struct Job {
    // The part of the tile's uv this layer covers: u0, v0, u1, v1.
    coverage: vec4f,
    // uv of the imagery tile = uv of the tile * scale + translation.
    translation: vec2f,
    scale: vec2f,
    // The colour of a tile where no layer reaches.
    base: vec4f,
    // Where the dispatch begins in the destination: only the layer's own
    // rectangle is visited.
    origin: vec2u,
    // 0: fill with `base`. 1: write the layer.
    mode: u32,
    pad: u32,
    // What the layer's colour is multiplied by, in linear light: the tone
    // correction of the layer's level. Exactly 1, and the stored bytes go
    // through untouched.
    gain: vec4f,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> job: Job;

// A byte's worth, as the bake holds it between two steps: truncated.
fn step_down(a: vec4f, b: vec4f, f: f32) -> vec4f {
    return floor(a + (b - a) * f);
}

fn bytes(at: vec2u) -> vec4f {
    return round(textureLoad(src, at, 0) * 255.0);
}

fn linear_of(c: vec3f) -> vec3f {
    return select(pow((c + 0.055) / 1.055, vec3f(2.4)), c / 12.92, c <= vec3f(0.04045));
}

fn stored_of(c: vec3f) -> vec3f {
    return select(1.055 * pow(c, vec3f(1.0 / 2.4)) - 0.055, c * 12.92, c <= vec3f(0.0031308));
}

// The layer's level brought to the tone of the anchor level. Every texel of
// every tile of a level goes through the same curve, so two tiles of one
// level meet as they met before.
fn toned(bytes: vec3f) -> vec3f {
    if (all(job.gain.rgb == vec3f(1.0))) { return bytes; }
    let lit = min(linear_of(bytes / 255.0) * job.gain.rgb, vec3f(1.0));
    return round(stored_of(lit) * 255.0);
}

@compute @workgroup_size(8, 8)
fn compose(@builtin(global_invocation_id) id: vec3u) {
    let at = id.xy + job.origin;
    let size = textureDimensions(dst);
    if (at.x >= size.x || at.y >= size.y) { return; }
    if (job.mode == 0u) {
        let base = floor(clamp(job.base.rgb, vec3f(0.0), vec3f(1.0)) * 255.0 + 0.5);
        textureStore(dst, at, vec4f(base / 255.0, 1.0));
        return;
    }
    let uv = (vec2f(at) + 0.5) / vec2f(size);
    if (uv.x < job.coverage.x || uv.y < job.coverage.y
        || uv.x > job.coverage.z || uv.y > job.coverage.w) {
        return;
    }
    let t = clamp(uv * job.scale + job.translation, vec2f(0.0), vec2f(1.0));
    let last = textureDimensions(src) - 1u;
    let f = t * vec2f(last);
    let p0 = vec2u(floor(f));
    let p1 = min(p0 + 1u, last);
    let w = f - floor(f);
    let top = step_down(bytes(p0), bytes(vec2u(p1.x, p0.y)), w.x);
    let bottom = step_down(bytes(vec2u(p0.x, p1.y)), bytes(p1), w.x);
    let texel = step_down(top, bottom, w.y);
    textureStore(dst, at, vec4f(toned(texel.rgb) / 255.0, 1.0));
}
