// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the view is right now, in numbers a script can ask for.
//!
//! The render loop publishes a [`Snapshot`] once a frame; a script's question
//! is answered from the last one published. The two never meet: the script's
//! handler runs inside the desktop's event dispatch and has no way to reach
//! the camera, and the render loop never waits for a script. A snapshot is at
//! most one frame old.
//!
//! [`PROPERTIES`] is the vocabulary — the keys the application's scripting
//! dictionary names, each with its type — and [`property`] the one function
//! that answers them. The desktop glue only carries a key in and a value out,
//! and a test holds the dictionary file to this table.
//!
//! **Where the machine is stays private here too.** Once the view has been
//! centred on the current location, the properties that would give the
//! position away — the eye's and the target's longitude and latitude, and the
//! view's URL — answer [`Value::Withheld`], which a script sees as
//! `missing value`. Altitude, heading, pitch and the rest say nothing about
//! where, and keep answering. A `go to` somewhere named lifts it.

use std::sync::Mutex;

use tuile_camera::CameraController;
use tuile_core::geo::ecef_to_geodetic;

use crate::start::StartView;

/// The ground point at the centre of the screen: degrees, and metres above
/// the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Target {
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
}

/// One frame's worth of facts about the view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Snapshot {
    /// The eye: where it is and how it looks, as the flags speak of it.
    pub view: StartView,
    /// What the centre of the screen looks at, and how far it is from the
    /// eye, in metres. `None` when the centre looks at the sky.
    pub target: Option<(Target, f64)>,
    /// Vertical field of view, degrees.
    pub field_of_view: f64,
    /// The drawable surface, in device pixels.
    pub size: (u32, u32),
    pub wireframe: bool,
    pub frozen: bool,
    /// Whether the location service put the view where it is.
    pub located: bool,
    /// Tiles drawn in the last frame.
    pub tiles: usize,
    /// Whether every one of them was the tile the traversal selected, rather
    /// than a coarser stand-in still waiting for it: the picture has stopped
    /// refining, and a capture taken now is the one that was meant.
    pub settled: bool,
    /// The imagery layer being draped, of the host's list.
    pub imagery: Option<&'static crate::embed::ImageryChoice>,
}

impl Snapshot {
    /// Reads the view off the controller. The numbers are those of the camera
    /// that reaches the screen, not of the gesture target: a script asking
    /// during a move is told what is drawn.
    pub(crate) fn of(controller: &CameraController, size: (u32, u32)) -> Self {
        let camera = &controller.camera;
        let viewport = (f64::from(size.0), f64::from(size.1));
        // The pick runs on the gesture target, which is the drawn camera once
        // the easing has settled — within a few frames of any move.
        let target = controller
            .pick((viewport.0 * 0.5, viewport.1 * 0.5), viewport)
            .map(|point| {
                let g = ecef_to_geodetic(point);
                (
                    Target {
                        lon: g.lon.to_degrees(),
                        lat: g.lat.to_degrees(),
                        height: g.height,
                    },
                    (controller.target().position - point).length(),
                )
            });
        Self {
            view: crate::steer::view_of(camera),
            target,
            field_of_view: camera.fovy.to_degrees(),
            size,
            wireframe: false,
            frozen: false,
            located: false,
            tiles: 0,
            settled: false,
            imagery: None,
        }
    }
}

/// The type a property has in the scripting dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Real,
    Integer,
    Flag,
    Text,
}

/// The answer to a property.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Real(f64),
    Integer(i64),
    Flag(bool),
    Text(String),
    /// There is an answer and it is not being given: see the module header.
    Withheld,
    /// There is no answer: the centre of the screen looks at the sky.
    Missing,
}

/// Every property of the view, by the key the dictionary's `cocoa key` names:
/// the key, its type, and whether a script may set it.
pub(crate) const PROPERTIES: [(&str, Kind, bool); 20] = [
    ("longitude", Kind::Real, false),
    ("latitude", Kind::Real, false),
    ("altitude", Kind::Real, false),
    ("heading", Kind::Real, false),
    ("pitch", Kind::Real, false),
    ("targetLongitude", Kind::Real, false),
    ("targetLatitude", Kind::Real, false),
    ("targetHeight", Kind::Real, false),
    ("range", Kind::Real, false),
    ("fieldOfView", Kind::Real, false),
    ("viewWidth", Kind::Integer, false),
    ("viewHeight", Kind::Integer, false),
    ("wireframe", Kind::Flag, true),
    ("frozen", Kind::Flag, true),
    ("viewURL", Kind::Text, false),
    ("tileCount", Kind::Integer, false),
    ("settled", Kind::Flag, false),
    // The layer by its key — what `--imagery` and the URLs call it — which a
    // script may set; then the same layer in the words a person reads.
    ("imagery", Kind::Text, true),
    ("imageryName", Kind::Text, false),
    ("imageryAttribution", Kind::Text, false),
];

/// Answers one property from a snapshot; `None` for a key that is not one.
pub(crate) fn property(s: &Snapshot, key: &str) -> Option<Value> {
    // The four that say where, and the URL that says it too.
    let place = |degrees: f64| {
        if s.located {
            Value::Withheld
        } else {
            Value::Real(degrees)
        }
    };
    let of_target = |pick: fn(&Target, f64) -> f64, says_where: bool| match s.target {
        None => Value::Missing,
        Some(_) if says_where && s.located => Value::Withheld,
        Some((target, range)) => Value::Real(pick(&target, range)),
    };
    Some(match key {
        "longitude" => place(s.view.lon),
        "latitude" => place(s.view.lat),
        "altitude" => Value::Real(s.view.altitude),
        "heading" => Value::Real(s.view.heading),
        "pitch" => Value::Real(s.view.pitch),
        "targetLongitude" => of_target(|t, _| t.lon, true),
        "targetLatitude" => of_target(|t, _| t.lat, true),
        "targetHeight" => of_target(|t, _| t.height, false),
        "range" => of_target(|_, range| range, false),
        "fieldOfView" => Value::Real(s.field_of_view),
        "viewWidth" => Value::Integer(i64::from(s.size.0)),
        "viewHeight" => Value::Integer(i64::from(s.size.1)),
        "wireframe" => Value::Flag(s.wireframe),
        "frozen" => Value::Flag(s.frozen),
        "viewURL" if s.located => Value::Withheld,
        "viewURL" => Value::Text(crate::steer::link(&s.view)),
        "tileCount" => Value::Integer(i64::try_from(s.tiles).unwrap_or(i64::MAX)),
        "settled" => Value::Flag(s.settled),
        "imagery" => s
            .imagery
            .map_or(Value::Missing, |l| Value::Text(l.key.clone())),
        "imageryName" => s
            .imagery
            .map_or(Value::Missing, |l| Value::Text(l.name.clone())),
        "imageryAttribution" => s
            .imagery
            .map_or(Value::Missing, |l| Value::Text(l.attribution.clone())),
        _ => return None,
    })
}

/// The position, short, for the title bar: the cheapest read-out there is,
/// for a person and for anything that can read a window's name.
///
/// Four decimals of a degree is eleven metres — what a title is for. And the
/// same rule as the properties: no position once it is the machine's own.
pub(crate) fn title(s: &Snapshot) -> String {
    if s.located {
        return "current location (position withheld)".to_owned();
    }
    let (lat, lon) = (s.view.lat, s.view.lon);
    let altitude = if s.view.altitude >= 10_000.0 {
        format!("{:.0} km", s.view.altitude / 1000.0)
    } else {
        format!("{:.0} m", s.view.altitude)
    };
    format!(
        "{:.4}°{} {:.4}°{} · {altitude}",
        lat.abs(),
        if lat < 0.0 { 'S' } else { 'N' },
        lon.abs(),
        if lon < 0.0 { 'W' } else { 'E' },
    )
}

static CURRENT: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Replaces the published snapshot. The render loop's, once a frame.
pub(crate) fn publish(snapshot: Snapshot) {
    if let Ok(mut current) = CURRENT.lock() {
        *current = Some(snapshot);
    }
}

/// The last snapshot published; `None` before the first frame.
pub(crate) fn current() -> Option<Snapshot> {
    CURRENT.lock().ok().and_then(|current| *current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        let view = StartView {
            lon: -4.0167,
            lat: 5.3364,
            altitude: 1500.0,
            heading: 45.0,
            pitch: 30.0,
        };
        let controller = CameraController::new(view.camera());
        let layer: &'static crate::embed::ImageryChoice = Box::leak(Box::new(
            crate::embed::ImageryChoice::new("labels", "Aerial with labels", 3, "© someone"),
        ));
        Snapshot {
            wireframe: true,
            tiles: 42,
            settled: true,
            imagery: Some(layer),
            ..Snapshot::of(&controller, (1600, 1000))
        }
    }

    fn real(s: &Snapshot, key: &str) -> f64 {
        match property(s, key) {
            Some(Value::Real(x)) => x,
            other => unreachable!("{key} should be a real, got {other:?}"),
        }
    }

    /// The eye reads back as it was placed, and the target is where the
    /// geometry says: ahead along the heading, at the range a 30° pitch from
    /// 1500 m gives — three kilometres, on a planet this size.
    #[test]
    fn the_properties_are_the_view() {
        let s = snapshot();
        assert!((real(&s, "longitude") - -4.0167).abs() < 1e-9);
        assert!((real(&s, "latitude") - 5.3364).abs() < 1e-9);
        assert!((real(&s, "altitude") - 1500.0).abs() < 1e-3);
        assert!((real(&s, "heading") - 45.0).abs() < 1e-6);
        assert!((real(&s, "pitch") - 30.0).abs() < 1e-6);
        assert!((real(&s, "fieldOfView") - 35.0).abs() < 1e-9);
        let range = real(&s, "range");
        assert!((range - 3000.0).abs() < 5.0, "range {range}");
        assert!(real(&s, "targetHeight").abs() < 1e-3);
        // North-east of the eye, since the heading is 45°.
        assert!(real(&s, "targetLongitude") > -4.0167);
        assert!(real(&s, "targetLatitude") > 5.3364);
        assert_eq!(property(&s, "viewWidth"), Some(Value::Integer(1600)));
        assert_eq!(property(&s, "viewHeight"), Some(Value::Integer(1000)));
        assert_eq!(property(&s, "wireframe"), Some(Value::Flag(true)));
        assert_eq!(property(&s, "frozen"), Some(Value::Flag(false)));
        assert_eq!(property(&s, "tileCount"), Some(Value::Integer(42)));
        assert_eq!(property(&s, "settled"), Some(Value::Flag(true)));
        assert_eq!(
            property(&s, "viewURL"),
            Some(Value::Text(crate::steer::link(&s.view)))
        );
        assert_eq!(property(&s, "password"), None);
    }

    /// Every key in the table answers, with the type the table says.
    #[test]
    fn every_listed_property_answers_with_its_own_type() {
        let s = snapshot();
        for (key, kind, _) in PROPERTIES {
            let got = match property(&s, key) {
                Some(Value::Real(_)) => Kind::Real,
                Some(Value::Integer(_)) => Kind::Integer,
                Some(Value::Flag(_)) => Kind::Flag,
                Some(Value::Text(_)) => Kind::Text,
                other => unreachable!("{key} answered {other:?}"),
            };
            assert_eq!(got, kind, "{key}");
        }
    }

    /// Centred on the current location: what says where is withheld — all
    /// five of them — and what does not, still answers.
    #[test]
    fn a_view_on_the_current_location_does_not_say_where_it_is() {
        let s = Snapshot {
            located: true,
            ..snapshot()
        };
        for key in [
            "longitude",
            "latitude",
            "targetLongitude",
            "targetLatitude",
            "viewURL",
        ] {
            assert_eq!(property(&s, key), Some(Value::Withheld), "{key}");
        }
        for key in ["altitude", "heading", "pitch", "range", "targetHeight"] {
            assert!(matches!(property(&s, key), Some(Value::Real(_))), "{key}");
        }
        let title = title(&s);
        assert!(title.contains("withheld"), "{title}");
        assert!(
            !title.contains("5.33") && !title.contains("4.01"),
            "{title}"
        );
    }

    #[test]
    fn a_view_of_the_sky_has_no_target() {
        let view = StartView {
            pitch: 0.0,
            altitude: 5000.0,
            ..StartView::default()
        };
        let s = Snapshot::of(&CameraController::new(view.camera()), (800, 600));
        for key in ["targetLongitude", "targetLatitude", "targetHeight", "range"] {
            assert_eq!(property(&s, key), Some(Value::Missing), "{key}");
        }
        assert!(matches!(property(&s, "longitude"), Some(Value::Real(_))));
    }

    #[test]
    fn the_title_says_where_in_a_form_a_person_reads() {
        assert_eq!(title(&snapshot()), "5.3364°N 4.0167°W · 1500 m");
        let high = Snapshot {
            view: StartView::default(),
            ..snapshot()
        };
        assert_eq!(title(&high), "46.0000°N 2.0000°E · 2000 km");
    }

    /// A script reads the layer three ways — the key it would set, the name
    /// a person reads, whose pictures they are — and reads nothing where
    /// there is no layer.
    #[test]
    fn the_imagery_properties_are_the_draped_layers() {
        let s = snapshot();
        let none = Snapshot {
            imagery: None,
            ..snapshot()
        };
        let text = |key: &str| property(&s, key);
        assert_eq!(text("imagery"), Some(Value::Text("labels".into())));
        assert_eq!(
            text("imageryName"),
            Some(Value::Text("Aerial with labels".into()))
        );
        assert_eq!(
            text("imageryAttribution"),
            Some(Value::Text("© someone".into()))
        );
        for key in ["imagery", "imageryName", "imageryAttribution"] {
            assert_eq!(property(&none, key), Some(Value::Missing), "{key}");
        }
        // Only the key is a script's to set.
        let writable: Vec<&str> = PROPERTIES
            .iter()
            .filter(|(key, _, writable)| *writable && key.starts_with("imagery"))
            .map(|(key, _, _)| *key)
            .collect();
        assert_eq!(writable, ["imagery"]);
    }

    /// The dictionary file and this table are the same list: a property in
    /// one and not the other is either a script error or dead vocabulary.
    #[test]
    fn the_dictionary_names_exactly_these_properties() {
        let sdef = include_str!("../macos/Tuile.sdef");
        let extension = sdef
            .split("<class-extension")
            .nth(1)
            .and_then(|rest| rest.split("</class-extension>").next())
            .expect("the dictionary extends the application class");
        let mut named: Vec<(&str, bool)> = extension
            .split("<property ")
            .skip(1)
            .map(|property| {
                let key = property
                    .split("<cocoa key=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .expect("every property names its key");
                let writable = property
                    .split('>')
                    .next()
                    .is_some_and(|tag| tag.contains("access=\"rw\""));
                (key, writable)
            })
            .collect();
        let mut listed: Vec<(&str, bool)> = PROPERTIES.iter().map(|(k, _, w)| (*k, *w)).collect();
        named.sort_unstable();
        listed.sort_unstable();
        assert_eq!(named, listed);
    }
}
