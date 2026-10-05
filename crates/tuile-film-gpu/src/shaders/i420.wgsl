// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Display pixels → planar YUV 4:2:0, BT.709, limited range: what an H.264
// encoder takes, at half the bytes of RGBA. Four bytes per thread, packed
// into one u32. The width must be a multiple of 8 and the height of 2.

@group(0) @binding(0) var rgb: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> planes: array<u32>;

fn luma(c: vec3f) -> f32 {
    return dot(c, vec3f(0.2126, 0.7152, 0.0722));
}

fn byte(x: f32) -> u32 {
    return u32(clamp(round(x), 0.0, 255.0));
}

@compute @workgroup_size(8, 8)
fn y_plane(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(rgb);
    if (id.x * 4u >= size.x || id.y >= size.y) { return; }
    var word = 0u;
    for (var i = 0u; i < 4u; i++) {
        let c = textureLoad(rgb, vec2u(id.x * 4u + i, id.y), 0).rgb;
        word |= byte(16.0 + 219.0 * luma(c)) << (8u * i);
    }
    planes[(id.y * size.x) / 4u + id.x] = word;
}

@compute @workgroup_size(8, 8)
fn uv_planes(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(rgb);
    let half = size / 2u;
    if (id.x * 4u >= half.x || id.y >= half.y) { return; }
    var u = 0u;
    var v = 0u;
    for (var i = 0u; i < 4u; i++) {
        let p = vec2u((id.x * 4u + i) * 2u, id.y * 2u);
        let c = (textureLoad(rgb, p, 0).rgb + textureLoad(rgb, p + vec2u(1u, 0u), 0).rgb
            + textureLoad(rgb, p + vec2u(0u, 1u), 0).rgb + textureLoad(rgb, p + vec2u(1u, 1u), 0).rgb) * 0.25;
        let y = luma(c);
        u |= byte(128.0 + 224.0 * (c.b - y) / 1.8556) << (8u * i);
        v |= byte(128.0 + 224.0 * (c.r - y) / 1.5748) << (8u * i);
    }
    let y_words = size.x * size.y / 4u;
    let row = half.x / 4u;
    planes[y_words + id.y * row + id.x] = u;
    planes[y_words + half.x * half.y / 4u + id.y * row + id.x] = v;
}
