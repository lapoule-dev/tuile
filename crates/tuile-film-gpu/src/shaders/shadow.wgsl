// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The frame's tiles as the sun sees them, depth alone: what stands between
// the ground and the sun. The same vertices as the visibility raster,
// through the sun's view in place of the camera's.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> tiles: array<TileFrame>;

@group(1) @binding(0) var<storage, read> positions: array<f32>;
@group(1) @binding(3) var<storage, read> indices: array<u32>;

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) slot: u32) -> @builtin(position) vec4f {
    let i = indices[v];
    let p = vec3f(positions[3u * i], positions[3u * i + 1u], positions[3u * i + 2u]);
    var clip = frame.sun_view_proj * vec4f(p + tiles[slot].offset, 1.0);
    // The map's planes hold the ground that receives. Ground nearer the
    // sun than the near one casts all the same: it is laid flat on that
    // plane, where clipping would have cut it away.
    clip.z = max(clip.z, 0.0);
    return clip;
}
