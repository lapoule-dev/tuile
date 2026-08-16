// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile render pipeline. The host owns the surface, the event loop and
//! the render pass; this type owns the pipelines and the view uniform.

use crate::context::{GpuContext, DEPTH_FORMAT};
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

pub struct TileRenderer {
    pipeline: wgpu::RenderPipeline,
    /// Draws without touching depth — see [`TileRenderer::render_background`].
    background: wgpu::RenderPipeline,
    wireframe: Option<wgpu::RenderPipeline>,
    lines: wgpu::RenderPipeline,
    view_buf: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    line_buf: Option<(wgpu::Buffer, u32)>,
}

impl TileRenderer {
    /// `target_format` is the color format of the host's surface or
    /// offscreen target. Wireframe is created only when the device has
    /// `POLYGON_MODE_LINE`.
    pub fn new(gpu: &GpuContext, target_format: wgpu::TextureFormat) -> Self {
        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile pipeline"),
                bind_group_layouts: &[
                    Some(&gpu.view_bgl),
                    Some(&gpu.tile_bgl),
                    Some(&gpu.material_bgl),
                ],
                immediate_size: 0,
            });

        let make_pipeline = |polygon_mode: wgpu::PolygonMode, background: bool| {
            gpu.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("tuile tiles"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[Vertex::LAYOUT],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        cull_mode: Some(wgpu::Face::Back),
                        polygon_mode,
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH_FORMAT,
                        // A background neither writes depth nor tests against
                        // it: it is not geometry competing for a place in the
                        // scene, it is what sits behind everything.
                        depth_write_enabled: Some(!background),
                        depth_compare: Some(if background {
                            wgpu::CompareFunction::Always
                        } else {
                            wgpu::CompareFunction::Less
                        }),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_main"),
                        compilation_options: Default::default(),
                        targets: &[Some(target_format.into())],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        };
        let pipeline = make_pipeline(wgpu::PolygonMode::Fill, false);
        let background = make_pipeline(wgpu::PolygonMode::Fill, true);
        let wireframe = gpu
            .device
            .features()
            .contains(wgpu::Features::POLYGON_MODE_LINE)
            .then(|| make_pipeline(wgpu::PolygonMode::Line, false));

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
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<LineVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                    }],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::LineList,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &line_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(target_format.into())],
                }),
                multiview_mask: None,
                cache: None,
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
            background,
            wireframe,
            lines,
            view_buf,
            view_bg,
            line_buf: None,
        }
    }

    pub fn has_wireframe(&self) -> bool {
        self.wireframe.is_some()
    }

    /// Uploads the per-frame view uniform.
    pub fn set_view(&self, queue: &wgpu::Queue, view: &ViewUniform) {
        queue.write_buffer(&self.view_buf, 0, bytemuck::bytes_of(view));
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

    /// Draws tiles into the host's pass (color target = `target_format`,
    /// depth = [`DEPTH_FORMAT`]). Falls back to fill when wireframe is
    /// requested but unsupported.
    /// Draws tiles into the host's pass (color target = `target_format`,
    /// depth = [`DEPTH_FORMAT`]). Falls back to fill when wireframe is
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
    pub fn render_background<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
    ) {
        pass.set_pipeline(&self.background);
        pass.set_bind_group(0, &self.view_bg, &[]);
        for tile in tiles {
            pass.set_bind_group(1, &tile.tile_bg, &[]);
            for mesh in &tile.meshes {
                // First pass only. A backdrop neither writes nor tests depth, so
                // a second pass over it would not be composed on top of the
                // first — it would race it, and the winner would be whichever
                // was submitted last. What is behind everything is drawn once.
                pass.set_bind_group(2, &mesh.material_bg, &[]);
                pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
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
        overlay: Option<&crate::overlay::OverlayRenderer>,
        wireframe: bool,
    ) {
        self.render_background(pass, backdrop);
        self.render(pass, tiles, wireframe);
        if let Some(overlay) = overlay {
            overlay.render(pass);
        }
    }

    pub fn render<'t>(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        tiles: impl Iterator<Item = &'t PreparedTile>,
        wireframe: bool,
    ) {
        let pipeline = match (wireframe, &self.wireframe) {
            (true, Some(wf)) => wf,
            _ => &self.pipeline,
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &self.view_bg, &[]);
        for tile in tiles {
            pass.set_bind_group(1, &tile.tile_bg, &[]);
            for mesh in &tile.meshes {
                pass.set_bind_group(2, &mesh.material_bg, &[]);
                pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
        if let Some((buf, count)) = &self.line_buf {
            pass.set_pipeline(&self.lines);
            pass.set_bind_group(0, &self.view_bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }
    }
}
