// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A frame's light, worked out on the GPU every frame (feature `wgpu`): the
//! one engine a film and a viewer light their frames by.
//!
//! [`crate::LutAtmosphere::light`] is the model of it: what a moment's sun,
//! Moon and sky give the ground, the camera metered over what it sees, its
//! exposure, its white balance and the sky's harmonic. Some milliseconds of
//! CPU — too many for every frame, so a renderer that ran it on the CPU
//! worked it out again by steps, and every step showed: the whole picture
//! jumped four times a second while the camera moved. Here the same sums
//! run on the GPU, every frame, as functions of that frame's inputs alone:
//! nothing is kept from one frame to the next, nothing waits for a
//! threshold, and a camera that moves a little moves the light a little.
//!
//! What stays on the CPU is the geometry, which costs microseconds: where
//! the ground under the eye is and its frame, the meter's rays
//! ([`crate::hillaire::metering`]) and the ground each meets, and the
//! calibration against a clear day's air at that ground's height
//! ([`reference_physical`], a table made once per process and read
//! between its rows). [`FrameLightInputs::new`] packs them.
//!
//! [`FrameLightPass::record`] makes the light in two passes — the shares
//! of every sum, then each slot of the uniform finished from them — in
//! compute where the device has compute and drawn where it has not
//! (WebGL2), the same WGSL either way (`frame_light.wgsl`).
//! [`FrameLightPass::copy_into_uniform`] then copies the light's fields
//! into the frame's [`AtmosphereUniform`] on the GPU, over the geometry the
//! CPU wrote: nothing is read back in the frame's loop.

use std::sync::OnceLock;

use glam::{DMat3, DVec3};
use tuile_core::geo::{ecef_to_geodetic, enu_frame, Geodetic};

use crate::gpu::{AtmosphereGpu, AtmosphereUniform, SkyViewPass, LUT_FORMAT};
use crate::hillaire::{Calibration, Metered, Moment, NIGHT_SKY_ALBEDO, NIGHT_TINT, NIGHT_WHITE};
use crate::illumination::{ReferenceIllumination, DAYLIGHT_EV100};
use crate::{AtmosphereFrame, AtmosphereParams, LutAtmosphere, Moonlight, SkyLight};

/// The sums of a frame's light (`light_partial`, `light_slot`), after
/// [`crate::hillaire::WGSL`].
pub const FRAME_LIGHT_WGSL: &str = include_str!("frame_light.wgsl");

/// How many shares the sky's sums are cut into: one invocation, or one
/// texel, a direction.
pub const LIGHT_THREADS: u32 = 1024;
/// The nodes of a meter's cell: four on its ground, eight on its air
/// ([`crate::hillaire::MeterCell::nodes`]); one invocation a node.
pub const METER_NODES: u32 = 12;
/// How many slices every share is added up in before the slots read the
/// totals: a reduction in two steps, every slot then reading this many.
pub const LIGHT_TOTALS: u32 = 64;
/// How many directions a facing's sky is summed over:
/// [`crate::LutAtmosphere::sky_irradiance`]'s.
pub const LIGHT_DIRECTIONS: u32 = 1024;
/// The steps of the air between the eye and a metered ground point.
pub const METER_STEPS: u32 = 16;
/// The rows of the sky's shares: one a light, the sun's and the Moon's —
/// their five facings packed in four textures.
const PARTIAL_ROWS: u32 = 2;

/// How many rays the meter has at most: [`crate::hillaire::metering`]'s
/// grid.
pub const METER_RAYS: usize = 24 * 14;

/// The vec4s of [`AtmosphereUniform`].
const UNIFORM_SLOTS: u32 = (std::mem::size_of::<AtmosphereUniform>() / 16) as u32;

/// The extras after the uniform's slots: what a renderer reads of the
/// light besides the uniform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    /// The sky averaged over every facing, pre-exposed; w the host's.
    Ambient,
    /// The sun's irradiance on a surface facing it, pre-exposed; w the
    /// host's.
    Sun,
    /// The white balance, the look's times the camera's; w the host's.
    White,
    /// x: the exposure's linear multiplier; y: the EV100 metered; z: its
    /// difference from daylight's, in stops; w: the metered luminance.
    Exposure,
}

impl Extra {
    const ALL: [Extra; 4] = [Extra::Ambient, Extra::Sun, Extra::White, Extra::Exposure];

    fn slot(self) -> u32 {
        UNIFORM_SLOTS
            + match self {
                Extra::Ambient => 0,
                Extra::Sun => 1,
                Extra::White => 2,
                Extra::Exposure => 3,
            }
    }
}

const SLOTS: u32 = UNIFORM_SLOTS + Extra::ALL.len() as u32;

/// The uniform's slots the light writes, by their offsets in it.
fn light_slots() -> [(&'static str, u32); 12] {
    use std::mem::offset_of;
    let s = |o: usize| (o / 16) as u32;
    [
        (
            "SLOT_ILLUMINANCE",
            s(offset_of!(AtmosphereUniform, illuminance)),
        ),
        ("SLOT_FLOOR", s(offset_of!(AtmosphereUniform, floor))),
        ("SLOT_SH_X", s(offset_of!(AtmosphereUniform, sh_x))),
        ("SLOT_SH_Y", s(offset_of!(AtmosphereUniform, sh_y))),
        ("SLOT_SH_Z", s(offset_of!(AtmosphereUniform, sh_z))),
        ("SLOT_GROUND", s(offset_of!(AtmosphereUniform, ground))),
        ("SLOT_ADDED", s(offset_of!(AtmosphereUniform, added))),
        ("SLOT_LEAST", s(offset_of!(AtmosphereUniform, least))),
        (
            "SLOT_LEAST_SKY",
            s(offset_of!(AtmosphereUniform, least_sky)),
        ),
        (
            "SLOT_LEVEL_REF",
            s(offset_of!(AtmosphereUniform, level_ref)),
        ),
        ("SLOT_AMBIENT", s(offset_of!(AtmosphereUniform, ambient))),
        ("SLOT_GRADE", s(offset_of!(AtmosphereUniform, grade))),
    ]
}

/// The runs of consecutive slots the light writes: what is copied into
/// the uniform, each run in one copy.
fn light_runs() -> Vec<(u32, u32)> {
    let mut slots: Vec<u32> = light_slots().iter().map(|&(_, s)| s).collect();
    slots.sort_unstable();
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for s in slots {
        match runs.last_mut() {
            Some((start, len)) if *start + *len == s => *len += 1,
            _ => runs.push((s, 1)),
        }
    }
    runs
}

/// The constants `frame_light.wgsl` reads, from the CPU's own.
fn constants() -> String {
    let mut out = String::new();
    for (name, slot) in light_slots() {
        out += &format!("const {name}: u32 = {slot}u;\n");
    }
    for extra in Extra::ALL {
        let name = match extra {
            Extra::Ambient => "EXTRA_AMBIENT",
            Extra::Sun => "EXTRA_SUN",
            Extra::White => "EXTRA_WHITE",
            Extra::Exposure => "EXTRA_EXPOSURE",
        };
        out += &format!("const {name}: u32 = {}u;\n", extra.slot());
    }
    let v = |c: [f32; 3]| format!("vec3f({:?}, {:?}, {:?})", c[0], c[1], c[2]);
    out += &format!(
        "const LIGHT_THREADS: u32 = {LIGHT_THREADS}u;\n\
         const LIGHT_DIRECTIONS: u32 = {LIGHT_DIRECTIONS}u;\n\
         const METER_STEPS: u32 = {METER_STEPS}u;\n\
         const LIGHT_SLOTS: u32 = {SLOTS}u;\n\
         const PARTIAL_ROWS: u32 = {PARTIAL_ROWS}u;\n\
         const METER_NODES: u32 = {METER_NODES}u;\n\
         const LIGHT_TOTALS: u32 = {LIGHT_TOTALS}u;\n\
         const METER_RAYS: u32 = {METER_RAYS}u;\n\
         const NIGHT_TINT: vec3f = {};\n\
         const NIGHT_WHITE: vec3f = {};\n\
         const NIGHT_FLOOR: f32 = {:?};\n\
         const SHOWN_FLOOR: f32 = {:?};\n\
         const NIGHT_SKY_ALBEDO: f32 = {:?};\n\
         const DAYLIGHT_EV100: f32 = {:?};\n\
         const METER_AIR: f32 = {:?};\n\
         const METER_FLOOR: f32 = {:?};\n",
        v(NIGHT_TINT),
        v(NIGHT_WHITE),
        ReferenceIllumination::NIGHT_FLOOR,
        ReferenceIllumination::SHOWN_FLOOR,
        NIGHT_SKY_ALBEDO as f32,
        DAYLIGHT_EV100,
        crate::hillaire::METER_AIR as f32,
        crate::hillaire::METER_FLOOR as f32,
    );
    out
}

/// `LightInputs` in `frame_light.wgsl`, byte for byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct LightInputs {
    enu_x: [f32; 4],
    enu_y: [f32; 4],
    enu_z: [f32; 4],
    to_sun: [f32; 4],
    to_moon: [f32; 4],
    eye_sun: [f32; 4],
    eye_moon: [f32; 4],
    scale: [f32; 4],
    calibrated: [f32; 4],
    look_white: [f32; 4],
    look: [f32; 4],
    extras_w: [f32; 4],
    meter_toward: [f32; 4],
    meter_radii: [f32; 4],
    eye_x: [f32; 4],
    eye_y: [f32; 4],
    eye_z: [f32; 4],
}

/// How many rows the calibration's table has: heights from the ground to
/// the top of the air, bunched towards the ground as the irradiance
/// table's are.
const REFERENCE_ROWS: usize = 33;

/// What a clear day's sun 45° up gives level ground at `height`, per unit
/// of it outside the air, over pi — what [`crate::LutAtmosphere::light`]
/// calibrates the renderer's units against — from a table of a clear day's
/// air made once per process, read between its rows: continuous in the
/// height.
pub fn reference_physical(height: f64) -> [f64; 3] {
    static TABLE: OnceLock<(f64, Vec<[f64; 3]>)> = OnceLock::new();
    let (thickness, rows) = TABLE.get_or_init(|| {
        let reference = LutAtmosphere::today();
        let thickness = reference.params().thickness;
        let rows = (0..REFERENCE_ROWS)
            .map(|i| {
                let u = i as f64 / (REFERENCE_ROWS - 1) as f64;
                reference_physical_exact(&reference, u * u * thickness)
            })
            .collect();
        (thickness, rows)
    });
    let u = (height.max(0.0) / thickness).min(1.0).sqrt() * (REFERENCE_ROWS - 1) as f64;
    let i = (u.floor() as usize).min(REFERENCE_ROWS - 2);
    let f = u - i as f64;
    [0, 1, 2].map(|c| rows[i][c] * (1.0 - f) + rows[i + 1][c] * f)
}

/// [`reference_physical`] summed at `height`, as
/// [`crate::LutAtmosphere::light`] sums it.
pub fn reference_physical_exact(reference: &LutAtmosphere, height: f64) -> [f64; 3] {
    let up45 = std::f64::consts::FRAC_1_SQRT_2;
    let r = crate::hillaire::LUT_RADIUS + height.max(0.0);
    let sun = reference.sunlight(r, up45);
    let sky =
        reference.sky_irradiance(height.max(0.0), DVec3::new(up45, 0.0, up45), &[DVec3::Z])[0];
    [0, 1, 2].map(|c| (sun[c] * up45 + sky[c]) / std::f64::consts::PI)
}

/// What one frame's light is made from: everything the GPU needs that is
/// the frame's geometry — the ground under the eye, the lights, the meter's
/// rays — and the look's calibration, exposure and white balance.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameLightInputs {
    raw: LightInputs,
    rays: Vec<[f32; 4]>,
    /// The light's frame: where the air's sphere is laid, the ground under
    /// the eye, the sun and the Moon — what the uniform's geometry is
    /// written from ([`Self::uniform`]).
    pub light: SkyLight,
}

impl FrameLightInputs {
    /// The light of the air `params` at the ground `at` under the eye `eye` (ECEF), at
    /// `moment`, the camera metering `metering`, calibrated by
    /// `calibration`, exposed `exposure_ev` stops over the camera's own
    /// and balanced by `white_balance` on top of it. `extras_w`: the w
    /// of each [`Extra`] slot, the host's (`Extra::Exposure`'s is not).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        params: &AtmosphereParams,
        at: Geodetic,
        eye: DVec3,
        moment: Moment,
        metering: &[Metered],
        calibration: Calibration,
        exposure_ev: f32,
        white_balance: [f32; 3],
        extras_w: [f32; 3],
    ) -> Self {
        let height = at.height.max(0.0);
        let enu: DMat3 = enu_frame(at);
        let to_sun = moment.to_sun_ecef.try_normalize().unwrap_or(enu.z_axis);
        let moon = moment.moon.map(|m| {
            let to_moon = m.to_moon_ecef.try_normalize().unwrap_or(-enu.z_axis);
            (
                Moonlight {
                    to_moon_ecef: to_moon,
                    ..m
                },
                m.outside_air(),
            )
        });
        let strength = moon.map_or(0.0, |(_, s)| s);
        let to_moon = moon.map_or(-enu.z_axis, |(m, _)| m.to_moon_ecef);

        let physical = reference_physical(height);
        let up45 = std::f64::consts::FRAC_1_SQRT_2;
        let mut scale = [0.0f64; 3];
        let mut calibrated = [0.0f64; 3];
        for c in 0..3 {
            calibrated[c] = f64::from(calibration.sun[c]) * up45 + f64::from(calibration.sky[c]);
            scale[c] = calibrated[c] / physical[c].max(1e-12);
        }

        let from = ecef_to_geodetic(eye);
        let eye_frame = enu_frame(from);
        let local = eye_frame.transpose();
        let sphere = AtmosphereFrame::at(at);
        let to_centre = sphere.centre - eye;
        let distance = to_centre.length();
        let toward = to_centre / distance.max(1e-9);
        let mut rays = vec![[0.0f32; 4]; 2 * METER_RAYS];
        let counted = metering.len().min(METER_RAYS);
        for (j, m) in metering.iter().take(METER_RAYS).enumerate() {
            // A point of the ground or of the sky is a cell of no extent.
            let (across, from, to) = match *m {
                Metered::Cell(cell) => (cell.across, cell.from, cell.to),
                Metered::Ground { point, .. }
                | Metered::Sky {
                    direction: point, ..
                } => {
                    let d = match *m {
                        Metered::Ground { .. } => (point - eye).normalize_or(toward),
                        _ => point.normalize_or(toward),
                    };
                    let cos = d.dot(toward).clamp(-1.0, 1.0);
                    let across = (d - toward * cos)
                        .try_normalize()
                        .unwrap_or_else(|| toward.any_orthonormal_vector());
                    (across, cos.acos(), cos.acos())
                }
            };
            let a = local * across;
            rays[j] = [a.x as f32, a.y as f32, a.z as f32, from as f32];
            rays[METER_RAYS + j] = [to as f32, 0.0, 0.0, 0.0];
        }
        let v3 = |v: DVec3, w: f64| [v.x as f32, v.y as f32, v.z as f32, w as f32];
        let camera = calibration.camera;
        let raw = LightInputs {
            enu_x: v3(enu.x_axis, height),
            enu_y: v3(enu.y_axis, f64::from(moment.compensation_ev)),
            enu_z: v3(enu.z_axis, strength),
            to_sun: v3(to_sun, from.height),
            to_moon: v3(to_moon, counted as f64),
            eye_sun: v3(local * to_sun, f64::from(calibration.reflectance)),
            eye_moon: v3(local * to_moon, camera.meter_reflectance),
            scale: [
                scale[0] as f32,
                scale[1] as f32,
                scale[2] as f32,
                camera.follows,
            ],
            calibrated: [
                calibrated[0] as f32,
                calibrated[1] as f32,
                calibrated[2] as f32,
                camera.night_depth_ev,
            ],
            look_white: [
                white_balance[0],
                white_balance[1],
                white_balance[2],
                camera.white_adaptation,
            ],
            look: [exposure_ev, camera.meter_floor as f32, 0.0, 0.0],
            extras_w: [extras_w[0], extras_w[1], extras_w[2], 0.0],
            meter_toward: v3(local * toward, distance),
            meter_radii: [
                (sphere.radius + height) as f32,
                (sphere.radius + params.thickness) as f32,
                0.0,
                0.0,
            ],
            eye_x: v3(eye_frame.x_axis, 0.0),
            eye_y: v3(eye_frame.y_axis, 0.0),
            eye_z: v3(eye_frame.z_axis, 0.0),
        };
        let mut illumination = ReferenceIllumination::provisional(to_sun, [0.0; 3], [0.0; 3]);
        illumination.moon = moon.map(|(m, _)| m);
        illumination.compensation_ev = moment.compensation_ev;
        let light = SkyLight {
            illumination,
            illuminance: [0.0; 3],
            floor: [0.0; 3],
            ground: [0.0; 3],
            added: [0.0; 3],
            least: [0.0; 3],
            level_reference: [0.0; 3],
            moon_strength: strength,
            frame: AtmosphereFrame::at(at),
            height,
        };
        Self { raw, rays, light }
    }

    /// The frame's uniform, its geometry written — the air, the sphere seen
    /// from `eye`, the sun, the Moon, the stars, the discs — and its light
    /// left for [`FrameLightPass::copy_into_uniform`] to fill.
    pub fn uniform(
        &self,
        params: &AtmosphereParams,
        eye: DVec3,
        stars: DMat3,
        look: &crate::SkyLook,
    ) -> AtmosphereUniform {
        AtmosphereUniform::new(params, &self.light, eye, 1.0, stars, look)
            .seen_from(&self.light, eye)
    }
}

/// One of the light's passes: its pipeline, what it binds, and what it
/// covers — invocations in compute, texels drawn otherwise.
enum Stage {
    Compute {
        pipeline: wgpu::ComputePipeline,
        bind: wgpu::BindGroup,
        groups: (u32, u32),
    },
    Render {
        pipeline: wgpu::RenderPipeline,
        bind: wgpu::BindGroup,
        targets: Vec<wgpu::TextureView>,
    },
}

/// What a pass binds.
#[derive(Clone, Copy)]
enum Kind<'a> {
    Uniform(&'a wgpu::Buffer),
    Texture(&'a wgpu::TextureView),
    Output(&'a wgpu::TextureView),
}

/// What every pass is made with.
struct Builder<'a> {
    device: &'a wgpu::Device,
    module: &'a wgpu::ShaderModule,
    compute: bool,
    visibility: wgpu::ShaderStages,
}

impl Builder<'_> {
    /// The pass of entry point `entry` (`…_main` in compute, `…_fragment`
    /// drawn), binding `bindings` and writing `outputs` — storage textures
    /// at their bindings in compute, colour targets in order drawn — over
    /// `size`.
    fn stage<'b>(
        &self,
        label: &str,
        entry: &str,
        mut bindings: Vec<(u32, Kind<'b>)>,
        outputs: &[(u32, &'b wgpu::TextureView)],
        size: (u32, u32),
    ) -> Stage {
        let (device, module, compute, visibility) =
            (self.device, self.module, self.compute, self.visibility);
        if compute {
            for (binding, view) in outputs {
                bindings.push((*binding, Kind::Output(view)));
            }
        }
        let layout_entries: Vec<wgpu::BindGroupLayoutEntry> = bindings
            .iter()
            .map(|(binding, kind)| wgpu::BindGroupLayoutEntry {
                binding: *binding,
                visibility,
                ty: match kind {
                    Kind::Uniform(_) => wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    Kind::Texture(_) => wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    Kind::Output(_) => wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: LUT_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                },
                count: None,
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
            entries: &layout_entries,
        });
        let entries: Vec<wgpu::BindGroupEntry> = bindings
            .iter()
            .map(|(binding, kind)| wgpu::BindGroupEntry {
                binding: *binding,
                resource: match kind {
                    Kind::Uniform(b) => b.as_entire_binding(),
                    Kind::Texture(v) | Kind::Output(v) => wgpu::BindingResource::TextureView(v),
                },
            })
            .collect();
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &layout,
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        if compute {
            Stage::Compute {
                pipeline: device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(label),
                    layout: Some(&pipeline_layout),
                    module,
                    entry_point: Some(&format!("{entry}_main")),
                    compilation_options: Default::default(),
                    cache: None,
                }),
                bind,
                groups: (size.0.div_ceil(64), size.1),
            }
        } else {
            let target: Vec<Option<wgpu::ColorTargetState>> = outputs
                .iter()
                .map(|_| {
                    Some(wgpu::ColorTargetState {
                        format: LUT_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })
                })
                .collect();
            Stage::Render {
                pipeline: device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module,
                        entry_point: Some("light_vertex"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    fragment: Some(wgpu::FragmentState {
                        module,
                        entry_point: Some(&format!("{entry}_fragment")),
                        compilation_options: Default::default(),
                        targets: &target,
                    }),
                    multiview_mask: None,
                    cache: None,
                }),
                bind,
                targets: outputs.iter().map(|(_, v)| (*v).clone()).collect(),
            }
        }
    }
}

/// The frame's light on the GPU: its inputs, its shares, its slots, and the
/// passes that make them — the sky's sums, the meter's, then the slots.
pub struct FrameLightPass {
    inputs: wgpu::Buffer,
    rays: wgpu::Texture,
    /// The finished slots: the uniform's, then the [`Extra`]s — one texel
    /// a slot, `LUT_FORMAT`.
    pub slots: wgpu::Texture,
    stages: Vec<Stage>,
    runs: Vec<(u32, u32)>,
}

impl FrameLightPass {
    /// The passes, against `atmosphere`'s tables and uniform, made by
    /// `pass` (compute, or drawn for a device without it).
    pub fn new(device: &wgpu::Device, atmosphere: &AtmosphereGpu, pass: SkyViewPass) -> Self {
        let inputs = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame light inputs"),
            size: std::mem::size_of::<LightInputs>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        use wgpu::TextureUsages as U;
        let compute = pass == SkyViewPass::Compute;
        let made = if compute {
            U::STORAGE_BINDING
        } else {
            U::RENDER_ATTACHMENT
        };
        let texture = |label, w: u32, h: u32, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: LUT_FORMAT,
                usage,
                view_formats: &[],
            })
        };
        let rays = texture(
            "frame light rays",
            METER_RAYS as u32,
            2,
            U::TEXTURE_BINDING | U::COPY_DST,
        );
        let view_of = |t: &wgpu::Texture| t.create_view(&Default::default());
        let shares = |label| {
            texture(
                label,
                LIGHT_THREADS,
                PARTIAL_ROWS,
                U::TEXTURE_BINDING | made,
            )
        };
        let sky = [
            shares("frame light sky a"),
            shares("frame light sky b"),
            shares("frame light sky c"),
            shares("frame light sky d"),
        ];
        let meter = |label| {
            texture(
                label,
                METER_NODES,
                METER_RAYS as u32,
                U::TEXTURE_BINDING | made,
            )
        };
        let levels = texture("frame light level", 1, 1, U::TEXTURE_BINDING | made);
        let levels_view = view_of(&levels);
        let sky_totals = texture(
            "frame light sky totals",
            LIGHT_TOTALS,
            4 * PARTIAL_ROWS,
            U::TEXTURE_BINDING | made,
        );
        let sky_totals_view = view_of(&sky_totals);
        let meter_totals = texture(
            "frame light meter totals",
            LIGHT_TOTALS,
            2,
            U::TEXTURE_BINDING | made,
        );
        let meter_totals_view = view_of(&meter_totals);
        let meters = [
            meter("frame light meter seen"),
            meter("frame light meter logs"),
        ];
        let slots = texture("frame light slots", SLOTS, 1, U::COPY_SRC | made);
        let view = view_of;
        let rays_view = view(&rays);
        let sky_views = sky.each_ref().map(view);
        let meter_views = meters.each_ref().map(view);
        let slots_view = view(&slots);
        let (tv, mv, iv) = (
            view(&atmosphere.transmittance),
            view(&atmosphere.multi_scattering),
            view(&atmosphere.irradiance),
        );
        let visibility = if compute {
            wgpu::ShaderStages::COMPUTE
        } else {
            wgpu::ShaderStages::FRAGMENT
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("frame light"),
            source: wgpu::ShaderSource::Wgsl(wgsl(pass).into()),
        });
        let builder = Builder {
            device,
            module: &module,
            compute,
            visibility,
        };
        // The tables and the inputs every pass reads, then its own.
        let base = [
            (0, Kind::Uniform(&atmosphere.uniform)),
            (1, Kind::Texture(&tv)),
            (2, Kind::Texture(&mv)),
            (3, Kind::Texture(&iv)),
            (4, Kind::Uniform(&inputs)),
        ];
        let sky_read = [
            (6, Kind::Texture(&sky_views[0])),
            (14, Kind::Texture(&sky_views[1])),
            (15, Kind::Texture(&sky_views[2])),
            (16, Kind::Texture(&sky_views[3])),
        ];
        // The sky's shares: four textures from one invocation in compute;
        // drawn, two passes of two targets each — a colour attachment holds
        // 32 bytes a sample, two of these texels.
        let mut stages = if compute {
            vec![builder.stage(
                "frame light sky",
                "light_sky",
                base.to_vec(),
                &[
                    (7, &sky_views[0]),
                    (18, &sky_views[1]),
                    (19, &sky_views[2]),
                    (20, &sky_views[3]),
                ],
                (LIGHT_THREADS, PARTIAL_ROWS),
            )]
        } else {
            vec![
                builder.stage(
                    "frame light sky ab",
                    "light_sky_ab",
                    base.to_vec(),
                    &[(7, &sky_views[0]), (18, &sky_views[1])],
                    (LIGHT_THREADS, PARTIAL_ROWS),
                ),
                builder.stage(
                    "frame light sky cd",
                    "light_sky_cd",
                    base.to_vec(),
                    &[(19, &sky_views[2]), (20, &sky_views[3])],
                    (LIGHT_THREADS, PARTIAL_ROWS),
                ),
            ]
        };
        let sky_totals_read = (24, Kind::Texture(&sky_totals_view));
        stages.extend([
            builder.stage(
                "frame light sky totals",
                "light_sky_totals",
                [&base[..], &sky_read[..]].concat(),
                &[(25, &sky_totals_view)],
                (LIGHT_TOTALS, 4 * PARTIAL_ROWS),
            ),
            builder.stage(
                "frame light level",
                "light_levels",
                [&base[..], &[sky_totals_read]].concat(),
                &[(23, &levels_view)],
                (1, 1),
            ),
            builder.stage(
                "frame light meter",
                "light_meters",
                [
                    &base[..],
                    &[
                        (5, Kind::Texture(&rays_view)),
                        (22, Kind::Texture(&levels_view)),
                    ],
                ]
                .concat(),
                &[(12, &meter_views[0]), (21, &meter_views[1])],
                (METER_NODES, METER_RAYS as u32),
            ),
            builder.stage(
                "frame light meter totals",
                "light_meter_totals",
                [
                    &base[..],
                    &[
                        (11, Kind::Texture(&meter_views[0])),
                        (17, Kind::Texture(&meter_views[1])),
                    ],
                ]
                .concat(),
                &[(27, &meter_totals_view)],
                (LIGHT_TOTALS, 2),
            ),
            builder.stage(
                "frame light slots",
                "light_slots",
                [
                    &base[..],
                    &[sky_totals_read, (26, Kind::Texture(&meter_totals_view))],
                ]
                .concat(),
                &[(8, &slots_view)],
                (SLOTS, 1),
            ),
        ]);
        Self {
            inputs,
            rays,
            slots,
            stages,
            runs: light_runs(),
        }
    }

    /// Writes the frame's inputs, for the next [`Self::record`].
    pub fn write(&self, queue: &wgpu::Queue, inputs: &FrameLightInputs) {
        queue.write_buffer(&self.inputs, 0, bytemuck::bytes_of(&inputs.raw));
        queue.write_texture(
            self.rays.as_image_copy(),
            bytemuck::cast_slice(&inputs.rays),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(METER_RAYS as u32 * 16),
                rows_per_image: Some(2),
            },
            wgpu::Extent3d {
                width: METER_RAYS as u32,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Makes the frame's light: the sky's sums, the meter's, then its
    /// slots. Reads the atmosphere's uniform for the air and where its
    /// lights are — written before the submission this is recorded into.
    pub fn record(&self, encoder: &mut wgpu::CommandEncoder) {
        for stage in &self.stages {
            match stage {
                Stage::Compute {
                    pipeline,
                    bind,
                    groups,
                } => {
                    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("frame light"),
                        timestamp_writes: None,
                    });
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, bind, &[]);
                    pass.dispatch_workgroups(groups.0, groups.1, 1);
                }
                Stage::Render {
                    pipeline,
                    bind,
                    targets,
                } => {
                    let attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = targets
                        .iter()
                        .map(|view| {
                            Some(wgpu::RenderPassColorAttachment {
                                view,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                    store: wgpu::StoreOp::Store,
                                },
                            })
                        })
                        .collect();
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("frame light"),
                        color_attachments: &attachments,
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

    /// Copies the light's slots into `uniform` — an [`AtmosphereUniform`]
    /// whose geometry the CPU wrote — on the GPU, after [`Self::record`].
    pub fn copy_into_uniform(&self, encoder: &mut wgpu::CommandEncoder, uniform: &wgpu::Buffer) {
        for &(start, len) in &self.runs {
            self.copy_slots(encoder, start, len, uniform, u64::from(start) * 16);
        }
    }

    /// Copies one [`Extra`] into `buffer` at `offset`, a multiple of 16.
    pub fn copy_extra(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        extra: Extra,
        buffer: &wgpu::Buffer,
        offset: u64,
    ) {
        self.copy_slots(encoder, extra.slot(), 1, buffer, offset);
    }

    fn copy_slots(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        start: u32,
        len: u32,
        buffer: &wgpu::Buffer,
        offset: u64,
    ) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.slots,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: start,
                    y: 0,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset,
                    bytes_per_row: Some((len * 16).next_multiple_of(256)),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: len,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    }

    /// The light's slots read back, blocking: for a test or an instrument,
    /// never a frame's loop. The uniform's slots, then the [`Extra`]s.
    pub fn read(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> FrameLightReadout {
        let size = u64::from(SLOTS) * 16;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame light readout"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        self.copy_slots(&mut encoder, 0, SLOTS, &staging, 0);
        queue.submit([encoder.finish()]);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the device answers");
        let data = slice.get_mapped_range().expect("mapped").to_vec();
        let slots: Vec<[f32; 4]> = bytemuck::cast_slice(&data).to_vec();
        staging.unmap();
        FrameLightReadout { slots }
    }
}

/// The light's slots, read back ([`FrameLightPass::read`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameLightReadout {
    slots: Vec<[f32; 4]>,
}

impl FrameLightReadout {
    /// One [`Extra`].
    pub fn extra(&self, extra: Extra) -> [f32; 4] {
        self.slots[extra.slot() as usize]
    }

    /// The uniform with the light's fields as the GPU wrote them, over
    /// `geometry`'s.
    pub fn over(&self, geometry: AtmosphereUniform) -> AtmosphereUniform {
        let mut u = geometry;
        let slots: &mut [[f32; 4]] = bytemuck::cast_slice_mut(std::slice::from_mut(&mut u));
        for &(start, len) in &light_runs() {
            for s in start..start + len {
                slots[s as usize] = self.slots[s as usize];
            }
        }
        u
    }
}

/// The light's module as `pass` makes it: its bindings, the constants,
/// [`crate::hillaire::WGSL`], [`FRAME_LIGHT_WGSL`] and the entry points —
/// in compute, or drawn, as a device without compute (WebGL2) draws it.
pub fn wgsl(pass: SkyViewPass) -> String {
    let outputs = match pass {
        SkyViewPass::Compute => {
            "@group(0) @binding(7) var sky_out_a: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(18) var sky_out_b: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(19) var sky_out_c: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(20) var sky_out_d: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(8) var slots_out: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(12) var meter_out_seen: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(21) var meter_out_logs: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(23) var levels_out: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(25) var sky_totals_out: texture_storage_2d<rgba32float, write>;\n\
             @group(0) @binding(27) var meter_totals_out: texture_storage_2d<rgba32float, write>;\n"
        }
        SkyViewPass::Render => "",
    };
    format!(
        "@group(0) @binding(0) var<uniform> atmosphere: Atmosphere;\n\
         @group(0) @binding(1) var transmittance_lut: texture_2d<f32>;\n\
         @group(0) @binding(2) var multi_scattering_lut: texture_2d<f32>;\n\
         @group(0) @binding(3) var irradiance_lut: texture_2d<f32>;\n\
         @group(0) @binding(4) var<uniform> light_in: LightInputs;\n\
         @group(0) @binding(5) var light_rays: texture_2d<f32>;\n\
         @group(0) @binding(6) var light_sky_a: texture_2d<f32>;\n\
         @group(0) @binding(14) var light_sky_b: texture_2d<f32>;\n\
         @group(0) @binding(15) var light_sky_c: texture_2d<f32>;\n\
         @group(0) @binding(16) var light_sky_d: texture_2d<f32>;\n\
         @group(0) @binding(11) var light_meter_seen: texture_2d<f32>;\n\
         @group(0) @binding(17) var light_meter_logs: texture_2d<f32>;\n\
         @group(0) @binding(22) var light_levels: texture_2d<f32>;\n\
         @group(0) @binding(24) var light_sky_totals: texture_2d<f32>;\n\
         @group(0) @binding(26) var light_meter_totals: texture_2d<f32>;\n\
         {outputs}\
         @group(0) @binding(9) var night_lights_lut: texture_2d<f32>;\n\
         @group(0) @binding(10) var lut_sampler: sampler;\n\
         @group(0) @binding(13) var star_table: texture_2d<f32>;\n\
         {}\n{}\n{}\n{}\n",
        constants(),
        crate::hillaire::WGSL,
        FRAME_LIGHT_WGSL,
        match pass {
            SkyViewPass::Compute => COMPUTE_WGSL,
            SkyViewPass::Render => RENDER_WGSL,
        },
    )
}

const COMPUTE_WGSL: &str = "
@compute @workgroup_size(64)
fn light_sky_main(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= LIGHT_THREADS || id.y >= PARTIAL_ROWS) { return; }
    let share = light_sky(id.x, id.y);
    textureStore(sky_out_a, id.xy, share.a);
    textureStore(sky_out_b, id.xy, share.b);
    textureStore(sky_out_c, id.xy, share.c);
    textureStore(sky_out_d, id.xy, share.d);
}

@compute @workgroup_size(64)
fn light_sky_totals_main(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= LIGHT_TOTALS || id.y >= 4u * PARTIAL_ROWS) { return; }
    textureStore(sky_totals_out, id.xy, light_sky_total(id.x, id.y));
}

@compute @workgroup_size(64)
fn light_meter_totals_main(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= LIGHT_TOTALS || id.y >= 2u) { return; }
    textureStore(meter_totals_out, id.xy, light_meter_total(id.x, id.y));
}

@compute @workgroup_size(64)
fn light_levels_main(@builtin(global_invocation_id) id: vec3u) {
    if (id.x > 0u || id.y > 0u) { return; }
    textureStore(levels_out, vec2u(0u, 0u), vec4f(light_agree(), 0.0));
}

@compute @workgroup_size(64)
fn light_meters_main(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= METER_NODES || id.y >= METER_RAYS) { return; }
    let share = light_meter(id.x, id.y);
    textureStore(meter_out_seen, id.xy, share.seen);
    textureStore(meter_out_logs, id.xy, share.logs);
}

@compute @workgroup_size(64)
fn light_slots_main(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= LIGHT_SLOTS) { return; }
    textureStore(slots_out, vec2u(id.x, 0u), light_slot(id.x));
}
";

const RENDER_WGSL: &str = "
@vertex
fn light_vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    let xy = vec2f(f32((index << 1u) & 2u), f32(index & 2u)) * 2.0 - 1.0;
    return vec4f(xy, 0.0, 1.0);
}

struct SkyTargets {
    @location(0) first: vec4f,
    @location(1) second: vec4f,
}

@fragment
fn light_sky_ab_fragment(@builtin(position) at: vec4f) -> SkyTargets {
    let share = light_sky(u32(at.x), u32(at.y));
    var out: SkyTargets;
    out.first = share.a;
    out.second = share.b;
    return out;
}

@fragment
fn light_sky_cd_fragment(@builtin(position) at: vec4f) -> SkyTargets {
    let share = light_sky(u32(at.x), u32(at.y));
    var out: SkyTargets;
    out.first = share.c;
    out.second = share.d;
    return out;
}

struct MeterTargets {
    @location(0) seen: vec4f,
    @location(1) logs: vec4f,
}

@fragment
fn light_sky_totals_fragment(@builtin(position) at: vec4f) -> @location(0) vec4f {
    return light_sky_total(u32(at.x), u32(at.y));
}

@fragment
fn light_meter_totals_fragment(@builtin(position) at: vec4f) -> @location(0) vec4f {
    return light_meter_total(u32(at.x), u32(at.y));
}

@fragment
fn light_levels_fragment(@builtin(position) at: vec4f) -> @location(0) vec4f {
    return vec4f(light_agree(), 0.0);
}

@fragment
fn light_meters_fragment(@builtin(position) at: vec4f) -> MeterTargets {
    let share = light_meter(u32(at.x), u32(at.y));
    var out: MeterTargets;
    out.seen = share.seen;
    out.logs = share.logs;
    return out;
}

@fragment
fn light_slots_fragment(@builtin(position) at: vec4f) -> @location(0) vec4f {
    return light_slot(u32(at.x));
}
";
