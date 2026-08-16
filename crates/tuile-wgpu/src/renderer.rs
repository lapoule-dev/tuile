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
    /// Redraws geometry the main pipeline already drew, to spend imagery layers
    /// one draw could not bind. See [`TileRenderer::render`].
    layers: wgpu::RenderPipeline,
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

        // Three shapes of the same pipeline, differing only in how they meet
        // the depth buffer and what they do to the colour already there.
        #[derive(Clone, Copy, PartialEq)]
        enum Pass {
            /// Writes depth, tests `Less`, replaces the colour. The scene.
            Solid,
            /// Neither writes nor tests depth. What is behind everything.
            Backdrop,
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
        }
        let make_pipeline = |polygon_mode: wgpu::PolygonMode, kind: Pass| {
            let background = kind == Pass::Backdrop;
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
                        depth_write_enabled: Some(!background),
                        depth_compare: Some(match kind {
                            Pass::Backdrop => wgpu::CompareFunction::Always,
                            Pass::Solid => wgpu::CompareFunction::Less,
                            Pass::Layers => wgpu::CompareFunction::LessEqual,
                        }),
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
                multisample: wgpu::MultisampleState {
                        count: crate::context::SAMPLES,
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
            layers,
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
        pass.set_pipeline(solid);
        pass.set_bind_group(0, &self.view_bg, &[]);
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
                        pass.set_index_buffer(
                            mesh.index_buf.slice(..),
                            wgpu::IndexFormat::Uint32,
                        );
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
        "{}\n{}\n{}",
        tuile_core::raster::WGSL,
        tuile_atmosphere::WGSL,
        include_str!("shader.wgsl")
            .replace("//#IMAGERY_BINDINGS", &bindings)
            .replace("//#IMAGERY_SAMPLES", samples.trim_start()),
    )
}
