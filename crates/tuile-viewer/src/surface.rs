// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! 3D Tiles drawn over the globe: a host's tileset — a mesh of the whole
//! surface, buildings and trees and the ground under them — on top of the
//! terrain and imagery.
//!
//! Each tileset is a geometry server of its own, fed by the host's fetcher
//! ([`tuile_planetary::Sources::tileset`]) and drained by a pump of its own,
//! and drawn as a surface that **replaces** the ground where it is drawn
//! (`tuile_wgpu::TileRenderer::paint_replacing`): never fought for a pixel by
//! the globe's own tiles, and where it has nothing yet the globe is there as
//! it would be without it.
//!
//! Its credits are the provider's terms, not decoration. What each tile
//! names in its own payload is gathered from the tiles drawn, most frequent
//! first, followed by what the host names, and shown in one line in the
//! credits bar below the picture for as long as the tileset is in the
//! session (`app::credits`).

use std::sync::Arc;

use tuile_core::protocol::InProcessStream;
use tuile_planetary::Credit;
use tuile_wgpu::ContentPump;

/// A tileset that is up: its stream and what must be shown with it.
pub(crate) struct Surface {
    /// The asset number it was asked for by.
    pub asset: i64,
    pub stream: InProcessStream,
    /// Its server's budget signals, which its pump records frames into.
    pub meter: tuile_core::runtime::SharedMeter,
    /// What the host says must be credited while it is drawn.
    pub credits: Vec<Credit>,
}

/// A tileset on its way up, after a person switched it on: the surface, or
/// why it could not be opened.
pub(crate) type Opening = tokio::sync::oneshot::Receiver<Result<Surface, String>>;

/// A tileset in the window: its session and its pump, and what its last
/// frame's tiles credited.
pub(crate) struct Layer {
    pub surface: Surface,
    pub pump: ContentPump,
    /// The `copyright` of each tile drawn in the last frame.
    pub drawn: Vec<Arc<str>>,
    /// Whether anything of it was drawn in the last frame.
    pub showing: bool,
}

impl Layer {
    pub(crate) fn new(surface: Surface, render_origin: glam::DVec3) -> Self {
        let mut pump = ContentPump::new(render_origin);
        pump.observe(Arc::clone(&surface.meter));
        Self {
            surface,
            pump,
            drawn: Vec::new(),
            showing: false,
        }
    }
}

/// How a tileset is traversed: at the screen-space error 3D Tiles content
/// is authored for — the specification's customary 16 pixels, against which
/// a tileset's geometric errors are chosen — and otherwise as the globe is
/// (its culling).
///
/// Not the globe's own threshold: a terrain's errors are the quantized
/// mesh's, a tileset's are its author's, and asking the second for the
/// first's single pixel loads many times the tiles the screen can show,
/// fills the class's share of the budget well before the view is complete,
/// and leaves the rest of the view coarse.
// The native session's: a page opens no tileset yet.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn traversal(globe: &tuile_core::traversal::Config) -> tuile_core::traversal::Config {
    tuile_core::traversal::Config {
        cull: globe.cull,
        horizon_culling: globe.horizon_culling,
        ..tuile_core::traversal::Config::default()
    }
}

/// A tileset's budget: the globe's, with the 3D Tiles mesh class given the
/// room the globe's own mesh class has.
///
/// A tileset's server holds that one class and nothing else, and the
/// interactive budget sizes it as a guest beside a ground (a sixth of the
/// device): a photographic mesh of a city fills that before the view is
/// complete. It is the ground here, so it gets the ground mesh's limits and
/// its loads in flight.
// The native session's: a page opens no tileset yet.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn budget(globe: &tuile_core::budget::BudgetConfig) -> tuile_core::budget::BudgetConfig {
    use tuile_core::budget::Class;
    let mut b = globe.clone();
    let (from, to) = (Class::TerrainMesh, Class::Tiles3dMesh);
    b.hard_limit[to] = globe.hard_limit[from];
    b.target[to] = globe.target[from];
    b.overflow[to] = globe.overflow[from];
    b.max_in_flight[to] = globe.max_in_flight[from].max(globe.max_in_flight[to]);
    b
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

    /// A tileset is traversed at the 3D Tiles convention's error, not the
    /// globe's, and its mesh class is given the ground mesh's room.
    #[test]
    fn a_tileset_is_traversed_and_budgeted_as_the_ground() {
        use tuile_core::budget::{BudgetConfig, Bytes, Class};
        let globe = tuile_core::traversal::Config {
            maximum_screen_space_error: 1.0,
            cull: false,
            ..tuile_core::traversal::Config::interactive_globe()
        };
        let mine = traversal(&globe);
        assert_eq!(mine.maximum_screen_space_error, 16.0);
        assert!(!mine.cull, "the globe's culling is kept");
        let ground = BudgetConfig::interactive(Bytes::mib(4096));
        let b = budget(&ground);
        assert_eq!(
            b.hard_limit[Class::Tiles3dMesh],
            ground.hard_limit[Class::TerrainMesh]
        );
        assert_eq!(
            b.target[Class::Tiles3dMesh],
            ground.target[Class::TerrainMesh]
        );
        assert!(b.max_in_flight[Class::Tiles3dMesh] >= ground.max_in_flight[Class::TerrainMesh]);
        assert_eq!(b.hard_limit[Class::Drape], ground.hard_limit[Class::Drape]);
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
