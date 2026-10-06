// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What an instrument is told.
//!
//! The render calls an [`Observer`] at three moments and takes nothing
//! back: an observer cannot change a film, only watch it being made.

use tuile_film::TileKey;

/// An imagery tile, the first time the render reads it.
pub struct ImageryIn<'a> {
    pub level: u8,
    pub x: u32,
    pub y: u32,
    /// As the store holds it: an encoded picture.
    pub bytes: &'a [u8],
    /// The store has renewed it since the pack was baked.
    pub renewed: bool,
    /// What its level's colour is multiplied by at composition.
    pub gain: [f32; 3],
}

/// Where a tile's mesh and drape came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Built from the tile store, through the pack's references.
    Store,
    /// Carried by the pack.
    Pack,
}

/// A tile, as it enters the film.
pub struct TileIn<'a> {
    pub frame: u32,
    pub key: TileKey,
    pub origin: Origin,
    /// The imagery tiles of its drape, bottom first: level, x, y.
    pub imagery: &'a [(u8, u32, u32)],
}

/// Where a frame's time went, in milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    /// Reading tiles and payloads, cache or network.
    pub read: f64,
    /// Decoding them into meshes and textures.
    pub build: f64,
    /// Composing, drawing and reading the picture back.
    pub gpu: f64,
}

/// A picture, as it leaves.
pub struct FrameOut<'a> {
    /// The frame's number in the film.
    pub frame: u32,
    /// Its place among the frames rendered, from 0.
    pub index: u32,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, sRGB-encoded: what a viewer sees.
    pub rgba: &'a [u8],
    /// Tiles drawn, and how many of them entered with this frame.
    pub tiles: usize,
    pub entered: usize,
    /// Drape layers drawn, by imagery level.
    pub layers: &'a [(u8, u32)],
    pub timings: Timings,
}

/// An instrument on a render. Every method does nothing unless overridden.
pub trait Observer {
    fn imagery(&mut self, _tile: &ImageryIn<'_>) {}
    fn tile(&mut self, _tile: &TileIn<'_>) {}
    fn frame(&mut self, _frame: &FrameOut<'_>) {}
}

/// Watches nothing.
impl Observer for () {}

/// Several instruments on one render, each told in turn.
impl Observer for Vec<Box<dyn Observer>> {
    fn imagery(&mut self, tile: &ImageryIn<'_>) {
        self.iter_mut().for_each(|o| o.imagery(tile));
    }
    fn tile(&mut self, tile: &TileIn<'_>) {
        self.iter_mut().for_each(|o| o.tile(tile));
    }
    fn frame(&mut self, frame: &FrameOut<'_>) {
        self.iter_mut().for_each(|o| o.frame(frame));
    }
}
