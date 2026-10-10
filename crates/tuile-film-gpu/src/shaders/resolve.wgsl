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
// Groups in a row: see `bin.wgsl`, which lays the dispatch out.
const RESOLVE_ROW: u32 = 1024u;

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> tiles: array<TileFrame>;
@group(0) @binding(2) var<storage, read> counts: array<u32>;
@group(0) @binding(3) var<storage, read> starts: array<u32>;
@group(0) @binding(4) var<storage, read> list: array<u32>;
@group(0) @binding(5) var vis: texture_2d<u32>;
@group(0) @binding(6) var hdr: texture_storage_2d<rgba16float, write>;
@group(0) @binding(7) var samp: sampler;
@group(0) @binding(8) var shadow_map: texture_depth_2d;
@group(0) @binding(9) var shadow_samp: sampler_comparison;

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

// How much of the sun reaches `p` (eye-relative, its normal `n`): 1 where
// nothing in the sun's map is nearer the sun, over 3×3 texels each read
// through the comparison's own bilinear filter.
//
// The point is moved off its surface by two texels before it is looked up,
// so that ground does not shade itself across the texel it falls in. And
// what the map does not hold is lit: beyond it nothing is known to stand
// before the sun, and unknown is never dark.
fn sunlit(p: vec3f, n: vec3f) -> f32 {
    let at = frame.sun_view_proj * vec4f(p + n * (2.0 * frame.shadow.y), 1.0);
    let uv = vec2f(at.x, -at.y) * 0.5 + 0.5;
    if (any(uv < vec2f(0.0)) || any(uv > vec2f(1.0)) || at.z < 0.0 || at.z > 1.0) {
        return 1.0;
    }
    let texel = 1.0 / frame.shadow.z;
    var lit = 0.0;
    for (var j = -1; j <= 1; j++) {
        for (var i = -1; i <= 1; i++) {
            let to = uv + vec2f(f32(i), f32(j)) * texel;
            lit += textureSampleCompareLevel(shadow_map, shadow_samp, to, at.z);
        }
    }
    // Towards where they stop, shadows fade: no line across the ground
    // where the map ends.
    let fade = smoothstep(0.8 * frame.shadow.w, frame.shadow.w, length(p));
    return lit / 9.0 * (1.0 - fade) + fade;
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
    let n = id.y * (RESOLVE_ROW * RESOLVE_GROUP) + id.x;
    if (n >= counts[slot]) { return; }
    let pixel = list[starts[slot] + n];
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
    var radiance = base * (frame.world.xyz + frame.sun.xyz * cos);
    if (frame.shadow.x > 0.0 || frame.haze.w != 0.0) {
        // Where the ray meets the triangle, from the eye — held inside the
        // triangle. The raster says the pixel is on it; of a triangle seen
        // nearly edge-on, a skirt at a seam, the ray's own meeting with
        // its plane may be kilometres away, and the air and the sun would
        // be read there.
        let inside = clamp(b, vec2f(0.0), vec2f(1.0));
        let held = inside / max(inside.x + inside.y, 1.0);
        let p = a + held.x * e1 + held.y * e2;
        // A point that is no place — not a number, or further than any
        // ground an eye over the Earth can see — is left as the look
        // without sun's map or air shades it. Where two levels of tiles
        // part by a hair, what shows through the gap is the far side of
        // the planet, thousands of kilometres off: the air would whiten
        // that hair, and it is the gap that is wrong, not the air. Not a
        // number is told by its bits: a compiler free to assume no such
        // number exists answers any comparison with it as it likes.
        let known = all((bitcast<vec3u>(p) & vec3u(0x7f800000u)) != vec3u(0x7f800000u))
            && all(abs(p) < vec3f(2.0e6));
        if (known && frame.shadow.x > 0.0 && cos > 0.0) {
            // A shadow takes the sun, and only the sun: the dome lights
            // shadowed ground as it lights any other.
            let sun = cos * (1.0 - frame.shadow.x * (1.0 - sunlit(p, normal)));
            radiance = base * (frame.world.xyz + frame.sun.xyz * sun);
        }
        if (known && frame.haze.w != 0.0) {
            // What the air leaves of the ground, and its own light for the
            // rest. Written out: air that takes nothing leaves the ground
            // exactly as it was.
            let left = air_left(p);
            radiance = radiance * left + frame.horizon.xyz * (1.0 - left);
        }
    }
    // PROBE (exploration): what a gap shows, painted so it can be counted.
    if (frame.local_up.w != 0.0) {
        let inside = clamp(b, vec2f(0.0), vec2f(1.0));
        let held = inside / max(inside.x + inside.y, 1.0);
        let p = a + held.x * e1 + held.y * e2;
        if (!all(abs(p) < vec3f(2.0e6))) {
            radiance = vec3f(1000.0, 0.0, 1000.0);
        }
    }
    textureStore(hdr, xy, vec4f(radiance, 1.0));
}
