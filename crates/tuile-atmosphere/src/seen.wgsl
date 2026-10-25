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
// air before them and hidden by the planet — the nearer body hiding the
// farther (`disc_seen`). `pixel`: the angular size of the pixel the line
// of sight is drawn for, in radians — what a disc's edge is spread over
// and a star's image is as wide as.
fn sky_seen(d: vec3f, pixel: f32) -> vec3f {
    let n = normalize(d);
    let h = atmosphere.sun.w;
    let up = atmosphere.up.xyz;
    // The line of sight as the tables' planet has it: grazing the air the
    // drawn globe's limb holds (`lut_direction`).
    let nl = lut_direction(n, up, h);
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
        let local = vec3f(dot(nl, east), dot(nl, north), dot(nl, up));
        let s = atmosphere.sun.xyz;
        air = sky_radiance(h, local, vec3f(dot(s, east), dot(s, north), dot(s, up)), SUN_ANGULAR_RADIUS);
        if (atmosphere.moon.w > 0.0) {
            let m = atmosphere.moon.xyz;
            air += sky_radiance(h, local, vec3f(dot(m, east), dot(m, north), dot(m, up)),
                atmosphere.moon_disc.w) * atmosphere.moon.w;
        }
        air *= marched;
    }
    if (marched < 1.0) {
        var tabled = lut_bilinear(sky_view_lut, sky_view_uv_of(nl, atmosphere.sun.xyz)).rgb;
        if (atmosphere.moon.w > 0.0) {
            tabled += lut_bilinear(moon_view_lut, sky_view_uv_of(nl, atmosphere.moon.xyz)).rgb
                * atmosphere.moon.w;
        }
        air += tabled * (1.0 - marched);
    }
    var sky = max(air * atmosphere.illuminance.xyz + atmosphere.floor.xyz, atmosphere.least_sky.xyz);
    // Behind the air, unless the planet stands before it.
    let r = LUT_RADIUS + max(h, 0.0);
    if (ground_distance(r, dot(nl, up), max(h, 0.0)) >= 0.0) { return sky; }
    var beyond = vec3f(0.0);
    // The discs, the nearer in front of the farther.
    let sun = disc_seen(n, pixel, body_of(0u));
    let moon = disc_seen(n, pixel, body_of(1u));
    var near = moon;
    var far = sun;
    if (atmosphere.bodies.x < atmosphere.bodies.y) {
        near = sun;
        far = moon;
    }
    beyond += near.radiance + far.radiance * (1.0 - near.cover);
    beyond += stars_seen(n, 0.8 * pixel);
    return sky + beyond * atmosphere.illuminance.xyz;
}

// A body of the sky as its disc is drawn: towards its centre, its drawn
// angular radius (its own times its scale) and its own, its solid angle,
// its light outside the air against the sun's, and its kind — an emitter,
// darkened towards its limb, or a reflector, lit in its phase.
struct Body {
    towards: vec3f,
    rho: f32,
    true_rho: f32,
    solid: f32,
    weight: f32,
    reflector: bool,
}

// The frame's bodies: 0 the sun, 1 the Moon (weight 0 when there is none).
fn body_of(i: u32) -> Body {
    var b: Body;
    if (i == 0u) {
        b.towards = atmosphere.sun.xyz;
        b.true_rho = atmosphere.sun_disc.x;
        b.rho = b.true_rho * max(atmosphere.discs.x, 0.0);
        b.solid = atmosphere.sun_disc.y;
        b.weight = 1.0;
        b.reflector = false;
    } else {
        b.towards = atmosphere.moon.xyz;
        b.true_rho = atmosphere.moon_disc.w;
        b.rho = b.true_rho * max(atmosphere.discs.y, 0.0);
        b.solid = atmosphere.moon_shade.z;
        b.weight = atmosphere.moon.w;
        b.reflector = true;
    }
    return b;
}

// A disc along a line of sight: the radiance it sends the eye there, and
// how much of the pixel it covers — what it leaves of a farther disc.
struct DiscSeen {
    radiance: vec3f,
    cover: f32,
}

// The body's law at the point `p` of its disc (a unit vector's share of
// the disc's radius, from its centre, `mu` the cosine of its surface's
// slope to the eye there), divided by its mean over the disc, so that the
// disc carries its light: an emitter's limb darkening, a reflector's
// Lommel-Seeliger shading μ₀/(μ₀ + μ) — the full Moon about as bright at
// its limb as at its centre — and its night side's earthshine. And the
// law's sum over the disc, per unit of its light: 1, and for a reflector
// with its earthshine a little more.
fn disc_law(b: Body, p: vec3f, mu: f32) -> vec2f {
    if (!b.reflector) {
        let u = atmosphere.sun_disc.z;
        return vec2f((1.0 - u * (1.0 - mu)) / atmosphere.sun_disc.w, 1.0);
    }
    // The surface's normal, towards the eye's side: −towards is the eye.
    let normal = p - b.towards * mu;
    let mu0 = dot(normal, atmosphere.moon_disc.xyz);
    var shade = atmosphere.moon_shade.y;
    if (mu0 > 0.0) { shade += mu0 / (mu0 + mu); }
    let mean = atmosphere.moon_shade.x;
    return vec2f(shade / mean, 1.0 + atmosphere.moon_shade.y / mean);
}

// The disc of `b` along the unit `n` from the frame's eye, in the model's
// units, drawn for a pixel `pixel` radians wide. Each of its points is
// seen through the air along its own line of sight — the lower limb of a
// setting sun redder and dimmer than its upper — and cut by the planet
// where the sky is (`sky_seen` draws nothing behind it). Its edge is the
// share of the pixel it covers, to first order a ramp a pixel wide,
// scaled so the disc keeps its light whatever its size; a disc a few
// pixels across is drawn as a star is, a Gaussian as wide as a star's
// image and the disc's own spread, carrying the disc's light, and the two
// laws meet in a blend on the disc's size against the pixel's — never a
// switch.
fn disc_seen(n: vec3f, pixel: f32, b: Body) -> DiscSeen {
    var out: DiscSeen;
    out.radiance = vec3f(0.0);
    out.cover = 0.0;
    if (b.weight <= 0.0 || b.rho <= 0.0) { return out; }
    let c = dot(n, b.towards);
    if (c <= 0.0) { return out; }
    let off = n - b.towards * c;
    let apart = length(off);
    let sigma = 0.8 * pixel;
    let spread = sqrt(sigma * sigma + 0.25 * b.rho * b.rho);
    if (apart > b.rho + pixel + 5.0 * spread) { return out; }
    // The point of the disc the pixel stands for — past the edge, the point
    // as far inside it: the ramp's light outside the disc is then the light
    // it takes from inside, whatever the law's slope at the limb.
    let x = clamp(min(apart, 2.0 * b.rho - apart) / b.rho, 0.0, 1.0);
    let p = off / max(apart, 1e-30) * x;
    let mu = sqrt(max(1.0 - x * x, 0.0));
    let law = disc_law(b, p, mu);
    let through = transmittance(atmosphere.sun.w, dot(atmosphere.up.xyz, n));
    let cover = clamp((b.rho - apart) / pixel + 0.5, 0.0, 1.0);
    // ∫ cover = π ρ² + π p²/12 for a disc wider than half a pixel.
    let sharp = b.weight * law.x / b.solid * cover
        / (1.0 + pixel * pixel / (12.0 * b.rho * b.rho));
    // Its light — its law's sum, its drawn size against its own — over a
    // Gaussian as wide as a star's image and the disc's own spread.
    let scale = b.rho / b.true_rho;
    let flux = b.weight * law.y * scale * scale;
    let soft = flux * exp(-apart * apart / (2.0 * spread * spread))
        / (2.0 * ATMOSPHERE_PI * spread * spread);
    // A disc sampled once a pixel keeps its light, to a few thousandths,
    // as the Gaussian up to five pixels across and as the ramp from ten:
    // a ramp a pixel wide, sampled at the pixels, is out by a percent or two
    // on a disc a few pixels across, depending on where it falls on them.
    let k = clamp((b.rho / pixel - 2.5) / 2.5, 0.0, 1.0);
    out.radiance = through * mix(soft, sharp, k);
    out.cover = cover;
    return out;
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
