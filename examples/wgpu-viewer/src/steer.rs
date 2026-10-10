// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Steering: everything that moves or changes the view without being a
//! gesture, as one vocabulary.
//!
//! [`Command`] is that vocabulary. A key press, a steering URL from another
//! program and (for the start view) the command line all end in the same
//! values, checked by the same table of bounds in [`crate::start`], and applied
//! by the same code — so a place reached by a script is exactly the place
//! reached by a flag.
//!
//! # Steering from outside
//!
//! A running viewer takes commands as URLs of the scheme [`SCHEME`]. On macOS
//! the application bundle registers the scheme, so the system delivers them:
//!
//! ```text
//! open "tuile://goto?lon=-2.86&lat=52.51&altitude=1200&heading=0&pitch=30"
//! osascript -e 'tell application "Tuile" to open location "tuile://north"'
//! ```
//!
//! | URL | effect |
//! |---|---|
//! | `tuile://goto?lon=&lat=&altitude=&heading=&pitch=` | puts the eye there; what is left out keeps its present value |
//! | `tuile://north` | north up, about the point at the centre of the view |
//! | `tuile://here` | centres on the current location (the system still asks the person) |
//! | `tuile://view?freeze=on\|off&wireframe=on\|off` | the two display switches |
//!
//! **What a URL can do is all in that table.** It moves the camera and flips
//! two display switches. It reads no file, runs nothing, and carries no
//! credential; anything else — an unknown command, an unknown parameter, a
//! value out of the flags' bounds, a URL longer than [`LONGEST_URL`] — is
//! dropped whole with one line in the log, and the view does not move.
//!
//! The moves themselves are written against the controller's public gestures
//! and constructors. Nothing here reaches into the camera crate, so nothing
//! here can disagree with what a drag or a wheel does to the same state.

use std::f64::consts::{PI, TAU};

use tuile_camera::{CameraController, GlobeCamera};
use tuile_core::geo::ecef_to_geodetic;

use crate::start::StartView;

/// The URL scheme a running viewer is steered by. The application bundle's
/// `Info.plist` declares the same word; a test in the bundler holds them equal.
pub(crate) const SCHEME: &str = "tuile";

/// No steering URL is anywhere near this long; one that is, is not one.
pub(crate) const LONGEST_URL: usize = 512;

/// Where to put the eye. What is `None` keeps its present value, so
/// `goto?heading=90` turns the view east where it stands and
/// `goto?lon=…&lat=…` travels without changing how the eye looks.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Goto {
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub altitude: Option<f64>,
    pub heading: Option<f64>,
    pub pitch: Option<f64>,
}

/// One thing the view can be told to do, whoever is telling it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Command {
    Goto(Goto),
    NorthUp,
    /// Centre on the current location.
    Here,
    Freeze(bool),
    Wireframe(bool),
}

/// What reaches the render loop from outside it: a URL still to be read, or a
/// command a script already spelt out.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    not(all(target_os = "macos", feature = "application")),
    allow(dead_code)
)]
pub(crate) enum Request {
    Url(String),
    Do(Command),
}

impl Goto {
    /// A goto from numbers a script handed over, held to the flags' bounds
    /// like everything else: `(name, value)` pairs, names as in the URLs.
    #[cfg_attr(
        not(all(target_os = "macos", feature = "application")),
        allow(dead_code)
    )]
    pub(crate) fn from_numbers(pairs: &[(&str, f64)]) -> Result<Self, String> {
        let mut goto = Self::default();
        for (name, value) in pairs {
            let value = crate::start::checked(name, *value)?;
            match *name {
                "lon" => goto.lon = Some(value),
                "lat" => goto.lat = Some(value),
                "altitude" => goto.altitude = Some(value),
                "heading" => goto.heading = Some(value),
                _ => goto.pitch = Some(value),
            }
        }
        if goto == Self::default() {
            return Err("go to says nowhere: give at least one parameter".to_owned());
        }
        Ok(goto)
    }
}

/// Reads a steering URL into the commands it carries — several, for
/// `view?freeze=on&wireframe=on` — or says why it carries none.
///
/// All or nothing: one bad parameter refuses the whole URL, because half of a
/// `goto` is a different place from the one that was meant.
pub(crate) fn parse_url(url: &str) -> Result<Vec<Command>, String> {
    if url.len() > LONGEST_URL {
        return Err(format!("a steering URL of {} bytes is not one", url.len()));
    }
    let (scheme, rest) = url
        .trim()
        .split_once(':')
        .ok_or_else(|| "not a URL".to_owned())?;
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return Err(format!("the scheme is {SCHEME:?}, not {scheme:?}"));
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
    let name = name.trim_end_matches('/').to_ascii_lowercase();
    let mut pairs = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("{pair:?} is not `name=value`"))?;
        if pairs.iter().any(|(seen, _)| *seen == key) {
            return Err(format!("{key:?} given twice"));
        }
        pairs.push((key, value));
    }
    let bare = |command: Command| {
        if pairs.is_empty() {
            Ok(vec![command])
        } else {
            Err(format!("{name:?} takes no parameter"))
        }
    };
    match name.as_str() {
        "goto" => {
            let mut goto = Goto::default();
            for (key, text) in &pairs {
                let value = crate::start::value(key, text)?;
                match *key {
                    "lon" => goto.lon = Some(value),
                    "lat" => goto.lat = Some(value),
                    "altitude" => goto.altitude = Some(value),
                    "heading" => goto.heading = Some(value),
                    // `value` refused every name that is not one of the five.
                    _ => goto.pitch = Some(value),
                }
            }
            if goto == Goto::default() {
                return Err("goto says nowhere: give at least one of lon, lat, \
                            altitude, heading, pitch"
                    .to_owned());
            }
            Ok(vec![Command::Goto(goto)])
        }
        "north" => bare(Command::NorthUp),
        "here" => bare(Command::Here),
        "view" => {
            if pairs.is_empty() {
                return Err("view changes nothing: give freeze= or wireframe=".to_owned());
            }
            pairs
                .iter()
                .map(|(key, text)| {
                    let on = match *text {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("{key} is on or off, not {text:?}")),
                    };
                    match *key {
                        "freeze" => Ok(Command::Freeze(on)),
                        "wireframe" => Ok(Command::Wireframe(on)),
                        _ => Err(format!("unknown switch {key:?}")),
                    }
                })
                .collect()
        }
        _ => Err(format!("unknown command {name:?}")),
    }
}

/// A camera as the five numbers the flags and the URLs speak in.
pub(crate) fn view_of(camera: &GlobeCamera) -> StartView {
    let eye = ecef_to_geodetic(camera.position);
    StartView {
        lon: eye.lon.to_degrees(),
        lat: eye.lat.to_degrees(),
        altitude: eye.height,
        heading: tidy_heading(camera.heading().to_degrees()),
        pitch: camera.pitch().to_degrees(),
    }
}

/// North is 0, not 1.5e-14 and not 359.99999999: a heading within a billionth
/// of a degree of a full turn is the rounding of a view looking north, and a
/// script comparing it to zero should find zero.
fn tidy_heading(degrees: f64) -> f64 {
    if !(1e-9..=360.0 - 1e-9).contains(&degrees) {
        0.0
    } else {
        degrees
    }
}

/// Puts the eye where `goto` says, keeping the present value of whatever it
/// leaves out. A jump, not a glide: see [`jump`].
pub(crate) fn go(controller: &mut CameraController, goto: Goto) {
    let now = view_of(controller.target());
    let view = StartView {
        lon: goto.lon.unwrap_or(now.lon),
        lat: goto.lat.unwrap_or(now.lat),
        altitude: goto.altitude.unwrap_or(now.altitude),
        heading: goto.heading.unwrap_or(now.heading),
        pitch: goto.pitch.unwrap_or(now.pitch),
    };
    jump(controller, view.camera());
}

/// The URL that brings a viewer back to this view: a bookmark, and the
/// `view url` a script reads.
///
/// Seven decimals of a degree is a centimetre on the ground, a centimetre of
/// altitude and a thousandth of a degree of attitude are below anything a
/// screen shows; more digits would only make the link longer.
pub(crate) fn link(view: &StartView) -> String {
    format!(
        "{SCHEME}://goto?lon={:.7}&lat={:.7}&altitude={:.2}&heading={:.3}&pitch={:.3}",
        view.lon,
        view.lat,
        view.altitude,
        // A heading a hair under a full turn prints as 360.000, which is out
        // of nobody's bounds but reads oddly; it is north.
        if view.heading > 359.9995 {
            0.0
        } else {
            view.heading
        },
        view.pitch
    )
}

/// Turns the view north-up about the ground point at the centre of the screen.
///
/// That point, its distance from the eye and the tilt all stay as they are —
/// the picture pivots, it does not travel. It is the same rotation as dragging
/// the compass ring, by exactly the heading there is to lose, the short way
/// round.
///
/// Done in a few passes rather than one. The gesture turns about the vertical
/// *at the centre of the screen*, the heading is read against north *at the
/// eye*, and on a globe those two norths are not parallel: seen from high up
/// and tilted, one turn by the heading leaves a residue of a few degrees (the
/// convergence of the meridians between the two points). Each pass removes the
/// heading that is left, and the residue shrinks geometrically: a handful of
/// passes near the ground, a few dozen from orbit with the horizon in frame.
/// The loop stops when there is nothing left to remove, and is bounded so that
/// a view it cannot converge on — there is none known — costs microseconds,
/// not a hang.
///
/// Acts on the gesture target, like every gesture: the eye eases there over
/// the controller's usual settle time, so the turn is a short swing and not a
/// cut.
pub(crate) fn north_up(controller: &mut CameraController, viewport: (f64, f64)) {
    for _ in 0..64 {
        let heading = controller.target().heading();
        // Signed, in (-π, π]: 350° is ten degrees to undo, not three hundred
        // and fifty.
        let remaining = if heading > PI { heading - TAU } else { heading };
        if remaining.abs() < 1e-13 {
            break;
        }
        // The gesture turns counter-clockwise for a positive angle and
        // headings count clockwise, so turning *by* the heading removes it.
        controller.rotate_heading(remaining, viewport);
    }
}

/// Puts the eye at `camera` outright: no easing, nothing kept of where it was.
///
/// A jump across a country is not a gesture, and easing it would drag the eye
/// through the planet in a straight line. The controller is rebuilt rather
/// than patched because its gesture target is private — and that is right: a
/// target left behind would pull the eye straight back. The floor and the
/// relief it is measured against carry over.
pub(crate) fn jump(controller: &mut CameraController, camera: GlobeCamera) {
    let mut moved = CameraController::new(camera).with_min_altitude(controller.min_altitude);
    if let Some(ground) = controller.ground() {
        moved = moved.with_ground(ground.clone());
    }
    *controller = moved;
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: (f64, f64) = (1280.0, 800.0);
    const CENTRE: (f64, f64) = (640.0, 400.0);

    fn controller(view: StartView) -> CameraController {
        CameraController::new(view.camera())
    }

    /// What north-up must leave alone, read off the settled camera.
    struct Framing {
        centre: glam::DVec3,
        range: f64,
        pitch: f64,
    }

    fn framing(controller: &mut CameraController) -> Framing {
        controller.update(1.0);
        let centre = controller
            .pick(CENTRE, VIEWPORT)
            .expect("the view is aimed at the ground");
        Framing {
            centre,
            range: (controller.camera.position - centre).length(),
            pitch: controller.camera.pitch(),
        }
    }

    /// From every quarter, low and tilted, high and tilted, and straight down:
    /// afterwards north is up, and the framing has not moved.
    ///
    /// Tolerances. Heading: 1e-9°. Centre and range: a millimetre — they are
    /// exactly preserved by a rotation about an axis through the centre, so
    /// this is rounding. Pitch: half a degree, and that one is not rounding.
    /// The controller's heading gesture turns about the *geocentric* vertical
    /// of the pivot, pitch is read against the *geodetic* vertical at the eye,
    /// and at mid latitudes those differ by a fifth of a degree: a half turn
    /// shows up to twice that as tilt (0.32° measured at 52° north). It is the
    /// same drift as dragging the compass ring round by hand, since this is
    /// that gesture; the bound is here so that it cannot quietly grow.
    #[test]
    fn north_up_zeroes_the_heading_and_keeps_the_centre_the_range_and_the_tilt() {
        for (heading, pitch, altitude) in [
            (135.0, 30.0, 1200.0),
            (200.0, 30.0, 1200.0),
            (-90.0, 55.0, 40_000.0),
            (359.0, 20.0, 300_000.0),
            (77.0, 90.0, 2_000_000.0),
            (180.0, 45.0, 5000.0),
        ] {
            let mut c = controller(StartView {
                lon: -2.86,
                lat: 52.51,
                altitude,
                heading,
                pitch,
            });
            let before = framing(&mut c);
            north_up(&mut c, VIEWPORT);
            let after = framing(&mut c);

            let case = format!("heading {heading}, pitch {pitch}, altitude {altitude}");
            let heading_left = (c.camera.heading().to_degrees() + 180.0).rem_euclid(360.0) - 180.0;
            assert!(heading_left.abs() < 1e-9, "{case}: {heading_left}° left");
            assert!(
                (after.centre - before.centre).length() < 1e-3,
                "{case}: the centre moved {} m",
                (after.centre - before.centre).length()
            );
            assert!(
                (after.range - before.range).abs() < 1e-3,
                "{case}: the range changed by {} m",
                after.range - before.range
            );
            assert!(
                (after.pitch - before.pitch).to_degrees().abs() < 0.5,
                "{case}: the pitch changed by {}°",
                (after.pitch - before.pitch).to_degrees()
            );
        }
    }

    #[test]
    fn north_up_on_a_view_already_north_up_changes_nothing() {
        let mut c = controller(StartView::default());
        let before = *c.target();
        north_up(&mut c, VIEWPORT);
        assert_eq!(c.target().position, before.position);
        assert_eq!(c.target().direction, before.direction);
        assert_eq!(c.target().up, before.up);
    }

    /// A jump leaves nothing behind to ease back to, and keeps the floor.
    #[test]
    fn a_jump_moves_the_eye_and_its_target_together_and_keeps_the_floor() {
        let mut c = controller(StartView::default()).with_min_altitude(7.0);
        c.zoom(3.0, CENTRE, VIEWPORT);
        let there = StartView {
            lon: 10.0,
            lat: -20.0,
            altitude: 9000.0,
            heading: 40.0,
            pitch: 60.0,
        }
        .camera();
        jump(&mut c, there);
        assert_eq!(c.camera.position, there.position);
        assert_eq!(c.target().position, there.position);
        assert_eq!(c.target().direction, there.direction);
        assert_eq!(c.min_altitude, 7.0);
        // And it stays there: easing has nowhere else to go.
        c.update(1.0);
        assert!((c.camera.position - there.position).length() < 1e-6);
    }

    fn one(url: &str) -> Command {
        let parsed = parse_url(url);
        let Ok([command]) = parsed.as_deref() else {
            unreachable!("{url:?} should carry one command, got {parsed:?}");
        };
        *command
    }

    #[test]
    fn a_full_goto_reads_every_parameter() {
        assert_eq!(
            one("tuile://goto?lon=-2.86&lat=52.51&altitude=1200&heading=0&pitch=30"),
            Command::Goto(Goto {
                lon: Some(-2.86),
                lat: Some(52.51),
                altitude: Some(1200.0),
                heading: Some(0.0),
                pitch: Some(30.0),
            })
        );
    }

    #[test]
    fn a_partial_goto_names_only_what_it_changes() {
        assert_eq!(
            one("TUILE:goto?heading=90"),
            Command::Goto(Goto {
                heading: Some(90.0),
                ..Goto::default()
            })
        );
    }

    #[test]
    fn the_bare_commands_and_the_switches_read() {
        assert_eq!(one("tuile://north"), Command::NorthUp);
        assert_eq!(one("tuile://north/"), Command::NorthUp);
        assert_eq!(one("tuile://here"), Command::Here);
        assert_eq!(
            parse_url("tuile://view?freeze=on&wireframe=off"),
            Ok(vec![Command::Freeze(true), Command::Wireframe(false)])
        );
    }

    /// Everything that is not a command is refused whole — and with the same
    /// bounds as the flags, because it is the same table.
    #[test]
    fn what_is_malformed_unknown_or_out_of_range_carries_no_command() {
        // A perfectly good longitude, written with six hundred zeros: only
        // its length is wrong with it.
        let long = format!("tuile://goto?lon={}1", "0".repeat(LONGEST_URL + 100));
        for url in [
            "",
            "goto?lon=1",
            "https://goto?lon=1",
            "tuile://",
            "tuile://launch",
            // Was a command once; a script reads the view from the
            // application's properties now, and no file is written.
            "tuile://report",
            "tuile://goto",
            "tuile://goto?",
            "tuile://goto?lon",
            "tuile://goto?lon=",
            "tuile://goto?lon=west",
            "tuile://goto?lon=nan",
            "tuile://goto?lon=181",
            "tuile://goto?lat=-90.5",
            "tuile://goto?altitude=0",
            "tuile://goto?altitude=1e12",
            "tuile://goto?pitch=120",
            "tuile://goto?lon=1&lon=2",
            "tuile://goto?lon=1&zoom=3",
            // One good parameter does not carry a bad one through.
            "tuile://goto?lon=1&lat=500",
            "tuile://goto?file=/etc/passwd",
            "tuile://north?now=1",
            "tuile://view",
            "tuile://view?freeze=maybe",
            "tuile://view?shell=green",
            long.as_str(),
        ] {
            assert!(parse_url(url).is_err(), "{url:?} was taken for a command");
        }
    }

    /// A partial goto changes what it names and nothing else. Tolerances as
    /// for the flags: 1e-9° of position, a millimetre, and 1e-6° of attitude
    /// (heading and pitch are read back off the camera and put in again).
    #[test]
    fn a_goto_changes_what_it_names_and_keeps_the_rest() {
        let start = StartView {
            lon: 6.86,
            lat: 45.83,
            altitude: 6000.0,
            heading: 120.0,
            pitch: 25.0,
        };
        let mut c = controller(start);
        go(
            &mut c,
            Goto {
                lon: Some(-2.86),
                lat: Some(52.51),
                ..Goto::default()
            },
        );
        let there = view_of(&c.camera);
        assert!((there.lon - -2.86).abs() < 1e-9 && (there.lat - 52.51).abs() < 1e-9);
        assert!((there.altitude - 6000.0).abs() < 1e-3, "{there:?}");
        assert!((there.heading - 120.0).abs() < 1e-6, "{there:?}");
        assert!((there.pitch - 25.0).abs() < 1e-6, "{there:?}");

        go(
            &mut c,
            Goto {
                altitude: Some(900.0),
                pitch: Some(60.0),
                ..Goto::default()
            },
        );
        let turned = view_of(&c.camera);
        assert!((turned.lon - -2.86).abs() < 1e-9 && (turned.lat - 52.51).abs() < 1e-9);
        assert!((turned.altitude - 900.0).abs() < 1e-3, "{turned:?}");
        assert!((turned.heading - 120.0).abs() < 1e-6, "{turned:?}");
        assert!((turned.pitch - 60.0).abs() < 1e-6, "{turned:?}");
    }

    #[test]
    fn a_view_looking_north_reads_a_heading_of_exactly_zero() {
        // Straight down, where the heading comes off the camera's own up and
        // carries the last bit of a sine.
        assert_eq!(view_of(&StartView::default().camera()).heading, 0.0);
        assert_eq!(tidy_heading(359.999_999_999_9), 0.0);
        // What an eased camera actually reads when it has settled on north.
        assert_eq!(tidy_heading(1.47e-14), 0.0);
        assert_eq!(tidy_heading(359.99), 359.99);
        assert_eq!(tidy_heading(0.01), 0.01);
    }

    /// The link a view prints is a command that returns to it: to a
    /// centimetre, and a thousandth of a degree of attitude.
    #[test]
    fn the_link_to_a_view_leads_back_to_it() {
        for view in [
            StartView::default(),
            StartView {
                lon: -2.8612345,
                lat: 52.5198765,
                altitude: 1234.56,
                heading: 359.99999,
                pitch: 31.25,
            },
        ] {
            let Command::Goto(goto) = one(&link(&view)) else {
                unreachable!("a link is a goto");
            };
            let mut c = controller(StartView {
                lon: 100.0,
                lat: -10.0,
                altitude: 50_000.0,
                heading: 200.0,
                pitch: 45.0,
            });
            go(&mut c, goto);
            let back = view_of(&c.camera);
            assert!((back.lon - view.lon).abs() < 1e-6, "{back:?}");
            assert!((back.lat - view.lat).abs() < 1e-6, "{back:?}");
            assert!((back.altitude - view.altitude).abs() < 1e-2, "{back:?}");
            let turn = (back.heading - view.heading + 180.0).rem_euclid(360.0) - 180.0;
            assert!(turn.abs() < 1e-3, "{back:?}");
            assert!((back.pitch - view.pitch).abs() < 1e-3, "{back:?}");
        }
    }

    #[test]
    fn a_goto_from_numbers_is_held_to_the_same_bounds() {
        assert_eq!(
            Goto::from_numbers(&[("lat", 5.36), ("lon", -4.0)]),
            Ok(Goto {
                lon: Some(-4.0),
                lat: Some(5.36),
                ..Goto::default()
            })
        );
        for bad in [
            &[("lat", 95.0)][..],
            &[("lon", f64::NAN)],
            &[("altitude", 0.0)],
            &[("lon", 1.0), ("pitch", 91.0)],
            &[("zoom", 3.0)],
            &[],
        ] {
            assert!(Goto::from_numbers(bad).is_err(), "{bad:?}");
        }
    }
}
