// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A host's shapes, drawn in the world: depth-tested triangles in Earth
//! coordinates, through the frame's own camera.
//!
//! [`crate::OverlayRenderer`] draws 2-D triangles in pixel coordinates, for
//! on-screen controls. This draws `tuile_overlay::OverlayMesh`: a trail that
//! disappears behind a ridge, a frustum standing in the air, a HUD laid in
//! front of the eye. It is the same geometry a film draws, through the same
//! shared light (`tuile_overlay::gpu::LIGHT_WGSL`), so a host that has a
//! film's overlays sees them in the viewer as the film will make them.
//!
//! # The three depth modes, and why one of them needs a pass of its own
//!
//! [`OverlayDepth`] asks for three different things, and the viewer's single
//! depth attachment — shared with the ground, in the viewer's forward-Z
//! convention — can only answer two of them:
//!
//! * [`OverlayDepth::Terrain`]: the attachment's own test. The ground wrote
//!   its depth there, so `Less` is exactly "hidden where the ground is
//!   nearer".
//! * [`OverlayDepth::Always`]: `Always`, writing nothing. Over the ground
//!   whatever stands before it.
//! * [`OverlayDepth::OverlaysOnly`]: hidden by nearer *overlays* and never by
//!   the ground. The ground's depth is in the way of saying that, so these
//!   meshes are drawn in a **second pass, against a depth buffer of their
//!   own** — the same separation the film makes, where the overlays' depth
//!   attachment holds what opaque overlays wrote and nothing of the ground.
//!   It is what the HUD needs: its own plates stand before one another as
//!   their depths say, and no ridge reaches into it.
//!
//! Within a pass, opaque meshes go first — writing depth, so a mesh tested
//! against overlays is hidden where they are nearer — then translucent ones
//! in the order given, reading depth and writing none.
//!
//! # f64, and what is narrowed where
//!
//! A mesh's `origin_ecef` is f64 and so is the render origin. They are
//! subtracted in f64 and only the difference becomes f32, exactly as a tile's
//! vertices are: a position is then a few kilometres at most, and a
//! centimetre survives it. A mesh whose vertices are stored against the
//! Earth's centre has lost its centimetres before it ever arrives, which is
//! the host's to avoid and is said so in `OverlayMesh`.
//!
//! # Shadows
//!
//! None. `OverlayShadow` is read and ignored: the viewer draws no map of the
//! sun, so a shape neither casts nor receives. The field keeps its meaning
//! for the film.

use crate::context::{GpuContext, DEPTH_FORMAT};
use glam::{DVec3, Mat4, Vec3};
use tuile_overlay::gpu::OverlayVertex;
use tuile_overlay::{OverlayBlend, OverlayDepth, OverlayMesh};
use wgpu::util::DeviceExt;

/// What the shader is told about the frame: the view, and the camera's own
/// axes for the light that goes with it.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct WorldOverlayView {
    /// Eye-relative world to clip, against the same render origin the tiles
    /// were prepared with.
    pub view_proj: [f32; 16],
    /// The camera's right, w unused.
    pub right: [f32; 4],
    /// The camera's up, w unused.
    pub up: [f32; 4],
}

impl Default for WorldOverlayView {
    fn default() -> Self {
        Self {
            view_proj: Mat4::IDENTITY.to_cols_array(),
            right: [1.0, 0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0, 0.0],
        }
    }
}

impl WorldOverlayView {
    /// The view a camera gives, with the axes its light is built on.
    pub fn new(view_proj: Mat4, right: Vec3, up: Vec3) -> Self {
        Self {
            view_proj: view_proj.to_cols_array(),
            right: [right.x, right.y, right.z, 0.0],
            up: [up.x, up.y, up.z, 0.0],
        }
    }
}

/// What is wrong with a batch of meshes, if anything.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorldOverlayError {
    #[error("{0}")]
    Mesh(&'static str),
}

/// One batch to draw: which pipeline, and which indices.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Batch {
    pass: Pass,
    blend: OverlayBlend,
    /// Indices into the frame's one index buffer.
    indices: std::ops::Range<u32>,
    /// The first vertex of the mesh these indices are relative to.
    base: i32,
}

/// Which depth a batch is tested against: the frame's, or the overlays' own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Pass {
    /// Drawn in the frame's pass, against the ground's depth.
    InFrame(InFrame),
    /// Drawn in a pass of its own, against a depth buffer of its own.
    OverlaysOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InFrame {
    /// Hidden where the ground, or a nearer opaque overlay, is.
    Terrain,
    /// Never hidden.
    Always,
}

/// The vertices and triangles of one frame's overlays, rebased and packed.
///
/// Built before anything is recorded, so a faulty mesh is refused with
/// nothing drawn rather than half a frame committed.
#[derive(Debug, Default)]
struct Packed {
    vertices: Vec<OverlayVertex>,
    indices: Vec<u32>,
    batches: Vec<Batch>,
}

impl Packed {
    /// `origin` is the render origin the tiles were prepared against: the
    /// same f64 point the view matrix is relative to.
    fn of(
        meshes: &[OverlayMesh],
        origin: DVec3,
        eye: Option<tuile_overlay::Eye>,
    ) -> Result<Self, WorldOverlayError> {
        let mut packed = Self::default();
        for mesh in meshes {
            if let Some(fault) = mesh.fault() {
                return Err(WorldOverlayError::Mesh(fault));
            }
            if mesh.indices.is_empty() || !mesh.eyes.shows(eye) {
                continue;
            }
            // f64 all the way to here: the origin meets the render origin,
            // and only their difference is narrowed.
            let from_origin = DVec3::from_array(mesh.origin_ecef) - origin;
            let base = i32::try_from(packed.vertices.len()).map_err(|_| {
                WorldOverlayError::Mesh("a frame's overlays have too many vertices")
            })?;
            let flags = tuile_overlay::gpu::flags(mesh);
            let lit = !mesh.normals.is_empty();
            packed
                .vertices
                .extend(
                    mesh.positions
                        .iter()
                        .zip(&mesh.colors)
                        .enumerate()
                        .map(|(i, (p, c))| OverlayVertex {
                            position: (from_origin
                                + DVec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2])))
                            .as_vec3()
                            .to_array(),
                            color: *c,
                            normal: if lit { mesh.normals[i] } else { [0.0; 3] },
                            flags,
                            // The viewer draws no map of the sun.
                            sunk: 0.0,
                        }),
                );
            let first = packed.indices.len() as u32;
            packed.indices.extend_from_slice(&mesh.indices);
            packed.batches.push(Batch {
                pass: match mesh.depth {
                    OverlayDepth::Terrain => Pass::InFrame(InFrame::Terrain),
                    OverlayDepth::Always => Pass::InFrame(InFrame::Always),
                    OverlayDepth::OverlaysOnly => Pass::OverlaysOnly,
                },
                blend: mesh.blend,
                indices: first..packed.indices.len() as u32,
                base,
            });
        }
        // Opaque first — it writes the depth a translucent mesh is tested
        // against — then each kind in the order the host gave. A stable sort,
        // so "the order given" survives it.
        packed
            .batches
            .sort_by_key(|b| (b.blend == OverlayBlend::Translucent, b.pass));
        Ok(packed)
    }
}

/// A buffer only ever made larger: overlays change every frame and their
/// size hardly does.
#[derive(Default)]
struct Growing(Option<wgpu::Buffer>);

impl Growing {
    fn holding(
        &mut self,
        gpu: &GpuContext,
        label: &'static str,
        usage: wgpu::BufferUsages,
        bytes: &[u8],
    ) -> &wgpu::Buffer {
        let buffer = match self.0.take() {
            Some(buffer) if buffer.size() >= bytes.len() as u64 => buffer,
            _ => gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (bytes.len() as u64).next_power_of_two().max(4096),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        };
        gpu.queue.write_buffer(&buffer, 0, bytes);
        self.0.insert(buffer)
    }
}

/// Draws `tuile_overlay::OverlayMesh` in the world, through the frame's
/// camera.
///
/// Vertex and fragment stages only: no storage buffer, no compute, so it runs
/// on the WebGL2 path (`crate::webgl2`).
pub struct WorldOverlayRenderer {
    /// By [`Pass`] and [`OverlayBlend`], since a pipeline is the only place a
    /// depth test and a blend can be stated.
    terrain: [wgpu::RenderPipeline; 2],
    always: [wgpu::RenderPipeline; 2],
    overlays_only: [wgpu::RenderPipeline; 2],
    view_buf: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    vertices: Growing,
    indices: Growing,
    /// This frame's batches, and how much of each buffer holds them.
    packed: Packed,
    /// The overlays' own depth, for [`OverlayDepth::OverlaysOnly`]. Made when
    /// first needed, and remade when the frame changes size.
    own_depth: Option<(wgpu::TextureView, (u32, u32))>,
}

impl WorldOverlayRenderer {
    /// `target_format` is the colour format of the pass this draws into.
    ///
    /// The frame's pass carries [`crate::DEPTH_FORMAT`] with a stencil, which
    /// a pipeline has to declare even when it leaves it alone: wgpu rejects
    /// the draw outright otherwise.
    pub fn new(gpu: &GpuContext, target_format: wgpu::TextureFormat) -> Self {
        // The shared light, then this crate's own stages. One string, because
        // a renderer concatenates the text rather than importing a module —
        // see `tuile_overlay::gpu::LIGHT_WGSL`.
        let source = [
            tuile_overlay::gpu::LIGHT_WGSL,
            include_str!("world_overlay.wgsl"),
        ]
        .concat();
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tuile world overlay"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
        let bgl = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tuile world overlay"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
                label: Some("tuile world overlay"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
        let attributes =
            wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4, 2 => Float32x3, 3 => Uint32];
        let buffers = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<OverlayVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        }];
        let pipeline = |label: &str, compare, write: bool, blend: OverlayBlend| {
            gpu.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &buffers,
                    },
                    primitive: wgpu::PrimitiveState {
                        // A ribbon is seen from both sides, and a host's
                        // triangles are not consistently wound: `OverlayMesh`
                        // says either winding.
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH_FORMAT,
                        depth_write_enabled: Some(write),
                        depth_compare: Some(compare),
                        // The ground's mark is nothing to an overlay: it is
                        // not ground competing for a pixel of it.
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: wgpu::MultisampleState {
                        count: crate::context::SAMPLES,
                        ..Default::default()
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: target_format,
                            // Colours arrive premultiplied: `OverlayMesh`
                            // says so, and the film blends them the same way.
                            blend: Some(match blend {
                                OverlayBlend::Opaque => wgpu::BlendState::REPLACE,
                                OverlayBlend::Translucent => {
                                    wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
                                }
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        };
        // Opaque writes the depth translucent is tested against; translucent
        // reads it and writes none, so it hides nothing behind it.
        let pair = |label: &str, compare| {
            [
                pipeline(label, compare, true, OverlayBlend::Opaque),
                pipeline(label, compare, false, OverlayBlend::Translucent),
            ]
        };
        let view_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile world overlay view"),
                contents: bytemuck::bytes_of(&WorldOverlayView::default()),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let view_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile world overlay view"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: view_buf.as_entire_binding(),
            }],
        });
        Self {
            // Forward-Z, as the whole interactive path is: nearer is less.
            terrain: pair("tuile world overlay (terrain)", wgpu::CompareFunction::Less),
            always: pair(
                "tuile world overlay (always)",
                wgpu::CompareFunction::Always,
            ),
            // Its own depth buffer, cleared to the far plane each frame, so
            // the test is against overlays and nothing else.
            overlays_only: pair(
                "tuile world overlay (overlays only)",
                wgpu::CompareFunction::Less,
            ),
            view_buf,
            view_bg,
            vertices: Growing::default(),
            indices: Growing::default(),
            packed: Packed::default(),
            own_depth: None,
        }
    }

    /// The view this frame's overlays are drawn through. Its `view_proj` must
    /// be relative to the same render origin the meshes are rebased onto.
    pub fn set_view(&self, queue: &wgpu::Queue, view: &WorldOverlayView) {
        queue.write_buffer(&self.view_buf, 0, bytemuck::bytes_of(view));
    }

    /// The meshes to draw, rebased onto `origin` and uploaded.
    ///
    /// Everything that can fail does here, before anything is recorded: a
    /// faulty mesh leaves the frame holding **what it held before**, which is
    /// the previous overlays and never nothing.
    ///
    /// `eye` is which eye of a stereo pair is being drawn, `None` for one eye.
    pub fn set_meshes(
        &mut self,
        gpu: &GpuContext,
        meshes: &[OverlayMesh],
        origin: DVec3,
        eye: Option<tuile_overlay::Eye>,
    ) -> Result<(), WorldOverlayError> {
        let packed = Packed::of(meshes, origin, eye)?;
        if !packed.batches.is_empty() {
            self.vertices.holding(
                gpu,
                "tuile world overlay vertices",
                wgpu::BufferUsages::VERTEX,
                bytemuck::cast_slice(&packed.vertices),
            );
            self.indices.holding(
                gpu,
                "tuile world overlay indices",
                wgpu::BufferUsages::INDEX,
                bytemuck::cast_slice(&packed.indices),
            );
        }
        self.packed = packed;
        Ok(())
    }

    /// Whether this frame has anything to draw at all.
    pub fn is_empty(&self) -> bool {
        self.packed.batches.is_empty()
    }

    /// Whether any mesh asked for [`OverlayDepth::OverlaysOnly`], and so
    /// whether [`Self::render_overlays_only`] has a pass to run.
    pub fn needs_own_depth(&self) -> bool {
        self.packed
            .batches
            .iter()
            .any(|b| b.pass == Pass::OverlaysOnly)
    }

    /// Draws every mesh tested against the frame's own depth — the ground's:
    /// [`OverlayDepth::Terrain`] and [`OverlayDepth::Always`].
    ///
    /// Call it inside the frame's pass, after the ground. What asked for
    /// [`OverlayDepth::OverlaysOnly`] is not drawn here: see
    /// [`Self::render_overlays_only`].
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw(pass, |p| matches!(p, Pass::InFrame(_)));
    }

    /// Draws every mesh tested against overlays alone, in a pass of its own.
    ///
    /// `colour` is the frame's **own** colour attachment — the multisampled
    /// one, with the `resolve` target it resolves into — loaded rather than
    /// cleared: these shapes are laid over the picture the frame's pass left.
    /// Every attachment of a pass must share its sample count, and these
    /// pipelines are built for [`crate::SAMPLES`], so a single-sample view
    /// here is rejected by wgpu outright.
    ///
    /// The depth is this renderer's, cleared to the far plane, so the ground
    /// is nothing to these shapes.
    ///
    /// Nothing happens when no mesh asked for it, and resizing the frame is
    /// simply calling it at the new size.
    pub fn render_overlays_only(
        &mut self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        colour: &wgpu::TextureView,
        resolve: Option<&wgpu::TextureView>,
        size: (u32, u32),
    ) {
        if !self.needs_own_depth() {
            return;
        }
        let size = (size.0.max(1), size.1.max(1));
        let depth = match &self.own_depth {
            Some((view, had)) if *had == size => view,
            _ => {
                let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("tuile world overlay depth"),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: crate::context::SAMPLES,
                    dimension: wgpu::TextureDimension::D2,
                    format: DEPTH_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                &self.own_depth.insert((view, size)).0
            }
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tuile world overlay (overlays only)"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: colour,
                depth_slice: None,
                resolve_target: resolve,
                ops: wgpu::Operations {
                    // The picture is already there. Clearing it here would be
                    // the whole frame gone.
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations {
                    // Forward-Z: the far plane, so every overlay is nearer
                    // than nothing and the ground has no say.
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.draw(&mut pass, |p| p == Pass::OverlaysOnly);
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, wanted: impl Fn(Pass) -> bool) {
        let (Some(vertices), Some(indices)) = (&self.vertices.0, &self.indices.0) else {
            return;
        };
        let mut set = None;
        pass.set_bind_group(0, &self.view_bg, &[]);
        pass.set_vertex_buffer(0, vertices.slice(..));
        pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
        for batch in &self.packed.batches {
            if !wanted(batch.pass) {
                continue;
            }
            let pipelines = match batch.pass {
                Pass::InFrame(InFrame::Terrain) => &self.terrain,
                Pass::InFrame(InFrame::Always) => &self.always,
                Pass::OverlaysOnly => &self.overlays_only,
            };
            let which = (batch.pass, batch.blend);
            if set != Some(which) {
                pass.set_pipeline(
                    &pipelines[usize::from(batch.blend == OverlayBlend::Translucent)],
                );
                set = Some(which);
            }
            pass.draw_indexed(batch.indices.clone(), batch.base, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_overlay::{OverlayEyes, OverlayShadow, MIN_SHADOW_TEXELS};

    fn triangle(origin: [f64; 3], depth: OverlayDepth, blend: OverlayBlend) -> OverlayMesh {
        OverlayMesh {
            origin_ecef: origin,
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            colors: vec![[1.0, 0.0, 0.0, 1.0]; 3],
            normals: Vec::new(),
            indices: vec![0, 1, 2],
            depth,
            blend,
            eyes: OverlayEyes::Both,
            shadow: OverlayShadow::None,
        }
    }

    #[test]
    fn a_meshs_vertices_are_rebased_in_f64_before_they_are_narrowed() {
        // A point 20 cm from a mesh origin, with the origin on the far side
        // of the planet from the render origin: narrowing the absolute
        // position first would lose the 20 cm entirely — an f32 holds about
        // half a metre of resolution at 6378 km.
        let far = [6_378_137.0, 0.0, 0.0];
        let mut mesh = triangle(far, OverlayDepth::Terrain, OverlayBlend::Opaque);
        mesh.positions[1] = [0.2, 0.0, 0.0];
        let origin = DVec3::new(6_378_137.0 - 1_000.0, 0.0, 0.0);
        let packed = Packed::of(&[mesh], origin, None).expect("a sound mesh");
        let x = packed.vertices[1].position[0];
        assert!(
            (f64::from(x) - 1_000.2).abs() < 1e-3,
            "rebased to {x}, which is not 1000.2 m from the render origin"
        );
        // And the 20 cm is still there, distinct from the vertex at 1000 m.
        let zero = packed.vertices[0].position[0];
        assert!(
            (f64::from(x - zero) - 0.2).abs() < 1e-3,
            "the 20 cm between two vertices did not survive: {} m",
            x - zero
        );
    }

    #[test]
    fn opaque_is_drawn_before_translucent_whatever_order_it_arrives_in() {
        let meshes = [
            triangle([0.0; 3], OverlayDepth::Terrain, OverlayBlend::Translucent),
            triangle([0.0; 3], OverlayDepth::Terrain, OverlayBlend::Opaque),
        ];
        let packed = Packed::of(&meshes, DVec3::ZERO, None).expect("sound meshes");
        assert_eq!(packed.batches[0].blend, OverlayBlend::Opaque);
        assert_eq!(packed.batches[1].blend, OverlayBlend::Translucent);
    }

    #[test]
    fn the_order_a_host_gave_survives_within_one_blend() {
        // Two translucent meshes: the second is laid over the first, which is
        // what `OverlayMesh` promises, so the sort has to be stable.
        let mut first = triangle([0.0; 3], OverlayDepth::Always, OverlayBlend::Translucent);
        first.colors = vec![[1.0, 0.0, 0.0, 1.0]; 3];
        let mut second = triangle([0.0; 3], OverlayDepth::Always, OverlayBlend::Translucent);
        second.colors = vec![[0.0, 1.0, 0.0, 1.0]; 3];
        let packed = Packed::of(&[first, second], DVec3::ZERO, None).expect("sound meshes");
        assert_eq!(
            packed.vertices[packed.batches[0].base as usize].color[0],
            1.0
        );
        assert_eq!(
            packed.vertices[packed.batches[1].base as usize].color[1],
            1.0
        );
    }

    #[test]
    fn each_depth_mode_reaches_the_pass_it_needs() {
        let meshes = [
            triangle([0.0; 3], OverlayDepth::Terrain, OverlayBlend::Opaque),
            triangle([0.0; 3], OverlayDepth::Always, OverlayBlend::Opaque),
            triangle([0.0; 3], OverlayDepth::OverlaysOnly, OverlayBlend::Opaque),
        ];
        let packed = Packed::of(&meshes, DVec3::ZERO, None).expect("sound meshes");
        let passes: Vec<Pass> = packed.batches.iter().map(|b| b.pass).collect();
        assert!(passes.contains(&Pass::InFrame(InFrame::Terrain)));
        assert!(passes.contains(&Pass::InFrame(InFrame::Always)));
        assert!(passes.contains(&Pass::OverlaysOnly));
        // And an overlays-only mesh is the only thing that costs a pass.
        let hud = Packed::of(&meshes[2..], DVec3::ZERO, None).expect("a sound mesh");
        assert_eq!(hud.batches.len(), 1);
        let ground = Packed::of(&meshes[..2], DVec3::ZERO, None).expect("sound meshes");
        assert!(ground.batches.iter().all(|b| b.pass != Pass::OverlaysOnly));
    }

    #[test]
    fn terrain_is_the_only_mode_tested_against_the_ground() {
        for (depth, behind) in [
            (OverlayDepth::Terrain, true),
            (OverlayDepth::Always, false),
            (OverlayDepth::OverlaysOnly, false),
        ] {
            let packed = Packed::of(
                &[triangle([0.0; 3], depth, OverlayBlend::Opaque)],
                DVec3::ZERO,
                None,
            )
            .expect("a sound mesh");
            let flags = packed.vertices[0].flags;
            assert_eq!(
                flags & tuile_overlay::gpu::BEHIND_GROUND != 0,
                behind,
                "{depth:?}"
            );
        }
    }

    #[test]
    fn a_faulty_mesh_is_refused_with_nothing_packed() {
        let mut mesh = triangle([0.0; 3], OverlayDepth::Terrain, OverlayBlend::Opaque);
        mesh.indices = vec![0, 1, 9];
        let fault = Packed::of(&[mesh], DVec3::ZERO, None).expect_err("an index past the vertices");
        assert!(matches!(fault, WorldOverlayError::Mesh(_)));
    }

    #[test]
    fn a_mesh_for_the_other_eye_is_not_drawn() {
        let mut mesh = triangle([0.0; 3], OverlayDepth::Terrain, OverlayBlend::Opaque);
        mesh.eyes = OverlayEyes::Left;
        let right = Packed::of(
            &[mesh.clone()],
            DVec3::ZERO,
            Some(tuile_overlay::Eye::Right),
        )
        .expect("a sound mesh");
        assert!(right.batches.is_empty());
        let left = Packed::of(&[mesh.clone()], DVec3::ZERO, Some(tuile_overlay::Eye::Left))
            .expect("a sound mesh");
        assert_eq!(left.batches.len(), 1);
        // A viewer of one eye draws every mesh, whatever it says.
        let one = Packed::of(&[mesh], DVec3::ZERO, None).expect("a sound mesh");
        assert_eq!(one.batches.len(), 1);
    }

    #[test]
    fn a_shadow_is_read_and_left_alone() {
        // Phase A draws no map of the sun. A mesh that casts is still drawn,
        // and `sunk` — which only a sun's map uses — stays zero.
        let mut mesh = triangle([0.0; 3], OverlayDepth::Terrain, OverlayBlend::Opaque);
        mesh.shadow = OverlayShadow::CastAndReceive;
        let packed = Packed::of(&[mesh], DVec3::ZERO, None).expect("a sound mesh");
        assert_eq!(packed.batches.len(), 1, "a caster is still drawn");
        assert!(packed.vertices.iter().all(|v| v.sunk == 0.0));
        let _ = MIN_SHADOW_TEXELS;
    }

    #[test]
    fn an_empty_batch_leaves_the_buffers_alone() {
        let packed = Packed::of(&[], DVec3::ZERO, None).expect("nothing is sound");
        assert!(packed.batches.is_empty());
        assert!(packed.vertices.is_empty());
    }

    #[test]
    fn a_meshs_normals_are_dropped_when_it_has_none() {
        let flat = triangle([0.0; 3], OverlayDepth::Always, OverlayBlend::Opaque);
        let packed = Packed::of(&[flat], DVec3::ZERO, None).expect("a sound mesh");
        assert!(packed.vertices.iter().all(|v| v.normal == [0.0; 3]));
        assert!(packed
            .vertices
            .iter()
            .all(|v| v.flags & tuile_overlay::gpu::LIT == 0));
    }

    #[test]
    fn the_view_is_what_the_camera_gave() {
        let view = WorldOverlayView::new(Mat4::IDENTITY, Vec3::X, Vec3::Y);
        assert_eq!(view.right, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(view.up, [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(view.view_proj, Mat4::IDENTITY.to_cols_array());
    }
}
