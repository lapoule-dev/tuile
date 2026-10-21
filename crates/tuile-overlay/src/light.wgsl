// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// How an overlay's triangles are lit, shared by every renderer that draws
// them: the film's and the viewer's alike.
//
// This is text, not a module of its own: a renderer concatenates it into
// its own shader, where `OverlayVertex`'s attributes are bound, and calls
// `overlay_lit` from its fragment stage. Nothing here touches a uniform, a
// texture or a binding, so the film's `Frame` and a viewer's view uniform
// can live side by side above it and the light is the same under both.
//
// It exists because the HUD has to come out of the viewer exactly as it
// comes out of the film. Two copies of this arithmetic would agree on the
// day they were written and drift the week after.

// What a vertex's mesh asks for: `OverlayVertex::flags`.
const OVERLAY_LIT: u32 = 1u;
const OVERLAY_RECEIVES: u32 = 2u;
const OVERLAY_BEHIND_GROUND: u32 = 4u;

// How much of a lit shape's colour it keeps facing away from the light, and
// how strong and how tight its highlight is.
const OVERLAY_AMBIENT: f32 = 0.45;
const OVERLAY_HIGHLIGHT: f32 = 0.2;
const OVERLAY_SHININESS: f32 = 24.0;

// `color` lit for a shape at `at` (eye-relative metres) whose normal is
// `normal` (zero for none), seen by a camera whose own axes are `right` and
// `up`. Colours are display-linear and premultiplied, so the highlight is
// scaled by the alpha the colour already carries.
//
// A light that goes with the camera: above it, a little to its left, mostly
// from behind it. Either face is lit as the face the eye sees, so a ribbon
// seen from its back is not black.
fn overlay_lit(color: vec4f, normal: vec3f, at: vec3f, right: vec3f, up: vec3f) -> vec4f {
    if (dot(normal, normal) <= 0.0) {
        return color;
    }
    let r = normalize(right);
    let u = normalize(up);
    let back = cross(r, u);
    let light = normalize(-0.35 * r + 0.55 * u + 0.75 * back);
    let towards_eye = normalize(-at);
    var n = normalize(normal);
    if (dot(n, towards_eye) < 0.0) {
        n = -n;
    }
    let diffuse = max(dot(n, light), 0.0);
    let half_way = normalize(light + towards_eye);
    let shine = select(0.0, pow(max(dot(n, half_way), 0.0), OVERLAY_SHININESS), diffuse > 0.0);
    return vec4f(
        color.rgb * (OVERLAY_AMBIENT + (1.0 - OVERLAY_AMBIENT) * diffuse)
            + OVERLAY_HIGHLIGHT * shine * color.a,
        color.a,
    );
}
