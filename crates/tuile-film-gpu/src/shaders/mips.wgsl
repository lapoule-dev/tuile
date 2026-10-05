// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// One mip level from the one above it, averaged in linear light.
//
// The source is read through an sRGB view, so loads come back linear; the
// destination is the same texture's plain-unorm view (sRGB formats cannot be
// storage), so the curve is applied here by hand.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;

fn oetf(c: vec3f) -> vec3f {
    let x = clamp(c, vec3f(0.0), vec3f(1.0));
    return select(1.055 * pow(x, vec3f(1.0 / 2.4)) - 0.055, 12.92 * x, x <= vec3f(0.0031308));
}

@compute @workgroup_size(8, 8)
fn downsample(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(dst);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let last = textureDimensions(src) - 1u;
    var sum = vec4f(0.0);
    for (var j = 0u; j < 2u; j++) {
        for (var i = 0u; i < 2u; i++) {
            sum += textureLoad(src, min(id.xy * 2u + vec2u(i, j), last), 0);
        }
    }
    let c = sum * 0.25;
    textureStore(dst, id.xy, vec4f(oetf(c.rgb), c.a));
}
