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
//! [`Bounds::max_gap`] frames apart. What the camera's own motion explains
//! is not a flicker.

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
    let mut out = Vec::new();
    let mut last: Option<&Row> = None;
    for (i, row) in rows.iter().enumerate() {
        if !row.pictured {
            continue;
        }
        let mut why = Vec::new();
        if let Some(prev) = last.filter(|prev| still(prev, row, bounds)) {
            let both = |a: u64, b: u64| a >= bounds.min_pixels && b >= bounds.min_pixels;
            let sky = row.sky_luma - prev.sky_luma;
            if both(row.sky_pixels, prev.sky_pixels) && sky.abs() > bounds.luma_step {
                why.push(Why::Sky(sky));
            }
            let ground = row.ground_luma - prev.ground_luma;
            if both(row.ground_pixels, prev.ground_pixels) && ground.abs() > bounds.luma_step {
                why.push(Why::Ground(ground));
            }
            if prev.stars >= bounds.min_stars
                && (row.stars as f64) < (1.0 - bounds.star_drop) * prev.stars as f64
            {
                why.push(Why::Stars(prev.stars, row.stars));
            }
        }
        if row.black_ground > 0 {
            why.push(Why::Black(row.black_ground));
        }
        if !why.is_empty() {
            out.push(Flagged { row: i, why });
        }
        last = Some(row);
    }
    out
}

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
