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

// The physical sky is `sky_seen` (`tuile_atmosphere`'s `seen.wgsl`, the
// viewer's too), drawn for a supersample: its angle, what a disc's edge is
// spread over and a star's image is as wide as.
fn sample_angle() -> f32 {
    return 2.0 * length(frame.up.xyz) / f32(frame.size.y);
}

// Where the unit direction `s` from the eye falls on the supersampled
// picture, in its pixels; `vec2f(-1.0)` when behind the camera.
fn on_picture(s: vec3f) -> vec2f {
    let r = frame.right.xyz;
    let u = frame.up.xyz;
    let f = frame.fwd.xyz;
    let rr = dot(r, r);
    let uu = dot(u, u);
    let axis = normalize(f - r * dot(f, r) / rr - u * dot(f, u) / uu);
    let depth = dot(s, axis);
    if (depth <= 1e-4) { return vec2f(-1.0); }
    let ndc = vec2f(dot(s, r) / (rr * depth) - dot(f, r) / rr, dot(s, u) / (uu * depth) - dot(f, u) / uu);
    return (vec2f(ndc.x, -ndc.y) * 0.5 + 0.5) * vec2f(frame.size.xy);
}

// Whether ground stands before the direction `s` in this picture: its
// pixel drew a tile. Off the picture, nothing is known to.
fn hidden(s: vec3f) -> bool {
    let at = on_picture(s);
    let size = vec2f(frame.size.xy);
    if (any(at < vec2f(0.0)) || any(at >= size)) { return false; }
    return textureLoad(vis, vec2u(at), 0).x != 0u;
}

// The camera's glare about the sun's disc and the Moon's along `d`, in
// the renderer's units: their light, spread by the lens.
fn glare(d: vec3f) -> vec3f {
    if (atmosphere.shape.w == 0.0 || atmosphere.discs.z == 0.0) { return vec3f(0.0); }
    let n = normalize(d);
    let right = normalize(frame.right.xyz);
    let up = normalize(frame.up.xyz);
    let h = atmosphere.sun.w;
    var light = vec3f(0.0);
    let s = atmosphere.sun.xyz;
    if (!hidden(s)) {
        light += glare_of(n, s, sunlight(h, dot(atmosphere.up.xyz, s)), right, up);
    }
    let m = atmosphere.moon.xyz;
    if (atmosphere.moon.w > 0.0 && !hidden(m)) {
        // A fifth of the sun's: the Moon's disc, exposed for the night it
        // lights, keeps its phase readable under its own glow.
        light += glare_of(n, m, body_light(h, dot(atmosphere.up.xyz, m), atmosphere.moon_disc.w)
            * atmosphere.moon.w, right, up) * 0.2;
    }
    return light * atmosphere.illuminance.xyz;
}

// The sky a supersample `s` no tile covers shows, in the renderer's units:
// under a physical atmosphere `sky_seen` along its own ray, else the
// look's air or its dome.
fn sky_sample(s: vec2u) -> vec3f {
    if (atmosphere.shape.w != 0.0) {
        let ndc = (vec2f(s) + 0.5) / vec2f(frame.size.xy) * vec2f(2.0, -2.0)
            + vec2f(-1.0, 1.0);
        return sky_seen(frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz, sample_angle());
    } else if (frame.haze.w != 0.0) {
        // The sample's own ray, as the resolve builds it.
        let ndc = (vec2f(s) + 0.5) / vec2f(frame.size.xy) * vec2f(2.0, -2.0)
            + vec2f(-1.0, 1.0);
        return sky(frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz);
    }
    return frame.world.xyz;
}

// Where the roll-off of highlights begins, in linear luminance: the
// frame's `white_balance.w` (0.5 by default, `Look::highlight_knee`).

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
                sum += sky_sample(s);
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
    var linear = sum / f32(k * k);
    {
        // The lens's glare, at the pixel's centre.
        let ndc = (vec2f(id.xy) + 0.5) / vec2f(size.xy) * vec2f(2.0, -2.0) + vec2f(-1.0, 1.0);
        linear += glare(frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz);
    }
    linear *= frame.world.w * frame.white_balance.xyz;
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
    // Highlights are rolled off, not cut (`roll_off`).
    linear = roll_off(linear, frame.white_balance.w);
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

// The picture's light as `present` averages it over each pixel's samples,
// before the lens's glare, the exposure and the grade: the renderer's
// units, pre-exposed. For an instrument (`FilmGpu::write_linear`) — what a
// disc's light is measured on — never for the picture.
@group(2) @binding(0) var linear_out: texture_storage_2d<rgba32float, write>;

@compute @workgroup_size(8, 8)
fn linear(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(linear_out);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let k = frame.size.z;
    var sum = vec3f(0.0);
    for (var j = 0u; j < k; j++) {
        for (var i = 0u; i < k; i++) {
            let s = id.xy * k + vec2u(i, j);
            if (textureLoad(vis, s, 0).x == 0u) {
                sum += sky_sample(s);
            } else {
                sum += textureLoad(hdr, s, 0).rgb;
            }
        }
    }
    textureStore(linear_out, id.xy, vec4f(sum / f32(k * k), 1.0));
}
