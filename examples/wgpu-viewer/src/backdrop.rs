// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What is drawn behind the selection, so that ground is never bare.
//!
//! Two layers, both drawn without touching depth so the selection always covers
//! them: a whole-planet shell, the only thing that can cover ground before any
//! tile has arrived at all, and a complete coarse level of real tiles, which
//! turns a flat patch into blurry imagery. `CLAUDE.md` is the reason — a black
//! square on a globe is indistinguishable from a rendering fault.

/// The colour bare ground is painted.
///
/// A deep, desaturated ocean blue — the colour most of the planet is, and the
/// colour a missing patch is most likely to be surrounded by. It disappears
/// into the picture instead of shouting at it.
///
/// It was a bright green first, deliberately: while a hole was being chased,
/// something that says "no data here" at a glance is worth more than a pretty
/// frame. That trade is off now, and the cost of turning it off is worth
/// stating — the backstop still works, and it no longer announces itself, so a
/// gap that used to be obvious is now merely a slightly flat patch of sea.
/// `TUILE_SHELL=green` puts the marker back for a session spent diagnosing.
const BARE_GROUND: [f32; 4] = [0.09, 0.15, 0.24, 1.0];

/// The level drawn behind everything, complete, every frame.
///
/// One level covers the globe on its own — there is no need for the pyramid —
/// and the count is `2·4^level`, so this is the whole trade: level 3 is 128
/// draw calls and about 1.2 km per texel of imagery; level 4 is 512 and half
/// that. Three is enough to make a hole blurry rather than bare, which is all a
/// backstop owes.
///
/// It must not exceed the pinned floor, or the layer it draws is one eviction
/// can take away.
pub(crate) const BASE_LEVEL: u32 = 3;

/// The diagnostic colour, from `TUILE_SHELL=green`.
const MARKER_GROUND: [f32; 4] = [0.35, 0.72, 0.32, 1.0];

pub(crate) fn shell_content() -> tuile_core::content::DecodedTileContent {
    let marker = std::env::var("TUILE_SHELL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("green"));
    tuile_terrain::globe_shell(if marker { MARKER_GROUND } else { BARE_GROUND })
}

pub(crate) fn shell_enabled() -> bool {
    !std::env::var("TUILE_SHELL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"))
}

