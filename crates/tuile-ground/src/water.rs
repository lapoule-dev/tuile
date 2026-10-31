// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Water as data: bodies, each at its own level.
//!
//! A sea is one body, a lake another at a different height. The ground tells,
//! per column, which body covers it (if any); the water surface is that
//! body's level, and the depth is the level minus the highest solid surface.
//! A renderer draws the same bodies, so the shoreline it shows is the one a
//! simulation walks to.
//!
//! Coverage is part of the ground's source, not of its edits: digging below
//! a body's level beside it does not flood the hole, and raising land in a
//! lake only makes the column dry where the land rises above the level.

/// A water body: its index in
/// [`Ground::water_bodies`](crate::query::Ground::water_bodies), the one
/// place a body is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WaterBodyId(pub u32);

/// What kind of water: a hint for rendering and rules, not for queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaterKind {
    Sea,
    Lake,
    River,
}

/// One body of water with a flat surface. Its id is its index in the
/// ground's list of bodies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterBody {
    pub kind: WaterKind,
    /// The surface's height, metres; finite.
    pub level: f64,
}

/// The water over one column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterAt {
    pub body: WaterBodyId,
    pub level: f64,
    /// Level minus the highest solid surface of the column; positive.
    pub depth: f64,
}
