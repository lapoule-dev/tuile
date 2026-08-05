// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Minimal PBR-ish tile shader: base color texture × factor, lambert + ambient.

struct ViewUniform {
    view_proj: mat4x4f,
    // xyz = direction the light travels (normalized), w unused.
    sun_dir: vec4f,
    // x = ambient amount, yzw unused.
    params: vec4f,
    // Aerial perspective. Mirrors tuile_atmosphere::AerialPerspective field for
    // field; that crate owns the physics, this only spends it.
    //   air_eye:      eye in render space (xyz), its height above ground (w)
    //   air_earth:    planet centre in render space (xyz), its radius (w)
    //   air_rayleigh: scattering per metre RGB (xyz), scale height (w)
    //   air_mie:      scattering (x), scale height (y), anisotropy (z),
    //                 strength (w) — 0 turns the atmosphere off
    //   air_sun:      direction sunlight travels (xyz), haze brightness (w)
    air_eye: vec4f,
    air_earth: vec4f,
    air_rayleigh: vec4f,
    air_mie: vec4f,
    air_sun: vec4f,
}
@group(0) @binding(0) var<uniform> view: ViewUniform;

struct TileUniform {
    model: mat4x4f,
}
@group(1) @binding(0) var<uniform> tile: TileUniform;

struct MaterialUniform {
    base_color: vec4f,
}
@group(2) @binding(0) var<uniform> material: MaterialUniform;
@group(2) @binding(1) var base_tex: texture_2d<f32>;
@group(2) @binding(2) var base_samp: sampler;

// Imagery draped over the tile, blended on top of the base colour.
//
// A layer is two vec4, so that the whole set is one uniform array with the
// natural 16-byte stride:
//   [2i]     coverage:  u_min, v_min, u_max, v_max — in the TILE's uv space
//   [2i + 1] placement: translation.xy, scale.xy
// and `texture_uv = tile_uv * scale + translation`.
//
// The slot count must equal `tuile_core::raster::MAX_IMAGERY_LAYERS`; a test in
// this crate reads it back out of this file and asserts they agree.
const IMAGERY_LAYERS: u32 = 12u;
struct ImageryUniform {
    layers: array<vec4f, 24>,
}
@group(2) @binding(3) var<uniform> imagery: ImageryUniform;
@group(2) @binding(4) var img0: texture_2d<f32>;
@group(2) @binding(5) var img1: texture_2d<f32>;
@group(2) @binding(6) var img2: texture_2d<f32>;
@group(2) @binding(7) var img3: texture_2d<f32>;
@group(2) @binding(8) var img4: texture_2d<f32>;
@group(2) @binding(9) var img5: texture_2d<f32>;
@group(2) @binding(10) var img6: texture_2d<f32>;
@group(2) @binding(11) var img7: texture_2d<f32>;
@group(2) @binding(12) var img8: texture_2d<f32>;
@group(2) @binding(13) var img9: texture_2d<f32>;
@group(2) @binding(14) var img10: texture_2d<f32>;
@group(2) @binding(15) var img11: texture_2d<f32>;

struct VsIn {
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) normal: vec3f,
    @location(1) uv: vec2f,
    // Render space, not ECEF. Interpolating an ECEF position across a triangle
    // in f32 loses metres; render space keeps the magnitudes small enough that
    // it does not, which is the same reason the positions were rebased.
    @location(2) world: vec3f,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let world = tile.model * vec4f(in.position, 1.0);
    out.clip = view.view_proj * world;
    out.normal = (tile.model * vec4f(in.normal, 0.0)).xyz;
    out.uv = in.uv;
    out.world = world.xyz;
    return out;
}

/// Blends one imagery layer over what is already there.
///
/// Branch-free on purpose, twice over. The coverage test is `step` rather than
/// an `if` because a discarded layer must still not diverge the control flow
/// `textureSample` sits in; and an *unused* slot needs no test of its own,
/// because it carries an empty coverage rectangle and so masks itself out. That
/// is why the sampler reads a 1×1 white texture in unused slots rather than
/// nothing: it costs a guaranteed cache hit and keeps this straight-line.
fn blend_layer(dst: vec3f, tex: texture_2d<f32>, uv: vec2f, slot: u32) -> vec3f {
    let coverage = imagery.layers[2u * slot];
    let placement = imagery.layers[2u * slot + 1u];
    let above = step(coverage.xy, uv);
    let below = step(uv, coverage.zw);
    let mask = above.x * above.y * below.x * below.y;
    // Outside the coverage rectangle this reads past the layer's own edge and
    // the clamping sampler smears it — which never shows, because that is
    // exactly where the mask is zero.
    let texel = textureSample(tex, base_samp, uv * placement.zw + placement.xy);
    return mix(dst, texel.rgb, mask * texel.a);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    var albedo = textureSample(base_tex, base_samp, in.uv) * material.base_color;
    // Content that owns its texturing has no layers, and terrain has no base
    // colour texture — the two never both contribute, but nothing here needs to
    // know which case it is in.
    var ground = albedo.rgb;
    ground = blend_layer(ground, img0, in.uv, 0u);
    ground = blend_layer(ground, img1, in.uv, 1u);
    ground = blend_layer(ground, img2, in.uv, 2u);
    ground = blend_layer(ground, img3, in.uv, 3u);
    ground = blend_layer(ground, img4, in.uv, 4u);
    ground = blend_layer(ground, img5, in.uv, 5u);
    ground = blend_layer(ground, img6, in.uv, 6u);
    ground = blend_layer(ground, img7, in.uv, 7u);
    ground = blend_layer(ground, img8, in.uv, 8u);
    ground = blend_layer(ground, img9, in.uv, 9u);
    ground = blend_layer(ground, img10, in.uv, 10u);
    ground = blend_layer(ground, img11, in.uv, 11u);
    albedo = vec4f(ground, albedo.a);

    let n = normalize(in.normal);
    let lambert = max(dot(n, -view.sun_dir.xyz), 0.0);
    let ambient = view.params.x;
    let lit = albedo.rgb * (ambient + (1.0 - ambient) * lambert);
    return vec4f(aerial_perspective(lit, in.world), albedo.a);
}

/// What the air between the eye and this fragment does to its colour.
///
/// Single scattering through an exponential atmosphere, integrated in closed
/// form. The reference implementation ray-marches this; over the ground the two
/// agree closely and a march costs sixty-four samples a fragment.
///
/// Two things come out of it, and the second is the one people notice. Distant
/// ground *loses* its own colour — extinction — and it *gains* the colour of the
/// air in front of it. Only the second makes a ridge read as far away; extinction
/// alone would just make it dark.
const PI: f32 = 3.14159265358979;

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

/// Below this fraction of a scale height of climb, a path counts as level — the
/// substitution in `air_column` would otherwise divide by its own rise.
const LEVEL_PATH_RISE: f32 = 1e-6;

/// How much air a ray actually crosses, as a sea-level-equivalent length:
/// `∫ exp(-h/H) ds` between two endpoint heights over a distance.
///
/// Mirrors `tuile_atmosphere::aerial::air_column`, where the derivation and the
/// tests live. Exact when height varies linearly along the path — substituting
/// `ds = dh · distance / Δh` closes the integral.
///
/// Averaging the two endpoint densities instead is the obvious thing and it is
/// wrong: it only agrees when the ends sit at similar heights. From orbit it
/// charges half a ray's length at ground density and renders the planet as a
/// featureless blue disc.
fn air_column(height_a: f32, height_b: f32, distance: f32, scale_height: f32) -> f32 {
    let low = max(min(height_a, height_b), 0.0);
    let high = max(max(height_a, height_b), 0.0);
    let rise = high - low;
    if (rise < scale_height * LEVEL_PATH_RISE) {
        return exp(-low / scale_height) * distance;
    }
    return scale_height * (exp(-low / scale_height) - exp(-high / scale_height))
        * (distance / rise);
}

fn aerial_perspective(lit: vec3f, world: vec3f) -> vec3f {
    let strength = view.air_mie.w;
    // Uniform across the draw: it comes from a uniform buffer, and nothing
    // inside samples a texture, so branching here is free and legal.
    if (strength <= 0.0) {
        return lit;
    }

    let to_eye = view.air_eye.xyz - world;
    let distance = length(to_eye);
    let up = normalize(world - view.air_earth.xyz);
    let ground_height = max(length(world - view.air_earth.xyz) - view.air_earth.w, 0.0);
    let eye_height = view.air_eye.w;

    // How much air is actually on this ray, integrated rather than averaged.
    let rayleigh_scale = view.air_rayleigh.w;
    let mie_scale = view.air_mie.y;
    let rayleigh_column = air_column(eye_height, ground_height, distance, rayleigh_scale);
    let mie_column = air_column(eye_height, ground_height, distance, mie_scale);

    let rayleigh_depth = view.air_rayleigh.xyz * rayleigh_column;
    let mie_depth = vec3f(view.air_mie.x * mie_column);
    let transmittance = exp(-(rayleigh_depth + mie_depth) * strength);

    // Phase functions: how much of the sunlight crossing the ray is turned
    // toward the eye. Rayleigh is nearly symmetric; Mie throws light forward,
    // which is why haze glares when you look toward the sun and not away.
    let view_dir = -normalize(to_eye);
    let cos_angle = dot(view_dir, -view.air_sun.xyz);
    let cos_sq = cos_angle * cos_angle;
    let rayleigh_phase = RAYLEIGH_PHASE_NORM * (1.0 + cos_sq);
    let g = view.air_mie.z;
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
    let sun_up = clamp(dot(-view.air_sun.xyz, up), 0.0, 1.0);
    let in_scatter = source * (vec3f(1.0) - transmittance)
        * (SPHERE_SOLID_ANGLE * sun_up * view.air_sun.w);

    return lit * transmittance + in_scatter;
}
