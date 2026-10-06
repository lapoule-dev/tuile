// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What an instrument is told.
//!
//! The render calls an [`Observer`] at three moments and takes nothing
//! back: an observer cannot change a film, only watch it being made.

use tuile_film::TileKey;
use tuile_radiometry::Grade;

/// An imagery tile, the first time the render reads it.
pub struct ImageryIn<'a> {
    pub level: u8,
    pub x: u32,
    pub y: u32,
    /// As the store holds it: an encoded picture.
    pub bytes: &'a [u8],
    /// The store has renewed it since the pack was baked.
    pub renewed: bool,
    /// The grade its level is composed with.
    pub grade: Grade,
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

/// Where time went, in milliseconds, step by step.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    /// Reading terrain tiles from the store (cache or network).
    pub read_terrain: f64,
    /// Reading imagery tiles from the store.
    pub read_imagery: f64,
    /// Reading payloads from the pack.
    pub read_pack: f64,
    /// Terrain tiles made into meshes.
    pub mesh: f64,
    /// Imagery decoded and laid on the terrain's spacing.
    pub decode: f64,
    /// Textures written to the GPU.
    pub upload: f64,
    /// Drapes queued for composition, meshes entered.
    pub enter: f64,
    /// The frame recorded and submitted.
    pub draw: f64,
    /// Waiting for the GPU and reading the picture back.
    pub readback: f64,
    /// Tiles let go.
    pub leave: f64,
    /// The observers.
    pub observe: f64,
    /// Handing the picture to the sink (for an encoder on its own thread:
    /// the wait for room, not the encoding).
    pub sink: f64,
}

impl Timings {
    /// Every step, named, in the order a frame goes through them.
    pub fn steps(&self) -> [(&'static str, f64); 12] {
        [
            ("read terrain", self.read_terrain),
            ("read imagery", self.read_imagery),
            ("read pack", self.read_pack),
            ("build meshes", self.mesh),
            ("decode imagery", self.decode),
            ("upload textures", self.upload),
            ("enter tiles", self.enter),
            ("record frame", self.draw),
            ("GPU + readback", self.readback),
            ("leave tiles", self.leave),
            ("observers", self.observe),
            ("sink", self.sink),
        ]
    }

    pub fn total(&self) -> f64 {
        self.steps().iter().map(|s| s.1).sum()
    }

    pub fn add(&mut self, other: &Timings) {
        self.read_terrain += other.read_terrain;
        self.read_imagery += other.read_imagery;
        self.read_pack += other.read_pack;
        self.mesh += other.mesh;
        self.decode += other.decode;
        self.upload += other.upload;
        self.enter += other.enter;
        self.draw += other.draw;
        self.readback += other.readback;
        self.leave += other.leave;
        self.observe += other.observe;
        self.sink += other.sink;
    }
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
    /// Up to the picture's readback: what follows has not happened yet.
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
