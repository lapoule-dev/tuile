// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Supersampled HDR → display pixels: box filter over the k×k block, sky where
// no tile was drawn, exposure, then the sRGB curve ("Standard" view: no tone
// mapping, a straight clip). The sky is the dome's one colour, or, for a
// look that has air, runs from the horizon's colour to the zenith's — or,
// under a physical atmosphere, is read from the frame's sky-view table.
//
// A host's overlays, when the frame carries any, are laid over the picture
// between the two: after its tone — they are display-linear, and no
// exposure or grade is theirs — and before the curve, filtered by the same
// box, so their edges are smoothed as the ground's are.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var hdr: texture_2d<f32>;
@group(0) @binding(2) var vis: texture_2d<u32>;
@group(0) @binding(3) var out: texture_storage_2d<rgba8unorm, write>;
// Premultiplied, at the supersampled size; read only when `frame.size.w`
// says the frame drew into it.
@group(0) @binding(4) var overlay: texture_2d<f32>;

fn oetf(c: vec3f) -> vec3f {
    let x = clamp(c, vec3f(0.0), vec3f(1.0));
    return select(1.055 * pow(x, vec3f(1.0 / 2.4)) - 0.055, 12.92 * x, x <= vec3f(0.0031308));
}

// The physical sky along `d` from the frame's eye: what the air sends
// under the sun and under the Moon — from the frame's sky-view tables in
// the air, marched along the ray itself from above it, where the air is a
// band a few degrees thick along the limb that no table row could hold —
// what the night floor adds, and behind the air the sun's disc, darkened
// towards its limb, the Moon's in its phase and the stars, dimmed by the
// air before them and hidden by the planet.
fn sky_seen(d: vec3f) -> vec3f {
    let n = normalize(d);
    let h = atmosphere.sun.w;
    let up = atmosphere.up.xyz;
    var air = vec3f(0.0);
    if (h > atmosphere.shape.y) {
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
    } else {
        air = lut_bilinear(sky_view_lut, sky_view_uv_of(d, atmosphere.sun.xyz)).rgb;
        if (atmosphere.moon.w > 0.0) {
            air += lut_bilinear(moon_view_lut, sky_view_uv_of(d, atmosphere.moon.xyz)).rgb
                * atmosphere.moon.w;
        }
    }
    var sky = max(air * atmosphere.illuminance.xyz + atmosphere.floor.xyz, atmosphere.least_sky.xyz);
    // Behind the air, unless the planet stands before it.
    let r = LUT_RADIUS + max(h, 0.0);
    if (ground_distance(r, dot(n, up), max(h, 0.0)) >= 0.0) { return sky; }
    var beyond = vec3f(0.0);
    let apart = acos(clamp(dot(n, atmosphere.sun.xyz), -1.0, 1.0));
    if (apart < SUN_ANGULAR_RADIUS) {
        let x = apart / SUN_ANGULAR_RADIUS;
        let limb = 1.0 - 0.6 * (1.0 - sqrt(max(1.0 - x * x, 0.0)));
        let solid = 2.0 * ATMOSPHERE_PI * (1.0 - cos(SUN_ANGULAR_RADIUS));
        let mu_s = dot(up, atmosphere.sun.xyz);
        beyond += sunlight(h, mu_s) * limb / solid;
    }
    beyond += moon_seen(n);
    // A star's image about a pixel wide: the pixel's angle, supersampled.
    let pixel = 2.0 * length(frame.up.xyz) / f32(frame.size.y);
    beyond += stars_seen(n, 0.8 * pixel);
    return sky + beyond * atmosphere.illuminance.xyz;
}

// Where the roll-off of highlights begins, in linear luminance.
const KNEE: f32 = 0.5;

@compute @workgroup_size(8, 8)
fn present(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(out);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let k = frame.size.z;
    let overlaid = frame.size.w != 0u;
    var sum = vec3f(0.0);
    var over = vec4f(0.0);
    // The probe of holes: a sample no tile covers whose ray meets the Earth
    // is not sky.
    var hole = false;
    for (var j = 0u; j < k; j++) {
        for (var i = 0u; i < k; i++) {
            let s = id.xy * k + vec2u(i, j);
            if (textureLoad(vis, s, 0).x == 0u) {
                if (frame.local_up.w != 0.0) {
                    let ndc = (vec2f(s) + 0.5) / vec2f(frame.size.xy) * vec2f(2.0, -2.0)
                        + vec2f(-1.0, 1.0);
                    let d = frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz;
                    // A sphere well under any ground, 6 350 km: the eye is
                    // `r` from its centre.
                    let r = 0.5 / frame.haze.z;
                    let towards = r * dot(frame.local_up.xyz, d);
                    let inside = towards * towards - (r - 6.35e6) * (r + 6.35e6) * dot(d, d);
                    hole = hole || (towards < 0.0 && inside > 0.0);
                }
                if (atmosphere.shape.w != 0.0) {
                    let ndc = (vec2f(s) + 0.5) / vec2f(frame.size.xy) * vec2f(2.0, -2.0)
                        + vec2f(-1.0, 1.0);
                    sum += sky_seen(frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz);
                } else if (frame.haze.w != 0.0) {
                    // The sample's own ray, as the resolve builds it.
                    let ndc = (vec2f(s) + 0.5) / vec2f(frame.size.xy) * vec2f(2.0, -2.0)
                        + vec2f(-1.0, 1.0);
                    sum += sky(frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz);
                } else {
                    sum += frame.world.xyz;
                }
            } else {
                let shaded = textureLoad(hdr, s, 0);
                // The probe: a sample the resolve marked as seen through a
                // gap, or never wrote.
                hole = hole || (frame.local_up.w != 0.0 && shaded.a != 1.0);
                sum += shaded.rgb;
            }
            if (overlaid) { over += textureLoad(overlay, s, 0); }
        }
    }
    var linear = sum / f32(k * k) * frame.world.w * frame.white_balance.xyz;
    // The picture's own contrast and saturation: one setting for every
    // film, whatever its imagery. Luminance is raised to a power about
    // middle grey, colours keeping their ratios, then what is not
    // luminance is scaled.
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    if (frame.sun.w != 1.0) {
        linear *= pow(max(dot(linear, luma), 1e-5) / 0.18, frame.sun.w - 1.0);
    }
    if (frame.to_sun.w != 1.0) {
        let y = dot(linear, luma);
        linear = max(vec3f(y) + (linear - vec3f(y)) * frame.to_sun.w, vec3f(0.0));
    }
    // Highlights are rolled off, not cut: above the knee luminance bends
    // towards white and never reaches past it, colours keeping their
    // ratios. A picture pushed bright keeps what its bright ground showed,
    // where a straight clip turned towns and beaches to flat white.
    let lit = dot(linear, luma);
    if (lit > KNEE) {
        // tanh of a large number is 1, but written as a ratio of two
        // exponentials some drivers make it ∞/∞: a sun's disc, thousands of
        // times white, came out black. Past 10 it is 1 to the last bit.
        let bent = KNEE + (1.0 - KNEE) * tanh(min((lit - KNEE) / (1.0 - KNEE), 10.0));
        linear *= bent / lit;
    }
    if (overlaid) {
        // Over what the display would show: the picture is clipped first,
        // as the curve below clips it anyway, so a pixel no overlay touches
        // is the pixel it was.
        let o = over / f32(k * k);
        linear = clamp(linear, vec3f(0.0), vec3f(1.0)) * (1.0 - o.a) + o.rgb;
    }
    if (hole) { linear = vec3f(1.0, 0.0, 1.0); }
    textureStore(out, id.xy, vec4f(oetf(linear), 1.0));
}
