// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile render pipeline. The host owns the surface, the event loop and
//! the render pass; this type owns the pipelines and the view uniform.

use crate::context::{GpuContext, DEPTH_FORMAT};
use crate::prepare::{PreparedTile, Vertex};
use glam::Mat4;
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
}

impl Default for ViewUniform {
    fn default() -> Self {
        Self {
            view_proj: Mat4::IDENTITY.to_cols_array(),
            sun_dir: [0.0, 0.0, -1.0, 0.0],
            params: [0.25, 0.0, 0.0, 0.0],
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

        let make_pipeline = |polygon_mode: wgpu::PolygonMode| {
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
                        depth_write_enabled: Some(true),
                        depth_compare: Some(wgpu::CompareFunction::Less),
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
        let pipeline = make_pipeline(wgpu::PolygonMode::Fill);
        let wireframe = gpu
            .device
            .features()
            .contains(wgpu::Features::POLYGON_MODE_LINE)
            .then(|| make_pipeline(wgpu::PolygonMode::Line));

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
