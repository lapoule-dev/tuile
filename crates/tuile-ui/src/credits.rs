// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The credits bar: a strip along the bottom of the window, outside the
//! picture, with the line of credits the data on screen asks for centred in
//! it.
//!
//! Outside the picture because a credit is not part of the view: the host
//! gives the picture the window minus the bar ([`CreditBar::height`]), and
//! the bar is drawn in what is left — its background as triangles, its line
//! as a [`Label`]. On a phone the bar reaches the bottom edge of the screen
//! and its line stays within the safe area, clear of the home indicator and
//! the rounded corners.

use crate::text::{Font, Label};
use crate::{Mesh, Vertex};

/// The bar's background: a dark neutral grey, opaque — it is not over the
/// picture, and a line over the ground would have to fight the ground.
/// Linear, as the overlay's colours are (the frame encodes them as sRGB):
/// about `#1c1c1c` on screen.
pub const BACKGROUND: [f32; 4] = [0.0116, 0.0116, 0.0116, 1.0];
/// The line's colour: a light grey, read easily on the background without
/// shouting over the picture beside it.
pub const TEXT: [f32; 4] = [0.72, 0.72, 0.72, 1.0];

/// The credits bar's proportions, in points (device pixels at scale 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditBar {
    /// The line's em, when it fits.
    pub text_size: f64,
    /// The smallest em the line is shrunk to before it is cut.
    pub min_text_size: f64,
    /// The room above and below the line, and either side of it.
    pub padding: f64,
}

impl Default for CreditBar {
    fn default() -> Self {
        Self {
            text_size: 12.0,
            min_text_size: 9.0,
            padding: 5.0,
        }
    }
}

/// The bar as it is drawn on one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct BarLayout {
    /// How many device pixels of the frame's bottom the bar takes: the
    /// picture is the rest.
    pub height: u32,
    /// Its background, across the whole width of the frame.
    pub background: Mesh,
    /// Its line. `None` when not a character of it fits.
    pub label: Option<Label>,
}

impl CreditBar {
    /// How tall the band holding the line is, in device pixels, at `scale`
    /// device pixels a point: the line's height and the padding.
    fn band(&self, font: &Font, scale: f64) -> u32 {
        let line = font.line_height((self.text_size * scale) as f32);
        line + (2.0 * self.padding * scale).round() as u32
    }

    /// How many device pixels of a frame `frame` tall the bar takes, with the
    /// safe area `(left, top, width, height)` in device pixels: its band, and
    /// under it whatever the safe area leaves at the bottom of the screen.
    ///
    /// Never more than a third of the frame: a window dragged to a sliver
    /// keeps a picture.
    pub fn height(
        &self,
        font: &Font,
        frame: (u32, u32),
        safe: (f64, f64, f64, f64),
        scale: f64,
    ) -> u32 {
        let (_, top, _, height) = safe;
        let inset = (f64::from(frame.1) - (top + height)).max(0.0).round() as u32;
        (self.band(font, scale) + inset).min(frame.1 / 3)
    }

    /// The bar for `text` in a frame of `frame` device pixels, with the safe
    /// area `(left, top, width, height)` in device pixels and `scale` device
    /// pixels a point. `None` when there is nothing to credit: then there is
    /// no bar at all and the picture is the whole frame.
    pub fn layout(
        &self,
        font: &Font,
        text: &str,
        frame: (u32, u32),
        safe: (f64, f64, f64, f64),
        scale: f64,
    ) -> Option<BarLayout> {
        let text = text.trim();
        if text.is_empty() || frame.0 == 0 || frame.1 == 0 {
            return None;
        }
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let height = self.height(font, frame, safe, scale);
        let (w, h) = (f64::from(frame.0), f64::from(frame.1));
        let top = h - f64::from(height);
        let mut background = Mesh::new();
        quad(&mut background, (0.0, top), (w, h), BACKGROUND);

        // The line: centred on the safe area's width, inside it, in the band
        // above the bottom inset.
        let (safe_left, _, safe_width, _) = safe;
        let pad = self.padding * scale;
        let room = (safe_width.min(w) - 2.0 * pad).max(0.0);
        let fitted = font.fit(
            text,
            (self.text_size * scale) as f32,
            (self.min_text_size * scale) as f32,
            room as f32,
        );
        let label = (!fitted.text.is_empty()).then(|| {
            let mask = font.rasterize(&fitted.text, fitted.size);
            let band = f64::from(self.band(font, scale).min(height));
            let centre = safe_left + safe_width.min(w) / 2.0;
            let x = (centre - f64::from(mask.width) / 2.0).round();
            let y = (top + (band - f64::from(mask.height)) / 2.0).round();
            Label {
                mask,
                position: [x as f32, y as f32],
                color: TEXT,
            }
        });
        Some(BarLayout {
            height,
            background,
            label,
        })
    }
}

/// Two triangles covering `from`–`to`.
fn quad(mesh: &mut Mesh, from: (f64, f64), to: (f64, f64), color: [f32; 4]) {
    let (x0, y0, x1, y1) = (from.0, from.1, to.0, to.1);
    for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x0, y1), (x1, y0), (x1, y1)] {
        mesh.push(Vertex::new(x, y, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "Google; Airbus; Landsat / Copernicus; Cesium ion";

    /// Nothing to credit, no bar: the picture keeps the whole frame.
    #[test]
    fn no_credits_no_bar() {
        let font = Font::bundled();
        let whole = (0.0, 0.0, 1280.0, 800.0);
        assert_eq!(
            CreditBar::default().layout(font, "", (1280, 800), whole, 1.0),
            None
        );
        assert_eq!(
            CreditBar::default().layout(font, "  ", (1280, 800), whole, 1.0),
            None
        );
    }

    /// On a desktop the bar is the band along the bottom, the full width,
    /// and the line is centred in it, inside it.
    #[test]
    fn the_line_is_centred_along_the_bottom() {
        let font = Font::bundled();
        let bar = CreditBar::default()
            .layout(font, LINE, (1280, 800), (0.0, 0.0, 1280.0, 800.0), 1.0)
            .expect("a bar");
        assert!(bar.height >= 20 && bar.height <= 32, "{}", bar.height);
        let top = 800.0 - bar.height as f32;
        let xs: Vec<f32> = bar.background.iter().map(|v| v.position[0]).collect();
        let ys: Vec<f32> = bar.background.iter().map(|v| v.position[1]).collect();
        assert_eq!(xs.iter().copied().fold(f32::MAX, f32::min), 0.0);
        assert_eq!(xs.iter().copied().fold(f32::MIN, f32::max), 1280.0);
        assert_eq!(ys.iter().copied().fold(f32::MAX, f32::min), top);
        assert_eq!(ys.iter().copied().fold(f32::MIN, f32::max), 800.0);
        let label = bar.label.expect("a line");
        let [x, y] = label.position;
        let centre = x + label.mask.width as f32 / 2.0;
        assert!((centre - 640.0).abs() <= 1.0, "centred at {centre}");
        assert!(y >= top && y + label.mask.height as f32 <= 800.0);
        assert_eq!(label.color, TEXT);
    }

    /// Twice the pixels a point, twice the bar.
    #[test]
    fn the_bar_follows_the_display_density() {
        let font = Font::bundled();
        let one = CreditBar::default().height(font, (1280, 800), (0.0, 0.0, 1280.0, 800.0), 1.0);
        let two = CreditBar::default().height(font, (2560, 1600), (0.0, 0.0, 2560.0, 1600.0), 2.0);
        assert!((two as i64 - 2 * one as i64).abs() <= 2, "{one} then {two}");
    }

    /// On a phone the bar reaches the bottom edge, under the home
    /// indicator, and its line stays in the safe area.
    #[test]
    fn on_a_phone_the_line_stays_in_the_safe_area() {
        let font = Font::bundled();
        // 1179 × 2556 at 3 pixels a point, 59 pt off the top, 34 off the
        // bottom.
        let safe = (0.0, 177.0, 1179.0, 2556.0 - 177.0 - 102.0);
        let bar = CreditBar::default()
            .layout(font, LINE, (1179, 2556), safe, 3.0)
            .expect("a bar");
        let band = CreditBar::default().band(font, 3.0);
        assert_eq!(bar.height, band + 102);
        let label = bar.label.expect("a line");
        let bottom = label.position[1] + label.mask.height as f32;
        assert!(
            bottom <= (2556 - 102) as f32,
            "under the indicator: {bottom}"
        );
        assert!(label.position[0] >= 0.0);
        assert!(label.position[0] + label.mask.width as f32 <= 1179.0);
    }

    /// A line too long for the window is shrunk, then cut — never wider than
    /// the room.
    #[test]
    fn a_long_line_never_leaves_the_window() {
        let font = Font::bundled();
        let long = LINE.repeat(8);
        let bar = CreditBar::default()
            .layout(font, &long, (640, 480), (0.0, 0.0, 640.0, 480.0), 1.0)
            .expect("a bar");
        let label = bar.label.expect("a line");
        assert!(label.position[0] >= 0.0);
        assert!(label.position[0] + label.mask.width as f32 <= 640.0);
    }
}
