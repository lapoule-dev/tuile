// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The mist pass: a stated field of mist (`tuile_atmosphere::mist`)
//! marched over the picture, after the ground and before the overlays that
//! must stay clear of it.
//!
//! One triangle over the picture, in a pass of its own on the frame's
//! encoder, once the frame's pass has closed: it loads the frame's colour,
//! reads the frame's **depth** — the ground's, so the march stops at the
//! relief — and lays the mist over, premultiplied. It has no depth
//! attachment of its own: nothing is tested, the depth is read as a
//! texture, sample by sample, which is what a multisampled depth allows on
//! WebGPU and the native backends and not on WebGL2. Where the device
//! cannot bind the frame's depth there is no mist pass at all, and the
//! picture is today's (never a hole: the host's shapes and ground are as
//! they were).
//!
//! What the pass needs of the frame is [`crate::FramePass::depth`]: the
//! frame's depth, viewed for binding.

use glam::DVec3;
use tuile_atmosphere::mist::{MistField, MistUniform, MistView};

use crate::context::{GpuContext, SAMPLES};
use crate::world_overlay::Picture;

/// The field as a frame marches it: the host's statement and the frame's
/// view.
#[derive(Debug, Clone, PartialEq)]
pub struct MistFrame<'a> {
    pub field: &'a MistField,
    /// The render origin the frame's `view_proj` is relative to, ECEF.
    pub origin: DVec3,
    /// The eye, ECEF.
    pub eye: DVec3,
    /// The frame's view-projection, relative to `origin`.
    pub view_proj: [f32; 16],
    /// Towards the sun, ECEF.
    pub to_sun: DVec3,
}

/// Samples along a layer's crossing: a veil's gradient is gentle, and the
/// jitter turns what banding is left into grain.
pub const STEPS: u32 = 40;
/// How far a line of sight that meets nothing is marched, metres: well
/// past any layer a film states, and short of the horizon's numbers.
pub const FARTHEST: f64 = 300_000.0;

/// The pass: its pipeline, the field's uniform, and whether there is
/// anything to draw this frame.
pub struct MistRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    on: bool,
}

impl MistRenderer {
    /// Whether `gpu` can read the frame's multisampled depth in a shader —
    /// what the pass needs. False on WebGL2.
    pub fn supported(gpu: &GpuContext) -> bool {
        crate::sky::device_has_compute(&gpu.device)
    }

    /// For a frame drawn in `target_format` at [`SAMPLES`].
    pub fn new(gpu: &GpuContext, target_format: wgpu::TextureFormat) -> Self {
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tuile mist"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
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
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: SAMPLES > 1,
                        },
                        count: None,
                    },
                ],
            });
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tuile mist"),
                source: wgpu::ShaderSource::Wgsl(mist_wgsl().into()),
            });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile mist"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("tuile mist"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_mist"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                // No depth attachment: the depth is read, not tested.
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: SAMPLES,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_mist"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        // Premultiplied: the mist's own light, over what
                        // it lets through.
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let uniform = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tuile mist uniform"),
            size: std::mem::size_of::<MistUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            uniform,
            on: false,
        }
    }

    /// The field for this frame, or none: nothing is drawn until a frame
    /// states one, and a stated field with nothing in it draws nothing.
    pub fn set(&mut self, queue: &wgpu::Queue, frame: Option<&MistFrame<'_>>) {
        let Some(frame) = frame.filter(|f| !f.field.is_empty()) else {
            self.on = false;
            return;
        };
        let view = MistView {
            origin: frame.origin,
            eye: frame.eye,
            inverse_view_proj: crate::sky::inverse_view_proj(&frame.view_proj),
            to_sun: frame.to_sun,
            farthest: FARTHEST,
            steps: STEPS,
        };
        queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&MistUniform::of(frame.field, &view)),
        );
        self.on = true;
    }

    /// Whether this frame draws any mist: what decides whether the HUD's
    /// overlays are drawn after it ([`crate::WorldOverlayRenderer`]).
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Draws the mist over the frame, in a pass of its own on `encoder`.
    ///
    /// `colour` and `resolve` are the frame's own, as the overlays' pass
    /// takes them; `depth` is the frame's depth viewed for binding
    /// ([`crate::FramePass::depth`]); `picture` is where the picture sits,
    /// held to as the frame's pass was. Nothing without a field.
    pub fn render(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        colour: &wgpu::TextureView,
        resolve: Option<&wgpu::TextureView>,
        depth: &wgpu::TextureView,
        picture: Picture,
    ) {
        if !self.on {
            return;
        }
        // Made each frame: the depth is the frame's, which a resize
        // replaces, and a bind group is cheap.
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile mist"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(depth),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tuile mist"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: colour,
                depth_slice: None,
                resolve_target: resolve,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if picture.width != 0 && picture.height != 0 {
            pass.set_viewport(
                picture.x as f32,
                picture.y as f32,
                picture.width as f32,
                picture.height as f32,
                0.0,
                1.0,
            );
            pass.set_scissor_rect(picture.x, picture.y, picture.width, picture.height);
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The pass's shader: the field's uniform (group 0, binding 0), the medium,
/// then the pass itself, reading the frame's depth at binding 1.
///
/// Public for the same reason as the ground's and the sky's: a test, or a
/// translator, asks for exactly what the backend compiles.
pub fn mist_wgsl() -> String {
    let depth = if SAMPLES > 1 {
        "texture_depth_multisampled_2d"
    } else {
        "texture_depth_2d"
    };
    format!(
        "{}{}\n{}",
        tuile_atmosphere::mist::wgsl_bindings(0, 0),
        tuile_atmosphere::mist::WGSL,
        include_str!("mist.wgsl")
            .replace("/*DEPTH_TEXTURE*/", depth)
            .replace("/*FAR_DEPTH*/", &format!("{FAR_DEPTH:?}")),
    )
}

/// The frame's depth, as the pass reads it: the direction of the frame's
/// own depth ([`crate::TargetLayout::own`]), forward, 1 at the far plane.
pub(crate) const FAR_DEPTH: f32 = 1.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// The pass's module, as the backend compiles it, parsed and validated:
    /// a mistake in the WGSL is a failed test, not a dead pass at run time.
    #[test]
    fn the_mist_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(&mist_wgsl()).expect("parses");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator.validate(&module).expect("validates");
        let entries: Vec<&str> = module
            .entry_points
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            entries.contains(&"vs_mist") && entries.contains(&"fs_mist"),
            "{entries:?}"
        );
    }

    #[test]
    fn the_far_depth_is_the_frames_own() {
        assert_eq!(FAR_DEPTH, crate::DepthDirection::Forward.far());
    }
}
