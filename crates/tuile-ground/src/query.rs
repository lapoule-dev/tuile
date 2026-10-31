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
//!
//! # Complete over its bounds, whatever is resident
//!
//! A ground answers for every column of its bounds, always: an answer never
//! depends on what is in memory or on the order things were loaded, and there
//! is no "unknown yet" (unlike the camera's advisory, streaming ground
//! height). How it gets there is the implementation's business: it may load
//! or generate its tiles on demand and evict them invisibly — a procedural
//! source computing a tile on first query, caching it and recomputing it bit
//! for bit after evicting it; a disk-backed source paging tiles; edits kept
//! as a per-tile, versioned, ordered layer re-applied when a tile comes back
//! — as long as no answer can tell. It matters for large worlds: 128 km at
//! 8 m spacing is about 2.7·10⁸ samples, some 2 GB of `f64`. A memory budget
//! for such a source is an implementation setting, not part of this
//! contract.
//!
//! Reads are infallible: a source that cannot load or regenerate a tile has
//! no answer to give, and must not give a wrong one. Such a failure is
//! outside the contract and fatal to the ground — the host handles it as it
//! handles running out of memory, not as a query result.
//!
//! # Boundaries
//!
//! Solid is closed: a point exactly at a span's top (or bottom) is solid, a
//! span includes both ends, and a probe exactly at a surface has that surface
//! below it. A column on the max edge of the bounds is outside the ground.
//!
//! # Costs
//!
//! Every query's cost per item is bounded by the ground's size, never by the
//! values in the query: an implementation documents its bounds (see
//! [`crate::Heightfield`]). A batch costs the sum of its items.

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
    pub ranges: Vec<(usize, usize)>,
}

impl SpanBuffer {
    /// The spans of query `i`.
    pub fn of(&self, i: usize) -> &[Span] {
        let Some(&(a, b)) = self.ranges.get(i) else {
            return &[];
        };
        self.spans.get(a..b).unwrap_or(&[])
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.ranges.clear();
    }
}

/// A segment from `from` to `to`, **straight in the frame's coordinates**.
///
/// In a planar frame that is a straight line. In a geodetic frame it is
/// straight in longitude, latitude and height: a ray at constant height
/// follows the curved surface, where a true chord between the same ends would
/// sag below it by about `d²/8R` at its middle for a length `d` (`R` the
/// earth's radius): ≈ 2 m at 10 km, ≈ 49 m at 50 km. A line of sight longer
/// than a few kilometres in a geodetic frame is optimistic by that much.
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

/// Why a ground refused an edit.
///
/// [`Stale`](Self::Stale) and [`Gap`](Self::Gap) change nothing. The others
/// are refusals for the edit's content, the same on every replica: they
/// **consume** the edit's sequence number and change nothing else (see
/// [`Self::consumes_seq`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GroundError {
    #[error("edit {got:?} is not after the last consumed edit {last:?}")]
    Stale { last: EditSeq, got: EditSeq },
    #[error(
        "edit {got:?} arrived while {expected:?} is expected: deliver the missing edits first"
    )]
    Gap { expected: EditSeq, got: EditSeq },
    #[error("the brush lies outside the ground")]
    OutOfBounds,
    /// A non-finite coordinate, a radius not positive, a box whose min is
    /// above its max, or a reach the frame's metric cannot measure.
    #[error("the brush is not a finite volume")]
    InvalidBrush,
}

impl GroundError {
    /// Whether the refused edit consumed its sequence number: the next
    /// expected edit is the one after it.
    pub fn consumes_seq(&self) -> bool {
        matches!(self, GroundError::OutOfBounds | GroundError::InvalidBrush)
    }
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
/// Inside them every query has its answer, independent of what the
/// implementation holds in memory (see the module's documentation).
pub trait Ground: Send + Sync {
    /// The frame the ground's source declared.
    fn frame(&self) -> &dyn Frame;

    /// The edit generation of an edit-level tile. A key at another level,
    /// or outside the grid, answers `Version(0)`.
    fn version(&self, tile: TileKey) -> Version;

    /// The water bodies; a body's [`WaterBodyId`](crate::WaterBodyId) is its
    /// index here.
    fn water_bodies(&self) -> &[WaterBody];

    /// The highest surface at or below each probe: where a walker dropped
    /// at the probe lands. `None` outside the bounds or with no surface
    /// below the probe (a probe inside solid ground has none). A probe at
    /// `+∞` gets the top of the column: the idiom for "the ground height
    /// here".
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
    /// does: a clear line of sight is `None`. A segment starting inside
    /// solid hits at fraction 0. Water does not block a ray. Outside the
    /// bounds there is no ground: the part of a segment beyond them is
    /// clear. The segment is straight in the frame's coordinates (see
    /// [`Ray`]).
    fn raycast(&self, rays: &[Ray], out: &mut [Option<Hit>]) -> Result<(), BatchMismatch>;

    /// The water over each column: its body, level and depth above the
    /// highest solid surface; `None` on dry land.
    fn water(&self, columns: &[Column], out: &mut [Option<WaterAt>]) -> Result<(), BatchMismatch>;
}

/// The authoritative ground: write side.
pub trait EditGround: Ground {
    /// The last consumed edit, `EditSeq(0)` on a fresh ground; the next
    /// edit must carry the number after it.
    fn last_seq(&self) -> EditSeq;

    /// Applies `edit` if it carries the next sequence number (see
    /// [`crate::edit`] for the order and which refusals consume the number).
    ///
    /// An applied edit appends one [`TileChanged`] per edit-level tile whose
    /// answers may have changed, in tile order, after bumping its version.
    /// The report is complete: a point query (`surface`, `spans`, `solid`,
    /// `water`) on a column of an unreported tile answers bit for bit what it
    /// answered before, and a ray changes only if it crosses a reported
    /// tile. It may be conservative: a reported tile's answers may be
    /// unchanged. An edit that changes nothing reports nothing. A refused
    /// edit appends nothing.
    fn apply(&mut self, edit: &Edit, changed: &mut Vec<TileChanged>) -> Result<(), GroundError>;

    /// A digest of everything that decides the ground's answers and its
    /// order (the frame, the state, the last consumed edit). Two replicas of
    /// one implementation agree on it iff their states agree (for practical
    /// purposes), on every target. Digests of different implementations are
    /// not comparable.
    ///
    /// It covers the whole state, resident or not. An implementation that
    /// pages or regenerates its tiles may compute it from the identity of
    /// its source (what regenerates the tiles bit for bit) and its edit
    /// layer (the consumed edits, in order), without loading anything.
    fn digest(&self) -> u64;
}

pub(crate) fn check(queries: usize, answers: usize) -> Result<(), BatchMismatch> {
    if queries == answers {
        Ok(())
    } else {
        Err(BatchMismatch { queries, answers })
    }
}
