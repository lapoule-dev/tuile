// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The air of a look that has some: what it takes from a ray on its way to
// the eye, and the sky it makes. For the shaders that bind `frame`.

// The share of light left along the straight path from the eye to `p`,
// eye-relative. `p`'s height over the eye is what it is along `up`, less
// what the ground has curved away under it at that distance.
fn air_left(p: vec3f) -> f32 {
    let rise = dot(p, frame.local_up.xyz);
    let climb = rise + (dot(p, p) - rise * rise) * frame.haze.z;
    return exp(-frame.haze.x * length(p) * mean_thinning(climb * frame.haze.y));
}

// The sky along `d`: the horizon's colour seen through the whole depth of
// the air, the zenith's through the little there is straight up.
fn sky(d: vec3f) -> vec3f {
    let sine = dot(normalize(d), frame.local_up.xyz);
    // The air overhead, as deep as its scale height at the eye's density,
    // and as many times that as the ray is slanted. At and under the
    // horizon there is no end to it.
    let depth = frame.haze.x / frame.haze.y / max(sine, 1e-4);
    let through = exp(-SKY_DEPTH * depth);
    return frame.zenith.xyz * through + frame.horizon.xyz * (1.0 - through);
}

// The sky's own air is deeper than the haze near the ground the look gives
// the density of: its gradient reaches this many times higher.
const SKY_DEPTH: f32 = 40.0;
