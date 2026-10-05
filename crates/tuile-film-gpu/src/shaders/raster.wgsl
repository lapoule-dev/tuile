// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The only raster pass: which triangle of which tile covers each pixel.
// Nothing is shaded here. The vertex stage fetches its own indices so the
// triangle id is simply `vertex_index / 3`, without the non-portable
// primitive-index builtin.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> tiles: array<TileFrame>;

@group(1) @binding(0) var<storage, read> positions: array<f32>;
@group(1) @binding(3) var<storage, read> indices: array<u32>;

struct Out {
    @builtin(position) clip: vec4f,
    @location(0) @interpolate(flat) id: vec2u,
}

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) slot: u32) -> Out {
    let i = indices[v];
    let p = vec3f(positions[3u * i], positions[3u * i + 1u], positions[3u * i + 2u]);
    var out: Out;
    out.clip = frame.view_proj * vec4f(p + tiles[slot].offset, 1.0);
    // Slot + 1, so that 0 is "no tile": the sky.
    out.id = vec2u(slot + 1u, v / 3u);
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec2u {
    return in.id;
}
