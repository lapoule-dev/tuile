// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground contract: the traits every ground implements, and their batched
//! queries.
//!
//! Queries come in slices and answer into caller-owned slices of the same
//! length: a simulation asks about ten thousand columns per tick without one
//! allocation, and an implementation is free to sort, cache or vectorise
//! internally as long as each answer depends only on its own query and the
//! ground's state. Everything is expressed in the ground's [`Frame`].
//!
//! The model is 3D-ready from the start: a column holds a list of solid
//! **spans** (a cave or a bridge makes two), surfaces are the tops of spans,
//! and edits are volume operations. A heightfield is the special case of one
//! span per column, which is what [`crate::Heightfield`] implements.

use glam::DVec3;
use thiserror::Error;

use crate::edit::{Edit, EditSeq, TileChanged};
use crate::frame::{Column, Frame, Position, TileKey, Version};
use crate::water::{WaterAt, WaterBody};

/// A surface something can stand on: the top of a solid span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Surface {
    /// Metres along the column.
    pub height: f64,
    /// The unit normal in a local east-north-up frame at the column (metres
    /// on every axis, whatever the frame's units).
    pub normal: DVec3,
}

/// A solid interval of a column, bottom and top included.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub bottom: f64,
    pub top: f64,
}

/// The spans of many columns, packed: the spans of query `i` are
/// `spans[ranges[i].0 .. ranges[i].1]`, bottom to top.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanBuffer {
    pub spans: Vec<Span>,
    pub ranges: Vec<(u32, u32)>,
}

impl SpanBuffer {
    /// The spans of query `i`.
    pub fn of(&self, i: usize) -> &[Span] {
        let Some(&(a, b)) = self.ranges.get(i) else {
            return &[];
        };
        let (a, b) = (
            usize::try_from(a).unwrap_or(0),
            usize::try_from(b).unwrap_or(0),
        );
        self.spans.get(a..b).unwrap_or(&[])
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.ranges.clear();
    }
}

/// A segment from `from` to `to`, straight in the frame's coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub from: Position,
    pub to: Position,
}

/// Where a segment first enters solid ground.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub at: Position,
    /// How far along the segment, `0..=1`.
    pub fraction: f64,
}

/// Why a ground refused an edit. Nothing changed when it did.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GroundError {
    #[error("edit {got:?} is not after the last applied edit {last:?}")]
    OutOfOrder { last: EditSeq, got: EditSeq },
    #[error("the brush lies outside the ground")]
    OutOfBounds,
    #[error("the brush is not finite")]
    NotFinite,
}

/// Why a batch could not be answered: `out` is not as long as the queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{queries} queries for {answers} answer slots")]
pub struct BatchMismatch {
    pub queries: usize,
    pub answers: usize,
}

/// The authoritative ground: read side.
///
/// Every method answers a batch: `out[i]` is the answer to `queries[i]`, and
/// `out` must be as long as the queries (else [`BatchMismatch`], nothing
/// written). A column outside the frame's bounds answers `None` (or no span).
pub trait Ground: Send + Sync {
    /// The frame the ground's source declared.
    fn frame(&self) -> &dyn Frame;

    /// The edit generation of an edit-level tile.
    fn version(&self, tile: TileKey) -> Version;

    /// The water bodies, by id.
    fn water_bodies(&self) -> &[WaterBody];

    /// The highest surface at or below each probe: where a walker dropped
    /// at the probe lands. `None` outside the bounds or with no surface
    /// below the probe (a probe inside solid ground has none).
    fn surface(
        &self,
        probes: &[Position],
        out: &mut [Option<Surface>],
    ) -> Result<(), BatchMismatch>;

    /// The solid spans of each column, bottom to top, replacing `out`'s
    /// content.
    fn spans(&self, columns: &[Column], out: &mut SpanBuffer);

    /// Whether each point is inside solid ground (collision, volume tests).
    fn solid(&self, points: &[Position], out: &mut [bool]) -> Result<(), BatchMismatch>;

    /// Where each segment first enters solid ground, `None` when it never
    /// does: a clear line of sight is `None`.
    fn raycast(&self, rays: &[Ray], out: &mut [Option<Hit>]) -> Result<(), BatchMismatch>;

    /// The water over each column: its body, level and depth above the
    /// highest solid surface; `None` on dry land.
    fn water(&self, columns: &[Column], out: &mut [Option<WaterAt>]) -> Result<(), BatchMismatch>;
}

/// The authoritative ground: write side.
pub trait EditGround: Ground {
    /// Applies `edit` if its sequence number comes after the last applied
    /// one, appending one [`TileChanged`] per edit-level tile its brush
    /// covers, in tile order. On error nothing changes and nothing is
    /// appended.
    fn apply(&mut self, edit: &Edit, changed: &mut Vec<TileChanged>) -> Result<(), GroundError>;
}

pub(crate) fn check(queries: usize, answers: usize) -> Result<(), BatchMismatch> {
    if queries == answers {
        Ok(())
    } else {
        Err(BatchMismatch { queries, answers })
    }
}
