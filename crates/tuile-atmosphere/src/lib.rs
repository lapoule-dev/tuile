// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod aerial;
pub mod clock;
#[cfg(feature = "wgpu")]
pub mod frame_light;
#[cfg(feature = "wgpu")]
pub mod gpu;
pub mod hillaire;
pub mod illumination;
pub mod look;
pub mod moon;
pub mod params;
pub mod planets;
pub mod sky;
pub mod stars;
pub mod sun;
pub mod tuning;

pub use aerial::{AerialAir, AerialPerspective, WGSL};
pub use clock::{ClockSettings, Event, SceneClock, Start};
pub use hillaire::{Calibration, LutAtmosphere, SkyLight};
#[allow(deprecated)]
pub use illumination::MAX_ADAPTATION_EV;
pub use illumination::{
    shown_ev, CameraResponse, Moonlight, ReferenceIllumination, DAYLIGHT_EV100, EXPOSURE_FOLLOWS,
    NIGHT_DEPTH_EV,
};
pub use look::SkyLook;
pub use moon::{celestial_to_ecef, moon_irradiance, Moon, MoonSeen};
pub use params::{AtmosphereFrame, AtmosphereParams};
pub use sky::SkyShell;
pub use sun::{Sun, SunSeen};
pub use tuning::{AirTuning, LookTuning, TuningError};
