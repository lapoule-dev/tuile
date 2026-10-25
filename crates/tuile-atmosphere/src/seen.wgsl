// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// What an eye sees of the physical atmosphere, in the renderer's units: the
// sky along a line of sight, and a ground point through the air before it.
// One implementation for every renderer that draws this air — the film's
// compute passes and the viewer's fragments call these, and nothing else
// says what the sky or the lit ground is. Included after `hillaire.wgsl`,
// whose bindings it reads: the sky's the sky-view tables besides.

// The share of the air's thickness from which the sky is blended from the
// tables towards the march, all of it march at the top.
const SKY_BLEND: f32 = 0.7;

// The physical sky along `d` from the frame's eye: what the air sends
// under the sun and under the Moon — from the frame's sky-view tables in
// the air, marched along the ray itself from above it, where the air is a
// band a few degrees thick along the limb that no table row could hold —
// what the night floor adds, and behind the air the sun's disc, darkened
// towards its limb, the Moon's in its phase and the stars, dimmed by the
// air before them and hidden by the planet. `star_sigma`: the radius of a
// star's image, in radians — about a pixel.
fn sky_seen(d: vec3f, star_sigma: f32) -> vec3f {
    let n = normalize(d);
    let h = atmosphere.sun.w;
    let up = atmosphere.up.xyz;
    // In the air, from the sky-view tables; from above it, marched along
    // the ray, where the air is a band a few degrees thick along the limb
    // that no table row could hold. Between the two a band of heights where
    // both are summed and blended, so that an eye climbing out of the air
    // meets no step: one sky, continuous in the eye's height.
    let marched = smoothstep(SKY_BLEND * atmosphere.shape.y, atmosphere.shape.y, h);
    var air = vec3f(0.0);
    if (marched > 0.0) {
        var axis = vec3f(0.0, 0.0, 1.0);
        if (abs(up.z) > 0.9) { axis = vec3f(1.0, 0.0, 0.0); }
        let east = normalize(cross(axis, up));
        let north = cross(up, east);
        let local = vec3f(dot(n, east), dot(n, north), dot(n, up));
        let s = atmosphere.sun.xyz;
        air = sky_radiance(h, local, vec3f(dot(s, east), dot(s, north), dot(s, up)));
        if (atmosphere.moon.w > 0.0) {
            let m = atmosphere.moon.xyz;
            air += sky_radiance(h, local, vec3f(dot(m, east), dot(m, north), dot(m, up)))
                * atmosphere.moon.w;
        }
        air *= marched;
    }
    if (marched < 1.0) {
        var tabled = lut_bilinear(sky_view_lut, sky_view_uv_of(d, atmosphere.sun.xyz)).rgb;
        if (atmosphere.moon.w > 0.0) {
            tabled += lut_bilinear(moon_view_lut, sky_view_uv_of(d, atmosphere.moon.xyz)).rgb
                * atmosphere.moon.w;
        }
        air += tabled * (1.0 - marched);
    }
    var sky = max(air * atmosphere.illuminance.xyz + atmosphere.floor.xyz, atmosphere.least_sky.xyz);
    // Behind the air, unless the planet stands before it.
    let r = LUT_RADIUS + max(h, 0.0);
    if (ground_distance(r, dot(n, up), max(h, 0.0)) >= 0.0) { return sky; }
    var beyond = vec3f(0.0);
    let apart = acos(clamp(dot(n, atmosphere.sun.xyz), -1.0, 1.0));
    // Drawn larger by its scale, its surface as bright as it is.
    let sun_rho = SUN_ANGULAR_RADIUS * max(atmosphere.discs.x, 0.0);
    if (apart < sun_rho) {
        let x = apart / sun_rho;
        let limb = 1.0 - 0.6 * (1.0 - sqrt(max(1.0 - x * x, 0.0)));
        let solid = 2.0 * ATMOSPHERE_PI * (1.0 - cos(SUN_ANGULAR_RADIUS));
        let mu_s = dot(up, atmosphere.sun.xyz);
        beyond += sunlight(h, mu_s) * limb / solid;
    }
    beyond += moon_seen(n);
    beyond += stars_seen(n, star_sigma);
    return sky + beyond * atmosphere.illuminance.xyz;
}

// A ground point as the eye sees it, in the renderer's units: `base` its
// imagery's colour, `p` where it is drawn and `ground` the ground it means
// (both from the eye: see `meant_ground` in a film's resolve), `normal`
// its facing towards the eye, `ambient` the frame's sky light on that
// facing and `lit` the sun's share a shadow leaves it.
//
// Its own light, where it is: its sun and Moon through the air above it,
// its sky — so that a terminator a frame shows falls where it is, the night
// side lit by the night — and never darker than ground of a fortieth's
// reflectance: a night sea's imagery is all but black, and so would be its
// picture. Then the air between the eye and the ground, marched over the
// tables: what it lets through, and the light it scatters towards the eye
// under the sun and the Moon — with, at night, the floor's share of the
// sky's, so that far ground fades to the night sky.
fn ground_seen(base: vec3f, p: vec3f, ground: vec3f, normal: vec3f, ambient: vec3f, lit: f32) -> vec3f {
    var radiance = vec3f(0.0);
    {
        let x = ground - atmosphere.centre.xyz;
        let r = length(x);
        let light = ground_light(r - atmosphere.centre.w, x / r, normal, ambient, lit);
        let texel_floor = 0.025 * atmosphere.illuminance.y
            / (ATMOSPHERE_PI * max(atmosphere.ground.y, 1e-30));
        radiance = max(base, vec3f(texel_floor)) * light + city_light(p, light);
    }
    let air = air_between(ground);
    var veil = air.inscattered * atmosphere.illuminance.xyz;
    {
        // The veil bounded: never more than a few times the light of the
        // ground under it, on ground of a tenth's reflectance. By day the
        // air seldom reaches it; at twilight the air, still lit by the sky
        // high above, would otherwise lay a uniform grey over ground lit by
        // little — and the relief would go. A soft bound: the veil it
        // leaves keeps its colour and its gradient with distance.
        let luma = vec3f(0.2126, 0.7152, 0.0722);
        let x = ground - atmosphere.centre.xyz;
        let r = length(x);
        let lit = ground_light(r - atmosphere.centre.w, x / r, x / r, ambient, 1.0);
        let texel = 0.1 * atmosphere.illuminance.y / (ATMOSPHERE_PI * max(atmosphere.ground.y, 1e-30));
        let cap = atmosphere.tuning.x * texel * dot(lit, luma);
        let v = dot(veil, luma);
        veil /= sqrt(1.0 + (v / max(cap, 1e-30)) * (v / max(cap, 1e-30)));
    }
    radiance = radiance * air.transmittance
        + veil
        + atmosphere.floor.xyz * (vec3f(1.0) - air.transmittance);
    // Where the camera is exposed for a day side, the night side is shown
    // no darker than the night sky beside it: ground of a tenth's
    // reflectance at the sky's least, its own imagery scaling it (a sea
    // darker, snow brighter), fading with distance to the sky's least
    // itself. Without it, ground the least light shows at a sixteenth of
    // noon times its reflectance is all but black, and across a night side
    // seen at a grazing angle the air leaves next to nothing of it — a
    // black band under a sky drawn no darker than its least.
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    let texel_floor = 0.025 * atmosphere.illuminance.y
        / (ATMOSPHERE_PI * max(atmosphere.ground.y, 1e-30));
    let tenths = dot(max(base, vec3f(texel_floor)), luma)
        / max(0.1 * atmosphere.illuminance.y / (ATMOSPHERE_PI * max(atmosphere.ground.y, 1e-30)), 1e-30);
    let shown = atmosphere.least_sky.xyz * (min(tenths, 3.0) * air.transmittance + (vec3f(1.0) - air.transmittance));
    return max(radiance, shown);
}

// Highlights rolled off, not cut: above `knee` (a linear luminance)
// luminance bends towards white and never reaches past it, colours keeping
// their ratios. A picture pushed bright keeps what its bright ground
// showed, where a straight clip turned towns and beaches to flat white.
fn roll_off(linear: vec3f, knee: f32) -> vec3f {
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    let lit = dot(linear, luma);
    var out = linear;
    if (lit > knee) {
        // tanh of a large number is 1, but written as a ratio of two
        // exponentials some drivers make it ∞/∞: a sun's disc, thousands of
        // times white, came out black. Past 10 it is 1 to the last bit.
        let bent = knee + (1.0 - knee) * tanh(min((lit - knee) / (1.0 - knee), 10.0));
        out *= bent / lit;
    }
    return out;
}

// A picture's light as a renderer that draws straight to the display shows
// it: pre-exposed already, balanced by the camera's white and its
// highlights rolled off — the film's grade without its contrast and
// saturation, which are a film's own.
fn graded(linear: vec3f) -> vec3f {
    return roll_off(linear * atmosphere.grade.xyz, atmosphere.grade.w);
}
