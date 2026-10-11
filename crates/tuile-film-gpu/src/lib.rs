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
//! Two things the look may ask for, and by default does not. **Shadows**
//! ([`tuile_film::Look::shadow`]): one more raster, of depth alone, of the
//! frame's tiles as the sun sees them, which the resolve reads to take the
//! sun — and only the sun — from ground that other ground stands before.
//! **Air** ([`tuile_film::Look::haze`]): what a ray crosses on its way to
//! the eye, in closed form in the resolve, and a sky that runs from the
//! horizon's colour to the zenith's in `present`.
//!
//! A frame may also carry a host's **overlays** ([`tuile_film::OverlayMesh`]):
//! coloured triangles placed in the world. They are the one thing rastered
//! besides visibility — after it, into a target of their own, against the
//! depth the ground left — and `present` lays them over the picture once
//! its tone is done. A frame without any costs nothing more than before.
//!
//! Binning is what makes this portable: WebGPU has no bindless textures, so
//! the resolve cannot pick a tile's texture per pixel — but it can run once
//! per tile over exactly that tile's pixels.
//!
//! The device is the caller's. A browser hands in a WebGPU device, a binary a
//! headless one; this crate never creates either.

mod frame;

use std::collections::HashMap;

use glam::{DVec3, Vec3};
use tuile_film::{FrameCamera, Look, OverlayDepth, OverlayMesh, TileKey};
use wgpu::util::DeviceExt;

pub use frame::{shadow_reach, FrameUniform, SunView, TileFrame};

/// How many tiles may be resident at once. Fixed because the scan that bins
/// pixels by tile runs over all of them in one workgroup; it matches
/// `MAX_SLOTS` in `bin.wgsl`.
pub const MAX_SLOTS: u32 = 8192;
/// Dynamic uniform offsets must be 256-aligned on every backend.
const SLOT_STRIDE: u64 = 256;

const VIS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Overlays are blended in linear light before they are filtered down: a
/// byte a channel would band their dark colours.
const OVERLAY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The side of the sun's map, in texels, where the device allows it.
const SHADOW_SIDE: u32 = 4096;
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
    #[error("{0}")]
    Overlay(&'static str),
}

/// `overlay.wgsl`'s vertex: where it is from the eye, and its colour.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OverlayVertex {
    position: [f32; 3],
    color: [f32; 4],
}

/// A buffer that is only ever made larger: overlays change every frame,
/// and their size hardly does.
#[derive(Default)]
struct Growing(Option<wgpu::Buffer>);

impl Growing {
    fn holding(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &'static str,
        usage: wgpu::BufferUsages,
        bytes: &[u8],
    ) -> &wgpu::Buffer {
        let size = bytes.len() as u64;
        let buffer = match self.0.take() {
            Some(buffer) if buffer.size() >= size => buffer,
            _ => device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.next_power_of_two().max(4096),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        };
        queue.write_buffer(&buffer, 0, bytes);
        self.0.insert(buffer)
    }
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
    /// A sphere around the mesh, from its origin: what the sun's view is
    /// fitted to.
    centre: Vec3,
    radius: f32,
    /// How far stitching moves its vertices at most, in metres.
    reach: f32,
    bind: wgpu::BindGroup,
    // Held for as long as the bind group that names them — and to make it
    // again when the tile is stitched anew. The first is the positions the
    // stages read; `as_meshed` is the positions the mesh came with, which
    // stitching moves each time from where they were.
    buffers: [wgpu::Buffer; 4],
    as_meshed: wgpu::Buffer,
    vertices: u32,
    view: Option<wgpu::TextureView>,
    _texture: Option<wgpu::Texture>,
}

struct Targets {
    vis: wgpu::TextureView,
    depth: wgpu::TextureView,
    hdr: wgpu::TextureView,
    /// Made when a frame first carries an overlay: a film that never does
    /// never pays for it.
    overlay: Option<wgpu::TextureView>,
    out: wgpu::Texture,
    out_view: wgpu::TextureView,
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
    /// The same tiles, depth alone, as the sun sees them.
    shadow: wgpu::RenderPipeline,
    /// A tile's vertices, moved by its strips: see `stitch.wgsl`.
    displace: wgpu::ComputePipeline,
    /// Overlays hidden by nearer ground, and overlays never hidden.
    overlay_terrain: wgpu::RenderPipeline,
    overlay_always: wgpu::RenderPipeline,
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
    overlay_bg: wgpu::BindGroup,
    overlay_vertices: Growing,
    overlay_indices: Growing,
    /// The sun's map and its side, for a look that has shadows.
    shadow_map: Option<(wgpu::TextureView, u32)>,
    /// A texel of that map on the ground, in the frame last recorded.
    shadow_texel: Option<f32>,
    slot_bg: wgpu::BindGroup,
    frame_buf: wgpu::Buffer,
    tiles_buf: wgpu::Buffer,
    counts: wgpu::Buffer,
    args: wgpu::Buffer,
    targets: Targets,
    sampler: wgpu::Sampler,
    white: wgpu::TextureView,
    empty: wgpu::Buffer,
    /// Tiles whose vertices are to be put where stitching has them, before
    /// the next frame is drawn.
    pending_stitches: Vec<(wgpu::BindGroup, u32)>,
    /// Paint what shows through a gap: see [`Self::probe_holes`].
    probe_holes: bool,
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
    /// The grade of its level; the identity leaves the stored bytes as
    /// they are, exactly.
    pub grade: LayerGrade,
    /// A correction that varies across the imagery tile, in place of
    /// `grade`: see [`LayerField`].
    pub field: Option<LayerField>,
}

/// Points a transfer curve is given at: a stop apart, from eight stops
/// under white to one.
pub const CURVE_KNOTS: usize = 8;

/// What is done to an imagery tile at one of its corners, as it is blended
/// across the tile: every number here is mixed between the four corners
/// before anything is made of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerCorner {
    /// The gain, in stops a channel.
    pub gain_stops: [f32; 3],
    /// A colour taken away, in the light the gain leaves.
    pub black: [f32; 3],
    /// A power on luminance about the pivot, as its stops (the power is
    /// two to this), and the pivot as its stops under white.
    pub contrast_stops: f32,
    pub pivot_stops: f32,
    /// A factor on what is not luminance, as its stops.
    pub saturation_stops: f32,
    /// A transfer curve a channel: stops added to a texel by the light it
    /// came in with, at each of [`CURVE_KNOTS`] lights from −8 stops to
    /// −1, the line between two of them, the end's value beyond.
    pub curve: [[f32; CURVE_KNOTS]; 3],
}

impl LayerCorner {
    /// Changes nothing.
    pub const IDENTITY: Self = Self {
        gain_stops: [0.0; 3],
        black: [0.0; 3],
        contrast_stops: 0.0,
        pivot_stops: -2.5,
        saturation_stops: 0.0,
        curve: [[0.0; CURVE_KNOTS]; 3],
    };
}

/// A correction carried by the four corners of an imagery tile — top-left,
/// top-right, bottom-left, bottom-right — and blended across it. Two tiles
/// that share an edge and are given the same two corners along it are
/// composed the same along that edge, whatever lies between.
///
/// Applied in linear light and in this order: the gain, the colour taken
/// away, the transfer curve read at the light the texel came in with, the
/// power on luminance, the factor on what is not luminance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerField {
    pub corners: [LayerCorner; 4],
    /// A colour matrix at each corner, in place of everything above: three
    /// rows of four — a channel out for each; red, green, blue in, then
    /// what is added — blended across the tile and applied to the texel in
    /// linear light. A fitted function of where a texel is and what colour
    /// it has.
    pub matrices: Option<[[[f32; 4]; 3]; 4]>,
}

/// What a layer's colour goes through at composition, in linear light and
/// in this order: `black` taken away, multiplied by `gain`, luminance
/// raised to `contrast` about `pivot`, what is not luminance multiplied by
/// `saturation`. One per imagery level — every tile of a level the same —
/// so that levels of different sources meet without a step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerGrade {
    pub black: [f32; 3],
    pub gain: [f32; 3],
    pub contrast: f32,
    pub pivot: f32,
    pub saturation: f32,
}

impl LayerGrade {
    /// Changes nothing.
    pub const IDENTITY: Self = Self {
        black: [0.0; 3],
        gain: [1.0; 3],
        contrast: 1.0,
        pivot: 0.18,
        saturation: 1.0,
    };

    pub fn is_identity(&self) -> bool {
        self.black == [0.0; 3]
            && self.gain == [1.0; 3]
            && self.contrast == 1.0
            && self.saturation == 1.0
    }
}

impl Default for LayerGrade {
    fn default() -> Self {
        Self::IDENTITY
    }
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
    gain: [f32; 4],
    black: [f32; 4],
    more: [f32; 4],
    /// A field's four corners, nine vectors each: gain and contrast, black
    /// and saturation, pivot, then the curve of each channel in two.
    corners: [[f32; 4]; 36],
}

impl LayerField {
    fn packed(&self) -> [[f32; 4]; 36] {
        let mut out = [[0.0f32; 4]; 36];
        if let Some(matrices) = &self.matrices {
            // The same nine vectors a corner; a matrix takes the first
            // three, a row each.
            for (matrix, at) in matrices.iter().zip(out.chunks_exact_mut(9)) {
                at[..3].copy_from_slice(matrix);
            }
            return out;
        }
        for (corner, at) in self.corners.iter().zip(out.chunks_exact_mut(9)) {
            let (g, b) = (corner.gain_stops, corner.black);
            at[0] = [g[0], g[1], g[2], corner.contrast_stops];
            at[1] = [b[0], b[1], b[2], corner.saturation_stops];
            at[2] = [corner.pivot_stops, 0.0, 0.0, 0.0];
            for (channel, curve) in corner.curve.iter().enumerate() {
                at[3 + 2 * channel] = [curve[0], curve[1], curve[2], curve[3]];
                at[4 + 2 * channel] = [curve[4], curve[5], curve[6], curve[7]];
            }
        }
        out
    }
}

/// The pass that puts a tile's vertices where stitching has them, its band
/// the engine's.
fn stitch_wgsl() -> String {
    include_str!("shaders/stitch.wgsl")
        .replace("/*BAND*/", &format!("{:?}", tuile_core::stitch::BAND))
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
                texture(8, wgpu::TextureSampleType::Depth),
                wgpu::BindGroupLayoutEntry {
                    binding: 9,
                    visibility: compute_only,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
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

        let shadow_module = shader(&device, "film shadow", include_str!("shaders/shadow.wgsl"));
        let shadow = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("film shadow"),
            layout: Some(
                &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("film shadow"),
                    bind_group_layouts: &[Some(&raster_bgl), Some(&tile_bgl)],
                    immediate_size: 0,
                }),
            ),
            vertex: wgpu::VertexState {
                module: &shadow_module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                // Ground shades ground whichever way it faces.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                // Zero is nearest the sun.
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                // Ground seen at a grazing angle by the sun is pushed back
                // by its own slope, so that it does not shade itself.
                bias: wgpu::DepthBiasState {
                    constant: 2,
                    slope_scale: 2.0,
                    clamp: 0.0,
                },
            }),
            multisample: Default::default(),
            fragment: None,
            multiview_mask: None,
            cache: None,
        });

        let overlay_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film overlay"),
            entries: &[uniform(0, wgpu::ShaderStages::VERTEX, false)],
        });
        let overlay_module = shader(
            &device,
            "film overlay",
            include_str!("shaders/overlay.wgsl"),
        );
        let overlay_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film overlay"),
            bind_group_layouts: &[Some(&overlay_bgl)],
            immediate_size: 0,
        });
        let overlay = |label, depth_compare| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&overlay_layout),
                vertex: wgpu::VertexState {
                    module: &overlay_module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<OverlayVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
                    }],
                },
                primitive: wgpu::PrimitiveState {
                    // A ribbon is seen from both sides.
                    cull_mode: None,
                    ..Default::default()
                },
                // The ground's depth is read and never written: an overlay
                // is tested against the ground, not against another.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(depth_compare),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &overlay_module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: OVERLAY_FORMAT,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        // Reverse-Z: an overlay shows where it is nearer than the ground,
        // and nearer is greater.
        let overlay_terrain = overlay("film overlay", wgpu::CompareFunction::Greater);
        let overlay_always = overlay("film overlay, never hidden", wgpu::CompareFunction::Always);

        let displace = compute(
            &device,
            &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("film stitch"),
                source: wgpu::ShaderSource::Wgsl(stitch_wgsl().into()),
            }),
            "displace",
            None,
        );
        let bin = shader(&device, "film bin", include_str!("shaders/bin.wgsl"));
        let resolve_module = shader(
            &device,
            "film resolve",
            concat!(
                include_str!("shaders/air.wgsl"),
                include_str!("shaders/resolve.wgsl")
            ),
        );
        let resolve_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film resolve"),
            bind_group_layouts: &[Some(&resolve_bgl), Some(&tile_bgl), Some(&slot_bgl)],
            immediate_size: 0,
        });
        let present_module = shader(
            &device,
            "film present",
            concat!(
                include_str!("shaders/air.wgsl"),
                include_str!("shaders/present.wgsl")
            ),
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
            shadow,
            displace,
            overlay_terrain,
            overlay_always,
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

        let overlay_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film overlay"),
            layout: &overlay_bgl,
            entries: &[entry(0, frame_buf.as_entire_binding())],
        });
        // What `present` is bound in an overlay's place until there is one.
        let blank = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("film no overlay"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OVERLAY_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());

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
        // The sun's map, for a look that has shadows; a texel that shades
        // nothing otherwise, since a binding may not be empty.
        let shadow_side = if settings.look.shadow > 0.0 {
            SHADOW_SIDE.min(device.limits().max_texture_dimension_2d)
        } else {
            1
        };
        let shadow_view = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("film shadow map"),
                size: wgpu::Extent3d {
                    width: shadow_side,
                    height: shadow_side,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let shadow_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("film shadow"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Lit where nothing in the map is nearer the sun.
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
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
            &blank,
            &shadow_view,
            &shadow_sampler,
        );

        Self {
            device,
            queue,
            settings,
            pipelines,
            tile_bgl,
            raster_bg,
            overlay_bg,
            overlay_vertices: Growing::default(),
            overlay_indices: Growing::default(),
            shadow_map: (settings.look.shadow > 0.0).then_some((shadow_view, shadow_side)),
            shadow_texel: None,
            slot_bg,
            frame_buf,
            tiles_buf,
            counts,
            args,
            targets,
            sampler,
            white,
            empty,
            pending_stitches: Vec::new(),
            probe_holes: false,
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
        blank: &wgpu::TextureView,
        shadow_map: &wgpu::TextureView,
        shadow_sampler: &wgpu::Sampler,
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
                    entry(8, wgpu::BindingResource::TextureView(shadow_map)),
                    entry(9, wgpu::BindingResource::Sampler(shadow_sampler)),
                ],
            }),
            present_bg: Self::present_bg(
                device, pipelines, frame_buf, &hdr, &vis, &out_view, blank,
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
            hdr,
            overlay: None,
            out,
            out_view,
            list,
            planes,
        }
    }

    fn present_bg(
        device: &wgpu::Device,
        pipelines: &Pipelines,
        frame_buf: &wgpu::Buffer,
        hdr: &wgpu::TextureView,
        vis: &wgpu::TextureView,
        out: &wgpu::TextureView,
        overlay: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film present"),
            layout: &pipelines.present.get_bind_group_layout(0),
            entries: &[
                entry(0, frame_buf.as_entire_binding()),
                entry(1, wgpu::BindingResource::TextureView(hdr)),
                entry(2, wgpu::BindingResource::TextureView(vis)),
                entry(3, wgpu::BindingResource::TextureView(out)),
                entry(4, wgpu::BindingResource::TextureView(overlay)),
            ],
        })
    }

    /// The target overlays are drawn into, at the supersampled size, made
    /// the first time a frame carries one and bound to `present` from then.
    fn overlay_target(&mut self) -> wgpu::TextureView {
        if let Some(view) = &self.targets.overlay {
            return view.clone();
        }
        let k = self.settings.supersample.max(1);
        let view = self
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("film overlay"),
                size: wgpu::Extent3d {
                    width: self.settings.width * k,
                    height: self.settings.height * k,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OVERLAY_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        self.targets.present_bg = Self::present_bg(
            &self.device,
            &self.pipelines,
            &self.frame_buf,
            &self.targets.hdr,
            &self.targets.vis,
            &self.targets.out_view,
            &view,
        );
        self.targets.overlay = Some(view.clone());
        view
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

    /// How fine the shadows of the frame last recorded are: the metres of
    /// ground, facing the sun, that one texel of the sun's map covers. The
    /// map holds what the camera sees as far as shadows reach
    /// ([`shadow_reach`]), so a frame that looks to the horizon has coarser
    /// shadows than one that looks down. `None` without shadows.
    pub fn shadow_texel(&self) -> Option<f32> {
        self.shadow_texel
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
        if !self.pending_stitches.is_empty() {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("film stitch"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipelines.displace);
            for (bind, vertices) in self.pending_stitches.drain(..) {
                pass.set_bind_group(0, &bind, &[]);
                pass.dispatch_workgroups(vertices.div_ceil(64), 1, 1);
            }
        }
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
                gain: [1.0; 4],
                black: [0.0, 0.0, 0.0, 1.0],
                more: [0.18, 0.0, 0.0, 0.0],
                corners: [[0.0; 4]; 36],
            };
            dispatch(blank, &self.white, [w, h]);
            for layer in &drape.layers {
                // The texels whose centres can fall in the layer's rectangle,
                // generously: the shader decides each one exactly.
                let (c, g) = (layer.coverage, layer.grade);
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
                        gain: [g.gain[0], g.gain[1], g.gain[2], g.contrast],
                        black: [g.black[0], g.black[1], g.black[2], g.saturation],
                        more: [
                            g.pivot,
                            f32::from(u8::from(!g.is_identity() && layer.field.is_none())),
                            f32::from(u8::from(layer.field.is_some())),
                            f32::from(u8::from(layer.field.is_some_and(|f| f.matrices.is_some()))),
                        ],
                        corners: layer.field.map_or([[0.0; 4]; 36], |f| f.packed()),
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

    /// A mesh's four buffers, its flags, and a sphere around it from its
    /// origin.
    fn buffers_of(&self, mesh: &TileMesh<'_>) -> ([wgpu::Buffer; 4], u32, Vec3, f32) {
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
        // A sphere around the mesh, from the box its vertices fill.
        let (mut low, mut high) = (Vec3::INFINITY, Vec3::NEG_INFINITY);
        for vertex in mesh.positions.chunks_exact(12) {
            let at = Vec3::from_array(std::array::from_fn(|axis| {
                f32::from_le_bytes(std::array::from_fn(|byte| vertex[4 * axis + byte]))
            }));
            (low, high) = (low.min(at), high.max(at));
        }
        let (centre, radius) = if low.is_finite() && high.is_finite() {
            ((low + high) / 2.0, (high - low).length() / 2.0)
        } else {
            (Vec3::ZERO, 0.0)
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
        let empty = || self.empty.clone();
        let buffers = [
            positions.unwrap_or_else(empty),
            normals.unwrap_or_else(empty),
            uvs.unwrap_or_else(empty),
            indices.unwrap_or_else(empty),
        ];
        (buffers, flags, centre, radius)
    }

    fn bind_of(
        &self,
        buffers: &[wgpu::Buffer; 4],
        view: Option<&wgpu::TextureView>,
    ) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film tile"),
            layout: &self.tile_bgl,
            entries: &[
                entry(0, buffers[0].as_entire_binding()),
                entry(1, buffers[1].as_entire_binding()),
                entry(2, buffers[2].as_entire_binding()),
                entry(3, buffers[3].as_entire_binding()),
                entry(
                    4,
                    wgpu::BindingResource::TextureView(view.unwrap_or(&self.white)),
                ),
            ],
        })
    }

    /// Stitches a resident tile to its neighbours, as
    /// `tuile_core::stitch::plan` decided for the frames to come: `strips`
    /// are its `Stitch::packed`; `mesh` is the tile's mesh again when the
    /// plan gave it vertices (`stitch::split`), `None` when its triangles
    /// are the ones it has. `reach` is how far a vertex moves at most.
    ///
    /// The tile's vertices are moved by a compute pass recorded with the
    /// next frame, once, into the positions every stage reads from then on.
    /// The tile's drape is kept.
    pub fn stitch(
        &mut self,
        key: &TileKey,
        mesh: Option<&TileMesh<'_>>,
        strips: &[[f32; 4]],
        reach: f32,
    ) -> Result<(), FilmGpuError> {
        let made = mesh.map(|mesh| {
            (
                self.buffers_of(mesh),
                mesh.index_count,
                mesh.indices.is_empty(),
                (mesh.positions.len() / 12) as u32,
            )
        });
        let Some(r) = self.resident.get(key) else {
            return Err(FilmGpuError::NotResident(*key));
        };
        let (mut buffers, as_meshed, vertices) = match &made {
            Some(((buffers, ..), _, _, vertices)) => {
                (buffers.clone(), buffers[0].clone(), *vertices)
            }
            None => (r.buffers.clone(), r.as_meshed.clone(), r.vertices),
        };
        let flags = made
            .as_ref()
            .map_or(r.flags, |((_, flags, ..), ..)| flags | (r.flags & 4));
        // Knots on a side, or a side that laps: the rows before the knots.
        let any = strips.iter().take(10).any(|row| *row != [0.0; 4]);
        // Moved from where the mesh has them, never from where the last
        // stitching left them.
        buffers[0] = as_meshed.clone();
        if any && flags & 2 != 0 && vertices > 0 {
            let held = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("film stitch strips"),
                    contents: bytemuck::cast_slice(strips),
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let moved = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("film stitched positions"),
                size: as_meshed.size(),
                // Read back by whoever checks the pass against the engine.
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            self.pending_stitches.push((
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("film stitch"),
                    layout: &self.pipelines.displace.get_bind_group_layout(0),
                    entries: &[
                        entry(0, as_meshed.as_entire_binding()),
                        entry(1, buffers[2].as_entire_binding()),
                        entry(2, held.as_entire_binding()),
                        entry(3, moved.as_entire_binding()),
                    ],
                }),
                vertices,
            ));
            buffers[0] = moved;
        }
        let bind = self.bind_of(&buffers, r.view.as_ref());
        let Some(r) = self.resident.get_mut(key) else {
            return Err(FilmGpuError::NotResident(*key));
        };
        if let Some(((_, _, centre, radius), index_count, no_indices, _)) = made {
            (r.centre, r.radius) = (centre, radius);
            r.index_count = if no_indices { 0 } else { index_count };
        }
        r.flags = flags;
        r.reach = reach;
        r.vertices = vertices;
        r.as_meshed = as_meshed;
        r.buffers = buffers;
        r.bind = bind;
        Ok(())
    }

    /// The positions the stages read for a tile that stitching moved, once
    /// a frame has been recorded since: `f32` triples about the tile's
    /// origin, in the order of the mesh it was last given. `None` for a tile
    /// stitching leaves where its mesh has it. For an instrument or a test
    /// to hold the pass against `tuile_core::stitch::displaced`.
    pub fn stitched_positions(&self, key: &TileKey) -> Option<&wgpu::Buffer> {
        let r = self.resident.get(key)?;
        (r.buffers[0] != r.as_meshed).then_some(&r.buffers[0])
    }

    /// Paints magenta what a gap between two tiles shows: a pixel whose
    /// ground is further than any an eye over the Earth can see — the far
    /// side of the planet, where the frame draws it — or that no tile covers
    /// though its ray meets the Earth. A probe, for counting holes.
    pub fn probe_holes(&mut self, on: bool) {
        self.probe_holes = on;
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
        let (buffers, flags, centre, radius) = self.buffers_of(mesh);
        let flags = flags | if albedo.is_some() { 4 } else { 0 };
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
        let bind = self.bind_of(&buffers, view.as_ref());
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
                centre,
                radius,
                reach: 0.0,
                bind,
                as_meshed: buffers[0].clone(),
                vertices: (mesh.positions.len() / 12) as u32,
                buffers,
                view,
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
        self.render_with(camera, selection, &[])
    }

    /// [`Self::render`], with a host's overlays drawn in the same scene:
    /// through the same camera, each mesh tested against the ground as its
    /// [`OverlayDepth`] says, in the order given. They are laid over the
    /// picture once its tone is done and before its output curve, through
    /// the supersampling filter — so their edges are smoothed as the
    /// ground's are, and their colours are the ones given whatever the
    /// look.
    pub fn render_with(
        &mut self,
        camera: &FrameCamera,
        selection: &[TileKey],
        overlays: &[OverlayMesh],
    ) -> Result<wgpu::CommandEncoder, FilmGpuError> {
        // Everything of the overlays that can fail does before anything is
        // recorded: one vertex buffer for the frame, eye-relative — each
        // origin meets the eye in f64, and only the difference is narrowed.
        let mut vertices: Vec<OverlayVertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        // Of each mesh: its indices, its first vertex, how it is tested.
        let mut drawn_over = Vec::new();
        for mesh in overlays {
            if let Some(fault) = mesh.fault() {
                return Err(FilmGpuError::Overlay(fault));
            }
            if mesh.indices.is_empty() {
                continue;
            }
            let from_eye = DVec3::from_array(mesh.origin_ecef) - camera.eye;
            let base = i32::try_from(vertices.len())
                .map_err(|_| FilmGpuError::Overlay("a frame's overlays have too many vertices"))?;
            vertices.extend(mesh.positions.iter().zip(&mesh.colors).map(|(p, c)| {
                OverlayVertex {
                    position: (from_eye + DVec3::new(p[0].into(), p[1].into(), p[2].into()))
                        .as_vec3()
                        .to_array(),
                    color: *c,
                }
            }));
            let first = indices.len() as u32;
            indices.extend_from_slice(&mesh.indices);
            drawn_over.push((first..indices.len() as u32, base, mesh.depth));
        }
        let overlaid = !drawn_over.is_empty();
        let overlay_target = overlaid.then(|| self.overlay_target());

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
        let mut uniform =
            FrameUniform::new(camera, &self.settings.look, [w, h, k, u32::from(overlaid)]);
        // The sun's view of what this frame draws, when the look has
        // shadows and there is anything to cast one.
        let sun = self.shadow_map.as_ref().and_then(|(map, side)| {
            let spheres = drawn.iter().filter(|r| r.index_count > 0).map(|r| {
                let centre = DVec3::from_array(r.origin_ecef) - camera.eye + r.centre.as_dvec3();
                // Stitching moves a vertex by no more than twice its reach.
                (centre, f64::from(r.radius + 2.0 * r.reach))
            });
            SunView::fitted(
                self.settings.look.to_sun,
                camera.eye,
                spheres,
                &uniform.bounding_rays(),
                shadow_reach(camera.height()),
                *side,
            )
            .map(|view| (view, map, *side))
        });
        if let Some((view, _, side)) = &sun {
            uniform = uniform.shadowed(view, self.settings.look.shadow, *side);
        }
        if self.probe_holes {
            // Up at the eye and its distance from the Earth's centre, as a
            // look with air sets them: the probe needs both with no air.
            uniform.local_up[3] = 1.0;
            if self.settings.look.haze.is_none() {
                let up = camera.eye.normalize().as_vec3();
                uniform.local_up = [up.x, up.y, up.z, 1.0];
                uniform.haze[2] = (0.5 / camera.eye.length()) as f32;
            }
        }
        self.shadow_texel = sun.as_ref().map(|(view, _, _)| view.texel);
        self.queue
            .write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&uniform));
        if top > 0 {
            self.queue
                .write_buffer(&self.tiles_buf, 0, bytemuck::cast_slice(&self.tiles[..top]));
        }

        encoder.clear_buffer(&self.counts, 0, None);

        if let Some((_, map, _)) = &sun {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("film shadow"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: map,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipelines.shadow);
            pass.set_bind_group(0, &self.raster_bg, &[]);
            for r in &drawn {
                pass.set_bind_group(1, &r.bind, &[]);
                pass.draw(0..r.index_count, r.slot..r.slot + 1);
            }
        }

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
                        // Kept only for overlays to be tested against.
                        store: if overlaid {
                            wgpu::StoreOp::Store
                        } else {
                            wgpu::StoreOp::Discard
                        },
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

        if let Some(target) = &overlay_target {
            let vertices = self.overlay_vertices.holding(
                &self.device,
                &self.queue,
                "film overlay vertices",
                wgpu::BufferUsages::VERTEX,
                bytemuck::cast_slice(&vertices),
            );
            let indices = self.overlay_indices.holding(
                &self.device,
                &self.queue,
                "film overlay indices",
                wgpu::BufferUsages::INDEX,
                bytemuck::cast_slice(&indices),
            );
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("film overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                // No operations: the ground's depth is read, never written.
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.overlay_bg, &[]);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            for (range, base, depth) in drawn_over {
                pass.set_pipeline(match depth {
                    OverlayDepth::Terrain => &self.pipelines.overlay_terrain,
                    OverlayDepth::Always => &self.pipelines.overlay_always,
                });
                pass.draw_indexed(range, base, 0..1);
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
