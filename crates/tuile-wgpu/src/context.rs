// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU context: device, queue, shared bind-group layouts and default
//! resources.
//!
//! The crate never creates a window or a surface — the host owns those
//! (`docs/11-crate-wgpu.md`). [`GpuContext::headless`] exists for tests
//! and offscreen rendering.

use wgpu::util::DeviceExt;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("no compatible GPU adapter: {0}")]
    NoAdapter(String),
    #[error("device request failed: {0}")]
    NoDevice(String),
}

/// Texture format used for all base-color textures.
pub const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// Depth format expected by [`crate::TileRenderer`].
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub(crate) view_bgl: wgpu::BindGroupLayout,
    pub(crate) tile_bgl: wgpu::BindGroupLayout,
    pub(crate) material_bgl: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
    /// 1×1 white texture for untextured materials.
    pub(crate) white_view: wgpu::TextureView,
}

impl GpuContext {
    /// Wraps an existing device/queue (the viewer path: the host created
    /// them against its surface).
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let uniform_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let view_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tuile view"),
            entries: &[uniform_entry(0)],
        });
        let tile_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tuile tile"),
            entries: &[uniform_entry(0)],
        });
        let material_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tuile material"),
            entries: &[
                uniform_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Clamp, never repeat. A tile's texture covers exactly that tile, with
        // UVs spanning the full [0,1] range, so `Repeat` makes the filter wrap
        // at the border and blend the opposite edge in: a hairline of the far
        // side's colour along every tile boundary — the land at the top of a
        // coastal tile bleeding across the sea at its bottom. The seam widens
        // in ground metres at each mip level, so it reads as a lit thread over
        // the whole globe.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tuile sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let white = device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: Some("tuile white"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEXTURE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &[255, 255, 255, 255],
        );
        let white_view = white.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            device,
            queue,
            view_bgl,
            tile_bgl,
            material_bgl,
            sampler,
            white_view,
        }
    }

    /// Creates a context without any surface (tests, offscreen render).
    /// Returns a typed error when the machine has no usable adapter, so
    /// CI without a GPU can skip cleanly.
    pub async fn headless() -> Result<Self, ContextError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .map_err(|e| ContextError::NoAdapter(e.to_string()))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("tuile headless"),
                // Take the adapter's full limits so high-resolution offscreen
                // targets (8K readback buffers, large textures) aren't capped
                // at the conservative defaults (256 MiB / 8192 px).
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| ContextError::NoDevice(e.to_string()))?;
        Ok(Self::new(device, queue))
    }
}
