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
use tuile_core::texels::{
    GpuTextureCaps, ImageryCompression, ImageryEncode, ImageryFormat, ImageryPlan,
};

use crate::compress::{Compressor, EncodeStats};
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
/// Depth **and stencil**, because depth alone cannot say "cover this ground but
/// never win a pixel from the surface that owns it".
///
/// Every comparison that rejects a fallback above an owning surface also
/// rejects it above bare ground, where it is the only thing covering — so the
/// distinction is not expressible as a depth test. A one-bit mark per pixel is,
/// and that needs a format that carries one. See `TileRenderer`'s `Pass`.
///
/// `Depth24PlusStencil8` rather than `Depth32FloatStencil8`: this one is
/// mandatory in WebGPU, so it needs no feature request and no runtime branch,
/// and it can stay a constant. On Metal it maps to `Depth32Float_Stencil8`
/// anyway — Apple silicon has no 24-bit depth — so the precision is unchanged
/// there. Where it really is 24-bit unorm, the camera's own clip planes already
/// bound the near/far ratio; if that is ever not enough, reverse-Z is the fix
/// for precision and it is orthogonal to this.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;
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
    texture: wgpu::Texture,
    /// What this cost to upload, mip chain included.
    pub(crate) bytes: usize,
    /// The format it is held in: RGBA8 or blocks as uploaded, or blocks
    /// once compressed in place.
    pub(crate) format: ImageryFormat,
    /// The block format it may be compressed into in place (CB9): RGBA8
    /// marked for this context's encoder, whole blocks, kept readable.
    /// `None` once compressed, or never compressible.
    pub(crate) compressible: Option<tuile_core::texels::BlockFormat>,
    /// Its compression was tried and refused: it stays as it is.
    pub(crate) refused: std::sync::atomic::AtomicBool,
}

impl GpuImagery {
    pub(crate) fn new(
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        bytes: usize,
        format: ImageryFormat,
    ) -> Self {
        Self {
            view,
            texture,
            bytes,
            format,
            compressible: None,
            refused: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `self`, compressible in place into `format` (CB9).
    pub(crate) fn compressible_into(mut self, format: tuile_core::texels::BlockFormat) -> Self {
        self.compressible = Some(format);
        self
    }

    /// The texture, for a compression in place to read.
    pub(crate) fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Whether it waits to be compressed in place into `format`.
    pub(crate) fn compresses_into(&self, format: tuile_core::texels::BlockFormat) -> bool {
        self.compressible == Some(format)
            && !self.refused.load(std::sync::atomic::Ordering::Relaxed)
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

    /// Live imagery bytes by the format each texture went resident in.
    pub fn live_by_format(&self) -> Vec<(ImageryFormat, usize)> {
        let mut out: Vec<(ImageryFormat, usize)> = Vec::new();
        for held in self.0.live() {
            match out.iter_mut().find(|(f, _)| *f == held.format) {
                Some((_, bytes)) => *bytes += held.bytes,
                None => out.push((held.format, held.bytes)),
            }
        }
        out
    }

    /// Live imagery by the format each texture went resident in: the bytes,
    /// and how many textures hold them.
    pub fn live_textures_by_format(&self) -> Vec<(ImageryFormat, usize, usize)> {
        let mut out: Vec<(ImageryFormat, usize, usize)> = Vec::new();
        for held in self.0.live() {
            match out.iter_mut().find(|(f, _, _)| *f == held.format) {
                Some((_, bytes, count)) => {
                    *bytes += held.bytes;
                    *count += 1;
                }
                None => out.push((held.format, held.bytes, 1)),
            }
        }
        out
    }

    /// Tile `coord` of imagery `source` is held as `with` from now on: its
    /// compressed copy (CB9). The holders of the old one keep it until they
    /// are bound to the new one.
    pub(crate) fn replace(&mut self, source: u32, coord: ImageryCoord, with: &Arc<GpuImagery>) {
        self.0.replace(source, coord, with);
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
    /// How this context holds imagery: the consumer's plan, RGBA8 unless it
    /// chose otherwise. See [`Self::with_imagery_plan`].
    imagery_plan: ImageryPlan,
    /// What the adapter this device came from can do with block-compressed
    /// textures: what a choice made after the device exists is resolved
    /// against ([`Self::set_imagery_compression`]). Nothing until the
    /// consumer says ([`Self::with_texture_caps`]).
    texture_caps: GpuTextureCaps,
    /// The GPU encoder, when the plan encodes on the GPU.
    pub(crate) compressor: Option<Mutex<Compressor>>,
    /// How hard that encoder searches: see [`Self::with_encode_level`].
    encode_level: u32,
    /// What the upload and release path asked of the device: see
    /// [`Self::upload_ops`].
    pub(crate) ops: crate::ops::OpCounters,
    /// The CPU mip chains of what is about to be uploaded, built off the
    /// render thread: see [`crate::chains`].
    pub(crate) chains: crate::chains::Chains,
    /// The frame's uploads, recorded and not yet submitted: see
    /// [`crate::upload`].
    uploads: Mutex<crate::upload::Uploads>,
}

impl GpuContext {
    /// The texture for one tile of one imagery set, uploaded once however many
    /// tiles drape it. `upload` runs only on a miss.
    pub(crate) fn shared_imagery(
        &self,
        source: u32,
        coord: ImageryCoord,
        upload: impl FnOnce() -> GpuImagery,
    ) -> Arc<GpuImagery> {
        self.imagery
            .lock()
            .expect("imagery textures")
            .0
            .get_or_insert_from(source, coord, upload)
    }

    /// Wraps an existing device/queue (the viewer path: the host created
    /// them against its surface).
    ///
    /// The ground shader gets the default ceiling of imagery slots
    /// ([`tuile_core::raster::USEFUL_IMAGERY_SLOTS`]); a host that weighs the
    /// per-fragment cost otherwise uses [`Self::with_slot_ceiling`].
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self::with_slot_ceiling(device, queue, tuile_core::raster::USEFUL_IMAGERY_SLOTS)
    }

    /// [`Self::new`], with the most imagery slots the ground shader may be
    /// built for: the host's own number, raised or lowered as it weighs the
    /// cost of a fetch per slot on every fragment against the sharpness of a
    /// wider mosaic. A phone passes
    /// [`tuile_core::raster::MIN_IMAGERY_SLOTS`]. Never below that floor, and
    /// never above what the device binds ([`tuile_core::raster::imagery_slots_capped`]).
    ///
    /// The loader's `tuile_planetary` budget must be told the same
    /// number (`LayerBudget::set_from_device_capped`), or it asks for drapes
    /// of another width than the shader reads.
    pub fn with_slot_ceiling(device: wgpu::Device, queue: wgpu::Queue, ceiling: u32) -> Self {
        // The physical atmosphere's tables share the ground's fragment
        // stage with the imagery.
        let imagery_slots = tuile_core::raster::imagery_slots_capped(
            device
                .limits()
                .max_sampled_textures_per_shader_stage
                .saturating_sub(tuile_atmosphere::gpu::GROUND_TEXTURES),
            ceiling,
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
            // Ground seen at a grazing angle is foreshortened along the view
            // only: an isotropic filter picks the mip level of the long axis
            // and blurs the short one, which is the whole of an oblique view
            // over a plain. The film's albedo sampler takes 16 already
            // (`tuile-film-gpu`), and the two pictures are compared. A device
            // without anisotropic filtering filters as before.
            anisotropy_clamp: 16,
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
        let uploads = crate::upload::Uploads::new(&device);
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
            imagery_plan: ImageryPlan::RGBA8,
            texture_caps: GpuTextureCaps::default(),
            compressor: None,
            encode_level: crate::compress::DEFAULT_ENCODE_LEVEL,
            ops: crate::ops::OpCounters::default(),
            chains: crate::chains::Chains::default(),
            uploads: Mutex::new(uploads),
        }
    }

    /// Holds imagery as `plan` says: the consumer's
    /// [`ImageryCompression`] resolved against its adapter
    /// ([`crate::plan_for`]), on a device created with the features it named.
    ///
    /// A plan whose format this device was not created to sample falls back
    /// to RGBA8 — the plan is what [`Self::imagery_plan`] then reports, and
    /// what the consumer hands its loader. Off, the default, changes nothing.
    pub fn with_imagery_plan(mut self, plan: ImageryPlan) -> Self {
        self.set_imagery_plan(plan);
        self
    }

    /// What the adapter this device came from can do with block-compressed
    /// textures ([`crate::texture_caps`]): what a choice made under a
    /// running session is resolved against.
    pub fn with_texture_caps(mut self, caps: GpuTextureCaps) -> Self {
        self.texture_caps = caps;
        self
    }

    /// See [`Self::with_texture_caps`].
    pub fn texture_caps(&self) -> GpuTextureCaps {
        self.texture_caps
    }

    /// Holds imagery as `plan` says from now on, under a running session;
    /// returns the plan in force, which is RGBA8 where this device was not
    /// created to sample the plan's format.
    ///
    /// What the GPU already holds stays as it is, and drawn: a texture is
    /// complete and independent of the encoder that made it. Only what is
    /// uploaded from now on follows the new plan — and texels marked for a
    /// format the context no longer encodes upload as RGBA8, never as
    /// nothing. Off lets go of the encoder and its scratch: no encode work,
    /// no scratch bytes. The frame's uploads are submitted first, so no
    /// record still waits on the encoder being let go.
    pub fn set_imagery_plan(&mut self, plan: ImageryPlan) -> ImageryPlan {
        let sampled = crate::compress::plan_features(plan);
        let plan = if self.device.features().contains(sampled) {
            plan
        } else {
            ImageryPlan::RGBA8
        };
        self.flush_uploads();
        let keeps = |c: &Mutex<Compressor>| {
            c.lock()
                .is_ok_and(|c| Some(c.format()) == plan.target.block())
        };
        self.compressor = match (plan.encode, plan.target.block()) {
            (ImageryEncode::Gpu, Some(_)) if self.compressor.as_ref().is_some_and(keeps) => {
                self.compressor.take()
            }
            (ImageryEncode::Gpu, Some(format)) => Some(Mutex::new(Compressor::new(
                &self.device,
                &self.queue,
                format,
                self.encode_level,
            ))),
            _ => None,
        };
        self.imagery_plan = plan;
        plan
    }

    /// [`Self::set_imagery_plan`], from a person's choice resolved against
    /// this context's [`texture_caps`](Self::with_texture_caps): what a
    /// switch in a running session calls.
    pub fn set_imagery_compression(&mut self, choice: ImageryCompression) -> ImageryPlan {
        self.set_imagery_plan(choice.plan(self.texture_caps))
    }

    /// Encodes imagery at `level` ([`crate::DEFAULT_ENCODE_LEVEL`] unless
    /// set, clamped to [`crate::MAX_ENCODE_LEVEL`]): how hard the GPU
    /// encoder searches each block, a trade of GPU time for quality. The
    /// pump paces by the cost of a block at that level, so a frame's encode
    /// share stays the same GPU time. Without an encoder (a plan that does
    /// not encode on the GPU) it is kept for one, and changes nothing.
    pub fn with_encode_level(mut self, level: u32) -> Self {
        let level = level.min(crate::compress::MAX_ENCODE_LEVEL);
        if level != self.encode_level {
            self.encode_level = level;
            if let Some(format) = self.imagery_plan.target.block() {
                if self.compressor.is_some() {
                    self.compressor = Some(Mutex::new(Compressor::new(
                        &self.device,
                        &self.queue,
                        format,
                        level,
                    )));
                }
            }
        }
        self
    }

    /// The encode level this context's encoder searches at: see
    /// [`Self::with_encode_level`].
    pub fn encode_level(&self) -> u32 {
        self.encode_level
    }

    /// How this context holds imagery: what the consumer's loader is told.
    pub fn imagery_plan(&self) -> ImageryPlan {
        self.imagery_plan
    }

    /// What the upload and release path has asked of the device since this
    /// context was made: buffers, textures and bind groups created, writes,
    /// staging, encoders, submits, encodes and readbacks. Two snapshots
    /// subtract into one frame's.
    pub fn upload_ops(&self) -> crate::ops::UploadOps {
        self.ops.snapshot()
    }

    /// `bytes` into `target` at `offset`, in the frame's uploads: lands at
    /// the next [`Self::flush_uploads`].
    pub(crate) fn write_buffer(&self, target: &wgpu::Buffer, offset: u64, bytes: &[u8]) {
        self.uploads.lock().expect("uploads").write_buffer(
            &self.device,
            &self.queue,
            &self.ops,
            target,
            offset,
            bytes,
        );
    }

    /// A buffer holding `bytes`, written in the frame's uploads: what
    /// `create_buffer_init` made, without a staging buffer of its own.
    pub(crate) fn buffer_with(
        &self,
        label: &str,
        usage: wgpu::BufferUsages,
        bytes: &[u8],
    ) -> wgpu::Buffer {
        crate::ops::OpCounters::add(&self.ops.buffers_created, 1);
        // The size `create_buffer_init` gives it: padded to a whole copy
        // unit, none for nothing.
        let size = if bytes.is_empty() {
            0
        } else {
            (bytes.len() as u64).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT)
        };
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if size as usize == bytes.len() {
            self.write_buffer(&buffer, 0, bytes);
        } else if !bytes.is_empty() {
            let mut padded = bytes.to_vec();
            padded.resize(size as usize, 0);
            self.write_buffer(&buffer, 0, &padded);
        }
        buffer
    }

    /// One mip level of `texture`, in the frame's uploads: `rows` rows of
    /// `row_bytes` packed in `bytes`, copied to `extent`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_texture(
        &self,
        texture: &wgpu::Texture,
        mip_level: u32,
        bytes: &[u8],
        row_bytes: u32,
        rows: u32,
        extent: wgpu::Extent3d,
    ) {
        self.uploads.lock().expect("uploads").write_texture(
            &self.device,
            &self.queue,
            &self.ops,
            texture,
            mip_level,
            bytes,
            row_bytes,
            rows,
            extent,
        );
    }

    /// Runs `record` on the frame's encoder: what lands with the frame's
    /// uploads, in order with them.
    pub(crate) fn record<R>(&self, record: impl FnOnce(&mut wgpu::CommandEncoder) -> R) -> R {
        let mut uploads = self.uploads.lock().expect("uploads");
        record(uploads.encoder(&self.device, &self.ops))
    }

    /// Submits everything the frame's uploads recorded: one submit, or
    /// none when nothing was. Then reads the encoder's stopwatch, if a
    /// measured pass went with it.
    pub(crate) fn flush_uploads(&self) {
        let submitted = self
            .uploads
            .lock()
            .expect("uploads")
            .flush(&self.queue, &self.ops);
        if submitted {
            if let Some(compressor) = &self.compressor {
                if let Ok(mut c) = compressor.lock() {
                    c.submitted(&self.ops);
                }
            }
        }
    }

    /// What the GPU encoder has done, when there is one.
    pub fn imagery_encode_stats(&self) -> Option<EncodeStats> {
        self.compressor
            .as_ref()
            .and_then(|c| c.lock().ok().map(|c| c.stats()))
    }

    /// The GPU bytes the encoder's scratch holds, when there is an encoder.
    pub fn imagery_scratch_bytes(&self) -> Option<u64> {
        self.compressor
            .as_ref()
            .and_then(|c| c.lock().ok().map(|c| c.scratch_bytes()))
    }

    /// The GPU encoder's cost of a block, nanoseconds: measured by
    /// timestamps where the device has them, assumed until then. `None`
    /// without an encoder.
    pub fn imagery_ns_per_block(&self) -> Option<f64> {
        self.compressor
            .as_ref()
            .and_then(|c| c.lock().ok().map(|c| c.ns_per_block()))
    }

    /// Creates a context without any surface (tests, offscreen render).
    /// Returns a typed error when the machine has no usable adapter, so
    /// CI without a GPU can skip cleanly.
    pub async fn headless() -> Result<Self, ContextError> {
        Self::headless_with(ImageryCompression::Off).await
    }

    /// [`Self::headless`], holding imagery as `compression` comes to on the
    /// adapter found: the device is created with the features the plan
    /// needs, and only those. Off is exactly [`Self::headless`].
    pub async fn headless_with(compression: ImageryCompression) -> Result<Self, ContextError> {
        Self::headless_on(compression, false).await
    }

    /// [`Self::headless_with`], on a device that can also hold imagery as
    /// any other choice the adapter can: what a consumer that lets a person
    /// change the choice under a running session creates
    /// ([`Self::set_imagery_compression`], [`crate::switchable_features`]).
    pub async fn headless_switchable(
        compression: ImageryCompression,
    ) -> Result<Self, ContextError> {
        Self::headless_on(compression, true).await
    }

    async fn headless_on(
        compression: ImageryCompression,
        switchable: bool,
    ) -> Result<Self, ContextError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
                apply_limit_buckets: false,
            })
            .await
            .map_err(|e| ContextError::NoAdapter(e.to_string()))?;
        let (plan, mut required_features) = crate::compress::plan_for(compression, &adapter);
        if switchable {
            required_features |= switchable_features(&adapter);
        }
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("tuile headless"),
                // Take the adapter's full limits so high-resolution offscreen
                // targets (8K readback buffers, large textures) aren't capped
                // at the conservative defaults (256 MiB / 8192 px).
                required_limits: adapter.limits(),
                required_features,
                ..Default::default()
            })
            .await
            .map_err(|e| ContextError::NoDevice(e.to_string()))?;
        Ok(Self::new(device, queue)
            .with_texture_caps(crate::compress::texture_caps(&adapter))
            .with_imagery_plan(plan))
    }
}

/// The features a device needs on top of its plan's for every other choice
/// of imagery compression the adapter can hold to be switched to under a
/// running session: each block format it samples, and timestamps for the
/// encoder's pace. Requesting them costs nothing until a texture uses them.
pub fn switchable_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    let wanted = wgpu::Features::TEXTURE_COMPRESSION_BC
        | wgpu::Features::TEXTURE_COMPRESSION_ASTC
        | wgpu::Features::TIMESTAMP_QUERY;
    adapter.features() & wanted
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
