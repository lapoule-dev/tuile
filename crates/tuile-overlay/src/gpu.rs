// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How an [`OverlayMesh`](crate::OverlayMesh) reaches a vertex buffer, and
//! how its triangles are lit once there.
//!
//! Two renderers draw these meshes — a film's, which composes them into a
//! baked picture under the sun's own maps, and an interactive viewer's,
//! which lays them over streaming ground. They share nothing of their
//! targets, their depth convention or their uniforms, and they must agree
//! exactly on **what a vertex is** and **how it is lit**: the film's HUD is
//! shown in the viewer, at the same frame, and a pixel of difference is a
//! difference in the tool.
//!
//! So this holds the two things that have to be one: the vertex layout, and
//! the WGSL of the light. Neither needs a GPU — one is a `repr(C)` struct,
//! the other is text a renderer concatenates into its own shader — so the
//! crate stays what it was: data and arithmetic, no `wgpu`, wasm-clean.

/// An overlay's vertex, as every renderer of overlays packs it: where it is
/// from the eye, its colour, its normal (zero for none), and what its mesh
/// asks for.
///
/// Positions are **eye-relative metres**: each mesh's `origin_ecef` met the
/// eye in f64 and only the difference was narrowed. Colours are
/// display-linear and premultiplied.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct OverlayVertex {
    pub position: [f32; 3],
    pub color: [f32; 4],
    pub normal: [f32; 3],
    /// [`LIT`], [`RECEIVES`], [`BEHIND_GROUND`].
    pub flags: u32,
    /// How far a caster's edges are pushed away from the sun in its map, in
    /// metres. A renderer that draws no shadows leaves it zero.
    pub sunk: f32,
}

/// The mesh has normals, so it is lit: see [`LIGHT_WGSL`].
pub const LIT: u32 = 1;
/// The mesh loses the sun where something stands before it.
pub const RECEIVES: u32 = 2;
/// The mesh is hidden where the ground is nearer —
/// [`OverlayDepth::Terrain`](crate::OverlayDepth) — which a renderer
/// resolves against the ground's own depth rather than its attachment.
pub const BEHIND_GROUND: u32 = 4;

/// What a mesh's settings come to in [`OverlayVertex::flags`].
///
/// One place, because the three bits are read by two shaders and written by
/// two renderers, and a renderer that got `BEHIND_GROUND` wrong would draw
/// a trail through a ridge with nothing failing anywhere.
pub fn flags(mesh: &crate::OverlayMesh) -> u32 {
    u32::from(!mesh.normals.is_empty()) * LIT
        + u32::from(mesh.shadow.receives()) * RECEIVES
        + u32::from(mesh.depth == crate::OverlayDepth::Terrain) * BEHIND_GROUND
}

/// The WGSL of an overlay's light, for a renderer to concatenate into its
/// own shader ahead of a stage that calls `overlay_lit`.
///
/// It declares the flag constants (`OVERLAY_LIT`, `OVERLAY_RECEIVES`,
/// `OVERLAY_BEHIND_GROUND`) and one function:
///
/// ```wgsl
/// fn overlay_lit(color: vec4f, normal: vec3f, at: vec3f, right: vec3f, up: vec3f) -> vec4f
/// ```
///
/// Vertex and fragment arithmetic only: no binding, no texture, no storage
/// buffer and no compute, so it translates to GLSL ES 3.00 and runs on
/// WebGL2.
pub const LIGHT_WGSL: &str = include_str!("light.wgsl");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OverlayDepth, OverlayMesh, OverlayShadow};

    /// The attribute offsets a renderer's vertex layout declares. A field
    /// reordered here and not there costs colours, not a compile error.
    #[test]
    fn a_vertex_is_packed_where_every_renderer_says_it_is() {
        assert_eq!(std::mem::size_of::<OverlayVertex>(), 48);
        assert_eq!(std::mem::offset_of!(OverlayVertex, position), 0);
        assert_eq!(std::mem::offset_of!(OverlayVertex, color), 12);
        assert_eq!(std::mem::offset_of!(OverlayVertex, normal), 28);
        assert_eq!(std::mem::offset_of!(OverlayVertex, flags), 40);
        assert_eq!(std::mem::offset_of!(OverlayVertex, sunk), 44);
    }

    #[test]
    fn a_flat_mesh_on_the_ground_is_unlit_and_tested_against_it() {
        let mesh = OverlayMesh {
            depth: OverlayDepth::Terrain,
            ..Default::default()
        };
        assert_eq!(flags(&mesh), BEHIND_GROUND);
    }

    #[test]
    fn normals_are_what_make_a_mesh_lit() {
        let mesh = OverlayMesh {
            positions: vec![[0.0; 3]],
            colors: vec![[1.0; 4]],
            normals: vec![[0.0, 0.0, 1.0]],
            depth: OverlayDepth::Always,
            ..Default::default()
        };
        assert_eq!(flags(&mesh), LIT, "lit, and not tested against the ground");
    }

    #[test]
    fn a_receiver_says_so_and_a_caster_does_not() {
        let receives = OverlayMesh {
            depth: OverlayDepth::Always,
            shadow: OverlayShadow::Receive,
            ..Default::default()
        };
        assert_eq!(flags(&receives), RECEIVES);
        // Casting is the sun's map, drawn in another pass: it is nothing to
        // a vertex drawn into the picture.
        let casts = OverlayMesh {
            depth: OverlayDepth::Always,
            shadow: OverlayShadow::Cast,
            ..Default::default()
        };
        assert_eq!(flags(&casts), 0);
    }

    /// The contract the renderers rely on: the names they call and the
    /// constants they test. A rename here is a shader that will not compile
    /// in two crates, which is better found in this crate's own tests.
    #[test]
    fn the_shared_wgsl_declares_what_a_renderer_calls() {
        assert!(LIGHT_WGSL.contains("fn overlay_lit("));
        assert!(LIGHT_WGSL.contains("const OVERLAY_LIT: u32 = 1u;"));
        assert!(LIGHT_WGSL.contains("const OVERLAY_RECEIVES: u32 = 2u;"));
        assert!(LIGHT_WGSL.contains("const OVERLAY_BEHIND_GROUND: u32 = 4u;"));
        // Vertex and fragment arithmetic only: anything of a binding or a
        // compute stage here would not reach WebGL2.
        for forbidden in ["@group", "@binding", "@compute", "var<storage"] {
            assert!(
                !LIGHT_WGSL.contains(forbidden),
                "{forbidden} is not WebGL2-safe here"
            );
        }
    }
}
