// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod edit;
pub mod frame;
pub mod heightfield;
pub mod query;
pub mod water;

pub use edit::{Brush, Edit, EditOp, EditSeq, TileChanged};
pub use frame::{
    tiles_over, Column, Frame, Geodetic, GeodeticFrame, InvalidFrame, PlanarFrame, Position,
    TileKey, Version, EARTH_RADIUS, MAX_LEVEL,
};
pub use heightfield::{Heightfield, InvalidGround};
pub use query::{
    BatchMismatch, EditGround, Ground, GroundError, Hit, Ray, Span, SpanBuffer, Surface,
};
pub use water::{WaterAt, WaterBody, WaterBodyId, WaterKind};
