// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Radiometric harmonisation of imagery tiles.
//!
//! The tiles of an imagery pyramid do not agree in colour. A level is a
//! mosaic of captures — another sensor, another season, another exposure —
//! and the level below it is another mosaic; where the source changes
//! between two levels, the same ground comes in two colours, and a globe
//! that draws both draws the boundary between them.
//!
//! # What is corrected, and what is kept
//!
//! A picture of ground is a **tone** — what is left when it is blurred: this
//! is dark green, that is pale ochre — and **detail**, everything the blur
//! removes. Two captures of one place differ almost only in tone. So a tile
//! is given the tone of a reference and keeps its own detail: a smooth field
//! of gains, a few numbers a tile, multiplied into its texels in linear
//! light.
//!
//! # The reference is the pyramid
//!
//! A tile and its parent cover the same ground entirely — the best overlap
//! there is. A tile's field is the gain that brings its tone to its parent's
//! ([`Transfer`]), plus the parent's own field ([`GainField::chained`]), down
//! from an anchor level that is left as it is. Where two levels are the same
//! source resampled, the gain is nothing and the field is exactly nothing
//! ([`Transfer::same_source`]), so a chain does not drift across the many
//! levels where nothing changes.
//!
//! # What it is not
//!
//! Not histogram matching (which repaints detail with the reference's
//! content), not a solve over a whole mosaic, and not a look: it makes tiles
//! agree, and says nothing about how the agreed picture should be graded.
//!
//! Everything here is a pure function of texels and parameters. This is the
//! reference: what a GPU computes is checked against it.

mod field;
mod grade;
mod levels;
mod stats;
mod transfer;

pub use field::{apply, GainField, LATTICE, STOPS_PER_UNIT};
pub use grade::{Grade, GradeReport, LevelGrades, Seen};
pub use levels::{apply_multipliers, tone_of, LevelGains, LevelParams, Observation, PairReport};
pub use stats::{linear_of, BlockStats, BLOCKS};
pub use transfer::{transfer, Params, Transfer};
