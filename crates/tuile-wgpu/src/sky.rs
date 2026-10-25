// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere in the interactive renderer: the sky past the
//! ground and the air between the eye and it, lit by the sun and the Moon
//! of a stated instant.
//!
//! The model and the code that draws it are `tuile_atmosphere`'s — its
//! tables, its sky-view pass, its uniform, and `sky_seen`/`ground_seen` in
//! WGSL — the very ones a film renders with. What is this renderer's own is
//! when the light is worked out again:
//!
//! - **the sun and the Moon** are one direction each at the instant (ECEF),
//!   from their ephemerides. Every ground fragment and every line of sight
//!   is lit by them where it is, so one frame holds a day side and a night
//!   side, a terminator, a midnight sun near the pole;
//! - **the frame's light** — the exposure, the white balance, the sky's
//!   harmonic, all metered over what the camera sees
//!   ([`tuile_atmosphere::LutAtmosphere::light`], some milliseconds of CPU)
//!   — is worked out again when the sun or the Moon has moved by
//!   [`LIGHT_SUN_STEP`], the eye by a share of its height, or the view has
//!   turned, and never more often than [`LIGHT_INTERVAL`]: a sun hurried a
//!   thousandfold by a host's clock still costs four of them a second;
//! - **the sky-view tables** (Hillaire's per-eye table) are made again on
//!   the GPU with each light, and in between when the eye has climbed,
//!   descended or travelled enough that the horizon it holds would be off;
//! - the tables of the air itself are a clear day's, read rather than
//!   computed ([`tuile_atmosphere::LutAtmosphere::of`]).
//!
//! Between those, a frame writes one uniform: the air's sphere placed from
//! its eye, and the stars turned to its instant.

use glam::DMat3;
use glam::{DMat4, DVec3, Mat4};
use tuile_atmosphere::gpu::{AtmosphereGpu, AtmosphereUniform, SkyViewPass};
use tuile_atmosphere::hillaire::{metering, Moment};
use tuile_atmosphere::{
    AtmosphereFrame, AtmosphereParams, LutAtmosphere, Moon, Moonlight, SkyLook, Sun,
};
use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef, Geodetic};

/// How far, in radians, the sun or the Moon moves before a frame's light
/// is worked out again: a fifth of the sun's disc, twelve seconds of a real
/// sun's day.
pub const LIGHT_SUN_STEP: f64 = 0.05 * std::f64::consts::PI / 180.0;

/// The shortest time between two lights, wall seconds, however fast the sun
/// moves or the eye travels: a quarter of a second.
pub const LIGHT_INTERVAL: f64 = 0.25;

/// How far the view may turn, in radians, before the light is metered
/// again over what it now sees.
const LIGHT_VIEW_STEP: f64 = 4.0 * std::f64::consts::PI / 180.0;

/// The share of its height over the ground the eye may move before the
/// light — and the sky-view, its horizon — is worked out again.
const EYE_STEP: f64 = 0.02;

/// How far, as a log of the ratio, the eye's height over the ground under
/// it may change before the light is metered again: a tenth.
const GROUND_STEP: f64 = 0.1;

/// How far the sun moves in one frame, radians, for a jump: a degree,
/// four minutes of a real sun — what a host setting its clock does, and no
/// clock running does from one frame to the next.
const JUMP_SUN: f64 = std::f64::consts::PI / 180.0;

/// How many times [`EYE_STEP`] the eye moves in one frame for a jump.
const JUMP_EYE: f64 = 10.0;

/// Where the lights of one frame are: towards the sun and the Moon from
/// the planet, and the stars' turn — whichever [`Lights`] put them there.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Bodies {
    /// Unit vector towards the sun, in the planet's frame (ECEF).
    pub to_sun: DVec3,
    /// The Moon, if there is one: where, its phase, its size.
    pub moon: Option<Moonlight>,
    /// The turn from the star catalogue's axes (J2000) to the planet's.
    /// Stars are always drawn: the air and the camera decide whether they
    /// show.
    pub stars: DMat3,
    /// The planets, Mercury to Uranus, as the sky draws them
    /// ([`tuile_atmosphere::planets::lights_at`]): towards each (ECEF) and
    /// its irradiance over the sun's, and its colour.
    pub planets: [([f32; 4], [f32; 4]); 6],
}

impl Bodies {
    /// The real sun, Moon and stars at `instant` (Unix seconds UTC), the
    /// Moon as seen from `eye` (ECEF): `illumination.rs`'s ephemerides.
    pub fn ephemeris(instant: f64, eye: DVec3) -> Self {
        Self {
            to_sun: Sun::at_unix_seconds(instant).direction_ecef,
            moon: Moon::seen_from(instant, ecef_to_geodetic(eye)).map(|m| Moonlight::of(&m)),
            stars: tuile_atmosphere::stars::j2000_to_ecef(instant),
            planets: tuile_atmosphere::planets::lights_at(instant),
        }
    }

    /// A sun along `to_sun` and a Moon along `to_moon`, as a host sets
    /// them: the Moon at the mean distance, in the phase the two directions
    /// make; the stars as they stood at J2000 — a stated sky has no instant
    /// to turn them by, and its stars are there all the same.
    pub fn fixed(to_sun: DVec3, to_moon: Option<DVec3>) -> Self {
        let to_sun = to_sun.normalize_or(DVec3::Z);
        Self {
            to_sun,
            moon: to_moon.map(|m| {
                let m = m.normalize_or(DVec3::X);
                Moonlight {
                    to_moon_ecef: m,
                    // At the Moon, between the sun and the eye: the sun is
                    // as far as to be along `to_sun` from either.
                    phase_angle: (std::f64::consts::PI - m.angle_between(to_sun)) as f32,
                    angular_radius: (tuile_atmosphere::moon::MOON_RADIUS
                        / tuile_atmosphere::moon::MOON_MEAN_DISTANCE)
                        .asin() as f32,
                    moon_rgb: [0.0; 3],
                }
            }),
            stars: tuile_atmosphere::stars::j2000_to_ecef(tuile_atmosphere::stars::J2000_UNIX),
            planets: tuile_atmosphere::planets::lights_at(tuile_atmosphere::stars::J2000_UNIX),
        }
    }
}

/// Where a scene's sun comes from: a host's choice, a value and not a
/// switch. The light on every point, the terminator, the sky — everything
/// downstream is the same for all three.
///
/// Open to more sources (several suns, other bodies): a host matching on it
/// keeps a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Lights {
    /// The real sun, Moon and stars at the clock's instant, read as Unix
    /// seconds UTC: the Earth as it is ([`Bodies::ephemeris`]).
    Ephemeris {
        /// Whether the Moon is drawn and lights the night.
        moon: bool,
    },
    /// A world of its own: a sun going round in `day_length` seconds of the
    /// clock, its declination swinging by `axial_tilt` radians over
    /// `year_length` seconds (no seasons without one), `phase` the share of
    /// a day past local noon at the prime meridian at the clock's zero. No
    /// Moon; the stars turn with the day.
    Orbit {
        day_length: f64,
        year_length: Option<f64>,
        axial_tilt: f64,
        phase: f64,
    },
    /// The directions as the host sets them (ECEF), changed by the host
    /// when it likes: a frozen golden hour, a scripted scene.
    Fixed {
        to_sun: DVec3,
        to_moon: Option<DVec3>,
    },
}

impl Default for Lights {
    fn default() -> Self {
        Self::Ephemeris { moon: true }
    }
}

impl Lights {
    /// The real sun and Moon ([`Self::Ephemeris`]).
    pub fn ephemeris() -> Self {
        Self::Ephemeris { moon: true }
    }

    /// A world's own sun going round once in `day_length` seconds of the
    /// clock, no seasons ([`Self::Orbit`]); [`Self::with_seasons`] adds them.
    pub fn orbit(day_length: f64) -> Self {
        Self::Orbit {
            day_length,
            year_length: None,
            axial_tilt: 0.0,
            phase: 0.0,
        }
    }

    /// An orbit with seasons: a year of `year_length` seconds, an axis
    /// tilted by `axial_tilt` radians. Nothing on another source.
    pub fn with_seasons(self, year_length: f64, axial_tilt: f64) -> Self {
        match self {
            Self::Orbit {
                day_length, phase, ..
            } => Self::Orbit {
                day_length,
                year_length: Some(year_length),
                axial_tilt,
                phase,
            },
            other => other,
        }
    }

    /// The directions a host sets ([`Self::Fixed`]).
    pub fn fixed(to_sun: DVec3, to_moon: Option<DVec3>) -> Self {
        Self::Fixed { to_sun, to_moon }
    }

    /// Where the lights are at the clock's `instant`, seen from `eye`.
    pub fn at(&self, instant: f64, eye: DVec3) -> Bodies {
        match *self {
            Self::Ephemeris { moon } => {
                let b = Bodies::ephemeris(instant, eye);
                Bodies {
                    moon: b.moon.filter(|_| moon),
                    ..b
                }
            }
            Self::Orbit {
                day_length,
                year_length,
                axial_tilt,
                phase,
            } => {
                let tau = std::f64::consts::TAU;
                let day = instant / day_length.abs().max(1e-3) + phase;
                // The sun over the meridian `−day` turns west of the prime
                // one: the planet turns east, as the Earth does.
                let lon = -tau * day;
                let declination = match year_length {
                    Some(year) => axial_tilt * (tau * instant / year.abs().max(1e-3)).sin(),
                    None => 0.0,
                };
                let to_sun = DVec3::new(
                    declination.cos() * lon.cos(),
                    declination.cos() * lon.sin(),
                    declination.sin(),
                );
                Bodies {
                    to_sun,
                    moon: None,
                    stars: DMat3::from_rotation_z(-lon),
                    // The planets of J2000, turning with the stars: a
                    // world of its own has no date to move them by.
                    planets: tuile_atmosphere::planets::lights_turned(
                        tuile_atmosphere::stars::J2000_UNIX,
                        DMat3::from_rotation_z(-lon),
                    ),
                }
            }
            Self::Fixed { to_sun, to_moon } => Bodies::fixed(to_sun, to_moon),
        }
    }
}

/// What the sky of one frame is drawn for: where its lights are, what the
/// camera sees, and the wall's time — the one clock its budget is kept by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyFrame {
    pub bodies: Bodies,
    pub view: SkyView,
    /// The wall's present, seconds (any origin): how often the light is
    /// worked out again is measured on it, and nothing else reads a clock.
    pub wall: f64,
}

/// Where the camera is and what it sees, for the sky.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyView {
    /// The eye, ECEF metres.
    pub eye: DVec3,
    /// The origin tiles were rebased onto ([`crate::ContentPump::render_origin`]).
    pub render_origin: DVec3,
    /// Unit vectors, ECEF: where the camera looks, and its up.
    pub forward: DVec3,
    pub up: DVec3,
    /// The vertical field of view, radians, and width over height.
    pub fov_y: f64,
    pub aspect: f64,
    /// The picture's height in pixels: how large a star's image is.
    pub height_px: u32,
    /// The ground's height under the eye, metres over the ellipsoid, as far
    /// as it is known: where the light is worked out and what the meter
    /// meets.
    pub ground_height: f64,
}

impl SkyView {
    /// This view under the real sky of `instant` (Unix seconds UTC), at
    /// the wall's `wall`.
    pub fn at(self, instant: f64, wall: f64) -> SkyFrame {
        SkyFrame {
            bodies: Bodies::ephemeris(instant, self.eye),
            view: self,
            wall,
        }
    }
}

/// How a host wants the physical atmosphere, before it runs: on or off,
/// where its lights come from, what the air is made of, how it is drawn,
/// and its clock.
///
/// Off by default: a scene that is not a planet has no air, and its ground
/// keeps [`crate::ViewUniform::atmosphere`]'s closed-form haze, past it the
/// clear colour. A globe asks for it — the viewer does, from its flags.
///
/// The body is the terrain's: the air is laid on the sphere osculating its
/// ellipsoid under the eye, and the tables are laid out for an Earth-sized
/// planet — a planet of another radius is not offered.
///
/// Open to more (media, rings, several suns): built from
/// [`Self::default`], [`Self::earth`] or [`Self::tuned`] and the `with_*`
/// methods.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct AtmosphereConfig {
    pub enabled: bool,
    pub lights: Lights,
    /// The air: its molecules (Rayleigh), aerosols (Mie), ozone, their
    /// scale heights, its thickness and the ground's albedo
    /// ([`AtmosphereParams::default`] a clear day; see its presets).
    pub params: AtmosphereParams,
    /// How it is drawn: the calibration of its light against the imagery,
    /// the exposure, the white balance, the highlights, the discs' sizes and
    /// glare, the march. A film's default look by default.
    pub look: SkyLook,
    /// The scene's clock: start, offset, rate. The present in real time by
    /// default.
    pub clock: tuile_atmosphere::ClockSettings,
}

impl AtmosphereConfig {
    /// The Earth as it is: on, the real sun and Moon, a clear day's air and
    /// a film's look, at `clock`.
    pub fn earth(clock: tuile_atmosphere::ClockSettings) -> Self {
        Self {
            enabled: true,
            clock,
            ..Self::default()
        }
    }

    /// On or off.
    pub fn enabled(self, enabled: bool) -> Self {
        Self { enabled, ..self }
    }

    pub fn with_lights(self, lights: Lights) -> Self {
        Self { lights, ..self }
    }

    pub fn with_params(self, params: AtmosphereParams) -> Self {
        Self { params, ..self }
    }

    pub fn with_look(self, look: SkyLook) -> Self {
        Self { look, ..self }
    }

    pub fn with_clock(self, clock: tuile_atmosphere::ClockSettings) -> Self {
        Self { clock, ..self }
    }

    /// [`Self::earth`] with the air and the look a host read from its
    /// tuning values ([`tuile_atmosphere::AirTuning`]).
    pub fn tuned(
        air: &tuile_atmosphere::AirTuning,
        clock: tuile_atmosphere::ClockSettings,
    ) -> Self {
        Self {
            params: air.params,
            look: SkyLook::default().tuned(air),
            ..Self::earth(clock)
        }
    }
}

/// The physical atmosphere as it runs: a host's [`AtmosphereConfig`] with
/// its clock started. Every field may be changed between two frames — the
/// next [`crate::TileRenderer::draw_atmosphere`] draws what it says. Given
/// the same configuration and the same clock, the same sky: nothing here
/// reads a clock of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Atmosphere {
    pub enabled: bool,
    pub lights: Lights,
    pub params: AtmosphereParams,
    pub look: SkyLook,
    /// The scene's clock, running: shift it, set its rate, pause it.
    pub clock: tuile_atmosphere::SceneClock,
}

impl Atmosphere {
    /// `config`, its clock started at `wall` (Unix seconds) for a camera at
    /// `eye` (ECEF) — where a sunset or a sunrise start is resolved.
    pub fn new(config: AtmosphereConfig, wall: f64, eye: DVec3) -> Self {
        Self {
            enabled: config.enabled,
            lights: config.lights,
            params: config.params,
            look: config.look,
            clock: config.clock.start(wall, ecef_to_geodetic(eye)),
        }
    }

    /// The instant a frame at `wall` is lit for.
    pub fn instant(&self, wall: f64) -> f64 {
        self.clock.at(wall)
    }

    /// The frame of `view` at `wall`: its lights where [`Self::lights`]
    /// puts them at the clock's instant.
    pub fn frame(&self, view: SkyView, wall: f64) -> SkyFrame {
        SkyFrame {
            bodies: self.lights.at(self.instant(wall), view.eye),
            view,
            wall,
        }
    }
}

/// What a frame's light was worked out from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LightKey {
    to_sun: DVec3,
    to_moon: Option<DVec3>,
    eye: DVec3,
    forward: DVec3,
    clearance: f64,
    at: f64,
}

/// What the sky-view was made from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewKey {
    eye: DVec3,
    clearance: f64,
}

pub(crate) struct Sky {
    pub(crate) atmosphere: AtmosphereGpu,
    params: AtmosphereParams,
    look: SkyLook,
    light_key: Option<LightKey>,
    view_key: Option<ViewKey>,
    /// The last frame's sun and eye: what tells a jump — a host's clock set
    /// forward, a camera sent across the globe — from motion.
    last: Option<(DVec3, DVec3)>,
    exposure: f32,
    on: bool,
    /// How many times the light was worked out, and the sky-view made: what
    /// a test or a profile reads to see the budget kept.
    pub(crate) lights: u64,
    pub(crate) views: u64,
}

impl Sky {
    pub(crate) fn new(device: &wgpu::Device, full_layout: &wgpu::BindGroupLayout) -> Self {
        // Made in compute where the device has compute; drawn where it has
        // not (WebGL2). The same texel sums either way.
        let pass = if device_has_compute(device) {
            SkyViewPass::Compute
        } else {
            SkyViewPass::Render
        };
        Self {
            atmosphere: AtmosphereGpu::new(device, full_layout, pass),
            params: AtmosphereParams::default(),
            look: SkyLook::default(),
            light_key: None,
            view_key: None,
            last: None,
            exposure: 1.0,
            on: false,
            lights: 0,
            views: 0,
        }
    }

    pub(crate) fn is_on(&self) -> bool {
        self.on
    }

    pub(crate) fn exposure(&self) -> f32 {
        self.exposure
    }

    pub(crate) fn set_air(&mut self, params: AtmosphereParams, look: SkyLook) {
        if (params, look) != (self.params, self.look) {
            self.params = params;
            self.look = look;
            self.light_key = None;
        }
    }

    pub(crate) fn off(&mut self, queue: &wgpu::Queue) {
        if self.on {
            self.on = false;
            queue.write_buffer(
                &self.atmosphere.uniform,
                0,
                bytemuck::bytes_of(&AtmosphereUniform::zeroed_off()),
            );
        }
    }

    /// The frame's uniform, and — when the sun, the Moon or the eye moved
    /// enough — its light and its sky-view made again.
    pub(crate) fn update(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &SkyFrame) {
        let now = frame.wall;
        let bodies = frame.bodies;
        let frame = &frame.view;
        let to_sun = bodies.to_sun;
        if !to_sun.is_finite() || !frame.eye.is_finite() {
            self.off(queue);
            return;
        }
        let at_eye = ecef_to_geodetic(frame.eye);
        let moon = bodies.moon;
        let to_moon = moon.map(|m| m.to_moon_ecef);
        let clearance = (at_eye.height - frame.ground_height).max(1.0);
        let moved = |key: &LightKey| {
            let angle = |a: DVec3, b: DVec3| a.angle_between(b);
            let moon_moved = match (key.to_moon, to_moon) {
                (Some(a), Some(b)) => angle(a, b) > LIGHT_SUN_STEP,
                (None, None) => false,
                _ => true,
            };
            angle(key.to_sun, to_sun) > LIGHT_SUN_STEP
                || moon_moved
                || (frame.eye - key.eye).length() > EYE_STEP * key.clearance.min(clearance)
                || angle(key.forward, frame.forward) > LIGHT_VIEW_STEP
                // The ground under the eye found to be elsewhere — terrain
                // streaming in: the meter met the wrong ground.
                || (clearance / key.clearance).ln().abs() > GROUND_STEP
        };
        // A jump since the last frame is answered at once; motion, however
        // fast, once an interval.
        let jumped = self.last.is_some_and(|(sun, eye)| {
            sun.angle_between(to_sun) > JUMP_SUN
                || (frame.eye - eye).length() > JUMP_EYE * EYE_STEP * clearance
        });
        self.last = Some((to_sun, frame.eye));
        let due = match &self.light_key {
            None => true,
            Some(key) => moved(key) && (jumped || now - key.at >= LIGHT_INTERVAL),
        };
        self.atmosphere
            .hold_with(queue, self.params, LutAtmosphere::of);
        if due {
            let model = self.atmosphere.model.as_ref().expect("held above");
            let reference = self.atmosphere.reference.as_ref().expect("held above");
            let ground = Geodetic {
                height: frame.ground_height.clamp(0.0, at_eye.height.max(0.0)),
                ..at_eye
            };
            let (right, up) = view_axes(frame);
            let metered = metering(
                frame.eye,
                frame.forward,
                right,
                up,
                &AtmosphereFrame::at(ground),
                ground.height,
            );
            let light = model.light(
                reference,
                ground,
                Moment {
                    to_sun_ecef: to_sun,
                    moon,
                    compensation_ev: 0.0,
                },
                &metered,
                self.look.calibration,
            );
            self.exposure = self.look.exposure_scale(&light);
            self.atmosphere.light = Some(light);
            self.light_key = Some(LightKey {
                to_sun,
                to_moon,
                eye: frame.eye,
                forward: frame.forward,
                clearance,
                at: now,
            });
            self.view_key = None;
            self.lights += 1;
        }
        let light = self.atmosphere.light.expect("worked out above");
        // The sky-view's eye: the light's, or a newer one where the eye has
        // gone far enough from it that the table's horizon would be off.
        let view_stale = match &self.view_key {
            None => true,
            Some(key) => (frame.eye - key.eye).length() > EYE_STEP * key.clearance.min(clearance),
        };
        if view_stale {
            self.view_key = Some(ViewKey {
                eye: frame.eye,
                clearance,
            });
        }
        let view_eye = self.view_key.expect("set above").eye;
        let stars = bodies.stars.transpose();
        let uniform = AtmosphereUniform::new(
            &self.params,
            &light,
            view_eye,
            self.exposure,
            stars,
            &self.look,
        )
        .with_planets(&bodies.planets);
        if view_stale {
            // Written for the table's own pass, from its own eye, then made.
            queue.write_buffer(
                &self.atmosphere.uniform,
                0,
                bytemuck::bytes_of(&uniform.seen_from(&light, view_eye)),
            );
            self.atmosphere.sky_view_stale = true;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tuile sky view"),
            });
            self.atmosphere.record_sky_view(&mut encoder);
            queue.submit([encoder.finish()]);
            self.views += 1;
        }
        let placed = uniform
            .seen_from(&light, frame.eye)
            .with_eye_from_origin((frame.eye - frame.render_origin).as_vec3());
        queue.write_buffer(&self.atmosphere.uniform, 0, bytemuck::bytes_of(&placed));
        self.atmosphere.frame_uniform = placed;
        self.on = true;
    }
}

/// The camera's right and up, ECEF, each scaled to the half-extent of the
/// picture at unit distance — what the meter's rays are spread over.
fn view_axes(frame: &SkyView) -> (DVec3, DVec3) {
    let forward = frame.forward.normalize_or(DVec3::X);
    let right = forward
        .cross(frame.up)
        .normalize_or(forward.any_orthonormal_vector());
    let up = right.cross(forward);
    let half = (frame.fov_y * 0.5).tan();
    (right * half * frame.aspect, up * half)
}

/// The radius of a star's image, radians: about a pixel.
pub(crate) fn star_sigma(frame: &SkyView) -> f32 {
    let pixel = 2.0 * (frame.fov_y * 0.5).tan() / f64::from(frame.height_px.max(1));
    (0.8 * pixel) as f32
}

/// Whether the device runs compute shaders: everything but WebGL2.
pub(crate) fn device_has_compute(device: &wgpu::Device) -> bool {
    // A device does not report its adapter's downlevel flags; the backend
    // that lacks compute is GL, and its limits say so: no workgroup at all.
    device.limits().max_compute_workgroups_per_dimension > 0
}

/// `view_proj`, inverted: from a pixel back to its line of sight. The
/// identity for a projection that has none — the sky then draws nothing
/// rather than not a number.
pub(crate) fn inverse_view_proj(view_proj: &[f32; 16]) -> Mat4 {
    let inverse = DMat4::from_cols_array(&view_proj.map(f64::from)).inverse();
    let inverse = inverse.as_mat4();
    if inverse.is_finite() {
        inverse
    } else {
        Mat4::IDENTITY
    }
}

/// Where `geodetic` is, for a test or a host placing a camera.
pub fn ecef_of(lon_deg: f64, lat_deg: f64, height: f64) -> DVec3 {
    geodetic_to_ecef(Geodetic {
        lon: lon_deg.to_radians(),
        lat: lat_deg.to_radians(),
        height,
    })
}
