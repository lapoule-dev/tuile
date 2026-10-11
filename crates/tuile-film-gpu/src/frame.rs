// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use glam::{DMat4, DVec3, Mat4, Vec3};
use tuile_film::{FrameCamera, Look};

/// `Frame` in `common.wgsl`, byte for byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FrameUniform {
    pub view_proj: [[f32; 4]; 4],
    pub right: [f32; 4],
    pub up: [f32; 4],
    pub fwd: [f32; 4],
    pub world: [f32; 4],
    pub sun: [f32; 4],
    pub to_sun: [f32; 4],
    pub size: [u32; 4],
    /// Eye-relative world to the sun's map: see [`SunView`].
    pub sun_view_proj: [[f32; 4]; 4],
    /// x: how much of the sun a shadow takes, 0 for no shadows at all;
    /// y: a texel of the map on the ground, in metres; z: its side, in
    /// texels; w: how far from the eye shadows reach, in metres.
    pub shadow: [f32; 4],
    /// x: extinction at the eye, per metre; y: one over the scale height;
    /// z: one over twice the eye's distance from the Earth's centre;
    /// w: whether there is air at all.
    pub haze: [f32; 4],
    /// xyz: up at the eye.
    pub local_up: [f32; 4],
    pub horizon: [f32; 4],
    pub zenith: [f32; 4],
}

/// The sun's view of one frame: an orthographic projection down the sun's
/// rays over the ground the frame shows, as far as its shadows reach.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunView {
    /// Eye-relative world to the map's clip space, depth 0 nearest the sun.
    pub view_proj: Mat4,
    /// A texel of the map on a plane facing the sun, in metres.
    pub texel: f32,
    /// How far from the eye shadows are drawn, in metres: past it the
    /// ground is lit, and they fade out towards it.
    pub reach: f32,
}

/// How far shadows reach for an eye `height` metres over the ellipsoid:
/// this many times its height, and four kilometres at the least. A film's
/// frame draws tiles all the way round the Earth, coarser with distance;
/// a map that held them all would spend a texel on kilometres of ground.
/// Shadows are drawn where they can be seen for what they are.
pub fn shadow_reach(height: f64) -> f64 {
    (16.0 * height).clamp(4_000.0, 400_000.0)
}

impl SunView {
    /// Fitted, on a map `side` texels across, to the ground that can
    /// show a shadow: what lies both among `spheres` — each drawn tile's
    /// bounding sphere, its centre from the eye — and in the part of the
    /// camera's view that `rays`, its corners' and edges' unit rays, bound
    /// out to `reach`. `None` when there is nothing to fit to, or no sun
    /// to look from.
    ///
    /// Only the ground that receives is held between the map's near and
    /// far planes. Ground nearer the sun than that casts all the same: the
    /// shadow raster flattens it onto the near plane.
    ///
    /// The map's window is cut on a grid fixed to the Earth, a texel a
    /// step, and its width is one of a ladder of widths: from one frame to
    /// the next, for as long as the tiles drawn ask for the same width, the
    /// ground falls on the same texels and a shadow's edge does not crawl
    /// as the camera moves.
    pub fn fitted(
        to_sun: Vec3,
        eye: DVec3,
        spheres: impl Iterator<Item = (DVec3, f64)>,
        rays: &[Vec3],
        reach: f64,
        side: u32,
    ) -> Option<Self> {
        let to_sun = to_sun.as_dvec3().try_normalize()?;
        // A rotation only: the eye stays the origin, as for the camera.
        let view = DMat4::look_to_rh(DVec3::ZERO, -to_sun, to_sun.any_orthonormal_vector());
        let (mut low, mut high) = (DVec3::INFINITY, DVec3::NEG_INFINITY);
        for (centre, radius) in spheres {
            let centre = view.transform_vector3(centre);
            low = low.min(centre - radius);
            high = high.max(centre + radius);
        }
        // What the camera sees within reach: the eye, and the end of each
        // ray. Ground that shows a shadow is in both boxes.
        let (mut near, mut far) = (DVec3::ZERO, DVec3::ZERO);
        for ray in rays {
            let end = view.transform_vector3(ray.as_dvec3() * reach);
            (near, far) = (near.min(end), far.max(end));
        }
        let (low, high) = (low.max(near), high.min(far));
        if !(low.is_finite() && high.is_finite()) || low.cmpgt(high).any() {
            return None;
        }
        // The next width up the ladder, four rungs to a doubling.
        let wanted = (high - low).truncate().max_element().max(1.0) * 1.01;
        let width = ((wanted.log2() * 4.0).ceil() / 4.0).exp2();
        let texel = width / f64::from(side);
        // Where the eye is along the sun's axes: what turns a place from
        // the eye into a place on the Earth, which is where the grid is.
        let eye = view.transform_vector3(eye);
        let snapped = |from_eye: f64, eye: f64| ((from_eye + eye) / texel).floor() * texel - eye;
        let (x, y) = (snapped(low.x, eye.x), snapped(low.y, eye.y));
        // The view looks down −z: what is nearest the sun has the greatest z.
        let projection = DMat4::orthographic_rh(x, x + width, y, y + width, -high.z, -low.z);
        Some(Self {
            view_proj: (projection * view).as_mat4(),
            texel: texel as f32,
            reach: reach as f32,
        })
    }
}

impl FrameUniform {
    pub fn new(camera: &FrameCamera, look: &Look, size: [u32; 4]) -> Self {
        // The view matrix is a pure rotation whose rows are the camera's
        // right, up and backward axes; the projection's diagonal holds
        // 1/tan(fovy/2), over the aspect for x. A pixel's ray is then
        // `fwd + x·right + y·up` for its NDC x, y.
        let v = camera.view;
        let p = camera.projection;
        let right = v.row(0).truncate() / p.x_axis.x;
        let up = v.row(1).truncate() / p.y_axis.y;
        let fwd = -v.row(2).truncate();
        let air = look.haze.map(|haze| (haze, haze.at(camera.eye)));
        Self {
            view_proj: camera.view_projection().to_cols_array_2d(),
            right: right.extend(0.0).to_array(),
            up: up.extend(0.0).to_array(),
            fwd: fwd.extend(0.0).to_array(),
            world: look.world.extend(look.exposure_scale()).to_array(),
            sun: look.sun.extend(look.contrast).to_array(),
            to_sun: look.to_sun.extend(look.saturation).to_array(),
            size,
            // No sun's view until one is fitted: no shadow.
            sun_view_proj: Mat4::IDENTITY.to_cols_array_2d(),
            shadow: [0.0; 4],
            haze: air.map_or([0.0; 4], |(_, at)| {
                [at.density, at.per_height, at.curve, 1.0]
            }),
            // `w` is the probe of holes, set by the renderer when asked.
            local_up: air.map_or([0.0; 4], |(_, at)| at.up.extend(0.0).to_array()),
            horizon: air.map_or([0.0; 4], |(haze, _)| haze.horizon.extend(0.0).to_array()),
            zenith: air.map_or([0.0; 4], |(haze, _)| haze.zenith.extend(0.0).to_array()),
        }
    }

    /// With the frame's shadows: `strength` of the sun taken away where
    /// `sun`'s map, `side` texels across, finds ground before it.
    pub fn shadowed(mut self, sun: &SunView, strength: f32, side: u32) -> Self {
        self.sun_view_proj = sun.view_proj.to_cols_array_2d();
        self.shadow = [strength.clamp(0.0, 1.0), sun.texel, side as f32, sun.reach];
        self
    }

    /// Unit rays that bound what the camera sees: its four corners, the
    /// middles of its four edges, and its centre.
    pub fn bounding_rays(&self) -> [Vec3; 9] {
        let axis = |v: [f32; 4]| Vec3::new(v[0], v[1], v[2]);
        let (right, up, fwd) = (axis(self.right), axis(self.up), axis(self.fwd));
        let mut rays = [Vec3::ZERO; 9];
        for (ray, at) in rays.iter_mut().zip(0..9) {
            let (x, y) = ((at % 3) as f32 - 1.0, (at / 3) as f32 - 1.0);
            *ray = (fwd + x * right + y * up).normalize();
        }
        rays
    }
}

/// `TileFrame` in `common.wgsl`, byte for byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TileFrame {
    pub offset: [f32; 3],
    pub flags: u32,
    pub factor: [f32; 4],
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec4Swizzles;
    use tuile_film::BakedView;

    /// A tile's sphere `east` metres from a point over null island.
    fn sphere(east: f64) -> (DVec3, f64) {
        (DVec3::new(-1000.0, east, 0.0), 300.0)
    }

    const EYE: DVec3 = DVec3::new(6_379_137.0, 10.0, 20.0);

    /// Rays to every side: a camera that sees everything, so that what is
    /// drawn is all that bounds the map.
    const ALL_ROUND: [Vec3; 6] = [
        Vec3::X,
        Vec3::NEG_X,
        Vec3::Y,
        Vec3::NEG_Y,
        Vec3::Z,
        Vec3::NEG_Z,
    ];
    const FAR: f64 = 100_000.0;

    #[test]
    fn the_suns_view_holds_every_sphere_it_was_fitted_to() {
        let to_sun = Vec3::new(1.0, 0.4, 0.3);
        let spheres = [sphere(-2000.0), sphere(0.0), sphere(5000.0)];
        let sun = SunView::fitted(to_sun, EYE, spheres.into_iter(), &ALL_ROUND, FAR, 1024)
            .expect("a view");
        let along = to_sun.normalize().as_dvec3();
        for (centre, radius) in spheres {
            // The centre, and the ends of the sphere along and across.
            for reach in [DVec3::ZERO, along, -along, along.any_orthonormal_vector()] {
                let clip = sun
                    .view_proj
                    .project_point3((centre + reach * radius).as_vec3());
                assert!(clip.x.abs() <= 1.0 && clip.y.abs() <= 1.0, "{clip}");
                assert!((-1e-4..=1.0 + 1e-4).contains(&clip.z), "{clip}");
            }
        }
        // Nearer the sun is nearer zero.
        let (near, far) = (
            sun.view_proj.project_point3((along * 100.0).as_vec3()),
            sun.view_proj.project_point3((along * -100.0).as_vec3()),
        );
        assert!(near.z < far.z);
        // And no wider than a rung of the ladder past what they span.
        assert!(sun.texel * 1024.0 < 7600.0 * 1.25, "{}", sun.texel);
    }

    /// As the eye moves, a place on the ground stays on the map's grid: its
    /// texel coordinate moves by whole texels, never by a part of one.
    #[test]
    fn the_suns_map_is_cut_on_a_grid_fixed_to_the_ground() {
        let to_sun = Vec3::new(1.0, 0.4, 0.3);
        let side = 1024;
        // A place on the ground, and what is drawn around the eye: it
        // follows the eye, so the window slides with every frame.
        let place = DVec3::new(6_378_137.0, 1234.5, -321.25);
        let around = [
            DVec3::new(-1000.0, -2000.0, 0.0),
            DVec3::new(-1000.0, 5000.0, 900.0),
        ];
        let texel_of = |eye: DVec3| {
            let drawn = around.iter().map(|c| (*c, 300.0));
            let sun = SunView::fitted(to_sun, eye, drawn, &ALL_ROUND, FAR, side).expect("a view");
            let clip = sun.view_proj.project_point3((place - eye).as_vec3());
            (clip.truncate() * 0.5 + 0.5) * side as f32
        };
        let first = texel_of(EYE);
        let mut moved = false;
        for step in 1..40 {
            // 3.7 m a frame, along nothing in particular.
            let eye = EYE + DVec3::new(0.9, 3.1, -1.8) * f64::from(step);
            let delta = texel_of(eye) - first;
            let off = (delta - delta.round()).abs().max_element();
            assert!(off < 0.02, "step {step}: {off} of a texel off the grid");
            moved |= delta.round().abs().max_element() >= 1.0;
        }
        assert!(moved, "the window never moved: nothing was tested");
    }

    #[test]
    fn nothing_drawn_and_no_sun_have_no_view() {
        let none = std::iter::empty();
        assert!(SunView::fitted(Vec3::X, EYE, none, &ALL_ROUND, FAR, 1024).is_none());
        let one = [sphere(0.0)].into_iter();
        assert!(SunView::fitted(Vec3::ZERO, EYE, one, &ALL_ROUND, FAR, 1024).is_none());
        // What is drawn is all behind the camera: nothing shows a shadow.
        let behind = [(DVec3::new(5000.0, 0.0, 0.0), 300.0)].into_iter();
        assert!(SunView::fitted(Vec3::Z, EYE, behind, &[Vec3::NEG_X], 3000.0, 1024).is_none());
    }

    /// A frame draws tiles all the way round the Earth. The map is fitted
    /// to what the camera sees within reach, not to them: its texel is the
    /// reach's, whatever else is drawn — and the near ground is still in it.
    #[test]
    fn the_far_side_of_the_earth_does_not_coarsen_the_map() {
        let to_sun = Vec3::new(0.6, 0.2, 0.7);
        let u = FrameUniform::new(
            &FrameCamera::of(
                &BakedView {
                    position: EYE.to_array(),
                    direction: [-1.0, 0.3, 0.0],
                    up: [0.0, 0.0, 1.0],
                    viewport_px: [1920.0, 1080.0],
                    fovy_rad: 0.8,
                },
                16.0 / 9.0,
            ),
            &Look::default(),
            [1920, 1080, 1, 0],
        );
        let rays = u.bounding_rays();
        let reach = shadow_reach(1000.0);
        assert_eq!(reach, 16_000.0);
        // The ground under the eye, and half the planet.
        let near = (DVec3::new(-1000.0, 300.0, 0.0), 2000.0);
        let planet = (DVec3::new(-6_379_137.0, 0.0, 0.0), 6_400_000.0);
        let sun = SunView::fitted(to_sun, EYE, [near, planet].into_iter(), &rays, reach, 4096)
            .expect("a view");
        assert!(
            sun.texel < 2.5 * 16_000.0 / 4096.0,
            "{} m a texel",
            sun.texel
        );
        // A point on the ground straight ahead is on the map, between its
        // planes.
        let ahead = (rays[4] * 1200.0).as_dvec3();
        let clip = sun.view_proj.project_point3(ahead.as_vec3());
        assert!(clip.abs().max_element() <= 1.0 && clip.z >= 0.0, "{clip}");
        assert_eq!(shadow_reach(10.0), 4_000.0);
    }

    /// The ray the resolve builds for a pixel must pass through the point the
    /// raster projected to that pixel — or barycentrics come out of the wrong
    /// triangle's plane.
    #[test]
    fn a_pixel_ray_goes_through_what_projects_there() {
        let view = BakedView {
            position: [7_000_000.0, 10.0, 20.0],
            direction: [-1.0, 0.2, 0.1],
            up: [0.0, 0.0, 1.0],
            viewport_px: [1920.0, 1080.0],
            fovy_rad: 0.9,
        };
        let cam = FrameCamera::of(&view, 16.0 / 9.0);
        let u = FrameUniform::new(&cam, &Look::default(), [1920, 1080, 1, 0]);
        let point = Vec3::new(-3000.0, 900.0, -400.0);
        let clip = cam.view_projection() * point.extend(1.0);
        let ndc = clip.xy() / clip.w;
        let ray = Vec3::from_slice(&u.fwd)
            + ndc.x * Vec3::from_slice(&u.right)
            + ndc.y * Vec3::from_slice(&u.up);
        let angle = ray.normalize().angle_between(point.normalize());
        assert!(angle < 1e-5, "{angle}");
    }
}
