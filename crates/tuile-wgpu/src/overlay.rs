// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Screen-space overlay: draws 2-D triangles given in pixel coordinates over
//! the rendered scene.
//!
//! Deliberately dumb — no texture, no batching beyond one buffer, and depth
//! only ever declared, never used.
//! It exists so on-screen controls (`tuile-ui`) can be drawn by this backend
//! without either crate learning about the other: the widget hands over
//! triangles, this uploads them.

use wgpu::util::DeviceExt;

use crate::context::GpuContext;

/// A 2-D vertex in pixel coordinates with a straight RGBA colour — the layout
/// `tuile_ui::Vertex` produces.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct OverlayVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl OverlayVertex {
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<OverlayVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
    };
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ScreenUniform {
    size: [f32; 4],
}

/// Draws pixel-space triangles over the scene.
pub struct OverlayRenderer {
    pipeline: wgpu::RenderPipeline,
    screen_buf: wgpu::Buffer,
    screen_bg: wgpu::BindGroup,
    /// The current geometry, reallocated only when it outgrows the buffer.
    vertices: Option<(wgpu::Buffer, u32)>,
    capacity: u32,
}

impl OverlayRenderer {
    /// `target_format` is the colour format of the surface this draws onto, and
    /// `depth_format` the format of the depth attachment the host's pass
    /// carries — `None` when it has none.
    ///
    /// A pipeline must declare the same attachments as the pass it runs in,
    /// even when it ignores them: wgpu rejects the draw outright otherwise.
    /// The overlay still behaves as if there were no depth — it never tests and
    /// never writes — so it lands on top whatever the scene left behind.
    pub fn new(
        gpu: &GpuContext,
        target_format: wgpu::TextureFormat,
        depth_format: Option<wgpu::TextureFormat>,
    ) -> Self {
        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("overlay.wgsl"));
        let bgl = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tuile overlay"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile overlay"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("tuile overlay"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[OverlayVertex::LAYOUT],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    // The widget's triangles are not consistently wound, and a
                    // flat overlay has no back to cull.
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                    format,
                    // Present for compatibility only: always passes, never
                    // writes. The overlay is 2-D and owns the last word.
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                        count: crate::context::SAMPLES,
                        ..Default::default()
                    },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        // Premultiplied alpha — the fragment shader emits it.
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });

        let screen_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile overlay screen"),
                contents: bytemuck::bytes_of(&ScreenUniform { size: [1.0; 4] }),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let screen_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile overlay"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_buf.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            screen_buf,
            screen_bg,
            vertices: None,
            capacity: 0,
        }
    }

    /// Replaces the geometry to draw. Pass an empty slice to draw nothing.
    ///
    /// Reuses the buffer while the new geometry fits, so the steady state — a
    /// widget whose triangle count barely changes between frames — allocates
    /// nothing. Growing rounds up, so a slowly growing overlay does not
    /// reallocate every frame.
    pub fn set_geometry(&mut self, gpu: &GpuContext, vertices: &[OverlayVertex], size: (u32, u32)) {
        gpu.queue.write_buffer(
            &self.screen_buf,
            0,
            bytemuck::bytes_of(&ScreenUniform {
                size: [size.0 as f32, size.1 as f32, 0.0, 0.0],
            }),
        );

        let count = vertices.len() as u32;
        if count == 0 {
            self.vertices = None;
            return;
        }
        match &self.vertices {
            Some((buf, _)) if count <= self.capacity => {
                gpu.queue
                    .write_buffer(buf, 0, bytemuck::cast_slice(vertices));
                self.vertices = Some((buf.clone(), count));
            }
            _ => {
                let capacity = count.next_power_of_two();
                let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tuile overlay vertices"),
                    size: u64::from(capacity) * std::mem::size_of::<OverlayVertex>() as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                gpu.queue
                    .write_buffer(&buf, 0, bytemuck::cast_slice(vertices));
                self.capacity = capacity;
                self.vertices = Some((buf, count));
            }
        }
    }

    /// Draws the current geometry into the host's pass. Call after the scene,
    /// into the same colour attachment.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some((buf, count)) = &self.vertices else {
            return;
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.screen_bg, &[]);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..*count, 0..1);
    }
}
