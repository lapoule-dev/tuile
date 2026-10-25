// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The sky and the light on the ground, set from the host.
//!
//! [`TuileAtmosphere`] is tuile-wgpu's atmosphere as a Bevy resource. It
//! holds the same values the film and the viewer take, and no second model
//! of the air:
//!
//! - **whether** it is drawn ([`TuileAtmosphere::enabled`]). Off, the
//!   ground is lit by [`crate::TuileLight`] and no sky is drawn: the
//!   camera's clear colour shows past the ground, as before;
//! - **where the light comes from** ([`Lights`]):
//!   - `Ephemeris` is the real sun and Moon at the clock's instant, read as
//!     UTC;
//!   - `Orbit` is a world of the host's own, a day of `day_length` clock
//!     seconds, seasons if it has a tilt;
//!   - `Fixed` takes the directions the host sets;
//! - **the air** ([`AirTuning`]): the body and its atmosphere
//!   ([`AtmosphereParams`]: planet radius, the atmosphere's height, the
//!   scattering, the sun's size and intensity; presets `earth`, `clear`,
//!   `hazy`), how the sky is drawn, and the air the ground is seen through;
//! - **the clock** ([`ClockSettings`]: a start — now, an instant, the next
//!   sunset or sunrise over the camera — an offset and a rate). It runs on
//!   the wall's time, and [`TuileAtmosphere::clock_mut`] shifts, sets,
//!   pauses or speeds it up from there without a jump.
//!
//! Every field can be changed at any frame. What it comes to is read back,
//! for the host's own lighting: [`TuileAtmosphere::to_sun`],
//! [`TuileAtmosphere::to_moon`] and [`TuileAtmosphere::instant`]. A
//! `DirectionalLight` aligned with tuile's sun lights the host's meshes as
//! the ground is lit (`examples/bevy-viewer`).
//!
//! **Deterministic.** The scene's instant is a function of the settings and
//! of the wall clock. The sun and the Moon are functions of that instant and
//! of the eye. Two clients with the same settings, the same clock and the
//! same camera see the same sky.

use bevy::prelude::*;
use glam::DVec3;

pub use tuile_atmosphere::{
    AirTuning, AtmosphereParams, ClockSettings, SceneClock, SkyLook, Start,
};
pub use tuile_wgpu::{Bodies, Lights};

use crate::camera::TuileCamera;

/// The atmosphere: see the module.
#[derive(Resource, Debug, Clone)]
pub struct TuileAtmosphere {
    /// Drawn: the sky, the sun's light on every point, the air before the
    /// ground. Off by default.
    pub enabled: bool,
    pub lights: Lights,
    pub tuning: AirTuning,
    /// Where the clock starts. Changing it starts the clock again from it.
    pub clock: ClockSettings,
    /// The atmosphere as it runs: settings applied, and its clock.
    state: Option<Running>,
    /// What the last frame came to.
    now: Now,
}

#[derive(Debug, Clone, Copy)]
struct Running {
    atmosphere: tuile_wgpu::Atmosphere,
    /// The settings the clock was started from.
    clock: ClockSettings,
}

/// One frame's sky: the wall instant it is drawn at, and what it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Now {
    wall: f64,
    instant: f64,
    to_sun: DVec3,
    to_moon: Option<DVec3>,
}

impl Default for TuileAtmosphere {
    /// Off: the flat light the globe had before ([`crate::TuileLight`]).
    fn default() -> Self {
        Self::earth(ClockSettings::default()).enabled(false)
    }
}

impl TuileAtmosphere {
    /// The Earth's atmosphere and its real sun and Moon, on `clock`. On.
    pub fn earth(clock: ClockSettings) -> Self {
        Self {
            enabled: true,
            lights: Lights::default(),
            tuning: AirTuning::default(),
            clock,
            state: None,
            now: Now {
                wall: 0.0,
                instant: 0.0,
                to_sun: DVec3::Z,
                to_moon: None,
            },
        }
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_lights(mut self, lights: Lights) -> Self {
        self.lights = lights;
        self
    }

    /// The body and its air: `AtmosphereParams::earth()`, `clear()`,
    /// `hazy()`, or a world of the host's own.
    #[must_use]
    pub fn with_params(mut self, params: AtmosphereParams) -> Self {
        self.tuning.params = params;
        self
    }

    #[must_use]
    pub fn with_tuning(mut self, tuning: AirTuning) -> Self {
        self.tuning = tuning;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: ClockSettings) -> Self {
        self.clock = clock;
        self
    }

    /// The scene's clock, running: shift, set, pause, set its rate, back
    /// to now — each from the wall instant [`Self::wall`] says. `None`
    /// until the first frame has started it.
    pub fn clock_mut(&mut self) -> Option<&mut SceneClock> {
        self.state.as_mut().map(|s| &mut s.atmosphere.clock)
    }

    /// The wall instant the last frame was drawn at, Unix seconds.
    pub fn wall(&self) -> f64 {
        self.now.wall
    }

    /// The scene's instant the last frame showed, the clock's seconds (Unix
    /// seconds UTC for `Ephemeris`).
    pub fn instant(&self) -> f64 {
        self.now.instant
    }

    /// Towards the sun, unit, ECEF — Bevy's world axes. A `DirectionalLight`
    /// shines along its `-Z`: `Transform::looking_to(-to_sun, …)` lights the
    /// host's meshes as the ground is lit.
    pub fn to_sun(&self) -> DVec3 {
        self.now.to_sun
    }

    /// Towards the Moon, unit, ECEF, when there is one.
    pub fn to_moon(&self) -> Option<DVec3> {
        self.now.to_moon
    }

    /// The atmosphere as tuile-wgpu draws it this frame, for the render
    /// world.
    pub(crate) fn running(&self) -> Option<(tuile_wgpu::Atmosphere, f64)> {
        self.state.map(|s| (s.atmosphere, self.now.wall))
    }

    /// The frame at `wall` for an eye at `eye`: the settings applied, the
    /// clock started again if its settings changed, the bodies worked out.
    pub(crate) fn advance(&mut self, wall: f64, eye: DVec3) {
        let restart = self.state.is_none_or(|s| s.clock != self.clock);
        if restart {
            let config = tuile_wgpu::AtmosphereConfig::tuned(&self.tuning, self.clock);
            self.state = Some(Running {
                atmosphere: tuile_wgpu::Atmosphere::new(config, wall, eye),
                clock: self.clock,
            });
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let a = &mut state.atmosphere;
        a.enabled = self.enabled;
        a.lights = self.lights;
        let tuned = tuile_wgpu::AtmosphereConfig::tuned(&self.tuning, self.clock);
        a.params = tuned.params;
        a.look = tuned.look;
        let instant = a.instant(wall);
        let bodies = a.lights.at(instant, eye);
        self.now = Now {
            wall,
            instant,
            to_sun: bodies.to_sun,
            to_moon: bodies.moon.map(|m| m.to_moon_ecef),
        };
    }
}

/// The wall's present, Unix seconds UTC.
pub(crate) fn wall_seconds() -> f64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// This frame's sky, for the camera as it stands.
pub(crate) fn advance_atmosphere(
    mut atmosphere: ResMut<TuileAtmosphere>,
    cameras: Query<&TuileCamera>,
) {
    let Some(eye) = cameras.iter().next().map(|c| c.pose.position) else {
        return;
    };
    atmosphere.advance(wall_seconds(), eye);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};

    fn eye() -> DVec3 {
        geodetic_to_ecef(Geodetic {
            lon: 10f64.to_radians(),
            lat: 45f64.to_radians(),
            height: 3_000.0,
        })
    }

    /// The same settings and the same wall clock: the same sky, on two
    /// clients.
    #[test]
    fn two_clients_with_the_same_settings_see_the_same_sky() {
        let clock = ClockSettings {
            start: Start::At(1_790_000_000.0),
            offset: 3_600.0,
            rate: 60.0,
        };
        let mut a = TuileAtmosphere::earth(clock);
        let mut b = TuileAtmosphere::earth(clock);
        a.advance(1_000.0, eye());
        b.advance(1_000.0, eye());
        a.advance(1_010.0, eye());
        b.advance(1_010.0, eye());
        assert_eq!(a.to_sun(), b.to_sun());
        assert_eq!(a.instant(), b.instant());
        // Ten wall seconds at sixty: ten scene minutes.
        assert!(
            (a.instant() - (1_790_000_000.0 + 3_600.0 + 600.0)).abs() < 1e-6,
            "{}",
            a.instant()
        );
    }

    /// A fixed sun is where the host puts it; an orbit turns with its day.
    #[test]
    fn the_light_is_where_the_lights_say() {
        let to_sun = DVec3::new(1.0, 2.0, 3.0).normalize();
        let mut fixed = TuileAtmosphere::default().with_lights(Lights::fixed(to_sun, None));
        fixed.advance(0.0, eye());
        assert!((fixed.to_sun() - to_sun).length() < 1e-9);
        assert_eq!(fixed.to_moon(), None);

        let mut world = TuileAtmosphere::default()
            .with_lights(Lights::orbit(600.0))
            .with_clock(ClockSettings {
                start: Start::At(0.0),
                offset: 0.0,
                rate: 1.0,
            });
        world.advance(0.0, eye());
        let noon = world.to_sun();
        world.advance(300.0, eye());
        // Half a ten-minute day later the sun is on the other side.
        assert!(
            world.to_sun().dot(noon) < -0.5,
            "{} vs {}",
            world.to_sun(),
            noon
        );
    }

    /// A change of the clock's settings starts it again from them; the
    /// running clock can be paused from the host.
    #[test]
    fn the_clock_is_the_hosts() {
        let mut a = TuileAtmosphere::earth(ClockSettings {
            start: Start::At(1_000.0),
            offset: 0.0,
            rate: 1.0,
        });
        a.advance(50.0, eye());
        a.advance(60.0, eye());
        assert!((a.instant() - 1_010.0).abs() < 1e-9);
        let wall = a.wall();
        a.clock_mut().expect("running").pause(wall);
        a.advance(100.0, eye());
        assert!(
            (a.instant() - 1_010.0).abs() < 1e-9,
            "paused: {}",
            a.instant()
        );
        a.clock = ClockSettings {
            start: Start::At(5_000.0),
            offset: 0.0,
            rate: 1.0,
        };
        a.advance(200.0, eye());
        assert!(
            (a.instant() - 5_000.0).abs() < 1e-9,
            "restarted: {}",
            a.instant()
        );
    }
}
