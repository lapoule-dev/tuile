// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Decoded content → GPU resources. This is the crate's implementation of
//! the core's `PrepareRenderResources` contract: the core decodes, this
//! materializes.

use crate::context::{GpuContext, TEXTURE_FORMAT};
use glam::{DVec3, Mat4, Vec3};
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
    pub material_bg: wgpu::BindGroup,
    _material_buf: wgpu::Buffer,
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
    _textures: Vec<wgpu::Texture>,
    /// Approximate GPU memory of this tile, bytes.
    pub gpu_bytes: usize,
}

impl PreparedTile {
    /// Recomputes the model matrix relative to a new render origin and rewrites
    /// the tile uniform — the second half of the anti-jitter protocol for a
    /// MOVING camera: keep the render origin near the eye so the f32 the GPU
    /// sees stays small (sub-meter precise), even at planetary ECEF scale.
    pub fn rebase(&self, queue: &wgpu::Queue, render_origin: DVec3) {
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

    let mut gpu_bytes = 0usize;
    let textures: Vec<(wgpu::Texture, wgpu::TextureView)> = content
        .textures
        .iter()
        .map(|t| upload_texture(gpu, t, &mut gpu_bytes))
        .collect();

    let meshes = content
        .meshes
        .iter()
        .map(|m| prepare_mesh(gpu, m, &textures, &mut gpu_bytes))
        .collect();

    PreparedTile {
        meshes,
        tile_bg,
        tile_buf,
        origin_ecef: content.local_origin_ecef,
        transform_local: content.transform_local,
        _textures: textures.into_iter().map(|(t, _)| t).collect(),
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
fn upload_texture(
    gpu: &GpuContext,
    t: &DecodedTexture,
    gpu_bytes: &mut usize,
) -> (wgpu::Texture, wgpu::TextureView) {
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
        *gpu_bytes += image.rgba8.len();
    }

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

fn prepare_mesh(
    gpu: &GpuContext,
    mesh: &DecodedMesh,
    textures: &[(wgpu::Texture, wgpu::TextureView)],
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

    let material_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile material"),
            contents: bytemuck::cast_slice(&mesh.material.base_color_factor),
            usage: wgpu::BufferUsages::UNIFORM,
        });
    let texture_view = mesh
        .material
        .base_color_texture
        .and_then(|i| textures.get(i))
        .map(|(_, v)| v)
        .unwrap_or(&gpu.white_view);
    let material_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tuile material"),
        layout: &gpu.material_bgl,
        entries: &[
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
        ],
    });

    PreparedMesh {
        vertex_buf,
        index_buf,
        index_count: mesh.indices.len() as u32,
        material_bg,
        _material_buf: material_buf,
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
