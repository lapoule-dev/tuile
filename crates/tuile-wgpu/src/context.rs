// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU context: device, queue, shared bind-group layouts and default
//! resources.
//!
//! The crate never creates a window or a surface — the host owns those
//! (`docs/11-crate-wgpu.md`). [`GpuContext::headless`] exists for tests
//! and offscreen rendering.

use std::sync::{Arc, Mutex};
use tuile_core::raster::{ImageryCoord, ImageryPool};
use wgpu::util::DeviceExt;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("no compatible GPU adapter: {0}")]
    NoAdapter(String),
    #[error("device request failed: {0}")]
    NoDevice(String),
}

/// Texture format used for all base-color textures.
/// How many samples every attachment and every pipeline uses.
///
/// A globe is mostly long, near-horizontal edges — the limb against space, a
/// ridge against the sky — and those are exactly the edges a single sample
/// renders as a staircase. Four samples removes it; eight buys little more on
/// content this smooth and costs another copy of every attachment.
///
/// One number, exported, because a pipeline and its render target must agree:
/// they are created in different crates, and a mismatch is a validation error
/// at the first draw rather than something visible in review.
pub const SAMPLES: u32 = 4;

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
    /// Live imagery by its own level: how many textures, and their bytes.
    ///
    /// The imagery level is not the terrain level a tile sits at, and the gap
    /// between them is exactly the question of whether the ground is as sharp as
    /// the source allows — so the breakdown has to be by the level the pixels
    /// came from, not the level they are drawn on.
    pub fn live_by_level(&self) -> Vec<(u32, usize)> {
        self.0
            .live_with_coords()
            .into_iter()
            .map(|(coord, held)| (coord.level, held.bytes))
            .collect()
    }

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
    /// How many imagery textures one draw may bind, as this device allows.
    ///
    /// Detected here because this is where the device is: the limit is 16 on
    /// the WebGPU baseline and on WebGL2, 128 on Metal, and
    /// [`tuile_core::raster::imagery_slots`] turns that into the number worth
    /// spending. Everything downstream — the bind-group layout above, the
    /// generated shader, the uniform table a tile uploads, and the mosaic the
    /// loader is allowed to ask for — reads it from here rather than agreeing
    /// with a constant.
    pub imagery_slots: u32,
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
        let imagery_slots = tuile_core::raster::imagery_slots(
            device.limits().max_sampled_textures_per_shader_stage,
        );
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
        material_entries.extend((0..imagery_slots).map(|i| texture_entry(IMAGERY_BINDING_0 + i)));
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
            imagery_slots,
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

    /// The assembled source for a given budget, which is what the backend
    /// actually compiles.
    fn ground(slots: u32) -> String {
        crate::renderer::ground_wgsl(slots)
    }

    /// **Every slot the budget allows is declared, and every declared slot is
    /// read — at every budget a device can produce.**
    ///
    /// The three numbers that have to agree are written in three different
    /// syntaxes in the same generated file: the length of the uniform array, a
    /// run of `@binding` declarations, and a run of calls. A mismatch between
    /// the last two is silent at run time — extra slots are simply never
    /// sampled, and the ground quietly loses its finest layers at exactly the
    /// tiles that straddle worst, which is the artefact hardest to attribute.
    ///
    /// This used to be written out by hand against a constant, and needed a test
    /// that read the count back out of the `.wgsl` with a regex. That is what a
    /// value with two homes costs; it has one now.
    #[test]
    fn the_shader_declares_and_reads_exactly_the_budgeted_slots() {
        for slots in [
            tuile_core::raster::MIN_IMAGERY_SLOTS,
            tuile_core::raster::imagery_slots(16),
            tuile_core::raster::USEFUL_IMAGERY_SLOTS,
        ] {
            let shader = ground(slots);
            // Two vec4 per slot, one uniform array.
            assert!(
                shader.contains(&format!("array<vec4f, {}>", 2 * slots)),
                "budget {slots}: the layer table must hold two vec4 per slot"
            );
            for slot in 0..slots {
                let binding = format!(
                    "@group(2) @binding({}) var img{slot}: texture_2d<f32>;",
                    IMAGERY_BINDING_0 + slot
                );
                assert!(
                    shader.contains(&binding),
                    "budget {slots}: missing {binding}"
                );
                let read = format!("layer(ground, img{slot}, in.uv, {slot}u)");
                assert!(
                    shader.contains(&read),
                    "budget {slots}: slot {slot} is declared but never read"
                );
            }
            // And not one past the budget, which would be a texture the
            // bind-group layout never declared.
            assert!(
                !shader.contains(&format!("var img{slots}:")),
                "budget {slots}: declared a slot past the budget"
            );
        }
    }

    /// **The assembled shader carries both fragments, once each.**
    ///
    /// Cheap, and it catches the one way the split can fail silently: an
    /// assembly that forgets a part still compiles as Rust, and only fails when
    /// a pipeline is created — which is to say, on a machine with a GPU, which
    /// is not every machine that runs these tests.
    #[test]
    fn the_assembled_shader_carries_both_fragments() {
        let shader = ground(tuile_core::raster::MIN_IMAGERY_SLOTS);
        for (what, needle) in [
            ("the mosaic rule", "fn blend_layer("),
            ("the air", "fn aerial_perspective("),
            ("the bindings", "@group(0) @binding(0)"),
        ] {
            assert_eq!(
                shader.matches(needle).count(),
                1,
                "{what} should appear exactly once in the assembled shader"
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
        let shader = ground(tuile_core::raster::MIN_IMAGERY_SLOTS);
        assert!(shader.contains("@group(2) @binding(3) var<uniform> imagery:"));
        assert!(shader.contains("@group(2) @binding(2) var base_samp: sampler;"));
    }
}
