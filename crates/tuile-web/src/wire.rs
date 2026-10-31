// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The streaming protocol as bytes, for `postMessage`.
//!
//! A Web Worker and its page share no memory: what crosses is a structured
//! clone, or an `ArrayBuffer` *transferred* — handed over without a copy. So
//! each [`ClientMessage`] and [`ServerMessage`] is written into one buffer,
//! which the sender transfers and the receiver reads back into the very same
//! types. Nothing is translated into a JavaScript shape on the way: the page
//! hands the decoded messages to the same consumer a desktop does.
//!
//! # Format
//!
//! Little-endian, one tag byte per enum, lengths as `u32`. Floats travel as
//! their bit patterns, so a value is the same value on both sides — the f64
//! origins that carry the anti-jitter rebasing included. There is no version
//! field: both ends are the same wasm module, built together.
//!
//! The matches below are exhaustive on purpose. A variant added to the
//! protocol is a compile error here until it is given an encoding, never a
//! message silently dropped between the worker and the page.

use std::sync::Arc;

use glam::{DVec2, DVec3, Mat4};
use tuile_core::budget::{
    Bytes, Class, DrapeChoice, DrawItem, DrawSet, Generation, LayerId, PerClass, Pressure,
    ResourceId, Role, SwapGroup, Thermal, UploadPlan, UvTransform,
};
use tuile_core::content::{
    DecodedMesh, DecodedTexture, DecodedTileContent, DrapeContent, MaterialDesc,
};
use tuile_core::protocol::{Ancestry, ClientMessage, Priming, ResourceContent, ServerMessage};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_core::source::TileId;
use tuile_core::traversal::{Fov, TraversalStats, ViewState, ViewStateParams};

/// A buffer that does not read back into a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("the message ends {0} bytes early")]
    Truncated(usize),
    #[error("unknown tag {tag} for {what}")]
    Tag { what: &'static str, tag: u8 },
    #[error("{0} bytes left over after the message")]
    Trailing(usize),
    #[error("a message string is not UTF-8")]
    Utf8,
}

/// Writes a message from the page to the worker.
pub fn encode_client(message: &ClientMessage) -> Vec<u8> {
    let mut w = Writer::default();
    match message {
        ClientMessage::ViewerState { views, generation } => {
            w.u8(0);
            write_views(&mut w, views);
            w.u64(*generation);
        }
        ClientMessage::Ack {
            resource,
            gen,
            bytes,
        } => {
            w.u8(1);
            w.resource(resource);
            w.u64(gen.0);
            w.u64(*bytes);
        }
        ClientMessage::Discarded { resource, gen } => {
            w.u8(2);
            w.resource(resource);
            w.u64(gen.0);
        }
        ClientMessage::Pressure(p) => {
            w.u8(3);
            w.u8(match p {
                Pressure::Normal => 0,
                Pressure::Warning => 1,
                Pressure::Critical => 2,
                Pressure::Background => 3,
            });
        }
        ClientMessage::Thermal(t) => {
            w.u8(4);
            w.u8(match t {
                Thermal::Nominal => 0,
                Thermal::Fair => 1,
                Thermal::Serious => 2,
                Thermal::Critical => 3,
            });
        }
        ClientMessage::Retarget { total_bytes } => {
            w.u8(5);
            w.u64(*total_bytes);
        }
        ClientMessage::ReleaseBeyond { bytes } => {
            w.u8(6);
            w.u64(*bytes);
        }
        ClientMessage::Ahead { views } => {
            w.u8(7);
            write_views(&mut w, views);
        }
    }
    w.0
}

/// The views of a message, each with its lens and scale.
fn write_views(w: &mut Writer, views: &[ViewState]) {
    w.len(views.len());
    for view in views {
        let p = ViewStateParams::from(*view);
        w.dvec3(p.position);
        w.dvec3(p.direction);
        w.dvec3(p.up);
        w.f64(p.viewport_px.x);
        w.f64(p.viewport_px.y);
        w.f64(p.fovy_rad);
        match p.fov {
            Some(fov) => {
                w.u8(1);
                for v in [fov.left, fov.right, fov.up, fov.down] {
                    w.f64(v);
                }
            }
            None => w.u8(0),
        }
        w.f64(p.scale);
    }
}

/// The views [`write_views`] wrote.
fn read_views(r: &mut Reader<'_>) -> Result<Vec<ViewState>, WireError> {
    let n = r.len()?;
    let mut views = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        let position = r.dvec3()?;
        let direction = r.dvec3()?;
        let up = r.dvec3()?;
        let viewport_px = DVec2::new(r.f64()?, r.f64()?);
        let fovy_rad = r.f64()?;
        let fov = match r.u8()? {
            0 => None,
            1 => Some(Fov {
                left: r.f64()?,
                right: r.f64()?,
                up: r.f64()?,
                down: r.f64()?,
            }),
            tag => {
                return Err(WireError::Tag {
                    what: "a lens",
                    tag,
                })
            }
        };
        let mut params =
            ViewStateParams::perspective(position, direction, up, viewport_px, fovy_rad);
        params.fov = fov;
        params.scale = r.f64()?;
        views.push(ViewState::from(params));
    }
    Ok(views)
}

/// Reads a message the page wrote.
pub fn decode_client(bytes: &[u8]) -> Result<ClientMessage, WireError> {
    let mut r = Reader::new(bytes);
    let message = match r.u8()? {
        0 => ClientMessage::ViewerState {
            views: read_views(&mut r)?,
            generation: r.u64()?,
        },
        1 => ClientMessage::Ack {
            resource: r.resource()?,
            gen: Generation(r.u64()?),
            bytes: r.u64()?,
        },
        2 => ClientMessage::Discarded {
            resource: r.resource()?,
            gen: Generation(r.u64()?),
        },
        3 => ClientMessage::Pressure(match r.u8()? {
            0 => Pressure::Normal,
            1 => Pressure::Warning,
            2 => Pressure::Critical,
            3 => Pressure::Background,
            tag => {
                return Err(WireError::Tag {
                    what: "a pressure",
                    tag,
                })
            }
        }),
        4 => ClientMessage::Thermal(match r.u8()? {
            0 => Thermal::Nominal,
            1 => Thermal::Fair,
            2 => Thermal::Serious,
            3 => Thermal::Critical,
            tag => {
                return Err(WireError::Tag {
                    what: "a thermal state",
                    tag,
                })
            }
        }),
        5 => ClientMessage::Retarget {
            total_bytes: r.u64()?,
        },
        6 => ClientMessage::ReleaseBeyond { bytes: r.u64()? },
        7 => ClientMessage::Ahead {
            views: read_views(&mut r)?,
        },
        tag => {
            return Err(WireError::Tag {
                what: "a client message",
                tag,
            })
        }
    };
    r.finish()?;
    Ok(message)
}

/// Writes a message from the worker to the page.
pub fn encode_server(message: &ServerMessage) -> Vec<u8> {
    let mut w = Writer::default();
    match message {
        ServerMessage::Select {
            draw,
            plan,
            ancestry,
            stats,
            hard_limit,
            frame_ready,
            converged,
            lost,
            bare,
            generation,
        } => {
            w.u8(0);
            w.len(draw.items.len());
            for item in &draw.items {
                w.item(item);
            }
            w.resources(&plan.items);
            w.u64(plan.bytes.0);
            w.len(plan.groups.len());
            for group in &plan.groups {
                w.resources(&group.members);
            }
            w.len(ancestry.len());
            for (tile, a) in ancestry {
                w.u64(tile.0);
                w.ancestry(a);
            }
            w.stats(stats);
            for (_, b) in hard_limit.iter() {
                w.u64(b.0);
            }
            w.u8(u8::from(*frame_ready));
            w.u8(u8::from(*converged));
            w.u32(*lost);
            w.u32(*bare);
            w.u64(*generation);
        }
        ServerMessage::Content {
            resource,
            gen,
            class,
            ancestry,
            content,
        } => {
            w.u8(1);
            w.resource(resource);
            w.u64(gen.0);
            w.u8(match class {
                Class::TerrainMesh => 0,
                Class::Tiles3dMesh => 1,
                Class::Drape => 2,
                Class::Overlay => 3,
            });
            w.ancestry(ancestry);
            match content {
                ResourceContent::Mesh(mesh) => {
                    w.u8(0);
                    w.decoded(mesh);
                }
                ResourceContent::Drape(drape) => {
                    w.u8(1);
                    w.drape(drape);
                }
            }
        }
        ServerMessage::Cancel { resources } => {
            w.u8(2);
            w.len(resources.len());
            for (r, g) in resources {
                w.resource(r);
                w.u64(g.0);
            }
        }
        ServerMessage::Evict { resources } => {
            w.u8(3);
            w.resources(resources);
        }
        ServerMessage::Error { resource, message } => {
            w.u8(4);
            match resource {
                Some(r) => {
                    w.u8(1);
                    w.resource(r);
                }
                None => w.u8(0),
            }
            w.bytes(message.as_bytes());
        }
        ServerMessage::Priming(p) => {
            w.u8(5);
            w.u32(p.total);
            w.u32(p.unavailable);
            w.u32(p.outstanding);
        }
    }
    w.0
}

/// Reads a message the worker wrote.
pub fn decode_server(bytes: &[u8]) -> Result<ServerMessage, WireError> {
    let mut r = Reader::new(bytes);
    let message = match r.u8()? {
        0 => {
            let n = r.len()?;
            let mut items = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                items.push(r.item()?);
            }
            let plan_items = r.resources()?;
            let plan_bytes = Bytes(r.u64()?);
            let n = r.len()?;
            let mut groups = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                groups.push(SwapGroup {
                    members: r.resources()?,
                });
            }
            let n = r.len()?;
            let mut ancestry = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                ancestry.push((TileId(r.u64()?), r.ancestry()?));
            }
            let stats = r.stats()?;
            let hard_limit = PerClass([
                Bytes(r.u64()?),
                Bytes(r.u64()?),
                Bytes(r.u64()?),
                Bytes(r.u64()?),
            ]);
            ServerMessage::Select {
                draw: DrawSet { items },
                plan: UploadPlan {
                    items: plan_items,
                    bytes: plan_bytes,
                    groups,
                },
                ancestry,
                stats,
                hard_limit,
                frame_ready: r.flag()?,
                converged: r.flag()?,
                lost: r.u32()?,
                bare: r.u32()?,
                generation: r.u64()?,
            }
        }
        1 => {
            let resource = r.resource()?;
            let gen = Generation(r.u64()?);
            let class = match r.u8()? {
                0 => Class::TerrainMesh,
                1 => Class::Tiles3dMesh,
                2 => Class::Drape,
                3 => Class::Overlay,
                tag => {
                    return Err(WireError::Tag {
                        what: "a class",
                        tag,
                    })
                }
            };
            let ancestry = r.ancestry()?;
            let content = match r.u8()? {
                0 => ResourceContent::Mesh(Arc::new(r.decoded()?)),
                1 => ResourceContent::Drape(Arc::new(r.drape()?)),
                tag => {
                    return Err(WireError::Tag {
                        what: "a resource content",
                        tag,
                    })
                }
            };
            ServerMessage::Content {
                resource,
                gen,
                class,
                ancestry,
                content,
            }
        }
        2 => {
            let n = r.len()?;
            let mut resources = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                resources.push((r.resource()?, Generation(r.u64()?)));
            }
            ServerMessage::Cancel { resources }
        }
        3 => ServerMessage::Evict {
            resources: r.resources()?,
        },
        4 => ServerMessage::Error {
            resource: match r.u8()? {
                0 => None,
                1 => Some(r.resource()?),
                tag => {
                    return Err(WireError::Tag {
                        what: "an option",
                        tag,
                    })
                }
            },
            message: String::from_utf8(r.bytes()?.to_vec()).map_err(|_| WireError::Utf8)?,
        },
        5 => ServerMessage::Priming(Priming {
            total: r.u32()?,
            unavailable: r.u32()?,
            outstanding: r.u32()?,
        }),
        tag => {
            return Err(WireError::Tag {
                what: "a server message",
                tag,
            })
        }
    };
    r.finish()?;
    Ok(message)
}

/// Several messages in one buffer, as one `postMessage` carries them: each
/// prefixed with its length.
///
/// One message per post made one event per message on the other side — a
/// frame's acknowledgements, dozens of them, each its own event, and the
/// geometry server deciding once per event on a worker's single thread: the
/// decisions starved the fetches and decodes they were waiting on. A frame's
/// messages now cross together and are decided on once.
pub fn frame(messages: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::default();
    w.len(messages.len());
    for m in messages {
        w.bytes(m);
    }
    w.0
}

/// The messages of a [`frame`], each still encoded.
pub fn unframe(bytes: &[u8]) -> Result<Vec<&[u8]>, WireError> {
    let mut r = Reader::new(bytes);
    let n = r.len()?;
    let mut out = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        out.push(r.bytes()?);
    }
    r.finish()?;
    Ok(out)
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// Lengths are `u32`: no message comes near four billion of anything.
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
    fn dvec3(&mut self, v: DVec3) {
        self.f64(v.x);
        self.f64(v.y);
        self.f64(v.z);
    }
    fn floats(&mut self, values: &[f32]) {
        self.len(values.len());
        self.0.reserve(values.len() * 4);
        for v in values {
            self.f32(*v);
        }
    }
    fn option_u64(&mut self, v: Option<u64>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.u64(v);
            }
            None => self.u8(0),
        }
    }
    fn tiles(&mut self, tiles: &[TileId]) {
        self.len(tiles.len());
        for t in tiles {
            self.u64(t.0);
        }
    }
    fn resource(&mut self, r: &ResourceId) {
        match r {
            ResourceId::Mesh(tile) => {
                self.u8(0);
                self.u64(tile.0);
            }
            ResourceId::Drape { tile, layer, level } => {
                self.u8(1);
                self.u64(tile.0);
                self.u32(u32::from(layer.0));
                self.u32(*level);
            }
        }
    }
    fn resources(&mut self, rs: &[ResourceId]) {
        self.len(rs.len());
        for r in rs {
            self.resource(r);
        }
    }
    fn item(&mut self, item: &DrawItem) {
        let DrawItem {
            mesh,
            role,
            drapes,
            serves,
        } = item;
        self.resource(mesh);
        self.u8(match role {
            Role::Exact => 0,
            Role::Held => 1,
            Role::Fallback => 2,
        });
        self.len(drapes.len());
        for DrapeChoice {
            resource,
            layer,
            level,
            uv,
        } in drapes
        {
            self.resource(resource);
            self.u32(u32::from(layer.0));
            self.u32(*level);
            for v in uv.scale.iter().chain(&uv.offset) {
                self.f64(*v);
            }
        }
        self.tiles(serves);
    }
    fn layer(&mut self, layer: &ImageryLayer) {
        let ImageryLayer {
            coord,
            source,
            texture,
            coverage,
            translation,
            scale,
        } = layer;
        self.u32(coord.level);
        self.u64(coord.x);
        self.u64(coord.y);
        self.u32(*source);
        self.texture(texture);
        for f in coverage.iter().chain(translation).chain(scale) {
            self.f32(*f);
        }
    }
    fn drape(&mut self, d: &DrapeContent) {
        let DrapeContent {
            layer,
            level,
            tiles,
            floor_tiles,
            identity,
            withheld,
        } = d;
        self.u32(u32::from(layer.0));
        self.u32(*level);
        self.len(tiles.len());
        for t in tiles {
            self.layer(t);
        }
        self.len(*floor_tiles);
        self.u64(*identity);
        self.u8(u8::from(*withheld));
    }
    fn ancestry(&mut self, a: &Ancestry) {
        self.u32(a.level);
        self.option_u64(a.parent.map(|p| p.0));
    }
    fn stats(&mut self, s: &TraversalStats) {
        let TraversalStats {
            held_but_drawn,
            deferred_subtrees,
            visited,
            culled,
            selected,
            requested,
            max_depth,
            gaps,
        } = *s;
        for v in [
            held_but_drawn,
            deferred_subtrees,
            visited,
            culled,
            selected,
            requested,
            max_depth,
            gaps,
        ] {
            self.u32(v);
        }
    }
    /// A texture as it is: RGBA8 with the format it is to be held in, or
    /// blocks with every level they carry. A tag says which.
    fn texture(&mut self, t: &DecodedTexture) {
        use tuile_core::texels::Texels;
        self.u32(t.width);
        self.u32(t.height);
        match &t.texels {
            Texels::Rgba8 { pixels, resident } => {
                self.u8(TEXELS_RGBA8);
                self.u8(format_tag(*resident));
                self.bytes(pixels);
            }
            Texels::Blocks { format, levels } => {
                self.u8(TEXELS_BLOCKS);
                self.u8(format_tag((*format).into()));
                self.len(levels.len());
                for level in levels {
                    self.bytes(level);
                }
            }
        }
    }
    fn decoded(&mut self, c: &DecodedTileContent) {
        let DecodedTileContent {
            meshes,
            textures,
            imagery,
            withheld_drape,
            copyright,
            local_origin_ecef,
            transform_local,
        } = c;
        self.len(meshes.len());
        for m in meshes {
            let DecodedMesh {
                positions,
                normals,
                uvs,
                indices,
                material,
            } = m;
            self.floats(positions.as_flattened());
            match normals {
                Some(n) => {
                    self.u8(1);
                    self.floats(n.as_flattened());
                }
                None => self.u8(0),
            }
            match uvs {
                Some(uv) => {
                    self.u8(1);
                    self.floats(uv.as_flattened());
                }
                None => self.u8(0),
            }
            self.len(indices.len());
            self.0.reserve(indices.len() * 4);
            for i in indices {
                self.u32(*i);
            }
            for f in material.base_color_factor {
                self.f32(f);
            }
            self.option_u64(material.base_color_texture.map(|t| t as u64));
        }
        self.len(textures.len());
        for t in textures {
            self.texture(t);
        }
        self.len(imagery.len());
        for layer in imagery {
            self.layer(layer);
        }
        self.option_u64(*withheld_drape);
        // Who the tile credits: shown below the picture while it is drawn.
        match copyright {
            Some(text) => {
                self.u8(1);
                self.bytes(text.as_bytes());
            }
            None => self.u8(0),
        }
        self.dvec3(*local_origin_ecef);
        for f in transform_local.to_cols_array() {
            self.f32(f);
        }
    }
}

/// A texture's texels on the wire: RGBA8, or blocks.
const TEXELS_RGBA8: u8 = 0;
const TEXELS_BLOCKS: u8 = 1;

fn format_tag(format: tuile_core::texels::ImageryFormat) -> u8 {
    use tuile_core::texels::ImageryFormat;
    match format {
        ImageryFormat::Rgba8 => 0,
        ImageryFormat::Bc7 => 1,
        ImageryFormat::Astc4x4 => 2,
    }
}

fn format_of(tag: u8) -> Result<tuile_core::texels::ImageryFormat, WireError> {
    use tuile_core::texels::ImageryFormat;
    match tag {
        0 => Ok(ImageryFormat::Rgba8),
        1 => Ok(ImageryFormat::Bc7),
        2 => Ok(ImageryFormat::Astc4x4),
        tag => Err(WireError::Tag {
            what: "an imagery format",
            tag,
        }),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.saturating_add(n);
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(WireError::Truncated(end.saturating_sub(self.bytes.len())))?;
        self.at = end;
        Ok(slice)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.array::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn f32(&mut self) -> Result<f32, WireError> {
        Ok(f32::from_le_bytes(self.array()?))
    }
    fn f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_le_bytes(self.array()?))
    }
    fn len(&mut self) -> Result<usize, WireError> {
        Ok(self.u32()? as usize)
    }
    fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let n = self.len()?;
        self.take(n)
    }
    fn dvec3(&mut self) -> Result<DVec3, WireError> {
        Ok(DVec3::new(self.f64()?, self.f64()?, self.f64()?))
    }
    fn floats(&mut self) -> Result<Vec<f32>, WireError> {
        let n = self.len()?;
        let raw = self.take(n.saturating_mul(4))?;
        Ok(raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect())
    }
    fn vec3s(&mut self) -> Result<Vec<[f32; 3]>, WireError> {
        Ok(self.floats()?.as_chunks::<3>().0.to_vec())
    }
    fn vec2s(&mut self) -> Result<Vec<[f32; 2]>, WireError> {
        Ok(self.floats()?.as_chunks::<2>().0.to_vec())
    }
    fn option_u64(&mut self) -> Result<Option<u64>, WireError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            tag => Err(WireError::Tag {
                what: "an option",
                tag,
            }),
        }
    }
    fn flag(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(WireError::Tag {
                what: "a flag",
                tag,
            }),
        }
    }
    fn layer_id(&mut self) -> Result<LayerId, WireError> {
        let raw = self.u32()?;
        u16::try_from(raw).map(LayerId).map_err(|_| WireError::Tag {
            what: "a layer",
            tag: u8::MAX,
        })
    }
    fn resource(&mut self) -> Result<ResourceId, WireError> {
        match self.u8()? {
            0 => Ok(ResourceId::Mesh(TileId(self.u64()?))),
            1 => Ok(ResourceId::Drape {
                tile: TileId(self.u64()?),
                layer: self.layer_id()?,
                level: self.u32()?,
            }),
            tag => Err(WireError::Tag {
                what: "a resource",
                tag,
            }),
        }
    }
    fn resources(&mut self) -> Result<Vec<ResourceId>, WireError> {
        let n = self.len()?;
        let mut out = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            out.push(self.resource()?);
        }
        Ok(out)
    }
    fn item(&mut self) -> Result<DrawItem, WireError> {
        let mesh = self.resource()?;
        let role = match self.u8()? {
            0 => Role::Exact,
            1 => Role::Held,
            2 => Role::Fallback,
            tag => {
                return Err(WireError::Tag {
                    what: "a role",
                    tag,
                })
            }
        };
        let n = self.len()?;
        let mut drapes = tuile_core::budget::Drapes::with_capacity(n.min(16));
        for _ in 0..n {
            drapes.push(DrapeChoice {
                resource: self.resource()?,
                layer: self.layer_id()?,
                level: self.u32()?,
                uv: UvTransform {
                    scale: [self.f64()?, self.f64()?],
                    offset: [self.f64()?, self.f64()?],
                },
            });
        }
        Ok(DrawItem {
            mesh,
            role,
            drapes,
            serves: self.tiles()?.into(),
        })
    }
    fn layer(&mut self) -> Result<ImageryLayer, WireError> {
        let coord = ImageryCoord {
            level: self.u32()?,
            x: self.u64()?,
            y: self.u64()?,
        };
        let source = self.u32()?;
        let texture = Arc::new(self.texture()?);
        Ok(ImageryLayer {
            coord,
            source,
            texture,
            coverage: self.f32s()?,
            translation: self.f32s()?,
            scale: self.f32s()?,
        })
    }
    fn drape(&mut self) -> Result<DrapeContent, WireError> {
        let layer = self.layer_id()?;
        let level = self.u32()?;
        let n = self.len()?;
        let mut tiles = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            tiles.push(self.layer()?);
        }
        Ok(DrapeContent {
            layer,
            level,
            tiles,
            floor_tiles: self.len()?,
            identity: self.u64()?,
            withheld: self.flag()?,
        })
    }
    fn tiles(&mut self) -> Result<Vec<TileId>, WireError> {
        let n = self.len()?;
        let mut tiles = Vec::with_capacity(n);
        for _ in 0..n {
            tiles.push(TileId(self.u64()?));
        }
        Ok(tiles)
    }
    fn ancestry(&mut self) -> Result<Ancestry, WireError> {
        Ok(Ancestry {
            level: self.u32()?,
            parent: self.option_u64()?.map(TileId),
        })
    }
    fn stats(&mut self) -> Result<TraversalStats, WireError> {
        Ok(TraversalStats {
            held_but_drawn: self.u32()?,
            deferred_subtrees: self.u32()?,
            visited: self.u32()?,
            culled: self.u32()?,
            selected: self.u32()?,
            requested: self.u32()?,
            max_depth: self.u32()?,
            gaps: self.u32()?,
        })
    }
    fn texture(&mut self) -> Result<DecodedTexture, WireError> {
        use tuile_core::texels::Texels;
        let (width, height) = (self.u32()?, self.u32()?);
        let kind = self.u8()?;
        let format = format_of(self.u8()?)?;
        let texels = match kind {
            TEXELS_RGBA8 => Texels::Rgba8 {
                pixels: self.bytes()?.to_vec(),
                resident: format,
            },
            TEXELS_BLOCKS => {
                let format = format.block().ok_or(WireError::Tag {
                    what: "a block format",
                    tag: format_tag(format),
                })?;
                let n = self.len()?;
                let mut levels = Vec::with_capacity(n.min(16));
                for _ in 0..n {
                    levels.push(self.bytes()?.to_vec());
                }
                Texels::Blocks { format, levels }
            }
            tag => {
                return Err(WireError::Tag {
                    what: "texels",
                    tag,
                })
            }
        };
        Ok(DecodedTexture {
            width,
            height,
            texels,
        })
    }
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], WireError> {
        let mut out = [0.0; N];
        for v in &mut out {
            *v = self.f32()?;
        }
        Ok(out)
    }
    fn decoded(&mut self) -> Result<DecodedTileContent, WireError> {
        let n = self.len()?;
        let mut meshes = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let positions = self.vec3s()?;
            let normals = match self.u8()? {
                0 => None,
                _ => Some(self.vec3s()?),
            };
            let uvs = match self.u8()? {
                0 => None,
                _ => Some(self.vec2s()?),
            };
            let count = self.len()?;
            let raw = self.take(count.saturating_mul(4))?;
            let indices = raw
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect();
            let material = MaterialDesc {
                base_color_factor: self.f32s()?,
                base_color_texture: self.option_u64()?.map(|t| t as usize),
            };
            meshes.push(DecodedMesh {
                positions,
                normals,
                uvs,
                indices,
                material,
            });
        }
        let n = self.len()?;
        let mut textures = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            textures.push(self.texture()?);
        }
        let n = self.len()?;
        let mut imagery = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            imagery.push(self.layer()?);
        }
        Ok(DecodedTileContent {
            meshes,
            textures,
            imagery,
            withheld_drape: self.option_u64()?,
            copyright: match self.u8()? {
                0 => None,
                _ => Some(Arc::from(
                    std::str::from_utf8(self.bytes()?).map_err(|_| WireError::Utf8)?,
                )),
            },
            local_origin_ecef: self.dvec3()?,
            transform_local: Mat4::from_cols_array(&self.f32s()?),
        })
    }
    fn finish(self) -> Result<(), WireError> {
        match self.bytes.len() - self.at {
            0 => Ok(()),
            left => Err(WireError::Trailing(left)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> DecodedTileContent {
        DecodedTileContent {
            meshes: vec![DecodedMesh {
                positions: vec![[1.0, -2.5, 3.25], [f32::MIN_POSITIVE, 0.0, -0.0]],
                normals: Some(vec![[0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]),
                uvs: None,
                indices: vec![0, 1, 1, u32::MAX],
                material: MaterialDesc {
                    base_color_factor: [0.1, 0.2, 0.3, 1.0],
                    base_color_texture: Some(0),
                },
            }],
            textures: vec![DecodedTexture::from_rgba8(1, 1, vec![1, 2, 3, 4])],
            imagery: vec![ImageryLayer {
                coord: ImageryCoord {
                    level: 12,
                    x: 2048,
                    y: 1_000_001,
                },
                source: 3,
                texture: Arc::new(DecodedTexture::from_rgba8(2, 1, vec![9; 8])),
                coverage: [0.0, 0.25, 0.5, 1.0],
                translation: [0.125, -0.5],
                scale: [2.0, 4.0],
            }],
            withheld_drape: Some(u64::MAX - 1),
            copyright: Some(Arc::from("Data © someone; Imagery © someone else")),
            // The anti-jitter origin: every bit of the f64 must survive.
            local_origin_ecef: DVec3::new(4_201_234.123_456_789, 172_001.000_000_1, -4_779_000.5),
            transform_local: Mat4::from_translation(glam::Vec3::new(1.0, 2.0, 3.0)),
        }
    }

    fn same_content(a: &DecodedTileContent, b: &DecodedTileContent) {
        assert_eq!(a.meshes.len(), b.meshes.len());
        for (m, n) in a.meshes.iter().zip(&b.meshes) {
            assert_eq!(m.positions, n.positions);
            assert_eq!(m.normals, n.normals);
            assert_eq!(m.uvs, n.uvs);
            assert_eq!(m.indices, n.indices);
            assert_eq!(m.material.base_color_factor, n.material.base_color_factor);
            assert_eq!(m.material.base_color_texture, n.material.base_color_texture);
        }
        assert_eq!(a.textures.len(), b.textures.len());
        for (t, u) in a.textures.iter().zip(&b.textures) {
            assert_eq!(
                (t.width, t.height, &t.rgba8()),
                (u.width, u.height, &u.rgba8())
            );
        }
        assert_eq!(a.imagery.len(), b.imagery.len());
        for (l, k) in a.imagery.iter().zip(&b.imagery) {
            assert_eq!((l.coord, l.source), (k.coord, k.source));
            assert_eq!(l.texture.rgba8(), k.texture.rgba8());
            assert_eq!(
                (l.coverage, l.translation, l.scale),
                (k.coverage, k.translation, k.scale)
            );
        }
        assert_eq!(a.withheld_drape, b.withheld_drape);
        assert_eq!(a.copyright, b.copyright, "the credits cross");
        assert_eq!(
            a.local_origin_ecef.to_array().map(f64::to_bits),
            b.local_origin_ecef.to_array().map(f64::to_bits)
        );
        assert_eq!(a.transform_local, b.transform_local);
    }

    fn round_trip(message: &ServerMessage) -> ServerMessage {
        decode_server(&encode_server(message)).expect("reads back")
    }

    fn drape_content() -> DrapeContent {
        DrapeContent {
            layer: LayerId(2),
            level: 11,
            tiles: content().imagery,
            floor_tiles: 1,
            identity: 0xdead_beef,
            withheld: true,
        }
    }

    /// **Every texture crosses as it is**: blocks with their levels and
    /// format, and RGBA8 with the format it is to be held in — never the
    /// stand-in slate.
    #[test]
    fn blocks_and_planned_rgba8_cross_whole() {
        use tuile_core::texels::{BlockFormat, ImageryFormat, Texels};
        let mut sent = content();
        let blocks = DecodedTexture {
            width: 4,
            height: 4,
            texels: Texels::Blocks {
                format: BlockFormat::Astc4x4,
                levels: vec![vec![7; 16], vec![8; 16], vec![9; 16]],
            },
        };
        let planned = DecodedTexture {
            width: 2,
            height: 2,
            texels: Texels::Rgba8 {
                pixels: vec![5; 16],
                resident: ImageryFormat::Bc7,
            },
        };
        sent.textures = vec![blocks.clone(), planned.clone()];
        let back = round_trip(&ServerMessage::Content {
            resource: ResourceId::Mesh(TileId(42)),
            gen: Generation(9),
            class: Class::TerrainMesh,
            ancestry: Ancestry {
                level: 7,
                parent: None,
            },
            content: ResourceContent::Mesh(Arc::new(sent)),
        });
        let ServerMessage::Content {
            content: ResourceContent::Mesh(got),
            ..
        } = back
        else {
            panic!("a mesh came back as something else");
        };
        for (got, sent) in got.textures.iter().zip([&blocks, &planned]) {
            assert_eq!((got.width, got.height), (sent.width, sent.height));
            assert_eq!(got.texels, sent.texels);
        }
    }

    /// A mesh crosses whole: meshes, owned textures, the f64 origin bit for
    /// bit — with its resource, generation and class.
    #[test]
    fn a_mesh_crosses_whole() {
        let ancestry = Ancestry {
            level: 7,
            parent: Some(TileId(41)),
        };
        let back = round_trip(&ServerMessage::Content {
            resource: ResourceId::Mesh(TileId(42)),
            gen: Generation(9),
            class: Class::TerrainMesh,
            ancestry,
            content: ResourceContent::Mesh(Arc::new(content())),
        });
        let ServerMessage::Content {
            resource,
            gen,
            class,
            ancestry: a,
            content: ResourceContent::Mesh(decoded),
        } = back
        else {
            panic!("not a mesh: {back:?}");
        };
        assert_eq!(
            (resource, gen, class, a),
            (
                ResourceId::Mesh(TileId(42)),
                Generation(9),
                Class::TerrainMesh,
                ancestry
            )
        );
        same_content(&content(), &decoded);
    }

    /// A drape crosses whole: its layer and level, its imagery tiles with
    /// their placement, the floor, its identity and whether it was withheld.
    #[test]
    fn a_drape_crosses_whole() {
        let resource = ResourceId::drape(TileId(42), LayerId(2), 11);
        let back = round_trip(&ServerMessage::Content {
            resource,
            gen: Generation(3),
            class: Class::Drape,
            ancestry: Ancestry {
                level: 7,
                parent: None,
            },
            content: ResourceContent::Drape(Arc::new(drape_content())),
        });
        let ServerMessage::Content {
            resource: r,
            class,
            content: ResourceContent::Drape(d),
            ..
        } = back
        else {
            panic!("not a drape: {back:?}");
        };
        assert_eq!((r, class), (resource, Class::Drape));
        let want = drape_content();
        assert_eq!(
            (d.layer, d.level, d.floor_tiles, d.identity, d.withheld),
            (
                want.layer,
                want.level,
                want.floor_tiles,
                want.identity,
                want.withheld
            )
        );
        assert_eq!(d.tiles.len(), want.tiles.len());
        for (l, k) in d.tiles.iter().zip(&want.tiles) {
            assert_eq!((l.coord, l.source), (k.coord, k.source));
            assert_eq!(l.texture.rgba8(), k.texture.rgba8());
            assert_eq!(
                (l.coverage, l.translation, l.scale),
                (k.coverage, k.translation, k.scale)
            );
        }
    }

    /// The draw set crosses as the budget decided it — roles, worn drapes
    /// and their uv transforms bit for bit, what each item serves — with the
    /// upload plan, the ancestry, the hard limits and the frame's signals.
    #[test]
    fn a_selection_keeps_its_draw_set_plan_and_signals() {
        let stats = TraversalStats {
            held_but_drawn: 1,
            deferred_subtrees: 2,
            visited: 3,
            culled: 4,
            selected: 5,
            requested: 6,
            max_depth: 7,
            gaps: 8,
        };
        let drape = ResourceId::drape(TileId(1), LayerId(0), 6);
        let draw = DrawSet {
            items: vec![
                DrawItem {
                    mesh: ResourceId::Mesh(TileId(1)),
                    role: Role::Fallback,
                    drapes: [DrapeChoice {
                        resource: drape,
                        layer: LayerId(0),
                        level: 6,
                        uv: UvTransform {
                            scale: [0.5, 0.25],
                            offset: [0.5, 1.0 / 3.0],
                        },
                    }]
                    .into_iter()
                    .collect(),
                    serves: [TileId(5), TileId(6)].into_iter().collect(),
                },
                DrawItem {
                    mesh: ResourceId::Mesh(TileId(5)),
                    role: Role::Exact,
                    drapes: Default::default(),
                    serves: [TileId(5)].into_iter().collect(),
                },
                DrawItem {
                    mesh: ResourceId::Mesh(TileId(9)),
                    role: Role::Held,
                    drapes: Default::default(),
                    serves: [TileId(2)].into_iter().collect(),
                },
            ],
        };
        let plan = UploadPlan {
            items: vec![drape, ResourceId::Mesh(TileId(5))],
            bytes: Bytes(1 << 20),
            groups: vec![SwapGroup {
                members: vec![drape, ResourceId::Mesh(TileId(5))],
            }],
        };
        let hard_limit = PerClass([Bytes(1), Bytes(2), Bytes(3), Bytes::MAX]);
        let message = ServerMessage::Select {
            draw: draw.clone(),
            plan: plan.clone(),
            ancestry: vec![(
                TileId(5),
                Ancestry {
                    level: 3,
                    parent: Some(TileId(1)),
                },
            )],
            stats,
            hard_limit,
            frame_ready: true,
            converged: false,
            lost: 4,
            bare: 2,
            generation: 77,
        };
        let ServerMessage::Select {
            draw: d,
            plan: p,
            ancestry,
            stats: s,
            hard_limit: h,
            frame_ready,
            converged,
            lost,
            bare,
            generation,
        } = round_trip(&message)
        else {
            panic!("not a selection");
        };
        assert_eq!(d, draw);
        assert_eq!(p, plan);
        assert_eq!(ancestry[0].1.parent, Some(TileId(1)));
        assert_eq!((s, h), (stats, hard_limit));
        assert_eq!(
            (frame_ready, converged, lost, bare, generation),
            (true, false, 4, 2, 77)
        );
    }

    /// A cancel names each resource with its generation; an eviction names
    /// resources only. Neither is taken for the other.
    #[test]
    fn cancel_and_evict_stay_apart() {
        let resources = vec![
            ResourceId::Mesh(TileId(1)),
            ResourceId::drape(TileId(u64::MAX), LayerId(u16::MAX), u32::MAX),
        ];
        assert!(matches!(
            round_trip(&ServerMessage::Evict { resources: resources.clone() }),
            ServerMessage::Evict { resources: r } if r == resources
        ));
        let cancelled: Vec<_> = resources
            .iter()
            .map(|r| (*r, Generation(u64::MAX)))
            .collect();
        assert!(matches!(
            round_trip(&ServerMessage::Cancel { resources: cancelled.clone() }),
            ServerMessage::Cancel { resources: r } if r == cancelled
        ));
    }

    #[test]
    fn errors_and_priming_cross() {
        let r = ResourceId::drape(TileId(3), LayerId(1), 4);
        assert!(matches!(
            round_trip(&ServerMessage::Error { resource: Some(r), message: "é 404".into() }),
            ServerMessage::Error { resource: Some(back), message } if back == r && message == "é 404"
        ));
        assert!(matches!(
            round_trip(&ServerMessage::Error {
                resource: None,
                message: String::new()
            }),
            ServerMessage::Error { resource: None, .. }
        ));
        let p = Priming {
            total: 85,
            unavailable: 4,
            outstanding: 2,
        };
        assert!(
            matches!(round_trip(&ServerMessage::Priming(p)), ServerMessage::Priming(q) if q == p)
        );
    }

    /// The camera crosses exactly — a symmetric one, and an eye of four
    /// angles at a display scale — so the server culls against what the page
    /// sees and counts its need in the page's logical pixels.
    #[test]
    fn client_messages_cross() {
        let view = ViewState::perspective(
            DVec3::new(6_400_000.0, 1.0, -2.0),
            DVec3::new(-1.0, 0.0, 0.0),
            DVec3::Z,
            DVec2::new(1280.0, 720.0),
            0.8,
        );
        let eye = ViewState::from_fov(
            DVec3::new(6_400_000.0, 1.0, -2.0),
            DVec3::new(-1.0, 0.0, 0.0),
            DVec3::Z,
            DVec2::new(1440.0, 1600.0),
            Fov {
                left: -0.9,
                right: 0.7,
                up: 0.8,
                down: -0.95,
            },
        )
        .with_scale(2.0);
        let message = ClientMessage::ViewerState {
            views: vec![view, eye],
            generation: 12,
        };
        let ClientMessage::ViewerState { views, generation } =
            decode_client(&encode_client(&message)).expect("reads back")
        else {
            panic!("not a viewer state");
        };
        assert_eq!(generation, 12);
        let same = |views: &[ViewState]| {
            assert_eq!(views.len(), 2);
            for (sent, got) in [view, eye].iter().zip(views) {
                let (a, b) = (ViewStateParams::from(*sent), ViewStateParams::from(*got));
                assert_eq!(
                    (a.position, a.direction, a.up),
                    (b.position, b.direction, b.up)
                );
                assert_eq!((a.viewport_px, a.fovy_rad), (b.viewport_px, b.fovy_rad));
                assert_eq!((a.fov, a.scale), (b.fov, b.scale));
            }
        };
        same(&views);
        // The frames ahead cross the same way.
        let ClientMessage::Ahead { views } = decode_client(&encode_client(&ClientMessage::Ahead {
            views: vec![view, eye],
        }))
        .expect("reads back") else {
            panic!("not the views ahead");
        };
        same(&views);
        let drape = ResourceId::drape(TileId(4), LayerId(3), 12);
        let mut messages = vec![
            ClientMessage::Ack {
                resource: drape,
                gen: Generation(7),
                bytes: 1 << 33,
            },
            ClientMessage::Discarded {
                resource: ResourceId::Mesh(TileId(5)),
                gen: Generation(8),
            },
            ClientMessage::Retarget {
                total_bytes: 3 << 30,
            },
            ClientMessage::ReleaseBeyond { bytes: 1 << 30 },
        ];
        messages.extend(
            [
                Pressure::Normal,
                Pressure::Warning,
                Pressure::Critical,
                Pressure::Background,
            ]
            .map(ClientMessage::Pressure),
        );
        messages.extend(
            [
                Thermal::Nominal,
                Thermal::Fair,
                Thermal::Serious,
                Thermal::Critical,
            ]
            .map(ClientMessage::Thermal),
        );
        for message in messages {
            let back = decode_client(&encode_client(&message)).expect("reads back");
            assert_eq!(format!("{back:?}"), format!("{message:?}"));
        }
    }

    /// A frame carries its messages whole and in order, and a damaged one
    /// is refused.
    #[test]
    fn a_frame_carries_its_messages_in_order() {
        let messages = [
            ClientMessage::ReleaseBeyond { bytes: 3 },
            ClientMessage::Ack {
                resource: ResourceId::Mesh(TileId(9)),
                gen: Generation(2),
                bytes: 77,
            },
            ClientMessage::Retarget { total_bytes: 5 },
        ];
        let encoded: Vec<Vec<u8>> = messages.iter().map(encode_client).collect();
        let framed = frame(&encoded);
        let back: Vec<String> = unframe(&framed)
            .expect("unframes")
            .into_iter()
            .map(|b| format!("{:?}", decode_client(b).expect("reads back")))
            .collect();
        let want: Vec<String> = messages.iter().map(|m| format!("{m:?}")).collect();
        assert_eq!(back, want);
        for cut in 0..framed.len() {
            assert!(unframe(&framed[..cut]).is_err(), "cut at {cut}");
        }
    }

    /// A damaged buffer is an error, never a panic or a wrong message.
    #[test]
    fn a_damaged_buffer_is_refused() {
        let bytes = encode_server(&ServerMessage::Content {
            resource: ResourceId::drape(TileId(1), LayerId(0), 3),
            gen: Generation(1),
            class: Class::Drape,
            ancestry: Ancestry {
                level: 0,
                parent: None,
            },
            content: ResourceContent::Drape(Arc::new(drape_content())),
        });
        for cut in 0..bytes.len() {
            assert!(decode_server(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode_server(&longer).err(), Some(WireError::Trailing(1)));
        assert!(matches!(
            decode_server(&[99]),
            Err(WireError::Tag { tag: 99, .. })
        ));
        assert!(decode_client(&[]).is_err());
    }
}
