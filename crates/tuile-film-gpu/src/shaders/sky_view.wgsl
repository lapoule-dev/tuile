// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The sky-view tables of one frame (Hillaire 2020 §5.3): the sky seen from
// the frame's eye in every direction, by its azimuth from the light and its
// zenith angle, the rows bunched about the horizon — one under the sun, one
// under the Moon, each per unit of its own light. Made once a frame, read
// by `present` for every sky pixel of both eyes. Their texels are
// `hillaire::sky_view_texel_direction`'s, the CPU twin they are tested
// against.

@compute @workgroup_size(8, 8)
fn sky_view(@builtin(global_invocation_id) id: vec3u) {
    let size = vec2u(SKY_VIEW_SIZE);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let height = atmosphere.sun.w;
    let unit = vec2f(id.xy) / (SKY_VIEW_SIZE - vec2f(1.0));
    let seen = sky_view_direction(unit, height);
    let mu = seen.x;
    let c = seen.y;
    let s = sqrt(max(1.0 - mu * mu, 0.0));
    let dir = vec3f(s * c, s * sqrt(max(1.0 - c * c, 0.0)), mu);
    // Each light in the table's frame: z up, x towards its azimuth.
    let mu_s = dot(atmosphere.sun.xyz, atmosphere.up.xyz);
    let sun = vec3f(sqrt(max(1.0 - mu_s * mu_s, 0.0)), 0.0, mu_s);
    textureStore(sky_view_out, id.xy, vec4f(sky_radiance(height, dir, sun), 1.0));
    var moonlit = vec3f(0.0);
    if (atmosphere.moon.w > 0.0) {
        let mu_m = dot(atmosphere.moon.xyz, atmosphere.up.xyz);
        moonlit = sky_radiance(height, dir, vec3f(sqrt(max(1.0 - mu_m * mu_m, 0.0)), 0.0, mu_m));
    }
    textureStore(moon_view_out, id.xy, vec4f(moonlit, 1.0));
}
