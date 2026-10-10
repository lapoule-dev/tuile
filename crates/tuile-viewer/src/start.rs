// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the session starts: the command line's flags, and the camera they
//! describe.
//!
//! These are flags where everything in [`crate::settings`] is an environment
//! variable, and the split is deliberate. A variable is a knob one turns
//! *between two runs of the same command* while comparing; where the eye starts
//! is not a knob, it is the subject of the run — "show me this valley" — and it
//! belongs in the command that says so, where a typo is an error rather than a
//! silently ignored variable.
//!
//! Parsed by hand: five flags that each take one number, and one that takes
//! none, do not earn a dependency, and the whole grammar fits in [`parse`].

use tuile_camera::GlobeCamera;

/// What `--help` prints. The keys are listed with the flags because this is the
/// only text the binary can show before a window exists.
const USAGE: &str = "\
FLAGS_HEADER

FLAGS (each takes one number; `--flag value` or `--flag=value`):
    --lon <deg>        longitude of the eye, east positive   [-180, 180]   default 2
    --lat <deg>        latitude of the eye, north positive   [-90, 90]     default 46
    --altitude <m>     height of the eye above the ellipsoid [1, 1e8]      default 2000000
    --heading <deg>    view direction, 0 = north, clockwise  [-360, 360]   default 0
    --pitch <deg>      angle below the horizon, 90 = down    [0, 90]       default 90
    --here             start over the current location instead of --lon/--lat
                       (asks the system's location service; the view moves
                       when the answer arrives, and stays put if none does)
    --imagery <name>   the imagery layer to open on, by key or by name
IMAGERY_LAYERS
    -h, --help         print this and exit

KEYS:
    drag               move the globe under the cursor
    right-drag         tilt (vertical) and turn (horizontal)
    wheel              zoom toward the cursor
    N                  north up: turn the view about the point at its centre
                       (a click on the compass ring does the same)
    L                  centre on the current location, at the present altitude
    C                  copy a link to this view (a tuile://goto?… URL)
    I                  the next imagery layer
    W                  wireframe
    D                  cycle the diagnostic views
    F                  freeze the traversal
    Esc                quit

Everything else a session is started with is an environment variable; see the
header of `settings.rs`. A running viewer is steered with URLs of its own
scheme (see the header of `steer.rs`) and, as the macOS application, by
scripts (see `macos/README.md`).";

/// What `--help` prints for this host: its executable's name, where its
/// credentials are read from, then the flags and the keys.
pub(crate) fn usage() -> String {
    let identity = crate::embed::identity();
    let mut head = format!(
        "{exe} — a window on the streaming geometry server\n\nUSAGE:\n    {exe} [FLAGS]\n",
        exe = identity.executable
    );
    if !identity.credentials.is_empty() {
        head.push_str(&format!("\n{}\n", identity.credentials));
    }
    let layers: Vec<String> = crate::embed::layers()
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            format!(
                "                         {:<12} {}{}",
                layer.key,
                layer.name,
                if index == 0 { "   (default)" } else { "" }
            )
        })
        .collect();
    USAGE
        .replace("IMAGERY_LAYERS\n", &format!("{}\n", layers.join("\n")))
        .replace("FLAGS_HEADER\n", &head)
        .replace("tuile://", &format!("{}://", identity.scheme))
}

/// The view a session opens on, in the units a person types: degrees and
/// metres. Everything is `f64` — a longitude in `f32` is already metres off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StartView {
    /// Longitude of the eye, degrees, east positive.
    pub lon: f64,
    /// Latitude of the eye, degrees, north positive.
    pub lat: f64,
    /// Height of the eye above the ellipsoid, metres. Not above the ground: the
    /// relief is not known until tiles land, and the controller's floor lifts an
    /// eye that turns out to be inside a mountain.
    pub altitude: f64,
    /// Compass heading of the view, degrees clockwise from north.
    pub heading: f64,
    /// Angle of the view below the horizon, degrees; 90 looks straight down.
    pub pitch: f64,
}

impl Default for StartView {
    /// Straight down at France from 2 000 km — what the viewer opened on when
    /// the view was written out in `main`, and still what it opens on when no
    /// flag is given. A test holds the two equal, bit for bit.
    fn default() -> Self {
        Self {
            lon: 2.0,
            lat: 46.0,
            altitude: 2_000_000.0,
            heading: 0.0,
            pitch: 90.0,
        }
    }
}

impl StartView {
    /// The camera this view describes.
    pub(crate) fn camera(&self) -> GlobeCamera {
        GlobeCamera::from_geodetic(
            self.lat.to_radians(),
            self.lon.to_radians(),
            self.altitude,
            self.heading.to_radians(),
            self.pitch.to_radians(),
            tuile_camera::DEFAULT_GLOBE_FOVY,
        )
    }
}

/// The view the last session started from an icon ended on, if it left one
/// and it still reads as a view. Anything else — no file, an edited file, a
/// value out of bounds — is no view, and the default stands.
pub(crate) fn remembered(host: &dyn crate::host::Host) -> Option<StartView> {
    let file = crate::host::last_view_file(host)?;
    let text = host.read(&file).ok()?;
    match crate::steer::parse_url(text.trim()).ok()?.as_slice() {
        [crate::steer::Command::Goto(goto)] => Some(StartView {
            lon: goto.lon?,
            lat: goto.lat?,
            altitude: goto.altitude?,
            heading: goto.heading?,
            pitch: goto.pitch?,
        }),
        _ => None,
    }
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Invocation {
    /// Open the window on this view — and, with `here`, move it over the
    /// current location once the system says where that is. The view's own
    /// longitude and latitude are then only where the eye waits meanwhile.
    ///
    /// `imagery` is the layer `--imagery` named, as typed: which layer that
    /// is depends on the host's list, which this parser does not have.
    Run {
        view: StartView,
        here: bool,
        imagery: Option<String>,
    },
    /// Print [`usage`] and leave.
    Help,
}

/// One flag: its name, the range it accepts (inclusive at both ends) and the
/// field it fills. A table rather than five `match` arms so the bounds a value
/// is checked against cannot drift apart from one flag to the next.
struct Flag {
    name: &'static str,
    unit: &'static str,
    min: f64,
    max: f64,
    field: fn(&mut StartView) -> &mut f64,
}

/// The bounds, each for a reason:
///
/// - longitude and latitude are the ranges the angles have; a latitude of 95°
///   is a typo, not a point over the pole;
/// - altitude starts at one metre, the controller's own floor, and stops at
///   10⁸ m — a quarter of the way to the Moon, far past where the planet is a
///   dot. Anything larger is a misplaced decimal or a value in the wrong unit;
/// - heading accepts a full turn either way, so both `-90` and `270` say west;
/// - pitch runs from level to straight down. Above the horizon there is only
///   sky, and past the nadir the view is upside down. A pitch flatter than the
///   horizon's dip at that altitude is accepted and is the controller's to
///   bring back into its usable band on the first gesture.
const FLAGS: [Flag; 5] = [
    Flag {
        name: "--lon",
        unit: "degrees",
        min: -180.0,
        max: 180.0,
        field: |v| &mut v.lon,
    },
    Flag {
        name: "--lat",
        unit: "degrees",
        min: -90.0,
        max: 90.0,
        field: |v| &mut v.lat,
    },
    Flag {
        name: "--altitude",
        unit: "metres",
        min: 1.0,
        max: 1.0e8,
        field: |v| &mut v.altitude,
    },
    Flag {
        name: "--heading",
        unit: "degrees",
        min: -360.0,
        max: 360.0,
        field: |v| &mut v.heading,
    },
    Flag {
        name: "--pitch",
        unit: "degrees",
        min: 0.0,
        max: 90.0,
        field: |v| &mut v.pitch,
    },
];

impl Flag {
    /// One value for this flag: a finite number inside the bounds, or the
    /// sentence that says what was wrong with it.
    fn read(&self, text: &str) -> Result<f64, String> {
        // `f64::from_str` reads "nan" and "inf" happily, and a NaN passes no
        // range check by failing every comparison — so finiteness is asked for
        // by name rather than left to the bounds.
        let value = text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| {
                format!(
                    "{} wants a number of {}, got {text:?}",
                    self.name, self.unit
                )
            })?;
        self.check(value)
    }

    /// The bounds alone, for a value that arrives as a number already — from a
    /// script, which speaks in reals and not in text.
    fn check(&self, value: f64) -> Result<f64, String> {
        if !value.is_finite() || value < self.min || value > self.max {
            return Err(format!(
                "{} must be between {} and {} {}, got {value}",
                self.name, self.min, self.max, self.unit
            ));
        }
        Ok(value)
    }
}

fn flag(name: &str) -> Result<&'static Flag, String> {
    FLAGS
        .iter()
        .find(|f| f.name.strip_prefix("--") == Some(name))
        .ok_or_else(|| format!("unknown parameter {name:?}"))
}

/// A number held to the bounds of the flag called `name` (without its dashes).
#[cfg_attr(
    not(all(target_os = "macos", feature = "application")),
    allow(dead_code)
)]
pub(crate) fn checked(name: &str, value: f64) -> Result<f64, String> {
    flag(name)?.check(value)
}

/// One value, checked exactly as the command line checks it: `name` is a
/// flag's name without its dashes. This is how a steering URL gets the same
/// vocabulary and the same bounds as the flags — there is one table, and both
/// read it.
pub(crate) fn value(name: &str, text: &str) -> Result<f64, String> {
    flag(name)?.read(text)
}

/// Reads the arguments after the program name.
///
/// Strict on purpose. An unknown flag, a missing or unreadable value, a value
/// out of range and a flag given twice are all errors, because the alternative
/// to each is a window that opens somewhere other than where it was asked to
/// and says nothing about it. The message names the flag and what it wanted.
///
/// A value is whatever follows its flag, taken unconditionally — `--lon -2.86`
/// is a western longitude, not a flag called `-2.86`.
pub(crate) fn parse<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut view = StartView::default();
    let mut here = false;
    let mut imagery: Option<String> = None;
    let mut seen = [false; FLAGS.len()];
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg.as_ref();
        if arg == "--help" || arg == "-h" {
            return Ok(Invocation::Help);
        }
        if arg == "--here" {
            here = true;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg, None),
        };
        if name == "--imagery" {
            if imagery.is_some() {
                return Err("--imagery given twice".to_owned());
            }
            let text = match inline {
                Some(text) => text,
                None => args
                    .next()
                    .map(|s| s.as_ref().to_owned())
                    .ok_or_else(|| "--imagery needs the name of a layer".to_owned())?,
            };
            if text.trim().is_empty() {
                return Err("--imagery needs the name of a layer".to_owned());
            }
            imagery = Some(text);
            continue;
        }
        let Some(index) = FLAGS.iter().position(|f| f.name == name) else {
            return Err(format!("unknown argument {arg:?}"));
        };
        let flag = &FLAGS[index];
        if std::mem::replace(&mut seen[index], true) {
            return Err(format!("{} given twice", flag.name));
        }
        let text = match inline {
            Some(text) => text,
            None => args
                .next()
                .map(|s| s.as_ref().to_owned())
                .ok_or_else(|| format!("{} needs a value in {}", flag.name, flag.unit))?,
        };
        let value = flag.read(&text)?;
        *(flag.field)(&mut view) = value;
    }
    // Two answers to "where" is one too many, and picking either silently
    // would be picking for the person. `FLAGS` opens with the two positions.
    if here && (seen[0] || seen[1]) {
        return Err("--here and --lon/--lat both say where to start; give one".to_owned());
    }
    Ok(Invocation::Run {
        view,
        here,
        imagery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::ecef_to_geodetic;

    fn run(args: &[&str]) -> StartView {
        let parsed = parse(args);
        let Ok(Invocation::Run {
            view,
            here: false,
            imagery: None,
        }) = parsed
        else {
            unreachable!("{args:?} should start a session, got {parsed:?}");
        };
        view
    }

    /// The contract with every command line written before the flags existed:
    /// no flag, same picture. Compared against the call `main` used to make,
    /// written out again here with its literals, and compared exactly — a start
    /// view that is "close" is a different first frame.
    #[test]
    fn no_flag_is_the_view_the_viewer_always_opened_on() {
        let was = GlobeCamera::from_geodetic(
            46f64.to_radians(),
            2f64.to_radians(),
            2_000_000.0,
            0.0,
            std::f64::consts::FRAC_PI_2,
            tuile_camera::DEFAULT_GLOBE_FOVY,
        );
        let now = run(&[]).camera();
        assert_eq!(now.position, was.position);
        assert_eq!(now.direction, was.direction);
        assert_eq!(now.up, was.up);
        assert_eq!(now.fovy, was.fovy);
    }

    #[test]
    fn every_flag_lands_in_its_own_field() {
        let view = run(&[
            "--lon",
            "-2.86",
            "--lat",
            "52.51",
            "--altitude",
            "1200",
            "--heading",
            "45",
            "--pitch",
            "30",
        ]);
        assert_eq!(
            view,
            StartView {
                lon: -2.86,
                lat: 52.51,
                altitude: 1200.0,
                heading: 45.0,
                pitch: 30.0,
            }
        );
    }

    #[test]
    fn a_flag_left_out_keeps_its_default_and_both_spellings_read() {
        let view = run(&["--lat=-33.9", "--altitude", "5e3"]);
        assert_eq!(
            view,
            StartView {
                lat: -33.9,
                altitude: 5000.0,
                ..StartView::default()
            }
        );
    }

    #[test]
    fn the_bounds_themselves_are_accepted() {
        for flag in &FLAGS {
            for bound in [flag.min, flag.max] {
                let text = bound.to_string();
                assert!(
                    parse([flag.name, text.as_str()]).is_ok(),
                    "{} {bound} is on the bound and must pass",
                    flag.name
                );
            }
        }
    }

    #[test]
    fn a_value_past_either_bound_is_refused_and_the_flag_is_named() {
        for (flag, value) in [
            ("--lon", "180.5"),
            ("--lon", "-181"),
            ("--lat", "90.01"),
            ("--lat", "-91"),
            ("--altitude", "0.5"),
            ("--altitude", "-1200"),
            ("--altitude", "2e8"),
            ("--heading", "361"),
            ("--heading", "-400"),
            ("--pitch", "-1"),
            ("--pitch", "91"),
        ] {
            let error = parse([flag, value]).expect_err(&format!("{flag} {value} is out of range"));
            assert!(error.contains(flag), "{error:?} should name {flag}");
            assert!(
                error.contains("between"),
                "{error:?} should state the range"
            );
        }
    }

    #[test]
    fn what_is_not_a_finite_number_is_refused() {
        for value in ["north", "", "12deg", "nan", "inf", "-inf"] {
            assert!(
                parse(["--lat", value]).is_err(),
                "--lat {value:?} is not a latitude"
            );
        }
    }

    #[test]
    fn an_unknown_flag_a_missing_value_and_a_repeat_are_errors() {
        assert!(parse(["--longitude", "2"])
            .expect_err("no such flag")
            .contains("unknown"));
        // A bare path used to be ignored; it is now said to be unknown rather
        // than quietly dropped.
        assert!(parse(["tileset.json"])
            .expect_err("not a flag")
            .contains("unknown"));
        assert!(parse(["--lon"])
            .expect_err("no value")
            .contains("needs a value"));
        assert!(parse(["--lon", "1", "--lon", "2"])
            .expect_err("a repeat")
            .contains("twice"));
    }

    /// `--imagery` carries a name through untouched — which layer it is, is
    /// the host's list to say — and is held to the same grammar as the rest.
    #[test]
    fn imagery_takes_a_name_in_either_spelling_once() {
        let named = |args: &[&str]| match parse(args) {
            Ok(Invocation::Run { imagery, view, .. }) => (imagery, view),
            other => unreachable!("{args:?} should start a session, got {other:?}"),
        };
        assert_eq!(named(&[]).0, None);
        assert_eq!(named(&["--imagery", "labels"]).0.as_deref(), Some("labels"));
        assert_eq!(
            named(&["--imagery=Aerial with labels"]).0.as_deref(),
            Some("Aerial with labels")
        );
        // Beside the numeric flags, in any order, and without disturbing them.
        let (imagery, view) = named(&["--lat", "10", "--imagery", "satellite", "--lon", "20"]);
        assert_eq!(imagery.as_deref(), Some("satellite"));
        assert_eq!((view.lat, view.lon), (10.0, 20.0));
        for bad in [
            &["--imagery"][..],
            &["--imagery="],
            &["--imagery", "a", "--imagery", "b"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(USAGE.contains("--imagery"));
    }

    #[test]
    fn here_replaces_the_position_and_refuses_to_share_it() {
        assert_eq!(
            parse(["--here", "--altitude", "3000", "--pitch", "40"]),
            Ok(Invocation::Run {
                view: StartView {
                    altitude: 3000.0,
                    pitch: 40.0,
                    ..StartView::default()
                },
                here: true,
                imagery: None,
            })
        );
        // Either order, either coordinate.
        for args in [["--here", "--lon", "2"], ["--lat", "46", "--here"]] {
            let error = parse(args).expect_err("two positions");
            assert!(error.contains("--here"), "{error:?}");
        }
    }

    #[test]
    fn help_wins_wherever_it_stands_and_lists_every_flag() {
        assert_eq!(parse(["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(["--lat", "10", "-h"]), Ok(Invocation::Help));
        for flag in &FLAGS {
            assert!(USAGE.contains(flag.name), "{} is not in --help", flag.name);
        }
    }

    /// The flags say where the eye is and where it looks; this reads both back
    /// off the camera through the camera's own accessors and the core's
    /// geodetic conversion — none of which `StartView::camera` calls.
    ///
    /// Tolerances: 1e-9° on every angle (a tenth of a millimetre on the ground)
    /// and one millimetre on the altitude.
    #[test]
    fn the_camera_reads_back_the_position_and_attitude_asked_for() {
        const ANGLE: f64 = 1e-9;
        const HEIGHT: f64 = 1e-3;
        for view in [
            StartView {
                lon: -2.86,
                lat: 52.51,
                altitude: 1200.0,
                heading: 0.0,
                pitch: 30.0,
            },
            StartView {
                lon: 138.73,
                lat: 35.36,
                altitude: 9000.0,
                heading: 250.0,
                pitch: 12.0,
            },
            StartView {
                lon: -70.0,
                lat: -33.0,
                altitude: 400_000.0,
                heading: 100.0,
                pitch: 75.0,
            },
        ] {
            let camera = view.camera();
            let eye = ecef_to_geodetic(camera.position);
            assert!((eye.lon.to_degrees() - view.lon).abs() < ANGLE, "{view:?}");
            assert!((eye.lat.to_degrees() - view.lat).abs() < ANGLE, "{view:?}");
            assert!(
                (camera.altitude() - view.altitude).abs() < HEIGHT,
                "{view:?}"
            );
            // Compared around the circle: a heading of 0 may read back as a
            // hair under 360, and that is the same direction.
            let turn = (camera.heading().to_degrees() - view.heading + 180.0).rem_euclid(360.0);
            assert!(
                (turn - 180.0).abs() < ANGLE,
                "{view:?} reads heading {}",
                camera.heading().to_degrees()
            );
            assert!(
                (camera.pitch().to_degrees() - view.pitch).abs() < ANGLE,
                "{view:?} reads pitch {}",
                camera.pitch().to_degrees()
            );
        }
    }
}
