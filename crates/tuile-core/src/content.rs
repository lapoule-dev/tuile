// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Tile content: the [`TileContent`] type and the raw → decoded transition.
//!
//! `TileContent::Raw` (glb/b3dm bytes) is the only form that ever crosses a
//! wire; `TileContent::Decoded` is what renderers consume and only exists
//! in the consumer's memory (`docs/01-architecture.md`).
//!
//! [`decode`] is a free, blocking, I/O-free function: callers place it where
//! it belongs for their runtime (spawn_blocking, a worker, rayon, …).

use bytes::Bytes;
use glam::{DMat4, DVec3, Mat4};

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("unsupported content format: {0}")]
    Unsupported(String),
    #[error("truncated or malformed {0} payload")]
    Malformed(&'static str),
    #[error("glTF error: {0}")]
    Gltf(#[from] gltf::Error),
    #[error("b3dm container: {0}")]
    B3dm(#[from] tuile_b3dm::B3dmError),
    #[error("unsupported texture format {0:?}")]
    Texture(String),
}

/// Wire-level format of a raw content payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentFormat {
    /// Binary glTF (3D Tiles 1.1 direct content).
    Glb,
    /// Batched 3D model (legacy 1.0), wraps a glb.
    B3dm,
}

/// The content of a tile, under its two forms. `Decoded` IS a `TileContent`:
/// the same `Content` protocol message carries either, only the form varies
/// with the binding (in-process = decoded, network = raw glb).
#[derive(Debug, Clone)]
pub enum TileContent {
    Raw { format: ContentFormat, bytes: Bytes },
    Decoded(DecodedTileContent),
}

/// Identifies a payload from its magic bytes. `i3dm`/`pnts`/`cmpt` are
/// recognized but unsupported in v1 (typed error, never a panic).
pub fn sniff(bytes: &[u8]) -> Result<ContentFormat, ContentError> {
    match bytes.get(0..4) {
        Some(b"glTF") => Ok(ContentFormat::Glb),
        Some(b"b3dm") => Ok(ContentFormat::B3dm),
        Some(m @ (b"i3dm" | b"pnts" | b"cmpt")) => Err(ContentError::Unsupported(
            String::from_utf8_lossy(m).into_owned(),
        )),
        _ => Err(ContentError::Unsupported("unknown magic".into())),
    }
}

/// Decode-time context: where the tile sits in the world and which local
/// origin to rebase onto.
#[derive(Debug, Clone, Copy)]
pub struct ContentHints {
    /// Composed root→tile transform (f64).
    pub world_transform: DMat4,
    /// Rebasing origin (typically the tile's bounding-volume center).
    /// Positions come out as f32 *relative to this origin*.
    pub origin_ecef: DVec3,
}

/// Decoded, render-ready content. Positions are f32 in a local frame
/// (already rebased — see `docs/01-architecture.md`, precision protocol).
#[derive(Debug, Clone)]
pub struct DecodedTileContent {
    pub meshes: Vec<DecodedMesh>,
    /// Textures this tile **owns**: the images its own glTF payload carried.
    ///
    /// Distinct from [`DecodedTileContent::imagery`], and deliberately so. A
    /// b3dm building's façade belongs to that building and to nothing else;
    /// draped imagery belongs to the map and is shared by every tile it covers.
    /// Conflating the two is what made every terrain tile carry a private copy
    /// of the same pixels.
    pub textures: Vec<DecodedTexture>,
    /// Imagery draped over this tile, referenced rather than owned. Empty for
    /// content that carries its own texturing.
    pub imagery: Vec<crate::raster::ImageryLayer>,
    /// The drape this tile would have carried, when its layers were
    /// deliberately **not** fetched because the consumer already holds the
    /// composed result.
    ///
    /// `None` is the ordinary case and means nothing was withheld — an empty
    /// [`DecodedTileContent::imagery`] then means what it always meant: this
    /// content carries no draped imagery.
    ///
    /// `Some(identity)` is a promise and a demand at once. The promise: the
    /// pixels exist, under this identity, wherever the consumer keeps them, and
    /// they are the same pixels the layers would have composed into — see
    /// [`crate::raster::drape_identity`]. The demand: a consumer that set this
    /// must resolve it. Treating `Some` as "no imagery" would drape nothing
    /// over real terrain and report success, which is bare ground nobody is
    /// told about.
    pub withheld_drape: Option<u64>,
    /// Who the content belongs to, as its glTF payload says (`asset.copyright`):
    /// the credit a consumer must show while it draws this tile. `None` for
    /// content that names nobody, and for every mesh built rather than read.
    pub copyright: Option<std::sync::Arc<str>>,
    /// The rebasing origin, f64 ECEF.
    pub local_origin_ecef: DVec3,
    /// Residual transform to apply at render time (identity: everything is
    /// baked into the positions; kept for future non-baking strategies).
    pub transform_local: Mat4,
}

impl DecodedTileContent {
    /// Approximate CPU-side size in bytes, for the resident-cache budget —
    /// **excluding draped imagery**.
    ///
    /// Imagery is shared: twenty terrain tiles routinely drape one Bing tile.
    /// Charging each of them the full texture would report twenty copies of
    /// memory that was allocated once, and the cache would then evict geometry
    /// to reclaim bytes that do not exist — spending the sharing on nothing.
    ///
    /// Imagery is budgeted where it is actually held, keyed by `ImageryCoord`,
    /// which is the only place the count can be right. What this number means
    /// is therefore "what dropping this tile would free", and that is what a
    /// resident cache needs it to mean.
    pub fn byte_size(&self) -> usize {
        let meshes: usize = self
            .meshes
            .iter()
            .map(|m| {
                m.positions.len() * 12
                    + m.normals.as_ref().map_or(0, |n| n.len() * 12)
                    + m.uvs.as_ref().map_or(0, |u| u.len() * 8)
                    + m.indices.len() * 4
            })
            .sum();
        let textures: usize = self.textures.iter().map(|t| t.texels.byte_len()).sum();
        meshes + textures
    }

    /// What this tile's imagery would cost **if it were not shared** — the sum
    /// over its layers, counting a texture once per tile that references it.
    ///
    /// Only meaningful as a diagnostic: against the true cost of the same
    /// layers held once each, the ratio is the sharing factor, and that number
    /// is the whole argument for referencing rather than resampling. Never feed
    /// it to a budget; see [`DecodedTileContent::byte_size`].
    pub fn imagery_byte_size_unshared(&self) -> usize {
        self.imagery
            .iter()
            .map(|l| l.texture.texels.byte_len())
            .sum()
    }
}

/// The vertex a consumer uploads per position: position, normal and uv, as
/// f32 — what the cost estimator charges per vertex.
pub const VERTEX_STRIDE: u64 = 32;

impl DecodedTileContent {
    /// What this content costs a consumer that uploads it, by the budget's
    /// shared estimator: its meshes interleaved (with the CPU copy a consumer
    /// keeps for stitching), its per-tile uniform, and the textures it owns
    /// with their mip chains. Draped imagery is a resource of its own
    /// ([`DrapeContent`]) and is not counted here.
    pub fn cost(&self) -> crate::budget::cost::Cost {
        use crate::budget::cost::mesh_cost;
        let vertices: u64 = self.meshes.iter().map(|m| m.positions.len() as u64).sum();
        let indices: u64 = self.meshes.iter().map(|m| m.indices.len() as u64).sum();
        let mut cost = mesh_cost(vertices, VERTEX_STRIDE, indices, true);
        for t in &self.textures {
            let c = t.cost();
            cost.gpu = cost.gpu.plus(c.gpu);
            cost.staging = cost.staging.plus(c.staging);
        }
        cost
    }
}

/// One tile's imagery of one layer at one level: a drape, a resource of its
/// own beside the tile's mesh (D-A2), so imagery sharpens and coarsens
/// without the mesh being sent again (I10).
///
/// The imagery tiles it is made of are placed in the tile's own uv square,
/// in stacking order: a coarse floor that covers the whole tile first, then
/// the mosaic of the level asked for, each masked to its own coverage. A
/// consumer drapes them over the tile's mesh, or over a descendant's through
/// a [`UvTransform`](crate::budget::UvTransform) when the descendant's own
/// drape is not drawable yet.
#[derive(Debug, Clone)]
pub struct DrapeContent {
    /// The layer it is of.
    pub layer: crate::budget::LayerId,
    /// The budget's imagery level it was composed at.
    pub level: u32,
    /// The imagery tiles, placed in the tile's uv square, in stacking order.
    pub tiles: Vec<crate::raster::ImageryLayer>,
    /// How many of [`Self::tiles`], from the first, are the coarse floor
    /// under the mosaic: shared by every drape over a region, and not
    /// charged to this one.
    pub floor_tiles: usize,
    /// What makes it different from another: see
    /// [`crate::raster::drape_identity`].
    pub identity: u64,
    /// The tiles were deliberately **not** fetched, because the consumer said
    /// it already holds the composed drape under [`Self::identity`] (a bake
    /// whose packs hold it). A promise and a demand at once: the pixels exist
    /// wherever the consumer keeps them, and a consumer that set this must
    /// resolve them — an empty drape read as "no imagery" would be bare
    /// ground nobody is told about. Never set for a consumer that did not ask.
    pub withheld: bool,
}

impl DrapeContent {
    /// What it costs a consumer: each imagery tile of the mosaic with its
    /// mip chain, in the share of it the drape covers — the shared floor
    /// excepted.
    ///
    /// An imagery tile is uploaded once however many drapes name it, and the
    /// drapes over neighbouring ground at one level name the same tiles where
    /// they meet: charging each drape the whole of every tile it touches
    /// counted a tile straddling an edge twice and a coarse one over a corner
    /// four times. Charged by covered area, drapes over a partition of the
    /// ground at one level charge each tile once between them. Two drapes
    /// over the same ground at the same level (a parent's and its child's)
    /// still charge the overlap twice: an over-estimate, never an under-one.
    /// Its staging bytes — what an upload moves — are every tile's whole.
    pub fn cost(&self) -> crate::budget::cost::Cost {
        // In the format each texture is held in, recorded on it.
        self.cost_with(|_, t| t.texture.cost())
    }

    /// [`Self::cost`], with each imagery tile's whole cost said by `whole` —
    /// given its index in [`Self::tiles`] and the tile — rather than read off
    /// its texture: what a consumer acknowledges once it
    /// knows the format each tile really went resident in — a texture headed
    /// for a block format that had to stay RGBA8 costs RGBA8.
    pub fn cost_with(
        &self,
        mut whole: impl FnMut(usize, &crate::raster::ImageryLayer) -> crate::budget::cost::Cost,
    ) -> crate::budget::cost::Cost {
        use crate::budget::cost::Cost;
        use crate::budget::Bytes;
        self.tiles
            .iter()
            .enumerate()
            .skip(self.floor_tiles)
            .map(|(i, t)| {
                let whole = whole(i, t);
                // The tile spans 1 / scale of the drape's uv square on each
                // axis; the part of it inside the square is its coverage.
                let c = t.coverage;
                let covered = (c[2].min(1.0) - c[0].max(0.0)).max(0.0) as f64
                    * (c[3].min(1.0) - c[1].max(0.0)).max(0.0) as f64;
                let share =
                    (covered * f64::from(t.scale[0]) * f64::from(t.scale[1])).clamp(0.0, 1.0);
                // In parts per million: the same integers on every platform.
                let ppm = (share * 1_000_000.0).round() as u64;
                // Held: the share. Moved: the whole tile, for an upload is a
                // texture's, whoever else names it — what an executor's
                // frame deadline and the delivery credit measure.
                Cost {
                    gpu: Bytes(whole.gpu.0).scaled(ppm, 1_000_000),
                    staging: whole.staging,
                }
            })
            .fold(Cost::default(), |a, c| Cost {
                gpu: a.gpu.plus(c.gpu),
                staging: a.staging.plus(c.staging),
            })
    }
}

#[derive(Debug, Clone)]
pub struct DecodedMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub uvs: Option<Vec<[f32; 2]>>,
    pub indices: Vec<u32>,
    pub material: MaterialDesc,
}

#[derive(Debug, Clone)]
pub struct DecodedTexture {
    pub width: u32,
    pub height: u32,
    /// The texels, and the format they are held in once resident.
    pub texels: crate::texels::Texels,
}

impl DecodedTexture {
    /// What holding this texture actually costs a consumer, mip chain included.
    ///
    /// A full chain adds exactly a third: each level is a quarter of the one
    /// above, and `1/4 + 1/16 + …` sums to `1/3`. Deterministic, so there is no
    /// excuse for a budget that counts only the base.
    ///
    /// Counting only the base is what let a session hold 3386 MiB of imagery
    /// under a 3072 MiB budget without a single eviction: the cache measured
    /// 2540 MiB, the GPU held the other third, and the two never disagreed
    /// loudly enough to notice.
    ///
    /// Blocks carry their chain already, so what they hold is what they carry.
    /// RGBA8 headed for a block format is held at that format's chain.
    pub fn resident_bytes(&self) -> usize {
        use crate::texels::{ImageryFormat, Texels};
        match &self.texels {
            Texels::Rgba8 {
                pixels,
                resident: ImageryFormat::Rgba8,
            } => pixels.len() * 4 / 3,
            Texels::Rgba8 { .. } => self.cost().gpu.0 as usize,
            Texels::Blocks { .. } => self.texels.byte_len(),
        }
    }

    /// RGBA8 pixels held as RGBA8: what every producer makes today.
    pub fn from_rgba8(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        DecodedTexture {
            width,
            height,
            texels: crate::texels::Texels::rgba8(pixels),
        }
    }

    /// The same texture, held as `plan` says: RGBA8 the consumer's GPU
    /// encodes at upload is marked with the format it will be held in, so its
    /// cost follows. Blocks, and any plan that does not encode on the GPU,
    /// are left as they are.
    pub fn planned(mut self, plan: crate::texels::ImageryPlan) -> Self {
        if let crate::texels::Texels::Rgba8 { resident, .. } = &mut self.texels {
            *resident = plan.rgba8_resident();
        }
        self
    }

    /// The RGBA8 pixels, if that is what this texture carries.
    pub fn rgba8(&self) -> Option<&[u8]> {
        self.texels.as_rgba8()
    }

    /// The RGBA8 pixels, mutably, if that is what this texture carries.
    pub fn rgba8_mut(&mut self) -> Option<&mut Vec<u8>> {
        match &mut self.texels {
            crate::texels::Texels::Rgba8 { pixels, .. } => Some(pixels),
            crate::texels::Texels::Blocks { .. } => None,
        }
    }

    /// The RGBA8 pixels, taken, if that is what this texture carries.
    pub fn into_rgba8(self) -> Option<Vec<u8>> {
        match self.texels {
            crate::texels::Texels::Rgba8 { pixels, .. } => Some(pixels),
            crate::texels::Texels::Blocks { .. } => None,
        }
    }

    /// The format it is held in once resident.
    pub fn resident_format(&self) -> crate::texels::ImageryFormat {
        self.texels.resident()
    }

    /// What it costs a consumer whole, mip chain included, in the format it
    /// is held in: see [`Texels::cost`](crate::texels::Texels::cost).
    pub fn cost(&self) -> crate::budget::cost::Cost {
        self.texels.cost(self.width, self.height)
    }

    /// What it costs held in `format` instead of the format it records: the
    /// chain in that format, and the same bytes moved. A consumer that could
    /// not hold it as planned — an encode that failed, a device without the
    /// format — charges this.
    pub fn cost_as(&self, format: crate::texels::ImageryFormat) -> crate::budget::cost::Cost {
        let moved = self.cost().staging;
        crate::budget::cost::Cost {
            gpu: crate::budget::cost::drape_cost(self.width, self.height, format.cost_format()).gpu,
            staging: moved,
        }
    }

    /// A stand-in for a texture a consumer cannot read: one texel of the
    /// slate the ground shows where no imagery reaches.
    ///
    /// For the paths that only take RGBA8 and are handed blocks — never by
    /// design, and if ever, ground shown flat beats ground not shown.
    pub fn stand_in() -> Self {
        DecodedTexture::from_rgba8(1, 1, STAND_IN_RGBA.to_vec())
    }
}

/// The colour of [`DecodedTexture::stand_in`]: a muted slate, close to deep
/// water and to ground in shadow, never black.
pub const STAND_IN_RGBA: [u8; 4] = [41, 51, 61, 255];

#[derive(Debug, Clone, Copy)]
pub struct MaterialDesc {
    pub base_color_factor: [f32; 4],
    /// Index into [`DecodedTileContent::textures`].
    pub base_color_texture: Option<usize>,
    /// The material declares `KHR_materials_unlit`: its colour already
    /// carries its light (photogrammetry), and it is not to be shaded by its
    /// own facets.
    pub unlit: bool,
}

impl Default for MaterialDesc {
    fn default() -> Self {
        Self {
            base_color_factor: [1.0; 4],
            base_color_texture: None,
            unlit: false,
        }
    }
}

/// glTF is Y-up; the tileset frame is Z-up. The spec mandates this implicit
/// rotation on glTF content (+90° around X): (x, y, z) → (x, -z, y).
fn y_up_to_z_up() -> DMat4 {
    DMat4::from_rotation_x(std::f64::consts::FRAC_PI_2)
}

/// Decodes a raw payload (glb or b3dm) into render-ready geometry.
///
/// Blocking, no I/O. The b3dm `RTC_CENTER`, the Y-up→Z-up rotation, the
/// glTF node hierarchy and the tile transform are all composed in f64, then
/// positions are rebased onto `hints.origin_ecef` and cast to f32.
pub fn decode(bytes: &[u8], hints: &ContentHints) -> Result<DecodedTileContent, ContentError> {
    match sniff(bytes)? {
        ContentFormat::Glb => decode_glb(bytes, hints, DVec3::ZERO),
        ContentFormat::B3dm => {
            let b3dm = tuile_b3dm::parse(bytes)?;
            let rtc = b3dm
                .rtc_center()?
                .map(|[x, y, z]| DVec3::new(x, y, z))
                .unwrap_or(DVec3::ZERO);
            decode_glb(b3dm.glb, hints, rtc)
        }
    }
}

fn decode_glb(
    glb: &[u8],
    hints: &ContentHints,
    rtc_center: DVec3,
) -> Result<DecodedTileContent, ContentError> {
    // Skip glTF semantic validation: 3D Tiles content routinely carries
    // custom vertex attributes (`_BATCHID`, `_FEATURE_ID_0`, …) that the
    // strict validator rejects but that we simply ignore. We still load
    // buffers and images exactly like `import_slice` does.
    let gltf::Gltf {
        document: doc,
        blob,
    } = gltf::Gltf::from_slice_without_validation(glb)?;
    let buffers = gltf::import_buffers(&doc, None, blob)?;
    let images = gltf::import_images(&doc, None, &buffers)?;

    // tile world × RTC translation × Y-up→Z-up; glTF node transforms
    // compose underneath.
    let content_world =
        hints.world_transform * DMat4::from_translation(rtc_center) * y_up_to_z_up();

    let textures = images.iter().map(to_rgba8).collect::<Result<Vec<_>, _>>()?;

    let mut meshes = Vec::new();
    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or(ContentError::Malformed("glb without scene"))?;
    for node in scene.nodes() {
        walk_node(
            &node,
            content_world,
            hints.origin_ecef,
            &buffers,
            &mut meshes,
        )?;
    }

    let copyright: Option<std::sync::Arc<str>> = doc
        .as_json()
        .asset
        .copyright
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(std::sync::Arc::from);

    Ok(DecodedTileContent {
        withheld_drape: None,
        copyright,
        meshes,
        textures,
        // glTF content carries its own texturing; draping happens above.
        imagery: Vec::new(),
        local_origin_ecef: hints.origin_ecef,
        transform_local: Mat4::IDENTITY,
    })
}

fn walk_node(
    node: &gltf::Node<'_>,
    parent_world: DMat4,
    origin: DVec3,
    buffers: &[gltf::buffer::Data],
    out: &mut Vec<DecodedMesh>,
) -> Result<(), ContentError> {
    let local = DMat4::from_cols_array_2d(&node.transform().matrix().map(|c| c.map(f64::from)));
    let world = parent_world * local;

    if let Some(mesh) = node.mesh() {
        let normal_matrix = glam::DMat3::from_mat4(world).inverse().transpose();
        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                // Lines/points content is out of v1 scope; skip rather than fail
                // the whole tile.
                continue;
            }
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            let positions: Vec<[f32; 3]> = positions
                .map(|p| {
                    let wp = world.transform_point3(DVec3::new(
                        f64::from(p[0]),
                        f64::from(p[1]),
                        f64::from(p[2]),
                    ));
                    let local = wp - origin;
                    [local.x as f32, local.y as f32, local.z as f32]
                })
                .collect();

            let normals = reader.read_normals().map(|ns| {
                ns.map(|n| {
                    let wn = (normal_matrix
                        * glam::DVec3::new(f64::from(n[0]), f64::from(n[1]), f64::from(n[2])))
                    .normalize_or_zero();
                    [wn.x as f32, wn.y as f32, wn.z as f32]
                })
                .collect::<Vec<_>>()
            });

            let uvs = reader
                .read_tex_coords(0)
                .map(|tc| tc.into_f32().collect::<Vec<_>>());

            let indices = match reader.read_indices() {
                Some(idx) => idx.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };

            let pbr = prim.material().pbr_metallic_roughness();
            let material = MaterialDesc {
                base_color_factor: pbr.base_color_factor(),
                base_color_texture: pbr
                    .base_color_texture()
                    .map(|info| info.texture().source().index()),
                unlit: prim.material().unlit(),
            };

            out.push(DecodedMesh {
                positions,
                normals,
                uvs,
                indices,
                material,
            });
        }
    }

    for child in node.children() {
        walk_node(&child, world, origin, buffers, out)?;
    }
    Ok(())
}

/// Converts a gltf-imported image to tightly packed RGBA8.
fn to_rgba8(img: &gltf::image::Data) -> Result<DecodedTexture, ContentError> {
    use gltf::image::Format;
    let px = (img.width as usize) * (img.height as usize);
    let mut rgba8 = vec![0u8; px * 4];
    match img.format {
        Format::R8G8B8A8 => rgba8.copy_from_slice(&img.pixels),
        Format::R8G8B8 => {
            for i in 0..px {
                rgba8[i * 4..i * 4 + 3].copy_from_slice(&img.pixels[i * 3..i * 3 + 3]);
                rgba8[i * 4 + 3] = 255;
            }
        }
        Format::R8 => {
            for i in 0..px {
                let v = img.pixels[i];
                rgba8[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        Format::R8G8 => {
            for i in 0..px {
                let v = img.pixels[i * 2];
                rgba8[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, img.pixels[i * 2 + 1]]);
            }
        }
        other => return Err(ContentError::Texture(format!("{other:?}"))),
    }
    Ok(DecodedTexture::from_rgba8(img.width, img.height, rgba8))
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::budget::cost::{drape_cost, TextureFormat};
    use crate::raster::{GeoRect, ImageryCoord, ImageryLayer};

    /// An imagery tile of 256² placed over a terrain tile, its own rectangle
    /// `imagery` and the terrain tile's `tile`.
    fn placed(tile: &GeoRect, imagery: &GeoRect, x: u64) -> ImageryLayer {
        ImageryLayer::placed(
            ImageryCoord { level: 9, x, y: 0 },
            std::sync::Arc::new(DecodedTexture::from_rgba8(256, 256, Vec::new())),
            tile,
            imagery,
        )
    }

    fn drape(tiles: Vec<ImageryLayer>) -> DrapeContent {
        DrapeContent {
            layer: crate::budget::LayerId(0),
            level: 9,
            tiles,
            floor_tiles: 0,
            identity: 0,
            withheld: false,
        }
    }

    /// **A drape is charged the share of each imagery tile it covers, so two
    /// neighbours sharing a tile charge it once between them.** The imagery
    /// tiles a drape names are shared with every drape over the same ground
    /// at the same level; charging each drape the whole of every tile it
    /// touches counted a tile straddling an edge twice, and a coarse drape
    /// over a corner four times — a ledger two to four times what the GPU
    /// held, and a budget that evicted at a third of its room.
    #[test]
    fn neighbouring_drapes_charge_a_shared_imagery_tile_once() {
        let whole = drape_cost(256, 256, TextureFormat::Rgba8).gpu.0;
        // One imagery tile [0, 2] × [0, 1], straddling two terrain tiles
        // [0, 1] and [1, 2].
        let imagery = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 2.0,
            north: 1.0,
        };
        let west = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        let east = GeoRect {
            west: 1.0,
            south: 0.0,
            east: 2.0,
            north: 1.0,
        };
        let a = drape(vec![placed(&west, &imagery, 0)]).cost().gpu.0;
        let b = drape(vec![placed(&east, &imagery, 0)]).cost().gpu.0;
        assert!(
            a.abs_diff(whole / 2) <= whole / 100,
            "{a} for half of {whole}"
        );
        assert!(
            (a + b).abs_diff(whole) <= whole / 100,
            "{} for one tile of {whole}",
            a + b
        );
        // A tile wholly inside is charged whole.
        let inside = drape(vec![placed(&imagery, &imagery, 0)]).cost().gpu.0;
        assert!(inside.abs_diff(whole) <= whole / 100);
    }

    use super::*;
    use glam::dvec3;

    /// Builds a minimal valid glb: one triangle with asymmetric vertices
    /// (1,0,0), (0,2,0), (0,0,3) so up-axis mistakes are visible.
    pub(crate) fn test_glb() -> Vec<u8> {
        glb_with_asset(serde_json::json!({ "version": "2.0" }))
    }

    /// [`test_glb`], with `asset` as its glTF `asset` object.
    fn glb_with_asset(asset: serde_json::Value) -> Vec<u8> {
        glb_with(
            asset,
            serde_json::json!({ "pbrMetallicRoughness": { "baseColorFactor": [1.0, 0.5, 0.25, 1.0] } }),
        )
    }

    /// [`test_glb`], with `asset` as its glTF `asset` object and `material`
    /// as its one material.
    fn glb_with(asset: serde_json::Value, material: serde_json::Value) -> Vec<u8> {
        let positions: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0]];
        let indices: [u16; 3] = [0, 1, 2];

        let mut bin = Vec::new();
        for p in &positions {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let idx_offset = bin.len();
        for i in &indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }

        let json = serde_json::json!({
            "asset": asset,
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "mesh": 0 }],
            "meshes": [{ "primitives": [{
                "attributes": { "POSITION": 0 },
                "indices": 1,
                "material": 0
            }]}],
            "materials": [material],
            "buffers": [{ "byteLength": bin.len() }],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": idx_offset, "target": 34962 },
                { "buffer": 0, "byteOffset": idx_offset, "byteLength": 6, "target": 34963 }
            ],
            "accessors": [
                { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
                  "min": [0.0, 0.0, 0.0], "max": [1.0, 2.0, 3.0] },
                { "bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR" }
            ]
        });
        let mut json_bytes = serde_json::to_vec(&json).expect("glb json");
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }

        let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::with_capacity(total);
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    /// Wraps a glb in a b3dm with the given feature-table JSON.
    pub(crate) fn wrap_b3dm(glb: &[u8], feature_table: &str) -> Vec<u8> {
        tuile_b3dm::encode(glb, feature_table)
    }

    fn hints(origin: DVec3) -> ContentHints {
        ContentHints {
            world_transform: DMat4::IDENTITY,
            origin_ecef: origin,
        }
    }

    fn pos(d: &DecodedTileContent, i: usize) -> DVec3 {
        let p = d.meshes[0].positions[i];
        dvec3(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
    }

    #[test]
    fn sniff_identifies_formats() {
        assert_eq!(sniff(b"glTFxxxx").ok(), Some(ContentFormat::Glb));
        assert_eq!(sniff(b"b3dmxxxx").ok(), Some(ContentFormat::B3dm));
        assert!(matches!(
            sniff(b"pntsxxxx"),
            Err(ContentError::Unsupported(_))
        ));
    }

    #[test]
    fn glb_decodes_with_up_axis_rotation() {
        let d = decode(&test_glb(), &hints(DVec3::ZERO)).expect("decode");
        assert_eq!(d.meshes.len(), 1);
        // glTF Y-up → Z-up: (x, y, z) → (x, -z, y).
        assert!((pos(&d, 0) - dvec3(1.0, 0.0, 0.0)).length() < 1e-6);
        assert!((pos(&d, 1) - dvec3(0.0, 0.0, 2.0)).length() < 1e-6);
        assert!((pos(&d, 2) - dvec3(0.0, -3.0, 0.0)).length() < 1e-6);
        assert_eq!(d.meshes[0].indices, vec![0, 1, 2]);
        assert_eq!(
            d.meshes[0].material.base_color_factor,
            [1.0, 0.5, 0.25, 1.0]
        );
    }

    /// The credit a glTF payload names in `asset.copyright` comes out with
    /// the content, trimmed; a payload that names nobody gives `None`, and so
    /// does an empty one.
    #[test]
    fn the_copyright_a_glb_states_is_kept() {
        let named = glb_with_asset(serde_json::json!({
            "version": "2.0",
            "copyright": " Data SIO, NOAA; Landsat / Copernicus "
        }));
        let d = decode(&named, &hints(DVec3::ZERO)).expect("decode");
        assert_eq!(
            d.copyright.as_deref(),
            Some("Data SIO, NOAA; Landsat / Copernicus")
        );
        let silent = decode(&test_glb(), &hints(DVec3::ZERO)).expect("decode");
        assert_eq!(silent.copyright, None);
        let empty = glb_with_asset(serde_json::json!({ "version": "2.0", "copyright": "  " }));
        let d = decode(&empty, &hints(DVec3::ZERO)).expect("decode");
        assert_eq!(d.copyright, None);
    }

    /// A material that declares `KHR_materials_unlit` is decoded as unlit —
    /// what the photorealistic 3D Tiles declare, and require — and one that
    /// does not, as lit.
    #[test]
    fn an_unlit_material_is_decoded_unlit() {
        let unlit = glb_with(
            serde_json::json!({ "version": "2.0" }),
            serde_json::json!({
                "pbrMetallicRoughness": { "baseColorFactor": [1.0, 1.0, 1.0, 1.0] },
                "extensions": { "KHR_materials_unlit": {} }
            }),
        );
        let d = decode(&unlit, &hints(DVec3::ZERO)).expect("decode");
        assert!(d.meshes[0].material.unlit);
        let lit = decode(&test_glb(), &hints(DVec3::ZERO)).expect("decode");
        assert!(!lit.meshes[0].material.unlit);
    }

    #[test]
    fn b3dm_matches_equivalent_glb() {
        let glb = test_glb();
        let from_glb = decode(&glb, &hints(DVec3::ZERO)).expect("glb");
        let b3dm = wrap_b3dm(&glb, r#"{"BATCH_LENGTH":0}"#);
        let from_b3dm = decode(&b3dm, &hints(DVec3::ZERO)).expect("b3dm");
        assert_eq!(from_glb.meshes[0].positions, from_b3dm.meshes[0].positions);
    }

    #[test]
    fn b3dm_rtc_center_translates() {
        let glb = test_glb();
        let b3dm = wrap_b3dm(&glb, r#"{"BATCH_LENGTH":0,"RTC_CENTER":[10.0,20.0,30.0]}"#);
        let d = decode(&b3dm, &hints(DVec3::ZERO)).expect("b3dm");
        // RTC translation applies in tile space, before the up-axis rotation
        // of the glTF payload: vertex0 = rtc + yup2zup * (1,0,0).
        assert!((pos(&d, 0) - dvec3(11.0, 20.0, 30.0)).length() < 1e-6);
    }

    #[test]
    fn rebasing_subtracts_origin_in_f64() {
        let origin = dvec3(6.4e6, 1.0e6, 2.0e6);
        let h = ContentHints {
            world_transform: DMat4::from_translation(origin),
            origin_ecef: origin,
        };
        let d = decode(&test_glb(), &h).expect("decode");
        // World position is origin + rotated vertex; rebased = rotated vertex,
        // exactly representable in f32 (this is the anti-jitter protocol).
        assert!((pos(&d, 0) - dvec3(1.0, 0.0, 0.0)).length() < 1e-6);
        assert_eq!(d.local_origin_ecef, origin);
    }

    #[test]
    fn malformed_b3dm_is_a_typed_error_never_a_panic() {
        // Truncated header (full container validation lives in tuile-b3dm;
        // here we check the error surfaces through decode() as a typed error).
        assert!(matches!(
            decode(b"b3dm\x01\x00\x00\x00", &hints(DVec3::ZERO)),
            Err(ContentError::B3dm(_))
        ));
        // Header with table lengths overflowing the payload.
        let mut bad = Vec::new();
        bad.extend_from_slice(b"b3dm");
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.extend_from_slice(&28u32.to_le_bytes()); // byteLength
        bad.extend_from_slice(&9999u32.to_le_bytes()); // ftJson overflows
        bad.extend_from_slice(&[0u8; 12]);
        assert!(matches!(
            decode(&bad, &hints(DVec3::ZERO)),
            Err(ContentError::B3dm(_))
        ));
        // Valid header, garbage glb inside.
        let garbage = wrap_b3dm(b"glTFgarbage-not-a-real-payload", "{}");
        assert!(decode(&garbage, &hints(DVec3::ZERO)).is_err());
    }

    #[test]
    fn normals_are_rotated_with_the_content() {
        // Add normals pointing +Y in glTF space: after Y-up→Z-up they must
        // point +Z in world space.
        let d = decode(&test_glb(), &hints(DVec3::ZERO)).expect("decode");
        assert!(d.meshes[0].normals.is_none(), "test glb has no normals");
        // (The rotation itself is covered through positions; a normal-equipped
        // fixture lands with the wgpu crate's quad fixture.)
    }

    #[test]
    fn byte_size_accounts_geometry() {
        let d = decode(&test_glb(), &hints(DVec3::ZERO)).expect("decode");
        assert_eq!(d.byte_size(), 3 * 12 + 3 * 4);
    }

    /// A 256² texture with its whole chain encoded: 87,408 bytes of blocks.
    fn blocks_256(format: crate::texels::BlockFormat) -> DecodedTexture {
        use crate::texels::{BlockFormat, Texels};
        DecodedTexture {
            width: 256,
            height: 256,
            texels: Texels::Blocks {
                format,
                levels: (0..9)
                    .map(|l| vec![0; BlockFormat::level_bytes(256 >> l, 256 >> l)])
                    .collect(),
            },
        }
    }

    fn whole(texture: DecodedTexture) -> ImageryLayer {
        let rect = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        ImageryLayer::placed(
            ImageryCoord {
                level: 9,
                x: 0,
                y: 0,
            },
            std::sync::Arc::new(texture),
            &rect,
            &rect,
        )
    }

    fn owning(textures: Vec<DecodedTexture>) -> DecodedTileContent {
        DecodedTileContent {
            meshes: Vec::new(),
            textures,
            imagery: Vec::new(),
            withheld_drape: None,
            copyright: None,
            local_origin_ecef: DVec3::ZERO,
            transform_local: Mat4::IDENTITY,
        }
    }

    /// **A drape held as blocks is charged a quarter**: 87,408 bytes for a
    /// 256² tile with its chain against RGBA8's 349,524, whether the worker
    /// encoded the blocks or the GPU encodes them at upload. What it moves
    /// follows what crosses: blocks from the worker, RGBA8 to the GPU.
    #[test]
    fn a_drape_held_as_blocks_is_charged_a_quarter() {
        use crate::budget::Bytes;
        use crate::texels::{BlockFormat, ImageryFormat, Texels};
        let rgba = drape(vec![whole(DecodedTexture::from_rgba8(
            256,
            256,
            Vec::new(),
        ))])
        .cost();
        assert_eq!(rgba, drape_cost(256, 256, TextureFormat::Rgba8));
        assert_eq!(rgba.gpu, Bytes(349_524));
        for format in [BlockFormat::Bc7, BlockFormat::Astc4x4] {
            let worker = drape(vec![whole(blocks_256(format))]).cost();
            assert_eq!(
                worker,
                drape_cost(256, 256, TextureFormat::Block16),
                "{format:?}"
            );
            assert_eq!(worker.gpu, Bytes(87_408), "{format:?}");
            let on_gpu = DecodedTexture {
                width: 256,
                height: 256,
                texels: Texels::Rgba8 {
                    pixels: Vec::new(),
                    resident: ImageryFormat::from(format),
                },
            };
            let gpu = drape(vec![whole(on_gpu)]).cost();
            assert_eq!(gpu.gpu, Bytes(87_408), "{format:?}");
            assert_eq!(gpu.staging, rgba.staging, "{format:?}");
        }
    }

    /// **A drape is acknowledged in the format each tile really went
    /// resident in.** Planned as blocks and held as blocks, it is charged
    /// what [`DrapeContent::cost`] says; a tile that had to stay RGBA8 (an
    /// encode refused) is charged RGBA8, and the bytes moved do not change.
    #[test]
    fn a_drape_is_acknowledged_in_the_format_it_went_resident_in() {
        use crate::budget::Bytes;
        use crate::texels::{ImageryFormat, Texels};
        let planned = DecodedTexture {
            width: 256,
            height: 256,
            texels: Texels::Rgba8 {
                pixels: Vec::new(),
                resident: ImageryFormat::Bc7,
            },
        };
        let d = drape(vec![whole(planned)]);
        assert_eq!(d.cost_with(|_, t| t.texture.cost()), d.cost());
        assert_eq!(d.cost().gpu, Bytes(87_408));
        let fell_back = d.cost_with(|_, t| t.texture.cost_as(ImageryFormat::Rgba8));
        assert_eq!(fell_back.gpu, Bytes(349_524));
        assert_eq!(fell_back.staging, d.cost().staging, "the same bytes moved");
        // RGBA8 charged as RGBA8 is exactly what it always was.
        let rgba = DecodedTexture::from_rgba8(256, 256, Vec::new());
        assert_eq!(rgba.cost_as(ImageryFormat::Rgba8), rgba.cost());
    }

    /// **A plan that encodes on the GPU marks RGBA8 with its target**, and
    /// the charge follows; RGBA8, the CPU path and blocks are left alone.
    #[test]
    fn a_texture_is_held_as_its_plan_says() {
        use crate::texels::{
            BlockFormat, GpuTextureCaps, ImageryCompression, ImageryFormat, ImageryPlan,
        };
        let apple = GpuTextureCaps {
            bc: true,
            astc: true,
            compute: true,
        };
        let rgba = || DecodedTexture::from_rgba8(256, 256, vec![7; 256 * 256 * 4]);
        assert_eq!(
            rgba().planned(ImageryPlan::RGBA8).resident_format(),
            ImageryFormat::Rgba8
        );
        assert_eq!(
            rgba().planned(ImageryCompression::Off.plan(apple)).cost(),
            rgba().cost(),
            "off is RGBA8, byte for byte"
        );
        let bc7 = rgba().planned(ImageryCompression::Auto.plan(apple));
        assert_eq!(bc7.resident_format(), ImageryFormat::Bc7);
        assert_eq!(bc7.cost().gpu.0, 87_408);
        assert_eq!(
            bc7.rgba8().map(<[u8]>::len),
            Some(256 * 256 * 4),
            "pixels kept"
        );
        let astc = rgba().planned(ImageryCompression::Format(BlockFormat::Astc4x4).plan(apple));
        assert_eq!(astc.resident_format(), ImageryFormat::Astc4x4);
        // No compute: the CPU encoder's plan hands over blocks, not RGBA8.
        let webgl2 = GpuTextureCaps {
            compute: false,
            ..apple
        };
        assert_eq!(
            rgba()
                .planned(ImageryCompression::Auto.plan(webgl2))
                .resident_format(),
            ImageryFormat::Rgba8
        );
        assert_eq!(
            blocks_256(BlockFormat::Astc4x4)
                .planned(ImageryCompression::Auto.plan(apple))
                .resident_format(),
            ImageryFormat::Astc4x4
        );
    }

    /// The textures a tile owns are charged in the format they are held in.
    #[test]
    fn owned_blocks_are_charged_a_quarter() {
        use crate::texels::BlockFormat;
        let none = owning(Vec::new()).cost().gpu.0;
        let rgba = owning(vec![DecodedTexture::from_rgba8(256, 256, Vec::new())])
            .cost()
            .gpu
            .0;
        let blocks = owning(vec![blocks_256(BlockFormat::Bc7)]).cost().gpu.0;
        assert_eq!(rgba - none, 349_524);
        assert_eq!(blocks - none, 87_408);
    }

    /// Byte sizes count what each form holds: RGBA8 pixels and a third more
    /// for the chain a consumer builds; blocks, their levels as carried.
    #[test]
    fn byte_sizes_per_format() {
        use crate::texels::{BlockFormat, ImageryFormat, Texels};
        let rgba = DecodedTexture::from_rgba8(256, 256, vec![0; 256 * 256 * 4]);
        assert_eq!(rgba.resident_bytes(), 349_525);
        assert_eq!(rgba.resident_format(), ImageryFormat::Rgba8);
        let blocks = blocks_256(BlockFormat::Astc4x4);
        assert_eq!(blocks.resident_bytes(), 87_408);
        assert_eq!(blocks.resident_format(), ImageryFormat::Astc4x4);
        let on_gpu = DecodedTexture {
            width: 256,
            height: 256,
            texels: Texels::Rgba8 {
                pixels: vec![0; 256 * 256 * 4],
                resident: ImageryFormat::Bc7,
            },
        };
        assert_eq!(on_gpu.resident_bytes(), 87_408);

        let mut tile = owning(vec![rgba.clone()]);
        assert_eq!(tile.byte_size(), 256 * 256 * 4);
        tile.textures = vec![blocks_256(BlockFormat::Bc7)];
        assert_eq!(tile.byte_size(), 87_408);
        tile.imagery = vec![whole(rgba), whole(blocks_256(BlockFormat::Bc7))];
        assert_eq!(tile.imagery_byte_size_unshared(), 256 * 256 * 4 + 87_408);
    }
}
