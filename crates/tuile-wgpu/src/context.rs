// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU context: device, queue, shared bind-group layouts and default
//! resources.
//!
//! The crate never creates a window or a surface — the host owns those
//! (`docs/11-crate-wgpu.md`). [`GpuContext::headless`] exists for tests
//! and offscreen rendering.

use std::sync::{Arc, Mutex};
use tuile_core::raster::{ImageryCoord, ImageryPool, MAX_IMAGERY_LAYERS};
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
/// Where the imagery slots start in the material bind group; slot `i` is at
/// `IMAGERY_BINDING_0 + i`. Must match `shader.wgsl`.
pub(crate) const IMAGERY_BINDING_0: u32 = 4;

/// An imagery texture on the GPU, held by every tile that drapes it.
///
/// The view is what a bind group needs; the texture is kept beside it so that
/// dropping the last tile that references this imagery frees the memory, which
/// is the whole eviction policy — see [`ImageryTextures`].
pub struct GpuImagery {
    pub(crate) view: wgpu::TextureView,
    _texture: wgpu::Texture,
    /// What this cost to upload, mip chain included.
    pub(crate) bytes: usize,
}

impl GpuImagery {
    pub(crate) fn new(texture: wgpu::Texture, view: wgpu::TextureView, bytes: usize) -> Self {
        Self {
            view,
            _texture: texture,
            bytes,
        }
    }
}

/// Imagery textures shared between the tiles that drape them.
///
/// The sharing itself is [`ImageryPool`], in the core: which imagery a tile may
/// let go of, and when, is a property of the layered model and not of any one
/// renderer — a second backend wants precisely the same lifetimes. What is left
/// here is the only part the pool cannot answer, because only a backend knows
/// it: what a texture costs.
#[derive(Default)]
pub struct ImageryTextures(ImageryPool<GpuImagery>);

impl ImageryTextures {
    /// Live imagery on the GPU: distinct textures, and the bytes they hold.
    ///
    /// Counted once each however many tiles reference them — which is the number
    /// that matters, and the one a per-tile total cannot express.
    pub fn live(&self) -> (usize, usize) {
        let live = self.0.live();
        (live.len(), live.iter().map(|e| e.bytes).sum())
    }
}

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub(crate) view_bgl: wgpu::BindGroupLayout,
    pub(crate) tile_bgl: wgpu::BindGroupLayout,
    pub(crate) material_bgl: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
    /// 1×1 white texture for untextured materials and unused imagery slots.
    pub(crate) white_view: wgpu::TextureView,
    /// Shared imagery, behind a lock because `prepare` takes the context by
    /// shared reference from wherever the host drives it.
    pub imagery: Mutex<ImageryTextures>,
}

impl GpuContext {
    /// The texture for an imagery coord, uploaded once however many tiles drape
    /// it. `upload` runs only on a miss.
    pub(crate) fn shared_imagery(
        &self,
        coord: ImageryCoord,
        upload: impl FnOnce() -> GpuImagery,
    ) -> Arc<GpuImagery> {
        self.imagery
            .lock()
            .expect("imagery textures")
            .0
            .get_or_insert(coord, upload)
    }

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
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        // 0 material uniform, 1 base colour, 2 sampler, 3 imagery uniform, then
        // one binding per imagery slot. Slots a tile does not use are bound to
        // the white texture with an empty coverage rectangle — the shader has no
        // count to test and stays branch-free.
        let mut material_entries = vec![
            uniform_entry(0),
            texture_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            uniform_entry(3),
        ];
        material_entries
            .extend((0..MAX_IMAGERY_LAYERS).map(|i| texture_entry(IMAGERY_BINDING_0 + i)));
        let material_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tuile material"),
            entries: &material_entries,
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
            imagery: Mutex::new(ImageryTextures::default()),
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

#[cfg(test)]
mod tests {
    use super::*;

    const SHADER: &str = include_str!("shader.wgsl");

    /// The shader's slot count is written out by hand, because generating twelve
    /// bindings and twelve blend calls would cost more in readability than the
    /// drift costs here. So the drift is what gets tested: three declarations
    /// have to agree, and a mismatch is silent at runtime — extra slots are
    /// simply never sampled, and the ground quietly loses its finest layers at
    /// exactly the tiles that straddle worst.
    #[test]
    fn the_shader_binds_every_imagery_slot_the_core_allows() {
        let declared = SHADER
            .lines()
            .find_map(|l| l.trim().strip_prefix("const IMAGERY_LAYERS: u32 = "))
            .and_then(|v| v.trim_end_matches(&['u', ';'][..]).parse::<u32>().ok())
            .expect("shader.wgsl declares IMAGERY_LAYERS");
        assert_eq!(
            declared, MAX_IMAGERY_LAYERS,
            "shader.wgsl says {declared} imagery slots, the core allows \
             {MAX_IMAGERY_LAYERS}"
        );

        // Two vec4 per slot, one uniform array.
        assert!(
            SHADER.contains(&format!("array<vec4f, {}>", 2 * MAX_IMAGERY_LAYERS)),
            "the layer table must hold two vec4 per slot"
        );

        // And each slot needs its own texture binding and its own blend call.
        for slot in 0..MAX_IMAGERY_LAYERS {
            let binding = format!(
                "@group(2) @binding({}) var img{slot}: texture_2d<f32>;",
                IMAGERY_BINDING_0 + slot
            );
            assert!(SHADER.contains(&binding), "missing binding: {binding}");
            let blend = format!("blend_layer(ground, img{slot}, in.uv, {slot}u)");
            assert!(
                SHADER.contains(&blend),
                "slot {slot} is bound but never read"
            );
        }
    }

    /// The layout this crate builds has to describe exactly what the shader
    /// declares, or pipeline creation fails at run time rather than here.
    #[test]
    fn the_material_layout_reserves_the_bindings_the_shader_names() {
        // 0 material, 1 base colour, 2 sampler, 3 the layer table, then slots.
        assert_eq!(
            IMAGERY_BINDING_0, 4,
            "the imagery slots start after the layer table"
        );
        assert!(SHADER.contains("@group(2) @binding(3) var<uniform> imagery:"));
        assert!(SHADER.contains("@group(2) @binding(2) var base_samp: sampler;"));
    }
}
