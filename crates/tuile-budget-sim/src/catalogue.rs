// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The scenario catalogue (spec §9.3): named, scripted, deterministic camera
//! paths over the simulation's world — sea in the west, a coastal plain,
//! mountains in the east, a 3D Tiles city on the plain — crossed with input
//! profiles. The simulator always runs it whole, and the parameter search
//! scores across it, never on one trajectory.
//!
//! Adding, removing or changing a scenario bumps [`CATALOGUE_VERSION`],
//! recorded in every result, so two results compare only on one version.
//!
//! Not yet in this version (listed in the spec's plan): the warm disk cache
//! axis, the memory squeezes, and the film driven by a pack file (PR 8).

use crate::server::Profile;
use crate::world::{Camera, SIDE};

/// The catalogue's version: bumped with any change of a path, a profile or a
/// bound.
pub const CATALOGUE_VERSION: u32 = 1;

/// A camera path: where the eye is at `t` seconds.
pub type Path = fn(f64) -> Camera;

/// One scripted scenario: a path, how long it moves, whether it ends at rest
/// (then it is watched still for [`REST_SECONDS`]).
#[derive(Clone, Copy, Debug)]
pub struct Scenario {
    /// Its name in the tables.
    pub name: &'static str,
    /// The path.
    pub path: Path,
    /// How long the path moves, seconds.
    pub seconds: f64,
    /// Whether it ends at rest (the deficit must then fall to 0).
    pub rests: bool,
    /// Over the 3D Tiles city.
    pub city: bool,
    /// Two eyes at 90 Hz.
    pub headset: bool,
    /// Its phases: each a name and the time it ends, seconds (empty: one).
    pub phases: &'static [(&'static str, f64)],
}

/// How long a path that ends at rest is watched still.
pub const REST_SECONDS: f64 = 4.0;

/// The device budget axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    /// 2 GiB, a good network's quotas.
    Desktop,
    /// 512 MiB, a phone's.
    Phone,
}

/// The network axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// 20 ms, 50 MB/s.
    Fast,
    /// 150 ms, 2 MB/s.
    Slow,
    /// Slow, and 10 % of requests failing.
    Flaky,
}

impl Device {
    /// Every device.
    pub const ALL: [Device; 2] = [Device::Desktop, Device::Phone];

    /// Its name in the tables.
    pub fn label(self) -> &'static str {
        match self {
            Device::Desktop => "desktop",
            Device::Phone => "phone",
        }
    }
}

impl Network {
    /// Every network.
    pub const ALL: [Network; 3] = [Network::Fast, Network::Slow, Network::Flaky];

    /// Its name in the tables.
    pub fn label(self) -> &'static str {
        match self {
            Network::Fast => "fast",
            Network::Slow => "slow",
            Network::Flaky => "flaky",
        }
    }
}

/// The simulation's profile for `device` on `network`, two eyes when
/// `headset`.
pub(crate) fn profile(device: Device, network: Network, headset: bool) -> Profile {
    let mut p = match (device, headset) {
        (_, true) => Profile::headset(90),
        (Device::Desktop, false) => Profile::desktop(),
        (Device::Phone, false) => Profile::handheld(),
    };
    for s in [&mut p.net.meshes, &mut p.net.imagery] {
        // No stalls: a stall is a timeout's matter, not the decider's.
        s.stall_permille = 0;
        match network {
            Network::Fast => {
                s.base_ms = 20.0;
                s.jitter_ms = 10.0;
                s.bandwidth = 50.0e6;
                s.fail_permille = 0;
            }
            Network::Slow => {
                s.base_ms = 150.0;
                s.jitter_ms = 50.0;
                s.bandwidth = 2.0e6;
                s.fail_permille = 5;
            }
            Network::Flaky => {
                s.base_ms = 150.0;
                s.jitter_ms = 80.0;
                s.bandwidth = 2.0e6;
                s.fail_permille = 100;
            }
        }
    }
    p
}

/// The desktop's profile on the budget `cfg` (the viewer-scale runs).
pub fn desktop_on(cfg: tuile_budget::BudgetConfig) -> Profile {
    let mut p = Profile::desktop();
    p.budget = cfg;
    p
}

/// The soak's four-minute tour, looped: orbit, pan, zoom, turn.
pub fn tour(t: f64) -> Camera {
    crate::soak::tour(t)
}

/// Over the mountains, east of the plain.
const MOUNTAINS: (f64, f64) = (0.78 * SIDE, 0.5 * SIDE);
/// Over the plain.
const PLAIN: (f64, f64) = (0.6 * SIDE, 0.5 * SIDE);

fn at(place: (f64, f64), h: f64, heading_deg: f64, pitch_deg: f64) -> Camera {
    Camera {
        x: place.0,
        y: place.1,
        h,
        heading: heading_deg.to_radians(),
        pitch: pitch_deg.to_radians(),
    }
}

/// `a` to `b` geometrically (heights), `u` in `[0, 1]`.
fn geo(a: f64, b: f64, u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    crate::detmath::exp2(
        crate::detmath::log2(a) + (crate::detmath::log2(b) - crate::detmath::log2(a)) * u,
    )
}

/// Smooth step in `[0, 1]`.
fn ease(u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    u * u * (3.0 - 2.0 * u)
}

fn still_low(_: f64) -> Camera {
    at(MOUNTAINS, 300.0, 90.0, 25.0)
}
fn still_mid(_: f64) -> Camera {
    at(MOUNTAINS, 3_000.0, 90.0, 30.0)
}
fn still_high(_: f64) -> Camera {
    at(MOUNTAINS, 300_000.0, 90.0, 60.0)
}
fn slow_dive(t: f64) -> Camera {
    at(MOUNTAINS, geo(400_000.0, 500.0, ease(t / 40.0)), 90.0, 30.0)
}
fn fast_dive(t: f64) -> Camera {
    at(MOUNTAINS, geo(400_000.0, 500.0, ease(t / 4.0)), 90.0, 30.0)
}
fn climb_to_orbit(t: f64) -> Camera {
    at(MOUNTAINS, geo(500.0, 1_500_000.0, ease(t / 10.0)), 90.0, 40.0)
}
fn yaw_sweep(t: f64) -> Camera {
    at(MOUNTAINS, 3_000.0, 360.0 * ease(t / 6.0), 25.0)
}
fn tilt_to_horizon(t: f64) -> Camera {
    at(MOUNTAINS, 1_000.0, 90.0, 90.0 - 85.0 * ease(t / 5.0))
}
fn long_pan(t: f64) -> Camera {
    let mut c = at(PLAIN, 800.0, 0.0, 30.0);
    c.x += 40_000.0 * (t / 60.0).clamp(0.0, 1.0);
    c
}
fn zoom_thrash(t: f64) -> Camera {
    // Ten in-and-outs in 20 s: 3 km ↔ 24 km.
    let phase = (t / 2.0).fract();
    let u = if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 };
    at(MOUNTAINS, geo(3_000.0, 24_000.0, ease(u)), 90.0, 30.0)
}
fn city_fly_through(t: f64) -> Camera {
    let mut c = at(
        (crate::world::CITY_X0 + 2_000.0, crate::world::CITY_Y0 + 2_000.0),
        150.0,
        45.0,
        15.0,
    );
    let d = 25.0 * t.min(30.0);
    c.x += d * 0.707;
    c.y += d * 0.707;
    c
}
fn headset_jitter(t: f64) -> Camera {
    let (s, _) = crate::detmath::sin_cos(t * 4.0 * std::f64::consts::TAU);
    let mut c = at(MOUNTAINS, 1_500.0, 90.0 + 0.5 * s, 20.0);
    c.x += 2.0 * t;
    c
}
/// FPV along a valley: 40–80 m over the ground at 35 m/s, banking turns.
fn fpv_valley(t: f64) -> Camera {
    let (s, _) = crate::detmath::sin_cos(t * 0.5);
    let (sh, _) = crate::detmath::sin_cos(t * 0.3);
    let mut c = at(MOUNTAINS, 1_200.0 + 20.0 * sh, 90.0 + 30.0 * s, 5.0);
    c.x += 35.0 * t.min(45.0);
    c.y += 400.0 * s;
    c
}
/// FPV along a ridge: 45 m/s, yaw reversals every 4 s, a dive at 30 s.
fn fpv_ridge(t: f64) -> Camera {
    let t = t.min(45.0);
    let rev = if (t / 4.0).floor() as i64 % 2 == 0 { 1.0 } else { -1.0 };
    let dive = if t > 30.0 { -40.0 * (t - 30.0) } else { 0.0 };
    let mut c = at(MOUNTAINS, (2_500.0 + dive).max(600.0), 90.0 + 20.0 * rev, 8.0);
    c.x += 45.0 * t;
    c
}

/// Laurent's sequence (2026-11-21), aggressive at every phase: zoom-in,
/// fast pan, tilt, full rotation, zoom-out, one continuous path.
pub const AGGRESSIVE_PHASES: &[(&str, f64)] = &[
    ("zoom-in", 3.0),
    ("pan", 6.0),
    ("tilt", 8.0),
    ("rotation", 11.0),
    ("zoom-out", 14.0),
    ("rest", f64::INFINITY),
];

fn aggressive_sequence(t: f64) -> Camera {
    let base = at(MOUNTAINS, 1_000.0, 90.0, 80.0);
    if t < 3.0 {
        return Camera {
            h: geo(300_000.0, 1_000.0, ease(t / 3.0)),
            ..base
        };
    }
    let mut c = base;
    let pan = 20_000.0 * ease((t - 3.0) / 3.0);
    c.x += pan;
    if t < 6.0 {
        return c;
    }
    c.pitch = (80.0 - 70.0 * ease((t - 6.0) / 2.0)).to_radians();
    if t < 8.0 {
        return c;
    }
    c.heading = (90.0 + 360.0 * ease((t - 8.0) / 3.0)).to_radians();
    if t < 11.0 {
        return c;
    }
    c.h = geo(1_000.0, 300_000.0, ease((t - 11.0) / 3.0));
    c
}

/// The catalogue, version [`CATALOGUE_VERSION`].
pub fn scenarios() -> Vec<Scenario> {
    let s = |name, path, seconds, rests| Scenario {
        name,
        path,
        seconds,
        rests,
        city: false,
        headset: false,
        phases: &[],
    };
    vec![
        Scenario {
            phases: AGGRESSIVE_PHASES,
            ..s("aggressive-sequence", aggressive_sequence as Path, 14.0, true)
        },
        s("still-low", still_low, 6.0, true),
        s("still-mid", still_mid, 6.0, true),
        s("still-high", still_high, 6.0, true),
        s("slow-dive", slow_dive, 40.0, true),
        s("fast-dive", fast_dive, 4.0, true),
        s("climb-to-orbit", climb_to_orbit, 10.0, true),
        s("yaw-sweep", yaw_sweep, 6.0, true),
        s("tilt-to-horizon", tilt_to_horizon, 5.0, true),
        s("long-pan", long_pan, 60.0, true),
        s("zoom-thrash", zoom_thrash, 20.0, true),
        Scenario {
            city: true,
            ..s("city-fly-through", city_fly_through, 30.0, true)
        },
        Scenario {
            headset: true,
            ..s("headset-jitter", headset_jitter, 10.0, false)
        },
        s("fpv-valley", fpv_valley, 45.0, true),
        s("fpv-ridge", fpv_ridge, 45.0, true),
    ]
}

/// The scenario named `name`.
pub fn named(name: &str) -> Option<Scenario> {
    scenarios().into_iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_names_each_scenario_once() {
        let names: Vec<_> = scenarios().iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        assert!(named("fpv-ridge").is_some());
    }

    #[test]
    fn the_paths_are_deterministic_and_stay_above_the_ground() {
        for s in scenarios() {
            for k in 0..=100 {
                let t = s.seconds * f64::from(k) / 100.0;
                let (a, b) = ((s.path)(t), (s.path)(t));
                assert_eq!(a.x.to_bits(), b.x.to_bits(), "{}", s.name);
                assert!(a.h > 50.0 && a.h.is_finite(), "{} at {t}: {}", s.name, a.h);
            }
        }
    }
}
