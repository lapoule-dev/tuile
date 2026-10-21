// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// A host's shapes: coloured triangles through the frame's own camera, into
// a target of their own at the supersampled size. Their vertices come in
// eye-relative already — the origin of each mesh met the eye in f64 before
// anything was narrowed — and their colours display-linear and
// premultiplied, which is how they are blended here and laid over the
// picture by `present`.
//
// The depth attachment is the overlays' own: what opaque overlays wrote,
// and nothing of the ground. A shape tested against the ground reads the
// ground's depth here, at its own sample, and is dropped where the ground
// is nearer — the comparison the attachment would have made.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var ground: texture_depth_2d;
@group(0) @binding(2) var shadow_map: texture_depth_2d_array;
@group(0) @binding(3) var shadow_samp: sampler_comparison;

// The flags a vertex carries and the light a lit mesh is given come from
// `tuile_overlay::gpu::LIGHT_WGSL`, concatenated ahead of this: the viewer
// draws the same meshes under the same light, and the film's HUD has to
// come out of both alike.

struct Out {
    @builtin(position) clip: vec4f,
    @location(0) color: vec4f,
    @location(1) normal: vec3f,
    @location(2) at: vec3f,
    @location(3) @interpolate(flat) flags: u32,
}

@vertex
fn vs(
    @location(0) position: vec3f,
    @location(1) color: vec4f,
    @location(2) normal: vec3f,
    @location(3) flags: u32,
) -> Out {
    var out: Out;
    out.clip = frame.view_proj * vec4f(position, 1.0);
    out.color = color;
    out.normal = normal;
    out.at = position;
    out.flags = flags;
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4f {
    if ((in.flags & OVERLAY_BEHIND_GROUND) != 0u) {
        // Reverse-Z: nearer is greater.
        let ground_at = textureLoad(ground, vec2i(in.clip.xy), 0);
        if (in.clip.z <= ground_at) {
            discard;
        }
    }
    var color = in.color;
    // The camera's own axes: the same for both eyes, which share a
    // rotation, so a shape is lit alike in each.
    if ((in.flags & OVERLAY_LIT) != 0u) {
        color = overlay_lit(color, in.normal, in.at, frame.right.xyz, frame.up.xyz);
    }
    if ((in.flags & OVERLAY_RECEIVES) != 0u && frame.shadow.x > 0.0) {
        // Looked up from a little towards the sun, as ground is from off
        // its surface: a shape does not shade itself where it stands.
        let sun = sunlit(in.at, frame.to_sun.xyz, vec2u(in.clip.xy));
        // A shadow takes the sun and leaves the dome: what is shaded keeps
        // the share of its light the dome gives ground, as ground does.
        let luma = vec3f(0.2126, 0.7152, 0.0722);
        let dome = dot(frame.world.xyz, luma);
        let whole = dome + dot(frame.sun.xyz, luma);
        let left = select(1.0, dome / whole, whole > 0.0);
        let k = 1.0 - frame.shadow.x * (1.0 - sun) * (1.0 - left);
        color = vec4f(color.rgb * k, color.a);
    }
    return color;
}

// The sun's map, as an overlay that casts is drawn into it: depth alone,
// through the sun's view, laid on the near plane when nearer the sun than
// it, as the ground is.
// Which of the sun's maps this pass draws.
struct SunMapPass { map: u32 }
@group(1) @binding(0) var<uniform> sun_map_pass: SunMapPass;

fn sun_clip(position: vec3f) -> vec4f {
    var clip = frame.sun_view_proj * vec4f(position, 1.0);
    clip = vec4f(clip.xy + sun_map_shift(sun_map_pass.map, frame.size.z, frame.shadow.z) * clip.w, clip.zw);
    clip.z = max(clip.z, 0.0);
    return clip;
}

@vertex
fn into_sun_map(@location(0) position: vec3f) -> @builtin(position) vec4f {
    return sun_clip(position);
}

// A caster's edges, as `into_sun_map`, each pushed `sunk` metres further
// from the sun: an edge lies on its own faces, and no raster bias can be
// asked of a line.
@vertex
fn edge_into_sun_map(@location(0) position: vec3f, @location(4) sunk: f32) -> @builtin(position) vec4f {
    return sun_clip(position - normalize(frame.to_sun.xyz) * sunk);
}
