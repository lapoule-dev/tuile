// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A stated field of mist: layers of a procedural, horizontally varying
//! medium a camera flies through, marched per pixel against the scene's
//! depth and composed over the picture.
//!
//! The **field** is the host's statement ([`MistField`]): a local frame
//! about an anchor, and a few [`MistLayer`]s, each a band of heights with a
//! coverage, an extinction, a scale, a warp and a wind. No data comes from
//! anywhere: the density is a function of position and time, the same on
//! every renderer and every platform (see
//! `docs/explorations/volumetric-clouds.md` §5, the stated source).
//!
//! The **medium** is in [`WGSL`]: a gradient noise, its octaves, a domain
//! warp, the layer profile, and the march with Beer-Lambert absorption,
//! lit simply — brighter towards a layer's top and the sun's side — because
//! the look wanted is a light veil, not a lit cumulus. Both renderers
//! concatenate it after [`wgsl_bindings`] and march with
//! `mist_march(origin, direction, farthest, jitter)`, in render space.
//!
//! **No field is today's picture**: a renderer with no field draws nothing
//! of this, and a field with no layers draws nothing either.

use glam::{DVec3, Mat4, Vec3};

/// The most layers a field may state: what the uniform holds.
pub const MOST_LAYERS: usize = 4;

/// One layer of the medium.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MistLayer {
    /// Its band, metres above the field's anchor: where it begins…
    pub base_m: f64,
    /// …and where it ends.
    pub top_m: f64,
    /// How much of the band is mist, 0..1: 0.3 is scattered veils, 0.8
    /// an overcast with gaps.
    pub coverage: f64,
    /// Extinction at full density, per metre: 1/σ is about the distance
    /// seen through the thickest of it (0.001 is a kilometre).
    pub extinction: f64,
    /// The patches' size, metres.
    pub scale_m: f64,
    /// How far the noise's coordinates are warped, in scales: 0 is round
    /// patches, 0.5 pulled and torn ones.
    pub warp: f64,
    /// The wind, metres a second east and north: what the patches drift
    /// with, by the field's `time`.
    pub wind_m_s: [f64; 2],
    /// The medium's colour, display-linear, as lit from above.
    pub colour: [f32; 3],
}

impl Default for MistLayer {
    fn default() -> Self {
        Self {
            base_m: 1_000.0,
            top_m: 1_600.0,
            coverage: 0.35,
            extinction: 0.0025,
            scale_m: 3_000.0,
            warp: 0.3,
            wind_m_s: [0.0, 0.0],
            colour: [0.93, 0.95, 0.98],
        }
    }
}

/// A stated field: its anchor and its layers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MistField {
    /// The field's origin, ECEF metres: the layers' heights are above it,
    /// along its vertical, and the noise is laid in the east-north-up
    /// frame there — so the patches stay put in the world as the camera
    /// moves, and never see a large coordinate.
    pub anchor: DVec3,
    /// At most [`MOST_LAYERS`]; more are ignored.
    pub layers: Vec<MistLayer>,
    /// Seconds, for the wind.
    pub time: f64,
}

impl MistField {
    /// Whether there is anything to draw.
    pub fn is_empty(&self) -> bool {
        self.layers
            .iter()
            .all(|l| l.coverage <= 0.0 || l.extinction <= 0.0 || l.top_m <= l.base_m)
    }

    /// The field's frame: unit east, north and up at the anchor, ECEF.
    /// The sphere's vertical, which stands a fraction of a degree from the
    /// ellipsoid's — nothing a veil shows.
    pub fn frame(&self) -> [DVec3; 3] {
        let up = self.anchor.try_normalize().unwrap_or(DVec3::Z);
        let east = DVec3::Z.cross(up).try_normalize().unwrap_or(DVec3::X);
        let north = up.cross(east);
        [east, north, up]
    }
}

/// The view a field is marched from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MistView {
    /// The render origin, ECEF: what the renderer's f32 positions are from.
    pub origin: DVec3,
    /// The eye, ECEF.
    pub eye: DVec3,
    /// From a pixel back to the line of sight: the view-projection the
    /// picture is drawn through, relative to `origin`, inverted.
    pub inverse_view_proj: Mat4,
    /// Towards the sun, ECEF, unit.
    pub to_sun: DVec3,
    /// How far a line of sight that meets nothing is marched, metres.
    pub farthest: f64,
    /// Samples along a layer's crossing.
    pub steps: u32,
}

/// One layer as the uniform holds it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LayerUniform {
    /// base, top, coverage, extinction.
    pub band: [f32; 4],
    /// scale, warp, the wind's drift east and north at this time (metres).
    pub shape: [f32; 4],
    /// rgb, unused.
    pub colour: [f32; 4],
}

/// The field and the view as a shader reads them: `Mist` in [`WGSL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MistUniform {
    pub inverse_view_proj: [f32; 16],
    /// The eye from the render origin; w: unused.
    pub eye: [f32; 4],
    /// The field's frame, ECEF directions; w: unused.
    pub east: [f32; 4],
    pub north: [f32; 4],
    pub up: [f32; 4],
    /// The anchor from the render origin; w: how many layers.
    pub anchor: [f32; 4],
    /// Towards the sun; w: the steps.
    pub to_sun: [f32; 4],
    /// x: the farthest march; yzw: unused.
    pub reach: [f32; 4],
    pub layers: [LayerUniform; MOST_LAYERS],
}

impl Default for MistUniform {
    fn default() -> Self {
        Self {
            inverse_view_proj: Mat4::IDENTITY.to_cols_array(),
            eye: [0.0; 4],
            east: [1.0, 0.0, 0.0, 0.0],
            north: [0.0, 1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0, 0.0],
            anchor: [0.0; 4],
            to_sun: [0.0, 0.0, 1.0, 32.0],
            reach: [0.0; 4],
            layers: [LayerUniform::default(); MOST_LAYERS],
        }
    }
}

impl MistUniform {
    /// Packs `field` seen from `view`. Offsets are taken in f64 and only
    /// then narrowed: the anchor and the eye are kilometres from the render
    /// origin at most, which f32 carries to the centimetre.
    pub fn of(field: &MistField, view: &MistView) -> Self {
        let [east, north, up] = field.frame();
        let v4 = |v: DVec3, w: f32| {
            let v = v.as_vec3();
            [v.x, v.y, v.z, w]
        };
        let count = field.layers.len().min(MOST_LAYERS);
        let mut layers = [LayerUniform::default(); MOST_LAYERS];
        for (slot, layer) in layers.iter_mut().zip(&field.layers) {
            *slot = LayerUniform {
                band: [
                    layer.base_m as f32,
                    layer.top_m as f32,
                    layer.coverage as f32,
                    layer.extinction as f32,
                ],
                shape: [
                    layer.scale_m.max(1.0) as f32,
                    layer.warp as f32,
                    (layer.wind_m_s[0] * field.time) as f32,
                    (layer.wind_m_s[1] * field.time) as f32,
                ],
                colour: [layer.colour[0], layer.colour[1], layer.colour[2], 0.0],
            };
        }
        Self {
            inverse_view_proj: view.inverse_view_proj.to_cols_array(),
            eye: v4(view.eye - view.origin, 0.0),
            east: v4(east, 0.0),
            north: v4(north, 0.0),
            up: v4(up, 0.0),
            anchor: v4(field.anchor - view.origin, count as f32),
            to_sun: v4(view.to_sun.normalize_or_zero(), view.steps.max(4) as f32),
            reach: [view.farthest as f32, 0.0, 0.0, 0.0],
            layers,
        }
    }

    /// The eye, as packed.
    pub fn eye(&self) -> Vec3 {
        Vec3::new(self.eye[0], self.eye[1], self.eye[2])
    }
}

/// The uniform's declaration at `group` and `binding`: prepend it, then
/// [`WGSL`], to a pass that marches the field.
pub fn wgsl_bindings(group: u32, binding: u32) -> String {
    format!(
        "struct MistLayer {{ band: vec4f, shape: vec4f, colour: vec4f }}\n\
         struct Mist {{\n    inverse_view_proj: mat4x4f,\n    eye: vec4f,\n    east: vec4f,\n    north: vec4f,\n    up: vec4f,\n    anchor: vec4f,\n    to_sun: vec4f,\n    reach: vec4f,\n    layers: array<MistLayer, {MOST_LAYERS}>,\n}}\n\
         @group({group}) @binding({binding}) var<uniform> mist: Mist;\n"
    )
}

/// The medium: the noise, the density of a layer at a point, and the march.
/// After [`wgsl_bindings`].
pub const WGSL: &str = include_str!("mist.wgsl");

#[cfg(test)]
mod tests {
    use super::*;

    fn field() -> MistField {
        MistField {
            anchor: DVec3::new(4_500_000.0, 100_000.0, 4_450_000.0),
            layers: vec![
                MistLayer::default(),
                MistLayer {
                    base_m: 4_000.0,
                    top_m: 4_600.0,
                    ..MistLayer::default()
                },
            ],
            time: 10.0,
        }
    }

    #[test]
    fn the_frame_is_orthonormal_with_up_along_the_anchor() {
        let [east, north, up] = field().frame();
        assert!((up - field().anchor.normalize()).length() < 1e-12);
        assert!(
            east.dot(up).abs() < 1e-12
                && north.dot(up).abs() < 1e-12
                && east.dot(north).abs() < 1e-12
        );
        assert!((east.length() - 1.0).abs() < 1e-12 && (north.length() - 1.0).abs() < 1e-12);
        assert!(east.z.abs() < 1e-12, "east is level");
        assert!(north.z > 0.0, "north climbs towards the pole");
    }

    #[test]
    fn the_uniform_holds_the_field_from_the_render_origin() {
        let f = field();
        let view = MistView {
            origin: f.anchor + DVec3::new(0.0, 0.0, 5_000.0),
            eye: f.anchor + DVec3::new(0.0, 0.0, 5_000.0),
            inverse_view_proj: Mat4::IDENTITY,
            to_sun: DVec3::new(0.0, 0.0, 2.0),
            farthest: 200_000.0,
            steps: 48,
        };
        let u = MistUniform::of(&f, &view);
        assert_eq!(u.anchor, [0.0, 0.0, -5_000.0, 2.0]);
        assert_eq!(u.eye, [0.0; 4]);
        assert_eq!(u.to_sun, [0.0, 0.0, 1.0, 48.0]);
        assert_eq!(u.layers[0].band, [1_000.0, 1_600.0, 0.35, 0.0025]);
        assert_eq!(u.layers[1].band[1], 4_600.0);
        assert_eq!(u.layers[2].band, [0.0; 4], "an unstated slot is nothing");
        assert_eq!(
            std::mem::size_of::<MistUniform>(),
            64 + 7 * 16 + MOST_LAYERS * 48
        );
    }

    #[test]
    fn a_field_with_no_mist_in_it_is_empty() {
        assert!(MistField::default().is_empty());
        assert!(MistField {
            layers: vec![MistLayer {
                coverage: 0.0,
                ..Default::default()
            }],
            ..Default::default()
        }
        .is_empty());
        assert!(!field().is_empty());
    }

    #[test]
    fn the_wind_drifts_the_patches_by_the_time() {
        let mut f = field();
        f.layers[0].wind_m_s = [3.0, -1.0];
        let view = MistView {
            origin: f.anchor,
            eye: f.anchor,
            inverse_view_proj: Mat4::IDENTITY,
            to_sun: DVec3::Z,
            farthest: 1.0,
            steps: 8,
        };
        let u = MistUniform::of(&f, &view);
        assert_eq!(u.layers[0].shape[2..], [30.0, -10.0]);
    }

    #[test]
    fn the_bindings_declare_the_uniform_at_the_group_asked() {
        let text = wgsl_bindings(2, 1);
        assert!(text.contains("@group(2) @binding(1) var<uniform> mist: Mist;"));
        assert!(text.contains("array<MistLayer, 4>"));
        assert!(WGSL.contains("fn mist_march("));
    }
}
