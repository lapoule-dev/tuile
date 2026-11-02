// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! 3D Tiles drawn over the globe: a host's tileset — a mesh of the whole
//! surface, buildings and trees and the ground under them — on top of the
//! terrain and imagery.
//!
//! Each tileset is a source of the session's one scene
//! ([`tuile_core::Composite`]): one geometry server, one budget, one stream
//! and one pump for the globe and every tileset over it. Switched on, it is
//! added to the scene under a tag of its own, traversed at the error its
//! author chose its geometric errors for ([`options`]); switched off, it is
//! removed, and the server lets go of what it held of it. It is drawn as a
//! surface that **replaces** the ground where it is drawn
//! (`tuile_wgpu::TileRenderer::paint_replacing`, the pump's
//! `Drawn::covering`): never fought for a pixel by the globe's own tiles,
//! and where it has nothing the globe is there — the ground under it kept,
//! a few levels coarser, as the fallback.
//!
//! Its credits are the provider's terms, not decoration. What each tile
//! names in its own payload is gathered from the tiles drawn, most frequent
//! first, followed by what the host names, and shown in one line in the
//! credits bar below the picture for as long as the tileset is in the
//! session (`app::credits`).

use std::sync::Arc;

use tuile_planetary::Credit;

/// A tileset that is up: a source of the session's scene, and what must be
/// shown with it. Dropped, it leaves the scene.
pub(crate) struct Surface {
    /// The asset number it was asked for by.
    pub asset: i64,
    /// The tag its tiles carry in the scene: what the window stops drawing
    /// the frame it is switched off (`ContentPump::hide_source`).
    pub tag: u8,
    /// What the host says must be credited while it is drawn.
    pub credits: Vec<Credit>,
    /// What takes it out of the scene: the session's.
    leave: Option<Leave>,
}

/// What takes a surface out of the scene: sent across threads on a desktop
/// and a phone, where it is opened on the runtime; a page's stays on its own.
#[cfg(not(target_arch = "wasm32"))]
type Leave = Box<dyn FnOnce() + Send>;
#[cfg(target_arch = "wasm32")]
type Leave = Box<dyn FnOnce()>;

impl Surface {
    /// The tileset `asset` with its `credits`, taken out of the scene by
    /// `leave` when it is dropped.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn new(
        asset: i64,
        tag: u8,
        credits: Vec<Credit>,
        leave: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            asset,
            tag,
            credits,
            leave: Some(Box::new(leave)),
        }
    }

    /// The tileset `asset` with its `credits`, taken out of the scene by
    /// `leave` when it is dropped.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn new(
        asset: i64,
        tag: u8,
        credits: Vec<Credit>,
        leave: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            asset,
            tag,
            credits,
            leave: Some(Box::new(leave)),
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(leave) = self.leave.take() {
            leave();
        }
    }
}

/// A tileset on its way up, after a person switched it on: the surface, or
/// why it could not be opened.
pub(crate) type Opening = tokio::sync::oneshot::Receiver<Result<Surface, String>>;

/// The error 3D Tiles content is authored for: the specification's
/// customary 16 pixels, against which a tileset's geometric errors are
/// chosen.
pub(crate) const TILESET_ERROR_PX: f64 = 16.0;

/// How a tileset joins the scene: traversed at [`TILESET_ERROR_PX`] whatever
/// the globe's threshold — a terrain's errors are the quantized mesh's, a
/// tileset's are its author's, and asking the second for the first's pixel or
/// two loads many times the tiles the screen can show — and standing for the
/// ground under it, which is then refined coarser, kept as the fallback.
pub(crate) fn options(globe: &tuile_core::traversal::Config) -> tuile_core::SourceOptions {
    tuile_core::SourceOptions {
        sse_factor: TILESET_ERROR_PX / globe.maximum_screen_space_error,
        covers_ground: true,
    }
}

/// The words a set of tilesets must show: what the tiles drawn credit, each
/// name once, the most frequent first — as a tile names several, separated by
/// `;` — then the host's own credits that are not already among them.
pub(crate) fn credit_line(credits: &[Credit], drawn: &[Arc<str>]) -> String {
    let mut counted: Vec<(String, usize)> = Vec::new();
    for copyright in drawn {
        for name in copyright
            .split(';')
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            match counted.iter_mut().find(|(seen, _)| seen == name) {
                Some((_, n)) => *n += 1,
                None => counted.push((name.to_owned(), 1)),
            }
        }
    }
    // Stable: equal counts keep the order they were first met in.
    counted.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let mut names: Vec<String> = counted.into_iter().map(|(name, _)| name).collect();
    for credit in credits {
        let text = credit.text.trim();
        if !text.is_empty() && !names.iter().any(|n| n == text) {
            names.push(text.to_owned());
        }
    }
    names.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credit(text: &str) -> Credit {
        Credit {
            text: text.into(),
            logo: None,
            link: None,
        }
    }

    /// A tileset is traversed at the 3D Tiles convention's error, whatever
    /// the globe's, and stands for the ground under it. (Its budget is the
    /// scene's: no clone of the globe's, no share of its own.)
    #[test]
    fn a_tileset_is_traversed_at_its_error_and_covers_the_ground() {
        for globe in [1.0, 2.0, 4.0] {
            let config = tuile_core::traversal::Config {
                maximum_screen_space_error: globe,
                ..tuile_core::traversal::Config::interactive_globe()
            };
            let o = options(&config);
            assert_eq!(o.sse_factor * globe, 16.0);
            assert!(o.covers_ground);
        }
    }

    /// A surface leaves the scene when it is dropped, once.
    #[test]
    fn a_surface_leaves_the_scene_when_dropped() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let left = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&left);
        let surface = Surface::new(7, 1, Vec::new(), move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(left.load(Ordering::SeqCst), 0);
        drop(surface);
        assert_eq!(left.load(Ordering::SeqCst), 1);
    }

    /// The names the tiles credit, each once, most frequent first; then the
    /// host's own that are not already said.
    #[test]
    fn credits_are_gathered_most_frequent_first() {
        let drawn: Vec<Arc<str>> = [
            "Google",
            "Google;Data SIO, NOAA",
            "Airbus; Google",
            "Airbus;Google",
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        let line = credit_line(&[credit("Cesium ion"), credit("Google")], &drawn);
        assert_eq!(line, "Google; Airbus; Data SIO, NOAA; Cesium ion");
        assert_eq!(credit_line(&[credit("Cesium ion")], &[]), "Cesium ion");
        assert_eq!(credit_line(&[], &[]), "");
    }
}
