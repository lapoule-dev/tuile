// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a host adds to the scene: shapes, placed in the world.
//!
//! An overlay is geometry — coloured triangles in Earth coordinates — drawn
//! in the same scene as the ground and through the same camera. It is not a
//! picture laid over the frame afterwards: a shape that passes behind a
//! ridge is hidden by the ridge, pixel by pixel.
//!
//! Nothing here knows what a shape stands for. The host decides what to
//! draw, where and in what colour, frame by frame; the renderer draws
//! triangles.
//!
//! # Why these types are their own crate
//!
//! They began in `tuile-film`, because a film was the only thing that drew
//! them. An interactive viewer draws them too — a host that is replaying a
//! camera, or previewing one, wants the same trail and the same badge — and
//! it has no film: no pack, no baked view, no look. Everything here is data
//! and arithmetic, so it costs that host nothing.
//!
//! What stayed behind is what a film's overlays are *made for*:
//! `OverlayCamera` carries a `BakedView` and the frame's own camera, which
//! are a film's own vocabulary.

pub mod gpu;

/// Which eye of a stereo pair is being drawn.
///
/// Here rather than in `tuile-film` because [`OverlayEyes`] answers questions
/// about it, and a crate that draws overlays into one eye of a headset should
/// not have to take a film to name the eye.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Eye {
    Left,
    Right,
}

impl Eye {
    pub const BOTH: [Eye; 2] = [Eye::Left, Eye::Right];
}

/// What a mesh is tested against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayDepth {
    /// Hidden wherever the ground, or an opaque overlay, is nearer: what
    /// lives in the landscape.
    #[default]
    Terrain,
    /// Never hidden: what must stay readable whatever stands before it.
    Always,
    /// Hidden by nearer opaque overlays and never by the ground: shapes
    /// placed before the scene, whose own layers are to stand in front of
    /// one another as their depths say.
    OverlaysOnly,
}

/// How a mesh is laid over what is behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayBlend {
    /// Drawn first, and it hides the overlays behind it: its depth is
    /// written, so any overlay tested against overlays is hidden where it
    /// is nearer.
    Opaque,
    /// Drawn after every opaque mesh, over what is before it in the order
    /// given, and hides nothing: its depth is read, never written.
    #[default]
    Translucent,
}

/// Which eyes of a stereo film draw a mesh. A film of one eye draws every
/// mesh, whatever it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayEyes {
    #[default]
    Both,
    Left,
    Right,
}

impl OverlayEyes {
    /// Whether the picture drawn for `eye` — `None` for a film of one eye —
    /// shows the mesh.
    pub fn shows(self, eye: Option<Eye>) -> bool {
        match (self, eye) {
            (_, None) | (OverlayEyes::Both, _) => true,
            (OverlayEyes::Left, Some(eye)) => eye == Eye::Left,
            (OverlayEyes::Right, Some(eye)) => eye == Eye::Right,
        }
    }
}

/// What a mesh has to do with the sun's shadows, when the film's look has
/// any (`tuile_film::Look::shadow`); nothing otherwise.
///
/// A mesh that **casts** is drawn into the same map of the sun as the
/// ground, once a frame and for both eyes: the ground under it, and any
/// mesh that receives, lose the sun where it stands before them. A mesh
/// that **receives** loses the sun where the ground, or a mesh that casts,
/// stands between it and the sun — darkened as shaded ground is, to what
/// the dome alone gives.
///
/// A caster is seen by the sun on a grid of texels
/// (`tuile_film::OverlayCamera::shadow_texel` wide, in metres): a shape narrower than
/// [`MIN_SHADOW_TEXELS`] of them, as the sun sees it, casts an outline a
/// texel wide at most — its edges are drawn into the map as well, so that
/// it never vanishes — and only one that wide or wider casts its whole
/// shadow. A host that wants a thin ribbon's shadow whole widens what casts
/// it.
///
/// A mesh that **projects** stays out of the map: its shadow is worked out
/// exactly, for each pixel of ground, by following the ground's own ray to
/// the sun and asking whether it meets one of the mesh's triangles. Nothing
/// of a grid of texels is in it, so a thin shape — a line, a ribbon a few
/// metres wide — casts its whole shadow, with edges as sharp as the
/// picture's, and that shadow stands still while the sun moves, as far
/// along the ground as the shape's height over the tangent of the sun's
/// elevation. Its shadow falls on the ground only: no overlay receives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayShadow {
    #[default]
    None,
    Cast,
    Receive,
    CastAndReceive,
    /// Casts by projection, receives nothing.
    Project,
    /// Casts by projection and receives the map's shadows.
    ProjectAndReceive,
}

/// The least width, in texels of the sun's map, of a caster whose shadow
/// comes out whole: see [`OverlayShadow`].
pub const MIN_SHADOW_TEXELS: f32 = 2.0;

impl OverlayShadow {
    pub fn casts(self) -> bool {
        matches!(self, OverlayShadow::Cast | OverlayShadow::CastAndReceive)
    }

    pub fn receives(self) -> bool {
        matches!(
            self,
            OverlayShadow::Receive
                | OverlayShadow::CastAndReceive
                | OverlayShadow::ProjectAndReceive
        )
    }

    /// Whether its shadow is projected onto the ground rather than drawn
    /// into the sun's map.
    pub fn projects(self) -> bool {
        matches!(
            self,
            OverlayShadow::Project | OverlayShadow::ProjectAndReceive
        )
    }
}

/// One mesh of a frame's overlays.
///
/// Positions are f32 **relative to `origin_ecef`**, which stays in f64: the
/// renderer subtracts the frame's eye from it before anything is narrowed,
/// as it does for a tile. Keep the origin near the mesh — a mesh stored
/// against the Earth's centre has lost its centimetres already.
///
/// Opaque meshes are drawn first, then translucent ones, each in the order
/// given, and so are a mesh's triangles: a later translucent triangle is
/// laid over an earlier one. Geometry that lies *on* the ground, or on
/// another overlay, is the host's to lift clear of it.
///
/// Every setting's default is what a mesh was before it had them: flat,
/// translucent, tested against the ground, in both eyes, out of the sun's
/// map.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OverlayMesh {
    pub origin_ecef: [f64; 3],
    /// A position a vertex, in metres from the origin, along ECEF axes.
    pub positions: Vec<[f32; 3]>,
    /// A colour a vertex: display-linear RGB **premultiplied** by its alpha.
    /// Display-linear is the picture's own light, after the film's exposure
    /// and grade — one is the display's white — so a colour comes out as
    /// given whatever the look; only the output curve is still to come.
    pub colors: Vec<[f32; 4]>,
    /// A normal a vertex, along ECEF axes, or none at all. With normals a
    /// mesh is lit, by a light that goes with the camera — a share of its
    /// colour all round, more where it faces the light, a soft highlight —
    /// so that its relief shows; without, its colours are drawn as given.
    pub normals: Vec<[f32; 3]>,
    /// Triangles, three indices each, either winding.
    pub indices: Vec<u32>,
    pub depth: OverlayDepth,
    pub blend: OverlayBlend,
    pub eyes: OverlayEyes,
    pub shadow: OverlayShadow,
}

impl OverlayMesh {
    /// What is wrong with the mesh, if anything: a renderer refuses it
    /// rather than read past its vertices.
    pub fn fault(&self) -> Option<&'static str> {
        if self.colors.len() != self.positions.len() {
            return Some("an overlay mesh has not one colour a vertex");
        }
        if !self.normals.is_empty() && self.normals.len() != self.positions.len() {
            return Some("an overlay mesh has normals, and not one a vertex");
        }
        if !self.indices.len().is_multiple_of(3) {
            return Some("an overlay mesh's indices are not triangles");
        }
        let vertices = self.positions.len() as u64;
        if self.indices.iter().any(|i| u64::from(*i) >= vertices) {
            return Some("an overlay mesh's index is past its vertices");
        }
        None
    }
}
