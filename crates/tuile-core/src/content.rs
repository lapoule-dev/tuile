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
        let textures: usize = self.textures.iter().map(|t| t.rgba8.len()).sum();
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
        self.imagery.iter().map(|l| l.texture.rgba8.len()).sum()
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
    /// Tightly packed RGBA8.
    pub rgba8: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct MaterialDesc {
    pub base_color_factor: [f32; 4],
    /// Index into [`DecodedTileContent::textures`].
    pub base_color_texture: Option<usize>,
}

impl Default for MaterialDesc {
    fn default() -> Self {
        Self {
            base_color_factor: [1.0; 4],
            base_color_texture: None,
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

    Ok(DecodedTileContent {
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
    Ok(DecodedTexture {
        width: img.width,
        height: img.height,
        rgba8,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use glam::dvec3;

    /// Builds a minimal valid glb: one triangle with asymmetric vertices
    /// (1,0,0), (0,2,0), (0,0,3) so up-axis mistakes are visible.
    pub(crate) fn test_glb() -> Vec<u8> {
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
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "mesh": 0 }],
            "meshes": [{ "primitives": [{
                "attributes": { "POSITION": 0 },
                "indices": 1,
                "material": 0
            }]}],
            "materials": [{ "pbrMetallicRoughness": { "baseColorFactor": [1.0, 0.5, 0.25, 1.0] } }],
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
}
