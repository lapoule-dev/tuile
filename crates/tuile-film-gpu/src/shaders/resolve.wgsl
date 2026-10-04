// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Shades one tile's pixels. Dispatched indirectly, once per visible tile, with
// that tile's geometry and texture bound.
//
// Each pixel re-intersects its own triangle with its own ray, which gives
// exact perspective-correct barycentrics — and, from the rays of the next
// pixel right and down on the same plane, exact UV gradients for anisotropic
// filtering. No fragment-stage derivative is needed, so this runs in compute.

const RESOLVE_GROUP: u32 = 64u;

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> tiles: array<TileFrame>;
@group(0) @binding(2) var<storage, read> counts: array<u32>;
@group(0) @binding(3) var<storage, read> starts: array<u32>;
@group(0) @binding(4) var<storage, read> list: array<u32>;
@group(0) @binding(5) var vis: texture_2d<u32>;
@group(0) @binding(6) var hdr: texture_storage_2d<rgba16float, write>;
@group(0) @binding(7) var samp: sampler;

@group(1) @binding(0) var<storage, read> positions: array<f32>;
@group(1) @binding(1) var<storage, read> normals: array<f32>;
@group(1) @binding(2) var<storage, read> uvs: array<f32>;
@group(1) @binding(3) var<storage, read> indices: array<u32>;
@group(1) @binding(4) var albedo: texture_2d<f32>;

struct Slot { slot: u32 }
@group(2) @binding(0) var<uniform> current: Slot;

fn ray(px: vec2f) -> vec3f {
    let size = vec2f(frame.size.xy);
    let ndc = vec2f(px.x / size.x * 2.0 - 1.0, 1.0 - px.y / size.y * 2.0);
    return frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz;
}

// Barycentrics (b1, b2) of where a ray from the eye meets the triangle's
// plane. Not clamped: a neighbour's ray may leave the triangle, and the
// gradient still wants where it meets the plane.
fn bary(d: vec3f, a: vec3f, e1: vec3f, e2: vec3f) -> vec2f {
    let p = cross(d, e2);
    let inv = 1.0 / dot(e1, p);
    let t = -a;
    let q = cross(t, e1);
    return vec2f(dot(t, p), dot(d, q)) * inv;
}

fn position_at(i: u32) -> vec3f {
    return vec3f(positions[3u * i], positions[3u * i + 1u], positions[3u * i + 2u]);
}

fn normal_at(i: u32) -> vec3f {
    return vec3f(normals[3u * i], normals[3u * i + 1u], normals[3u * i + 2u]);
}

fn uv_at(i: u32) -> vec2f {
    return vec2f(uvs[2u * i], uvs[2u * i + 1u]);
}

fn lerp3(b: vec2f, x: vec2f, y: vec2f, z: vec2f) -> vec2f {
    return x * (1.0 - b.x - b.y) + y * b.x + z * b.y;
}

@compute @workgroup_size(RESOLVE_GROUP)
fn resolve(@builtin(global_invocation_id) id: vec3u) {
    let slot = current.slot;
    if (id.x >= counts[slot]) { return; }
    let pixel = list[starts[slot] + id.x];
    let w = frame.size.x;
    let xy = vec2u(pixel % w, pixel / w);
    let tri = textureLoad(vis, xy, 0).y;
    let tile = tiles[slot];

    let i0 = indices[3u * tri];
    let i1 = indices[3u * tri + 1u];
    let i2 = indices[3u * tri + 2u];
    // Edges in the tile's own frame, where its coordinates are smallest.
    let p0 = position_at(i0);
    let a = p0 + tile.offset;
    let e1 = position_at(i1) - p0;
    let e2 = position_at(i2) - p0;

    let px = vec2f(xy) + 0.5;
    let d = ray(px);
    let b = bary(d, a, e1, e2);

    var normal = normalize(cross(e1, e2));
    if ((tile.flags & HAS_NORMALS) != 0u) {
        let n = normal_at(i0) * (1.0 - b.x - b.y)
            + normal_at(i1) * b.x
            + normal_at(i2) * b.y;
        normal = normalize(n);
    }
    // Seen from behind, a surface is lit as its visible side.
    if (dot(normal, d) > 0.0) { normal = -normal; }

    var base = tile.factor.rgb;
    if ((tile.flags & (HAS_UVS | HAS_TEXTURE)) == (HAS_UVS | HAS_TEXTURE)) {
        let t0 = uv_at(i0);
        let t1 = uv_at(i1);
        let t2 = uv_at(i2);
        let uv = lerp3(b, t0, t1, t2);
        let ddx = lerp3(bary(ray(px + vec2f(1.0, 0.0)), a, e1, e2), t0, t1, t2) - uv;
        let ddy = lerp3(bary(ray(px + vec2f(0.0, 1.0)), a, e1, e2), t0, t1, t2) - uv;
        base *= textureSampleGrad(albedo, samp, uv, ddx, ddy).rgb;
    }

    let cos = max(dot(normal, frame.to_sun.xyz), 0.0);
    let radiance = base * (frame.world.xyz + frame.sun.xyz * cos / 3.14159265);
    textureStore(hdr, xy, vec4f(radiance, 1.0));
}
