// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A status-bar readout of what is actually resident, broken down by level.
//!
//! The totals never settled an argument. Memory climbing all session looks
//! identical whether it is a thousand coarse tiles or twenty thousand deep ones,
//! and those have opposite fixes; "the ground is not sharp enough" looks the
//! same whether the imagery stopped at level 12 or arrived at 19 and was drawn
//! badly. The breakdown answers both at a glance, without a log to grep and
//! without stopping to reproduce anything.
//!
//! It lives in the menu bar rather than over the render because it is a thing to
//! consult, not a thing to watch: an overlay would be in the way of exactly the
//! picture it is there to explain.
//!
//! # Fixed rows, rewritten
//!
//! Every row is created once and its text rewritten. Rebuilding the menu each
//! second is simpler to write and wrong to use — the rebuild lands while the
//! menu is open and the row under the pointer moves.

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use tuile_core::metrics::{metrics, LEVELS};

/// How many levels get a row. Past this the rows would be permanently empty:
/// the imagery sources top out near 19 and the terrain a little below.
const ROWS: usize = 21;

/// A row for a level the metrics do not carry would read zero for ever, which
/// is noise pretending to be information. A build error rather than a test,
/// because the two constants are only meaningful against each other.
const _: () = assert!(
    ROWS <= LEVELS,
    "every row must correspond to a level the metrics actually track"
);

pub struct StatusBar {
    /// Held because dropping it removes the item from the bar.
    _tray: TrayIcon,
    terrain: Vec<MenuItem>,
    imagery: Vec<MenuItem>,
    summary: MenuItem,
    /// Where the eye is and which way it faces.
    ///
    /// Two lines rather than one because they answer different questions: a
    /// reproduction needs the *place*, and a bug that only appears under tilt
    /// needs the *posture*. Reading them off the screen is also the only way to
    /// tell someone else where to stand — a screenshot shows what went wrong
    /// and never where the camera was when it did.
    position: MenuItem,
    posture: MenuItem,
}

impl StatusBar {
    /// Builds the item and its menu.
    ///
    /// Returns `None` rather than failing the viewer: a status item is a
    /// convenience, and a platform that will not give one is not a reason to
    /// refuse to draw a globe.
    pub fn new() -> Option<Self> {
        let menu = Menu::new();
        let summary = MenuItem::new("no session yet", false, None);
        menu.append(&summary).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;

        let heading = |text: &str| MenuItem::new(text, false, None);
        menu.append(&heading("Camera")).ok()?;
        let position = MenuItem::new("  —", false, None);
        let posture = MenuItem::new("  —", false, None);
        menu.append(&position).ok()?;
        menu.append(&posture).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;

        menu.append(&heading("Terrain — meshes on the GPU")).ok()?;
        let terrain = Self::rows(&menu)?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&heading("Imagery — textures held, once each"))
            .ok()?;
        let imagery = Self::rows(&menu)?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&PredefinedMenuItem::quit(Some("Quit"))).ok()?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("tuile — resident tiles by level")
            .with_icon(globe_icon())
            .build()
            .ok()?;

        Some(Self {
            _tray: tray,
            terrain,
            imagery,
            summary,
            position,
            posture,
        })
    }

    fn rows(menu: &Menu) -> Option<Vec<MenuItem>> {
        let mut rows = Vec::with_capacity(ROWS);
        for _ in 0..ROWS {
            // Disabled: these are readouts, and a row that looks clickable
            // invites a click that does nothing.
            let item = MenuItem::new("", false, None);
            menu.append(&item).ok()?;
            rows.push(item);
        }
        Some(rows)
    }

    /// Writes where the eye is and how it is held.
    ///
    /// Longitude and latitude in decimal degrees to five places — about a metre
    /// at the equator, which is the resolution someone typing them back in
    /// needs. Altitude above the *ground*, not the ellipsoid, because that is
    /// the number a person recognises when they are looking at a hillside.
    ///
    /// Heading and pitch are the posture, and they are here because a bug that
    /// only appears under tilt cannot be reproduced from a position alone.
    /// `pitch` is degrees below horizontal: 90 is straight down.
    pub fn set_camera(&self, lon: f64, lat: f64, altitude: f64, heading: f64, pitch: f64) {
        let compass = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
        let point = compass[(((heading.to_degrees() + 22.5) / 45.0).floor() as usize) % 8];
        self.position.set_text(format!(
            "  {:.5}°, {:.5}°   {}",
            lat.to_degrees(),
            lon.to_degrees(),
            distance(altitude)
        ));
        self.posture.set_text(format!(
            "  heading {:.1}° {point}   pitch {:.1}°",
            heading.to_degrees(),
            pitch.to_degrees()
        ));
    }

    /// Rewrites every row from the current metrics. Call about once a second —
    /// often enough to watch a session drift, rarely enough to cost nothing.
    pub fn refresh(&self) {
        let m = metrics();

        let (mut terrain_tiles, mut terrain_bytes) = (0u64, 0u64);
        let (mut imagery_tiles, mut imagery_bytes) = (0u64, 0u64);

        for level in 0..ROWS {
            let level = level as u32;
            let count = m.meshes_by_level.get(level);
            let bytes = m.mesh_bytes_by_level.get(level);
            terrain_tiles += count;
            terrain_bytes += bytes;
            self.terrain[level as usize].set_text(row("z", level, count, bytes));

            let textures = m.textures_by_level.get(level);
            let texture_bytes = m.texture_bytes_by_level.get(level);
            imagery_tiles += textures;
            imagery_bytes += texture_bytes;
            self.imagery[level as usize].set_text(row("z", level, textures, texture_bytes));
        }

        // Resident on the left, outstanding on the right. The two together are
        // what separates "the ground is late" from "the ground is gone": a
        // count that will not come down is a stalled fetch, a count that stays
        // at zero while the picture is coarse is a traversal that never asked.
        self.summary.set_text(format!(
            "{terrain_tiles} meshes ({:.0} MiB) · {imagery_tiles} textures ({:.0} MiB) · \
             in flight {} mesh / {} texture · {} queued · {} evicted · {} cancelled",
            mib(terrain_bytes),
            mib(imagery_bytes),
            m.meshes_in_flight.get(),
            m.textures_in_flight.get(),
            m.pending_uploads.get(),
            m.tiles_evicted.get(),
            m.loads_cancelled.get(),
        ));
    }
}

/// One row: the level, what is held at it, and what that costs. Empty levels
/// read as a dash rather than as zeros, so the eye finds the occupied band.
/// An altitude a person reads without converting it.
///
/// Metres up close and kilometres from orbit: "35786000 m" is a number nobody
/// parses at a glance, and "0.2 km" hides the two hundred metres that matter
/// when the camera is about to touch a hillside.
fn distance(metres: f64) -> String {
    if metres.abs() < 10_000.0 {
        format!("{metres:.0} m")
    } else {
        format!("{:.1} km", metres / 1000.0)
    }
}

fn row(prefix: &str, level: u32, count: u64, bytes: u64) -> String {
    if count == 0 {
        return format!("  {prefix}{level:<2}  —");
    }
    format!("  {prefix}{level:<2}  {count:>6}   {:>7.1} MiB", mib(bytes))
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// A small filled disc, drawn rather than shipped.
///
/// An embedded PNG would be one more file to keep beside the binary and one more
/// thing to go missing; this is sixteen lines of arithmetic and cannot.
fn globe_icon() -> Icon {
    const SIZE: u32 = 32;
    let radius = SIZE as f32 / 2.0 - 1.0;
    let centre = SIZE as f32 / 2.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 + 0.5 - centre, y as f32 + 0.5 - centre);
            let distance = (dx * dx + dy * dy).sqrt();
            // One pixel of falloff at the rim, so it does not look bitten.
            let alpha = ((radius - distance).clamp(0.0, 1.0) * 255.0) as u8;
            // A little lighter toward the top left, which reads as a sphere
            // rather than a dot at sixteen points.
            let shade = (200.0 - (dx + dy) * 3.0).clamp(90.0, 245.0) as u8;
            rgba.extend_from_slice(&[shade, shade, shade, alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("a 32x32 RGBA buffer is a valid icon")
}
