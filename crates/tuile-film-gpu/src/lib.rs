// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod atmosphere;
mod frame;

use std::collections::HashMap;

use glam::{DVec3, Vec3};
// The vertex every renderer of overlays packs, and the light they share: the
// viewer draws these same meshes, and the HUD has to come out of both alike.
use tuile_film::{
    Eye, EyePair, FrameCamera, FrameLight, Look, OverlayBlend, OverlayDepth, OverlayMesh, TileKey,
};
use tuile_overlay::gpu::OverlayVertex;
use wgpu::util::DeviceExt;

pub use atmosphere::{sky_look, AtmosphereUniform};
pub use frame::{shadow_reach, FrameUniform, SunView, TileFrame};

/// How many tiles may be resident at once. Fixed because the scan that bins
/// pixels by tile runs over all of them in one workgroup; it matches
/// `MAX_SLOTS` in `bin.wgsl`.
pub const MAX_SLOTS: u32 = 16384;
/// Dynamic uniform offsets must be 256-aligned on every backend.
const SLOT_STRIDE: u64 = 256;

const VIS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// How much of the sun the ground loses to projected shadows
/// ([`OverlayShadow::Project`]): each triangle's share of the sun's disc,
/// added — so a float, a byte's steps would band a penumbra — and taken
/// at most whole by the resolve.
const PROJECTED_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;
/// The sun's angular radius, in radians: its disc is half a degree across.
pub const SUN_ANGULAR_RADIUS: f32 = 4.65e-3;
/// Overlays are blended in linear light before they are filtered down: a
/// byte a channel would band their dark colours.
const OVERLAY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// What [`FilmGpu::write_linear`] writes: 32-bit floats, a disc thousands of
/// times white kept as it is.
pub const LINEAR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;
/// The side of the sun's map, in texels, where the device allows it.
const SHADOW_SIDE: u32 = 4096;
/// The most maps of the sun a frame draws: one for each sample of a
/// supersampled pixel, up to this many, each on its grid moved by a
/// fraction of a texel (`SUN_MAP_JITTER` in `common.wgsl`), so that a
/// pixel averages the shadow of a thin caster over as many grids. A frame
/// drawn one sample a pixel draws one map, where it always was.
const SUN_MAPS_MAX: u32 = 4;

/// How many maps of the sun a film of `supersample` samples a side draws.
fn sun_maps(supersample: u32) -> u32 {
    (supersample.max(1) * supersample.max(1)).min(SUN_MAPS_MAX)
}
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

/// The steepest slope to the sun a caster's edges are pushed back for, as
/// a tangent: past it, a face all but edge-on to the sun is pushed back no
/// further.
const EDGE_SLOPE_MAX: f32 = 8.0;

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

/// One picture of a frame: the camera it is drawn through, the frame's own
/// camera, and which eye it is, for a stereo film.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shot {
    /// What the picture is drawn through: the frame's camera, or an eye's.
    pub camera: FrameCamera,
    /// The frame's own camera, between the eyes: what the sun's map is
    /// fitted to, so that both eyes are shaded by the same map.
    pub frame: FrameCamera,
    /// The eye drawn, for a stereo film; `None` for a film of one eye. An
    /// overlay meant for one eye only ([`tuile_film::OverlayEyes`]) is left
    /// out of the other's picture.
    pub eye: Option<Eye>,
}

impl Shot {
    /// The one picture of a film of one eye.
    pub fn mono(camera: FrameCamera) -> Self {
        Self {
            camera,
            frame: camera,
            eye: None,
        }
    }

    /// `eye`'s picture of the frame whose camera is `frame`.
    pub fn eye(frame: FrameCamera, pair: &EyePair, eye: Eye) -> Self {
        Self {
            camera: frame.eye(pair, eye),
            frame,
            eye: Some(eye),
        }
    }
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
    /// How much of the sun projected shadows take, a sample a texel: see
    /// `projected.wgsl`.
    projected: wgpu::TextureView,
    projected_texture: wgpu::Texture,
    hdr: wgpu::TextureView,
    /// Made when a frame first carries an overlay: a film that never does
    /// never pays for it.
    overlay: Option<OverlayTargets>,
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

/// What overlays are drawn into: their colour, their own depth — what
/// opaque overlays wrote, and nothing of the ground — and what they read.
struct OverlayTargets {
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    bind: wgpu::BindGroup,
}

/// An overlay pipeline for each way a mesh may be tested and blended.
struct OverlayPipelines {
    /// Tested against nearer overlays — and, in the shader, the ground —
    /// opaque, then translucent.
    tested: wgpu::RenderPipeline,
    tested_translucent: wgpu::RenderPipeline,
    /// Tested against nothing.
    always: wgpu::RenderPipeline,
    always_translucent: wgpu::RenderPipeline,
    /// Into the sun's map: a caster's triangles, and its edges, so that a
    /// caster thinner than a texel still leaves a line of shadow.
    ///
    /// No depth bias can be asked of lines, and an edge lies on its own
    /// faces: drawn as it is, it would shade them, a band along each of
    /// their diagonals. Each edge is therefore pushed away from the sun by
    /// what its faces' slope to the sun asks for — the bias the faces get
    /// from the raster, worked out by hand.
    cast: wgpu::RenderPipeline,
    cast_edges: wgpu::RenderPipeline,
}

impl OverlayPipelines {
    fn of(&self, depth: OverlayDepth, blend: OverlayBlend) -> &wgpu::RenderPipeline {
        match (depth, blend) {
            (OverlayDepth::Always, OverlayBlend::Opaque) => &self.always,
            (OverlayDepth::Always, OverlayBlend::Translucent) => &self.always_translucent,
            (_, OverlayBlend::Opaque) => &self.tested,
            (_, OverlayBlend::Translucent) => &self.tested_translucent,
        }
    }
}

struct Pipelines {
    raster: wgpu::RenderPipeline,
    /// The same tiles, depth alone, as the sun sees them.
    shadow: wgpu::RenderPipeline,
    /// The ground in the shadows shapes project: see `projected.wgsl`.
    projected: wgpu::RenderPipeline,
    /// A tile's vertices, moved by its strips: see `stitch.wgsl`.
    displace: wgpu::ComputePipeline,
    overlay: OverlayPipelines,
    count: wgpu::ComputePipeline,
    scan: wgpu::ComputePipeline,
    scatter: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    present: wgpu::ComputePipeline,
    /// `present`'s average before the lens and the grade, for an
    /// instrument: see [`FilmGpu::write_linear`].
    linear: wgpu::ComputePipeline,
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
    overlay_bgl: wgpu::BindGroupLayout,
    /// The frame alone, for overlays drawn into the sun's map.
    overlay_cast_bg: wgpu::BindGroup,
    overlay_vertices: Growing,
    overlay_indices: Growing,
    overlay_edges: Growing,
    /// The corners of the triangles whose shadows are projected, and what
    /// reads them.
    projected_corners: Growing,
    projected_bgl: wgpu::BindGroupLayout,
    /// The penumbra of projected shadows: `projected.wgsl`'s `penumbra`.
    projected_penumbra: wgpu::Buffer,
    /// The sun's maps, one view each, and their side, for a look that has
    /// shadows.
    shadow_map: Option<(Vec<wgpu::TextureView>, u32)>,
    /// Which map a pass draws, one entry each at its dynamic offset.
    sun_map_passes: wgpu::BindGroup,
    sun_map_stride: u32,
    /// The map — a texel that shades nothing without shadows — and how it
    /// is read, for overlays that receive.
    shadow_view: wgpu::TextureView,
    shadow_sampler: wgpu::Sampler,
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
    /// PROTOTYPE (terrain smoothing): how much of the vertices' normals a
    /// face takes (1 as before), and whether the mesh is drawn as lines.
    normal_strength: f32,
    wire: bool,
    tiles: Vec<TileFrame>,
    resident: HashMap<TileKey, Resident>,
    free: Vec<u32>,
    pending_mips: Vec<wgpu::Texture>,
    /// Drapes to compose before the next frame is drawn — and before their
    /// mips, which are made from what this writes.
    pending_drapes: Vec<Drape>,
    /// Drapes to be kept from one of their levels down: the whole one, the
    /// one kept, the first level kept.
    pending_tails: Vec<(wgpu::Texture, wgpu::Texture, u32)>,
    /// The light the frame being drawn was given ([`Self::set_light`]),
    /// and the look it makes of the ordered one: what the frame is drawn
    /// with. The ordered look itself, in `settings`, is never changed.
    light: FrameLight,
    lit: Look,
    /// The physical atmosphere's tables, uniform and sky-view.
    atmosphere: atmosphere::AtmosphereGpu,
    /// The frame's light, made on the GPU — the viewer's own pass
    /// ([`tuile_atmosphere::frame_light`]) — and whether the frame being
    /// drawn has yet to make it.
    frame_light: tuile_atmosphere::frame_light::FrameLightPass,
    light_pending: bool,
    /// What the frame's light under the physical atmosphere was worked out
    /// from.
    light_key: Option<atmosphere::LightKey>,
    /// Whether night lights were given ([`Self::set_night_lights`]).
    night_lights: bool,
    /// The picture's light for an instrument ([`Self::write_linear`]),
    /// made the first time one asks: a film no instrument measures never
    /// pays for it.
    linear: Option<(wgpu::Texture, wgpu::BindGroup)>,
    linear_bgl: wgpu::BindGroupLayout,
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
            for (matrix, at) in matrices.iter().zip(out.as_chunks_mut::<9>().0) {
                at[..3].copy_from_slice(matrix);
            }
            return out;
        }
        for (corner, at) in self.corners.iter().zip(out.as_chunks_mut::<9>().0) {
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

/// How far a corner at `at` (ECEF) of a shape that projects its shadow is
/// swept down the rays of a sun towards `to_sun`: past any ground under it —
/// the deepest ground on Earth is under a kilometre below the ellipsoid —
/// however low the sun. Nothing where the sun is under its horizon.
fn swept_to_the_ground(at: DVec3, to_sun: Vec3) -> f32 {
    let height = tuile_core::geo::ecef_to_geodetic(at).height;
    let rising = to_sun.as_dvec3().dot(at.normalize());
    if rising <= 0.0 {
        return 0.0;
    }
    // A sun a third of a degree up: some 200 km for a corner 1 km up.
    ((height + 1_000.0).max(0.0) / rising.max(0.005)).min(1.0e6) as f32
}

/// The tangent of the angular radius of the sun's disc that projected
/// shadows' penumbrae are cut to, `scale` times the sun's own
/// ([`SUN_ANGULAR_RADIUS`]); none, a hard shadow, at 0. Held under a
/// quarter turn's width either side, where a tangent still means a disc.
fn penumbra_tangent(scale: f32) -> f32 {
    if scale.is_nan() || scale <= 0.0 {
        return 0.0;
    }
    (SUN_ANGULAR_RADIUS * scale).min(1.2).tan()
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
                wgpu::BindGroupLayoutEntry {
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    ..texture(8, wgpu::TextureSampleType::Depth)
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 9,
                    visibility: compute_only,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
                texture(10, wgpu::TextureSampleType::Float { filterable: false }),
            ],
        });
        let slot_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film slot"),
            entries: &[uniform(0, compute_only, true)],
        });
        let atmosphere_bgl = atmosphere::layout(&device);
        let present_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film present"),
            entries: &[
                uniform(0, compute_only, false),
                texture(1, wgpu::TextureSampleType::Float { filterable: false }),
                texture(2, wgpu::TextureSampleType::Uint),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: compute_only,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: OUTPUT_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                texture(4, wgpu::TextureSampleType::Float { filterable: false }),
            ],
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

        // Which of the sun's maps a pass draws: see `SUN_MAPS_MAX`.
        let sun_map_pass_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film sun map pass"),
            entries: &[uniform(0, wgpu::ShaderStages::VERTEX, true)],
        });
        let shadow_module = shader(&device, "film shadow", include_str!("shaders/shadow.wgsl"));
        let shadow = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("film shadow"),
            layout: Some(
                &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("film shadow"),
                    bind_group_layouts: &[
                        Some(&raster_bgl),
                        Some(&tile_bgl),
                        Some(&sun_map_pass_bgl),
                    ],
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

        let fragment_only = wgpu::ShaderStages::FRAGMENT;
        let overlay_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film overlay"),
            entries: &[
                uniform(0, wgpu::ShaderStages::VERTEX_FRAGMENT, false),
                wgpu::BindGroupLayoutEntry {
                    visibility: fragment_only,
                    ..texture(1, wgpu::TextureSampleType::Depth)
                },
                wgpu::BindGroupLayoutEntry {
                    visibility: fragment_only,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    ..texture(2, wgpu::TextureSampleType::Depth)
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: fragment_only,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        });
        let overlay_cast_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film overlay cast"),
            entries: &[uniform(0, wgpu::ShaderStages::VERTEX, false)],
        });
        let overlay_module = shader(
            &device,
            "film overlay",
            &[
                tuile_overlay::gpu::LIGHT_WGSL,
                include_str!("shaders/sunlit.wgsl"),
                include_str!("shaders/overlay.wgsl"),
            ]
            .concat(),
        );
        let overlay_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film overlay"),
            bind_group_layouts: &[Some(&overlay_bgl)],
            immediate_size: 0,
        });
        let overlay_cast_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film overlay cast"),
            bind_group_layouts: &[Some(&overlay_cast_bgl), Some(&sun_map_pass_bgl)],
            immediate_size: 0,
        });
        let overlay_attributes =
            wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4, 2 => Float32x3, 3 => Uint32];
        let overlay_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<OverlayVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &overlay_attributes,
        })];
        let overlay = |label, depth_compare, write: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&overlay_layout),
                vertex: wgpu::VertexState {
                    module: &overlay_module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &overlay_buffers,
                },
                primitive: wgpu::PrimitiveState {
                    // A ribbon is seen from both sides.
                    cull_mode: None,
                    ..Default::default()
                },
                // The overlays' own depth: written by opaque meshes, read by
                // every mesh tested against overlays. The ground's is read
                // in the shader, by a mesh tested against it.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(write),
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
        let cast_attributes = wgpu::vertex_attr_array![0 => Float32x3];
        let cast_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<OverlayVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &cast_attributes,
        })];
        let edge_attributes = [
            cast_attributes[0],
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: std::mem::offset_of!(OverlayVertex, sunk) as u64,
                shader_location: 4,
            },
        ];
        let edge_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<OverlayVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &edge_attributes,
        })];
        let cast =
            |label, topology, entry_point, buffers: &[Option<wgpu::VertexBufferLayout>], bias| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&overlay_cast_layout),
                    vertex: wgpu::VertexState {
                        module: &overlay_module,
                        entry_point: Some(entry_point),
                        compilation_options: Default::default(),
                        buffers,
                    },
                    primitive: wgpu::PrimitiveState {
                        topology,
                        cull_mode: None,
                        ..Default::default()
                    },
                    // As the ground is drawn into the map.
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH_FORMAT,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(wgpu::CompareFunction::LessEqual),
                        stencil: Default::default(),
                        bias,
                    }),
                    multisample: Default::default(),
                    fragment: None,
                    multiview_mask: None,
                    cache: None,
                })
            };
        // Reverse-Z: an overlay shows where it is nearer than the overlays
        // before it, and nearer is greater.
        let tested = wgpu::CompareFunction::Greater;
        let never = wgpu::CompareFunction::Always;
        let overlay = OverlayPipelines {
            tested: overlay("film overlay", tested, true),
            tested_translucent: overlay("film overlay, translucent", tested, false),
            always: overlay("film overlay, never hidden", never, true),
            always_translucent: overlay("film overlay, never hidden, translucent", never, false),
            // The ground's own bias for a caster's faces; its edges are
            // pushed back in the shader.
            cast: cast(
                "film overlay cast",
                wgpu::PrimitiveTopology::TriangleList,
                "into_sun_map",
                &cast_buffers,
                wgpu::DepthBiasState {
                    constant: 2,
                    slope_scale: 2.0,
                    clamp: 0.0,
                },
            ),
            cast_edges: cast(
                "film overlay cast edges",
                wgpu::PrimitiveTopology::LineList,
                "edge_into_sun_map",
                &edge_buffers,
                Default::default(),
            ),
        };

        let projected_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film projected shadows"),
            entries: &[
                uniform(0, wgpu::ShaderStages::VERTEX_FRAGMENT, false),
                wgpu::BindGroupLayoutEntry {
                    visibility: fragment_only,
                    ..texture(1, wgpu::TextureSampleType::Depth)
                },
                storage(2, wgpu::ShaderStages::VERTEX_FRAGMENT, true),
                uniform(3, wgpu::ShaderStages::VERTEX_FRAGMENT, false),
            ],
        });
        let projected_penumbra = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film projected penumbra"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let projected_module = shader(
            &device,
            "film projected shadows",
            include_str!("shaders/projected.wgsl"),
        );
        let projected = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("film projected shadows"),
            layout: Some(
                &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("film projected shadows"),
                    bind_group_layouts: &[Some(&projected_bgl)],
                    immediate_size: 0,
                }),
            ),
            vertex: wgpu::VertexState {
                module: &projected_module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                // A box's far side only: it covers each pixel the box does
                // once, the eye outside the box or in it.
                cull_mode: Some(wgpu::Face::Front),
                ..Default::default()
            },
            // The ground is read in the shader; nothing is tested.
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &projected_module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                // Each triangle's share of the sun, added: the shares of a
                // mesh's abutting triangles sum to the whole's.
                targets: &[Some(wgpu::ColorTargetState {
                    format: PROJECTED_FORMAT,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::REPLACE,
                    }),
                    write_mask: wgpu::ColorWrites::RED,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

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
            &format!(
                "{}{}\n{}{}{}{}",
                atmosphere::declarations(3),
                tuile_atmosphere::hillaire::WGSL,
                tuile_atmosphere::gpu::SEEN_WGSL,
                include_str!("shaders/air.wgsl"),
                include_str!("shaders/sunlit.wgsl"),
                include_str!("shaders/resolve.wgsl")
            ),
        );
        let resolve_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film resolve"),
            bind_group_layouts: &[
                Some(&resolve_bgl),
                Some(&tile_bgl),
                Some(&slot_bgl),
                Some(&atmosphere_bgl),
            ],
            immediate_size: 0,
        });
        let present_module = shader(
            &device,
            "film present",
            &format!(
                "{}{}\n{}{}{}",
                atmosphere::declarations(1),
                tuile_atmosphere::hillaire::WGSL,
                tuile_atmosphere::gpu::SEEN_WGSL,
                include_str!("shaders/air.wgsl"),
                include_str!("shaders/present.wgsl")
            ),
        );
        let present_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film present"),
            bind_group_layouts: &[Some(&present_bgl), Some(&atmosphere_bgl)],
            immediate_size: 0,
        });
        let linear_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("film linear"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: compute_only,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: LINEAR_FORMAT,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            }],
        });
        let linear_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("film linear"),
            bind_group_layouts: &[Some(&present_bgl), Some(&atmosphere_bgl), Some(&linear_bgl)],
            immediate_size: 0,
        });
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
            projected,
            displace,
            overlay,
            count: compute(&device, &bin, "count", None),
            scan: compute(&device, &bin, "scan", None),
            scatter: compute(&device, &bin, "scatter", None),
            resolve: compute(&device, &resolve_module, "resolve", Some(&resolve_layout)),
            present: compute(&device, &present_module, "present", Some(&present_layout)),
            linear: compute(&device, &present_module, "linear", Some(&linear_layout)),
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

        let overlay_cast_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film overlay cast"),
            layout: &overlay_cast_bgl,
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
        let maps = if settings.look.shadow > 0.0 {
            sun_maps(settings.supersample)
        } else {
            1
        };
        let shadow_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("film shadow map"),
            size: wgpu::Extent3d {
                width: shadow_side,
                height: shadow_side,
                depth_or_array_layers: maps,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_view = shadow_texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let shadow_maps: Vec<wgpu::TextureView> = (0..maps)
            .map(|layer| {
                shadow_texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let sun_map_stride = device.limits().min_uniform_buffer_offset_alignment.max(16);
        let sun_map_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("film sun map passes"),
            size: u64::from(sun_map_stride * maps),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        for layer in 0..maps {
            queue.write_buffer(
                &sun_map_buffer,
                u64::from(layer * sun_map_stride),
                bytemuck::cast_slice(&[layer, 0, 0, 0]),
            );
        }
        let sun_map_passes = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film sun map passes"),
            layout: &sun_map_pass_bgl,
            entries: &[entry(
                0,
                wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &sun_map_buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(16),
                }),
            )],
        });
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

        let atmosphere = atmosphere::AtmosphereGpu::new(
            &device,
            &atmosphere_bgl,
            atmosphere::SkyViewPass::Compute,
        );
        queue.write_buffer(
            &atmosphere.uniform,
            0,
            bytemuck::bytes_of(&AtmosphereUniform::zeroed_off()),
        );
        let frame_light = tuile_atmosphere::frame_light::FrameLightPass::new(
            &device,
            &atmosphere,
            atmosphere::SkyViewPass::Compute,
        );

        Self {
            light: FrameLight::of(&settings.look),
            lit: settings.look,
            atmosphere,
            frame_light,
            light_pending: false,
            light_key: None,
            night_lights: false,
            linear: None,
            linear_bgl,
            device,
            queue,
            settings,
            pipelines,
            tile_bgl,
            raster_bg,
            overlay_bgl,
            overlay_cast_bg,
            overlay_vertices: Growing::default(),
            overlay_indices: Growing::default(),
            overlay_edges: Growing::default(),
            projected_corners: Growing::default(),
            projected_bgl,
            projected_penumbra,
            shadow_map: (settings.look.shadow > 0.0).then_some((shadow_maps, shadow_side)),
            sun_map_passes,
            sun_map_stride,
            shadow_view,
            shadow_sampler,
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
            normal_strength: 1.0,
            wire: false,
            tiles: vec![TileFrame::default(); MAX_SLOTS as usize],
            resident: HashMap::new(),
            free: (0..MAX_SLOTS).rev().collect(),
            pending_mips: Vec::new(),
            pending_drapes: Vec::new(),
            pending_tails: Vec::new(),
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
        // Read by overlays tested against the ground.
        let depth = make(
            "film depth",
            DEPTH_FORMAT,
            U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            w,
            h,
        )
        .create_view(&Default::default());
        let projected_texture = make(
            "film projected shadows",
            PROJECTED_FORMAT,
            U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC,
            w,
            h,
        );
        let projected = projected_texture.create_view(&Default::default());
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
                    entry(10, wgpu::BindingResource::TextureView(&projected)),
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
            projected,
            projected_texture,
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

    /// What overlays are drawn into, at the supersampled size, made the
    /// first time a frame carries one and bound to `present` from then.
    fn overlay_targets(&mut self) -> (wgpu::TextureView, wgpu::TextureView, wgpu::BindGroup) {
        if let Some(t) = &self.targets.overlay {
            return (t.color.clone(), t.depth.clone(), t.bind.clone());
        }
        let k = self.settings.supersample.max(1);
        let make = |label, format| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: self.settings.width * k,
                        height: self.settings.height * k,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let color = make("film overlay", OVERLAY_FORMAT);
        let depth = make("film overlay depth", DEPTH_FORMAT);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("film overlay"),
            layout: &self.overlay_bgl,
            entries: &[
                entry(0, self.frame_buf.as_entire_binding()),
                entry(1, wgpu::BindingResource::TextureView(&self.targets.depth)),
                entry(2, wgpu::BindingResource::TextureView(&self.shadow_view)),
                entry(3, wgpu::BindingResource::Sampler(&self.shadow_sampler)),
            ],
        });
        self.targets.present_bg = Self::present_bg(
            &self.device,
            &self.pipelines,
            &self.frame_buf,
            &self.targets.hdr,
            &self.targets.vis,
            &self.targets.out_view,
            &color,
        );
        self.targets.overlay = Some(OverlayTargets {
            color: color.clone(),
            depth: depth.clone(),
            bind: bind.clone(),
        });
        (color, depth, bind)
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

    /// Lights what is drawn from now on by `light`: the sun's place and
    /// strength and the dome, the rest of the look unchanged. The sun's map
    /// is fitted afresh for every picture, so the shadows follow it.
    ///
    /// Each frame's light is laid over the look as it was ordered, never
    /// over the last frame's: an exposure or a white balance does not
    /// accumulate from frame to frame. Under a physical atmosphere
    /// ([`Look::atmosphere`]) the light is worked out from the air when the
    /// frame is drawn, and of `light` only the sun's direction, the
    /// compensation and the air are heard.
    /// The night lights a physical atmosphere lights towns with at night:
    /// `luma`, `width` by `height`, a geographic grid from the antimeridian
    /// eastwards and from the north pole southwards, one byte a texel.
    pub fn set_night_lights(&mut self, width: u32, height: u32, luma: &[u8]) {
        self.atmosphere
            .set_night_lights(&self.device, &self.queue, width, height, luma);
        self.night_lights = true;
        // The next frame's light is worked out again, with the lights on.
        self.light_key = None;
    }

    pub fn set_light(&mut self, light: FrameLight) {
        self.light = light;
        self.lit = self.settings.look.lit_by(light);
    }

    /// The sky-view table of the frame last drawn under a physical
    /// atmosphere: `Rgba16Float`, `tuile_atmosphere::hillaire::SKY_VIEW_SIZE`,
    /// in the model's units.
    pub fn sky_view_texture(&self) -> &wgpu::Texture {
        &self.atmosphere.sky_view
    }

    /// The same table under the Moon, per unit of its own light: black
    /// when the frame has no Moon.
    pub fn moon_view_texture(&self) -> &wgpu::Texture {
        &self.atmosphere.moon_view
    }

    /// How much of the sun the projected shadows of the frame last drawn
    /// take from each sample of its supersampled picture: `R16Float`, each
    /// triangle's share of the sun's disc summed — the resolve takes at
    /// most the whole sun. Zero where no shadow is projected.
    pub fn projected_shadow_texture(&self) -> &wgpu::Texture {
        &self.targets.projected_texture
    }

    /// The look the frame last drawn was drawn with: the ordered one under
    /// the frame's light.
    pub fn lit(&self) -> &Look {
        &self.lit
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

    /// Records, after a frame's own commands in `encoder`, the frame's
    /// light as its picture averages it — each pixel's samples, the sky
    /// where no tile was drawn, before the lens's glare, the exposure and
    /// the grade — into a [`LINEAR_FORMAT`] texture of the picture's size,
    /// returned to be read back. In the renderer's units: pre-exposed under
    /// a physical atmosphere ([`Self::sky_seen`] says what one unit of the
    /// model's radiance is in them). For instruments, which measure a
    /// disc's light where the picture rolls it off; the picture is not
    /// touched.
    pub fn write_linear(&mut self, encoder: &mut wgpu::CommandEncoder) -> &wgpu::Texture {
        if self.linear.is_none() {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("film linear"),
                size: self.targets.out.size(),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: LINEAR_FORMAT,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("film linear"),
                layout: &self.linear_bgl,
                entries: &[entry(0, wgpu::BindingResource::TextureView(&view))],
            });
            self.linear = Some((texture, bind));
        }
        let (texture, bind) = self.linear.as_ref().expect("made above");
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipelines.linear);
            pass.set_bind_group(0, &self.targets.present_bg, &[]);
            pass.set_bind_group(1, &self.atmosphere.bind, &[]);
            pass.set_bind_group(2, bind, &[]);
            pass.dispatch_workgroups(
                self.settings.width.div_ceil(8),
                self.settings.height.div_ceil(8),
                1,
            );
        }
        texture
    }

    /// The physical atmosphere the frame last drawn was lit by: its
    /// uniform as the frame's eye has it — the sun's and the Moon's
    /// directions, the discs, what one unit of the model's radiance is in
    /// the renderer's (`illuminance`, pre-exposed) — and the model whose
    /// tables it reads. `None` without one.
    ///
    /// The light's fields are the GPU's, read back over the uniform's
    /// geometry — **blocking**: for an instrument or a test, after the
    /// frame was submitted, never a frame's loop.
    pub fn sky_seen(&self) -> Option<(AtmosphereUniform, &tuile_atmosphere::LutAtmosphere)> {
        self.atmosphere.light.as_ref()?;
        let model = self.atmosphere.model.as_ref()?;
        let read = self.frame_light.read(&self.device, &self.queue);
        Some((read.over(self.atmosphere.frame_uniform), model))
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

    /// The mip chain of `whole` from `level` down, as a texture of its own:
    /// what a tile too small on screen to be read above that level is entered
    /// with, in place of `whole`. Its texels are the very ones `whole` would
    /// have had there. `whole` is the caller's to fill or [`Self::compose`]
    /// before the next frame is drawn, and to drop; a `level` past the chain
    /// is its last.
    pub fn tail(&mut self, whole: &wgpu::Texture, level: u32) -> wgpu::Texture {
        let level = level.min(whole.mip_level_count() - 1);
        let kept = self.create_albedo(
            (whole.width() >> level).max(1),
            (whole.height() >> level).max(1),
        );
        self.pending_tails
            .push((whole.clone(), kept.clone(), level));
        kept
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
        if !self.pending_tails.is_empty() {
            // The whole chains first: a tail is cut from a level they made.
            let whole: Vec<wgpu::Texture> =
                self.pending_tails.iter().map(|t| t.0.clone()).collect();
            Self::record_mips(&self.device, &self.pipelines.mips, encoder, &whole);
            for (whole, kept, level) in self.pending_tails.drain(..) {
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        mip_level: level,
                        ..whole.as_image_copy()
                    },
                    kept.as_image_copy(),
                    kept.size(),
                );
            }
        }
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
        for vertex in mesh.positions.as_chunks::<12>().0 {
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

    /// PROTOTYPE (terrain smoothing): how much of its vertices' normals a
    /// face is lit by, 0 to 1 — at 0 a mesh with normals is lit as one with
    /// none — and whether the mesh is drawn as lines over a pale ground.
    pub fn smoothing_probe(&mut self, normal_strength: f32, wire: bool) {
        self.normal_strength = normal_strength.clamp(0.0, 1.0);
        self.wire = wire;
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
    /// through the same camera, each mesh tested and blended as it says, in
    /// the order given. They are laid over the picture once its tone is
    /// done and before its output curve, through the supersampling filter —
    /// so their edges are smoothed as the ground's are, and their colours
    /// are the ones given whatever the look.
    pub fn render_with(
        &mut self,
        camera: &FrameCamera,
        selection: &[TileKey],
        overlays: &[OverlayMesh],
    ) -> Result<wgpu::CommandEncoder, FilmGpuError> {
        self.render_shot(&Shot::mono(*camera), selection, overlays)
    }

    /// The sun's view of what `camera` would draw of `selection`, fitted as
    /// a frame fits it, eye-relative to `camera`: `None` without shadows or
    /// with nothing to cast one.
    fn sun_of(
        &self,
        camera: &FrameCamera,
        selection: &[TileKey],
    ) -> Result<Option<(SunView, u32)>, FilmGpuError> {
        let Some((_, side)) = &self.shadow_map else {
            return Ok(None);
        };
        let mut spheres = Vec::with_capacity(selection.len());
        for key in selection {
            let r = self
                .resident
                .get(key)
                .ok_or(FilmGpuError::NotResident(*key))?;
            if r.index_count > 0 {
                let centre = DVec3::from_array(r.origin_ecef) - camera.eye + r.centre.as_dvec3();
                // Stitching moves a vertex by no more than twice its reach.
                spheres.push((centre, f64::from(r.radius + 2.0 * r.reach)));
            }
        }
        let k = self.settings.supersample.max(1);
        let size = [self.settings.width * k, self.settings.height * k, k, 0];
        let rays = FrameUniform::new(camera, &self.lit, size).bounding_rays();
        Ok(SunView::fitted(
            self.lit.to_sun,
            camera.eye,
            spheres.into_iter(),
            &rays,
            shadow_reach(camera.height()),
            *side,
        )
        .map(|view| (view, *side)))
    }

    /// How wide a texel of the sun's map will be, in metres, for a frame
    /// drawn through `camera` — the frame's own, between the eyes of a
    /// stereo film — out of `selection`, all of it resident: what a host
    /// widens a thin caster by ([`tuile_film::OverlayShadow`]). `None`
    /// without shadows.
    pub fn shadow_texel_for(
        &self,
        camera: &FrameCamera,
        selection: &[TileKey],
    ) -> Result<Option<f32>, FilmGpuError> {
        Ok(self.sun_of(camera, selection)?.map(|(view, _)| view.texel))
    }

    /// [`Self::render_with`] for one picture of a frame: through
    /// `shot.camera`, the sun's map fitted to `shot.frame` — so that both
    /// eyes of a stereo frame are shaded by one map — and only the overlays
    /// `shot.eye` shows.
    pub fn render_shot(
        &mut self,
        shot: &Shot,
        selection: &[TileKey],
        overlays: &[OverlayMesh],
    ) -> Result<wgpu::CommandEncoder, FilmGpuError> {
        let camera = &shot.camera;
        // The frame's light, from the air, before anything reads the sun:
        // worked out once from the frame's own camera, so both eyes share it.
        self.light_frame(&shot.frame, selection)?;
        // The sun's view, fitted to the frame's own camera and placed for
        // this one: the same map, ground for ground, in either eye.
        let sun = self
            .sun_of(&shot.frame, selection)?
            .map(|(mut view, side)| {
                let apart = (camera.eye - shot.frame.eye).as_vec3();
                view.view_proj *= glam::Mat4::from_translation(apart);
                (view, side)
            });
        let to_sun = self.lit.to_sun.normalize_or_zero();
        // Everything of the overlays that can fail does before anything is
        // recorded: one vertex buffer for the frame, eye-relative — each
        // origin meets the eye in f64, and only the difference is narrowed.
        let mut vertices: Vec<OverlayVertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        // The edges of the meshes that cast, two indices each.
        let mut edges: Vec<u32> = Vec::new();
        // Of each mesh drawn: its indices, its first vertex, how it is drawn.
        let mut drawn_over = Vec::new();
        // Of each mesh that casts: its triangles, its edges, its first
        // vertex.
        let mut cast_triangles = Vec::new();
        let mut cast_edges = Vec::new();
        // Of each triangle whose shadow is projected: its corners from the
        // eye, each with how far it is swept down the sun's rays.
        let mut projected: Vec<[f32; 4]> = Vec::new();
        let projecting = self.lit.shadow > 0.0 && to_sun != Vec3::ZERO;
        for mesh in overlays {
            if let Some(fault) = mesh.fault() {
                return Err(FilmGpuError::Overlay(fault));
            }
            if mesh.indices.is_empty() || !mesh.eyes.shows(shot.eye) {
                continue;
            }
            let from_eye = DVec3::from_array(mesh.origin_ecef) - camera.eye;
            let base = i32::try_from(vertices.len())
                .map_err(|_| FilmGpuError::Overlay("a frame's overlays have too many vertices"))?;
            let lit = !mesh.normals.is_empty();
            // How far each vertex's edges are pushed back from the sun: two
            // texels, and three more for each unit of the steepest slope
            // to the sun of the faces it is on — as far, across the 3×3
            // texels a receiver reads, as the face falls away.
            let mut sunk = vec![0.0f32; mesh.positions.len()];
            if let (true, Some((view, _))) = (mesh.shadow.casts(), &sun) {
                let at = |i: u32| Vec3::from_array(mesh.positions[i as usize]);
                for [a, b, c] in mesh.indices.as_chunks::<3>().0 {
                    let n = (at(*b) - at(*a)).cross(at(*c) - at(*a)).normalize_or_zero();
                    let cos = n.dot(to_sun).abs();
                    let slope = if cos * EDGE_SLOPE_MAX > (1.0 - cos * cos).max(0.0).sqrt() {
                        (1.0 - cos * cos).max(0.0).sqrt() / cos
                    } else {
                        EDGE_SLOPE_MAX
                    };
                    let push = view.texel * (2.0 + 3.0 * slope);
                    for i in [a, b, c] {
                        sunk[*i as usize] = sunk[*i as usize].max(push);
                    }
                }
            }
            let flags = tuile_overlay::gpu::flags(mesh);
            vertices.extend(mesh.positions.iter().zip(&mesh.colors).enumerate().map(
                |(i, (p, c))| {
                    OverlayVertex {
                        position: (from_eye + DVec3::new(p[0].into(), p[1].into(), p[2].into()))
                            .as_vec3()
                            .to_array(),
                        color: *c,
                        normal: if lit { mesh.normals[i] } else { [0.0; 3] },
                        flags,
                        sunk: sunk[i],
                    }
                },
            ));
            if projecting && mesh.shadow.projects() {
                let corners: Vec<[f32; 4]> = mesh
                    .positions
                    .iter()
                    .map(|p| {
                        let at = DVec3::from_array(mesh.origin_ecef)
                            + DVec3::new(p[0].into(), p[1].into(), p[2].into());
                        let from_eye = (at - camera.eye).as_vec3();
                        [
                            from_eye.x,
                            from_eye.y,
                            from_eye.z,
                            swept_to_the_ground(at, to_sun),
                        ]
                    })
                    .collect();
                for &i in &mesh.indices {
                    projected.push(corners[i as usize]);
                }
            }
            let first = indices.len() as u32;
            indices.extend_from_slice(&mesh.indices);
            let triangles = first..indices.len() as u32;
            drawn_over.push((triangles.clone(), base, mesh.depth, mesh.blend));
            if mesh.shadow.casts() {
                let first = edges.len() as u32;
                for [a, b, c] in mesh.indices.as_chunks::<3>().0 {
                    edges.extend_from_slice(&[*a, *b, *b, *c, *c, *a]);
                }
                cast_triangles.push((triangles, base));
                cast_edges.push((first..edges.len() as u32, base));
            }
        }
        // Opaque meshes first, each kind in the order given.
        drawn_over.sort_by_key(|(_, _, _, blend)| *blend == OverlayBlend::Translucent);
        let overlaid = !drawn_over.is_empty();
        let overlay_targets = overlaid.then(|| self.overlay_targets());
        if overlaid {
            self.overlay_vertices.holding(
                &self.device,
                &self.queue,
                "film overlay vertices",
                wgpu::BufferUsages::VERTEX,
                bytemuck::cast_slice(&vertices),
            );
            self.overlay_indices.holding(
                &self.device,
                &self.queue,
                "film overlay indices",
                wgpu::BufferUsages::INDEX,
                bytemuck::cast_slice(&indices),
            );
            if !edges.is_empty() {
                self.overlay_edges.holding(
                    &self.device,
                    &self.queue,
                    "film overlay edges",
                    wgpu::BufferUsages::INDEX,
                    bytemuck::cast_slice(&edges),
                );
            }
        }

        // What was queued since the last frame comes first: the drapes, then
        // the mips made from them, before anything samples either.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("film frame"),
            });
        self.record_pending(&mut encoder);
        if self.atmosphere.light.is_some() {
            // The frame's light, made once a frame, then laid over each
            // picture's uniforms as they are written for it: the
            // atmosphere's light fields and the frame's own light.
            if std::mem::take(&mut self.light_pending) {
                self.frame_light.record(&mut encoder);
            }
            self.frame_light
                .copy_into_uniform(&mut encoder, &self.atmosphere.uniform);
            use tuile_atmosphere::frame_light::Extra;
            for (extra, offset) in [
                (Extra::Ambient, std::mem::offset_of!(FrameUniform, world)),
                (Extra::Sun, std::mem::offset_of!(FrameUniform, sun)),
                (
                    Extra::White,
                    std::mem::offset_of!(FrameUniform, white_balance),
                ),
            ] {
                self.frame_light
                    .copy_extra(&mut encoder, extra, &self.frame_buf, offset as u64);
            }
        }
        self.atmosphere.record_sky_view(&mut encoder);
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
        let mut uniform = FrameUniform::new(camera, &self.lit, [w, h, k, u32::from(overlaid)]);
        self.pre_expose(&mut uniform);
        if let Some((view, side)) = &sun {
            uniform = uniform.shadowed(view, self.lit.shadow, *side);
        }
        uniform.zenith[3] = self.normal_strength;
        uniform.horizon[3] = f32::from(u8::from(self.wire));
        if self.probe_holes {
            // Up at the eye and its distance from the Earth's centre, as a
            // look with air sets them: the probe needs both with no air.
            uniform.local_up[3] = 1.0;
            if uniform.haze[3] == 0.0 {
                let up = camera.eye.normalize().as_vec3();
                uniform.local_up = [up.x, up.y, up.z, 1.0];
                uniform.haze[2] = (0.5 / camera.eye.length()) as f32;
            }
        }
        self.shadow_texel = sun.as_ref().map(|(view, _)| view.texel);
        self.queue
            .write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&uniform));
        // The air's sphere, placed for this picture's eye.
        if let Some(light) = &self.atmosphere.light {
            let placed = self.atmosphere.frame_uniform.seen_from(light, camera.eye);
            self.queue
                .write_buffer(&self.atmosphere.uniform, 0, bytemuck::bytes_of(&placed));
        }
        if top > 0 {
            self.queue
                .write_buffer(&self.tiles_buf, 0, bytemuck::cast_slice(&self.tiles[..top]));
        }

        encoder.clear_buffer(&self.counts, 0, None);

        // Each of the sun's maps, on its own grid: see `SUN_MAPS_MAX`.
        let maps = match (&sun, &self.shadow_map) {
            (Some(_), Some((maps, _))) => maps.as_slice(),
            _ => &[],
        };
        for (layer, map) in maps.iter().enumerate() {
            let at = [layer as u32 * self.sun_map_stride];
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
            pass.set_bind_group(2, &self.sun_map_passes, &at);
            for r in &drawn {
                pass.set_bind_group(1, &r.bind, &[]);
                pass.draw(0..r.index_count, r.slot..r.slot + 1);
            }
            // Overlays that cast, into the same map: the ground under them
            // reads it as it reads the ground's own.
            if let (false, Some(vertices), Some(indices)) = (
                cast_triangles.is_empty(),
                &self.overlay_vertices.0,
                &self.overlay_indices.0,
            ) {
                pass.set_bind_group(0, &self.overlay_cast_bg, &[]);
                pass.set_bind_group(1, &self.sun_map_passes, &at);
                pass.set_vertex_buffer(0, vertices.slice(..));
                pass.set_pipeline(&self.pipelines.overlay.cast);
                pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                for (range, base) in &cast_triangles {
                    pass.draw_indexed(range.clone(), *base, 0..1);
                }
                if let Some(edges) = &self.overlay_edges.0 {
                    pass.set_pipeline(&self.pipelines.overlay.cast_edges);
                    pass.set_index_buffer(edges.slice(..), wgpu::IndexFormat::Uint32);
                    for (range, base) in &cast_edges {
                        pass.draw_indexed(range.clone(), *base, 0..1);
                    }
                }
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
                        // Kept only for overlays to be tested against, and
                        // for the ground projected shadows fall on.
                        store: if overlaid || !projected.is_empty() {
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

        {
            // The ground in projected shadows; none, as often as not, and
            // the target cleared all the same.
            let corners = (!projected.is_empty()).then(|| {
                self.projected_corners
                    .holding(
                        &self.device,
                        &self.queue,
                        "film projected corners",
                        wgpu::BufferUsages::STORAGE,
                        bytemuck::cast_slice(&projected),
                    )
                    .clone()
            });
            let bind = corners.as_ref().map(|corners| {
                self.queue.write_buffer(
                    &self.projected_penumbra,
                    0,
                    bytemuck::cast_slice(&[
                        penumbra_tangent(self.lit.penumbra_scale),
                        0.0,
                        0.0,
                        0.0,
                    ]),
                );
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("film projected shadows"),
                    layout: &self.projected_bgl,
                    entries: &[
                        entry(0, self.frame_buf.as_entire_binding()),
                        entry(1, wgpu::BindingResource::TextureView(&self.targets.depth)),
                        entry(2, corners.as_entire_binding()),
                        entry(3, self.projected_penumbra.as_entire_binding()),
                    ],
                })
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("film projected shadows"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets.projected,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(bind) = &bind {
                pass.set_pipeline(&self.pipelines.projected);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..36, 0..(projected.len() / 3) as u32);
            }
        }

        if let (Some((target, depth, bind)), Some(vertices), Some(indices)) = (
            &overlay_targets,
            &self.overlay_vertices.0,
            &self.overlay_indices.0,
        ) {
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
                // The overlays' own depth, from nothing: the ground's is
                // read in the shader.
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
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
            pass.set_bind_group(0, bind, &[]);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            for (range, base, depth, blend) in drawn_over {
                pass.set_pipeline(self.pipelines.overlay.of(depth, blend));
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
            pass.set_bind_group(3, &self.atmosphere.bind, &[]);
            for r in &drawn {
                pass.set_bind_group(1, &r.bind, &[]);
                pass.set_bind_group(2, &self.slot_bg, &[r.slot * SLOT_STRIDE as u32]);
                pass.dispatch_workgroups_indirect(&self.args, u64::from(r.slot) * 12);
            }

            pass.set_pipeline(&self.pipelines.present);
            pass.set_bind_group(0, &self.targets.present_bg, &[]);
            pass.set_bind_group(1, &self.atmosphere.bind, &[]);
            pass.dispatch_workgroups(
                self.settings.width.div_ceil(8),
                self.settings.height.div_ceil(8),
                1,
            );
        }
        Ok(encoder)
    }

    /// The frame's light under a physical atmosphere, worked out from the
    /// air for the frame whose camera is `frame` — once a frame: a second
    /// picture of it (the other eye) finds it made. Without an atmosphere,
    /// the light [`Self::set_light`] was given.
    fn light_frame(
        &mut self,
        frame: &FrameCamera,
        selection: &[TileKey],
    ) -> Result<(), FilmGpuError> {
        let base = self.settings.look;
        let Some(params) = base.atmosphere else {
            if self.atmosphere.light.take().is_some() {
                self.queue.write_buffer(
                    &self.atmosphere.uniform,
                    0,
                    bytemuck::bytes_of(&AtmosphereUniform::zeroed_off()),
                );
            }
            return Ok(());
        };
        let light = self.light;
        // The frame's air: the host's turbidity or the look's, and the
        // host's haze density as the aerosols' extinction.
        let mut params = params;
        if let Some(t) = light.air.turbidity.or(base.turbidity) {
            params = params.with_turbidity(f64::from(t));
        }
        if let Some(density) = light.air.haze_density {
            params = params.with_mie_extinction(f64::from(density));
        }
        let to_sun = light
            .illumination
            .to_sun_ecef
            .try_normalize()
            .unwrap_or(base.to_sun.as_dvec3());
        let ground = self.ground_under(frame.eye, selection)?;
        let key = atmosphere::LightKey {
            params,
            frame_eye: frame.eye,
            view: frame.view.to_cols_array(),
            ground: ground.to_array(),
            to_sun,
            moon: light.illumination.moon,
            instant: light.instant,
            compensation_ev: light.illumination.compensation_ev,
        };
        if self.light_key == Some(key) {
            return Ok(());
        }
        self.atmosphere.hold(&self.queue, params);
        let geodetic = tuile_core::geo::ecef_to_geodetic(ground);
        let metering = Self::metering_points_of(
            frame,
            &tuile_atmosphere::AtmosphereFrame::at(geodetic),
            geodetic.height,
        );
        // A host that states the instant and not the Moon: the Moon of that
        // instant, from the frame's own eye — its parallax is a degree.
        let moon = light.illumination.moon.or_else(|| {
            let instant = light.instant?;
            let eye = tuile_core::geo::ecef_to_geodetic(frame.eye);
            tuile_atmosphere::Moon::seen_from(instant, eye)
                .map(|m| tuile_atmosphere::Moonlight::of(&m))
        });
        let inputs = tuile_atmosphere::frame_light::FrameLightInputs::new(
            &params,
            geodetic,
            frame.eye,
            tuile_atmosphere::hillaire::Moment {
                to_sun_ecef: to_sun,
                moon,
                compensation_ev: light.illumination.compensation_ev,
            },
            &metering,
            tuile_atmosphere::Calibration {
                sun: base.sun.to_array(),
                sky: base.world.to_array(),
                reflectance: base.reflectance,
                camera: base.camera,
            },
            base.exposure_ev,
            base.white_balance.to_array(),
            [1.0, base.contrast, base.highlight_knee],
        );
        let sky = inputs.light;
        // The look's sun and ordered exposure; its light — the sky, the sun's
        // strength, the exposure and the white balance the meter calls for —
        // is the GPU's, copied over the frame's uniform as it is drawn.
        let mut lit = FrameLight::new(sky.illumination, light.air);
        lit.instant = light.instant;
        self.lit = base.lit_by(lit);
        // The stars of the instant, or — a film that states none — of
        // J2000: always there, the air and the camera deciding whether they
        // show.
        let instant = light.instant.unwrap_or(tuile_atmosphere::stars::J2000_UNIX);
        let stars = tuile_atmosphere::stars::j2000_to_ecef(instant).transpose();
        self.atmosphere.frame_uniform = inputs
            .uniform(&params, frame.eye, stars, &atmosphere::sky_look(&base))
            .with_planets(&tuile_atmosphere::planets::lights_at(instant));
        self.atmosphere.frame_uniform.earth[3] = f32::from(u8::from(self.night_lights));
        // Written now for the light and the sky-view, placed from each eye
        // when drawn.
        self.queue.write_buffer(
            &self.atmosphere.uniform,
            0,
            bytemuck::bytes_of(&self.atmosphere.frame_uniform),
        );
        self.frame_light.write(&self.queue, &inputs);
        self.light_pending = true;
        self.atmosphere.light = Some(sky);
        self.light_key = Some(key);
        self.atmosphere.sky_view_stale = true;
        Ok(())
    }

    /// The frame camera's metering rays ([`tuile_atmosphere::hillaire::metering`]).
    fn metering_points_of(
        frame: &FrameCamera,
        sphere: &tuile_atmosphere::AtmosphereFrame,
        ground_height: f64,
    ) -> Vec<tuile_atmosphere::hillaire::Metered> {
        let v = frame.view;
        let p = frame.projection;
        let right = (v.row(0).truncate() / p.x_axis.x).as_dvec3();
        let up = (v.row(1).truncate() / p.y_axis.y).as_dvec3();
        let fwd = (-v.row(2).truncate()).as_dvec3()
            + right * f64::from(p.z_axis.x)
            + up * f64::from(p.z_axis.y);
        tuile_atmosphere::hillaire::metering(frame.eye, fwd, right, up, sphere, ground_height)
    }

    /// Under a physical atmosphere the frame's lights are pre-exposed: the
    /// picture's exposure is already in them, so that the HDR target —
    /// half floats — holds a night twenty stops under noon. `uniform`'s
    /// lights are scaled, and its exposure set to one.
    fn pre_expose(&self, uniform: &mut FrameUniform) {
        if self.atmosphere.light.is_none() {
            return;
        }
        let k = uniform.world[3];
        for c in 0..3 {
            uniform.world[c] *= k;
            uniform.sun[c] *= k;
        }
        uniform.world[3] = 1.0;
    }

    /// The ground the frame's light is worked out at (ECEF): under the eye,
    /// at the height of the finest tiles drawn — those nearest the camera —
    /// and never over the eye. At sea level when nothing is drawn.
    fn ground_under(&self, eye: DVec3, selection: &[TileKey]) -> Result<DVec3, FilmGpuError> {
        let mut finest = f32::INFINITY;
        for key in selection {
            let r = self
                .resident
                .get(key)
                .ok_or(FilmGpuError::NotResident(*key))?;
            if r.index_count > 0 {
                finest = finest.min(r.radius);
            }
        }
        let (mut sum, mut n) = (0.0, 0u32);
        for key in selection {
            let r = &self.resident[key];
            if r.index_count > 0 && r.radius <= 2.0 * finest {
                let centre = DVec3::from_array(r.origin_ecef) + r.centre.as_dvec3();
                sum += tuile_core::geo::ecef_to_geodetic(centre).height;
                n += 1;
            }
        }
        let at = tuile_core::geo::ecef_to_geodetic(eye);
        let height = if n > 0 { sum / f64::from(n) } else { 0.0 };
        Ok(tuile_core::geo::geodetic_to_ecef(
            tuile_core::geo::Geodetic {
                height: height.clamp(0.0, at.height.max(0.0)),
                ..at
            },
        ))
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
