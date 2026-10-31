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

/// What `--help` prints before the keys. The keys are listed with the flags
/// because this is the only text the binary can show before a window exists.
const FLAGS_TEXT: &str = "\
FLAGS_HEADER

FLAGS (each takes one number; `--flag value` or `--flag=value`):
    --lon <deg>        longitude of the eye, east positive   [-180, 180]   default 2
    --lat <deg>        latitude of the eye, north positive   [-90, 90]     default 46
    --altitude <m>     height of the eye above the ellipsoid [1, 1e8]      default 2000000
    --heading <deg>    view direction, 0 = north, clockwise  [-360, 360]   default 0
    --pitch <deg>      angle below the horizon, 90 = down    [0, 90]       default 90
    --max-altitude <m> the highest the eye may go, metres above the ellipsoid
                       [1, 1e8]; default 36043000, twice the distance at which
                       the globe fits the view. --altitude above it, a link
                       or a zoom out are held under it
    --here             start over the current location instead of --lon/--lat
                       (asks the system's location service; the view moves
                       when the answer arrives, and stays put if none does)
    --imagery <name>   the imagery layer to open on, by key or by name
    --time <when>      the instant the scene is lit for: now (the default),
                       sunset or sunrise (the next one over the start view),
                       or an RFC 3339 instant (2026-06-21T19:42:00Z)
    --time-offset <d>  added to it: -3h30m, +45m, 90s
    --time-rate <x>    scene seconds a second: 1 (real time, the default),
                       60 (an hour a minute), 0 (frozen), -1 (backwards)
                       A link says the same three as time=, timeOffset= and
                       timeRate=, a sunset being the one where it leads:
                       tuile://goto?lon=0.1418&lat=42.9365&altitude=3100
                         &heading=290&pitch=0&time=sunset&timeOffset=-30m&timeRate=30
    --record <file>    record the session into an MCAP file: per frame the
                       camera, the wall and scene clocks, the viewport, the
                       atmosphere's state and the input events (the format
                       `tuile_tape::session` reads, and a replay flies),
                       and the pictures drawn, as they were on the screen.
                       O, or View > Record Session, starts and stops one
    --record-fps <hz>  pictures a second of a recording: 30 (the default),
                       0 for every frame drawn
    --record-scale <s> a picture's size over the frame's: 0.5 (the default),
                       1 for the frame as drawn, 0.25
    --record-codec <c> jpeg (the default) or png (every bit kept)
    --config <file>    a configuration file (a .tuile file: the access token,
                       the API address, the terrain, the layers, the start
                       view), read for this session and not kept. Without it,
                       config.tuile in the support directory is read if it
                       exists; a flag wins over what the file says. Opening a
                       file in the application (File > Open) keeps it instead
IMAGERY_LAYERS
    --imagery-compression <off|auto|bc7|astc>
                       how imagery is held on the GPU: off (RGBA8, the
                       default), auto (the best block format the GPU samples:
                       BC7 on a desktop, ASTC 4x4 on a phone), or that format.
                       Blocks hold a quarter of the bytes, encoded on the GPU
                       at upload; a tile that cannot be encoded stays RGBA8

SOURCES BY ADDRESS (no catalog, no token; each replaces the host's own):
    --terrain-url <url>          quantized-mesh terrain: the URL of its layer.json.
                                 Its tiles are addressed as the layer.json says
                                 (TMS: row 0 in the south)
    --imagery-template <url>     imagery from a URL template naming {z}, {x}, {y},
                                 addressed XYZ (row 0 in the north); the only layer
                                 Its tiles are stored by the template: when the server
                                 redraws them, change it (`?v=2`) to fetch them anew
    --imagery-scheme <grid>      the template's grid: webmercator (1 tile at level 0,
                                 the default) or geographic (2 tiles at level 0)
    --imagery-max-level <n>      the deepest level the template serves; deeper,
                                 its tiles are stretched
    -h, --help         print this and exit
";

/// The keys, the same in a window and on a page — but for the last line,
/// which only an application that is its own window answers.
const KEYS: &str = "\
KEYS:
    drag               move the globe under the cursor
    right-drag         tilt (vertical) and turn (horizontal)
    wheel              zoom toward the cursor
    N                  north up: turn the view about the point at its centre
                       (a click on the compass ring does the same)
    L                  centre on the current location, at the present altitude
    C                  copy a link to this view and its moment (a
                       tuile://goto?… URL; time=sunset&timeOffset=… when the
                       clock was started from a sunset)
    V                  go to the link on the clipboard
    I                  the next imagery layer
    W                  wireframe
    D                  cycle the diagnostic views
    F                  freeze the traversal
    J                  the scene's clock a quarter of an hour back
    K                  a quarter of an hour on
    Y                  an hour back
    U                  an hour on
    E                  the clock slower (past real time: backwards)
    R                  the clock faster
    P                  pause the clock, or run it again
    T                  back to the present, in real time
    O                  record the session, the pictures drawn included, or stop
";

/// What only an application that is its own window answers.
const QUIT: &str = "    Esc                quit\n";

/// The gestures, where there is a touch screen to make them on.
const TOUCH: &str = "\
TOUCH:
    one finger         move the globe under it
    two fingers        pinch to zoom, twist to turn, slide together to tilt
    double tap         zoom in
    finger held still  centre on the current location
";

/// What `--help` ends on.
const ELSEWHERE: &str = "
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
    format!("{FLAGS_TEXT}\n{KEYS}{QUIT}{ELSEWHERE}")
        .replace("IMAGERY_LAYERS\n", &format!("{}\n", layers.join("\n")))
        .replace("FLAGS_HEADER\n", &head)
        .replace("tuile://", &format!("{}://", identity.scheme))
}

/// The keys and gestures a viewer in a page answers: no flags, since a page
/// is started by its host's script, and no Esc, since the page is not the
/// viewer's to close.
pub(crate) fn keys() -> String {
    format!("{KEYS}\n{TOUCH}").replace(
        "tuile://",
        &format!("{}://", crate::embed::identity().scheme),
    )
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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn remembered(host: &dyn crate::host::Host) -> Option<StartView> {
    let file = crate::host::last_view_file(host)?;
    view_in_link(&host.read(&file).ok()?)
}

/// Fills in `view` from a configuration file's start view, for each number
/// the command line `arguments` did not give: a flag wins over the file.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fill_in(view: &mut StartView, arguments: &[String], start: &[(String, f64)]) {
    for (key, value) in start {
        let flag = format!("--{key}");
        let given = arguments
            .iter()
            .any(|a| a == &flag || a.starts_with(&format!("{flag}=")));
        if given {
            continue;
        }
        if let Some(f) = FLAGS.iter().find(|f| f.name == flag) {
            *(f.field)(view) = *value;
        }
    }
}

/// The view a remembered link describes — the whole of one, and nothing
/// else: a link that is not a single complete `goto` is no view.
pub(crate) fn view_in_link(text: &str) -> Option<StartView> {
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
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq)]
// Read once, at start: a few hundred bytes on the stack is nothing to box.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Invocation {
    /// Open the window on this view — and, with `here`, move it over the
    /// current location once the system says where that is. The view's own
    /// longitude and latitude are then only where the eye waits meanwhile.
    ///
    /// `addresses` are the sources the command line named by URL, which take
    /// the place of the host's.
    ///
    /// `imagery` is the layer `--imagery` named, as typed: which layer that
    /// is depends on the host's list, which this parser does not have.
    /// `compression` is what `--imagery-compression` said, if it was given.
    Run {
        view: StartView,
        here: bool,
        imagery: Option<String>,
        addresses: Addresses,
        compression: Option<tuile_wgpu::ImageryCompression>,
        /// `--time`, `--time-offset` and `--time-rate`: the instant the scene
        /// is lit for, and how it runs.
        clock: tuile_atmosphere::ClockSettings,
        /// `--max-altitude`, metres: the camera's ceiling, when said.
        max_altitude: Option<f64>,
        /// `--record` and its pictures' `--record-fps`, `--record-scale`
        /// and `--record-codec`.
        record: crate::recording::RecordSettings,
        /// `--config`: a configuration file read for this session — its
        /// token used, nothing kept (see `crate::import`).
        config: Option<std::path::PathBuf>,
    },
    /// Print [`usage`] and leave.
    Help,
}

/// Sources the command line names by their address: see [`crate::open`].
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Addresses {
    /// `--terrain-url`: a `layer.json`.
    pub terrain: Option<url::Url>,
    /// `--imagery-template` and the two flags that describe its grid.
    pub imagery: Option<Template>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Addresses {
    /// Whether the host has nothing left to answer: no catalog to ask, no
    /// token to find.
    pub(crate) fn are_everything(&self) -> bool {
        self.terrain.is_some() && self.imagery.is_some()
    }

    pub(crate) fn are_nothing(&self) -> bool {
        self.terrain.is_none() && self.imagery.is_none()
    }
}

/// An imagery template, as the command line gave it.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Template {
    /// `https://…/{z}/{x}/{y}.png`, addressed XYZ.
    pub template: String,
    /// On the geographic grid (two tiles at level 0) rather than the
    /// web-mercator one.
    pub geographic: bool,
    /// The deepest level served, when said.
    pub max_level: Option<u32>,
}

/// One flag: its name, the range it accepts (inclusive at both ends) and the
/// field it fills. A table rather than five `match` arms so the bounds a value
/// is checked against cannot drift apart from one flag to the next.
struct Flag {
    name: &'static str,
    unit: &'static str,
    min: f64,
    max: f64,
    /// The command line's: a page is given its view as numbers.
    #[cfg(not(target_arch = "wasm32"))]
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
        #[cfg(not(target_arch = "wasm32"))]
        field: |v| &mut v.lon,
    },
    Flag {
        name: "--lat",
        unit: "degrees",
        min: -90.0,
        max: 90.0,
        #[cfg(not(target_arch = "wasm32"))]
        field: |v| &mut v.lat,
    },
    Flag {
        name: "--altitude",
        unit: "metres",
        min: 1.0,
        max: 1.0e8,
        #[cfg(not(target_arch = "wasm32"))]
        field: |v| &mut v.altitude,
    },
    Flag {
        name: "--heading",
        unit: "degrees",
        min: -360.0,
        max: 360.0,
        #[cfg(not(target_arch = "wasm32"))]
        field: |v| &mut v.heading,
    },
    Flag {
        name: "--pitch",
        unit: "degrees",
        min: 0.0,
        max: 90.0,
        #[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn parse<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut view = StartView::default();
    let mut here = false;
    let mut imagery: Option<String> = None;
    // The flags that take text: each at most once.
    let mut texts: [(&str, Option<String>); 13] = [
        ("--terrain-url", None),
        ("--imagery-template", None),
        ("--imagery-scheme", None),
        ("--imagery-max-level", None),
        ("--time", None),
        ("--time-offset", None),
        ("--time-rate", None),
        ("--max-altitude", None),
        ("--record", None),
        ("--record-fps", None),
        ("--record-scale", None),
        ("--record-codec", None),
        ("--config", None),
    ];
    let mut compression: Option<tuile_wgpu::ImageryCompression> = None;
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
        if name == "--imagery-compression" {
            if compression.is_some() {
                return Err("--imagery-compression given twice".to_owned());
            }
            let text = match inline {
                Some(text) => text,
                None => args.next().map(|s| s.as_ref().to_owned()).ok_or_else(|| {
                    "--imagery-compression needs off, auto, bc7 or astc".to_owned()
                })?,
            };
            compression = Some(tuile_wgpu::compression_named(&text)?);
            continue;
        }
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
        if let Some((flag, slot)) = texts.iter_mut().find(|(flag, _)| *flag == name) {
            if slot.is_some() {
                return Err(format!("{flag} given twice"));
            }
            let text = match inline {
                Some(text) => text,
                None => args
                    .next()
                    .map(|s| s.as_ref().to_owned())
                    .ok_or_else(|| format!("{flag} needs a value"))?,
            };
            if text.trim().is_empty() {
                return Err(format!("{flag} needs a value"));
            }
            *slot = Some(text.trim().to_owned());
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
    let [(_, terrain), (_, template), (_, scheme), (_, max_level), (_, time), (_, offset), (_, rate), (_, ceiling), (_, record), (_, record_fps), (_, record_scale), (_, record_codec), (_, config)] =
        texts;
    // Read as an altitude is, with the same bounds: it is one.
    let max_altitude = ceiling
        .map(|text| {
            value("altitude", &text).map_err(|why| why.replace("--altitude", "--max-altitude"))
        })
        .transpose()?;
    let clock = tuile_atmosphere::ClockSettings::parse(
        time.as_deref(),
        offset.as_deref(),
        rate.as_deref(),
    )?;
    let addresses = addresses(terrain, template, scheme, max_level)?;
    // The template is the only layer: there is no other for `--imagery` to
    // name.
    if addresses.imagery.is_some() && imagery.is_some() {
        return Err(
            "--imagery names one of the host's layers, and --imagery-template replaces \
             them; give one"
                .to_owned(),
        );
    }
    Ok(Invocation::Run {
        view,
        here,
        imagery,
        addresses,
        compression,
        clock,
        max_altitude,
        record: crate::recording::RecordSettings {
            path: record.map(std::path::PathBuf::from),
            pictures: crate::recording::PictureSettings::parse(
                record_fps.as_deref(),
                record_scale.as_deref(),
                record_codec.as_deref(),
            )?,
        },
        config: config.map(std::path::PathBuf::from),
    })
}

/// The sources by address, from the text of their four flags.
#[cfg(not(target_arch = "wasm32"))]
fn addresses(
    terrain: Option<String>,
    template: Option<String>,
    scheme: Option<String>,
    max_level: Option<String>,
) -> Result<Addresses, String> {
    let terrain = terrain
        .map(|url| crate::open::absolute_url("--terrain-url", &url))
        .transpose()?;
    let geographic = match scheme.as_deref() {
        None | Some("webmercator") => false,
        Some("geographic") => true,
        Some(other) => {
            return Err(format!(
                "--imagery-scheme is geographic or webmercator, got {other:?}"
            ))
        }
    };
    let max_level = max_level
        .map(|text| {
            text.parse::<u32>()
                .ok()
                .filter(|level| *level <= 30)
                .ok_or_else(|| {
                    format!("--imagery-max-level wants a level from 0 to 30, got {text:?}")
                })
        })
        .transpose()?;
    let imagery = match template {
        Some(template) => {
            crate::open::check_template("--imagery-template", &template)?;
            Some(Template {
                template,
                geographic,
                max_level,
            })
        }
        None if scheme.is_some() || max_level.is_some() => {
            return Err(
                "--imagery-scheme and --imagery-max-level describe an --imagery-template, \
                 and there is none"
                    .to_owned(),
            )
        }
        None => None,
    };
    Ok(Addresses { terrain, imagery })
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
            addresses,
            compression: None,
            clock: _,
            max_altitude: None,
            record: _,
            config: None,
        }) = parsed
        else {
            unreachable!("{args:?} should start a session, got {parsed:?}");
        };
        assert!(addresses.are_nothing());
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
        assert!(FLAGS_TEXT.contains("--imagery"));
    }

    /// The sources by address: read in either spelling, checked, and
    /// refused when they contradict each other or describe nothing.
    #[test]
    fn sources_by_address() {
        let addresses = |args: &[&str]| match parse(args) {
            Ok(Invocation::Run { addresses, .. }) => Ok(addresses),
            Ok(other) => unreachable!("{other:?}"),
            Err(e) => Err(e),
        };
        let both = addresses(&[
            "--terrain-url",
            "http://127.0.0.1:8092/terrain/layer.json",
            "--imagery-template=http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png",
            "--imagery-scheme",
            "geographic",
            "--imagery-max-level=15",
            "--altitude",
            "50",
        ])
        .expect("valid");
        assert!(both.are_everything());
        assert_eq!(
            both.terrain.as_ref().map(url::Url::as_str),
            Some("http://127.0.0.1:8092/terrain/layer.json")
        );
        assert_eq!(
            both.imagery,
            Some(Template {
                template: "http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png".into(),
                geographic: true,
                max_level: Some(15),
            })
        );
        // Either alone; web-mercator unless said.
        let terrain = addresses(&["--terrain-url", "https://t.test/layer.json"]).expect("valid");
        assert!(terrain.terrain.is_some() && terrain.imagery.is_none());
        let imagery =
            addresses(&["--imagery-template", "https://t.test/{z}/{x}/{y}.jpg"]).expect("valid");
        assert!(!imagery.imagery.expect("a template").geographic);
        // Without any, nothing — the host's sources stand.
        assert!(addresses(&[]).expect("valid").are_nothing());
        for (args, says) in [
            (&["--terrain-url", "layer.json"][..], "not a URL"),
            (&["--terrain-url"][..], "needs a value"),
            (&["--terrain-url=a:b", "--terrain-url=a:c"][..], "twice"),
            (
                &["--imagery-template", "https://t.test/{z}/{x}.png"][..],
                "{y}",
            ),
            (&["--imagery-scheme", "geographic"][..], "there is none"),
            (&["--imagery-max-level", "12"][..], "there is none"),
            (
                &[
                    "--imagery-template",
                    "https://t/{z}/{x}/{y}",
                    "--imagery-scheme",
                    "mercator",
                ][..],
                "geographic or webmercator",
            ),
            (
                &[
                    "--imagery-template",
                    "https://t/{z}/{x}/{y}",
                    "--imagery-max-level",
                    "-1",
                ][..],
                "0 to 30",
            ),
            (
                &[
                    "--imagery-template",
                    "https://t/{z}/{x}/{y}",
                    "--imagery",
                    "aerial",
                ][..],
                "give one",
            ),
        ] {
            let error = addresses(args).expect_err("refused");
            assert!(error.contains(says), "{args:?}: {error:?}");
        }
        for flag in [
            "--terrain-url",
            "--imagery-template",
            "--imagery-scheme",
            "--imagery-max-level",
            "--max-altitude",
        ] {
            assert!(FLAGS_TEXT.contains(flag), "{flag} is in --help");
        }
    }

    /// `--max-altitude` is an altitude, read and bounded as `--altitude`
    /// is, once; the default `--help` states is the camera's own.
    #[test]
    fn the_max_altitude_is_read_as_an_altitude() {
        let said = |args: &[&str]| match parse(args) {
            Ok(Invocation::Run { max_altitude, .. }) => Ok(max_altitude),
            Ok(other) => unreachable!("{other:?}"),
            Err(why) => Err(why),
        };
        assert_eq!(said(&[]), Ok(None));
        assert_eq!(said(&["--max-altitude", "500000"]), Ok(Some(500_000.0)));
        assert_eq!(said(&["--max-altitude=2e7"]), Ok(Some(2.0e7)));
        for bad in [
            &["--max-altitude", "0"][..],
            &["--max-altitude", "far"],
            &["--max-altitude", "1e9"],
        ] {
            let why = said(bad).expect_err("refused");
            assert!(why.contains("--max-altitude"), "{why}");
        }
        assert!(said(&["--max-altitude", "1e6", "--max-altitude", "2e6"]).is_err());
        let default = (tuile_camera::default_max_altitude() / 1000.0).round() * 1000.0;
        assert!(
            FLAGS_TEXT.contains(&format!("default {default}")),
            "{default}"
        );
    }

    /// `--imagery-compression` takes one of four names, once; without it
    /// nothing is said and the session holds RGBA8.
    #[test]
    fn imagery_compression_is_named_once() {
        use tuile_wgpu::ImageryCompression;
        let said = |args: &[&str]| match parse(args) {
            Ok(Invocation::Run { compression, .. }) => compression,
            other => unreachable!("{args:?} should start a session, got {other:?}"),
        };
        assert_eq!(said(&[]), None);
        assert_eq!(
            said(&["--imagery-compression", "auto"]),
            Some(ImageryCompression::Auto)
        );
        assert_eq!(
            said(&["--imagery-compression=off", "--lat", "3"]),
            Some(ImageryCompression::Off)
        );
        assert_eq!(
            said(&["--imagery", "x", "--imagery-compression", "astc"]),
            Some(ImageryCompression::Format(
                tuile_core::texels::BlockFormat::Astc4x4
            ))
        );
        for bad in [
            &["--imagery-compression"][..],
            &["--imagery-compression", "etc2"],
            &[
                "--imagery-compression",
                "bc7",
                "--imagery-compression",
                "off",
            ],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(FLAGS_TEXT.contains("--imagery-compression <off|auto|bc7|astc>"));
    }

    /// `--record` names the file a session is recorded into, once.
    #[test]
    fn record_names_the_session_file() {
        let recorded = |args: &[&str]| match parse(args) {
            Ok(Invocation::Run { record, .. }) => Ok(record.path),
            Ok(Invocation::Help) => Err("help".to_owned()),
            Err(e) => Err(e),
        };
        assert_eq!(recorded(&[]), Ok(None));
        assert_eq!(
            recorded(&["--record", "flicker.mcap"]),
            Ok(Some(std::path::PathBuf::from("flicker.mcap")))
        );
        assert_eq!(
            recorded(&["--record=a b.mcap", "--lon", "0.14"]),
            Ok(Some(std::path::PathBuf::from("a b.mcap")))
        );
        assert!(recorded(&["--record"]).is_err());
        assert!(recorded(&["--record", "a", "--record", "b"]).is_err());
        assert!(FLAGS_TEXT.contains("--record <file>"));
    }

    #[test]
    fn a_files_start_view_fills_in_what_the_flags_did_not_say() {
        let arguments: Vec<String> = ["--lon", "3", "--altitude=900"].map(str::to_owned).into();
        let mut view = run(&["--lon", "3", "--altitude=900"]);
        fill_in(
            &mut view,
            &arguments,
            &[
                ("lon".into(), 10.0),
                ("lat".into(), 45.0),
                ("altitude".into(), 5000.0),
            ],
        );
        assert_eq!((view.lon, view.lat, view.altitude), (3.0, 45.0, 900.0));
    }

    #[test]
    fn config_names_a_file_once() {
        match parse(["--config", "mine.tuile", "--lon", "3"]) {
            Ok(Invocation::Run { config, view, .. }) => {
                assert_eq!(config.as_deref(), Some(std::path::Path::new("mine.tuile")));
                assert_eq!(view.lon, 3.0);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(["--config=a.tuile", "--config", "b.tuile"]).is_err());
        assert!(parse(["--config"]).is_err());
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
                addresses: Addresses::default(),
                compression: None,
                clock: Default::default(),
                max_altitude: None,
                record: Default::default(),
                config: None,
            })
        );
        // Either order, either coordinate.
        for args in [["--here", "--lon", "2"], ["--lat", "46", "--here"]] {
            let error = parse(args).expect_err("two positions");
            assert!(error.contains("--here"), "{error:?}");
        }
    }

    /// A page's help lists every key the window answers and every gesture,
    /// and neither the flags nor Esc, which a page's viewer does not have.
    #[test]
    fn a_pages_help_is_the_keys_and_the_gestures_only() {
        let keys = keys();
        for (key, _) in crate::menu::KEYS {
            let shown = format!("    {}  ", key.to_ascii_uppercase());
            assert!(keys.contains(&shown), "{key:?} is not in a page's help");
            assert!(usage().contains(&shown), "{key:?} is not in --help");
        }
        assert!(keys.contains("TOUCH:") && keys.contains("double tap"));
        assert!(!keys.contains("--lon") && !keys.contains("Esc"), "{keys}");
        assert!(usage().contains("Esc") && usage().contains("--lon"));
    }

    /// A remembered link is a view only when it is one whole `goto`.
    #[test]
    fn a_remembered_link_is_a_whole_view_or_none() {
        let view = StartView {
            lon: 6.86,
            lat: 45.83,
            altitude: 9000.0,
            heading: 120.0,
            pitch: 30.0,
        };
        let link = crate::steer::link(&view);
        let read = view_in_link(&format!("{link}\n")).expect("a view");
        assert!((read.lon - view.lon).abs() < 1e-9 && (read.pitch - view.pitch).abs() < 1e-9);
        assert_eq!(view_in_link("tuile://north"), None);
        assert_eq!(
            view_in_link("tuile://goto?lon=2&lat=46"),
            None,
            "half a view"
        );
        assert_eq!(view_in_link("not a link"), None);
    }

    #[test]
    fn help_wins_wherever_it_stands_and_lists_every_flag() {
        assert_eq!(parse(["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(["--lat", "10", "-h"]), Ok(Invocation::Help));
        for flag in &FLAGS {
            assert!(
                FLAGS_TEXT.contains(flag.name),
                "{} is not in --help",
                flag.name
            );
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
