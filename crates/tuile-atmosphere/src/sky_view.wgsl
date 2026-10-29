// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The sky-view tables of one frame (Hillaire 2020 §5.3): the sky seen from
// the frame's eye in every direction, by its azimuth from the light and its
// zenith angle, the rows bunched about the horizon — one under the sun, one
// under the Moon, each per unit of its own light. Made once for a frame's
// light, read for every sky pixel. Their texels are
// `hillaire::sky_view_texel_direction`'s, the CPU twin they are tested
// against.
//
// One texel's sum, for whichever pass makes the table: a compute pass that
// stores it (`sky_view_compute.wgsl`, a film's), or a render pass that draws
// it (`sky_view_render.wgsl`, for a device that has no compute, WebGL2).
// Included after `hillaire.wgsl`.

struct SkyViewTexel {
    sun: vec3f,
    moon: vec3f,
}

fn sky_view_texel(xy: vec2u) -> SkyViewTexel {
    let height = atmosphere.sun.w;
    let unit = vec2f(xy) / (SKY_VIEW_SIZE - vec2f(1.0));
    let seen = sky_view_direction(unit, height);
    let mu = seen.x;
    let c = seen.y;
    let s = sqrt(max(1.0 - mu * mu, 0.0));
    let dir = vec3f(s * c, s * sqrt(max(1.0 - c * c, 0.0)), mu);
    // Each light in the table's frame: z up, x towards its azimuth.
    let mu_s = dot(atmosphere.sun.xyz, atmosphere.up.xyz);
    let sun = vec3f(sqrt(max(1.0 - mu_s * mu_s, 0.0)), 0.0, mu_s);
    var texel: SkyViewTexel;
    texel.sun = sky_radiance(height, dir, sun, SUN_ANGULAR_RADIUS);
    texel.moon = vec3f(0.0);
    if (atmosphere.moon.w > 0.0) {
        let mu_m = dot(atmosphere.moon.xyz, atmosphere.up.xyz);
        texel.moon = sky_radiance(height, dir, vec3f(sqrt(max(1.0 - mu_m * mu_m, 0.0)), 0.0, mu_m),
            atmosphere.moon_disc.w);
    }
    return texel;
}
