// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A line of text, as pixels: laid out and rasterised here, drawn by
//! whatever backend the host has.
//!
//! The widgets around it hand over triangles; text is the one thing a
//! triangle list draws badly, so a line comes out as a **coverage mask** —
//! one byte a pixel, how much of it the glyphs cover — with where it goes and
//! in which colour ([`Label`]). A backend uploads the mask once as a
//! one-channel texture and draws it as one quad, for as long as the line does
//! not change.
//!
//! The glyphs are rasterised by `ab_glyph` from one bundled font, Inter
//! (SIL Open Font License 1.1, `fonts/Inter-OFL.txt`): a browser has no
//! system font a wasm module can read, and the same font everywhere is the
//! same line everywhere.

use std::sync::OnceLock;

use ab_glyph::{Font as _, FontRef, GlyphId, PxScale, ScaleFont as _};

/// The bundled font: Inter Regular, 4.1.
static INTER: &[u8] = include_bytes!("../fonts/Inter-Regular.ttf");

/// What a line too long for its room ends with.
const ELLIPSIS: char = '\u{2026}';

/// A font text is laid out and rasterised with.
#[derive(Debug, Clone)]
pub struct Font {
    font: FontRef<'static>,
}

/// One line, rasterised: `width × height` bytes, row by row from the top,
/// each how much of that pixel the glyphs cover (0 none, 255 all of it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// A mask placed on screen: its top-left corner in pixels (origin top-left,
/// y down, as [`crate::Vertex`]), drawn one mask pixel to one screen pixel,
/// in a straight RGBA colour whose alpha the coverage multiplies.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub mask: Mask,
    pub position: [f32; 2],
    pub color: [f32; 4],
}

/// A line made to fit a width: the text as it will be drawn — the whole of
/// it, or its beginning and an ellipsis — and the size it is drawn at.
#[derive(Debug, Clone, PartialEq)]
pub struct Fitted {
    pub text: String,
    pub size: f32,
}

impl Font {
    /// The font bundled with this crate, parsed once.
    pub fn bundled() -> &'static Font {
        static FONT: OnceLock<Font> = OnceLock::new();
        FONT.get_or_init(|| {
            Font::from_static(INTER).expect("the bundled font is a valid TrueType font")
        })
    }

    /// A TrueType or OpenType font from its bytes.
    pub fn from_static(bytes: &'static [u8]) -> Result<Self, String> {
        FontRef::try_from_slice(bytes)
            .map(|font| Self { font })
            .map_err(|e| e.to_string())
    }

    /// The scale for an em of `size` pixels. `ab_glyph` scales by the
    /// line's height, not the em, and its own `pt_to_px_scale` counts
    /// typographic points at 96 dpi: neither is a pixel em.
    fn scale(&self, size: f32) -> PxScale {
        let em = self.font.units_per_em().unwrap_or(1000.0);
        PxScale::from(size.max(0.0) * self.font.height_unscaled() / em)
    }

    /// Whether the font has a glyph for `c`, rather than its "missing" box.
    pub fn has(&self, c: char) -> bool {
        self.font.glyph_id(c) != GlyphId(0)
    }

    /// How wide `text` is at an em of `size` pixels.
    pub fn width(&self, text: &str, size: f32) -> f32 {
        let font = self.font.as_scaled(self.scale(size));
        let mut width = 0.0;
        let mut previous: Option<GlyphId> = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = previous {
                width += font.kern(p, id);
            }
            width += font.h_advance(id);
            previous = Some(id);
        }
        width
    }

    /// How tall a line is at an em of `size` pixels: from the highest ascent
    /// to the lowest descent, in whole pixels.
    pub fn line_height(&self, size: f32) -> u32 {
        let font = self.font.as_scaled(self.scale(size));
        (font.ascent() - font.descent()).ceil().max(1.0) as u32
    }

    /// `text` on one line at an em of `size` pixels, as a coverage mask as
    /// wide as the line and [`Font::line_height`] tall, the baseline at the
    /// font's ascent.
    pub fn rasterize(&self, text: &str, size: f32) -> Mask {
        let scale = self.scale(size);
        let font = self.font.as_scaled(scale);
        let height = self.line_height(size);
        // A pixel either side: antialiasing reaches past the advances.
        let width = (self.width(text, size).ceil() as u32 + 2).max(1);
        let mut coverage = vec![0u8; (width * height) as usize];
        let baseline = font.ascent().ceil();
        let mut pen = 1.0;
        let mut previous: Option<GlyphId> = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = previous {
                pen += font.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, ab_glyph::point(pen, baseline));
            pen += font.h_advance(id);
            previous = Some(id);
            let Some(outlined) = self.font.outline_glyph(glyph) else {
                continue;
            };
            let bounds = outlined.px_bounds();
            let (left, top) = (bounds.min.x as i64, bounds.min.y as i64);
            outlined.draw(|x, y, c| {
                let (px, py) = (left + i64::from(x), top + i64::from(y));
                if px < 0 || py < 0 || px >= i64::from(width) || py >= i64::from(height) {
                    return;
                }
                let i = (py as u32 * width + px as u32) as usize;
                // Two glyphs that touch share a pixel's coverage.
                let sum = u16::from(coverage[i]) + (c.clamp(0.0, 1.0) * 255.0).round() as u16;
                coverage[i] = sum.min(255) as u8;
            });
        }
        Mask {
            width,
            height,
            coverage,
        }
    }

    /// `text` made to fit `max_width` pixels: at `size` if it fits there;
    /// else smaller, down to `min_size`; and if it is still too wide at
    /// `min_size`, its beginning at `min_size` followed by an ellipsis.
    pub fn fit(&self, text: &str, size: f32, min_size: f32, max_width: f32) -> Fitted {
        let min_size = min_size.min(size);
        let width = self.width(text, size);
        if width <= max_width {
            return Fitted {
                text: text.to_owned(),
                size,
            };
        }
        // The width is proportional to the size: the size that fits, a hair
        // under so the rounding of advances cannot tip it over.
        let shrunk = size * max_width / width * 0.999;
        if shrunk >= min_size {
            return Fitted {
                text: text.to_owned(),
                size: shrunk,
            };
        }
        // As many characters as fit before the ellipsis, the separators at
        // the cut dropped.
        let mut kept = String::new();
        let mut best = String::new();
        for c in text.chars() {
            kept.push(c);
            let candidate = format!("{}{ELLIPSIS}", kept.trim_end_matches([' ', ';', ',', '·']));
            if self.width(&candidate, min_size) > max_width {
                break;
            }
            best = candidate;
        }
        if best.is_empty() && self.width(&ELLIPSIS.to_string(), min_size) <= max_width {
            best = ELLIPSIS.to_string();
        }
        Fitted {
            text: best,
            size: min_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled font reads, and has what a map's credits are written in:
    /// accents, the copyright sign, the ellipsis this crate cuts with.
    #[test]
    fn the_bundled_font_has_what_credits_are_written_in() {
        let font = Font::bundled();
        for c in "AaZz09©éèàçôüß…;·,./-".chars() {
            assert!(font.has(c), "no glyph for {c:?}");
        }
    }

    /// A line is as tall as the font's line and as wide as its advances, and
    /// its glyphs cover pixels fully at their stems and not at all around
    /// them.
    #[test]
    fn a_line_is_its_glyphs_coverage() {
        let font = Font::bundled();
        let mask = font.rasterize("Google; Airbus", 14.0);
        assert_eq!(mask.height, font.line_height(14.0));
        assert!(mask.height >= 14 && mask.height <= 20, "{}", mask.height);
        let advances = font.width("Google; Airbus", 14.0);
        assert_eq!(mask.width, advances.ceil() as u32 + 2);
        assert_eq!(mask.coverage.len(), (mask.width * mask.height) as usize);
        assert_eq!(mask.coverage.iter().copied().max(), Some(255));
        // The first and last columns are the margin, never inked.
        for y in 0..mask.height {
            let row = &mask.coverage[(y * mask.width) as usize..((y + 1) * mask.width) as usize];
            assert_eq!(row[0], 0);
            assert_eq!(row[row.len() - 1], 0);
        }
        // An empty line is empty.
        let empty = font.rasterize("", 14.0);
        assert!(empty.coverage.iter().all(|&c| c == 0));
    }

    /// A line that fits is left alone; one a little too wide is drawn
    /// smaller; one far too wide is cut, at the smallest size, with an
    /// ellipsis — and never comes out wider than the room.
    #[test]
    fn a_line_is_fitted_shrunk_then_cut() {
        let font = Font::bundled();
        let text = "Google; Airbus; Vexcel Imaging US, Inc.; Data SIO, NOAA, U.S. Navy, NGA, GEBCO";
        let wide = font.width(text, 14.0);

        let fits = font.fit(text, 14.0, 10.0, wide + 1.0);
        assert_eq!(fits.text, text);
        assert_eq!(fits.size, 14.0);

        let shrunk = font.fit(text, 14.0, 10.0, wide * 0.9);
        assert_eq!(shrunk.text, text);
        assert!(shrunk.size < 14.0 && shrunk.size >= 10.0);
        assert!(font.width(&shrunk.text, shrunk.size) <= wide * 0.9);

        let room = wide * 0.4;
        let cut = font.fit(text, 14.0, 10.0, room);
        assert_eq!(cut.size, 10.0);
        assert!(cut.text.ends_with(ELLIPSIS), "{}", cut.text);
        assert!(cut.text.starts_with("Google; Airbus"), "{}", cut.text);
        assert!(font.width(&cut.text, cut.size) <= room);
        // No separator just before the ellipsis.
        let before = cut.text.trim_end_matches(ELLIPSIS);
        assert!(!before.ends_with([' ', ';', ',']), "{}", cut.text);
    }
}
