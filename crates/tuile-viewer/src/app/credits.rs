// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The credits bar: what the 3D Tiles on screen credit, in one line along
//! the bottom of the window, **outside** the picture.
//!
//! The bar takes its height off the bottom of the frame and the 3D view is
//! the rest: the camera's aspect, the traversal's viewport, picking and the
//! gestures all read [`App::viewport`], which is that rest, from the same
//! top-left origin as the cursor — so a pixel of the view means the same to
//! all of them. The bar is drawn by the overlay, after the picture: its
//! background as triangles, its line as a coverage mask (`tuile_ui`'s
//! [`tuile_ui::CreditBar`], the same on every shell, browser included).
//!
//! Only while a tileset with something to credit is in the session. Without
//! one there is no bar at all, its height is nought, and every number the
//! frame is made from is the one it was before the bar existed.

use super::App;

/// The bar as last laid out, and what it was laid out for: a line is
/// rasterised again only when its words, the frame, the safe area or the
/// density change.
#[derive(Default)]
pub(super) struct Credits {
    key: Option<Key>,
    layout: Option<tuile_ui::BarLayout>,
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    text: String,
    frame: (u32, u32),
    safe: (f64, f64, f64, f64),
    scale: f64,
}

impl Credits {
    /// The bar for `text` in this frame, laid out again only if something
    /// it depends on changed.
    pub(super) fn lay_out(
        &mut self,
        text: &str,
        frame: (u32, u32),
        safe: (f64, f64, f64, f64),
        scale: f64,
    ) {
        let key = Key {
            text: text.to_owned(),
            frame,
            safe,
            scale,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.layout = tuile_ui::CreditBar::default().layout(
            tuile_ui::Font::bundled(),
            text,
            frame,
            safe,
            scale,
        );
        self.key = Some(key);
    }

    /// How many device pixels of the frame's bottom the bar takes: nought
    /// without one.
    pub(super) fn height(&self) -> u32 {
        self.layout.as_ref().map_or(0, |bar| bar.height)
    }

    /// The bar as drawn, if there is one.
    pub(super) fn layout(&self) -> Option<&tuile_ui::BarLayout> {
        self.layout.as_ref()
    }
}

/// The 3D view in a frame of `frame` device pixels with a bar `bar` pixels
/// tall: the frame above the bar. A frame the bar would leave nothing of
/// keeps its whole height, and no bar is the frame itself.
pub(super) fn view_of(frame: (f64, f64), bar: u32) -> (f64, f64) {
    let (w, h) = frame;
    if bar == 0 || f64::from(bar) >= h {
        return frame;
    }
    (w, h - f64::from(bar))
}

impl App {
    /// Lays the credits bar out for this frame, from what the tilesets'
    /// last frame drew. Before anything reads the viewport.
    pub(super) fn lay_out_the_credits(&mut self) {
        let text = self.surface_credits().unwrap_or_default();
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let (size, safe, scale) = (active.size, active.safe, active.scale.max(1.0));
        let before = self.credits.height();
        self.credits.lay_out(&text, size, safe, scale);
        // A phone's and a page's caption stand above the bar.
        if self.credits.height() != before {
            crate::desktop::caption_lift(self.credits.height());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: (u32, u32) = (1280, 800);
    const WHOLE: (f64, f64, f64, f64) = (0.0, 0.0, 1280.0, 800.0);

    /// Nothing to credit — no tileset in the session — and the view is the
    /// whole frame, exactly as before the bar existed.
    #[test]
    fn without_credits_the_view_is_the_whole_frame() {
        let mut credits = Credits::default();
        credits.lay_out("", FRAME, WHOLE, 1.0);
        assert_eq!(credits.height(), 0);
        assert!(credits.layout().is_none());
        assert_eq!(view_of((1280.0, 800.0), credits.height()), (1280.0, 800.0));
    }

    /// With credits the view is the frame above the bar, and the bar is
    /// what is left below it.
    #[test]
    fn with_credits_the_view_stops_at_the_bar() {
        let mut credits = Credits::default();
        credits.lay_out("Google; Cesium ion", FRAME, WHOLE, 1.0);
        let bar = credits.height();
        assert!(bar > 0);
        let (w, h) = view_of((1280.0, 800.0), bar);
        assert_eq!(w, 1280.0);
        assert_eq!(h + f64::from(bar), 800.0);
        let layout = credits.layout().expect("a bar");
        let top = layout
            .background
            .iter()
            .map(|v| v.position[1])
            .fold(f32::MAX, f32::min);
        assert_eq!(f64::from(top), h, "the bar starts where the view ends");
        // And a tileset that goes takes the bar with it.
        credits.lay_out("", FRAME, WHOLE, 1.0);
        assert_eq!(credits.height(), 0);
    }

    /// A frame too small to hold the bar and a view keeps its view.
    #[test]
    fn a_sliver_of_a_frame_keeps_its_view() {
        assert_eq!(view_of((300.0, 20.0), 30), (300.0, 20.0));
    }
}
