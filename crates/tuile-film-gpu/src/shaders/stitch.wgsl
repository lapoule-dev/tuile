// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// A tile's vertices, put where stitching has them.
//
// The rule is not here: `tuile_core::stitch::plan` decides, from the tiles a
// frame draws, how far each side of each tile is from the line it has to be
// on, and hands that over as strips — a few knots a side, linear between
// them. This adds them up for each vertex, by where it is in its tile: the
// whole correction on an edge, fading to nothing over the band inward.
//
// It runs once for a tile each time its neighbourhood changes, and writes
// the positions every stage then reads — the visibility raster, the sun's
// depth pass, the resolve that meets a pixel's ray with its triangle again.
// They read one buffer, so none of them can draw, shade or shadow a ground
// the others do not have; and a frame whose tiles keep their neighbours
// costs nothing more than it did. `Stitch::offset` is the same arithmetic
// on the CPU, operation for operation; a test holds the two together.
//
// `stitches`, four numbers a row (`Stitch::packed`):
//   0      knots on the west, south, east and north sides
//   1      the row each side's knots begin at
//   2..5   the correction at the south-west, south-east, north-east and
//          north-west corners
//   6..9   how far the west, south, east and north sides go past the
//          edge they are put on: a hair, so that rounding laps and never
//          gaps
//   10..   the knots: where along the side, then the correction

@group(0) @binding(0) var<storage, read> positions: array<f32>;
@group(0) @binding(1) var<storage, read> uvs: array<f32>;
@group(0) @binding(2) var<storage, read> stitches: array<vec4f>;
@group(0) @binding(3) var<storage, read_write> stitched: array<f32>;

// Set from `tuile_core::stitch::BAND` when the module is built.
const STITCH_BAND: f32 = /*BAND*/;

fn stitch_weight(across: f32) -> f32 {
    return clamp(1.0 - across / STITCH_BAND, 0.0, 1.0);
}

fn stitch_side(side: u32, s: f32) -> vec3f {
    let count = u32(stitches[0][side]);
    if (count == 0u) { return vec3f(0.0); }
    let start = u32(stitches[1][side]);
    var low = 0u;
    var high = count;
    while (low < high) {
        let mid = (low + high) / 2u;
        if (stitches[start + mid].x <= s) { low = mid + 1u; } else { high = mid; }
    }
    if (low == 0u) { return stitches[start].yzw; }
    if (low == count) { return stitches[start + count - 1u].yzw; }
    let a = stitches[start + low - 1u];
    let b = stitches[start + low];
    let t = (s - a.x) / (b.x - a.x);
    return a.yzw + (b.yzw - a.yzw) * t;
}

fn stitch_offset(uv: vec2f) -> vec3f {
    let u = uv.x;
    let north = 1.0 - uv.y;
    let w = vec4f(
        stitch_weight(u),
        stitch_weight(north),
        stitch_weight(1.0 - u),
        stitch_weight(1.0 - north),
    );
    if (all(w == vec4f(0.0))) { return vec3f(0.0); }
    return w.x * (stitch_side(0u, north) + stitches[6].xyz)
        + w.y * (stitch_side(1u, u) + stitches[7].xyz)
        + w.z * (stitch_side(2u, north) + stitches[8].xyz)
        + w.w * (stitch_side(3u, u) + stitches[9].xyz)
        - (w.x * w.y) * stitches[2].xyz
        - (w.y * w.z) * stitches[3].xyz
        - (w.z * w.w) * stitches[4].xyz
        - (w.w * w.x) * stitches[5].xyz;
}

@compute @workgroup_size(64)
fn displace(@builtin(global_invocation_id) id: vec3u) {
    let i = id.x;
    if (3u * i + 2u >= arrayLength(&stitched)) { return; }
    let p = vec3f(positions[3u * i], positions[3u * i + 1u], positions[3u * i + 2u])
        + stitch_offset(vec2f(uvs[2u * i], uvs[2u * i + 1u]));
    stitched[3u * i] = p.x;
    stitched[3u * i + 1u] = p.y;
    stitched[3u * i + 2u] = p.z;
}
