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
//! A running viewer takes commands as URLs of the scheme [`scheme`]. On macOS
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
//! | `tuile://goto?…&time=&timeOffset=&timeRate=` | also sets the scene's clock, as `--time`, `--time-offset` and `--time-rate` do: see below |
//! | `tuile://north` | north up, about the point at the centre of the view |
//! | `tuile://here` | centres on the current location (the system still asks the person) |
//! | `tuile://view?freeze=on\|off&wireframe=on\|off` | the two display switches |
//! | `tuile://imagery?name=<key>` | drapes another of the host's imagery layers; `name=next` takes the one after the present |
//! | `tuile://import?src=https://…` | fetches a configuration file and imports it, as opening it would (`crate::import`) |
//!
//! ## The scene's clock in a link
//!
//! A `goto` may also say when, with the flags' grammar, read by the same
//! parser ([`tuile_atmosphere::ClockSettings::parse`]) as the flags and a
//! page's options:
//!
//! | parameter | value |
//! |---|---|
//! | `time` | `now`, `sunset`, `sunrise`, or an RFC 3339 instant (`2026-06-21T19:42:00Z`) |
//! | `timeOffset` | added to it, a signed duration: `-30m`, `-3h30m`, `45m`, `90s` |
//! | `timeRate` | scene seconds a second: `1` real time, `30`, `0` frozen, `-1` backwards |
//!
//! `sunset` and `sunrise` are the next ones *at the place the link points
//! to* — its `lon` and `lat`, the eye's present ones where it leaves them
//! out — not where the eye was before. So a link watches a sunset in the
//! Pyrenees, half an hour before it, an hour of the scene every two minutes:
//!
//! ```text
//! tuile://goto?lon=0.1418&lat=42.9365&altitude=3100&heading=290&pitch=0&time=sunset&timeOffset=-30m&timeRate=30
//! ```
//!
//! Without `time` the clock is left alone, as a left-out `lon` leaves the
//! eye; `timeRate` alone changes the rate and keeps the instant, and
//! `timeOffset` alone is refused — it has nothing to be counted from. A
//! `goto` that says only when (`goto?time=sunset`) sets the clock where the
//! eye is and does not move it. The link `C` copies carries the clock: from
//! the nearest sunset or sunrise when the clock was started from one, so
//! the link brings back the same moment of the evening whatever day it is
//! opened, and as the instant otherwise; `timeRate` when it is not 1.
//!
//! **What a URL can do is all in that table.** It moves the camera, sets the
//! scene's clock, flips two display switches and picks among the layers the
//! host listed — or, `import`, fetches a configuration file from an `https`
//! address and imports it with the same checks as a file opened by hand. It
//! reads no local file, runs nothing, and carries no credential (an `import`
//! whose address names a token is refused); anything else — an unknown command, an unknown parameter, a
//! value out of the flags' bounds, a URL longer than [`LONGEST_URL`] — is
//! dropped whole with one line in the log, and the view does not move.
//!
//! The moves themselves are written against the controller's public gestures
//! and constructors. Nothing here reaches into the camera crate, so nothing
//! here can disagree with what a drag or a wheel does to the same state.

use std::f64::consts::{PI, TAU};

use tuile_atmosphere::{ClockSettings, SceneClock};
use tuile_camera::{CameraController, GlobeCamera};
use tuile_core::geo::{ecef_to_geodetic, Geodetic};

use crate::start::StartView;

/// The URL scheme a running viewer is steered by: the host's
/// ([`crate::embed::Identity::scheme`]), `tuile` for the project's own
/// application. The bundle's `Info.plist` must declare the same word, or the
/// system delivers the URLs elsewhere.
pub(crate) fn scheme() -> &'static str {
    &crate::embed::identity().scheme
}

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
    /// Glide there over this many seconds rather than jump: see
    /// `app::flight`. `seconds=` in a URL, `flyTo` on a page.
    pub seconds: Option<f64>,
    /// What it says of the scene's clock: `None` leaves it alone.
    pub time: Option<Retime>,
}

/// What a link says of the scene's clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Retime {
    /// `time=`, with `timeOffset=` and `timeRate=` if given: the clock
    /// started again, a sunset or a sunrise at the place the link points to.
    Start(ClockSettings),
    /// `timeRate=` alone: the instant kept, the rate changed.
    Rate(f64),
}

/// The longest glide a URL may ask for, seconds.
pub(crate) const LONGEST_GLIDE: f64 = 60.0;

/// One thing the view can be told to do, whoever is telling it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Command {
    Goto(Goto),
    NorthUp,
    /// Centre on the current location.
    Here,
    Freeze(bool),
    Wireframe(bool),
    /// Drape the host's imagery layer of this index — see
    /// [`crate::embed::ViewerHost::imagery`]. An index and not a name, so the
    /// command is checked once, where it is read, against the list.
    Imagery(usize),
    /// The layer after the present one, round the list.
    NextImagery,
    /// Draw this diagnostic view — an index into the views `D` cycles, 0
    /// being the real picture.
    Diagnostic(usize),
    /// Put a link to the present view on the clipboard.
    CopyLink,
    /// Go where the link on the clipboard says, if it holds one.
    PasteLink,
    /// Show the keys and the flags, in a window: from the menu bar.
    #[cfg_attr(
        not(all(target_os = "macos", feature = "application")),
        allow(dead_code)
    )]
    ShowKeys,
    /// Ask for a configuration file to import, in the system's open panel:
    /// *File › Open Configuration…*.
    #[cfg_attr(
        not(all(target_os = "macos", feature = "application")),
        allow(dead_code)
    )]
    OpenConfiguration,
    /// The scene's clock at this instant (Unix seconds UTC), running at this
    /// rate from now on.
    Clock {
        instant: f64,
        rate: f64,
    },
    /// The scene's clock back to the present, in real time.
    ClockNow,
    /// The session recorded, or the recording closed.
    Record(bool),
    /// The host's 3D Tiles tileset of this asset number drawn over the
    /// globe, or not: see `crate::surface`.
    Tileset {
        asset: i64,
        on: bool,
    },
}

/// Which of `layers` a person, a flag, a URL or a script means by `text`: a
/// layer's key, or its name, in any case. One reading for all four.
pub(crate) fn layer_named(
    layers: &[crate::embed::ImageryChoice],
    text: &str,
) -> Result<usize, String> {
    let wanted = text.trim();
    layers
        .iter()
        .position(|l| l.key.eq_ignore_ascii_case(wanted) || l.name.eq_ignore_ascii_case(wanted))
        .ok_or_else(|| {
            let known: Vec<&str> = layers.iter().map(|l| l.key.as_str()).collect();
            format!(
                "there is no imagery layer {wanted:?}; the layers are: {}",
                known.join(", ")
            )
        })
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
    /// A script's `set imagery to "…"`: a layer by key or name, still to be
    /// looked up in the host's list.
    Imagery(String),
    /// The clipboard's text, read for "go to copied link" by a platform
    /// whose clipboard answers later: see `crate::desktop::Pasted`.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Pasted(Option<String>),
    /// A configuration file's name and text, to import (`crate::import`):
    /// shared with or opened in the application, or fetched for a link.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Import {
        file: String,
        text: String,
    },
    /// A configuration file on disk, to read and import: opened from the
    /// Finder, *File › Open…*, or handed over by the system.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    ImportFile(std::path::PathBuf),
    /// An import that failed on its way to the loop — a link's file that
    /// could not be fetched — and why.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    ImportFailed(String),
}

impl Goto {
    /// Whether it says where, rather than only how long to take or when.
    pub(crate) fn says_where(&self) -> bool {
        [self.lon, self.lat, self.altitude, self.heading, self.pitch]
            .iter()
            .any(Option::is_some)
    }

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
        if !goto.says_where() {
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
    parse_url_among(url, &[])
}

/// [`parse_url`], for a session whose host offers `layers`: the only URLs it
/// adds are the ones that name one of them.
pub(crate) fn parse_url_among(
    url: &str,
    layers: &[crate::embed::ImageryChoice],
) -> Result<Vec<Command>, String> {
    if url.len() > LONGEST_URL {
        return Err(format!("a steering URL of {} bytes is not one", url.len()));
    }
    let (scheme, rest) = url
        .trim()
        .split_once(':')
        .ok_or_else(|| "not a URL".to_owned())?;
    if !scheme.eq_ignore_ascii_case(self::scheme()) {
        return Err(format!(
            "the scheme is {:?}, not {scheme:?}",
            self::scheme()
        ));
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
            let (mut time, mut offset, mut rate) = (None, None, None);
            for (key, text) in &pairs {
                let clock = match *key {
                    "time" => Some(&mut time),
                    "timeOffset" => Some(&mut offset),
                    "timeRate" => Some(&mut rate),
                    _ => None,
                };
                if let Some(slot) = clock {
                    *slot = Some(*text);
                    continue;
                }
                if *key == "seconds" {
                    goto.seconds = Some(
                        text.parse::<f64>()
                            .ok()
                            .filter(|s| (0.0..=LONGEST_GLIDE).contains(s))
                            .ok_or_else(|| {
                                format!("seconds is between 0 and {LONGEST_GLIDE}, not {text:?}")
                            })?,
                    );
                    continue;
                }
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
            goto.time = retime(time, offset, rate)?;
            if !goto.says_where() && goto.time.is_none() {
                return Err("goto says nothing: give at least one of lon, lat, \
                            altitude, heading, pitch, time, timeRate"
                    .to_owned());
            }
            Ok(vec![Command::Goto(goto)])
        }
        "north" => bare(Command::NorthUp),
        "here" => bare(Command::Here),
        "imagery" => match pairs.as_slice() {
            [("name", "next")] => Ok(vec![Command::NextImagery]),
            [("name", name)] => Ok(vec![Command::Imagery(layer_named(layers, name)?)]),
            _ => Err("imagery takes exactly one parameter, name=".to_owned()),
        },
        "tileset" => {
            let (mut asset, mut on) = (None, None);
            for (key, text) in &pairs {
                match *key {
                    "asset" => {
                        asset = Some(text.parse::<i64>().map_err(|_| {
                            format!("asset is a tileset's asset number, not {text:?}")
                        })?)
                    }
                    "show" => {
                        on = Some(match *text {
                            "on" => true,
                            "off" => false,
                            _ => return Err(format!("show is on or off, not {text:?}")),
                        })
                    }
                    _ => return Err(format!("tileset takes asset= and show=, not {key:?}")),
                }
            }
            match (asset, on) {
                (Some(asset), Some(on)) => Ok(vec![Command::Tileset { asset, on }]),
                _ => Err("tileset takes asset= and show=on|off".to_owned()),
            }
        }
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

/// The clock a `goto`'s `time`, `timeOffset` and `timeRate` say, read as
/// the flags read theirs — by the same parser — or the sentence that says
/// which is wrong.
fn retime(
    time: Option<&str>,
    offset: Option<&str>,
    rate: Option<&str>,
) -> Result<Option<Retime>, String> {
    let clock = |time| {
        ClockSettings::parse(time, offset, rate).map_err(|why| format!("goto's clock: {why}"))
    };
    match (time, offset, rate) {
        (None, None, None) => Ok(None),
        (None, Some(o), _) => Err(format!(
            "timeOffset={o} has nothing to be counted from: give time= \
             (now, sunset, sunrise or an RFC 3339 instant) beside it"
        )),
        (None, None, Some(_)) => Ok(Some(Retime::Rate(clock(None)?.rate))),
        (Some(t), _, _) => Ok(Some(Retime::Start(clock(Some(t))?))),
    }
}

/// Where a view's eye is, for the sun: what a sunset is the next one at.
fn place(view: &StartView) -> Geodetic {
    Geodetic {
        lon: view.lon.to_radians(),
        lat: view.lat.to_radians(),
        height: view.altitude,
    }
}

/// The clock a `goto` leaves behind, from `clock` at `wall` (Unix seconds):
/// `clock` itself when the link said no time; started again at `to` — the
/// view the link leads to, not the one it leaves — when it said one.
pub(crate) fn retimed(
    clock: SceneClock,
    time: Option<Retime>,
    to: &StartView,
    wall: f64,
) -> SceneClock {
    match time {
        None => clock,
        Some(Retime::Start(settings)) => settings.start(wall, place(to)),
        Some(Retime::Rate(rate)) => {
            let mut clock = clock;
            clock.set_rate(rate, wall);
            clock
        }
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
    jump(controller, destination(controller, goto).camera());
}

/// The view `goto` names from where the eye now is: what it leaves out keeps
/// its present value.
pub(crate) fn destination(controller: &CameraController, goto: Goto) -> StartView {
    let now = view_of(controller.target());
    StartView {
        lon: goto.lon.unwrap_or(now.lon),
        lat: goto.lat.unwrap_or(now.lat),
        altitude: goto.altitude.unwrap_or(now.altitude),
        heading: goto.heading.unwrap_or(now.heading),
        pitch: goto.pitch.unwrap_or(now.pitch),
    }
}

/// The URL that brings a viewer back to this view: a bookmark, and the
/// `view url` a script reads.
///
/// Seven decimals of a degree is a centimetre on the ground, a centimetre of
/// altitude and a thousandth of a degree of attitude are below anything a
/// screen shows; more digits would only make the link longer.
pub(crate) fn link(view: &StartView) -> String {
    format!(
        "{}://goto?lon={:.7}&lat={:.7}&altitude={:.2}&heading={:.3}&pitch={:.3}",
        scheme(),
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

/// [`link`], with the scene's clock: the URL that brings back this view at
/// this moment — what `C` copies and a script's `view url` reads.
///
/// Counted from the sunset or the sunrise nearest the instant, at this
/// view's place, when the clock was started from one
/// ([`crate::menu::Clock::event`]): `time=sunset&timeOffset=-12m` brings
/// back twelve minutes before sunset on whatever day it is opened. As the
/// instant otherwise. `timeRate` when the clock does not run in real time.
///
/// The view kept for the next session is [`link`], without it: a session
/// opens on the present, wherever the last one left the clock.
pub(crate) fn link_at(view: &StartView, clock: &crate::menu::Clock) -> String {
    let settings = ClockSettings::of_instant(clock.instant, clock.rate, clock.event, place(view));
    let (time, offset, rate) = settings.texts();
    let mut url = format!("{}&time={time}", link(view));
    if let Some(offset) = offset {
        url.push_str(&format!("&timeOffset={offset}"));
    }
    if let Some(rate) = rate {
        url.push_str(&format!("&timeRate={rate}"));
    }
    url
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
/// target left behind would pull the eye straight back. The floor, the
/// ceiling and the relief the floor is measured against carry over, so a
/// goto beyond the ceiling lands at it.
pub(crate) fn jump(controller: &mut CameraController, camera: GlobeCamera) {
    let mut moved = CameraController::new(camera)
        .with_min_altitude(controller.min_altitude)
        .with_max_altitude(controller.max_altitude);
    if let Some(ground) = controller.ground() {
        moved = moved.with_ground(ground.clone());
    }
    *controller = moved;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_layers() -> Vec<crate::embed::ImageryChoice> {
        vec![
            crate::embed::ImageryChoice::new("aerial", "Aerial", 2, ""),
            crate::embed::ImageryChoice::new("labels", "Aerial with labels", 3, ""),
            crate::embed::ImageryChoice::new("satellite", "Satellite", 9, ""),
        ]
    }

    /// A layer is named the same way wherever it is named — by its key or by
    /// its name, in any case — and a name that is not on the host's list is
    /// refused with the list.
    #[test]
    fn a_layer_is_found_by_key_or_by_name() {
        let layers = three_layers();
        for (text, index) in [
            ("aerial", 0),
            ("AERIAL", 0),
            ("labels", 1),
            ("aerial with labels", 1),
            (" Satellite ", 2),
        ] {
            assert_eq!(layer_named(&layers, text), Ok(index), "{text:?}");
        }
        let refused = layer_named(&layers, "road").expect_err("not a layer");
        assert!(refused.contains("aerial, labels, satellite"), "{refused}");
        assert!(layer_named(&[], "aerial").is_err());
    }

    /// The imagery URL names one of the host's layers, or the next one, and
    /// nothing else: the list is the whole of what it can ask for.
    #[test]
    fn an_imagery_url_picks_among_the_hosts_layers() {
        let layers = three_layers();
        let read = |url: &str| parse_url_among(url, &layers);
        assert_eq!(
            read("tuile://imagery?name=labels"),
            Ok(vec![Command::Imagery(1)])
        );
        assert_eq!(
            read("tuile://imagery?name=Satellite"),
            Ok(vec![Command::Imagery(2)])
        );
        assert_eq!(
            read("tuile://imagery?name=next"),
            Ok(vec![Command::NextImagery])
        );
        for bad in [
            "tuile://imagery",
            "tuile://imagery?name=road",
            "tuile://imagery?name=",
            "tuile://imagery?layer=labels",
            "tuile://imagery?name=labels&name=aerial",
            "tuile://imagery?name=labels&freeze=on",
        ] {
            assert!(read(bad).is_err(), "{bad}");
        }
        // With no list there is no layer to name: the same URL is refused.
        assert!(parse_url("tuile://imagery?name=labels").is_err());
        // And the rest of the vocabulary is untouched by the list.
        assert_eq!(read("tuile://north"), Ok(vec![Command::NorthUp]));
    }

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
                seconds: None,
                time: None,
            })
        );
    }

    /// The example the documentation gives: the Pic du Midi, half an hour
    /// before sunset, an hour of the scene every two minutes.
    const SUNSET: &str = "tuile://goto?lon=0.1418&lat=42.9365&altitude=3100&heading=290\
                          &pitch=0&time=sunset&timeOffset=-30m&timeRate=30";

    /// A goto's clock is read by the flags' parser: the keywords, an
    /// instant, an offset and a rate; a bad value refuses the whole link and
    /// says which one; an offset with nothing to count from is refused.
    #[test]
    fn a_goto_reads_the_clock_as_the_flags_do() {
        let Command::Goto(goto) = one(SUNSET) else {
            unreachable!("a goto");
        };
        assert_eq!(goto.lon, Some(0.1418));
        assert_eq!(
            goto.time,
            Some(Retime::Start(
                ClockSettings::parse(Some("sunset"), Some("-30m"), Some("30")).unwrap()
            ))
        );
        let time = |url: &str| match one(url) {
            Command::Goto(goto) => goto.time,
            other => unreachable!("{other:?}"),
        };
        let start = |time, offset, rate| {
            Some(Retime::Start(
                ClockSettings::parse(time, offset, rate).unwrap(),
            ))
        };
        assert_eq!(
            time("tuile://goto?lat=1&time=sunrise"),
            start(Some("sunrise"), None, None)
        );
        assert_eq!(
            time("tuile://goto?lat=1&time=now&timeOffset=-3h30m"),
            start(Some("now"), Some("-3h30m"), None)
        );
        assert_eq!(
            time("tuile://goto?lat=1&time=2026-06-21T19:42:00Z&timeRate=-1"),
            start(Some("2026-06-21T19:42:00Z"), None, Some("-1"))
        );
        // A rate alone keeps the instant; a clock alone does not move the eye.
        assert_eq!(
            time("tuile://goto?lat=1&timeRate=0"),
            Some(Retime::Rate(0.0))
        );
        let Command::Goto(only_when) = one("tuile://goto?time=sunset") else {
            unreachable!("a goto");
        };
        assert!(!only_when.says_where());
        // No time, no clock.
        assert_eq!(time("tuile://goto?lon=2&lat=46"), None);

        for (url, says) in [
            ("tuile://goto?lat=1&time=teatime", "RFC 3339"),
            ("tuile://goto?lat=1&time=sunset&timeOffset=soon", "duration"),
            ("tuile://goto?lat=1&time=sunset&timeRate=fast", "time rate"),
            ("tuile://goto?lat=1&time=sunset&timeRate=nan", "time rate"),
            ("tuile://goto?lat=1&timeOffset=-30m", "counted from"),
            ("tuile://goto?lat=1&time=now&time=sunset", "twice"),
            ("tuile://goto?lat=1&time=", "RFC 3339"),
        ] {
            let why = parse_url(url).expect_err(url);
            assert!(why.contains(says), "{url}: {why}");
        }
    }

    fn pic() -> StartView {
        StartView {
            lon: 0.1418,
            lat: 42.9365,
            altitude: 3100.0,
            heading: 290.0,
            pitch: 0.0,
        }
    }

    /// `time=sunset` is the sunset where the link leads, not where the eye
    /// was: from the Pic du Midi, a link to Tokyo is lit for Tokyo's sunset,
    /// some eight hours earlier.
    #[test]
    fn a_sunset_is_the_one_where_the_link_leads() {
        let wall = tuile_atmosphere::clock::parse_rfc3339("2026-06-21T00:00:00Z").unwrap();
        let mut c = controller(pic());
        let Command::Goto(goto) =
            one("tuile://goto?lon=139.6917&lat=35.6895&altitude=3000&time=sunset&timeOffset=-30m")
        else {
            unreachable!("a goto");
        };
        let to = destination(&c, goto);
        go(&mut c, goto);
        let clock = retimed(SceneClock::real_time(wall), goto.time, &to, wall);
        let sunset_at = |view: &StartView| {
            tuile_atmosphere::Sun::next_sunset(wall, place(view)).expect("the sun sets")
        };
        let tokyo = sunset_at(&view_of(&c.camera));
        assert!((clock.at(wall) - (tokyo - 1800.0)).abs() < 1.0);
        assert!(
            (sunset_at(&pic()) - tokyo) > 6.0 * 3600.0,
            "two sunsets apart"
        );
        // And it runs at the link's rate: real time, as it did not say.
        assert_eq!(clock.rate(), 1.0);
    }

    /// A link without `time` leaves the clock as it was — instant, rate,
    /// pause and all; `timeRate` alone keeps the instant.
    #[test]
    fn a_goto_without_time_leaves_the_clock_alone() {
        let wall = 1_790_000_000.0;
        let mut clock = SceneClock::real_time(wall - 100.0);
        clock.set_rate(60.0, wall - 50.0);
        clock.pause(wall - 10.0);
        let Command::Goto(goto) = one("tuile://goto?lon=139.69&lat=35.69") else {
            unreachable!("a goto");
        };
        assert_eq!(retimed(clock, goto.time, &pic(), wall), clock);
        let Command::Goto(rate) = one("tuile://goto?lat=1&timeRate=10") else {
            unreachable!("a goto");
        };
        let after = retimed(clock, rate.time, &pic(), wall);
        assert_eq!(after.at(wall), clock.at(wall));
        assert_eq!(after.rate(), 10.0);
    }

    /// The link `C` copies brings back the view and the clock: twelve
    /// minutes before the sunset it was counted from, at its rate; and an
    /// absolute instant when the clock was started from none.
    #[test]
    fn the_link_with_its_clock_leads_back_to_the_view_and_the_moment() {
        let noon = tuile_atmosphere::clock::parse_rfc3339("2026-06-21T12:00:00Z").unwrap();
        let view = pic();
        let sunset = tuile_atmosphere::Sun::next_sunset(noon, place(&view)).unwrap();
        let instant = (sunset - 720.0).round();
        let from_sunset = crate::menu::Clock {
            instant,
            rate: 30.0,
            resume: 30.0,
            event: Some(tuile_atmosphere::Event::Sunset),
        };
        let url = link_at(&view, &from_sunset);
        assert!(url.contains("&time=sunset&timeOffset=-1"), "{url}");
        assert!(url.ends_with("&timeRate=30"), "{url}");
        // Twelve minutes before it, to the second the ephemeris finds the
        // sunset to.
        let offset = url
            .split("timeOffset=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("an offset");
        let offset = tuile_atmosphere::clock::parse_duration(offset).unwrap();
        assert!((offset + 720.0).abs() <= 2.0, "{url}");
        let absolute = link_at(
            &view,
            &crate::menu::Clock {
                event: None,
                rate: 1.0,
                ..from_sunset
            },
        );
        assert!(absolute.contains("&time=2026-06-21T"), "{absolute}");
        assert!(!absolute.contains("timeRate"), "{absolute}");

        for (url, rate) in [(url, 30.0), (absolute, 1.0)] {
            let Command::Goto(goto) = one(&url) else {
                unreachable!("a link is a goto");
            };
            let mut c = controller(StartView {
                lon: 139.69,
                lat: 35.69,
                altitude: 50_000.0,
                heading: 10.0,
                pitch: 45.0,
            });
            let to = destination(&c, goto);
            go(&mut c, goto);
            // Opened at noon on that day.
            let clock = retimed(SceneClock::real_time(noon), goto.time, &to, noon);
            assert!((clock.at(noon) - instant).abs() <= 2.0, "{url}");
            assert_eq!(clock.rate(), rate);
            let back = view_of(&c.camera);
            assert!((back.lon - view.lon).abs() < 1e-6, "{back:?}");
            assert!((back.lat - view.lat).abs() < 1e-6, "{back:?}");
            assert!((back.altitude - view.altitude).abs() < 1e-2, "{back:?}");
        }
    }

    /// `seconds=` makes a goto a glide; it is bounded, and alone it says
    /// nowhere.
    #[test]
    fn a_goto_with_seconds_is_a_glide() {
        assert_eq!(
            one("tuile://goto?lon=6.86&lat=45.83&seconds=2.5"),
            Command::Goto(Goto {
                lon: Some(6.86),
                lat: Some(45.83),
                seconds: Some(2.5),
                ..Goto::default()
            })
        );
        assert!(parse_url("tuile://goto?seconds=3").is_err());
        assert!(parse_url("tuile://goto?lat=1&seconds=61").is_err());
        assert!(parse_url("tuile://goto?lat=1&seconds=soon").is_err());
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

    /// A goto past the ceiling lands at it, over the place it named; a
    /// goto under it is untouched; the controller's own ceiling survives
    /// the jump.
    #[test]
    fn a_goto_beyond_the_ceiling_is_held_to_it() {
        let mut c = controller(StartView::default()).with_max_altitude(5_000_000.0);
        let Command::Goto(goto) = one("tuile://goto?lon=139.69&lat=35.69&altitude=9e7&pitch=90")
        else {
            unreachable!("a goto");
        };
        go(&mut c, goto);
        let there = view_of(&c.camera);
        assert!((there.altitude - 5_000_000.0).abs() < 1e-3, "{there:?}");
        assert!((there.lon - 139.69).abs() < 1e-9 && (there.lat - 35.69).abs() < 1e-9);
        assert_eq!(c.max_altitude, 5_000_000.0);
        go(
            &mut c,
            Goto {
                altitude: Some(3_000.0),
                ..Goto::default()
            },
        );
        assert!((view_of(&c.camera).altitude - 3_000.0).abs() < 1e-3);
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

    /// A tileset is shown or hidden by its asset number, and nothing else
    /// reads as either.
    #[test]
    fn a_tileset_is_shown_or_hidden_by_url() {
        let url = |rest: &str| format!("{}://{rest}", scheme());
        assert_eq!(
            parse_url(&url("tileset?asset=2275207&show=off")),
            Ok(vec![Command::Tileset {
                asset: 2275207,
                on: false
            }])
        );
        assert_eq!(
            parse_url(&url("tileset?show=on&asset=7")),
            Ok(vec![Command::Tileset { asset: 7, on: true }])
        );
        for wrong in [
            "tileset",
            "tileset?asset=7",
            "tileset?show=on",
            "tileset?asset=x&show=on",
            "tileset?asset=7&show=maybe",
            "tileset?asset=7&show=on&layer=2",
        ] {
            assert!(parse_url(&url(wrong)).is_err(), "{wrong}");
        }
    }
}
