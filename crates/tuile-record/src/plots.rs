// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The measures against the pose, as pictures: one panel a pair, each
//! picture's measure a dot, the flagged frames in red — so that "at which
//! altitudes does the sky flicker" is a look, not a query.
//!
//! Plain scatter plots drawn pixel by pixel: a panel's axes are labelled
//! with their name and their range, nothing more.

use crate::film::burn;
use crate::Row;

/// What a panel's horizontal axis reads.
#[derive(Debug, Clone, Copy)]
pub struct Axis {
    pub name: &'static str,
    pub of: fn(&Row) -> f64,
    /// Drawn on a log scale (the altitude spans five decades).
    pub log: bool,
}

/// The poses the measures are plotted against.
pub const POSES: [Axis; 5] = [
    Axis { name: "FRAME", of: |r| r.frame as f64, log: false },
    Axis { name: "ALT M", of: |r| r.altitude, log: true },
    Axis { name: "HDG", of: |r| r.heading, log: false },
    Axis { name: "PITCH", of: |r| r.pitch, log: false },
    Axis { name: "ORIGIN STEP M", of: |r| r.origin_step, log: true },
];

/// The measures plotted.
pub const MEASURES: [Axis; 5] = [
    Axis { name: "SKY LUMA", of: |r| if r.sky_pixels >= 1000 { r.sky_luma } else { f64::NAN }, log: false },
    Axis { name: "GROUND LUMA", of: |r| if r.ground_pixels >= 1000 { r.ground_luma } else { f64::NAN }, log: false },
    Axis { name: "DELTA", of: |r| r.delta, log: false },
    Axis { name: "STARS", of: |r| r.stars as f64, log: false },
    Axis { name: "SKY DELTA", of: |r| if r.sky_pixels >= 1000 { r.sky_delta } else { f64::NAN }, log: false },
];

const PANEL: (u32, u32) = (420, 240);
const MARGIN: u32 = 22;

fn value(axis: &Axis, row: &Row) -> Option<f64> {
    let v = (axis.of)(row);
    let v = if axis.log {
        (v > 0.0).then(|| v.log10())?
    } else {
        v
    };
    v.is_finite().then_some(v)
}

fn range(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let (lo, hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    if !lo.is_finite() {
        return None;
    }
    Some(if hi - lo < 1e-12 { (lo - 0.5, hi + 0.5) } else { (lo, hi) })
}

fn shown(axis: &Axis, v: f64) -> String {
    let v = if axis.log { 10f64.powf(v) } else { v };
    if v.abs() >= 1000.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

/// Every measure against every pose, a grid of panels, as RGBA8, and its
/// size. Rows without a picture are left out; `flagged` rows are red.
pub fn grid(rows: &[Row], flagged: &[usize]) -> (Vec<u8>, (u32, u32)) {
    let (cols, lines) = (POSES.len() as u32, MEASURES.len() as u32);
    let size = (PANEL.0 * cols, PANEL.1 * lines);
    let mut rgba: Vec<u8> = (0..size.0 * size.1)
        .flat_map(|_| [250, 250, 250, 255])
        .collect();
    let red: std::collections::HashSet<usize> = flagged.iter().copied().collect();
    let pictured: Vec<(usize, &Row)> = rows.iter().enumerate().filter(|(_, r)| r.pictured).collect();
    for (my, measure) in MEASURES.iter().enumerate() {
        for (px, pose) in POSES.iter().enumerate() {
            let at = (px as u32 * PANEL.0, my as u32 * PANEL.1);
            let points: Vec<(f64, f64, bool)> = pictured
                .iter()
                .filter_map(|(i, r)| Some((value(pose, r)?, value(measure, r)?, red.contains(i))))
                .collect();
            panel(&mut rgba, size, at, pose, measure, &points);
        }
    }
    (rgba, size)
}

fn panel(
    rgba: &mut [u8],
    size: (u32, u32),
    at: (u32, u32),
    x_axis: &Axis,
    y_axis: &Axis,
    points: &[(f64, f64, bool)],
) {
    let put = |rgba: &mut [u8], x: u32, y: u32, c: [u8; 3]| {
        if x < size.0 && y < size.1 {
            let i = ((y * size.0 + x) * 4) as usize;
            rgba[i..i + 3].copy_from_slice(&c);
        }
    };
    // The frame of the plot.
    let (x0, y0) = (at.0 + MARGIN, at.1 + MARGIN);
    let (x1, y1) = (at.0 + PANEL.0 - 6, at.1 + PANEL.1 - MARGIN);
    for x in x0..=x1 {
        put(rgba, x, y0, [180; 3]);
        put(rgba, x, y1, [180; 3]);
    }
    for y in y0..=y1 {
        put(rgba, x0, y, [180; 3]);
        put(rgba, x1, y, [180; 3]);
    }
    let (Some((xl, xh)), Some((yl, yh))) = (
        range(points.iter().map(|p| p.0)),
        range(points.iter().map(|p| p.1)),
    ) else {
        label(rgba, size, (x0, at.1 + 4), &format!("{}: NO DATA", y_axis.name));
        return;
    };
    // Unflagged first, so a red dot is never covered.
    for flagged in [false, true] {
        for &(x, y, red) in points.iter().filter(|p| p.2 == flagged) {
            let fx = (x - xl) / (xh - xl);
            let fy = (y - yl) / (yh - yl);
            let px = x0 + 2 + (fx * f64::from(x1 - x0 - 4)) as u32;
            let py = y1 - 2 - (fy * f64::from(y1 - y0 - 4)) as u32;
            let c = if red { [220, 30, 30] } else { [40, 70, 160] };
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                put(rgba, px + dx, py + dy, c);
            }
        }
    }
    label(
        rgba,
        size,
        (x0, at.1 + 4),
        &format!("{} {}..{}", y_axis.name, shown(y_axis, yl), shown(y_axis, yh)),
    );
    label(
        rgba,
        size,
        (x0, y1 + 5),
        &format!(
            "{}{} {}..{}",
            x_axis.name,
            if x_axis.log { " LOG" } else { "" },
            shown(x_axis, xl),
            shown(x_axis, xh)
        ),
    );
}

/// Text at `at`, in the caption's glyphs, on its own small band.
fn label(rgba: &mut [u8], size: (u32, u32), at: (u32, u32), text: &str) {
    let (w, h) = (text.chars().count() as u32 * 4 + 2, 7);
    let mut band = vec![0u8; (w * h * 4) as usize];
    for p in band.chunks_exact_mut(4) {
        p.copy_from_slice(&[90, 90, 90, 255]);
    }
    burn(&mut band, (w, h), text);
    for y in 0..h {
        for x in 0..w {
            let (tx, ty) = (at.0 + x, at.1 + y);
            if tx < size.0 && ty < size.1 {
                let s = ((y * w + x) * 4) as usize;
                let d = ((ty * size.0 + tx) * 4) as usize;
                // Only the glyphs: white in the band, dark text on the plot.
                if band[s] == 255 {
                    rgba[d..d + 3].copy_from_slice(&[20, 20, 20]);
                }
            }
        }
    }
}
