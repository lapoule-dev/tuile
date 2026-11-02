// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod credits;
mod nav;
mod spinner;
#[cfg(feature = "statusbar")]
mod statusbar;
mod text;

pub use credits::{BarLayout, CreditBar};
pub use nav::{NavWidget, Part};
pub use spinner::{Spinner, SPOKES};
pub use text::{Fitted, Font, Label, Mask};

/// A 2-D vertex in **pixel** coordinates, with a straight (non-premultiplied)
/// RGBA colour.
///
/// The origin is the top-left of the viewport and y grows downward — the
/// convention every windowing system reports cursors in, so hit-testing and
/// drawing share one space with no flip in between.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex {
    pub fn new(x: f64, y: f64, color: [f32; 4]) -> Self {
        Self {
            position: [x as f32, y as f32],
            color,
        }
    }
}

/// Triangles to draw, in submission order. Later triangles paint over earlier
/// ones; nothing is depth-tested.
pub type Mesh = Vec<Vertex>;

#[cfg(feature = "statusbar")]
pub use statusbar::StatusBar;
