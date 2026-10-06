// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// One frame's camera and look. Eye-relative: the eye is the origin, and
// `right`, `up` and `fwd` span the view frustum (right and up already scaled
// by the half-angle), so a pixel's ray is `fwd + x * right + y * up` for its
// NDC `x`, `y`.
struct Frame {
    view_proj: mat4x4f,
    right: vec4f,
    up: vec4f,
    fwd: vec4f,
    // xyz: world radiance (linear); w: exposure scale.
    world: vec4f,
    // xyz: sun strength — a white surface facing it comes out at this.
    // w: the picture's contrast, a power on luminance about middle grey.
    sun: vec4f,
    // xyz: towards the sun. w: the picture's saturation.
    to_sun: vec4f,
    // x, y: internal (supersampled) size; z: supersampling factor; w: unused.
    size: vec4u,
}

// One resident tile, as this frame places it.
struct TileFrame {
    // Tile origin minus eye, narrowed only after the f64 subtraction.
    offset: vec3f,
    // bit 0: has normals; bit 1: has UVs; bit 2: has a texture.
    flags: u32,
    factor: vec4f,
}

const HAS_NORMALS: u32 = 1u;
const HAS_UVS: u32 = 2u;
const HAS_TEXTURE: u32 = 4u;
