// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A time-lapse of the sky: a camera held at one place while days go by,
//! and the clock that says which instant each frame shows.
//!
//! One trajectory for every such film — on a summit turning on itself to
//! show the sky all round, or at the edge of space looking at the limb —
//! parameterised by what differs between them: where, how high, when and
//! for how long, how long the film and at what rate, which frame the camera
//! is held in, and how it moves in it.
//!
//! - **Held to the Earth** ([`CameraFrame::Earth`]): the camera stays over
//!   its place; the sun and the Moon cross its sky, rise and set over its
//!   horizon, and the terminator sweeps its ground twice a day.
//! - **Held to the stars** ([`CameraFrame::Inertial`]): the camera keeps its
//!   place and its aim in a frame that does not turn with the Earth, which
//!   turns beneath it once a sidereal day. Sunlight comes from the same
//!   side all along, so the terminator stays where it is in the picture
//!   while the ground flows through it, and the sun neither rises nor sets
//!   — only the Moon, a dozen degrees a day, moves across the stars.
//!
//! The camera's aim is a heading and a pitch over the place's horizon at
//! the first frame; it can turn on itself ([`Motion::Spin`]) and nod
//! ([`TimeLapse::pitch_swing_deg`]).
//!
//! Time is not on the tape — a tape is cameras — but in [`TimeLapse::clock`]:
//! the first frame's instant and the seconds of sky between two frames,
//! which a renderer lights each frame by.

use crate::Frame;
use glam::{DMat3, DVec3};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

/// Where a body stands over a place: degrees from north through east, and
/// over the horizon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    pub azimuth_deg: f64,
    pub elevation_deg: f64,
}

/// Where the sun and the Moon stand over a place at an instant (Unix
/// seconds UTC): what a tracking camera ([`Motion::Track`]) turns to. Given
/// by whoever computes ephemerides — the tape knows no astronomy.
pub trait Sky {
    fn sun(&self, utc: f64, at: Geodetic) -> Option<Seen>;
    fn moon(&self, utc: f64, at: Geodetic) -> Option<Seen>;
}

/// The Earth's rotation rate against the stars, radians a second (IERS).
pub const EARTH_ROTATION: f64 = 7.292_115_0e-5;

/// Which frame the camera is held still in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFrame {
    Earth,
    Inertial,
}

/// How the camera moves in its frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    /// Its aim held.
    Held,
    /// Turning on itself about the vertical, this many whole turns over the
    /// film, clockwise seen from above.
    Spin { turns: f64 },
    /// Turned to the sun while it is up and through twilight, to the Moon
    /// when the sun is down and the Moon up, and raised to keep the one it
    /// follows in the picture with the ground under it: every day's
    /// sunrise and sunset, every night's moonrise and moonset, in frame.
    /// Its heading and pitch are eased over [`TRACK_EASE_S`] seconds of
    /// film, so that the turn from the set sun to the risen Moon is a pan.
    Track,
}

/// How long, in seconds of film, a tracking camera takes to settle on a
/// new aim.
pub const TRACK_EASE_S: f64 = 0.15;

/// A time-lapse of the sky. See the module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeLapse {
    /// The place, degrees, and the camera's height over the ellipsoid,
    /// metres.
    pub lon_deg: f64,
    pub lat_deg: f64,
    pub height_m: f64,
    /// The first frame's instant, Unix seconds UTC.
    pub start_utc: f64,
    /// Seconds of sky the film spans, from its first frame to past its
    /// last.
    pub duration_s: f64,
    /// Seconds of film, and frames a second.
    pub length_s: f64,
    pub fps: u32,
    pub frame: CameraFrame,
    pub motion: Motion,
    /// The aim at the first frame: degrees clockwise from north, and over
    /// the horizon (negative: down).
    pub heading_deg: f64,
    pub pitch_deg: f64,
    /// A slow nod about that pitch, degrees either way, and its period in
    /// seconds of film. Zero: none.
    pub pitch_swing_deg: f64,
    pub pitch_period_s: f64,
    /// The vertical field of view, degrees.
    pub fovy_deg: f64,
}

pub use crate::Clock;

impl TimeLapse {
    /// How many frames the film has.
    pub fn frames(&self) -> u32 {
        (self.length_s * f64::from(self.fps)).round().max(1.0) as u32
    }

    pub fn clock(&self) -> Clock {
        Clock {
            start_utc: self.start_utc,
            seconds_per_frame: self.duration_s / f64::from(self.frames()),
        }
    }

    /// The camera of frame `frame`, from 1, for a camera that is not
    /// tracking ([`Motion::Held`] or [`Motion::Spin`]); a tracking one is
    /// aimed by [`TimeLapse::cameras`].
    pub fn camera(&self, frame: u32) -> Frame {
        let (heading, pitch) = self.aim(frame, None);
        self.camera_aimed(frame, heading, pitch)
    }

    /// The cameras of every frame, in order — a tracking camera's aim
    /// taken from `sky` and eased from frame to frame ([`TRACK_EASE_S`]).
    /// A tracking camera with no sky is refused.
    pub fn cameras(&self, sky: Option<&dyn Sky>) -> Result<Vec<Frame>, String> {
        let frames = self.frames();
        if self.motion != Motion::Track {
            return Ok((1..=frames).map(|f| self.camera(f)).collect());
        }
        let sky = sky.ok_or("a camera tracking the sun and the Moon wants their ephemerides")?;
        let k = 1.0 - (-1.0 / (TRACK_EASE_S * f64::from(self.fps.max(1)))).exp();
        let mut eased: Option<(f64, f64)> = None;
        Ok((1..=frames)
            .map(|f| {
                let (h, p) = self.aim(f, Some(sky));
                let (eh, ep) = match eased {
                    None => (h, p),
                    Some((eh, ep)) => {
                        // The short way round.
                        let turn = (h - eh + std::f64::consts::PI)
                            .rem_euclid(std::f64::consts::TAU)
                            - std::f64::consts::PI;
                        (eh + turn * k, ep + (p - ep) * k)
                    }
                };
                eased = Some((eh, ep));
                self.camera_aimed(f, eh, ep)
            })
            .collect())
    }

    /// Frame `frame`'s heading and pitch, radians, before any easing.
    pub fn aim(&self, frame: u32, sky: Option<&dyn Sky>) -> (f64, f64) {
        let n = f64::from(frame.saturating_sub(1));
        let along = n / f64::from(self.frames());
        let seconds = n / f64::from(self.fps.max(1));
        let swing = if self.pitch_swing_deg != 0.0 && self.pitch_period_s > 0.0 {
            self.pitch_swing_deg * (std::f64::consts::TAU * seconds / self.pitch_period_s).sin()
        } else {
            0.0
        };
        match self.motion {
            Motion::Held => (
                self.heading_deg.to_radians(),
                (self.pitch_deg + swing).to_radians(),
            ),
            Motion::Spin { turns } => (
                self.heading_deg.to_radians() + std::f64::consts::TAU * turns * along,
                (self.pitch_deg + swing).to_radians(),
            ),
            Motion::Track => match sky {
                Some(sky) => self.tracked(frame, swing, sky),
                None => (
                    self.heading_deg.to_radians(),
                    (self.pitch_deg + swing).to_radians(),
                ),
            },
        }
    }

    /// Where a tracking camera turns at frame `frame`: see [`Motion::Track`].
    fn tracked(&self, frame: u32, swing: f64, sky: &dyn Sky) -> (f64, f64) {
        let place = Geodetic {
            lon: self.lon_deg.to_radians(),
            lat: self.lat_deg.to_radians(),
            height: self.height_m,
        };
        let t = self.clock().instant(frame);
        let step = |a: f64, b: f64, x: f64| {
            let u = ((x - a) / (b - a)).clamp(0.0, 1.0);
            u * u * (3.0 - 2.0 * u)
        };
        let sun = sky.sun(t, place);
        let moon = sky.moon(t, place);
        let (Some(sun), Some(moon)) = (sun, moon) else {
            return (self.heading_deg.to_radians(), self.pitch_deg.to_radians());
        };
        // The sun's weight: whole while it is up and through civil twilight.
        let w = step(-9.0, -3.0, sun.elevation_deg);
        let m = step(-4.0, 1.0, moon.elevation_deg) * (1.0 - w);
        let towards = |az: f64| glam::DVec2::new(az.to_radians().sin(), az.to_radians().cos());
        let mut dir = towards(sun.azimuth_deg) * w + towards(moon.azimuth_deg) * m;
        let rest = 1.0 - w - m;
        dir += towards(sun.azimuth_deg) * rest;
        let heading = dir.x.atan2(dir.y);
        let elevation = sun.elevation_deg.max(-2.0) * (w + rest) + moon.elevation_deg.max(-2.0) * m;
        let pitch =
            (self.pitch_deg + 0.6 * elevation).clamp(self.pitch_deg, self.pitch_deg + 24.0) + swing;
        (heading, pitch.to_radians())
    }

    /// The camera of frame `frame` aimed at `heading` and `pitch`, radians.
    fn camera_aimed(&self, frame: u32, heading: f64, pitch: f64) -> Frame {
        let place = Geodetic {
            lon: self.lon_deg.to_radians(),
            lat: self.lat_deg.to_radians(),
            height: self.height_m,
        };
        let eye = geodetic_to_ecef(place);
        let enu: DMat3 = enu_frame(place);
        let n = f64::from(frame.saturating_sub(1));
        let level = enu.x_axis * heading.sin() + enu.y_axis * heading.cos();
        let direction = level * pitch.cos() + enu.z_axis * pitch.sin();
        let up = (enu.z_axis - direction * direction.dot(enu.z_axis)).normalize();
        // Held to the stars: everything turned back by as much as the Earth
        // has turned since the first frame.
        let turn = match self.frame {
            CameraFrame::Earth => DMat3::IDENTITY,
            CameraFrame::Inertial => {
                DMat3::from_rotation_z(-EARTH_ROTATION * n * self.clock().seconds_per_frame)
            }
        };
        let (eye, direction, up) = (turn * eye, turn * direction, turn * up);
        Frame {
            position: eye.to_array(),
            direction: direction.normalize().to_array(),
            up: up.normalize().to_array(),
            fovy: self.fovy_deg.to_radians(),
        }
    }

    /// From `key=value` words: `place=<lon>,<lat>` (degrees), `height=<m>`,
    /// `start=<ISO 8601 UTC>`, `days=<d>` or `hours=<h>`, `length=<s>`,
    /// `fps=<n>`, `frame=earth|inertial`, `spin=<turns>` or `track=sun-moon`
    /// (none: held),
    /// `heading=<deg>`, `pitch=<deg>`, `swing=<deg>,<period s>`,
    /// `fov=<deg>` (vertical). What is not given is [`TimeLapse::default`]'s.
    pub fn parse(words: &[String]) -> Result<Self, String> {
        let mut t = Self::default();
        for word in words {
            let (key, value) = word
                .split_once('=')
                .ok_or_else(|| format!("{word}: a time-lapse takes key=value words"))?;
            let number = |v: &str| -> Result<f64, String> {
                v.parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite())
                    .ok_or_else(|| format!("{key}: not a number: {v}"))
            };
            let pair = |v: &str| -> Result<(f64, f64), String> {
                let (a, b) = v
                    .split_once(',')
                    .ok_or_else(|| format!("{key}: two numbers, a comma between"))?;
                Ok((number(a)?, number(b)?))
            };
            match key {
                "place" => (t.lon_deg, t.lat_deg) = pair(value)?,
                "height" => t.height_m = number(value)?,
                "start" => {
                    t.start_utc =
                        parse_utc(value).ok_or_else(|| format!("start: not an instant: {value}"))?
                }
                "days" => t.duration_s = number(value)? * 86_400.0,
                "hours" => t.duration_s = number(value)? * 3_600.0,
                "length" => t.length_s = number(value)?,
                "fps" => t.fps = number(value)? as u32,
                "frame" => {
                    t.frame = match value {
                        "earth" => CameraFrame::Earth,
                        "inertial" => CameraFrame::Inertial,
                        _ => return Err(format!("frame: earth or inertial, not {value}")),
                    }
                }
                "spin" => {
                    t.motion = Motion::Spin {
                        turns: number(value)?,
                    }
                }
                "track" => match value {
                    "sun-moon" => t.motion = Motion::Track,
                    _ => return Err(format!("track: sun-moon, not {value}")),
                },
                "heading" => t.heading_deg = number(value)?,
                "pitch" => t.pitch_deg = number(value)?,
                "swing" => (t.pitch_swing_deg, t.pitch_period_s) = pair(value)?,
                "fov" => t.fovy_deg = number(value)?,
                _ => return Err(format!("{key}: not a time-lapse parameter")),
            }
        }
        if t.fps == 0 || t.length_s <= 0.0 || t.duration_s <= 0.0 {
            return Err("a time-lapse wants fps, length and a duration over zero".into());
        }
        if !(1.0..=170.0).contains(&t.fovy_deg) {
            return Err(format!("fov: {} is not a field of view", t.fovy_deg));
        }
        Ok(t)
    }
}

impl Default for TimeLapse {
    /// Two minutes at sixty frames a second of a day, from a summit of the
    /// Pyrenees (the Pic du Midi de Bigorre, 2 877 m), a kilometre over it,
    /// held to the Earth, looking south a little over the horizon.
    fn default() -> Self {
        Self {
            lon_deg: 0.1423,
            lat_deg: 42.9364,
            height_m: 4_000.0,
            start_utc: 0.0,
            duration_s: 86_400.0,
            length_s: 120.0,
            fps: 60,
            frame: CameraFrame::Earth,
            motion: Motion::Held,
            heading_deg: 180.0,
            pitch_deg: 5.0,
            pitch_swing_deg: 0.0,
            pitch_period_s: 0.0,
            fovy_deg: 60.0,
        }
    }
}

/// Unix seconds of an ISO 8601 UTC instant — `YYYY-MM-DD`, then optionally
/// `THH:MM` and `:SS`, then optionally `Z` — or of a plain number of
/// seconds. `None` for anything else.
pub fn parse_utc(text: &str) -> Option<f64> {
    if let Ok(seconds) = text.parse::<f64>() {
        return seconds.is_finite().then_some(seconds);
    }
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (date, time) = text.split_once('T').unwrap_or((text, "00:00"));
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: f64 = t.next().map_or(Some(0.0), |s| s.parse().ok())?;
    // Days from 1970-01-01, by Howard Hinnant's days-from-civil.
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3_600 + minute * 60) as f64 + second)
}

/// The eye of `camera` as a vector.
pub fn eye(camera: &Frame) -> DVec3 {
    DVec3::from_array(camera.position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::ecef_to_geodetic;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn an_instant_is_read_as_utc() {
        assert_eq!(parse_utc("1970-01-01"), Some(0.0));
        assert_eq!(parse_utc("2026-10-26T04:11Z"), Some(1_792_987_860.0));
        assert_eq!(parse_utc("2026-10-26T04:11:30"), Some(1_792_987_890.0));
        assert_eq!(parse_utc("12.5"), Some(12.5));
        assert_eq!(parse_utc("2026-13-01"), None);
        let clock = Clock::parse("2026-10-19T00:00Z,168").expect("a clock");
        assert_eq!(clock.instant(1), parse_utc("2026-10-19").unwrap());
        assert_eq!(clock.instant(2) - clock.instant(1), 168.0);
        assert_eq!(Clock::parse(&clock.to_arg()), Ok(clock));
    }

    #[test]
    fn fourteen_days_in_two_minutes_at_sixty_a_second() {
        let t = TimeLapse::parse(&words("start=2026-10-19 days=14 length=120 fps=60")).unwrap();
        assert_eq!(t.frames(), 7200);
        assert_eq!(t.clock().seconds_per_frame, 168.0);
        // The last frame shows the last 168 s of the fourteenth day.
        assert_eq!(
            t.clock().instant(7200) + 168.0 - t.start_utc,
            14.0 * 86_400.0
        );
    }

    #[test]
    fn held_to_the_earth_the_camera_stays_over_its_place_and_turns_on_itself() {
        let t = TimeLapse::parse(&words(
            "place=0.14,42.94 height=4000 days=14 spin=10 pitch=8 swing=10,30 fov=70",
        ))
        .unwrap();
        let first = t.camera(1);
        for frame in [1, 360, 361, 4000, 7200] {
            let c = t.camera(frame);
            let g = ecef_to_geodetic(eye(&c));
            assert!((g.height - 4000.0).abs() < 1e-3);
            assert!((g.lon.to_degrees() - 0.14).abs() < 1e-9);
            assert_eq!(c.position, first.position);
            // Upright: the camera's up in the vertical plane of its aim.
            let up = enu_frame(g).z_axis;
            let d = DVec3::from_array(c.direction);
            let u = DVec3::from_array(c.up);
            assert!(u.dot(d).abs() < 1e-12 && u.dot(up) > 0.0);
            assert!(d.cross(up).dot(u).abs() < 1e-12);
        }
        // Ten turns over 7 200 frames: one in 720, a half turn at 361.
        let heading = |frame: u32| {
            let g = ecef_to_geodetic(eye(&t.camera(frame)));
            let enu = enu_frame(g);
            let d = DVec3::from_array(t.camera(frame).direction);
            d.dot(enu.x_axis).atan2(d.dot(enu.y_axis)).to_degrees()
        };
        assert!((heading(1) - 180.0).abs() < 1e-9);
        assert!(heading(361).abs() < 1e-6, "{}", heading(361));
    }

    #[test]
    fn held_to_the_stars_the_earth_turns_beneath_once_a_sidereal_day() {
        let t = TimeLapse::parse(&words(
            "height=70000 days=7 length=120 fps=60 frame=inertial",
        ))
        .unwrap();
        let a = eye(&t.camera(1));
        let b = eye(&t.camera(2));
        // 84 s of sky a frame: the camera moves west over the ground by the
        // Earth's turn in that time.
        let dlon = ecef_to_geodetic(b).lon - ecef_to_geodetic(a).lon;
        assert!((dlon + EARTH_ROTATION * 84.0).abs() < 1e-12, "{dlon}");
        assert!((ecef_to_geodetic(b).height - 70_000.0).abs() < 1e-3);
        // A sidereal day later, the same place over the Earth.
        let sidereal = std::f64::consts::TAU / EARTH_ROTATION;
        let frames = (sidereal / t.clock().seconds_per_frame).round() as u32;
        let back = eye(&t.camera(1 + frames));
        assert!(back.distance(a) < 0.02 * sidereal / 84.0 * 7.0e3);
    }
}
