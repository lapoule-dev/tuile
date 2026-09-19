// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Decoded content → GPU resources. This is the crate's implementation of
//! the core's `PrepareRenderResources` contract: the core decodes, this
//! materializes.

use crate::context::{GpuContext, GpuImagery, IMAGERY_BINDING_0, TEXTURE_FORMAT};
use glam::{DVec3, Mat4, Vec3};
use std::sync::Arc;
use tuile_core::content::{DecodedMesh, DecodedTexture, DecodedTileContent};
use tuile_core::raster;
use wgpu::util::DeviceExt;

/// Bytes per texel in [`TEXTURE_FORMAT`].
const BYTES_PER_TEXEL: u32 = 4;

/// Interleaved vertex layout: position, normal, uv.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

impl Vertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
    };
}

pub struct PreparedMesh {
    pub vertex_buf: wgpu::Buffer,
    pub index_buf: wgpu::Buffer,
    pub index_count: u32,
    /// One bind group per imagery pass, in the order they must be drawn.
    ///
    /// Never empty: a mesh with no imagery still has one pass, carrying its base
    /// colour and a table of slots that all mask themselves out. The first is
    /// drawn opaque and writes depth; the rest are the same geometry again, with
    /// the next batch of layers, composed by alpha blending — see
    /// [`crate::TileRenderer::render`] and `raster::MAX_IMAGERY_PASSES`.
    pub material_bgs: Vec<wgpu::BindGroup>,
    _material_bufs: Vec<wgpu::Buffer>,
}

/// One batch of imagery layers: as many as a single draw can bind, and the
/// packed table that places them.
struct ImageryPass {
    textures: Vec<Arc<GpuImagery>>,
    table: wgpu::Buffer,
}

/// What one draw of one mesh is told about its material. Mirrors
/// `MaterialUniform` in `shader.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialUniform {
    base_color: [f32; 4],
    /// x = 1 on the first pass, 0 after — the rest is padding the uniform
    /// alignment would cost anyway. Named `flags` rather than `pass` because
    /// WGSL reserves that word, and the two declarations must match.
    flags: [f32; 4],
}

/// GPU-resident tile, ready to draw.
pub struct PreparedTile {
    pub meshes: Vec<PreparedMesh>,
    pub tile_bg: wgpu::BindGroup,
    tile_buf: wgpu::Buffer,
    /// The tile's own rebasing origin (ECEF) and intrinsic transform, kept so
    /// the model matrix can be recomputed against a moving render origin.
    origin_ecef: DVec3,
    transform_local: Mat4,
    /// The render origin this tile's uniform currently holds.
    ///
    /// A `Cell` so a tile can bring itself up to date through a shared
    /// reference, at the moment it is about to be drawn, rather than being
    /// swept along with every other resident tile whether or not anyone will
    /// look at it. See [`Self::rebase`].
    rebased_to: std::cell::Cell<DVec3>,
    _textures: Vec<GpuImagery>,
    /// Imagery this tile drapes, batched into the passes that bind it, held so
    /// the shared textures and their tables outlive the tile. Which tile holds
    /// the last reference is what frees the memory.
    _imagery: Vec<ImageryPass>,
    /// Approximate GPU memory of this tile, bytes — **excluding draped
    /// imagery**, for the reason [`DecodedTileContent::byte_size`] gives: a
    /// texture twenty tiles share is not twenty textures. Ask
    /// `ImageryTextures::live` for that side of the total.
    pub gpu_bytes: usize,
    /// The deepest imagery level draped over this tile, if any — the measure
    /// of how sharp its ground can look. A tile whose sharpest layer sits
    /// several levels above its own is a smear: one texel stretched over the
    /// whole surface. A recorder gates on the gap; a viewer tolerates it for
    /// the frames the drape needs to catch up.
    pub sharpest_imagery_level: Option<u32>,
}

impl PreparedTile {
    /// Whether any mesh here carries more layers than one draw could bind.
    ///
    /// Asked before switching pipelines rather than discovered mesh by mesh: a
    /// scene where nothing needs a second pass must not pay for a pipeline
    /// switch, and most scenes are that scene.
    pub fn needs_more_passes(&self) -> bool {
        self.meshes.iter().any(|m| m.material_bgs.len() > 1)
    }

    /// Recomputes the model matrix relative to a new render origin and rewrites
    /// the tile uniform — the second half of the anti-jitter protocol for a
    /// MOVING camera: keep the render origin near the eye so the f32 the GPU
    /// sees stays small (sub-meter precise), even at planetary ECEF scale.
    pub fn rebase(&self, queue: &wgpu::Queue, render_origin: DVec3) {
        // Already there, to within a metre. The threshold used to live on the
        // caller, which meant it was asked once for the whole resident set: if
        // the eye had moved, *every* tile was rewritten. Asking per tile is what
        // lets a tile nobody is drawing simply not be written.
        //
        // A metre of staleness is invisible. The whole point of the render
        // origin is to keep the f32 the GPU sees small, and a metre out of the
        // tens of kilometres it is allowed to drift costs no precision at all.
        const CLOSE_ENOUGH_METRES: f64 = 1.0;
        if (render_origin - self.rebased_to.get()).length() < CLOSE_ENOUGH_METRES {
            return;
        }
        self.rebased_to.set(render_origin);
        let model =
            tuile_core::geo::rebased_model(self.origin_ecef, self.transform_local, render_origin);
        queue.write_buffer(
            &self.tile_buf,
            0,
            bytemuck::cast_slice(&model.to_cols_array()),
        );
    }
}

/// Uploads a decoded tile. `render_origin` is the f64 world point the view
/// matrix is relative to: the tile's model matrix is the (f32) translation
/// from its own rebasing origin to that render origin — the second half of
/// the anti-jitter protocol.
pub fn prepare(
    gpu: &GpuContext,
    content: &DecodedTileContent,
    render_origin: DVec3,
) -> PreparedTile {
    let model = tuile_core::geo::rebased_model(
        content.local_origin_ecef,
        content.transform_local,
        render_origin,
    );

    let tile_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile tile uniform"),
            contents: bytemuck::cast_slice(&model.to_cols_array()),
            // COPY_DST so the model can be rewritten on rebase (moving camera).
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
    let tile_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tuile tile"),
        layout: &gpu.tile_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: tile_buf.as_entire_binding(),
        }],
    });

    // Textures the tile owns, uploaded for it alone.
    let textures: Vec<GpuImagery> = content
        .textures
        .iter()
        .map(|t| upload_texture(gpu, t))
        .collect();
    let mut gpu_bytes: usize = textures.iter().map(|t| t.bytes).sum();

    // Imagery it merely references: uploaded once per imagery tile, however
    // many geometry tiles name it. This is where the memory win lands, and it
    // is why the upload is keyed by coord rather than by tile.
    //
    // Split into passes rather than truncated. What one draw cannot bind, a
    // second draw over the same geometry carries — see
    // [`raster::MAX_IMAGERY_PASSES`]. Truncating was the old answer and it lost
    // the *sharpest* layers, which is the half of the mosaic worth having.
    let slots = gpu.imagery_slots as usize;
    let passes: Vec<ImageryPass> = content
        .imagery
        .chunks(slots)
        .take(raster::MAX_IMAGERY_PASSES as usize)
        .map(|batch| ImageryPass {
            textures: batch
                .iter()
                .map(|layer| {
                    gpu.shared_imagery(layer.coord, || upload_texture(gpu, &layer.texture))
                })
                .collect(),
            table: gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("tuile imagery layers"),
                    contents: bytemuck::cast_slice(&raster::imagery_layer_table(
                        batch,
                        gpu.imagery_slots,
                    )),
                    usage: wgpu::BufferUsages::UNIFORM,
                }),
        })
        .collect();
    // Content with no imagery at all still needs one pass: it has a base colour
    // to draw, and every slot masks itself out.
    let passes = if passes.is_empty() {
        vec![ImageryPass {
            textures: Vec::new(),
            table: gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("tuile imagery layers"),
                    contents: bytemuck::cast_slice(&raster::imagery_layer_table(
                        &[],
                        gpu.imagery_slots,
                    )),
                    usage: wgpu::BufferUsages::UNIFORM,
                }),
        }]
    } else {
        passes
    };

    let meshes = content
        .meshes
        .iter()
        .map(|m| prepare_mesh(gpu, m, &textures, &passes, &mut gpu_bytes))
        .collect::<Vec<_>>();

    PreparedTile {
        sharpest_imagery_level: content.imagery.iter().map(|l| l.coord.level).max(),
        meshes,
        tile_bg,
        tile_buf,
        origin_ecef: content.local_origin_ecef,
        transform_local: content.transform_local,
        // Built against this origin just above, so it starts up to date — a
        // fresh tile that claimed otherwise would be rewritten on its first
        // frame for nothing.
        rebased_to: std::cell::Cell::new(render_origin),
        _textures: textures,
        _imagery: passes,
        gpu_bytes,
    }
}

/// Uploads a base-color texture with its full mip chain.
///
/// The chain is built on the CPU ([`raster::mip_chain`]) rather than blitted
/// level-by-level on the GPU. Apple's Metal driver leaks memory in proportion
/// to the number of **render passes** created — `AGX::Compiler::compileProgram`
/// under `renderCommandEncoderWithDescriptor`, see gfx-rs/wgpu#8768; Dawn has
/// it too, and no wgpu-side workaround exists. A GPU chain costs one render
/// pass per level per texture: at eight uploads a frame that is ~80 passes a
/// frame, enough to exhaust the driver in minutes and take the machine down
/// with it. Filtering on the CPU costs zero render passes.
fn upload_texture(gpu: &GpuContext, t: &DecodedTexture) -> GpuImagery {
    let mips = raster::mip_chain(t);
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("tuile base color"),
        size: wgpu::Extent3d {
            width: t.width,
            height: t.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1 + mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        // No RENDER_ATTACHMENT: nothing draws into this texture any more.
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    let mut bytes = 0usize;
    for (level, image) in std::iter::once(t).chain(mips.iter()).enumerate() {
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.rgba8,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * BYTES_PER_TEXEL),
                rows_per_image: Some(image.height),
            },
            wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );
        bytes += image.rgba8.len();
    }

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    GpuImagery::new(texture, view, bytes)
}

fn prepare_mesh(
    gpu: &GpuContext,
    mesh: &DecodedMesh,
    textures: &[GpuImagery],
    passes: &[ImageryPass],
    gpu_bytes: &mut usize,
) -> PreparedMesh {
    let normals = match &mesh.normals {
        Some(n) => n.clone(),
        None => compute_normals(&mesh.positions, &mesh.indices),
    };
    let vertices: Vec<Vertex> = mesh
        .positions
        .iter()
        .enumerate()
        .map(|(i, p)| Vertex {
            position: *p,
            normal: normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]),
            uv: mesh
                .uvs
                .as_ref()
                .and_then(|uv| uv.get(i))
                .copied()
                .unwrap_or([0.0, 0.0]),
        })
        .collect();

    let vertex_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
    let index_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile indices"),
            contents: bytemuck::cast_slice(&mesh.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
    *gpu_bytes += vertices.len() * std::mem::size_of::<Vertex>() + mesh.indices.len() * 4;

    let texture_view = mesh
        .material
        .base_color_texture
        .and_then(|i| textures.get(i))
        .map(|t| &t.view)
        .unwrap_or(&gpu.white_view);

    // One bind group per pass. They differ in two things and only two: which
    // batch of layers is bound, and whether the shader is told this is the first
    // pass — which is what decides that a later pass starts from nothing and
    // writes only where its own layers reached.
    let mut material_bufs = Vec::with_capacity(passes.len());
    let mut material_bgs = Vec::with_capacity(passes.len());
    for (index, imagery) in passes.iter().enumerate() {
        let first = f32::from(index == 0);
        let uniform = MaterialUniform {
            base_color: mesh.material.base_color_factor,
            flags: [first, 0.0, 0.0, 0.0],
        };
        let material_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile material"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        // Every slot is bound, always. An unused one reads the 1×1 white texture
        // and is masked out by an empty coverage rectangle, so the shader needs
        // no count and no branch — see `raster::imagery_layer_table`.
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: material_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: imagery.table.as_entire_binding(),
            },
        ];
        entries.extend((0..gpu.imagery_slots).map(|slot| {
            wgpu::BindGroupEntry {
                binding: IMAGERY_BINDING_0 + slot,
                resource: wgpu::BindingResource::TextureView(
                    imagery
                        .textures
                        .get(slot as usize)
                        .map(|t| &t.view)
                        .unwrap_or(&gpu.white_view),
                ),
            }
        }));
        material_bgs.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile material"),
            layout: &gpu.material_bgl,
            entries: &entries,
        }));
        material_bufs.push(material_buf);
    }

    PreparedMesh {
        vertex_buf,
        index_buf,
        index_count: mesh.indices.len() as u32,
        material_bgs,
        _material_bufs: material_bufs,
    }
}

/// Area-weighted vertex normals for meshes that ship without them.
fn compute_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for tri in indices.chunks_exact(3) {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (pa, pb, pc) = (
            Vec3::from(positions[a]),
            Vec3::from(positions[b]),
            Vec3::from(positions[c]),
        );
        let n = (pb - pa).cross(pc - pa);
        acc[a] += n;
        acc[b] += n;
        acc[c] += n;
    }
    acc.into_iter()
        .map(|n| n.normalize_or_zero().to_array())
        .collect()
}
