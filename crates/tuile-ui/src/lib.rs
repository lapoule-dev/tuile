// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-ui
//!
//! On-screen controls for a globe viewer, with no renderer attached.
//!
//! A widget here does three things and draws nothing: it says **where** its
//! parts sit for a given viewport, it says **what a pixel hits**, and it turns
//! a gesture into calls on a [`tuile_camera::CameraController`]. Geometry comes
//! out as plain 2-D triangles in pixel coordinates ([`Vertex`]), which any
//! backend can upload — wgpu today, a canvas or an SVG tomorrow.
//!
//! Keeping the widget out of the render backend is what lets the same control
//! work on every façade, and keeping it out of `tuile-camera` is what keeps
//! that crate to camera state and gestures. This crate is pure: no I/O, no
//! graphics API, wasm-clean.

mod nav;

pub use nav::{NavWidget, Part};

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
