// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The physical atmosphere, as a shader sees it: the GPU twin of
// `hillaire.rs`, where the derivations and the tests live. Hillaire 2020's
// tables, read and marched; the same parametrisations to the letter, in f32,
// heights carried apart from the planet's radius so that no f32 has to
// subtract two radii.
//
// A module that includes this declares:
//
//   var<uniform> atmosphere: Atmosphere;
//   var transmittance_lut: texture_2d<f32>;
//   var multi_scattering_lut: texture_2d<f32>;
//   var irradiance_lut: texture_2d<f32>;
//   var lut_sampler: sampler;   // unused by the tables, kept for the host
//
// The tables are 32-bit floats, read by `lut_bilinear`: a night's light is
// ten orders of magnitude under a day's, and half floats flush it to steps
// a picture shows as blocks.
//
// Units: a radiance is per unit of the sun's irradiance outside the air;
// `atmosphere.illuminance` says what one is in the renderer's units.

struct Atmosphere {
    // xyz: the centre of the sphere the air is laid on, from the eye;
    // w: its radius, in metres.
    centre: vec4f,
    // xyz: Rayleigh scattering at sea level, per metre; w: one over its
    // scale height.
    rayleigh: vec4f,
    // x: aerosol scattering at sea level; y: its extinction; z: one over
    // its scale height; w: its anisotropy.
    mie: vec4f,
    // xyz: ozone's absorption at its densest; w: the height it is densest.
    ozone: vec4f,
    // x: ozone's half width; y: the air's thickness; z: the ground's
    // albedo; w: 1 when the atmosphere is drawn at all.
    shape: vec4f,
    // xyz: towards the sun; w: the height of the frame's eye.
    sun: vec4f,
    // xyz: up at the frame's eye; w: nothing.
    up: vec4f,
    // xyz: what one unit of the model's radiance is in the renderer's.
    illuminance: vec4f,
    // xyz: the radiance the night floor adds to the sky.
    floor: vec4f,
    // The sky's irradiance on a surface of normal n is the frame's `world`
    // plus `sh_x * n.x + sh_y * n.y + sh_z * n.z` (xyz of each).
    sh_x: vec4f,
    sh_y: vec4f,
    sh_z: vec4f,
    // xyz: towards the Moon; w: its irradiance outside the air over the
    // sun's — 0 when there is no Moon.
    moon: vec4f,
    // xyz: from the Moon towards the sun, as its phase has it; w: the
    // disc's angular radius.
    moon_disc: vec4f,
    // x: what the disc's shading is divided by (its mean over the disc);
    // y: earthshine, against the full disc's light; zw: nothing.
    moon_shade: vec4f,
    // xyz: one unit of the model's irradiance on the ground, in the
    // renderer's units.
    ground: vec4f,
    // xyz: the night floor's light on every surface.
    added: vec4f,
    // xyz: the least light any ground is lit by.
    least: vec4f,
    // xyz: the least radiance any sky — space included — is drawn with.
    least_sky: vec4f,
    // xyz: level ground's sky light where the frame's light was worked
    // out, from the tables: what a fragment's own is set against.
    level_ref: vec4f,
    // Rows of the turn from the Earth's axes to the stars'. w of the
    // first: 1 to draw stars.
    stars_x: vec4f,
    stars_y: vec4f,
    stars_z: vec4f,
}

const ATMOSPHERE_PI: f32 = 3.14159265358979;
// The planet radius the tables are laid out for: `hillaire::LUT_RADIUS`.
const LUT_RADIUS: f32 = 6360000.0;
const TRANSMITTANCE_SIZE: vec2f = vec2f(256.0, 64.0);
const MULTI_SCATTERING_SIZE: vec2f = vec2f(32.0, 32.0);
const IRRADIANCE_SIZE: vec2f = vec2f(256.0, 16.0);
const SKY_VIEW_SIZE: vec2f = vec2f(192.0, 108.0);
const SUN_ANGULAR_RADIUS: f32 = 0.004652;
// Steps of the air between the eye and the ground: `docs/19` keeps the
// march short; the tables carry the light, the steps only the density.
const AERIAL_STEPS: u32 = 12u;
// From high up, the air between eye and ground is a long path of which the
// dense part is the last: twice the steps, bunched towards the ground.
const AERIAL_STEPS_HIGH: u32 = 24u;
const AERIAL_HIGH: f32 = 10000.0;
const SKY_STEPS: u32 = 30u;

struct Medium {
    rayleigh: vec3f,
    mie: f32,
    extinction: vec3f,
}

fn medium(height: f32) -> Medium {
    let h = max(height, 0.0);
    let rayleigh = atmosphere.rayleigh.xyz * exp(-h * atmosphere.rayleigh.w);
    let mie_density = exp(-h * atmosphere.mie.z);
    let ozone = max(1.0 - abs(h - atmosphere.ozone.w) / atmosphere.shape.x, 0.0);
    var m: Medium;
    m.rayleigh = rayleigh;
    m.mie = atmosphere.mie.x * mie_density;
    m.extinction = rayleigh + vec3f(atmosphere.mie.y * mie_density)
        + atmosphere.ozone.xyz * ozone;
    return m;
}

// A table read between its texels, by hand: 32-bit float textures are not
// filterable everywhere. Clamped to the edge texels, as a clamped sampler.
fn lut_bilinear(t: texture_2d<f32>, uv: vec2f) -> vec4f {
    let size = vec2f(textureDimensions(t));
    let x = clamp(uv * size - 0.5, vec2f(0.0), size - 1.0);
    let i = vec2i(floor(x));
    let f = x - floor(x);
    let top = vec2i(size) - 1;
    let j = min(i + 1, top);
    let a = textureLoad(t, i, 0);
    let b = textureLoad(t, vec2i(j.x, i.y), 0);
    let c = textureLoad(t, vec2i(i.x, j.y), 0);
    let d = textureLoad(t, j, 0);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

fn uv_from_unit(x: vec2f, size: vec2f) -> vec2f {
    return 0.5 / size + x * (1.0 - 1.0 / size);
}

// Bruneton's parametrisation, as `LutAtmosphere::transmittance_unit`, from
// a height rather than a radius: every difference of two radii is written
// as the product it factors into.
fn transmittance_uv(height: f32, mu: f32) -> vec2f {
    let h = max(height, 0.0);
    let thickness = atmosphere.shape.y;
    let r = LUT_RADIUS + h;
    let top = LUT_RADIUS + thickness;
    let big_h = sqrt(thickness * (2.0 * LUT_RADIUS + thickness));
    let rho = sqrt(h * (2.0 * LUT_RADIUS + h));
    let disc = (top - r) * (top + r) + r * r * mu * mu;
    let d = max(-r * mu + sqrt(max(disc, 0.0)), 0.0);
    let d_min = top - r;
    let d_max = rho + big_h;
    let x_mu = clamp((d - d_min) / (d_max - d_min), 0.0, 1.0);
    return uv_from_unit(vec2f(x_mu, rho / big_h), TRANSMITTANCE_SIZE);
}

// Where a ray from `height` at zenith cosine `mu` enters the air from
// above its top: the distance to it, 0 for a ray that starts in the air,
// negative for one that never meets it.
fn air_entry(height: f32, mu: f32) -> f32 {
    let thickness = atmosphere.shape.y;
    if (height <= thickness) { return 0.0; }
    let r = LUT_RADIUS + height;
    let top = LUT_RADIUS + thickness;
    let disc = (top - r) * (top + r) + r * r * mu * mu;
    if (mu >= 0.0 || disc < 0.0) { return -1.0; }
    return -r * mu - sqrt(disc);
}

// The share of light left along a ray to the top of the air; from above
// the air, along the line to where it enters it — all of it for a line
// that misses it. `LutAtmosphere::transmittance`.
fn transmittance(height: f32, mu: f32) -> vec3f {
    var h = height;
    var m = mu;
    if (height > atmosphere.shape.y) {
        let t = air_entry(height, mu);
        if (t < 0.0) { return vec3f(1.0); }
        let top = LUT_RADIUS + atmosphere.shape.y;
        m = clamp(((LUT_RADIUS + height) * mu + t) / top, -1.0, 1.0);
        h = atmosphere.shape.y;
    }
    return lut_bilinear(transmittance_lut, transmittance_uv(h, m)).rgb;
}

// How much of the sun's disc stands over the horizon: `sun_visible`.
fn sun_visible(height: f32, mu_s: f32) -> f32 {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let horizon = -sqrt(h * (2.0 * LUT_RADIUS + h)) / r;
    let over = asin(clamp(mu_s, -1.0, 1.0)) - asin(horizon);
    return smoothstep(-SUN_ANGULAR_RADIUS, SUN_ANGULAR_RADIUS, over);
}

fn sunlight(height: f32, mu_s: f32) -> vec3f {
    return transmittance(height, mu_s) * sun_visible(height, mu_s);
}

// The sky's light on level ground at `height`, the light at zenith cosine
// `mu_s`, per unit of it outside the air: `LutAtmosphere::level_irradiance`.
fn level_irradiance(height: f32, mu_s: f32) -> vec3f {
    let x = vec2f(clamp(mu_s * 0.5 + 0.5, 0.0, 1.0), sqrt(clamp(height / atmosphere.shape.y, 0.0, 1.0)));
    return lut_bilinear(irradiance_lut, uv_from_unit(x, IRRADIANCE_SIZE)).rgb;
}

fn multi_scattering(height: f32, mu_s: f32) -> vec3f {
    let x = vec2f(clamp(mu_s * 0.5 + 0.5, 0.0, 1.0), clamp(height / atmosphere.shape.y, 0.0, 1.0));
    return lut_bilinear(multi_scattering_lut, uv_from_unit(x, MULTI_SCATTERING_SIZE)).rgb;
}

fn rayleigh_phase(c: f32) -> f32 {
    return 3.0 / (16.0 * ATMOSPHERE_PI) * (1.0 + c * c);
}

fn mie_phase(c: f32) -> f32 {
    let g = atmosphere.mie.w;
    let g2 = g * g;
    return 3.0 / (8.0 * ATMOSPHERE_PI) * (1.0 - g2) * (1.0 + c * c)
        / ((2.0 + g2) * pow(max(1.0 + g2 - 2.0 * g * c, 1e-6), 1.5));
}

struct AirRay {
    inscattered: vec3f,
    transmittance: vec3f,
}

// The phases of one light along one ray, and its weight: 1 for the sun,
// the Moon's share of it for the Moon, 0 for none.
struct Light {
    rayleigh: f32,
    mie: f32,
    weight: f32,
}

fn light_along(c: f32, weight: f32) -> Light {
    var l: Light;
    l.rayleigh = rayleigh_phase(c);
    l.mie = mie_phase(c);
    l.weight = weight;
    return l;
}

// What one light scatters towards the eye at a point, per unit of path.
fn source(m: Medium, height: f32, mu: f32, l: Light) -> vec3f {
    if (l.weight == 0.0) { return vec3f(0.0); }
    let light = sunlight(height, mu);
    let multi = multi_scattering(height, mu);
    return (m.rayleigh * l.rayleigh * light + vec3f(m.mie * l.mie) * light
        + (m.rayleigh + vec3f(m.mie)) * multi) * l.weight;
}

// One step of a march, integrated as Hillaire does: the sources over the
// step's own extinction, exact however long the step. Two lights, the sun
// and the Moon, through one air.
fn air_step(ray: ptr<function, AirRay>, height: f32, mu_s: f32, mu_m: f32, sun: Light, moon: Light, dt: f32) {
    let m = medium(height);
    let gathered = source(m, height, mu_s, sun) + source(m, height, mu_m, moon);
    let e = max(m.extinction, vec3f(1e-12));
    let step = exp(-e * dt);
    (*ray).inscattered += (*ray).transmittance * gathered * (vec3f(1.0) - step) / e;
    (*ray).transmittance *= step;
}

// The air between the eye and `p`, eye-relative: what it lets through of
// the ground and what it adds, in the model's units, the sun's light and
// the Moon's together. From above the air, from where the path enters it;
// from high up, more steps and bunched towards the ground, where the air
// is.
fn air_between(p: vec3f) -> AirRay {
    var ray: AirRay;
    ray.inscattered = vec3f(0.0);
    ray.transmittance = vec3f(1.0);
    let length_ = length(p);
    if (length_ <= 0.0) { return ray; }
    let d = p / length_;
    let sun_dir = atmosphere.sun.xyz;
    let moon_dir = atmosphere.moon.xyz;
    let sun = light_along(dot(d, sun_dir), 1.0);
    let moon = light_along(dot(d, moon_dir), atmosphere.moon.w);
    let o = -atmosphere.centre.xyz;
    let eye_height = length(o) - atmosphere.centre.w;
    let t0 = max(air_entry(eye_height, dot(normalize(o), d)), 0.0);
    if (t0 >= length_) { return ray; }
    let high = eye_height > AERIAL_HIGH;
    var steps = AERIAL_STEPS;
    if (high) { steps = AERIAL_STEPS_HIGH; }
    let span = length_ - t0;
    var t_prev = 0.0;
    for (var i = 0u; i < steps; i++) {
        var a = (f32(i) + 0.5) / f32(steps);
        var b = (f32(i) + 1.0) / f32(steps);
        if (high) {
            a = 1.0 - (1.0 - a) * (1.0 - a);
            b = 1.0 - (1.0 - b) * (1.0 - b);
        }
        let dt = (b * span) - t_prev;
        t_prev = b * span;
        let x = o + d * (t0 + a * span);
        let r = length(x);
        air_step(&ray, r - atmosphere.centre.w, dot(x, sun_dir) / r, dot(x, moon_dir) / r, sun, moon, dt);
    }
    return ray;
}

// Where a ray from radius `r` at zenith cosine `mu` meets the ground, or a
// negative number.
fn ground_distance(r: f32, mu: f32, height: f32) -> f32 {
    let disc = r * r * mu * mu - height * (2.0 * LUT_RADIUS + height);
    if (mu >= 0.0 || disc < 0.0) { return -1.0; }
    return -r * mu - sqrt(disc);
}

// The sky seen from `height` along `dir` under one light along `light`,
// all unit in a frame whose z is up: `LutAtmosphere::sky_radiance`, without
// the light's disc, per unit of that light. From above the air, from where
// the ray enters it, and nothing for a ray that misses it. Steps spread
// quadratically; the ground, met, lit and seen.
fn sky_radiance(height: f32, dir: vec3f, light: vec3f) -> vec3f {
    let entry = air_entry(height, dir.z);
    if (entry < 0.0) { return vec3f(0.0); }
    var origin = vec3f(0.0, 0.0, LUT_RADIUS + max(height, 0.0));
    var d = dir;
    var l = light;
    var h = max(height, 0.0);
    if (entry > 0.0) {
        // Moved to the entry, and the frame turned so that z is up there.
        let start = origin + dir * entry;
        let up = normalize(start);
        var axis = vec3f(0.0, 1.0, 0.0);
        if (abs(up.y) > 0.9) { axis = vec3f(1.0, 0.0, 0.0); }
        let east = normalize(cross(axis, up));
        let north = cross(up, east);
        d = vec3f(dot(dir, east), dot(dir, north), dot(dir, up));
        l = vec3f(dot(light, east), dot(light, north), dot(light, up));
        h = atmosphere.shape.y;
        origin = vec3f(0.0, 0.0, LUT_RADIUS + h);
    }
    let r = LUT_RADIUS + h;
    let mu = d.z;
    let top = LUT_RADIUS + atmosphere.shape.y;
    let ground = ground_distance(r, mu, h);
    let disc = (top - r) * (top + r) + r * r * mu * mu;
    var t_max = max(-r * mu + sqrt(max(disc, 0.0)), 0.0);
    if (ground >= 0.0) { t_max = ground; }
    var ray: AirRay;
    ray.inscattered = vec3f(0.0);
    ray.transmittance = vec3f(1.0);
    let one = light_along(dot(d, l), 1.0);
    let none = light_along(0.0, 0.0);
    var t_prev = 0.0;
    for (var s = 0u; s < SKY_STEPS; s++) {
        let a = (f32(s) + 0.3) / f32(SKY_STEPS);
        let b = (f32(s) + 1.0) / f32(SKY_STEPS);
        let t = t_max * a * a;
        let t_next = t_max * b * b;
        let dt = t_next - t_prev;
        t_prev = t_next;
        let x = origin + d * t;
        let pr = length(x);
        air_step(&ray, pr - LUT_RADIUS, dot(x, l) / pr, 0.0, one, none, dt);
    }
    if (ground >= 0.0) {
        let x = origin + d * t_max;
        let up = normalize(x);
        let mu_s = dot(up, l);
        ray.inscattered += ray.transmittance * sunlight(0.0, mu_s) * max(mu_s, 0.0)
            * atmosphere.shape.z / ATMOSPHERE_PI;
    }
    return ray.inscattered;
}

// The sky-view table's parametrisation (Hillaire 2020 §5.3): across, the
// cosine of the azimuth from the sun, `1 − 2u²`; down, the zenith angle,
// the horizon at the middle and the rows bunched towards it on both sides.
// `sky_view_direction` and `sky_view_uv` are each other's inverse, and
// `hillaire::sky_view_*` their twins.
fn sky_view_direction(unit: vec2f, height: f32) -> vec2f {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let beta = acos(clamp(sqrt(h * (2.0 * LUT_RADIUS + h)) / r, -1.0, 1.0));
    let zenith_horizon = ATMOSPHERE_PI - beta;
    var theta: f32;
    if (unit.y < 0.5) {
        let k = 1.0 - 2.0 * unit.y;
        theta = zenith_horizon * (1.0 - k * k);
    } else {
        let k = 2.0 * unit.y - 1.0;
        theta = zenith_horizon + beta * k * k;
    }
    // (cosine of the zenith angle, cosine of the azimuth from the sun)
    return vec2f(cos(theta), 1.0 - 2.0 * unit.x * unit.x);
}

fn sky_view_uv(height: f32, mu: f32, cos_azimuth: f32) -> vec2f {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let beta = acos(clamp(sqrt(h * (2.0 * LUT_RADIUS + h)) / r, -1.0, 1.0));
    let zenith_horizon = ATMOSPHERE_PI - beta;
    let theta = acos(clamp(mu, -1.0, 1.0));
    var y: f32;
    if (theta < zenith_horizon) {
        y = 0.5 * (1.0 - sqrt(max(1.0 - theta / zenith_horizon, 0.0)));
    } else {
        y = 0.5 + 0.5 * sqrt(max((theta - zenith_horizon) / beta, 0.0));
    }
    let x = sqrt(clamp(0.5 - 0.5 * cos_azimuth, 0.0, 1.0));
    return uv_from_unit(vec2f(x, y), SKY_VIEW_SIZE);
}

// The direction `d` (any length) from the frame's eye as the sky-view
// table of the light along `light` indexes it.
fn sky_view_uv_of(d: vec3f, light: vec3f) -> vec2f {
    let up = atmosphere.up.xyz;
    let n = normalize(d);
    let mu = dot(n, up);
    let flat_d = n - up * mu;
    let sun = light;
    let flat_s = sun - up * dot(sun, up);
    var c = 1.0;
    let ld = dot(flat_d, flat_d);
    let ls = dot(flat_s, flat_s);
    if (ld > 1e-12 && ls > 1e-12) { c = dot(flat_d, flat_s) * inverseSqrt(ld * ls); }
    return sky_view_uv(atmosphere.sun.w, mu, c);
}

// The light a ground fragment at `height`, up `up`, facing `normal`, is lit
// by, in the renderer's units: its own sun and Moon through the air above
// it and its own sky — the frame's harmonic scaled by how its level
// ground's sky stands against the frame's — the night floor on all of it,
// and never less than the least light. `lit` takes the sun's share a
// shadow leaves.
fn ground_light(height: f32, up: vec3f, normal: vec3f, frame_sky: vec3f, lit: f32) -> vec3f {
    let sun_dir = atmosphere.sun.xyz;
    let moon_dir = atmosphere.moon.xyz;
    let mu_s = dot(up, sun_dir);
    let mu_m = dot(up, moon_dir);
    let g = atmosphere.ground.xyz;
    var level = g * level_irradiance(height, mu_s) + atmosphere.added.xyz;
    var direct = g * sunlight(height, mu_s) * max(dot(normal, sun_dir), 0.0) * lit;
    if (atmosphere.moon.w > 0.0) {
        level += g * level_irradiance(height, mu_m) * atmosphere.moon.w;
        direct += g * sunlight(height, mu_m) * atmosphere.moon.w * max(dot(normal, moon_dir), 0.0);
    }
    let ratio = level / max(atmosphere.level_ref.xyz, vec3f(1e-30));
    let sky = max(frame_sky, vec3f(0.0)) * ratio;
    return max(sky + direct, atmosphere.least.xyz);
}

// The Moon's disc along the unit `n` from the eye, in the model's units
// (per unit of the sun outside the air): lit as the Lommel-Seeliger law
// lights a dusty surface — μ₀/(μ₀ + μ), which leaves the full Moon about
// as bright at its limb as at its centre — its night side by the Earth's
// light, all of it through the air between.
fn moon_seen(n: vec3f) -> vec3f {
    let m = atmosphere.moon.xyz;
    let rho = atmosphere.moon_disc.w;
    if (atmosphere.moon.w == 0.0 || rho <= 0.0) { return vec3f(0.0); }
    let c = dot(n, m);
    let p = (n - m * c) / rho;
    let x2 = dot(p, p);
    if (c <= 0.0 || x2 >= 1.0) { return vec3f(0.0); }
    let mu = sqrt(1.0 - x2);
    // The surface's normal, towards the eye's side: −m is towards the eye.
    let normal = p - m * mu;
    let mu0 = dot(normal, atmosphere.moon_disc.xyz);
    var shade = atmosphere.moon_shade.y;
    if (mu0 > 0.0) { shade += mu0 / (mu0 + mu); }
    let solid = 2.0 * ATMOSPHERE_PI * (1.0 - cos(rho));
    let mu_m = dot(atmosphere.up.xyz, m);
    // An edge a texel soft: the disc is not drawn as a step.
    let edge = smoothstep(1.0, 0.96, sqrt(x2));
    return sunlight(atmosphere.sun.w, mu_m) * atmosphere.moon.w * shade
        / (atmosphere.moon_shade.x * solid) * edge;
}

fn star_hash(v: vec3u) -> vec3u {
    // A PCG-style mix: three words in, three out.
    var x = v * 1664525u + 1013904223u;
    x.x += x.y * x.z;
    x.y += x.z * x.x;
    x.z += x.x * x.y;
    x ^= x >> vec3u(16u);
    x.x += x.y * x.z;
    x.y += x.z * x.x;
    x.z += x.x * x.y;
    return x;
}

// Stars along the unit `n`, in the model's units, `sigma` the radius of a
// star's image in radians (about a pixel). Not a catalogue: one star a cell
// of a cube laid on the celestial sphere, its magnitude so drawn that the
// whole sky holds the stars it does — five brighter than magnitude 0, five
// thousand than 6 — fixed to the stars' own frame, so they turn about the
// pole once a sidereal day; dimmed by the air below them, and lost in any
// sky brighter than they are.
fn stars_seen(n: vec3f, sigma: f32) -> vec3f {
    if (atmosphere.stars_x.w == 0.0) { return vec3f(0.0); }
    let c = vec3f(dot(atmosphere.stars_x.xyz, n), dot(atmosphere.stars_y.xyz, n), dot(atmosphere.stars_z.xyz, n));
    let a = abs(c);
    var face = 0u;
    var uv = vec2f(0.0);
    var major = 0.0;
    if (a.x >= a.y && a.x >= a.z) {
        face = select(1u, 0u, c.x > 0.0); major = a.x; uv = c.yz;
    } else if (a.y >= a.z) {
        face = select(3u, 2u, c.y > 0.0); major = a.y; uv = c.xz;
    } else {
        face = select(5u, 4u, c.z > 0.0); major = a.z; uv = c.xy;
    }
    let cells = 96.0;
    let q = (uv / major * 0.5 + 0.5) * cells;
    let cell = floor(q);
    let h = star_hash(vec3u(u32(cell.x), u32(cell.y), face));
    let r = vec3f(h & vec3u(0xffffffu)) / 16777216.0;
    let at = (cell + 0.2 + 0.6 * r.xy) / cells * 2.0 - 1.0;
    var s = vec3f(0.0);
    let sign = select(-1.0, 1.0, face % 2u == 0u);
    if (face < 2u) { s = vec3f(sign, at.x, at.y); }
    else if (face < 4u) { s = vec3f(at.x, sign, at.y); }
    else { s = vec3f(at.x, at.y, sign); }
    s = normalize(s);
    // Its rank among the cells, brightest first, and so its magnitude.
    let rank = max(r.z, 1e-6) * 6.0 * cells * cells;
    let magnitude = 2.0 * log2(rank / 5.0) / log2(10.0);
    if (magnitude > 7.5) { return vec3f(0.0); }
    let e = pow(10.0, -0.4 * (magnitude + 26.74));
    let theta2 = dot(c - s, c - s);
    let spot = e / (2.0 * ATMOSPHERE_PI * sigma * sigma) * exp(-theta2 / (2.0 * sigma * sigma));
    let tint = mix(vec3f(1.0, 0.82, 0.66), vec3f(0.78, 0.86, 1.0), f32((h.x >> 24u) & 255u) / 255.0);
    let mu = dot(atmosphere.up.xyz, n);
    return spot * tint * transmittance(atmosphere.sun.w, mu);
}
