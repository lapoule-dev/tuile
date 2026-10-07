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
    // The grade of the layer's level, applied in linear light: a colour
    // taken away (rgb of `black`), a gain per channel (rgb of `gain`), a
    // power on luminance about a pivot (`gain.w` about `more.x`), a factor
    // on what is not luminance (`black.w`). `more.y` is 0 for a layer that
    // is not graded: its stored bytes go through untouched.
    gain: vec4f,
    black: vec4f,
    more: vec4f,
    // A correction carried by the imagery tile's four corners and blended
    // across it, used in place of the grade when `more.z` is 1. Nine
    // vectors a corner — top-left, top-right, bottom-left, bottom-right:
    //   0  gain in stops a channel; w: contrast, as stops of its power
    //   1  the colour taken away, in the light the gain leaves; w:
    //      saturation, as stops of its factor
    //   2  x: the pivot of the contrast, as its stops under white
    //   3–8 the transfer curve of red, green, blue, two vectors each: stops
    //      added to a texel by the light it came in with, at −8 … −1 stops
    // When `more.w` is 1 a corner carries a colour matrix instead, in its
    // first three vectors: a row a channel out — red, green, blue in, then
    // what is added.
    corners: array<vec4f, 36>,
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

// The layer's level brought to the anchor's: black point, gain, contrast,
// saturation. Every texel of every tile of a level goes through the same
// curve, so two tiles of one level meet as they met before.
fn toned(bytes: vec3f) -> vec3f {
    if (job.more.y == 0.0) { return bytes; }
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    var lit = max(linear_of(bytes / 255.0) - job.black.rgb, vec3f(0.0)) * job.gain.rgb;
    lit *= pow(max(dot(lit, luma), 1e-5) / max(job.more.x, 1e-5), job.gain.w - 1.0);
    let y = dot(lit, luma);
    lit = max(vec3f(y) + (lit - vec3f(y)) * job.black.w, vec3f(0.0));
    return round(stored_of(min(lit, vec3f(1.0))) * 255.0);
}

// One of a corner's vectors, blended across the imagery tile: `t` is where
// the texel lies in it. Everything is blended as it is given — stops as
// stops — and only then made into gains and powers, so that along an edge
// two tiles share, the same two corners give the same texel.
fn blended(k: u32, t: vec2f) -> vec4f {
    let top = mix(job.corners[k], job.corners[9u + k], t.x);
    let bottom = mix(job.corners[18u + k], job.corners[27u + k], t.x);
    return mix(top, bottom, t.y);
}

// A transfer curve at a light, both in stops: eight points from −8 to −1,
// the line between two of them, the end's value beyond.
fn along(low: vec4f, high: vec4f, stops: f32) -> f32 {
    var points = array<f32, 8>(low.x, low.y, low.z, low.w, high.x, high.y, high.z, high.w);
    let at = clamp(stops + 8.0, 0.0, 7.0);
    let below = min(u32(floor(at)), 6u);
    return mix(points[below], points[below + 1u], at - f32(below));
}

// A texel through the field: the gain, the colour taken away, the transfer
// curve read at the light the texel came in with, the power on luminance
// about the pivot, the factor on what is not luminance.
fn fielded(bytes: vec3f, t: vec2f) -> vec3f {
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    let a = blended(0u, t);
    let b = blended(1u, t);
    let gain = exp2(a.xyz);
    var lit = max(linear_of(bytes / 255.0) * gain - b.xyz, vec3f(0.0));
    let came = log2(max(lit / gain, vec3f(1e-9)));
    lit *= exp2(vec3f(
        along(blended(3u, t), blended(4u, t), came.x),
        along(blended(5u, t), blended(6u, t), came.y),
        along(blended(7u, t), blended(8u, t), came.z),
    ));
    let pivot = exp2(blended(2u, t).x);
    lit *= pow(max(dot(lit, luma), 1e-5) / max(pivot, 1e-5), exp2(a.w) - 1.0);
    let y = dot(lit, luma);
    lit = max(vec3f(y) + (lit - vec3f(y)) * exp2(b.w), vec3f(0.0));
    return round(stored_of(min(lit, vec3f(1.0))) * 255.0);
}

// A texel through the matrix its place in the tile gives it: the four
// corners' matrices blended, then applied in linear light. A function of
// where the texel is and of what colour it has.
fn matrixed(bytes: vec3f, t: vec2f) -> vec3f {
    let lit = vec4f(linear_of(bytes / 255.0), 1.0);
    let made = vec3f(
        dot(blended(0u, t), lit),
        dot(blended(1u, t), lit),
        dot(blended(2u, t), lit),
    );
    return round(stored_of(clamp(made, vec3f(0.0), vec3f(1.0))) * 255.0);
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
    if (job.more.w == 1.0) {
        textureStore(dst, at, vec4f(matrixed(texel.rgb, t) / 255.0, 1.0));
        return;
    }
    if (job.more.z == 1.0) {
        textureStore(dst, at, vec4f(fielded(texel.rgb, t) / 255.0, 1.0));
        return;
    }
    textureStore(dst, at, vec4f(toned(texel.rgb) / 255.0, 1.0));
}
