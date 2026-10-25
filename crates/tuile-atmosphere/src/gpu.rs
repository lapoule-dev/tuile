// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere on a GPU (feature `wgpu`): Hillaire's constant
//! tables, computed on the CPU ([`LutAtmosphere`]) and uploaded; the
//! sky-view tables, made on the GPU for a frame's light; and the uniform
//! every shader reads them by.
//!
//! Shared by every wgpu renderer of this air — a film's compute passes and
//! an interactive viewer's fragments — with the WGSL they draw it with
//! ([`SKY_VIEW_WGSL`], [`SEEN_WGSL`]): one implementation, so the two cannot
//! quietly disagree about the colour of the sky.
//!
//! Everything here is a frame's, not an eye's: the light, the tables and
//! the sky-view are worked out once, from the frame's own camera, and both
//! eyes of a stereo frame read the same. Only where the air's sphere stands
//! from the eye is restated for each picture.
//!
//! The light is the moment's, but it is not the camera's alone: the sun and
//! the Moon are one direction each, ECEF, and every ground point and every
//! line of sight is lit by them where it is — its own height, its own up —
//! so that one frame holds a day side and a night side with the terminator
//! between them. What is the frame's is the exposure, the sky's harmonic
//! set against each point's own level light, and the sky-view seen from
//! the eye.

use crate::hillaire::{IRRADIANCE_SIZE, MULTI_SCATTERING_SIZE, SKY_VIEW_SIZE, TRANSMITTANCE_SIZE};
use crate::{AtmosphereParams, LutAtmosphere, SkyLight, SkyLook};
use glam::DVec3;

/// The sky-view tables' texel sums (`sky_view_texel`), after
/// [`crate::hillaire::WGSL`].
pub const SKY_VIEW_WGSL: &str = include_str!("sky_view.wgsl");
/// The sky-view tables made in compute, after [`SKY_VIEW_WGSL`].
pub const SKY_VIEW_COMPUTE_WGSL: &str = include_str!("sky_view_compute.wgsl");
/// The sky-view tables drawn, after [`SKY_VIEW_WGSL`].
pub const SKY_VIEW_RENDER_WGSL: &str = include_str!("sky_view_render.wgsl");
/// The sky along a line of sight and a ground point through the air
/// (`sky_seen`, `ground_seen`), and the display's grade (`roll_off`,
/// `graded`), after [`crate::hillaire::WGSL`].
pub const SEEN_WGSL: &str = include_str!("seen.wgsl");

/// `Atmosphere` in `hillaire.wgsl`, byte for byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct AtmosphereUniform {
    pub centre: [f32; 4],
    pub rayleigh: [f32; 4],
    pub mie: [f32; 4],
    pub ozone: [f32; 4],
    pub shape: [f32; 4],
    pub sun: [f32; 4],
    pub up: [f32; 4],
    pub illuminance: [f32; 4],
    pub floor: [f32; 4],
    pub sh_x: [f32; 4],
    pub sh_y: [f32; 4],
    pub sh_z: [f32; 4],
    pub moon: [f32; 4],
    pub moon_disc: [f32; 4],
    pub moon_shade: [f32; 4],
    pub ground: [f32; 4],
    pub added: [f32; 4],
    pub least: [f32; 4],
    pub least_sky: [f32; 4],
    pub level_ref: [f32; 4],
    /// Rows of the turn from the planet's axes to the star catalogue's
    /// (J2000); w: nothing.
    pub stars_x: [f32; 4],
    pub stars_y: [f32; 4],
    pub stars_z: [f32; 4],
    /// xyz: the Earth's centre from the eye; w: 1 when there are night
    /// lights.
    pub earth: [f32; 4],
    /// x: the sun's disc's scale; y: the Moon's; z: the glare's strength.
    pub discs: [f32; 4],
    /// x: the veil's bound ([`SkyLook::veil_bound`]); y, z: the march's
    /// steps under 10 km and from over it ([`SkyLook::march_steps`]); w:
    /// nothing.
    pub tuning: [f32; 4],
    /// xyz: the eye from the origin the renderer's positions are relative
    /// to: nothing for a film, whose positions are the eye's.
    pub eye: [f32; 4],
    /// xyz: the sky's light averaged over every facing, pre-exposed.
    pub ambient: [f32; 4],
    /// xyz: the white balance; w: the highlight knee.
    pub grade: [f32; 4],
    /// The planets, Mercury to Uranus: xyz towards each, ECEF; w its
    /// irradiance over the sun's — 0 for none.
    pub planets: [[f32; 4]; 6],
    /// Their colours, in units of the sun's (rgb).
    pub planet_colours: [[f32; 4]; 6],
    /// The sun's disc: x its true angular radius, y its solid angle — both
    /// worked out in f64 here, never folded by a shader — z its linear limb
    /// darkening's coefficient, w that law's mean over the disc.
    pub sun_disc: [f32; 4],
    /// x: the sun's distance from the eye, y: the Moon's — metres, 0 for
    /// none — what orders the discs near to far; zw: nothing.
    pub bodies: [f32; 4],
}

/// The sun's linear limb darkening, `1 − u (1 − μ)`: grey, `u = 0.6`.
pub const SUN_LIMB_DARKENING: f64 = 0.6;

/// The solid angle of a disc of angular radius `rho`, radians: `2π (1 −
/// cos ρ)`, written as `4π sin²(ρ/2)` — the same in f64, and a disc a
/// quarter of a degree across keeps its digits.
pub fn disc_solid_angle(rho: f64) -> f64 {
    let s = (0.5 * rho).sin();
    4.0 * std::f64::consts::PI * s * s
}

/// The share of the full Moon's light its night side sends back by the
/// Earth's, the Earth full in its sky: earthshine is some ten thousand
/// times fainter than the lit crescent beside it.
pub const EARTHSHINE: f32 = 1.5e-4;

impl AtmosphereUniform {
    /// `params` as the shaders read them, lit by `light`, from the frame's
    /// eye at `frame_eye` (ECEF), every light pre-exposed by `exposure` —
    /// so that the HDR picture holds a night in half floats — with the
    /// stars turned by `stars`, from the planet's axes to the catalogue's
    /// (J2000): the transpose of [`crate::stars::j2000_to_ecef`]. Stars are
    /// always drawn; whether they show is the air's and the camera's.
    pub fn new(
        params: &AtmosphereParams,
        light: &SkyLight,
        frame_eye: DVec3,
        exposure: f32,
        stars: glam::DMat3,
        look: &SkyLook,
    ) -> Self {
        let i = &light.illumination;
        let sun = i.to_sun_ecef.as_vec3();
        let up = light.frame.up(frame_eye).as_vec3();
        let k = exposure;
        let sh = |n: usize| {
            [
                i.sky_sh[n][0] * k,
                i.sky_sh[n][1] * k,
                i.sky_sh[n][2] * k,
                0.0,
            ]
        };
        let rgb = |v: [f32; 3]| [v[0] * k, v[1] * k, v[2] * k, 0.0];
        let (moon, moon_disc, moon_shade) = match i.moon {
            Some(m) => {
                let to_moon = m.to_moon_ecef.normalize_or_zero();
                // From the Moon towards the sun: the phase angle away from
                // the eye, in the plane of the eye, the Moon and the sun.
                let towards_eye = -to_moon;
                let side = (i.to_sun_ecef - towards_eye * i.to_sun_ecef.dot(towards_eye))
                    .try_normalize()
                    .unwrap_or_else(|| towards_eye.any_orthonormal_vector());
                let a = f64::from(m.phase_angle);
                let lit = (towards_eye * a.cos() + side * a.sin()).as_vec3();
                let t = to_moon.as_vec3();
                (
                    [t.x, t.y, t.z, light.moon_strength as f32],
                    [lit.x, lit.y, lit.z, m.angular_radius],
                    [
                        crate::moon::disc_shading_mean(a) as f32,
                        EARTHSHINE * 0.5 * (1.0 - a.cos() as f32),
                        disc_solid_angle(f64::from(m.angular_radius)) as f32,
                        0.0,
                    ],
                )
            }
            None => ([0.0; 4], [0.0; 4], [1.0, 0.0, 0.0, 0.0]),
        };
        let moon_distance = i.moon.map_or(0.0, |m| {
            crate::moon::MOON_RADIUS / f64::from(m.angular_radius).sin().max(1e-9)
        });
        let star_rows = stars.transpose();
        let row = |n: usize| {
            let c = star_rows.col(n).as_vec3();
            [c.x, c.y, c.z, 0.0]
        };
        let p = params;
        Self {
            centre: [0.0, 0.0, 0.0, light.frame.radius as f32],
            rayleigh: [
                p.rayleigh_scattering[0] as f32,
                p.rayleigh_scattering[1] as f32,
                p.rayleigh_scattering[2] as f32,
                (1.0 / p.rayleigh_scale_height) as f32,
            ],
            mie: [
                p.mie_scattering as f32,
                (p.mie_scattering + p.mie_absorption) as f32,
                (1.0 / p.mie_scale_height) as f32,
                p.mie_anisotropy as f32,
            ],
            ozone: [
                p.ozone_absorption[0] as f32,
                p.ozone_absorption[1] as f32,
                p.ozone_absorption[2] as f32,
                p.ozone_centre as f32,
            ],
            shape: [
                p.ozone_half_width as f32,
                p.thickness as f32,
                p.ground_albedo as f32,
                1.0,
            ],
            sun: [
                sun.x,
                sun.y,
                sun.z,
                light.frame.height(frame_eye).max(0.0) as f32,
            ],
            up: [up.x, up.y, up.z, 0.0],
            illuminance: rgb(light.illuminance),
            floor: rgb(light.floor),
            sh_x: sh(1),
            sh_y: sh(2),
            sh_z: sh(3),
            moon,
            moon_disc,
            moon_shade,
            ground: rgb(light.ground),
            added: rgb(light.added),
            least: rgb(light.least),
            // The sky's least: as much over the floor's sky as the ground's
            // least is over the floor's light — space and a night side seen
            // beside the day are dark, not black.
            least_sky: rgb(
                [0, 1, 2].map(|c| light.floor[c] * light.least[c] / light.added[c].max(1e-30))
            ),
            level_ref: rgb(light.level_reference),
            stars_x: row(0),
            stars_y: row(1),
            stars_z: row(2),
            earth: [0.0; 4],
            discs: [
                look.sun_scale.max(0.0),
                look.moon_scale.max(0.0),
                look.glare.max(0.0),
                0.0,
            ],
            tuning: [
                look.veil_bound,
                look.march_steps[0] as f32,
                look.march_steps[1] as f32,
                0.0,
            ],
            eye: [0.0; 4],
            ambient: sh(0),
            grade: {
                let w = look.white_balance_under(light);
                [w[0], w[1], w[2], look.highlight_knee]
            },
            planets: [[0.0; 4]; 6],
            planet_colours: [[0.0; 4]; 6],
            sun_disc: [
                crate::hillaire::SUN_ANGULAR_RADIUS as f32,
                disc_solid_angle(crate::hillaire::SUN_ANGULAR_RADIUS) as f32,
                SUN_LIMB_DARKENING as f32,
                (1.0 - SUN_LIMB_DARKENING / 3.0) as f32,
            ],
            bodies: [
                crate::moon::ASTRONOMICAL_UNIT as f32,
                moon_distance as f32,
                0.0,
                0.0,
            ],
        }
    }

    /// With the planets `lights` ([`crate::planets::lights_at`]): drawn as
    /// the stars are, points of light through the air.
    pub fn with_planets(mut self, lights: &[([f32; 4], [f32; 4]); 6]) -> Self {
        for (i, (at, colour)) in lights.iter().enumerate() {
            self.planets[i] = *at;
            self.planet_colours[i] = *colour;
        }
        self
    }

    /// For the picture whose eye is at `eye` (ECEF): the sphere's centre
    /// from it, the subtraction in f64.
    pub fn seen_from(mut self, light: &SkyLight, eye: DVec3) -> Self {
        let centre = (light.frame.centre - eye).as_vec3();
        self.centre = [centre.x, centre.y, centre.z, self.centre[3]];
        let earth = (-eye).as_vec3();
        self.earth = [earth.x, earth.y, earth.z, self.earth[3]];
        self
    }

    /// With the renderer's positions relative to an origin `eye_from_origin`
    /// short of the eye (narrowed after an f64 subtraction): a viewer's
    /// tiles are rebased onto an origin of their own, a film's onto the eye.
    pub fn with_eye_from_origin(mut self, eye_from_origin: glam::Vec3) -> Self {
        self.eye = eye_from_origin.extend(0.0).to_array();
        self
    }
}

/// How the sky-view tables are made: in compute, or drawn by a render
/// pass for a device that has no compute (WebGL2). The same texel sums
/// either way (`sky_view_texel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkyViewPass {
    Compute,
    Render,
}

/// The tables on the GPU, the uniform, and the pass that makes the sky-view.
pub struct AtmosphereGpu {
    pub uniform: wgpu::Buffer,
    pub transmittance: wgpu::Texture,
    pub multi_scattering: wgpu::Texture,
    pub irradiance: wgpu::Texture,
    pub sky_view: wgpu::Texture,
    pub moon_view: wgpu::Texture,
    /// The atmosphere the tables hold, and a clear day's — the reference
    /// every light is calibrated against.
    pub model: Option<LutAtmosphere>,
    pub reference: Option<LutAtmosphere>,
    /// The frame's light, and its uniform before it is placed for an eye.
    pub light: Option<SkyLight>,
    pub frame_uniform: AtmosphereUniform,
    /// Whether the sky-view table is to be made again before the next
    /// picture is drawn.
    pub sky_view_stale: bool,
    /// Everything bound, against the layout [`Self::new`] was given.
    pub bind: wgpu::BindGroup,
    /// What [`Self::bind`] binds, to bind it again when the night lights
    /// change.
    views: Views,
    /// The layout [`Self::bind`] is laid out by.
    layout: wgpu::BindGroupLayout,
    /// The night lights: city light, a luminance on a geographic grid.
    pub night_lights: wgpu::Texture,
    /// The star catalogue's table ([`crate::stars::StarTable`]), and
    /// whether it is uploaded yet.
    pub stars: wgpu::Texture,
    stars_held: bool,
    maker: SkyViewMaker,
}

enum SkyViewMaker {
    Compute {
        bind: wgpu::BindGroup,
        pipeline: wgpu::ComputePipeline,
    },
    Render {
        bind: wgpu::BindGroup,
        pipeline: wgpu::RenderPipeline,
    },
}

/// 32-bit floats: a night's light is ten orders of magnitude under a
/// day's, under what half floats hold. Read by hand (`lut_bilinear`): not
/// filterable everywhere.
pub const LUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// The declarations `hillaire.wgsl` asks of the module that includes it, at
/// `group`, with the sky-view table beside them.
pub fn declarations(group: u32) -> String {
    crate::hillaire::wgsl_bindings(group)
}

/// The layout of [`AtmosphereGpu::bind_group`], seen by `visibility`:
/// every binding of [`declarations`], or — without `sky_view` — all but
/// the two sky-view tables and the stars, which only the sky reads: textures fewer
/// against a stage's limit, for a pass that has the ground's imagery to
/// bind beside them.
pub fn layout(
    device: &wgpu::Device,
    visibility: wgpu::ShaderStages,
    sky_view: bool,
) -> wgpu::BindGroupLayout {
    let lut = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let mut entries = vec![
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        lut(1),
        lut(2),
        wgpu::BindGroupLayoutEntry {
            binding: 4,
            visibility,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
        lut(5),
        wgpu::BindGroupLayoutEntry {
            binding: 7,
            visibility,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
    ];
    if sky_view {
        entries.extend([lut(3), lut(6), lut(8)]);
        entries.sort_by_key(|e| e.binding);
    }
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("atmosphere"),
        entries: &entries,
    })
}

/// How many textures [`layout`] binds to a stage without the sky-view
/// tables: what a pass drawing the ground has fewer of for its imagery.
pub const GROUND_TEXTURES: u32 = 4;

impl AtmosphereGpu {
    /// The tables, empty, and the off uniform: until a look asks for an
    /// atmosphere, nothing reads them. [`Self::bind`] is laid out by
    /// `layout` ([`layout`] with the sky-view); the sky-view is made by
    /// `pass`.
    pub fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, pass: SkyViewPass) -> Self {
        let texture = |label, [w, h]: [usize; 2], usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w as u32,
                    height: h as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: LUT_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | usage,
                view_formats: &[],
            })
        };
        use wgpu::TextureUsages as U;
        let made = match pass {
            SkyViewPass::Compute => U::STORAGE_BINDING | U::COPY_SRC,
            SkyViewPass::Render => U::RENDER_ATTACHMENT | U::COPY_SRC,
        };
        let transmittance = texture("atmosphere transmittance", TRANSMITTANCE_SIZE, U::COPY_DST);
        let multi_scattering = texture(
            "atmosphere multiple scattering",
            MULTI_SCATTERING_SIZE,
            U::COPY_DST,
        );
        let irradiance = texture("atmosphere irradiance", IRRADIANCE_SIZE, U::COPY_DST);
        let sky_view = texture("atmosphere sky view", SKY_VIEW_SIZE, made);
        let moon_view = texture("atmosphere moon view", SKY_VIEW_SIZE, made);
        let star_table = crate::stars::StarTable::catalogue();
        let stars = texture(
            "atmosphere stars",
            [star_table.width as usize, star_table.height as usize],
            U::COPY_DST,
        );
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("atmosphere"),
            size: std::mem::size_of::<AtmosphereUniform>() as u64,
            // Copied from, for a test that reads back what a frame drew with.
            usage: wgpu::BufferUsages::UNIFORM
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atmosphere"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        let (tv, mv, sv, iv, nv) = (
            view(&transmittance),
            view(&multi_scattering),
            view(&sky_view),
            view(&irradiance),
            view(&moon_view),
        );
        let night_lights = night_texture(device, 1, 1);
        let views = Views {
            transmittance: tv.clone(),
            multi_scattering: mv.clone(),
            sky_view: sv.clone(),
            irradiance: iv.clone(),
            moon_view: nv.clone(),
            night_lights: night_lights.create_view(&Default::default()),
            stars: view(&stars),
            sampler: sampler.clone(),
        };
        let bind = views.bind(device, layout, &uniform, true);
        let tables = |binding_out: &str| {
            format!(
                "@group(0) @binding(0) var<uniform> atmosphere: Atmosphere;\n\
                 @group(0) @binding(1) var transmittance_lut: texture_2d<f32>;\n\
                 @group(0) @binding(2) var multi_scattering_lut: texture_2d<f32>;\n\
                 {binding_out}\
                 @group(0) @binding(6) var irradiance_lut: texture_2d<f32>;\n\
                 @group(0) @binding(7) var night_lights_lut: texture_2d<f32>;\n\
                 @group(0) @binding(8) var lut_sampler: sampler;\n\
                 @group(0) @binding(9) var star_table: texture_2d<f32>;\n\
                 {}\n{}",
                crate::hillaire::WGSL,
                SKY_VIEW_WGSL,
            )
        };
        fn entry(binding: u32, v: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: wgpu::BindingResource::TextureView(v),
            }
        }
        let maker = match pass {
            SkyViewPass::Compute => {
                let source = format!(
                    "{}\n{}",
                    tables(
                        "@group(0) @binding(4) var sky_view_out: texture_storage_2d<rgba32float, write>;\n\
                         @group(0) @binding(5) var moon_view_out: texture_storage_2d<rgba32float, write>;\n"
                    ),
                    SKY_VIEW_COMPUTE_WGSL
                );
                let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("atmosphere sky view"),
                    source: wgpu::ShaderSource::Wgsl(source.into()),
                });
                let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("atmosphere sky view"),
                    layout: None,
                    module: &module,
                    entry_point: Some("sky_view"),
                    compilation_options: Default::default(),
                    cache: None,
                });
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("atmosphere sky view"),
                    layout: &pipeline.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform.as_entire_binding(),
                        },
                        entry(1, &tv),
                        entry(2, &mv),
                        entry(4, &sv),
                        entry(5, &nv),
                    ],
                });
                SkyViewMaker::Compute { bind, pipeline }
            }
            SkyViewPass::Render => {
                let source = format!("{}\n{}", tables(""), SKY_VIEW_RENDER_WGSL);
                let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("atmosphere sky view"),
                    source: wgpu::ShaderSource::Wgsl(source.into()),
                });
                let target = Some(wgpu::ColorTargetState {
                    format: LUT_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                });
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("atmosphere sky view"),
                    layout: None,
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("sky_view_vertex"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("sky_view_fragment"),
                        compilation_options: Default::default(),
                        targets: &[target.clone(), target],
                    }),
                    multiview_mask: None,
                    cache: None,
                });
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("atmosphere sky view"),
                    layout: &pipeline.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform.as_entire_binding(),
                        },
                        entry(1, &tv),
                        entry(2, &mv),
                    ],
                });
                SkyViewMaker::Render { bind, pipeline }
            }
        };
        Self {
            uniform,
            transmittance,
            multi_scattering,
            irradiance,
            sky_view,
            moon_view,
            model: None,
            reference: None,
            light: None,
            frame_uniform: AtmosphereUniform::zeroed_off(),
            sky_view_stale: false,
            bind,
            views,
            layout: layout.clone(),
            night_lights,
            stars,
            stars_held: false,
            maker,
        }
    }

    /// The tables and the uniform bound against `layout` ([`layout`] with
    /// the sky-view or without it, as `sky_view` says).
    pub fn bind_group(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sky_view: bool,
    ) -> wgpu::BindGroup {
        self.views.bind(device, layout, &self.uniform, sky_view)
    }

    /// The tables of `params`, built and uploaded unless they are already
    /// the ones held. `make` builds a model's tables: [`LutAtmosphere::new`]
    /// for a film, which can wait; [`LutAtmosphere::of`] for a viewer,
    /// which reads a clear day's instead.
    pub fn hold_with(
        &mut self,
        queue: &wgpu::Queue,
        params: AtmosphereParams,
        make: impl Fn(AtmosphereParams) -> LutAtmosphere,
    ) {
        if !self.stars_held {
            let table = crate::stars::StarTable::catalogue();
            queue.write_texture(
                self.stars.as_image_copy(),
                bytemuck::cast_slice(&table.texels),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(table.width * 16),
                    rows_per_image: Some(table.height),
                },
                self.stars.size(),
            );
            self.stars_held = true;
        }
        if self.reference.is_none() {
            self.reference = Some(make(AtmosphereParams::default()));
        }
        if self.model.as_ref().map(|m| *m.params()) == Some(params) {
            return;
        }
        let model = if params == AtmosphereParams::default() {
            self.reference.clone().expect("made above")
        } else {
            make(params)
        };
        upload(
            queue,
            &self.transmittance,
            TRANSMITTANCE_SIZE,
            model.transmittance_table(),
        );
        upload(
            queue,
            &self.multi_scattering,
            MULTI_SCATTERING_SIZE,
            model.multi_scattering_table(),
        );
        upload(
            queue,
            &self.irradiance,
            IRRADIANCE_SIZE,
            model.irradiance_table(),
        );
        self.model = Some(model);
    }

    /// [`Self::hold_with`] the tables computed.
    pub fn hold(&mut self, queue: &wgpu::Queue, params: AtmosphereParams) {
        self.hold_with(queue, params, LutAtmosphere::new);
    }

    /// Makes the sky-view table of the frame's uniform, if it is stale.
    pub fn record_sky_view(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if !self.sky_view_stale {
            return;
        }
        self.sky_view_stale = false;
        match &self.maker {
            SkyViewMaker::Compute { bind, pipeline } => {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("atmosphere sky view"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.dispatch_workgroups(
                    (SKY_VIEW_SIZE[0] as u32).div_ceil(8),
                    (SKY_VIEW_SIZE[1] as u32).div_ceil(8),
                    1,
                );
            }
            SkyViewMaker::Render { bind, pipeline } => {
                let target = |view| {
                    Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })
                };
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("atmosphere sky view"),
                    color_attachments: &[
                        target(&self.views.sky_view),
                        target(&self.views.moon_view),
                    ],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
    }
}

/// What the atmosphere's bind groups hold.
struct Views {
    transmittance: wgpu::TextureView,
    multi_scattering: wgpu::TextureView,
    sky_view: wgpu::TextureView,
    irradiance: wgpu::TextureView,
    moon_view: wgpu::TextureView,
    night_lights: wgpu::TextureView,
    stars: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

impl Views {
    fn bind(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform: &wgpu::Buffer,
        sky_view: bool,
    ) -> wgpu::BindGroup {
        fn view(binding: u32, v: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: wgpu::BindingResource::TextureView(v),
            }
        }
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            view(1, &self.transmittance),
            view(2, &self.multi_scattering),
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(&self.sampler),
            },
            view(5, &self.irradiance),
            view(7, &self.night_lights),
        ];
        if sky_view {
            entries.push(view(3, &self.sky_view));
            entries.push(view(6, &self.moon_view));
            entries.push(view(8, &self.stars));
            entries.sort_by_key(|e| e.binding);
        }
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atmosphere"),
            layout,
            entries: &entries,
        })
    }
}

/// A night-lights texture of `width` by `height`, one luminance a texel.
fn night_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atmosphere night lights"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

impl AtmosphereGpu {
    /// The night lights: `luma`, `width` by `height`, row 0 the north pole,
    /// column 0 the antimeridian, one byte a texel — the city light of the
    /// whole Earth on a geographic grid.
    pub fn set_night_lights(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        luma: &[u8],
    ) {
        let texture = night_texture(device, width, height);
        queue.write_texture(
            texture.as_image_copy(),
            luma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.views.night_lights = texture.create_view(&Default::default());
        self.bind = self.views.bind(device, &self.layout, &self.uniform, true);
        self.night_lights = texture;
    }
}

impl AtmosphereUniform {
    /// No atmosphere: `shape.w` zero, which every shader reads as none.
    pub fn zeroed_off() -> Self {
        bytemuck::Zeroable::zeroed()
    }
}

fn upload(queue: &wgpu::Queue, texture: &wgpu::Texture, [w, h]: [usize; 2], table: &[[f64; 3]]) {
    let texels: Vec<f32> = table
        .iter()
        .flat_map(|t| [t[0], t[1], t[2], 1.0].map(|v| v as f32))
        .collect();
    queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w as u32 * 16),
            rows_per_image: Some(h as u32),
        },
        wgpu::Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
    );
}
