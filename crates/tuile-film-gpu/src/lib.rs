// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Renders a pre-baked film on wgpu.
//!
//! Nothing here is interactive and nothing is decided: the pack already holds
//! every frame's selection, meshes and imagery. So the GPU does as little
//! rasterisation as it can — one pass that records which triangle of which
//! tile covers each pixel — and everything else in compute:
//!
//! 1. **mips** of every entering texture, averaged in linear light;
//! 2. **visibility** raster into `(slot, triangle)` and reverse-Z depth;
//! 3. **binning**: pixels counted, prefix-summed and scattered by tile;
//! 4. **resolve**, one indirect dispatch per visible tile with its geometry
//!    and texture bound: exact barycentrics and UV gradients from each pixel's
//!    own ray, anisotropic sampling, the film's look;
//! 5. **present**: supersampling box filter, exposure, sRGB curve;
//! 6. optionally **I420**, so a native encoder reads half the bytes.
//!
//! Binning is what makes this portable: WebGPU has no bindless textures, so
//! the resolve cannot pick a tile's texture per pixel — but it can run once
//! per tile over exactly that tile's pixels.
//!
//! The device is the caller's. A browser hands in a WebGPU device, a binary a
//! headless one; this crate never creates either.

mod frame;

use std::collections::HashMap;

use tuile_film::{FrameCamera, Look, TileKey};
use wgpu::util::DeviceExt;

pub use frame::{FrameUniform, TileFrame};

/// How many tiles may be resident at once. Fixed because the scan that bins
/// pixels by tile runs over all of them in one workgroup; it matches
/// `MAX_SLOTS` in `bin.wgsl`.
pub const MAX_SLOTS: u32 = 8192;
/// Dynamic uniform offsets must be 256-aligned on every backend.
const SLOT_STRIDE: u64 = 256;

const VIS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// What `present` writes, and what a canvas or an encoder reads.
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const ALBEDO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const ALBEDO_VIEW: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[derive(Debug, thiserror::Error)]
pub enum FilmGpuError {
    #[error("tile {0:?} is drawn but was never entered")]
    NotResident(TileKey),
    #[error("tile {0:?} entered twice")]
    AlreadyResident(TileKey),
    #[error("more than {MAX_SLOTS} tiles resident at once")]
    OutOfSlots,
    #[error("{0} must be a multiple of {1} for I420 output")]
    Unaligned(&'static str, u32),
}

/// What a film is rendered at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    /// Samples per pixel along each axis: 2 renders 4 samples per pixel.
    pub supersample: u32,
    pub look: Look,
}

/// One tile's mesh, as the pack stores it: little-endian bytes, uploaded
/// untouched. Positions are f32 relative to `origin_ecef`.
#[derive(Debug, Clone, Copy)]
pub struct TileMesh<'a> {
    pub origin_ecef: [f64; 3],
    pub positions: &'a [u8],
    pub normals: &'a [u8],
    pub uvs: &'a [u8],
    pub indices: &'a [u8],
    pub index_count: u32,
    pub base_color_factor: [f32; 4],
}

struct Resident {
    slot: u32,
    origin_ecef: [f64; 3],
    flags: u32,
    factor: [f32; 4],
    index_count: u32,
    bind: wgpu::BindGroup,
    // Held for as long as the bind group that names them.
    _buffers: [wgpu::Buffer; 4],
    _texture: Option<wgpu::Texture>,
}

struct Targets {
    vis: wgpu::TextureView,
    depth: wgpu::TextureView,
    out: wgpu::Texture,
    list: wgpu::Buffer,
    planes: wgpu::Buffer,
    count_bg: wgpu::BindGroup,
    scan_bg: wgpu::BindGroup,
    scatter_bg: wgpu::BindGroup,
    resolve_bg: wgpu::BindGroup,
    present_bg: wgpu::BindGroup,
    y_bg: wgpu::BindGroup,
    uv_bg: wgpu::BindGroup,
}

struct Pipelines {
    raster: wgpu::RenderPipeline,
    count: wgpu::ComputePipeline,
    scan: wgpu::ComputePipeline,
    scatter: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    present: wgpu::ComputePipeline,
    mips: wgpu::ComputePipeline,
    compose: wgpu::ComputePipeline,
    y_plane: wgpu::ComputePipeline,
    uv_planes: wgpu::ComputePipeline,
}

/// The film renderer: resident tiles, targets and pipelines on one device.
pub struct FilmGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    settings: Settings,
    pipelines: Pipelines,
    tile_bgl: wgpu::BindGroupLayout,
    raster_bg: wgpu::BindGroup,
    slot_bg: wgpu::BindGroup,
    frame_buf: wgpu::Buffer,
    tiles_buf: wgpu::Buffer,
    counts: wgpu::Buffer,
    args: wgpu::Buffer,
    targets: Targets,
    sampler: wgpu::Sampler,
    white: wgpu::TextureView,
    empty: wgpu::Buffer,
    tiles: Vec<TileFrame>,
    resident: HashMap<TileKey, Resident>,
    free: Vec<u32>,
    pending_mips: Vec<wgpu::Texture>,
    /// Drapes to compose before the next frame is drawn — and before their
    /// mips, which are made from what this writes.
    pending_drapes: Vec<Drape>,
}

/// One imagery tile of a drape and where it lies on the terrain tile.
pub struct DrapeLayer {
    /// The imagery tile, sRGB-encoded as stored, on geographic spacing:
    /// see [`FilmGpu::create_imagery`]. Shared by every tile it lies on.
    pub texture: wgpu::Texture,
    /// The part of the tile's uv it covers: u0, v0, u1, v1.
    pub coverage: [f32; 4],
    /// uv of the imagery tile = uv of the tile × scale + translation.
    pub translation: [f32; 2],
    pub scale: [f32; 2],
}

struct Drape {
    albedo: wgpu::Texture,
    base: [f32; 4],
    layers: Vec<DrapeLayer>,
}

/// What one dispatch of the composition is told: `compose.wgsl`'s `Job`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ComposeJob {
    coverage: [f32; 4],
    translation: [f32; 2],
    scale: [f32; 2],
    base: [f32; 4],
    origin: [u32; 2],
    mode: u32,
    pad: u32,
}

fn shader(device: &wgpu::Device, name: &str, body: &str) -> wgpu::ShaderModule {
    let source = format!("{}\n{body}", include_str!("shaders/common.wgsl"));
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(name),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    })
}

fn compute(
    device: &wgpu::Device,
    module: &wgpu::ShaderModule,
    entry: &str,
    layout: Option<&wgpu::PipelineLayout>,
) -> wgpu::ComputePipeline {
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry),
        layout,
        module,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: None,
    })
}

fn storage(
    binding: u32,
    visibility: wgpu::ShaderStages,
    read_only: bool,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform(
    binding: u32,
    visibility: wgpu::ShaderStages,
    dynamic: bool,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture(binding: u32, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

fn mip_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).max(1).leading_zeros()
}

impl FilmGpu {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, settings: Settings) -> Self {
        let vertex_compute = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::COMPUTE;
        let compute_only = wgpu::ShaderStages::COMPUTE;

        let tile_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film tile"),
            entries: &[
                storage(0, vertex_compute, true),
                storage(1, compute_only, true),
                storage(2, compute_only, true),
                storage(3, vertex_compute, true),
                texture(4, wgpu::TextureSampleType::Float { filterable: true }),
            ],
        });
        let raster_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film raster"),
            entries: &[
                uniform(0, wgpu::ShaderStages::VERTEX, false),
                storage(1, wgpu::ShaderStages::VERTEX, true),
            ],
        });
        let resolve_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film resolve"),
            entries: &[
                uniform(0, compute_only, false),
                storage(1, compute_only, true),
                storage(2, compute_only, true),
                storage(3, compute_only, true),
                storage(4, compute_only, true),
                texture(5, wgpu::TextureSampleType::Uint),
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: compute_only,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: HDR_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: compute_only,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let slot_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film slot"),
            entries: &[uniform(0, compute_only, true)],
        });

        let raster_module = shader(&device, "film raster", include_str!("shaders/raster.wgsl"));
        let raster = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("film visibility"),
            layout: Some(
                &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("film visibility"),
                    bind_group_layouts: &[Some(&raster_bgl), Some(&tile_bgl)],
                    immediate_size: 0,
                }),
            ),
            vertex: wgpu::VertexState {
                module: &raster_module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                // Terrain is seen from both sides near a skirt; the resolve
                // flips the normal of a back face, the raster keeps it.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                // Reverse-Z: nearer is greater.
                depth_compare: Some(wgpu::CompareFunction::Greater),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &raster_module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(VIS_FORMAT.into())],
            }),
            multiview_mask: None,
            cache: None,
        });

        let bin = shader(&device, "film bin", include_str!("shaders/bin.wgsl"));
        let resolve_module = shader(
            &device,
            "film resolve",
            include_str!("shaders/resolve.wgsl"),
        );
        let resolve_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film resolve"),
            bind_group_layouts: &[Some(&resolve_bgl), Some(&tile_bgl), Some(&slot_bgl)],
            immediate_size: 0,
        });
        let present_module = shader(
            &device,
            "film present",
            include_str!("shaders/present.wgsl"),
        );
        let mips_module = shader(&device, "film mips", include_str!("shaders/mips.wgsl"));
        let compose_module = shader(
            &device,
            "film compose",
            include_str!("shaders/compose.wgsl"),
        );
        let i420_module = shader(&device, "film i420", include_str!("shaders/i420.wgsl"));
        let pipelines = Pipelines {
            raster,
            count: compute(&device, &bin, "count", None),
            scan: compute(&device, &bin, "scan", None),
            scatter: compute(&device, &bin, "scatter", None),
            resolve: compute(&device, &resolve_module, "resolve", Some(&resolve_layout)),
            present: compute(&device, &present_module, "present", None),
            mips: compute(&device, &mips_module, "downsample", None),
            compose: compute(&device, &compose_module, "compose", None),
            y_plane: compute(&device, &i420_module, "y_plane", None),
            uv_planes: compute(&device, &i420_module, "uv_planes", None),
        };

        let frame_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film frame"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tiles_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film tiles"),
            size: u64::from(MAX_SLOTS) * std::mem::size_of::<TileFrame>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let per_slot = |label, bytes: u64, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: u64::from(MAX_SLOTS) * bytes,
                usage: wgpu::BufferUsages::STORAGE | usage,
                mapped_at_creation: false,
            })
        };
        let counts = per_slot("film counts", 4, wgpu::BufferUsages::COPY_DST);
        let starts = per_slot("film starts", 4, wgpu::BufferUsages::empty());
        let cursor = per_slot("film cursor", 4, wgpu::BufferUsages::empty());
        let args = per_slot("film args", 12, wgpu::BufferUsages::INDIRECT);

        let slots: Vec<u8> = (0..MAX_SLOTS)
            .flat_map(|s| {
                let mut row = vec![0u8; SLOT_STRIDE as usize];
                row[..4].copy_from_slice(&s.to_le_bytes());
                row
            })
            .collect();
        let slot_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("film slot ids"),
            contents: &slots,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let slot_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film slot"),
            layout: &slot_bgl,
            entries: &[entry(
                0,
                wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &slot_buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(16),
                }),
            )],
        });
        let raster_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film raster"),
            layout: &raster_bgl,
            entries: &[
                entry(0, frame_buf.as_entire_binding()),
                entry(1, tiles_buf.as_entire_binding()),
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("film albedo"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 16,
            ..Default::default()
        });
        let white = device
            .create_texture_with_data(
                &queue,
                &wgpu::TextureDescriptor {
                    label: Some("film white"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: ALBEDO_VIEW,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &[255; 4],
            )
            .create_view(&Default::default());
        // Bindings may not be empty; a tile without normals or UVs binds this.
        let empty = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film empty"),
            size: 16,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        let targets = Self::targets(
            &device,
            &pipelines,
            &resolve_bgl,
            settings,
            &frame_buf,
            &tiles_buf,
            &counts,
            &starts,
            &cursor,
            &args,
            &sampler,
        );

        Self {
            device,
            queue,
            settings,
            pipelines,
            tile_bgl,
            raster_bg,
            slot_bg,
            frame_buf,
            tiles_buf,
            counts,
            args,
            targets,
            sampler,
            white,
            empty,
            tiles: vec![TileFrame::default(); MAX_SLOTS as usize],
            resident: HashMap::new(),
            free: (0..MAX_SLOTS).rev().collect(),
            pending_mips: Vec::new(),
            pending_drapes: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn targets(
        device: &wgpu::Device,
        pipelines: &Pipelines,
        resolve_bgl: &wgpu::BindGroupLayout,
        settings: Settings,
        frame_buf: &wgpu::Buffer,
        tiles_buf: &wgpu::Buffer,
        counts: &wgpu::Buffer,
        starts: &wgpu::Buffer,
        cursor: &wgpu::Buffer,
        args: &wgpu::Buffer,
        sampler: &wgpu::Sampler,
    ) -> Targets {
        let k = settings.supersample.max(1);
        let (w, h) = (settings.width * k, settings.height * k);
        let make = |label, format, usage, width, height| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        use wgpu::TextureUsages as U;
        let vis = make(
            "film vis",
            VIS_FORMAT,
            U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            w,
            h,
        )
        .create_view(&Default::default());
        let depth = make("film depth", DEPTH_FORMAT, U::RENDER_ATTACHMENT, w, h)
            .create_view(&Default::default());
        let hdr = make(
            "film hdr",
            HDR_FORMAT,
            U::STORAGE_BINDING | U::TEXTURE_BINDING,
            w,
            h,
        )
        .create_view(&Default::default());
        let out = make(
            "film out",
            OUTPUT_FORMAT,
            U::STORAGE_BINDING | U::TEXTURE_BINDING | U::COPY_SRC,
            settings.width,
            settings.height,
        );
        let out_view = out.create_view(&Default::default());
        let list = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film pixel list"),
            size: u64::from(w) * u64::from(h) * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let planes = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film i420"),
            size: (u64::from(settings.width) * u64::from(settings.height) * 3 / 2).max(4),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let auto =
            |pipeline: &wgpu::ComputePipeline, label, entries: &[wgpu::BindGroupEntry<'_>]| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(label),
                    layout: &pipeline.get_bind_group_layout(0),
                    entries,
                })
            };
        let vis_res = || wgpu::BindingResource::TextureView(&vis);
        Targets {
            count_bg: auto(
                &pipelines.count,
                "film count",
                &[entry(0, vis_res()), entry(1, counts.as_entire_binding())],
            ),
            scan_bg: auto(
                &pipelines.scan,
                "film scan",
                &[
                    entry(1, counts.as_entire_binding()),
                    entry(2, starts.as_entire_binding()),
                    entry(3, cursor.as_entire_binding()),
                    entry(5, args.as_entire_binding()),
                ],
            ),
            scatter_bg: auto(
                &pipelines.scatter,
                "film scatter",
                &[
                    entry(0, vis_res()),
                    entry(3, cursor.as_entire_binding()),
                    entry(4, list.as_entire_binding()),
                ],
            ),
            resolve_bg: device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("film resolve"),
                layout: resolve_bgl,
                entries: &[
                    entry(0, frame_buf.as_entire_binding()),
                    entry(1, tiles_buf.as_entire_binding()),
                    entry(2, counts.as_entire_binding()),
                    entry(3, starts.as_entire_binding()),
                    entry(4, list.as_entire_binding()),
                    entry(5, vis_res()),
                    entry(6, wgpu::BindingResource::TextureView(&hdr)),
                    entry(7, wgpu::BindingResource::Sampler(sampler)),
                ],
            }),
            present_bg: auto(
                &pipelines.present,
                "film present",
                &[
                    entry(0, frame_buf.as_entire_binding()),
                    entry(1, wgpu::BindingResource::TextureView(&hdr)),
                    entry(2, vis_res()),
                    entry(3, wgpu::BindingResource::TextureView(&out_view)),
                ],
            ),
            y_bg: auto(
                &pipelines.y_plane,
                "film y",
                &[
                    entry(0, wgpu::BindingResource::TextureView(&out_view)),
                    entry(1, planes.as_entire_binding()),
                ],
            ),
            uv_bg: auto(
                &pipelines.uv_planes,
                "film uv",
                &[
                    entry(0, wgpu::BindingResource::TextureView(&out_view)),
                    entry(1, planes.as_entire_binding()),
                ],
            ),
            vis,
            depth,
            out,
            list,
            planes,
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Tiles currently resident on the GPU.
    pub fn resident(&self) -> usize {
        self.resident.len()
    }

    /// The display-resolution picture `render` writes, in [`OUTPUT_FORMAT`].
    pub fn output(&self) -> &wgpu::Texture {
        &self.targets.out
    }

    /// An albedo texture to fill, sized for its full mip chain. Its level 0 is
    /// the caller's to upload — [`Self::write_rgba`], or a browser's
    /// `copy_external_image_to_texture` — and the rest is computed when the
    /// tile it is entered with is first drawn.
    pub fn create_albedo(&self, width: u32, height: u32) -> wgpu::Texture {
        use wgpu::TextureUsages as U;
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("film albedo"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_count(width, height),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ALBEDO_FORMAT,
            // RENDER_ATTACHMENT is what a browser's external-image copy needs;
            // COPY_SRC lets a test read back what was composed into it.
            usage: U::TEXTURE_BINDING
                | U::STORAGE_BINDING
                | U::COPY_DST
                | U::COPY_SRC
                | U::RENDER_ATTACHMENT,
            view_formats: &[ALBEDO_VIEW],
        })
    }

    /// A texture for one imagery tile, to be composed into drapes: a single
    /// level, sRGB-encoded as stored. Fill it with [`Self::write_rgba`].
    pub fn create_imagery(&self, width: u32, height: u32) -> wgpu::Texture {
        use wgpu::TextureUsages as U;
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("film imagery"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ALBEDO_FORMAT,
            usage: U::TEXTURE_BINDING | U::COPY_DST | U::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    }

    /// Composes a drape into level 0 of `albedo`, before the next frame is
    /// drawn: `base` where no layer reaches, then each layer over its
    /// rectangle, in order. The arithmetic is the bake's.
    ///
    /// Every layer must be opaque — an imagery tile is. A stack with a
    /// translucent layer blends, which this pass does not do: compose that
    /// one on the CPU and upload it.
    pub fn compose(&mut self, albedo: &wgpu::Texture, base: [f32; 4], layers: Vec<DrapeLayer>) {
        self.pending_drapes.push(Drape {
            albedo: albedo.clone(),
            base,
            layers,
        });
    }

    /// Records what was queued since the last frame — drapes, then the mips
    /// of every texture entered — onto `encoder`. [`Self::render`] does this
    /// itself; it is public for a caller that wants the textures made without
    /// drawing a frame.
    pub fn record_pending(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.record_drapes(encoder);
        Self::record_mips(
            &self.device,
            &self.pipelines.mips,
            encoder,
            &self.pending_mips,
        );
        self.pending_mips.clear();
    }

    fn record_drapes(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if self.pending_drapes.is_empty() {
            return;
        }
        let layout = self.pipelines.compose.get_bind_group_layout(0);
        // Every dispatch's bind group is made before the pass begins: the
        // pass borrows them for as long as it lives.
        let mut dispatches = Vec::new();
        for drape in self.pending_drapes.drain(..) {
            let (w, h) = (drape.albedo.width(), drape.albedo.height());
            let dst = drape.albedo.create_view(&wgpu::TextureViewDescriptor {
                format: Some(ALBEDO_FORMAT),
                base_mip_level: 0,
                mip_level_count: Some(1),
                ..Default::default()
            });
            let mut dispatch = |job: ComposeJob, src: &wgpu::TextureView, size: [u32; 2]| {
                let uniform = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("film compose job"),
                        contents: bytemuck::bytes_of(&job),
                        usage: wgpu::BufferUsages::UNIFORM,
                    });
                let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("film compose"),
                    layout: &layout,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(src)),
                        entry(1, wgpu::BindingResource::TextureView(&dst)),
                        entry(2, uniform.as_entire_binding()),
                    ],
                });
                dispatches.push((group, size[0].div_ceil(8), size[1].div_ceil(8)));
            };
            let blank = ComposeJob {
                coverage: [0.0; 4],
                translation: [0.0; 2],
                scale: [0.0; 2],
                base: drape.base,
                origin: [0, 0],
                mode: 0,
                pad: 0,
            };
            dispatch(blank, &self.white, [w, h]);
            for layer in &drape.layers {
                // The texels whose centres can fall in the layer's rectangle,
                // generously: the shader decides each one exactly.
                let c = layer.coverage;
                let x0 = ((c[0] * w as f32).floor().max(0.0) as u32).min(w);
                let y0 = ((c[1] * h as f32).floor().max(0.0) as u32).min(h);
                let x1 = ((c[2] * w as f32).ceil().max(0.0) as u32).min(w);
                let y1 = ((c[3] * h as f32).ceil().max(0.0) as u32).min(h);
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                let src = layer
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                dispatch(
                    ComposeJob {
                        coverage: c,
                        translation: layer.translation,
                        scale: layer.scale,
                        origin: [x0, y0],
                        mode: 1,
                        ..blank
                    },
                    &src,
                    [x1 - x0, y1 - y0],
                );
            }
        }
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("film compose"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipelines.compose);
        for (group, x, y) in &dispatches {
            pass.set_bind_group(0, group, &[]);
            pass.dispatch_workgroups(*x, *y, 1);
        }
    }

    /// Uploads tightly packed sRGB-encoded RGBA8 into level 0.
    pub fn write_rgba(&self, texture: &wgpu::Texture, rgba: &[u8]) {
        let size = texture.size();
        self.queue.write_texture(
            texture.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.width * 4),
                rows_per_image: Some(size.height),
            },
            wgpu::Extent3d {
                depth_or_array_layers: 1,
                ..size
            },
        );
    }

    /// Makes a tile resident. Its buffers are created now; it is drawn by any
    /// later `render` whose selection names it.
    pub fn enter(
        &mut self,
        key: TileKey,
        mesh: &TileMesh<'_>,
        albedo: Option<wgpu::Texture>,
    ) -> Result<(), FilmGpuError> {
        if self.resident.contains_key(&key) {
            return Err(FilmGpuError::AlreadyResident(key));
        }
        let slot = self.free.pop().ok_or(FilmGpuError::OutOfSlots)?;
        let buffer = |label, bytes: &[u8]| {
            if bytes.is_empty() {
                None
            } else {
                Some(
                    self.device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some(label),
                            contents: bytes,
                            usage: wgpu::BufferUsages::STORAGE,
                        }),
                )
            }
        };
        let positions = buffer("film positions", mesh.positions);
        let normals = buffer("film normals", mesh.normals);
        let uvs = buffer("film uvs", mesh.uvs);
        let indices = buffer("film indices", mesh.indices);
        let mut flags = 0;
        if normals.is_some() {
            flags |= 1;
        }
        if uvs.is_some() {
            flags |= 2;
        }
        if albedo.is_some() {
            flags |= 4;
        }
        let empty = || self.empty.clone();
        let buffers = [
            positions.unwrap_or_else(empty),
            normals.unwrap_or_else(empty),
            uvs.unwrap_or_else(empty),
            indices.unwrap_or_else(empty),
        ];
        // Decoded by the sampler through an sRGB view, or read as it lies.
        let sampled_as = match self.settings.look.imagery {
            tuile_film::Imagery::Decoded => ALBEDO_VIEW,
            tuile_film::Imagery::AsStored => ALBEDO_FORMAT,
        };
        let view = albedo.as_ref().map(|t| {
            t.create_view(&wgpu::TextureViewDescriptor {
                format: Some(sampled_as),
                // An sRGB view cannot be storage; it is only ever sampled.
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                ..Default::default()
            })
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film tile"),
            layout: &self.tile_bgl,
            entries: &[
                entry(0, buffers[0].as_entire_binding()),
                entry(1, buffers[1].as_entire_binding()),
                entry(2, buffers[2].as_entire_binding()),
                entry(3, buffers[3].as_entire_binding()),
                entry(
                    4,
                    wgpu::BindingResource::TextureView(view.as_ref().unwrap_or(&self.white)),
                ),
            ],
        });
        if let Some(t) = &albedo {
            if t.mip_level_count() > 1 {
                self.pending_mips.push(t.clone());
            }
        }
        self.resident.insert(
            key,
            Resident {
                slot,
                origin_ecef: mesh.origin_ecef,
                flags,
                factor: mesh.base_color_factor,
                index_count: if mesh.indices.is_empty() {
                    0
                } else {
                    mesh.index_count
                },
                bind,
                _buffers: buffers,
                _texture: albedo,
            },
        );
        Ok(())
    }

    /// Releases a tile. Call after the frame that no longer draws it has been
    /// recorded, never before.
    pub fn leave(&mut self, key: &TileKey) {
        if let Some(r) = self.resident.remove(key) {
            self.tiles[r.slot as usize] = TileFrame::default();
            self.free.push(r.slot);
        }
    }

    /// Records one frame into a new encoder: mips of entering textures, the
    /// visibility raster, binning, resolve and present. The picture lands in
    /// [`Self::output`]; the caller submits, after appending a copy to a
    /// canvas or [`Self::encode_i420`] if it wants either.
    pub fn render(
        &mut self,
        camera: &FrameCamera,
        selection: &[TileKey],
    ) -> Result<wgpu::CommandEncoder, FilmGpuError> {
        // What was queued since the last frame comes first: the drapes, then
        // the mips made from them, before anything samples either.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("film frame"),
            });
        self.record_pending(&mut encoder);
        let mut drawn = Vec::with_capacity(selection.len());
        let mut top = 0usize;
        for key in selection {
            let r = self
                .resident
                .get(key)
                .ok_or(FilmGpuError::NotResident(*key))?;
            let offset = camera.offset(r.origin_ecef);
            self.tiles[r.slot as usize] = TileFrame {
                offset: offset.to_array(),
                flags: r.flags,
                factor: r.factor,
            };
            top = top.max(r.slot as usize + 1);
            drawn.push(r);
        }
        let k = self.settings.supersample.max(1);
        let (w, h) = (self.settings.width * k, self.settings.height * k);
        let uniform = FrameUniform::new(camera, &self.settings.look, [w, h, k, 0]);
        self.queue
            .write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&uniform));
        if top > 0 {
            self.queue
                .write_buffer(&self.tiles_buf, 0, bytemuck::cast_slice(&self.tiles[..top]));
        }

        encoder.clear_buffer(&self.counts, 0, None);

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("film visibility"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets.vis,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipelines.raster);
            pass.set_bind_group(0, &self.raster_bg, &[]);
            for r in &drawn {
                pass.set_bind_group(1, &r.bind, &[]);
                pass.draw(0..r.index_count, r.slot..r.slot + 1);
            }
        }

        {
            let groups = (w.div_ceil(8), h.div_ceil(8));
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipelines.count);
            pass.set_bind_group(0, &self.targets.count_bg, &[]);
            pass.dispatch_workgroups(groups.0, groups.1, 1);
            pass.set_pipeline(&self.pipelines.scan);
            pass.set_bind_group(0, &self.targets.scan_bg, &[]);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(&self.pipelines.scatter);
            pass.set_bind_group(0, &self.targets.scatter_bg, &[]);
            pass.dispatch_workgroups(groups.0, groups.1, 1);

            pass.set_pipeline(&self.pipelines.resolve);
            pass.set_bind_group(0, &self.targets.resolve_bg, &[]);
            for r in &drawn {
                pass.set_bind_group(1, &r.bind, &[]);
                pass.set_bind_group(2, &self.slot_bg, &[r.slot * SLOT_STRIDE as u32]);
                pass.dispatch_workgroups_indirect(&self.args, u64::from(r.slot) * 12);
            }

            pass.set_pipeline(&self.pipelines.present);
            pass.set_bind_group(0, &self.targets.present_bg, &[]);
            pass.dispatch_workgroups(
                self.settings.width.div_ceil(8),
                self.settings.height.div_ceil(8),
                1,
            );
        }
        Ok(encoder)
    }

    fn record_mips(
        device: &wgpu::Device,
        pipeline: &wgpu::ComputePipeline,
        encoder: &mut wgpu::CommandEncoder,
        textures: &[wgpu::Texture],
    ) {
        if textures.is_empty() {
            return;
        }
        let layout = pipeline.get_bind_group_layout(0);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("film mips"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        for t in textures {
            for level in 1..t.mip_level_count() {
                let src = t.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(ALBEDO_VIEW),
                    // An sRGB view cannot be storage; it is only ever sampled.
                    usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                    base_mip_level: level - 1,
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let dst = t.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(ALBEDO_FORMAT),
                    base_mip_level: level,
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("film mip"),
                    layout: &layout,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&src)),
                        entry(1, wgpu::BindingResource::TextureView(&dst)),
                    ],
                });
                let w = (t.width() >> level).max(1);
                let h = (t.height() >> level).max(1);
                pass.set_bind_group(0, &bg, &[]);
                pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
            }
        }
    }

    /// Appends the conversion of [`Self::output`] to planar I420 (Y, then U,
    /// then V, tightly packed) into [`Self::i420_planes`].
    pub fn encode_i420(&self, encoder: &mut wgpu::CommandEncoder) -> Result<(), FilmGpuError> {
        let (w, h) = (self.settings.width, self.settings.height);
        if w % 8 != 0 {
            return Err(FilmGpuError::Unaligned("width", 8));
        }
        if h % 2 != 0 {
            return Err(FilmGpuError::Unaligned("height", 2));
        }
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("film i420"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipelines.y_plane);
        pass.set_bind_group(0, &self.targets.y_bg, &[]);
        pass.dispatch_workgroups((w / 4).div_ceil(8), h.div_ceil(8), 1);
        pass.set_pipeline(&self.pipelines.uv_planes);
        pass.set_bind_group(0, &self.targets.uv_bg, &[]);
        pass.dispatch_workgroups((w / 8).div_ceil(8), (h / 2).div_ceil(8), 1);
        Ok(())
    }

    /// The buffer [`Self::encode_i420`] fills: `width × height × 3 / 2` bytes.
    pub fn i420_planes(&self) -> &wgpu::Buffer {
        &self.targets.planes
    }

    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// Bytes of the pixel list, for a caller sizing its memory budget.
    pub fn pixel_list_bytes(&self) -> u64 {
        self.targets.list.size()
    }
}
