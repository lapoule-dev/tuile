// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile render pipeline. The host owns the surface, the event loop and
//! the render pass; this type owns the pipelines and the view uniform.

use crate::context::GpuContext;
use crate::host::TargetLayout;
use crate::prepare::{PreparedTile, Vertex};
use glam::Mat4;
use tuile_atmosphere::AerialPerspective;
use wgpu::util::DeviceExt;

/// Per-frame view data. `view_proj` must be relative to the same render
/// origin the tiles were prepared with.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ViewUniform {
    pub view_proj: [f32; 16],
    /// Direction the light travels, normalized, w unused.
    pub sun_dir: [f32; 4],
    /// x = ambient term.
    pub params: [f32; 4],
    /// The air between the eye and the ground. Off by default — a scene that is
    /// not a planet has no air in it, and the model is
    /// [`tuile_atmosphere`]'s rather than this crate's so that a second backend
    /// cannot quietly disagree about the colour of air.
    pub atmosphere: AerialPerspective,
}

impl Default for ViewUniform {
    fn default() -> Self {
        Self {
            view_proj: Mat4::IDENTITY.to_cols_array(),
            sun_dir: [0.0, 0.0, -1.0, 0.0],
            params: [0.25, 0.0, 0.0, 0.0],
            atmosphere: AerialPerspective::default(),
        }
    }
}

/// Debug line vertex (bounding volumes).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LineVertex {
    pub position: [f32; 3],
    pub color: [f32; 3],
}

/// The mark a surface stamps to claim the ground it owns.
///
/// One value, set on every pass that touches the stencil. It must be set: the
/// default reference is 0, so an unset pass would stamp 0 and the fallback's
/// `NotEqual` test would then be false everywhere — the fallback would draw
/// nothing at all, which is a hole, which is black ground.
const OWNED: u32 = 1;

/// The mark a surface that **replaces** the ground stamps, besides
/// [`OWNED`]: a 3D Tiles mesh of the whole surface — buildings, trees and
/// the ground under them — drawn over a globe.
///
/// Two surfaces of the same ground from two sources never agree to the
/// centimetre, so a depth test between them is a coin toss per pixel: the
/// z-fighting this mark exists to make impossible. The replacing surface is
/// drawn first and stamps it; the globe's own surfaces draw only where it is
/// absent, whatever their depth. Where the replacing surface has nothing —
/// not loaded yet, or not covering — the globe is drawn exactly as without
/// it, so the ground is never bare.
const REPLACED: u32 = 2;

pub struct TileRenderer {
    pipeline: wgpu::RenderPipeline,
    /// Draws without touching depth — see [`TileRenderer::render_background`].
    background: wgpu::RenderPipeline,
    /// Redraws geometry the main pipeline already drew, to spend imagery layers
    /// one draw could not bind. See [`TileRenderer::render`].
    layers: wgpu::RenderPipeline,
    /// Draws ancestors standing in for ground that is not theirs: tests depth,
    /// never writes it, drawn last. See `Pass::Fallback`.
    fallback: wgpu::RenderPipeline,
    /// Draws a surface that replaces the ground where it is drawn, and its
    /// stand-ins. See [`REPLACED`] and [`TileRenderer::paint_replacing`].
    replacing: wgpu::RenderPipeline,
    replacing_fallback: wgpu::RenderPipeline,
    wireframe: Option<wgpu::RenderPipeline>,
    lines: wgpu::RenderPipeline,
    /// The physical atmosphere: its tables, uniform and sky-view, and when
    /// they are worked out again. See [`TileRenderer::set_sky`].
    sky: crate::sky::Sky,
    /// The atmosphere as the ground reads it (group 3): its tables without
    /// the sky-view.
    ground_air_bg: wgpu::BindGroup,
    /// The air past everything: see [`TileRenderer::render_sky`].
    sky_pipeline: wgpu::RenderPipeline,
    sky_buf: wgpu::Buffer,
    sky_bg: wgpu::BindGroup,
    sky_air_bg: wgpu::BindGroup,
    view_buf: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    line_buf: Option<(wgpu::Buffer, u32)>,
}

impl TileRenderer {
    /// `target_format` is the color format of the host's surface or
    /// offscreen target, drawn with our own frame's depth and samples
    /// ([`TargetLayout::own`]). Wireframe is created only when the device has
    /// `POLYGON_MODE_LINE`.
    pub fn new(gpu: &GpuContext, target_format: wgpu::TextureFormat) -> Self {
        Self::for_layout(gpu, TargetLayout::own(target_format))
    }

    /// The pipelines for targets of `layout`: a host's colour format, sample
    /// count and depth convention, drawn through a [`crate::HostPass`] of the
    /// same layout.
    pub fn for_layout(gpu: &GpuContext, layout: TargetLayout) -> Self {
        let target_format = layout.colour;
        let depth_format = layout.drawn_depth(&gpu.device);
        let direction = layout.direction;
        let samples = layout.samples;
        // Assembled, not one file: the two halves that are not about wgpu live
        // with the code they mirror — the air in `tuile-atmosphere`, the mosaic
        // rule beside the table it consumes in `tuile-core::raster`. What is
        // left in `shader.wgsl` is the bindings and the entry points, which is
        // what this crate is actually for.
        //
        // Order matters: WGSL wants a declaration before its use, and the
        // plumbing calls into both fragments.
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tuile ground"),
                source: wgpu::ShaderSource::Wgsl(ground_wgsl(gpu.imagery_slots).into()),
            });
        // The physical atmosphere: the ground reads its tables (group 3),
        // the sky its sky-view besides.
        let ground_air_bgl =
            tuile_atmosphere::gpu::layout(&gpu.device, wgpu::ShaderStages::VERTEX_FRAGMENT, false);
        let sky_air_bgl =
            tuile_atmosphere::gpu::layout(&gpu.device, wgpu::ShaderStages::FRAGMENT, true);
        let sky_state = crate::sky::Sky::new(&gpu.device, &sky_air_bgl);
        let ground_air_bg = sky_state
            .atmosphere
            .bind_group(&gpu.device, &ground_air_bgl, false);
        let sky_air_bg = sky_state
            .atmosphere
            .bind_group(&gpu.device, &sky_air_bgl, true);
        gpu.queue.write_buffer(
            &sky_state.atmosphere.uniform,
            0,
            bytemuck::bytes_of(&tuile_atmosphere::gpu::AtmosphereUniform::zeroed_off()),
        );
        let layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile pipeline"),
                bind_group_layouts: &[
                    Some(&gpu.view_bgl),
                    Some(&gpu.tile_bgl),
                    Some(&gpu.material_bgl),
                    Some(&ground_air_bgl),
                ],
                immediate_size: 0,
            });

        // Three shapes of the same pipeline, differing only in how they meet
        // the depth buffer and what they do to the colour already there.
        #[derive(Clone, Copy, PartialEq)]
        enum Pass {
            /// Writes depth, tests `Less`, replaces the colour. The scene.
            Solid,
            /// Neither writes nor tests depth. What is behind everything.
            Backdrop,
            /// An ancestor standing in for ground that is not its own.
            ///
            /// It **tests** depth and never **writes** it, and it is drawn after
            /// the surfaces that own their ground. Testing is what keeps it on
            /// the near side of the planet: a skirt is a vertical wall, so on the
            /// far side of the globe its outward face still points at the camera
            /// and back-face culling does not remove it. Drawn with `Always` —
            /// which is right for the shell, and was wrong here — such a wall
            /// paints straight through the Earth, and that is exactly what it
            /// looked like: pale ribbons crossing the ocean at angles, brightest
            /// at the limb, because a fragment that far away comes back as pure
            /// haze.
            ///
            /// Not writing is what keeps it from blocking anything: it colours
            /// the ground nothing else reached and leaves the depth buffer as
            /// the surfaces that own their ground left it.
            Fallback,
            /// The same geometry a `Solid` pass already drew, carrying the next
            /// batch of imagery layers.
            ///
            /// `LessOrEqual` rather than `Less`, because the fragments are at
            /// *exactly* the depth the first pass wrote and `Less` would reject
            /// every one of them — the pass would compile, bind, draw, and
            /// change nothing. Alpha blending composes it over what is there,
            /// which is what makes an uncovered fragment keep the pass beneath
            /// it rather than be painted with untextured ground.
            Layers,
            /// A surface that replaces the ground where it is drawn: depth
            /// as `Solid`, and the [`REPLACED`] mark, which the ground's own
            /// passes then keep out of.
            Replacing,
            /// An ancestor standing in for the replacing surface where none
            /// of its own tiles is ready: only where no replacing tile
            /// drew, and then replacing as they do.
            ReplacingFallback,
        }
        /// How a pass treats the ownership mark. See `stencil` below.
        fn stencil_face(kind: Pass) -> wgpu::StencilFaceState {
            // The comparison is `reference OP (stored & read_mask)`.
            let face = |compare, pass_op| wgpu::StencilFaceState {
                compare,
                fail_op: wgpu::StencilOperation::Keep,
                depth_fail_op: wgpu::StencilOperation::Keep,
                pass_op,
            };
            match kind {
                // Owns its ground: stamp the mark — except where a replacing
                // surface drew (read through the REPLACED bit alone, against
                // a reference that does not carry it).
                Pass::Solid | Pass::Layers => face(
                    wgpu::CompareFunction::Equal,
                    wgpu::StencilOperation::Replace,
                ),
                // Borrows it: draw only where nobody stamped anything.
                Pass::Fallback => {
                    face(wgpu::CompareFunction::Greater, wgpu::StencilOperation::Keep)
                }
                // Replaces it: stamp both marks, unconditionally.
                Pass::Replacing => face(
                    wgpu::CompareFunction::Always,
                    wgpu::StencilOperation::Replace,
                ),
                // Stands in for it: only where the replacing surface has
                // not stamped yet, and stamp as it does.
                Pass::ReplacingFallback => face(
                    wgpu::CompareFunction::Greater,
                    wgpu::StencilOperation::Replace,
                ),
                Pass::Backdrop => wgpu::StencilFaceState::IGNORE,
            }
        }
        let make_pipeline = |polygon_mode: wgpu::PolygonMode, kind: Pass| {
            gpu.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("tuile tiles"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[Some(Vertex::LAYOUT)],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        cull_mode: Some(wgpu::Face::Back),
                        polygon_mode,
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: depth_format,
                        // A background neither writes depth nor tests against
                        // it. It is not geometry competing for a place in the
                        // scene — it is what is behind everything, and saying
                        // so is the whole difference between a backstop that
                        // works and one that has to guess how deep to sit.
                        //
                        // Guessing was tried: a shell five hundred metres under
                        // the ellipsoid was shredded by the terrain's own
                        // triangles, which cut chords through the sphere and
                        // dip kilometres below it at coarse levels. There is no
                        // depth at which that stops being true for every level
                        // at once.
                        depth_write_enabled: Some(matches!(
                            kind,
                            Pass::Solid | Pass::Layers | Pass::Replacing | Pass::ReplacingFallback
                        )),
                        depth_compare: Some(match kind {
                            Pass::Backdrop => wgpu::CompareFunction::Always,
                            Pass::Solid
                            | Pass::Fallback
                            | Pass::Replacing
                            | Pass::ReplacingFallback => direction.nearer(),
                            Pass::Layers => direction.nearer_or_level(),
                        }),
                        // **The mark that says who owns this ground.**
                        //
                        // A surface the traversal chose writes 1. A fallback —
                        // an ancestor borrowing ground that is not its own —
                        // draws only where the mark is *not* 1, so it covers
                        // everything nobody owns and can never take a pixel from
                        // a surface that does.
                        //
                        // Depth cannot express that, and the failure is not
                        // subtle: over real relief a coarse tessellation rises
                        // *above* the fine tiles under it, passes `Less`, and
                        // paints over them — measured at 16384 pixels of 16384
                        // in `tests/no_two_surfaces.rs`, and on a globe as sharp
                        // and blurry imagery interleaved in ragged outlines that
                        // follow the terrain. Every comparison that would reject
                        // the fallback there also rejects it over bare ground,
                        // where it is the only thing covering — so the two cases
                        // are not separable by depth at all.
                        //
                        // A stand-in makes it worse and is why this could not
                        // wait: a stand-in *is* `tuile_terrain::upsample` of an
                        // ancestor, so a drawn stand-in and its drawn ancestor
                        // are one surface submitted twice, and the winner is
                        // decided by float equality. Measured in the viewer at
                        // 34 such pairs, stable while nothing was loading.
                        //
                        // `Backdrop` writes nothing and tests nothing: it is
                        // behind everything by construction and must not disturb
                        // the mark.
                        //
                        // A surface that replaces the ground adds a second
                        // bit, [`REPLACED`]: see there.
                        stencil: wgpu::StencilState {
                            front: stencil_face(kind),
                            back: stencil_face(kind),
                            read_mask: match kind {
                                Pass::Solid | Pass::Layers => REPLACED,
                                Pass::Fallback | Pass::ReplacingFallback => OWNED | REPLACED,
                                Pass::Replacing | Pass::Backdrop => 0,
                            },
                            write_mask: match kind {
                                Pass::Solid | Pass::Layers => OWNED,
                                Pass::Replacing | Pass::ReplacingFallback => OWNED | REPLACED,
                                Pass::Backdrop | Pass::Fallback => 0,
                            },
                        },
                        bias: Default::default(),
                    }),
                    multisample: wgpu::MultisampleState {
                        count: samples,
                        ..Default::default()
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_main"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: target_format,
                            blend: (kind == Pass::Layers)
                                .then_some(wgpu::BlendState::ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        };
        let pipeline = make_pipeline(wgpu::PolygonMode::Fill, Pass::Solid);
        let background = make_pipeline(wgpu::PolygonMode::Fill, Pass::Backdrop);
        let layers = make_pipeline(wgpu::PolygonMode::Fill, Pass::Layers);
        let fallback = make_pipeline(wgpu::PolygonMode::Fill, Pass::Fallback);
        let replacing = make_pipeline(wgpu::PolygonMode::Fill, Pass::Replacing);
        let replacing_fallback = make_pipeline(wgpu::PolygonMode::Fill, Pass::ReplacingFallback);
        let wireframe = gpu
            .device
            .features()
            .contains(wgpu::Features::POLYGON_MODE_LINE)
            .then(|| make_pipeline(wgpu::PolygonMode::Line, Pass::Solid));

        let line_shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("lines.wgsl"));
        let line_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile lines"),
                bind_group_layouts: &[Some(&gpu.view_bgl)],
                immediate_size: 0,
            });
        let lines = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("tuile lines"),
                layout: Some(&line_layout),
                vertex: wgpu::VertexState {
                    module: &line_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<LineVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::LineList,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: depth_format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(direction.nearer()),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &line_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(target_format.into())],
                }),
                multiview_mask: None,
                cache: None,
            });

        // The sky: one triangle over the picture, before everything, on the
        // far plane, writing neither depth nor the ownership mark — what is
        // behind the backdrop, as the backdrop is behind the ground.
        let sky_bgl = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tuile sky"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let sky_shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tuile sky"),
                source: wgpu::ShaderSource::Wgsl(sky_wgsl(direction.far()).into()),
            });
        let sky_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile sky"),
                bind_group_layouts: &[Some(&sky_bgl), Some(&sky_air_bgl)],
                immediate_size: 0,
            });
        let sky_pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("tuile sky"),
                layout: Some(&sky_layout),
                vertex: wgpu::VertexState {
                    module: &sky_shader,
                    entry_point: Some("vs_sky"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: depth_format,
                    depth_write_enabled: Some(false),
                    // On the far plane, so level with a cleared depth and
                    // behind anything a host drew first.
                    depth_compare: Some(direction.nearer_or_level()),
                    stencil: wgpu::StencilState {
                        front: wgpu::StencilFaceState::IGNORE,
                        back: wgpu::StencilFaceState::IGNORE,
                        read_mask: 0xff,
                        write_mask: 0,
                    },
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &sky_shader,
                    entry_point: Some("fs_sky"),
                    compilation_options: Default::default(),
                    targets: &[Some(target_format.into())],
                }),
                multiview_mask: None,
                cache: None,
            });
        let sky_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile sky uniform"),
                contents: bytemuck::bytes_of(&SkyUniform::default()),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let sky_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile sky"),
            layout: &sky_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: sky_buf.as_entire_binding(),
            }],
        });

        let view_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile view uniform"),
                contents: bytemuck::bytes_of(&ViewUniform::default()),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let view_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile view"),
            layout: &gpu.view_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: view_buf.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            replacing,
            replacing_fallback,
            background,
            layers,
            fallback,
            wireframe,
            lines,
            sky: sky_state,
            ground_air_bg,
            sky_pipeline,
            sky_buf,
            sky_bg,
            sky_air_bg,
            view_buf,
            view_bg,
            line_buf: None,
        }
    }

    pub fn has_wireframe(&self) -> bool {
        self.wireframe.is_some()
    }

    /// Uploads the per-frame view uniform — and, for the sky, its inverse
    /// projection, worked out here so that no host has to.
    pub fn set_view(&self, queue: &wgpu::Queue, view: &ViewUniform) {
        queue.write_buffer(&self.view_buf, 0, bytemuck::bytes_of(view));
        let inverse = crate::sky::inverse_view_proj(&view.view_proj);
        queue.write_buffer(
            &self.sky_buf,
            0,
            bytemuck::bytes_of(&inverse.to_cols_array()),
        );
    }

    /// The physical atmosphere for this frame, lit at `sky`'s instant — or
    /// none, the default: a scene that is not a planet has no air, and its
    /// ground keeps [`ViewUniform::atmosphere`]'s closed-form haze.
    ///
    /// With one, the sky is drawn past the ground ([`Self::render_sky`]),
    /// and every ground fragment is lit by the sun and the Moon of the
    /// instant where it is, through the air between it and the eye — the
    /// film's own model and code (`tuile_atmosphere::gpu`). Call it every
    /// frame, after [`Self::set_view`]: the light and the sky-view are made
    /// on the GPU every frame, from that frame alone (see `sky.rs`).
    pub fn set_sky(&mut self, gpu: &GpuContext, sky: Option<&crate::sky::SkyFrame>) {
        match sky {
            None => self.sky.off(&gpu.queue),
            Some(frame) => {
                self.sky.update(&gpu.device, &gpu.queue, frame);
                gpu.queue.write_buffer(
                    &self.sky_buf,
                    64,
                    bytemuck::bytes_of(&[crate::sky::pixel_angle(&frame.view), 0.0, 0.0, 0.0]),
                );
            }
        }
    }

    /// The host's atmosphere for this frame: drawn, from its clock at
    /// `wall` (Unix seconds) and `view`, when it is enabled; when it is
    /// not, the path without one — closed-form haze, the clear colour past
    /// the ground. The same as [`Self::set_air`] and [`Self::set_sky`]
    /// together.
    pub fn draw_atmosphere(
        &mut self,
        gpu: &GpuContext,
        atmosphere: &crate::sky::Atmosphere,
        view: &crate::sky::SkyView,
        wall: f64,
    ) {
        if !atmosphere.enabled {
            self.set_sky(gpu, None);
            return;
        }
        self.set_air(atmosphere.params, atmosphere.look);
        self.set_sky(gpu, Some(&atmosphere.frame(*view, wall)));
    }

    /// What the air is made of and how it is drawn: a clear day's and a
    /// film's default look unless a host says otherwise
    /// ([`tuile_atmosphere::AirTuning`], [`tuile_atmosphere::SkyLook::tuned`]).
    pub fn set_air(
        &mut self,
        params: tuile_atmosphere::AtmosphereParams,
        look: tuile_atmosphere::SkyLook,
    ) {
        self.sky.set_air(params, look);
    }

    /// The uniform the physical atmosphere was last drawn with, as the CPU
    /// wrote it: where the sun and the Moon stood, the air's sphere, the
    /// stars. Its light is made on the GPU ([`Self::read_sky_light`]).
    pub fn sky_uniform(&self) -> Option<tuile_atmosphere::gpu::AtmosphereUniform> {
        self.sky
            .is_on()
            .then_some(self.sky.atmosphere.frame_uniform)
    }

    /// The light the physical atmosphere was last drawn with, as the GPU
    /// made it, read back — blocking: for a test, a trace or an instrument,
    /// never a frame's loop. None without an atmosphere.
    pub fn read_sky_light(&self, gpu: &GpuContext) -> Option<SkyLightRead> {
        if !self.sky.is_on() {
            return None;
        }
        let read = self.sky.light.read(&gpu.device, &gpu.queue);
        let exposure = read.extra(tuile_atmosphere::frame_light::Extra::Exposure);
        Some(SkyLightRead {
            uniform: read.over(self.sky.atmosphere.frame_uniform),
            exposure: exposure[0],
            ev100: exposure[1],
        })
    }

    /// How many times the frame's light was worked out and its sky-view
    /// made since this renderer was built: once a frame each.
    pub fn sky_updates(&self) -> (u64, u64) {
        (self.sky.lights, self.sky.views)
    }

    /// Draws the sky: the air on every line of sight, over the whole picture,
    /// on the far plane and writing no depth — so everything drawn after it
    /// covers it, and nothing a host drew before it is covered. Nothing
    /// without a physical atmosphere ([`Self::set_sky`]): the clear colour
    /// stays.
    pub fn render_sky(&self, pass: &mut wgpu::RenderPass<'_>) {
        if !self.sky.is_on() {
            return;
        }
        pass.set_pipeline(&self.sky_pipeline);
        pass.set_bind_group(0, &self.sky_bg, &[]);
        pass.set_bind_group(1, &self.sky_air_bg, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Replaces the debug line set (e.g. bounding volume edges).
    pub fn set_lines(&mut self, gpu: &GpuContext, vertices: &[LineVertex]) {
        if vertices.is_empty() {
            self.line_buf = None;
            return;
        }
        let buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile lines"),
                contents: bytemuck::cast_slice(vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        self.line_buf = Some((buf, vertices.len() as u32));
    }

    /// Draws tiles into the host's pass (the layout's colour and depth). Falls back to fill when wireframe is
    /// requested but unsupported.
    /// Draws a surface **behind** everything, taking no part in depth.
    ///
    /// For the whole-planet shell that guarantees ground is never bare. Drawn
    /// first, it fills the colour buffer where the globe is; every tile drawn
    /// afterwards passes the depth test against a cleared buffer and covers it.
    /// The shell therefore cannot poke through the terrain no matter how coarse
    /// that terrain's triangulation is — which a depth-tested shell did, in
    /// bands of spikes, because a level-2 tile's flat triangles cut a chord
    /// kilometres below the ellipsoid the shell was following.
    /// Draws ancestors standing in for ground that is not their own.
    ///
    /// **After** the surfaces that own their ground, testing depth and never
    /// writing it. Both halves matter and each was got wrong once:
    ///
    /// - drawn with the shell, taking no part in depth, a fallback paints
    ///   through the planet. Not the tile — back-face culling removes that —
    ///   but its **skirt**, which is a vertical wall whose outward face still
    ///   points at the camera from the far side of the globe. On screen: pale
    ///   ribbons crossing the ocean at angles, brightest at the limb.
    /// - writing depth, a fallback blocks the surfaces that own the ground it
    ///   is only borrowing.
    ///
    /// What this does not fix, said plainly: where a coarse ancestor's surface
    /// rises *above* the fine tiles under it — which real relief does — it still
    /// passes `Less` and paints over them. Testing depth cannot express "lose to
    /// that surface but be occluded by it"; only a per-pixel mark can, and that
    /// is a stencil the depth format does not currently carry.
    pub fn render_fallback<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
    ) {
        pass.set_stencil_reference(OWNED);
        pass.set_pipeline(&self.fallback);
        pass.set_bind_group(0, &self.view_bg, &[]);
        pass.set_bind_group(3, &self.ground_air_bg, &[]);
        for tile in tiles {
            pass.set_bind_group(1, &tile.tile_bg, &[]);
            for mesh in &tile.meshes {
                let Some(first) = mesh.material_bgs.first() else {
                    continue;
                };
                pass.set_bind_group(2, first, &[]);
                pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
    }

    pub fn render_background<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
    ) {
        pass.set_stencil_reference(OWNED);
        pass.set_pipeline(&self.background);
        pass.set_bind_group(0, &self.view_bg, &[]);
        pass.set_bind_group(3, &self.ground_air_bg, &[]);
        for tile in tiles {
            pass.set_bind_group(1, &tile.tile_bg, &[]);
            for mesh in &tile.meshes {
                // First pass only. A backdrop neither writes nor tests depth, so
                // a second pass over it would not be composed on top of the
                // first — it would race it, and the winner would be whichever
                // was submitted last. What is behind everything is drawn once.
                let Some(first) = mesh.material_bgs.first() else {
                    continue;
                };
                pass.set_bind_group(2, first, &[]);
                pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
    }

    pub fn render<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
        wireframe: bool,
    ) {
        let (solid, wireframing) = match (wireframe, &self.wireframe) {
            (true, Some(wf)) => (wf, true),
            _ => (&self.pipeline, false),
        };
        // Every first pass, then every later one. Grouping by pipeline rather
        // than by mesh is not an optimisation here, it is the correctness: a
        // later pass has to compose over the *finished* solid surface, and a
        // per-mesh interleaving would compose it over whatever happened to be
        // drawn so far — a neighbouring tile's ground, in the worst case.
        pass.set_stencil_reference(OWNED);
        pass.set_pipeline(solid);
        pass.set_bind_group(0, &self.view_bg, &[]);
        pass.set_bind_group(3, &self.ground_air_bg, &[]);
        let tiles: Vec<&PreparedTile> = tiles.collect();
        for tile in &tiles {
            pass.set_bind_group(1, &tile.tile_bg, &[]);
            for mesh in &tile.meshes {
                let Some(first) = mesh.material_bgs.first() else {
                    continue;
                };
                pass.set_bind_group(2, first, &[]);
                pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
        // A wireframe is not a surface and has no layers to add; drawing them
        // would fill the lines back in.
        if !wireframing && tiles.iter().any(|t| t.needs_more_passes()) {
            pass.set_pipeline(&self.layers);
            for tile in &tiles {
                pass.set_bind_group(1, &tile.tile_bg, &[]);
                for mesh in &tile.meshes {
                    for bg in mesh.material_bgs.iter().skip(1) {
                        pass.set_bind_group(2, bg, &[]);
                        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                    }
                }
            }
        }
        if let Some((buf, count)) = &self.line_buf {
            pass.set_pipeline(&self.lines);
            pass.set_bind_group(0, &self.view_bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }
    }

    /// Draws a surface that replaces the ground where it is drawn: its tiles,
    /// then its stand-ins where none of them reached. Before the ground, so
    /// the ground keeps out of every pixel it took — see `REPLACED`.
    pub fn render_replacing<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
        fallback: impl Iterator<Item = &'t PreparedTile>,
    ) {
        pass.set_stencil_reference(OWNED | REPLACED);
        pass.set_bind_group(0, &self.view_bg, &[]);
        pass.set_bind_group(3, &self.ground_air_bg, &[]);
        for (pipeline, tiles) in [
            (&self.replacing, tiles.collect::<Vec<_>>()),
            (&self.replacing_fallback, fallback.collect::<Vec<_>>()),
        ] {
            pass.set_pipeline(pipeline);
            for tile in tiles {
                pass.set_bind_group(1, &tile.tile_bg, &[]);
                for mesh in &tile.meshes {
                    // A mesh of its own carries its own pictures: one pass.
                    let Some(first) = mesh.material_bgs.first() else {
                        continue;
                    };
                    pass.set_bind_group(2, first, &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
            }
        }
    }

    /// [`TileRenderer::paint`], with a surface that replaces the ground
    /// where it is drawn — a 3D Tiles mesh over a globe — between the
    /// backdrop and the ground: `replacing` its tiles, `replacing_fallback`
    /// the ancestors standing in for those not ready.
    ///
    /// The order is the guarantee once more. Drawn after the backdrop, the
    /// replacing surface covers it; drawn before the ground, it is never
    /// fought for a pixel — the ground keeps out of every pixel it took and
    /// covers every pixel it left, so where it has nothing yet the globe is
    /// there as it would be without it.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_replacing<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        backdrop: impl Iterator<Item = &'t PreparedTile>,
        replacing: impl Iterator<Item = &'t PreparedTile>,
        replacing_fallback: impl Iterator<Item = &'t PreparedTile>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
        fallback: impl Iterator<Item = &'t PreparedTile>,
        overlay: Option<&crate::overlay::OverlayRenderer>,
        wireframe: bool,
    ) {
        self.render_sky(pass);
        self.render_background(pass, backdrop);
        self.render_replacing(pass, replacing, replacing_fallback);
        self.render(pass, tiles, wireframe);
        self.render_fallback(pass, fallback);
        if let Some(overlay) = overlay {
            overlay.render(pass);
        }
    }

    /// The whole scene, in the order it has to go in.
    ///
    /// The order **is** the guarantee, and that is why it lives here instead of
    /// in each host. The backdrop is drawn first and without touching depth, so
    /// the selection always covers it and no frame can show bare ground; the
    /// overlay is drawn last into the same colour attachment, so a control is
    /// always on top and shares no depth with the globe.
    ///
    /// Left to the host, this was a sequence of four calls with the reason for
    /// their order written in a comment beside them — which is exactly the kind
    /// of instruction a second host reimplements in a different order without
    /// ever seeing the comment. `CLAUDE.md` calls black ground the one forbidden
    /// output; this is the smallest shape that makes the ordering part of the
    /// signature rather than part of the folklore.
    ///
    /// `backdrop` is whatever should sit behind the selection — a whole-planet
    /// shell, a complete coarse level, both, or nothing at all. What goes in it
    /// is the host's choice; that it is drawn *first and depthless* is not.
    pub fn paint<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        backdrop: impl Iterator<Item = &'t PreparedTile>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
        fallback: impl Iterator<Item = &'t PreparedTile>,
        overlay: Option<&crate::overlay::OverlayRenderer>,
        wireframe: bool,
    ) {
        // After the tiles that own their ground, so it is occluded by them —
        // see `render_fallback`.
        self.paint_replacing(
            pass,
            backdrop,
            std::iter::empty(),
            std::iter::empty(),
            tiles,
            fallback,
            overlay,
            wireframe,
        );
    }
}

/// The ground shader, assembled from its three parts and sized for `slots`
/// imagery layers.
///
/// `slots` is what the device said it would bind, passed through
/// [`tuile_core::raster::imagery_slots`] — not a number anyone wrote down. It
/// decides three things that must agree exactly, and this is the only place all
/// three are stated: the length of the uniform array, how many textures are
/// declared, and how many are sampled. A shader that declares more than the
/// bind-group layout binds fails validation; one that samples fewer than it
/// declares loses its finest layers silently, at exactly the tiles that straddle
/// worst.
///
/// Public so a test — or a translator aiming at another shading language — can
/// ask for exactly what the backend compiles, rather than reassembling it and
/// hoping the order matches.
pub fn ground_wgsl(slots: u32) -> String {
    let air = format!(
        "{}{}\n{}",
        tuile_atmosphere::hillaire::wgsl_bindings(3),
        tuile_atmosphere::hillaire::WGSL,
        tuile_atmosphere::gpu::SEEN_WGSL
    );
    let mut bindings = String::from("struct ImageryUniform {\n");
    // Two vec4 per slot: coverage, then placement. One array rather than an
    // array of structs, because that is the layout with no padding to reason
    // about — see `tuile_core::raster::imagery_layer_table`, which packs it.
    bindings.push_str(&format!("    layers: array<vec4f, {}>,\n}}\n", 2 * slots));
    bindings.push_str("@group(2) @binding(3) var<uniform> imagery: ImageryUniform;\n");
    let mut samples = String::new();
    for slot in 0..slots {
        bindings.push_str(&format!(
            "@group(2) @binding({}) var img{slot}: texture_2d<f32>;\n",
            crate::context::IMAGERY_BINDING_0 + slot
        ));
        samples.push_str(&format!(
            "    ground = layer(ground, img{slot}, in.uv, {slot}u);\n"
        ));
    }
    format!(
        "{}\n{}\n{air}\n{}",
        tuile_core::raster::WGSL,
        tuile_atmosphere::WGSL,
        include_str!("shader.wgsl")
            .replace(
                "/*STITCH_ROWS_TWICE*/",
                &(2 * crate::prepare::STITCH_ROWS).to_string()
            )
            .replace("/*STITCH_ROWS*/", &crate::prepare::STITCH_ROWS.to_string())
            .replace(
                "/*STITCH_BAND*/",
                &format!("{:?}", tuile_core::stitch::BAND)
            )
            .replace("//#IMAGERY_BINDINGS", &bindings)
            .replace("//#IMAGERY_SAMPLES", samples.trim_start()),
    )
}

/// The sky's own uniform: its inverse projection and a star's size.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SkyUniform {
    inverse_view_proj: [f32; 16],
    stars: [f32; 4],
}

impl Default for SkyUniform {
    fn default() -> Self {
        Self {
            inverse_view_proj: Mat4::IDENTITY.to_cols_array(),
            stars: [0.0; 4],
        }
    }
}

/// The sky shader: the atmosphere's bindings (group 1), its WGSL, then the
/// sky's own pass, drawn at `far_depth` — the far plane of the target's
/// depth convention.
///
/// Public for the same reason as the ground's shader: a test, or a translator,
/// asks for exactly what the backend compiles.
pub fn sky_wgsl(far_depth: f32) -> String {
    format!(
        "{}{}\n{}\n{}",
        tuile_atmosphere::hillaire::wgsl_bindings(1),
        tuile_atmosphere::hillaire::WGSL,
        tuile_atmosphere::gpu::SEEN_WGSL,
        include_str!("sky.wgsl").replace("/*SKY_DEPTH*/", &format!("{far_depth:?}")),
    )
}

/// The light a frame of the physical atmosphere was drawn with, read back
/// from the GPU ([`TileRenderer::read_sky_light`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyLightRead {
    /// The frame's uniform, its light as the GPU made it.
    pub uniform: tuile_atmosphere::gpu::AtmosphereUniform,
    /// The exposure's linear multiplier, every light pre-exposed by it.
    pub exposure: f32,
    /// The EV100 the camera metered.
    pub ev100: f32,
}
