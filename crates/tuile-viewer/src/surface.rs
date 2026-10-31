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
//! Its credits are the provider's terms, not decoration. The ones the host
//! names — logos included — are kept on screen for as long as the tileset is
//! in the session; the ones each tile names in its own payload are gathered
//! from the tiles drawn, most frequent first, and shown with them.

use std::sync::Arc;

use tuile_core::protocol::InProcessStream;
use tuile_planetary::Credit;
use tuile_wgpu::{ContentPump, OverlayVertex};

/// A tileset that is up: its stream and what must be shown with it.
pub(crate) struct Surface {
    /// The asset number it was asked for by.
    pub asset: i64,
    pub stream: InProcessStream,
    /// Its server's budget signals, which its pump records frames into.
    pub meter: tuile_core::runtime::SharedMeter,
    /// What the host says must be credited while it is drawn.
    pub credits: Vec<Credit>,
    /// The logos among those credits, decoded, in the same order as the
    /// credits that carry one.
    pub logos: Vec<Logo>,
}

/// A credit's logo, as pixels: straight RGBA8, sRGB.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Logo {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

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

/// Decodes a logo the provider serves as an image.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn decode_logo(bytes: &[u8]) -> Result<Logo, String> {
    let image = image::load_from_memory(bytes)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    Ok(Logo {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

/// sRGB to linear: the frame is drawn in an sRGB format, so a colour handed
/// to the overlay is linear and the target encodes it.
fn linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// A logo as the overlay's triangles, `height` device pixels tall with its
/// lower-left corner at `(left, bottom)`.
///
/// The overlay draws coloured triangles and nothing else, which is all a
/// logo needs: each row's runs of one colour become one quad, and fully
/// transparent pixels none. A logo of a few thousand pixels is a few
/// thousand quads, uploaded with the controls.
pub(crate) fn logo_mesh(logo: &Logo, left: f32, bottom: f32, height: f32) -> Vec<OverlayVertex> {
    let mut out = Vec::new();
    if logo.width == 0 || logo.height == 0 {
        return out;
    }
    let scale = height / logo.height as f32;
    let top = bottom - height;
    let px = |x: u32, y: u32| {
        let i = ((y * logo.width + x) * 4) as usize;
        [
            logo.rgba[i],
            logo.rgba[i + 1],
            logo.rgba[i + 2],
            logo.rgba[i + 3],
        ]
    };
    for y in 0..logo.height {
        let mut x = 0;
        while x < logo.width {
            let colour = px(x, y);
            let mut end = x + 1;
            while end < logo.width && px(end, y) == colour {
                end += 1;
            }
            if colour[3] > 0 {
                let color = [
                    linear(colour[0]),
                    linear(colour[1]),
                    linear(colour[2]),
                    f32::from(colour[3]) / 255.0,
                ];
                let (x0, x1) = (left + x as f32 * scale, left + end as f32 * scale);
                let (y0, y1) = (top + y as f32 * scale, top + (y + 1) as f32 * scale);
                for position in [[x0, y0], [x1, y0], [x0, y1], [x0, y1], [x1, y0], [x1, y1]] {
                    out.push(OverlayVertex { position, color });
                }
            }
            x = end;
        }
    }
    out
}

/// Every logo of every tileset, side by side along the bottom left of the
/// safe area `(left, top, width, height)`, each `height_px` tall — the place
/// a map's credits are looked for.
pub(crate) fn logos_mesh(
    logos: &[&Logo],
    safe: (f64, f64, f64, f64),
    height_px: f32,
    margin: f32,
) -> Vec<OverlayVertex> {
    let (left, top, _, height) = safe;
    let bottom = (top + height) as f32 - margin;
    let start = left as f32 + margin;
    let mut x = start;
    let mut out = Vec::new();
    for logo in logos {
        out.extend(logo_mesh(logo, x, bottom, height_px));
        x += logo.width as f32 * height_px / logo.height.max(1) as f32 + margin;
    }
    if out.is_empty() {
        return out;
    }
    // Behind them, a translucent dark band: a logo drawn in white is
    // unreadable over snow or a sunlit roof, and a credit nobody can read is
    // not given.
    let pad = margin * 0.5;
    let (x0, x1) = (start - pad, x - margin + pad);
    let (y0, y1) = (bottom - height_px - pad, bottom + pad);
    let color = [0.0, 0.0, 0.0, 0.45];
    let band = [[x0, y0], [x1, y0], [x0, y1], [x0, y1], [x1, y0], [x1, y1]]
        .map(|position| OverlayVertex { position, color });
    band.into_iter().chain(out).collect()
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

    /// A logo becomes one quad per run of one colour in a row, placed and
    /// scaled as asked, and its transparent pixels become nothing.
    #[test]
    fn a_logo_is_its_runs_of_colour() {
        // Two rows of three: [red red clear] / [blue blue blue].
        let (r, b, c) = ([255, 0, 0, 255], [0, 0, 255, 255], [9, 9, 9, 0]);
        let logo = Logo {
            width: 3,
            height: 2,
            rgba: [r, r, c, b, b, b].concat(),
        };
        let mesh = logo_mesh(&logo, 10.0, 100.0, 20.0);
        assert_eq!(
            mesh.len(),
            12,
            "two quads: one run per row, the clear one dropped"
        );
        // Ten pixels a source pixel; the top row from y 80 to 90.
        let xs: Vec<f32> = mesh[..6].iter().map(|v| v.position[0]).collect();
        let ys: Vec<f32> = mesh[..6].iter().map(|v| v.position[1]).collect();
        assert_eq!(xs.iter().copied().fold(f32::MAX, f32::min), 10.0);
        assert_eq!(xs.iter().copied().fold(f32::MIN, f32::max), 30.0);
        assert_eq!(
            (
                ys.iter().copied().fold(f32::MAX, f32::min),
                ys.iter().copied().fold(f32::MIN, f32::max)
            ),
            (80.0, 90.0)
        );
        assert_eq!(mesh[0].color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(mesh[6].position[0], 10.0);
        assert_eq!(mesh[11].position, [40.0, 100.0]);
    }
}
