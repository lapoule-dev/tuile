// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a host adds to a film's frames: the camera its overlays are made
//! for, and the trait a film asks for them through.
//!
//! The shapes themselves are `tuile-overlay`'s: an interactive viewer draws
//! the same triangles and has no film to take. What is here is what only a
//! film has — the view it was baked for, and the frame's own camera.

use tuile_pack::BakedView;

use crate::{EyePair, FrameCamera, FrameLight};

pub use tuile_overlay::{
    Eye, OverlayBlend, OverlayDepth, OverlayEyes, OverlayMesh, OverlayShadow, MIN_SHADOW_TEXELS,
};

/// The camera a frame's overlays are made for: the whole of it, once a
/// frame.
///
/// A stereo film draws the frame twice, from two eyes. Overlays are made
/// once, for the frame's own camera between them — the cyclopean eye — and
/// drawn into each eye through that eye's projection: a shape is then one
/// shape, at one depth, seen from two places, which is what relief is.
/// What depends on the camera (a width kept constant on the screen, a face
/// turned to the eye) is computed from `camera`, never from an eye: two
/// eyes computing it each would make two shapes, which the eyes cannot
/// fuse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayCamera {
    /// The view the frame was baked for.
    pub view: BakedView,
    /// The frame's camera at the picture's aspect: its eye, its rotation,
    /// its projection and its near plane ([`FrameCamera::near`]).
    pub camera: FrameCamera,
    /// The film's two eyes for this frame, when it has two.
    pub eyes: Option<EyePair>,
    /// A texel of the sun's map this frame, in metres across, when the
    /// film has shadows and there is ground for them: see
    /// [`OverlayShadow`].
    pub shadow_texel: Option<f32>,
}

impl OverlayCamera {
    /// The camera of a film of one eye, with no shadows.
    pub fn of(view: BakedView, aspect: f32) -> Self {
        Self {
            view,
            camera: FrameCamera::of(&view, aspect),
            eyes: None,
            shadow_texel: None,
        }
    }

    /// The near plane, in metres.
    pub fn near(&self) -> f32 {
        self.camera.near
    }

    /// The camera `eye` draws through: the frame's own for a film of one
    /// eye.
    pub fn eye(&self, eye: Eye) -> FrameCamera {
        match &self.eyes {
            Some(pair) => self.camera.eye(pair, eye),
            None => self.camera,
        }
    }
}

/// The overlays of a film, asked for frame by frame — and, when the host
/// places it, the frame's light.
pub trait Overlays {
    /// Adds to `out` what `frame` shows beside the ground, once a frame —
    /// for both eyes of a stereo film. `camera` is the frame's; `out`
    /// comes in empty.
    fn frame(&mut self, frame: u32, camera: &OverlayCamera, out: &mut Vec<OverlayMesh>);

    /// The light `frame` is lit by, asked once a frame before anything of
    /// it is drawn — before its overlays, and before the sun's map is
    /// fitted, so that the shadows fall from this sun — and shared by both
    /// eyes of a stereo film. `None`, the default: the film's look, as
    /// ordered.
    fn light(&mut self, frame: u32) -> Option<FrameLight> {
        let _ = frame;
        None
    }

    /// The mist `frame` is seen through ([`tuile_atmosphere::mist`]): a
    /// stated field, marched over the picture after the ground and under
    /// the overlays that are never hidden. `None`, the default: no mist.
    fn mist(&mut self, frame: u32) -> Option<tuile_atmosphere::mist::MistField> {
        let _ = frame;
        None
    }
}

/// No overlays.
impl Overlays for () {
    fn frame(&mut self, _: u32, _: &OverlayCamera, _: &mut Vec<OverlayMesh>) {}
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
    fn a_host_that_says_nothing_of_the_light_leaves_it_to_the_look() {
        assert_eq!(().light(7), None);
    }

    #[test]
    fn a_whole_mesh_has_no_fault_and_is_drawn_as_before_it_had_settings() {
        let mesh = triangle();
        assert_eq!(mesh.fault(), None);
        assert_eq!(mesh.depth, OverlayDepth::Terrain);
        assert_eq!(mesh.blend, OverlayBlend::Translucent);
        assert_eq!(mesh.eyes, OverlayEyes::Both);
        assert_eq!(mesh.shadow, OverlayShadow::None);
        assert!(mesh.normals.is_empty());
    }

    #[test]
    fn a_mesh_for_one_eye_is_in_that_eye_and_in_a_film_of_one() {
        assert!(OverlayEyes::Left.shows(Some(Eye::Left)));
        assert!(!OverlayEyes::Left.shows(Some(Eye::Right)));
        assert!(!OverlayEyes::Right.shows(Some(Eye::Left)));
        assert!(OverlayEyes::Right.shows(Some(Eye::Right)));
        for eyes in [OverlayEyes::Both, OverlayEyes::Left, OverlayEyes::Right] {
            assert!(eyes.shows(None));
        }
        assert!(OverlayEyes::Both.shows(Some(Eye::Right)));
    }

    #[test]
    fn what_casts_and_what_receives() {
        let cast: Vec<bool> = [
            OverlayShadow::None,
            OverlayShadow::Cast,
            OverlayShadow::Receive,
            OverlayShadow::CastAndReceive,
        ]
        .iter()
        .flat_map(|s| [s.casts(), s.receives()])
        .collect();
        assert_eq!(cast, [false, false, true, false, false, true, true, true]);
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
        let mut normals = triangle();
        normals.normals = vec![[1.0, 0.0, 0.0]; 2];
        assert!(normals.fault().is_some());
        normals.normals.push([0.0, 1.0, 0.0]);
        assert_eq!(normals.fault(), None);
    }
}
