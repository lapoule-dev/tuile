// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The frames worth looking at: where the sky or the ground changed
//! brightness from one picture to the next by more than a bound, where the
//! stars thinned out at once, where ground came out black.
//!
//! A flicker is a change of brightness the camera does not explain. The
//! bounds are on the change between two consecutive pictures, so a slow
//! sunset or a steady zoom passes, and a frame that steps does not; and two
//! pictures are compared only when the camera between them barely moved —
//! turned less than [`Bounds::max_turn_deg`], climbed less than
//! [`Bounds::max_climb`] of its altitude — and they are at most
//! [`Bounds::max_gap`] frames apart, the scene's clock moved less than
//! [`Bounds::max_scene_step`] between them; and a step is flagged only when
//! it stands out from its neighbours' ([`Bounds::trend`]). What the camera's
//! motion or the time of day explains is not a flicker.

use crate::Row;

/// What counts as worth looking at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// The largest change of the sky's, or the ground's, mean luma (0–1)
    /// from one picture to the next.
    pub luma_step: f64,
    /// The largest share of the last picture's stars that may go out at
    /// once, counted only where it had at least [`Self::min_stars`].
    pub star_drop: f64,
    pub min_stars: u64,
    /// Regions of fewer pixels than this are not compared: a sliver of
    /// sky at the top of a ground view has a mean that means nothing.
    pub min_pixels: u64,
    /// The most frames two compared pictures may be apart: a recording
    /// that dropped pictures compares across the gap only this far.
    pub max_gap: u64,
    /// The most the camera may turn between two compared pictures, degrees.
    pub max_turn_deg: f64,
    /// The most its altitude may change between them, as a share of it.
    pub max_climb: f64,
    /// The most the scene's clock may move between them, seconds: an
    /// accelerated clock changes the light on its own.
    pub max_scene_step: f64,
    /// How many times its neighbours' median step a step must be to stand
    /// out from the trend.
    pub trend: f64,
    /// Ground pixels at the clear colour a frame may have before it is
    /// flagged: a few pixels of night ground under 8 of 255 are dark, not
    /// missing.
    pub min_black: u64,
}

impl Default for Bounds {
    /// A step of 3 % of full scale (8 of 255), half the stars at once out
    /// of ten or more, regions of a thousand pixels.
    fn default() -> Self {
        Self {
            luma_step: 0.03,
            star_drop: 0.5,
            min_stars: 10,
            min_pixels: 1000,
            max_gap: 2,
            max_turn_deg: 0.5,
            max_climb: 0.02,
            max_scene_step: 900.0,
            trend: 3.0,
            min_black: 16,
        }
    }
}

/// Why a frame was flagged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Why {
    /// The sky's mean luma stepped by this much.
    Sky(f64),
    /// The ground's mean luma stepped by this much.
    Ground(f64),
    /// The stars went from the first count to the second.
    Stars(u64, u64),
    /// Ground pixels at the clear colour.
    Black(u64),
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sky(d) => write!(f, "sky luma {d:+.3}"),
            Self::Ground(d) => write!(f, "ground luma {d:+.3}"),
            Self::Stars(a, b) => write!(f, "stars {a} -> {b}"),
            Self::Black(n) => write!(f, "{n} black ground pixels"),
        }
    }
}

/// A flagged frame: its index in the rows, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Flagged {
    pub row: usize,
    pub why: Vec<Why>,
}

/// The frames of `rows` that break `bounds`, in order. Frames without a
/// picture are skipped; each picture is compared with the last one.
pub fn flag(rows: &[Row], bounds: &Bounds) -> Vec<Flagged> {
    // Each picture's step from the last one, where the two compare: the
    // sky's, the ground's, and the stars before and after.
    struct Step {
        row: usize,
        sky: Option<f64>,
        ground: Option<f64>,
        stars: Option<(u64, u64)>,
    }
    let mut steps = Vec::new();
    let mut last: Option<&Row> = None;
    for (i, row) in rows.iter().enumerate() {
        if !row.pictured {
            continue;
        }
        let mut step = Step {
            row: i,
            sky: None,
            ground: None,
            stars: None,
        };
        if let Some(prev) = last.filter(|prev| still(prev, row, bounds)) {
            let both = |a: u64, b: u64| a >= bounds.min_pixels && b >= bounds.min_pixels;
            step.sky = both(row.sky_pixels, prev.sky_pixels).then(|| row.sky_luma - prev.sky_luma);
            step.ground = both(row.ground_pixels, prev.ground_pixels)
                .then(|| row.ground_luma - prev.ground_luma);
            step.stars = Some((prev.stars, row.stars));
        }
        steps.push(step);
        last = Some(row);
    }
    // A step stands out from the trend when it is larger than
    // [`Bounds::trend`] times the median of its neighbours' — a sunset run
    // fast changes every picture, a flicker changes one.
    let trend = |n: usize, of: &dyn Fn(&Step) -> Option<f64>| -> f64 {
        let lo = n.saturating_sub(TREND_WINDOW);
        let hi = (n + TREND_WINDOW + 1).min(steps.len());
        let mut around: Vec<f64> = (lo..hi)
            .filter(|m| *m != n)
            .filter_map(|m| of(&steps[m]).map(f64::abs))
            .collect();
        if around.is_empty() {
            return 0.0;
        }
        around.sort_by(f64::total_cmp);
        around[around.len() / 2]
    };
    let mut out = Vec::new();
    for (n, step) in steps.iter().enumerate() {
        let row = &rows[step.row];
        let mut why = Vec::new();
        let stands_out = |d: f64, of: &dyn Fn(&Step) -> Option<f64>| {
            d.abs() > bounds.luma_step && d.abs() > bounds.trend * trend(n, of)
        };
        if let Some(d) = step.sky.filter(|d| stands_out(*d, &|s: &Step| s.sky)) {
            why.push(Why::Sky(d));
        }
        if let Some(d) = step.ground.filter(|d| stands_out(*d, &|s: &Step| s.ground)) {
            why.push(Why::Ground(d));
        }
        if let Some((before, after)) = step.stars {
            if before >= bounds.min_stars
                && (after as f64) < (1.0 - bounds.star_drop) * before as f64
            {
                why.push(Why::Stars(before, after));
            }
        }
        if row.black_ground >= bounds.min_black.max(1) {
            why.push(Why::Black(row.black_ground));
        }
        if !why.is_empty() {
            out.push(Flagged { row: step.row, why });
        }
    }
    out
}

/// Pictures on each side a step is held against.
const TREND_WINDOW: usize = 5;

/// Whether the camera barely moved from `a` to `b`, and they are close
/// enough in frames to be compared: see the module.
pub fn still(a: &Row, b: &Row, bounds: &Bounds) -> bool {
    let turn = |u: [f64; 3], v: [f64; 3]| {
        let (u, v) = (glam::DVec3::from_array(u), glam::DVec3::from_array(v));
        match (u.try_normalize(), v.try_normalize()) {
            (Some(u), Some(v)) => u.dot(v).clamp(-1.0, 1.0).acos().to_degrees(),
            _ => 0.0,
        }
    };
    let climb = (b.altitude - a.altitude).abs() / a.altitude.abs().max(1.0);
    b.frame.saturating_sub(a.frame) <= bounds.max_gap
        && turn(a.direction, b.direction) <= bounds.max_turn_deg
        && turn(a.up, b.up) <= bounds.max_turn_deg
        && climb <= bounds.max_climb
        // Unknown clocks compare: a plain tape has none.
        && !((b.instant - a.instant).abs() > bounds.max_scene_step)
}

/// The link that brings the viewer back to `row`'s view at its instant,
/// held: `tuile://goto?…&time=…&timeRate=0`. Without the clock when the
/// recording has none.
pub fn link(row: &Row) -> String {
    let mut url = format!(
        "tuile://goto?lon={:.7}&lat={:.7}&altitude={:.2}&heading={:.3}&pitch={:.3}",
        row.lon,
        row.lat,
        row.altitude,
        if row.heading > 359.9995 { 0.0 } else { row.heading },
        row.pitch.clamp(0.0, 90.0)
    );
    if row.instant.is_finite() {
        url.push_str(&format!(
            "&time={}&timeRate=0",
            tuile_atmosphere::clock::format_rfc3339(row.instant)
        ));
    }
    url
}

/// One line for a flagged frame: what it is, where, when, why, and the
/// link back.
pub fn line(row: &Row, why: &[Why]) -> String {
    let when = if row.instant.is_finite() {
        tuile_atmosphere::clock::format_rfc3339(row.instant)
    } else {
        "-".to_owned()
    };
    let why: Vec<String> = why.iter().map(ToString::to_string).collect();
    format!(
        "frame {:>6}  wall {:.3}  scene {when} (x{})  alt {:.0} m  hdg {:.1}  pitch {:.1}  \
         lon {:.5} lat {:.5}  | {}  | {}",
        row.frame,
        row.wall,
        row.rate,
        row.altitude,
        row.heading,
        row.pitch,
        row.lon,
        row.lat,
        why.join(", "),
        link(row)
    )
}
