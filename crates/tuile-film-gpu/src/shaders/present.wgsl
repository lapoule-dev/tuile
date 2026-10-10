// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Supersampled HDR → display pixels: box filter over the k×k block, sky where
// no tile was drawn, exposure, then the sRGB curve ("Standard" view: no tone
// mapping, a straight clip).
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
    for (var j = 0u; j < k; j++) {
        for (var i = 0u; i < k; i++) {
            let s = id.xy * k + vec2u(i, j);
            if (textureLoad(vis, s, 0).x == 0u) {
                sum += frame.world.xyz;
            } else {
                sum += textureLoad(hdr, s, 0).rgb;
            }
            if (overlaid) { over += textureLoad(overlay, s, 0); }
        }
    }
    var linear = sum / f32(k * k) * frame.world.w;
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
        let bent = KNEE + (1.0 - KNEE) * tanh((lit - KNEE) / (1.0 - KNEE));
        linear *= bent / lit;
    }
    if (overlaid) {
        // Over what the display would show: the picture is clipped first,
        // as the curve below clips it anyway, so a pixel no overlay touches
        // is the pixel it was.
        let o = over / f32(k * k);
        linear = clamp(linear, vec3f(0.0), vec3f(1.0)) * (1.0 - o.a) + o.rgb;
    }
    textureStore(out, id.xy, vec4f(oetf(linear), 1.0));
}
