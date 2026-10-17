// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere on the GPU: Hillaire's two constant tables,
//! computed on the CPU by `tuile-atmosphere` and uploaded; the sky-view
//! table, made in compute once a frame; and the uniform the resolve and
//! `present` read them by.
//!
//! Everything here is a frame's, not an eye's: the light, the tables and
//! the sky-view are worked out once, from the frame's own camera, and both
//! eyes of a stereo frame read the same. Only where the air's sphere stands
//! from the eye is restated for each picture.

use glam::DVec3;
use tuile_atmosphere::hillaire::{
    IRRADIANCE_SIZE, MULTI_SCATTERING_SIZE, SKY_VIEW_SIZE, TRANSMITTANCE_SIZE,
};
use tuile_atmosphere::{AtmosphereParams, LutAtmosphere, SkyLight};

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
    pub stars_x: [f32; 4],
    pub stars_y: [f32; 4],
    pub stars_z: [f32; 4],
}

/// The share of the full Moon's light its night side sends back by the
/// Earth's, the Earth full in its sky: earthshine is some ten thousand
/// times fainter than the lit crescent beside it.
pub const EARTHSHINE: f32 = 1.5e-4;

impl AtmosphereUniform {
    /// `params` as the shaders read them, lit by `light`, from the frame's
    /// eye at `frame_eye` (ECEF), every light pre-exposed by `exposure` —
    /// so that the HDR picture holds a night in half floats — with the
    /// stars turned by `stars` (Earth to celestial axes), if drawn.
    pub fn new(
        params: &AtmosphereParams,
        light: &SkyLight,
        frame_eye: DVec3,
        exposure: f32,
        stars: Option<glam::DMat3>,
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
                        tuile_atmosphere::moon::disc_shading_mean(a) as f32,
                        EARTHSHINE * 0.5 * (1.0 - a.cos() as f32),
                        0.0,
                        0.0,
                    ],
                )
            }
            None => ([0.0; 4], [0.0; 4], [1.0, 0.0, 0.0, 0.0]),
        };
        let star_rows = stars.map(|m| m.transpose());
        let row = |n: usize, on: f32| match star_rows {
            Some(r) => {
                let c = r.col(n).as_vec3();
                [c.x, c.y, c.z, on]
            }
            None => [0.0; 4],
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
            stars_x: row(0, 1.0),
            stars_y: row(1, 0.0),
            stars_z: row(2, 0.0),
        }
    }

    /// For the picture whose eye is at `eye` (ECEF): the sphere's centre
    /// from it, the subtraction in f64.
    pub fn seen_from(mut self, light: &SkyLight, eye: DVec3) -> Self {
        let centre = (light.frame.centre - eye).as_vec3();
        self.centre = [centre.x, centre.y, centre.z, self.centre[3]];
        self
    }
}

/// What a frame's light was worked out from: the same, to the bit, and the
/// frame's light and its sky-view are the last one's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LightKey {
    pub params: AtmosphereParams,
    pub frame_eye: DVec3,
    /// The frame camera's view: what the light is metered over.
    pub view: [f32; 16],
    pub ground: [f64; 3],
    pub to_sun: DVec3,
    pub moon: Option<tuile_atmosphere::Moonlight>,
    pub instant: Option<f64>,
    pub compensation_ev: f32,
}

/// The tables on the GPU and what made them.
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
    /// The frame's light, what it was worked out from, and its uniform
    /// before it is placed for an eye.
    pub key: Option<LightKey>,
    pub light: Option<SkyLight>,
    pub frame_uniform: AtmosphereUniform,
    /// Whether the sky-view table is to be made again before the next
    /// picture is drawn.
    pub sky_view_stale: bool,
    pub bind: wgpu::BindGroup,
    pub sky_view_bind: wgpu::BindGroup,
    pub sky_view_pipeline: wgpu::ComputePipeline,
}

/// 32-bit floats: a night's light is ten orders of magnitude under a
/// day's, under what half floats hold. Read by hand (`lut_bilinear`): not
/// filterable everywhere.
pub const LUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// The declarations `hillaire.wgsl` asks of the module that includes it, at
/// `group`, with the sky-view table beside them.
pub fn declarations(group: u32) -> String {
    format!(
        "@group({group}) @binding(0) var<uniform> atmosphere: Atmosphere;\n\
         @group({group}) @binding(1) var transmittance_lut: texture_2d<f32>;\n\
         @group({group}) @binding(2) var multi_scattering_lut: texture_2d<f32>;\n\
         @group({group}) @binding(3) var sky_view_lut: texture_2d<f32>;\n\
         @group({group}) @binding(4) var lut_sampler: sampler;\n\
         @group({group}) @binding(5) var irradiance_lut: texture_2d<f32>;\n\
         @group({group}) @binding(6) var moon_view_lut: texture_2d<f32>;\n"
    )
}

pub fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let lut = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("film atmosphere"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            lut(1),
            lut(2),
            lut(3),
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            lut(5),
            lut(6),
        ],
    })
}

impl AtmosphereGpu {
    /// The tables, empty, and the off uniform: until a look asks for an
    /// atmosphere, nothing reads them.
    pub fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> Self {
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
        let transmittance = texture("film transmittance", TRANSMITTANCE_SIZE, U::COPY_DST);
        let multi_scattering = texture(
            "film multiple scattering",
            MULTI_SCATTERING_SIZE,
            U::COPY_DST,
        );
        let irradiance = texture("film irradiance", IRRADIANCE_SIZE, U::COPY_DST);
        let sky_view = texture(
            "film sky view",
            SKY_VIEW_SIZE,
            U::STORAGE_BINDING | U::COPY_SRC,
        );
        let moon_view = texture(
            "film moon view",
            SKY_VIEW_SIZE,
            U::STORAGE_BINDING | U::COPY_SRC,
        );
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film atmosphere"),
            size: std::mem::size_of::<AtmosphereUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("film atmosphere"),
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
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film atmosphere"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&tv),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mv),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&sv),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&iv),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&nv),
                },
            ],
        });
        let source = format!(
            "@group(0) @binding(0) var<uniform> atmosphere: Atmosphere;\n\
             @group(0) @binding(1) var transmittance_lut: texture_2d<f32>;\n\
             @group(0) @binding(2) var multi_scattering_lut: texture_2d<f32>;\n\
             @group(0) @binding(4) var sky_view_out: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(5) var moon_view_out: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(6) var irradiance_lut: texture_2d<f32>;\n\
             {}\n{}",
            tuile_atmosphere::hillaire::WGSL,
            include_str!("shaders/sky_view.wgsl")
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("film sky view"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let sky_view_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("film sky view"),
            layout: None,
            module: &module,
            entry_point: Some("sky_view"),
            compilation_options: Default::default(),
            cache: None,
        });
        let sky_view_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film sky view"),
            layout: &sky_view_pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&tv),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&mv),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&sv),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&nv),
                },
            ],
        });
        Self {
            uniform,
            transmittance,
            multi_scattering,
            irradiance,
            sky_view,
            moon_view,
            model: None,
            reference: None,
            key: None,
            light: None,
            frame_uniform: AtmosphereUniform::zeroed_off(),
            sky_view_stale: false,
            bind,
            sky_view_bind,
            sky_view_pipeline,
        }
    }

    /// The tables of `params`, built and uploaded unless they are already
    /// the ones held.
    pub fn hold(&mut self, queue: &wgpu::Queue, params: AtmosphereParams) {
        if self.reference.is_none() {
            self.reference = Some(LutAtmosphere::new(AtmosphereParams::default()));
        }
        if self.model.as_ref().map(|m| *m.params()) == Some(params) {
            return;
        }
        let model = if params == AtmosphereParams::default() {
            self.reference.clone().expect("made above")
        } else {
            LutAtmosphere::new(params)
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

    /// Makes the sky-view table of the frame's uniform, if it is stale.
    pub fn record_sky_view(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if !self.sky_view_stale {
            return;
        }
        self.sky_view_stale = false;
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("film sky view"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.sky_view_pipeline);
        pass.set_bind_group(0, &self.sky_view_bind, &[]);
        pass.dispatch_workgroups(
            (SKY_VIEW_SIZE[0] as u32).div_ceil(8),
            (SKY_VIEW_SIZE[1] as u32).div_ceil(8),
            1,
        );
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
