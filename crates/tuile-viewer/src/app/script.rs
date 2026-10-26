// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A scripted hand, for a film of the gestures that can be made again.
//!
//! `TUILE_TOUCH_SCRIPT=tour` plays a fixed sequence of finger events into the
//! viewer once the first view is covered: a one-finger drag, three pinches
//! toward the ground, a two-finger tilt toward the horizon and a twist. The events
//! enter exactly where the window's own touches do — [`super::touch::Touches`]
//! and then the camera — so what is filmed is the gesture path itself, minus
//! only the system's delivery of the touch. It exists because a simulator
//! cannot be pinched from the command line, and a recording that needs a
//! steady hand is one nobody makes twice.
//!
//! Positions are fractions of the drawable, fixed when the script starts.

use std::time::Duration;
use web_time::Instant;

use super::touch::Phase;

/// One finger event: when, which finger, what, where (as fractions).
type Event = (f64, u64, Phase, (f64, f64));

/// How long to wait after the first view is covered before the first touch.
const AFTER_COVERED: Duration = Duration::from_millis(1500);

pub(super) struct TouchScript {
    events: Vec<Event>,
    next: usize,
    started: Option<Instant>,
}

impl TouchScript {
    /// The script `TUILE_TOUCH_SCRIPT` names, if any.
    pub(super) fn from_environment() -> Option<Self> {
        let name = std::env::var("TUILE_TOUCH_SCRIPT").ok()?;
        match name.trim() {
            "tour" => Some(Self {
                events: tour(),
                next: 0,
                started: None,
            }),
            other => {
                tracing::warn!(
                    "TUILE_TOUCH_SCRIPT={other:?} is not a script; the known one is \"tour\""
                );
                None
            }
        }
    }

    /// The events due by `now`, in device pixels for a drawable of `size`.
    /// Nothing until `covered` has been true for a moment.
    pub(super) fn due(
        &mut self,
        now: Instant,
        covered: bool,
        size: (f64, f64),
    ) -> Vec<(u64, Phase, (f64, f64))> {
        if self.started.is_none() && covered {
            self.started = Some(now + AFTER_COVERED);
            tracing::info!("touch script starts");
        }
        let Some(started) = self.started else {
            return Vec::new();
        };
        let Some(elapsed) = now.checked_duration_since(started) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(&(at, id, phase, (x, y))) = self.events.get(self.next) {
            if at > elapsed.as_secs_f64() {
                break;
            }
            out.push((id, phase, (x * size.0, y * size.1)));
            self.next += 1;
            if self.next == self.events.len() {
                tracing::info!("touch script over");
            }
        }
        out
    }
}

/// A finger moving in a straight line from `from` to `to` over `seconds`,
/// starting at `t`, in `steps` moves.
fn stroke(
    t: f64,
    seconds: f64,
    id: u64,
    from: (f64, f64),
    to: (f64, f64),
    steps: u32,
) -> Vec<Event> {
    let mut events = vec![(t, id, Phase::Started, from)];
    for i in 1..=steps {
        let k = f64::from(i) / f64::from(steps);
        events.push((
            t + seconds * k,
            id,
            Phase::Moved,
            (from.0 + (to.0 - from.0) * k, from.1 + (to.1 - from.1) * k),
        ));
    }
    events.push((t + seconds + 0.01, id, Phase::Ended, to));
    events
}

/// Two fingers at once, each on its own stroke, their moves interleaved as a
/// hand's are.
fn pair(
    t: f64,
    seconds: f64,
    a: ((f64, f64), (f64, f64)),
    b: ((f64, f64), (f64, f64)),
    steps: u32,
) -> Vec<Event> {
    let mut events: Vec<Event> = stroke(t, seconds, 1, a.0, a.1, steps)
        .into_iter()
        .chain(stroke(t + 0.001, seconds, 2, b.0, b.1, steps))
        .collect();
    events.sort_by(|x, y| x.0.total_cmp(&y.0));
    events
}

/// Twisting two fingers by `turn` radians about `centre`, at `radius`
/// (fractions of the width, both).
fn twist(
    t: f64,
    seconds: f64,
    centre: (f64, f64),
    radius: f64,
    turn: f64,
    steps: u32,
) -> Vec<Event> {
    let at = |a: f64, side: f64| {
        (
            centre.0 + side * radius * a.cos(),
            centre.1 + side * radius * a.sin() * 0.46,
        )
    };
    let mut events = vec![
        (t, 1, Phase::Started, at(0.0, 1.0)),
        (t + 0.001, 2, Phase::Started, at(0.0, -1.0)),
    ];
    for i in 1..=steps {
        let k = f64::from(i) / f64::from(steps);
        let when = t + seconds * k;
        events.push((when, 1, Phase::Moved, at(turn * k, 1.0)));
        events.push((when + 0.001, 2, Phase::Moved, at(turn * k, -1.0)));
    }
    events.push((t + seconds + 0.01, 1, Phase::Ended, at(turn, 1.0)));
    events.push((t + seconds + 0.02, 2, Phase::Ended, at(turn, -1.0)));
    events
}

/// The tour: a drag, three pinches toward the ground, a tilt toward the
/// horizon, a twist.
fn tour() -> Vec<Event> {
    let mut events = stroke(0.0, 1.2, 1, (0.55, 0.60), (0.45, 0.50), 24);
    let mut t = 2.5;
    for _ in 0..3 {
        // Fingers from close together to far apart: each pinch brings the
        // ground about five times closer.
        events.extend(pair(
            t,
            0.9,
            ((0.42, 0.5), (0.08, 0.5)),
            ((0.58, 0.5), (0.92, 0.5)),
            18,
        ));
        t += 2.5;
    }
    // Time for the ground to arrive.
    t += 6.0;
    // Both fingers up together: tip toward the horizon.
    events.extend(pair(
        t,
        1.5,
        ((0.38, 0.72), (0.38, 0.42)),
        ((0.62, 0.72), (0.62, 0.42)),
        30,
    ));
    t += 5.0;
    events.extend(twist(t, 2.0, (0.5, 0.55), 0.18, 1.0, 40));
    events.sort_by(|x, y| x.0.total_cmp(&y.0));
    events
}

#[cfg(test)]
mod tests {
    use super::super::touch::{Gesture, Touches};
    use super::*;

    /// The tour, played through the real recognizer, makes the gestures it
    /// says it makes — and zooms in by the ratio its pinches spread.
    #[test]
    fn the_tour_makes_the_gestures_it_names() {
        let t0 = Instant::now();
        let mut touches = Touches::default();
        let (mut drags, mut zoom, mut tilt, mut twist) = (0, 1.0, 0.0, 0.0);
        for (at, id, phase, (x, y)) in tour() {
            let now = t0 + Duration::from_secs_f64(at);
            for g in touches.on(now, id, phase, (x * 1179.0, y * 2556.0)) {
                match g {
                    Gesture::Drag { .. } => drags += 1,
                    Gesture::Pinch { ratio, .. } => zoom *= ratio,
                    Gesture::Tilt { radians } => tilt += radians,
                    Gesture::Twist { radians } => twist += radians,
                    _ => {}
                }
            }
        }
        assert!(drags > 10, "{drags}");
        assert!(zoom > 50.0, "three pinches zoomed {zoom}×");
        assert!(tilt > 0.5, "{tilt}");
        assert!(twist > 0.5, "{twist}");
        assert!(!touches.any(), "a finger was left down");
    }

    #[test]
    fn nothing_plays_before_the_view_is_covered() {
        let mut script = TouchScript {
            events: tour(),
            next: 0,
            started: None,
        };
        let t0 = Instant::now();
        assert!(script
            .due(t0 + Duration::from_secs(60), false, (100.0, 100.0))
            .is_empty());
        assert!(
            script.due(t0, true, (100.0, 100.0)).is_empty(),
            "plays after a pause"
        );
        let first = script.due(t0 + AFTER_COVERED, true, (100.0, 200.0));
        assert_eq!(first.len(), 1);
        let (id, phase, (x, y)) = first[0];
        assert_eq!((id, phase), (1, Phase::Started));
        assert!(
            (x - 55.0).abs() < 1e-9 && (y - 120.0).abs() < 1e-9,
            "{x},{y}"
        );
    }
}

/// A scripted steering, for a session that can be recorded again:
/// `TUILE_STEER_SCRIPT=<file>`, one line an action — seconds after the
/// first view is covered, then a steering URL (`tuile://goto?…&seconds=20`)
/// or `quit`. Blank lines and `#` comments are skipped.
///
/// The URLs go where every URL goes ([`crate::steer::Request::Url`]): a
/// script can do nothing a link cannot.
pub(super) struct SteerScript {
    lines: Vec<(f64, String)>,
    next: usize,
    started: Option<Instant>,
}

impl SteerScript {
    /// The script `TUILE_STEER_SCRIPT` names, if any.
    pub(super) fn from_environment() -> Option<Self> {
        let path = std::env::var("TUILE_STEER_SCRIPT").ok()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                tracing::error!(path, "cannot read the steering script: {e}");
                return None;
            }
        };
        match Self::parse(&text) {
            Ok(script) => Some(script),
            Err(why) => {
                tracing::error!(path, "the steering script is not read: {why}");
                None
            }
        }
    }

    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let mut lines = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (at, what) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("line {}: <seconds> <url|quit>", n + 1))?;
            let at: f64 = at
                .parse()
                .map_err(|_| format!("line {}: {at:?} is not seconds", n + 1))?;
            lines.push((at, what.trim().to_owned()));
        }
        lines.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(Self {
            lines,
            next: 0,
            started: None,
        })
    }

    /// Whether lines are still to come: the loop keeps turning for them.
    pub(super) fn pending(&self) -> bool {
        self.next < self.lines.len()
    }

    /// The lines due by `now`, once the first view is `covered`.
    pub(super) fn due(&mut self, now: Instant, covered: bool) -> Vec<String> {
        if !covered && self.started.is_none() {
            return Vec::new();
        }
        let started = *self.started.get_or_insert(now);
        let at = now.duration_since(started).as_secs_f64();
        let mut out = Vec::new();
        while let Some((when, what)) = self.lines.get(self.next) {
            if *when > at {
                break;
            }
            out.push(what.clone());
            self.next += 1;
        }
        out
    }
}

#[cfg(test)]
mod steer_tests {
    use super::*;

    #[test]
    fn a_steering_script_plays_its_lines_in_time_once_covered() {
        let mut script = SteerScript::parse(
            "# zoom out\n2 tuile://goto?altitude=1e8&seconds=20\n\n0 tuile://goto?lat=1\n30 quit\n",
        )
        .expect("reads");
        let t0 = Instant::now();
        assert!(script.due(t0, false).is_empty());
        assert_eq!(script.due(t0, true), vec!["tuile://goto?lat=1"]);
        assert_eq!(
            script.due(t0 + Duration::from_secs(3), true),
            vec!["tuile://goto?altitude=1e8&seconds=20"]
        );
        assert_eq!(script.due(t0 + Duration::from_secs(31), true), vec!["quit"]);
        assert!(SteerScript::parse("soon tuile://goto?lat=1").is_err());
    }
}
