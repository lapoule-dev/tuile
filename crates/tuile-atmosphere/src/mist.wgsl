// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The mist: a stated field of layers, marched along a line of sight.
// Prepended is the field's uniform (`mist`, see `mist::wgsl_bindings`).
// Every position here is in render space — metres from the render origin —
// and the noise is read in the field's own east-north-up frame about its
// anchor, so the patches stay put in the world and never see a large
// coordinate.

// ---- a gradient noise, from an integer hash: no texture, the same bytes
// everywhere.

fn mist_hash(p: vec3i, seed: u32) -> u32 {
    // Parenthesised in full: WGSL's browser compilers refuse `*` mixed
    // with `^` without, and naga does not say so.
    var h = (u32(p.x) * 0x9E3779B1u) ^ (u32(p.y) * 0x85EBCA77u) ^ (u32(p.z) * 0xC2B2AE3Du) ^ (seed * 0x27D4EB2Fu);
    h ^= h >> 15u;
    h *= 0x2C1B3C6Du;
    h ^= h >> 12u;
    h *= 0x297A2D39u;
    h ^= h >> 15u;
    return h;
}

// A direction at a lattice point: three signed components from one hash.
fn mist_gradient(p: vec3i, seed: u32) -> vec3f {
    let h = mist_hash(p, seed);
    let g = vec3f(f32(h & 0xFFu), f32((h >> 8u) & 0xFFu), f32((h >> 16u) & 0xFFu)) / 127.5 - 1.0;
    return normalize(g + vec3f(1e-4, 2e-4, 3e-4));
}

fn mist_quintic(t: vec3f) -> vec3f {
    return t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
}

// Gradient noise at `p`, about -0.7..0.7.
fn mist_noise(p: vec3f, seed: u32) -> f32 {
    let i = vec3i(floor(p));
    let f = fract(p);
    let u = mist_quintic(f);
    var n: array<f32, 8>;
    for (var k = 0u; k < 8u; k++) {
        let c = vec3i(i32(k & 1u), i32((k >> 1u) & 1u), i32((k >> 2u) & 1u));
        n[k] = dot(mist_gradient(i + c, seed), f - vec3f(c));
    }
    let x0 = mix(mix(n[0], n[1], u.x), mix(n[2], n[3], u.x), u.y);
    let x1 = mix(mix(n[4], n[5], u.x), mix(n[6], n[7], u.x), u.y);
    return mix(x0, x1, u.z);
}

// Four octaves, each turned against the last so no lattice shows, in
// about -1..1.
fn mist_fbm(p: vec3f, seed: u32) -> f32 {
    // A rotation far from the lattice's symmetries.
    let turn = mat3x3f(
        vec3f(0.80, 0.36, -0.48),
        vec3f(-0.60, 0.48, -0.64),
        vec3f(0.0, 0.80, 0.60),
    );
    var q = p;
    var amp = 0.5;
    var sum = 0.0;
    var norm = 0.0;
    for (var octave = 0u; octave < 4u; octave++) {
        sum += amp * mist_noise(q, seed + octave * 7u);
        norm += amp;
        q = turn * q * 2.03 + vec3f(1.7, 9.2, 3.1);
        amp *= 0.5;
    }
    return sum / (0.7 * norm);
}

// ---- the medium

// A point of render space in the field's frame: east, north, up metres
// from the anchor.
fn mist_local(p: vec3f) -> vec3f {
    let r = p - mist.anchor.xyz;
    return vec3f(dot(r, mist.east.xyz), dot(r, mist.north.xyz), dot(r, mist.up.xyz));
}

// The density of layer `l` at the local point `q`, 0..1: nothing outside
// its band, and inside it the patches a warped noise cuts above the
// coverage, thinned towards the band's floor and ceiling.
fn mist_density(q: vec3f, l: u32) -> f32 {
    let layer = mist.layers[l];
    let base = layer.band.x;
    let top = layer.band.y;
    let h = (q.z - base) / max(top - base, 1.0);
    if (h <= 0.0 || h >= 1.0) {
        return 0.0;
    }
    let profile = smoothstep(0.0, 0.25, h) * (1.0 - smoothstep(0.55, 1.0, h));
    let scale = layer.shape.x;
    // The wind's drift, and a vertical twice as fine as the horizontal:
    // veils are wider than they are thick.
    let w = vec3f(q.x - layer.shape.z, q.y - layer.shape.w, q.z * 2.0) / scale;
    let seed = 11u + l * 101u;
    let warp = layer.shape.y * vec3f(
        mist_noise(w * 0.6 + vec3f(31.0, 0.0, 0.0), seed + 1u),
        mist_noise(w * 0.6 + vec3f(0.0, 31.0, 0.0), seed + 2u),
        mist_noise(w * 0.6 + vec3f(0.0, 0.0, 31.0), seed + 3u) * 0.5,
    );
    let n = mist_fbm(w + warp, seed) * 0.5 + 0.5;
    let cut = 1.0 - layer.band.z;
    let cover = smoothstep(cut - 0.15, cut + 0.2, n);
    return cover * profile;
}

// How a point of layer `l` is lit, 0..1: brighter towards the band's
// ceiling and on the sun's side, dimmer at its floor, with the sun's
// elevation lifting the whole. A veil, not a cumulus: no light march.
fn mist_lit(q: vec3f, l: u32, lean: f32) -> f32 {
    let layer = mist.layers[l];
    let h = clamp((q.z - layer.band.x) / max(layer.band.y - layer.band.x, 1.0), 0.0, 1.0);
    let sun_up = clamp(dot(mist.to_sun.xyz, mist.up.xyz), 0.0, 1.0);
    return (0.55 + 0.45 * h) * (0.75 + 0.25 * sun_up) * (0.9 + 0.1 * lean);
}

// The mist along the line of sight from `origin` towards `direction`
// (unit), up to `farthest` metres: premultiplied colour, and how much of
// what lay beyond is covered (1 − transmittance). `jitter` in 0..1 offsets
// the samples per pixel, against banding.
fn mist_march(origin: vec3f, direction: vec3f, farthest: f32, jitter: f32) -> vec4f {
    let count = u32(mist.anchor.w);
    let steps = u32(mist.to_sun.w);
    let z0 = dot(origin - mist.anchor.xyz, mist.up.xyz);
    let dz = dot(direction, mist.up.xyz);
    // Towards the sun, as seen along this line: the side of a patch the
    // sun lights leans towards the eye when the sun is behind the eye.
    let lean = clamp(-dot(direction, mist.to_sun.xyz) * 0.5 + 0.5, 0.0, 1.0);
    var colour = vec3f(0.0);
    var transmittance = 1.0;
    for (var l = 0u; l < count; l++) {
        let layer = mist.layers[l];
        let base = layer.band.x;
        let top = layer.band.y;
        if (layer.band.z <= 0.0 || layer.band.w <= 0.0 || top <= base) {
            continue;
        }
        // Where the line crosses the band.
        var t_in = 0.0;
        var t_out = farthest;
        if (abs(dz) < 1e-6) {
            if (z0 < base || z0 > top) {
                continue;
            }
        } else {
            let ta = (base - z0) / dz;
            let tb = (top - z0) / dz;
            t_in = max(min(ta, tb), 0.0);
            t_out = min(max(ta, tb), farthest);
        }
        if (t_out <= t_in) {
            continue;
        }
        // As many samples as the crossing is long against the band's
        // thickness, a dozen a thickness: a line straight down through a
        // thin veil needs few, a grazing one many, up to the steps asked.
        let crossing = t_out - t_in;
        let wanted = u32(ceil(crossing / max(top - base, 1.0) * 12.0));
        let taken = clamp(wanted, 6u, steps);
        let dt = crossing / f32(taken);
        var t = t_in + jitter * dt;
        for (var s = 0u; s < taken; s++) {
            let p = origin + direction * t;
            let q = mist_local(p);
            let d = mist_density(q, l);
            if (d > 0.0) {
                let absorbed = 1.0 - exp(-d * layer.band.w * dt);
                colour += transmittance * absorbed * mist_lit(q, l, lean) * layer.colour.xyz;
                transmittance *= 1.0 - absorbed;
                if (transmittance < 0.02) {
                    break;
                }
            }
            t += dt;
        }
        if (transmittance < 0.02) {
            break;
        }
    }
    return vec4f(colour, 1.0 - transmittance);
}

// A per-pixel offset in 0..1 that differs from its neighbours': the
// samples of neighbouring pixels interleave, and banding becomes grain.
fn mist_jitter(pixel: vec2f) -> f32 {
    return fract(52.9829189 * fract(0.06711056 * pixel.x + 0.00583715 * pixel.y));
}
