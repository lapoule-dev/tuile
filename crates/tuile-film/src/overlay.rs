// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a host adds to a film's frames: shapes, placed in the world.
//!
//! An overlay is geometry — coloured triangles in Earth coordinates — drawn
//! in the same scene as the ground and through the same camera. It is not a
//! picture laid over the frame afterwards: a shape that passes behind a
//! ridge is hidden by the ridge, pixel by pixel.
//!
//! Nothing here knows what a shape stands for. The host decides what to
//! draw, where and in what colour, frame by frame; the renderer draws
//! triangles.

use tuile_pack::BakedView;

/// What a mesh is tested against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayDepth {
    /// Hidden wherever the ground is nearer: what lives in the landscape.
    #[default]
    Terrain,
    /// Never hidden: what must stay readable whatever stands before it.
    Always,
}

/// One mesh of a frame's overlays.
///
/// Positions are f32 **relative to `origin_ecef`**, which stays in f64: the
/// renderer subtracts the frame's eye from it before anything is narrowed,
/// as it does for a tile. Keep the origin near the mesh — a mesh stored
/// against the Earth's centre has lost its centimetres already.
///
/// Meshes are drawn in the order they are given, and so are a mesh's
/// triangles: overlays are tested against the ground, not against one
/// another, and a later triangle is laid over an earlier one. Geometry that
/// lies *on* the ground is the host's to lift clear of it.
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
    /// Triangles, three indices each, either winding.
    pub indices: Vec<u32>,
    pub depth: OverlayDepth,
}

impl OverlayMesh {
    /// What is wrong with the mesh, if anything: a renderer refuses it
    /// rather than read past its vertices.
    pub fn fault(&self) -> Option<&'static str> {
        if self.colors.len() != self.positions.len() {
            return Some("an overlay mesh has not one colour a vertex");
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

/// The overlays of a film, asked for frame by frame.
pub trait Overlays {
    /// Adds to `out` what `frame` shows beside the ground. `view` is the
    /// camera the frame was baked for; `out` comes in empty.
    fn frame(&mut self, frame: u32, view: &BakedView, out: &mut Vec<OverlayMesh>);
}

/// No overlays.
impl Overlays for () {
    fn frame(&mut self, _: u32, _: &BakedView, _: &mut Vec<OverlayMesh>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle() -> OverlayMesh {
        OverlayMesh {
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            colors: vec![[1.0; 4]; 3],
            indices: vec![0, 1, 2],
            ..OverlayMesh::default()
        }
    }

    #[test]
    fn a_whole_mesh_has_no_fault_and_is_tested_against_the_ground() {
        let mesh = triangle();
        assert_eq!(mesh.fault(), None);
        assert_eq!(mesh.depth, OverlayDepth::Terrain);
    }

    #[test]
    fn a_mesh_that_would_be_read_past_its_end_is_named() {
        let mut short = triangle();
        short.colors.pop();
        assert!(short.fault().is_some());
        let mut open = triangle();
        open.indices.pop();
        assert!(open.fault().is_some());
        let mut past = triangle();
        past.indices[2] = 3;
        assert!(past.fault().is_some());
    }
}
