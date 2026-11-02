// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Screen-space overlay: draws 2-D triangles given in pixel coordinates over
//! the rendered scene.
//!
//! Deliberately dumb — no batching beyond one buffer, and depth only ever
//! declared, never used.
//! It exists so on-screen controls (`tuile-ui`) can be drawn by this backend
//! without either crate learning about the other: the widget hands over
//! triangles, this uploads them.
//!
//! One texture, for one thing triangles draw badly: a line of text. It comes
//! as a coverage mask ([`OverlayMask`], what `tuile_ui::Label` holds), is
//! uploaded only when it changes, and is drawn as one quad over the
//! triangles, a mask pixel to a screen pixel.

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

/// A coverage mask to draw over the triangles: `width × height` bytes, row
/// by row from the top, each how much of its pixel is inked; its top-left
/// corner at `position` in pixels; inked in the straight RGBA `color`.
#[derive(Debug, Clone, Copy)]
pub struct OverlayMask<'a> {
    pub width: u32,
    pub height: u32,
    pub coverage: &'a [u8],
    pub position: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaskUniform {
    /// Left, top, width, height, in pixels.
    rect: [f32; 4],
    color: [f32; 4],
}

/// The mask on the GPU, and the bytes it was made from, to tell a new line
/// from the same one.
struct MaskDraw {
    size: (u32, u32),
    coverage: Vec<u8>,
    bind_group: wgpu::BindGroup,
}

/// Draws pixel-space triangles over the scene.
pub struct OverlayRenderer {
    pipeline: wgpu::RenderPipeline,
    screen_buf: wgpu::Buffer,
    screen_bg: wgpu::BindGroup,
    /// The current geometry, reallocated only when it outgrows the buffer.
    vertices: Option<(wgpu::Buffer, u32)>,
    capacity: u32,
    mask_pipeline: wgpu::RenderPipeline,
    mask_layout: wgpu::BindGroupLayout,
    mask_uniform: wgpu::Buffer,
    sampler: wgpu::Sampler,
    /// The mask drawn, if any.
    mask: Option<MaskDraw>,
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
                    buffers: &[Some(OverlayVertex::LAYOUT)],
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

        let (mask_pipeline, mask_layout) = mask_pipeline(gpu, &bgl, target_format, depth_format);
        let mask_uniform = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tuile overlay mask"),
            size: std::mem::size_of::<MaskUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // A mask pixel to a screen pixel: nothing to filter, and a
        // non-filtering sampler is what every backend (WebGL2 included)
        // takes for any format.
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tuile overlay mask"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            screen_buf,
            screen_bg,
            vertices: None,
            capacity: 0,
            mask_pipeline,
            mask_layout,
            mask_uniform,
            sampler,
            mask: None,
        }
    }

    /// Replaces the mask drawn over the triangles; `None` draws none.
    ///
    /// The texture is made again only when the mask's pixels change: a line
    /// that stays the same from frame to frame costs a 32-byte write for
    /// where it goes and in which colour.
    pub fn set_mask(&mut self, gpu: &GpuContext, mask: Option<OverlayMask<'_>>) {
        let Some(mask) = mask else {
            self.mask = None;
            return;
        };
        let (w, h) = (mask.width, mask.height);
        if w == 0 || h == 0 || mask.coverage.len() < (w * h) as usize {
            self.mask = None;
            return;
        }
        let coverage = &mask.coverage[..(w * h) as usize];
        gpu.queue.write_buffer(
            &self.mask_uniform,
            0,
            bytemuck::bytes_of(&MaskUniform {
                rect: [
                    mask.position[0].round(),
                    mask.position[1].round(),
                    w as f32,
                    h as f32,
                ],
                color: mask.color,
            }),
        );
        if self
            .mask
            .as_ref()
            .is_some_and(|m| m.size == (w, h) && m.coverage == coverage)
        {
            return;
        }
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tuile overlay mask"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            coverage,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            size,
        );
        let view = texture.create_view(&Default::default());
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile overlay mask"),
            layout: &self.mask_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.mask_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.mask = Some(MaskDraw {
            size: (w, h),
            coverage: coverage.to_vec(),
            bind_group,
        });
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
    /// The mask, if one is set, goes over the triangles.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some((buf, count)) = &self.vertices {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.screen_bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }
        if let Some(mask) = &self.mask {
            pass.set_pipeline(&self.mask_pipeline);
            pass.set_bind_group(0, &self.screen_bg, &[]);
            pass.set_bind_group(1, &mask.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
    }
}

/// The mask's pipeline: one quad made from the vertex index, the coverage
/// sampled a texel to a pixel, blended as the triangles are.
fn mask_pipeline(
    gpu: &GpuContext,
    screen: &wgpu::BindGroupLayout,
    target_format: wgpu::TextureFormat,
    depth_format: Option<wgpu::TextureFormat>,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let shader = gpu
        .device
        .create_shader_module(wgpu::include_wgsl!("overlay_mask.wgsl"));
    let layout = gpu
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tuile overlay mask"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
    let pipeline_layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tuile overlay mask"),
            bind_group_layouts: &[Some(screen), Some(&layout)],
            immediate_size: 0,
        });
    let pipeline = gpu
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tuile overlay mask"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                format,
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
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
    (pipeline, layout)
}
