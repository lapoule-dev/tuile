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
@group(0) @binding(8) var shadow_map: texture_depth_2d_array;
@group(0) @binding(9) var shadow_samp: sampler_comparison;
// How much of the sun projected shadows hide, summed over their
// triangles, so possibly more than all of it: see `projected.wgsl`.
@group(0) @binding(10) var projected: texture_2d<f32>;

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

// How many times the light of the ground under it the air's veil may
// reach — `atmosphere.tuning.x`, 6 by default: see `resolve`.

// The ground the triangle means where the pixel meets it, for the air: `p`,
// moved along the vertical by how far the flat triangle lies under the
// ground its vertices' heights describe.
//
// A terrain tile's vertices are on the ground, and its heights run linearly
// between them; the flat triangle between three vertices is a chord. Inside
// a facet of 5° — the first levels of tiles, which a zoom-out draws — it
// lies up to 12 km under that ground. Read there, the air under each
// facet's middle ended in kilometres of sea-level air, and a sea seen from
// orbit was a blotch of haze a facet, a darker lattice through its
// vertices. Where triangles are small the two are all but one point: a
// facet a kilometre across lies at most four centimetres under.
//
// Heights over a sphere about `centre`. What a point rises above the first
// vertex, `a`, is a difference of squares over a sum, so that no f32
// subtracts two radii: the sag is good to a centimetre for a facet of
// hundreds of kilometres, and is nothing at all at the vertices, where its
// terms are the same arithmetic. `held` is where the pixel is on the
// triangle: `p = a + held.x * e1 + held.y * e2`.
fn meant_ground(p: vec3f, a: vec3f, e1: vec3f, e2: vec3f, held: vec2f, centre: vec3f) -> vec3f {
    let q = a - centre;
    let w = held.x * e1 + held.y * e2;
    let r = length(q);
    let rise1 = (2.0 * dot(q, e1) + dot(e1, e1)) / (length(q + e1) + r);
    let rise2 = (2.0 * dot(q, e2) + dot(e2, e2)) / (length(q + e2) + r);
    let rise = (2.0 * dot(q, w) + dot(w, w)) / (length(q + w) + r);
    let sag = held.x * rise1 + held.y * rise2 - rise;
    return p + normalize(q + w) * sag;
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
        // PROTOTYPE (terrain smoothing): how much of the vertices' normals a
        // face takes, against its own — `zenith.w`, 1 as before.
        var flat = normal;
        if (dot(flat, n) < 0.0) { flat = -flat; }
        normal = normalize(mix(flat, normalize(n), frame.zenith.w));
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

    // PROTOTYPE (terrain smoothing): the mesh, drawn — a pale ground with
    // each triangle's edges a pixel wide — when `horizon.w` asks.
    if (frame.horizon.w != 0.0) {
        let bx = bary(ray(px + vec2f(1.0, 0.0)), a, e1, e2) - b;
        let by = bary(ray(px + vec2f(0.0, 1.0)), a, e1, e2) - b;
        let third = 1.0 - b.x - b.y;
        let near = min(
            min(b.x / length(vec2f(bx.x, by.x)), b.y / length(vec2f(bx.y, by.y))),
            third / length(vec2f(bx.x + bx.y, by.x + by.y)),
        );
        base = mix(vec3f(0.02, 0.02, 0.05), vec3f(0.55), smoothstep(0.6, 1.6, near));
    }
    let cos = max(dot(normal, frame.to_sun.xyz), 0.0);
    // The sky's light on this facing: the look's dome, and under a physical
    // atmosphere the sky's gradient over it (an L1 harmonic) — a slope
    // facing the sun's side of the sky receives more than one facing away.
    var ambient = frame.world.xyz;
    let physical = atmosphere.shape.w != 0.0;
    if (physical) {
        ambient += atmosphere.sh_x.xyz * normal.x + atmosphere.sh_y.xyz * normal.y
            + atmosphere.sh_z.xyz * normal.z;
        ambient = max(ambient, vec3f(0.0));
    }
    var radiance = base * (ambient + frame.sun.xyz * cos);
    if (frame.shadow.x > 0.0 || frame.haze.w != 0.0 || physical) {
        // Where the ray meets the triangle, from the eye — held inside the
        // triangle. The raster says the pixel is on it; of a triangle seen
        // nearly edge-on, a skirt at a seam, the ray's own meeting with
        // its plane may be kilometres away, and the air and the sun would
        // be read there.
        let inside = clamp(b, vec2f(0.0), vec2f(1.0));
        let held = inside / max(inside.x + inside.y, 1.0);
        let p = a + held.x * e1 + held.y * e2;
        // A point that is no place — not a number, or further than any
        // ground an eye over the Earth can see (from 300 km, the limb is
        // 2 000 km off) — is left as the look without sun's map or air
        // shades it. Where two levels of tiles part by a hair, what shows
        // through the gap is the far side of the planet, thousands of
        // kilometres off: the air would whiten that hair, and it is the
        // gap that is wrong, not the air. Not a number is told by its
        // bits: a compiler free to assume no such number exists answers
        // any comparison with it as it likes.
        // Under a physical atmosphere the eye may be anywhere up to the
        // Moon: the ground it can see is within its horizon over a sphere
        // well under any ground (6 350 km), by half again — what the probe
        // of holes reads below — a reach that grows with the eye, so that
        // no height draws the far ground of its limb without its air.
        // Without one, as it always was.
        let number = all((bitcast<vec3u>(p) & vec3u(0x7f800000u)) != vec3u(0x7f800000u));
        var known = number && all(abs(p) < vec3f(2.0e6));
        if (physical) {
            let r = length(atmosphere.centre.xyz);
            known = number && dot(p, p) < 1.5 * (r - 6.35e6) * (r + 6.35e6);
        }
        // The ground the triangle means, where the air reads it: its own
        // light, the air between, the veil's bound. Shadows, city lights
        // and the probe of holes keep the drawn point — the sun's map was
        // drawn from the same triangles.
        var ground = p;
        if (physical) {
            ground = meant_ground(p, a, e1, e2, held, atmosphere.centre.xyz);
        } else if (frame.haze.w != 0.0) {
            // The haze's own sphere: through the eye, about the centre
            // `air_left` curves the ground away from.
            ground = meant_ground(p, a, e1, e2, held, -frame.local_up.xyz * (0.5 / frame.haze.z));
        }
        var lit = 1.0;
        if (known && frame.shadow.x > 0.0 && cos > 0.0) {
            // A shadow takes the sun, and only the sun: the dome lights
            // shadowed ground as it lights any other.
            // And the shadows shapes project, worked out for this very
            // sample: the darker of the two.
            let open = 1.0 - min(textureLoad(projected, xy, 0).r, 1.0);
            lit = 1.0 - frame.shadow.x * (1.0 - min(sunlit(p, normal, xy), open));
            radiance = base * (ambient + frame.sun.xyz * cos * lit);
        }
        if (known && physical) {
            // The ground's own light and the air between the eye and it
            // (`ground_seen`, the viewer's too). Under a physical
            // atmosphere there is no closed-form haze: the block below is
            // not reached.
            radiance = ground_seen(base, p, ground, normal, ambient, lit);
        }
        if (known && frame.haze.w != 0.0) {
            // What the air leaves of the ground, and its own light for the
            // rest. Written out: air that takes nothing leaves the ground
            // exactly as it was.
            let left = air_left(ground);
            radiance = radiance * left + frame.horizon.xyz * (1.0 - left);
        }
    }
    // The probe of holes: a pixel whose ground is not a place, or is
    // further than the horizon of a sphere well under any ground (6 350 km)
    // by half again, is a ray that went through a gap — the far side of
    // the planet seen through it. It is marked, in the alpha nothing else
    // reads, for `present` to paint.
    var mark = 1.0;
    if (frame.local_up.w != 0.0) {
        let inside = clamp(b, vec2f(0.0), vec2f(1.0));
        let held = inside / max(inside.x + inside.y, 1.0);
        let p = a + held.x * e1 + held.y * e2;
        let number = all((bitcast<vec3u>(p) & vec3u(0x7f800000u)) != vec3u(0x7f800000u));
        // The eye is `r` from the Earth's centre.
        let r = 0.5 / frame.haze.z;
        let far = dot(p, p) > 1.5 * (r - 6.35e6) * (r + 6.35e6);
        if (!number || far) { mark = 2.0; }
    }
    textureStore(hdr, xy, vec4f(radiance, mark));
}
