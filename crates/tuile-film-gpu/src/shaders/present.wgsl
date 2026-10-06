// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Supersampled HDR → display pixels: box filter over the k×k block, sky where
// no tile was drawn, exposure, then the sRGB curve ("Standard" view: no tone
// mapping, a straight clip).

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var hdr: texture_2d<f32>;
@group(0) @binding(2) var vis: texture_2d<u32>;
@group(0) @binding(3) var out: texture_storage_2d<rgba8unorm, write>;

fn oetf(c: vec3f) -> vec3f {
    let x = clamp(c, vec3f(0.0), vec3f(1.0));
    return select(1.055 * pow(x, vec3f(1.0 / 2.4)) - 0.055, 12.92 * x, x <= vec3f(0.0031308));
}

@compute @workgroup_size(8, 8)
fn present(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(out);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let k = frame.size.z;
    var sum = vec3f(0.0);
    for (var j = 0u; j < k; j++) {
        for (var i = 0u; i < k; i++) {
            let s = id.xy * k + vec2u(i, j);
            if (textureLoad(vis, s, 0).x == 0u) {
                sum += frame.world.xyz;
            } else {
                sum += textureLoad(hdr, s, 0).rgb;
            }
        }
    }
    var linear = sum / f32(k * k) * frame.world.w;
    // The picture's own contrast and saturation: one setting for every
    // film, whatever its imagery. Luminance is raised to a power about
    // middle grey, colours keeping their ratios, then what is not
    // luminance is scaled.
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    if (frame.sun.w != 1.0) {
        linear *= pow(max(dot(linear, luma), 1e-5) / 0.18, frame.sun.w - 1.0);
    }
    if (frame.to_sun.w != 1.0) {
        let y = dot(linear, luma);
        linear = max(vec3f(y) + (linear - vec3f(y)) * frame.to_sun.w, vec3f(0.0));
    }
    textureStore(out, id.xy, vec4f(oetf(linear), 1.0));
}
