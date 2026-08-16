// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The air, as a shader sees it.
//
// This file is the GPU expression of `aerial.rs`, and it lives beside it so a
// change to one happens under the eyes of the other. It used to live in the
// wgpu backend, where its own comment said "that crate owns the physics, this
// only spends it" — which was true and was an argument for moving it here.
//
// It depends on nothing but its arguments: no bindings, no globals, no entry
// point. A backend supplies `Air` however it likes and calls
// `aerial_perspective`.
//
// One number deliberately disagrees with `aerial.rs`. `SMALL_RISE` is `1e-3`
// here and `1.0e-4` there, and that is not drift: the threshold is where
// `(1 - e^-x) / x` stops being computable and the series takes over, and f32
// loses that fight an order of magnitude earlier than f64 does. Same formula,
// different precision, different cutover.

/// ground *loses* its own colour — extinction — and it *gains* the colour of the
/// air in front of it. Only the second makes a ridge read as far away; extinction
/// alone would just make it dark.
const PI: f32 = 3.14159265358979;

/// The air between the eye and a fragment, as the CPU measured it.
///
/// Mirrors `tuile_atmosphere::AerialPerspective` field for field — that crate
/// owns the physics and the derivation, this only spends them. Passed in rather
/// than read from a global so the block below depends on nothing but its
/// arguments, which is what lets a second backend use the same source.
struct Air {
    /// Eye in render space (xyz), its height above ground (w).
    eye: vec4f,
    /// Planet centre in render space (xyz), its radius (w).
    earth: vec4f,
    /// Scattering per metre RGB (xyz), scale height (w).
    rayleigh: vec4f,
    /// Scattering (x), scale height (y), anisotropy (z), strength (w) — 0 turns
    /// the atmosphere off.
    mie: vec4f,
    /// Direction sunlight travels (xyz), haze brightness (w).
    sun: vec4f,
}

/// Normalisation of the Rayleigh phase function, `3 / 16π`. Written as the
/// expression rather than its decimal so it can be checked against the physics
/// instead of against a previous copy of itself.
const RAYLEIGH_PHASE_NORM: f32 = 3.0 / (16.0 * PI);

/// Normalisation of the Henyey-Greenstein phase function, `1 / 4π`.
const MIE_PHASE_NORM: f32 = 1.0 / (4.0 * PI);

/// A phase function integrates to 1 over the sphere, so it answers "per
/// steradian". The in-scattered term wants the whole sky's worth, which is the
/// sphere's solid angle.
const SPHERE_SOLID_ANGLE: f32 = 4.0 * PI;

/// Floor on total extinction before dividing by it. Not a physical quantity:
/// extinction is strictly positive wherever there is air, and this only keeps a
/// fragment far enough out that the density underflows from producing a NaN.
const MIN_EXTINCTION: f32 = 1e-12;

/// Floor on the Henyey-Greenstein denominator, which vanishes as `g` approaches
/// 1 looking straight at the light. Real haze never reaches `g = 1`; clamping
/// costs an instruction and a NaN costs the frame.
const MIN_PHASE_DENOM: f32 = 1e-4;

/// Below this, `(1 - e^-x) / x` is taken from its series instead of its closed
/// form — the closed form is 0/0 at zero and, just before that, a subtraction of
/// two numbers that agree to every digit f32 has.
const SMALL_RISE: f32 = 1e-3;

/// `(1 - e^-x) / x`, the mean of `e^-t` over `t` in `[0, x]`. One at `x = 0`.
fn falling_mean(x: f32) -> f32 {
    if (x < SMALL_RISE) {
        return 1.0 - x * 0.5 + x * x / 6.0;
    }
    return (1.0 - exp(-x)) / x;
}

/// How much air a ray actually crosses, as a sea-level-equivalent length.
///
/// Mirrors `tuile_atmosphere::aerial::air_column`, where the derivation and the
/// tests live. Written as `exp(-h_low/H) * (1 - e^-x) / x * distance` rather
/// than the algebraically equal `H * (e^-lo - e^-hi) * distance / rise`, because
/// only the first can be computed in f32: the second subtracts two exponentials
/// that both approach 1 as the path levels out, then divides by the vanishing
/// rise. That ran the optical depth away, drove transmittance to zero, and
/// rendered terrain black — appearing as the eye descended toward the ground it
/// was looking at, while the coarser tile above stayed fine at its own height.
fn air_column(height_a: f32, height_b: f32, distance: f32, scale_height: f32) -> f32 {
    let low = max(min(height_a, height_b), 0.0);
    let high = max(max(height_a, height_b), 0.0);
    let x = (high - low) / scale_height;
    return exp(-low / scale_height) * falling_mean(x) * distance;
}

fn aerial_perspective(lit: vec3f, world: vec3f, air: Air) -> vec3f {
    let strength = air.mie.w;
    // Uniform across the draw: it comes from a uniform buffer, and nothing
    // inside samples a texture, so branching here is free and legal.
    if (strength <= 0.0) {
        return lit;
    }

    let to_eye = air.eye.xyz - world;
    let distance = length(to_eye);
    let up = normalize(world - air.earth.xyz);
    let ground_height = max(length(world - air.earth.xyz) - air.earth.w, 0.0);
    let eye_height = air.eye.w;

    // How much air is actually on this ray, integrated rather than averaged.
    let rayleigh_scale = air.rayleigh.w;
    let mie_scale = air.mie.y;
    let rayleigh_column = air_column(eye_height, ground_height, distance, rayleigh_scale);
    let mie_column = air_column(eye_height, ground_height, distance, mie_scale);

    let rayleigh_depth = air.rayleigh.xyz * rayleigh_column;
    let mie_depth = vec3f(air.mie.x * mie_column);
    let transmittance = exp(-(rayleigh_depth + mie_depth) * strength);

    // Phase functions: how much of the sunlight crossing the ray is turned
    // toward the eye. Rayleigh is nearly symmetric; Mie throws light forward,
    // which is why haze glares when you look toward the sun and not away.
    let view_dir = -normalize(to_eye);
    let cos_angle = dot(view_dir, -air.sun.xyz);
    let cos_sq = cos_angle * cos_angle;
    let rayleigh_phase = RAYLEIGH_PHASE_NORM * (1.0 + cos_sq);
    let g = air.mie.z;
    let g_sq = g * g;
    let mie_phase = MIE_PHASE_NORM * (1.0 - g_sq)
        / pow(max(1.0 + g_sq - 2.0 * g * cos_angle, MIN_PHASE_DENOM), 1.5);

    // The source function: scattering toward the eye over total extinction. Its
    // *colour* comes out near white — but the amount that reaches the eye goes
    // as (1 - transmittance), which is far larger for blue. That is why distance
    // is blue near to and washes out to grey far away, and why this is not the
    // same thing as a fog colour someone picked.
    let scattering = rayleigh_depth * rayleigh_phase + mie_depth * mie_phase;
    let source = scattering / max(rayleigh_depth + mie_depth, vec3f(MIN_EXTINCTION));

    // How lit the air over this point is. Below the horizon there is no
    // sunlight to scatter, and haze on the night side has to go dark or the
    // terminator glows.
    let sun_up = clamp(dot(-air.sun.xyz, up), 0.0, 1.0);
    let in_scatter = source * (vec3f(1.0) - transmittance)
        * (SPHERE_SOLID_ANGLE * sun_up * air.sun.w);

    return lit * transmittance + in_scatter;
}
