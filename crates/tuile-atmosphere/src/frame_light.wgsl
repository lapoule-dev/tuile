// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// A frame's light on the GPU, every frame: the twin of
// `LutAtmosphere::light`, sum for sum. Two steps, for whichever pass makes
// them — compute where the device has it, a fragment pass where it has not
// (WebGL2):
//
// - `light_partial(x, row)`: one of LIGHT_THREADS shares of the frame's
//   sums — the sky's light on five facings under the sun and under the
//   Moon, over LIGHT_DIRECTIONS directions, and the meter's rays, marched;
// - `light_slot(j)`: the shares added up and the light finished — the
//   calibration, the harmonic, the meter, the night floor, the exposure,
//   the white balance — as slot `j` of the frame's uniform (`Atmosphere`),
//   pre-exposed, or one of the extras after it.
//
// Nothing here is kept from one frame to the next: the light is a function
// of the frame's own inputs, and a camera that moves a little moves it a
// little. Included after `hillaire.wgsl`, with `atmosphere` (the frame's
// uniform: the air and where the sun and the Moon are), the tables, and
// `light_in` and `light_rays` declared; `light_slot` reads `light_partials`.

struct LightInputs {
    // Unit east, north and up at the ground under the eye, ECEF; w of
    // each: that ground's height, the compensation in stops, the Moon's
    // strength (its irradiance outside the air over the sun's, 0: none).
    enu_x: vec4f,
    enu_y: vec4f,
    enu_z: vec4f,
    // Towards the sun and the Moon, ECEF; w: the eye's height, how many
    // rays the meter has.
    to_sun: vec4f,
    to_moon: vec4f,
    // Towards the sun and the Moon in the eye's own frame (z up); w: the
    // reflectance a texel stands for, the meter's reflectance.
    eye_sun: vec4f,
    eye_moon: vec4f,
    // What one unit of the model's irradiance is on the ground, times pi;
    // w: how much of a light's difference the exposure makes up.
    scale: vec4f,
    // The look's lights on level ground under the reference; w: how dark
    // a dim light is shown, in stops.
    calibrated: vec4f,
    // The look's white balance; w: how far the camera adapts to the white.
    look_white: vec4f,
    // x: the look's exposure, stops; yzw: nothing.
    look: vec4f,
    // The w of the extras: the sky's ambient, the sun, the white balance.
    extras_w: vec4f,
}

const LIGHT_LUMA: vec3f = vec3f(0.2126, 0.7152, 0.0722);

fn light_ground_light(l: vec3f) -> vec3f {
    return vec3f(dot(l, light_in.enu_x.xyz), dot(l, light_in.enu_y.xyz), dot(l, light_in.enu_z.xyz));
}

// The `i`th of the directions every facing's sky is summed over: a golden
// spiral, `LutAtmosphere::sky_irradiance`'s.
fn light_direction(i: u32) -> vec3f {
    let n = f32(LIGHT_DIRECTIONS);
    let z = 1.0 - 2.0 * (f32(i) + 0.5) / n;
    let ring = sqrt(max(1.0 - z * z, 0.0));
    // The golden angle's turns, folded to one before the cosine.
    let turns = fract(f32(i) * 0.38196601125010515);
    let phi = turns * 2.0 * ATMOSPHERE_PI;
    return vec3f(ring * cos(phi), ring * sin(phi), z);
}

// The light's horizontal direction, `toward`, and the one beside it.
fn light_toward(l: vec3f) -> vec3f {
    let flat_l = vec3f(l.x, l.y, 0.0);
    if (dot(flat_l, flat_l) < 1e-24) { return vec3f(1.0, 0.0, 0.0); }
    return normalize(flat_l);
}

// The five facings a light's sky is summed on: up, towards it, away from
// it, and the two sides.
fn light_facing(l: vec3f, f: u32) -> vec3f {
    let toward = light_toward(l);
    let aside = cross(vec3f(0.0, 0.0, 1.0), toward);
    switch (f) {
        case 0u: { return vec3f(0.0, 0.0, 1.0); }
        case 1u: { return toward; }
        case 2u: { return -toward; }
        case 3u: { return aside; }
        default: { return -aside; }
    }
}

// A ray from `height` along `dir`, the light along `light` (unit, z up at
// the start), for `length` metres at most: `LutAtmosphere::march` with a
// length, its steps even. From above the air, from where it enters it.
fn light_march(height: f32, dir: vec3f, light: vec3f, length_: f32, steps: u32) -> AirRay {
    var ray: AirRay;
    ray.inscattered = vec3f(0.0);
    ray.transmittance = vec3f(1.0);
    let top = LUT_RADIUS + atmosphere.shape.y;
    let r0 = LUT_RADIUS + max(height, 0.0);
    var entry = 0.0;
    if (r0 > top) {
        let disc = (top - r0) * (top + r0) + r0 * r0 * dir.z * dir.z;
        if (dir.z >= 0.0 || disc < 0.0) { return ray; }
        entry = -r0 * dir.z - sqrt(disc);
    }
    let start = vec3f(0.0, 0.0, r0) + dir * entry;
    let r = length(start);
    let up = start / r;
    var axis = vec3f(0.0, 1.0, 0.0);
    if (abs(up.y) > 0.9) { axis = vec3f(1.0, 0.0, 0.0); }
    let east = normalize(cross(axis, up));
    let north = cross(up, east);
    let d = vec3f(dot(dir, east), dot(dir, north), dot(dir, up));
    let l = vec3f(dot(light, east), dot(light, north), dot(light, up));
    let mu = d.z;
    var to_end = 0.0;
    let c = (r - LUT_RADIUS) * (r + LUT_RADIUS);
    let ground_disc = r * r * mu * mu - c;
    // The near root without cancellation, as `ground_distance`.
    let ground = c / (-r * mu + sqrt(max(ground_disc, 0.0)));
    if (mu < 0.0 && ground_disc >= 0.0 && ground >= 0.0) {
        to_end = ground;
    } else {
        let exit_disc = (top - r) * (top + r) + r * r * mu * mu;
        if (exit_disc >= 0.0) { to_end = -r * mu + sqrt(exit_disc); }
    }
    let t_max = max(min(length_ - entry, max(to_end, 0.0)), 0.0);
    let one = light_along(dot(d, l), 1.0);
    let none = light_along(0.0, 0.0);
    let origin = vec3f(0.0, 0.0, r);
    var t_prev = 0.0;
    for (var s = 0u; s < steps; s++) {
        let t = t_max * (f32(s) + 0.5) / f32(steps);
        let t_next = t_max * (f32(s) + 1.0) / f32(steps);
        let dt = t_next - t_prev;
        t_prev = t_next;
        let p = origin + d * t;
        let pr = length(p);
        air_step(&ray, pr - LUT_RADIUS, dot(p, l) / pr, 0.0, one, none, dt);
    }
    return ray;
}

// How much a meter's ray of the sky counts: by its optical depth, whole
// from METER_AIR of the column over the ground, nothing past the air.
fn light_meter_weight(height: f32, mu: f32) -> f32 {
    let column = -log(max(transmittance(0.0, 1.0).g, 1e-30));
    let depth = -log(max(transmittance(height, mu).g, 1e-30));
    return clamp(depth / (METER_AIR * column), 0.0, 1.0);
}

// Level ground's light at `height`, up `up` (ECEF), from the tables: the
// sun's beam and sky, the Moon's by its strength, in the renderer's units.
fn light_tabled(height: f32, up: vec3f) -> vec3f {
    let ground = light_in.scale.xyz / ATMOSPHERE_PI;
    let mu_s = dot(light_in.to_sun.xyz, up);
    var out = sunlight(height, mu_s) * max(mu_s, 0.0) + level_irradiance(height, mu_s);
    let strength = light_in.enu_z.w;
    if (strength > 0.0) {
        let mu_m = dot(light_in.to_moon.xyz, up);
        out += (sunlight(height, mu_m) * max(mu_m, 0.0) + level_irradiance(height, mu_m)) * strength;
    }
    return out * ground;
}

// One share of the frame's sums. Rows 0–4: the sky under the sun on the
// five facings; 5–9: under the Moon; 10: the meter's ground light through
// the air before it, w the rays counted; 11: what the air adds before the
// meter's ground and the meter's sky.
fn light_partial(x: u32, row: u32) -> vec4f {
    let height = light_in.enu_x.w;
    let strength = light_in.enu_z.w;
    if (row < 10u) {
        var l = light_ground_light(light_in.to_sun.xyz);
        if (row >= 5u) {
            if (strength <= 0.0) { return vec4f(0.0); }
            l = light_ground_light(light_in.to_moon.xyz);
        }
        let n = light_facing(l, row % 5u);
        let solid = 4.0 * ATMOSPHERE_PI / f32(LIGHT_DIRECTIONS);
        var sum = vec3f(0.0);
        for (var i = x; i < LIGHT_DIRECTIONS; i += LIGHT_THREADS) {
            let dir = light_direction(i);
            let c = dot(n, dir);
            if (c > 0.0) {
                sum += sky_radiance(height, dir, l) * c * solid;
            }
        }
        return vec4f(sum, 0.0);
    }
    let eye_height = light_in.to_sun.w;
    let rays = u32(light_in.to_moon.w);
    var seen = vec3f(0.0);
    var added = vec3f(0.0);
    var counted = 0.0;
    for (var j = x; j < rays; j += LIGHT_THREADS) {
        let a = textureLoad(light_rays, vec2i(i32(j), 0), 0);
        if (a.w >= 0.0) {
            let b = textureLoad(light_rays, vec2i(i32(j), 1), 0);
            let l = light_tabled(b.w, b.xyz);
            let sun = light_march(eye_height, a.xyz, light_in.eye_sun.xyz, a.w, METER_STEPS);
            var through = sun.transmittance;
            var air = sun.inscattered;
            if (strength > 0.0) {
                let moon = light_march(eye_height, a.xyz, light_in.eye_moon.xyz, a.w, METER_STEPS);
                through = moon.transmittance;
                air += moon.inscattered * strength;
            }
            seen += l * through;
            added += air;
            counted += 1.0;
        } else {
            // By the air it crosses: `LutAtmosphere::meter_weight`.
            let weight = light_meter_weight(eye_height, a.z);
            if (weight <= 0.0) { continue; }
            var l = sky_radiance(eye_height, a.xyz, light_in.eye_sun.xyz);
            if (strength > 0.0) {
                l += sky_radiance(eye_height, a.xyz, light_in.eye_moon.xyz) * strength;
            }
            added += l * weight;
            counted += weight;
        }
    }
    if (row == 10u) { return vec4f(seen, counted); }
    return vec4f(added, 0.0);
}

// The sum of one row's shares.
fn light_sum(row: u32) -> vec4f {
    var sum = vec4f(0.0);
    for (var x = 0u; x < LIGHT_THREADS; x++) {
        sum += textureLoad(light_partials, vec2i(i32(x), i32(row)), 0);
    }
    return sum;
}

fn light_shown_ev(ev: f32) -> f32 {
    let kept = 1.0 - light_in.scale.w;
    if (ev >= 0.0) { return kept * ev; }
    let depth = light_in.calibrated.w;
    let b = depth / (kept * 0.6931471805599453);
    return -depth * log2(1.0 - ev / b);
}

// A light's sky as an L1 harmonic, from its five facings' sums: the
// constant, the gradient (rows of ECEF x, y and z, one channel a
// component), and level ground's.
struct LightHarmonic {
    mean: vec3f,
    gx: vec3f,
    gy: vec3f,
    gz: vec3f,
    level: vec3f,
}

fn light_harmonic(first: u32, l: vec3f) -> LightHarmonic {
    let e0 = light_sum(first).xyz;
    let e1 = light_sum(first + 1u).xyz;
    let e2 = light_sum(first + 2u).xyz;
    let e3 = light_sum(first + 3u).xyz;
    let e4 = light_sum(first + 4u).xyz;
    let mean = (e1 + e2 + e3 + e4) / 4.0;
    let b0 = (e1 - e2) / 2.0;
    let b1 = (e3 - e4) / 2.0;
    let b2 = e0 - mean;
    let toward_l = light_toward(l);
    let aside_l = cross(vec3f(0.0, 0.0, 1.0), toward_l);
    let east = light_in.enu_x.xyz;
    let north = light_in.enu_y.xyz;
    let up = light_in.enu_z.xyz;
    let toward = east * toward_l.x + north * toward_l.y + up * toward_l.z;
    let aside = east * aside_l.x + north * aside_l.y + up * aside_l.z;
    var h: LightHarmonic;
    h.mean = mean;
    h.gx = toward.x * b0 + aside.x * b1 + up.x * b2;
    h.gy = toward.y * b0 + aside.y * b1 + up.y * b2;
    h.gz = toward.z * b0 + aside.z * b1 + up.z * b2;
    h.level = e0;
    return h;
}

fn light_smooth_step(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp((x - a) / (b - a), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Slot `j` of the frame's uniform, the light finished and pre-exposed —
// or, past the uniform's last, the extras: the sky's ambient and the sun
// for a renderer's own light, the white balance, and the exposure.
fn light_slot(j: u32) -> vec4f {
    let height = light_in.enu_x.w;
    let strength = light_in.enu_z.w;
    let scale = light_in.scale.xyz;
    let ground = scale / ATMOSPHERE_PI;
    let sun_l = light_ground_light(light_in.to_sun.xyz);
    let sun_sky = light_harmonic(0u, sun_l);
    let transmitted = sunlight(height, sun_l.z);
    var mean = sun_sky.mean;
    var gx = sun_sky.gx;
    var gy = sun_sky.gy;
    var gz = sun_sky.gz;
    var level = transmitted * max(sun_l.z, 0.0) + sun_sky.level;
    if (strength > 0.0) {
        let moon_l = light_ground_light(light_in.to_moon.xyz);
        let moon_sky = light_harmonic(5u, moon_l);
        let beam = sunlight(height, moon_l.z) * strength;
        mean += moon_sky.mean * strength;
        gx += moon_sky.gx * strength;
        gy += moon_sky.gy * strength;
        gz += moon_sky.gz * strength;
        level += beam * max(moon_l.z, 0.0) + moon_sky.level * strength;
    }
    var ambient = mean * ground;
    gx *= ground;
    gy *= ground;
    gz *= ground;
    level *= ground;

    // The meter: its ground set to agree with the exact sum here.
    let here = light_tabled(height, light_in.enu_z.xyz);
    let agree = select(vec3f(1.0), level / max(here, vec3f(1e-30)), here > vec3f(1e-30));
    let seen = light_sum(10u);
    let added_air = light_sum(11u).xyz;
    var metered = level;
    if (light_in.to_moon.w > 0.0) {
        metered = (agree * seen.xyz + added_air * scale / light_in.eye_moon.w + level * METER_FLOOR)
            / (seen.w + METER_FLOOR);
    }

    // The night floor, the exposure, the least light.
    let wanted = dot(light_in.calibrated.xyz, LIGHT_LUMA);
    let tint = NIGHT_TINT;
    let floor_light = NIGHT_FLOOR * wanted;
    let added = tint * floor_light / dot(tint, LIGHT_LUMA);
    metered += added;
    let x = log2(dot(metered, LIGHT_LUMA) / wanted);
    let ev100 = DAYLIGHT_EV100 + x;
    let least_light = SHOWN_FLOOR * wanted * exp2(x - light_shown_ev(x));
    let least = tint * max(least_light, floor_light) / dot(tint, LIGHT_LUMA);
    ambient += added;

    // The white the camera balances to.
    let white_lum = dot(metered, LIGHT_LUMA) / wanted;
    let night = light_smooth_step(-4.0, -20.0, x);
    let physical = metered / max(light_in.calibrated.xyz, vec3f(1e-12)) / max(white_lum, 1e-30);
    let white = max(physical * (1.0 - night) + NIGHT_WHITE * night, vec3f(1e-6));
    let full = vec3f(dot(white, LIGHT_LUMA)) / white;
    let adaptation = clamp(light_in.look_white.w, 0.0, 1.0);
    let balance = light_in.look_white.xyz * (vec3f(1.0) + (full - vec3f(1.0)) * adaptation);

    let air = max(light_in.eye_sun.w, 1e-6);
    let floor_ = added * NIGHT_SKY_ALBEDO / air;
    var sky_here = level_irradiance(height, dot(light_in.to_sun.xyz, light_in.enu_z.xyz));
    if (strength > 0.0) {
        sky_here += level_irradiance(height, dot(light_in.to_moon.xyz, light_in.enu_z.xyz)) * strength;
    }
    let level_reference = sky_here * ground + added;

    let k = exp2(light_in.look.x + light_in.enu_y.w + light_shown_ev(x) - x);
    switch (j) {
        case SLOT_ILLUMINANCE: { return vec4f(scale / air * k, 0.0); }
        case SLOT_FLOOR: { return vec4f(floor_ * k, 0.0); }
        case SLOT_SH_X: { return vec4f(gx * k, 0.0); }
        case SLOT_SH_Y: { return vec4f(gy * k, 0.0); }
        case SLOT_SH_Z: { return vec4f(gz * k, 0.0); }
        case SLOT_GROUND: { return vec4f(ground * k, 0.0); }
        case SLOT_ADDED: { return vec4f(added * k, 0.0); }
        case SLOT_LEAST: { return vec4f(least * k, 0.0); }
        case SLOT_LEAST_SKY: {
            return vec4f(floor_ * least / max(added, vec3f(1e-30)) * k, 0.0);
        }
        case SLOT_LEVEL_REF: { return vec4f(level_reference * k, 0.0); }
        case SLOT_AMBIENT: { return vec4f(ambient * k, 0.0); }
        case SLOT_GRADE: { return vec4f(balance, atmosphere.grade.w); }
        case EXTRA_AMBIENT: { return vec4f(ambient * k, light_in.extras_w.x); }
        case EXTRA_SUN: { return vec4f(transmitted * ground * k, light_in.extras_w.y); }
        case EXTRA_WHITE: { return vec4f(balance, light_in.extras_w.z); }
        case EXTRA_EXPOSURE: { return vec4f(k, ev100, x, dot(metered, LIGHT_LUMA)); }
        default: { return vec4f(0.0); }
    }
}
