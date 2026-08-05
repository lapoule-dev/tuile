// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-atmosphere
//!
//! The air around the planet: where the sun is, what the air between the eye and
//! the ground does to the ground's colour, and what the shell of air seen
//! against space looks like at a given hour.
//!
//! ## Why this is its own crate
//!
//! It is not part of the tile engine. `tuile-core` streams and decodes geometry;
//! nothing about scattering coefficients or solar ephemerides belongs to that
//! job, and putting them there would mean every consumer of the core — a server,
//! a WASM worker, a USD exporter — carried an atmosphere model it never asks a
//! question of.
//!
//! Nor is it part of any renderer. The physics is the same for wgpu, for a
//! WebGL facade and for a Hydra delegate; only the *pass* differs. Two backends
//! quietly disagreeing about the colour of air is exactly the kind of bug that
//! surfaces as "the wgpu one looks different" and takes a day to place.
//!
//! So: renderer-agnostic, engine-agnostic, and consumed by both. It computes
//! parameters; a backend draws with them.
//!
//! ## The three pieces
//!
//! - [`sun`] — where the sun is at a stated instant. Everything else is
//!   downstream of it, which is why a globe lit "at 18:00 UTC" needs this rather
//!   than a direction someone picked by eye.
//! - [`aerial`] — the air between the eye and the ground. A correction applied to
//!   a surface's colour, and the strongest depth cue a globe has.
//! - [`sky`] — the air with nothing behind it but space. A colour of its own,
//!   drawn as a shell, turning over from blue through orange to dark with the
//!   hour.
//!
//! ## Not here yet
//!
//! **Clouds.** Deliberately absent rather than stubbed. A convincing cloud layer
//! is a volumetric problem — a density field, a march through it, and a lighting
//! model that is not the one above — and it shares almost nothing with the
//! closed-form work here beyond the sun direction. It wants its own module and
//! its own pass, and shipping a flat billboard in the meantime would be worse
//! than shipping nothing, because it would look answered.
//!
//! ## Precision
//!
//! Positions crossing to a shader are in **render space** — relative to the
//! render origin the tiles were rebased onto — and only ever narrow to f32 after
//! the subtraction. This is the same anti-jitter protocol the geometry uses, and
//! for the same reason: an ECEF position in f32 is good to a few metres, and a
//! haze that moves by metres between frames reads as a shimmer across the whole
//! scene.

pub mod aerial;
pub mod sky;
pub mod sun;

pub use aerial::AerialPerspective;
pub use sky::SkyShell;
pub use sun::Sun;
