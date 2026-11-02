// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The vocabulary of the budget: classes, bytes, time, generations, resource
//! identities and the platform signals.
//!
//! Everything here is an integer. Floats enter the budget only through
//! [`Need`](super::Need) and are quantised once, at the boundary, so no
//! ordering decision ever compares two floats.

use std::ops::{Index, IndexMut};

pub use crate::tile_id::TileId;

/// A kind of GPU resource, each with its own target, overflow and request
/// quota, so that one class cannot starve another (a 3D Tiles fly-over cannot
/// eat the ground's imagery).
///
/// Deliberately exhaustive: [`PerClass`] is a fixed array indexed by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// A terrain tile's mesh (vertex and index buffers, uniform, CPU copy).
    TerrainMesh,
    /// A 3D Tiles content mesh.
    Tiles3dMesh,
    /// A terrain tile's imagery at one level: the mosaic texture draped on it.
    Drape,
    /// Any other overlay texture.
    Overlay,
}

// Decision D3 (spec §10): no class for stand-ins. A wanted tile that is not
// drawable is shown by the draw set's roles — `Role::Fallback` (coarser
// ground under it) and `Role::Held` (finer ground kept on a zoom-out).

impl Class {
    /// Every class, in index order.
    pub const ALL: [Class; 4] = [
        Class::TerrainMesh,
        Class::Tiles3dMesh,
        Class::Drape,
        Class::Overlay,
    ];

    /// Whether it lies on the globe — a terrain mesh, a drape, an overlay —
    /// rather than standing on it or over it as a 3D Tiles mesh does. The
    /// floor ([`BudgetConfig::pinned_level`](super::BudgetConfig::pinned_level))
    /// and the coarse ground drawn first while ground is lost (P1.1) are the
    /// ground's: a tileset's coarsest tiles are no floor, and a tileset's
    /// tile not yet drawn holds no ground back.
    pub const fn is_ground(self) -> bool {
        !matches!(self, Class::Tiles3dMesh)
    }

    /// The class's slot in a [`PerClass`].
    pub const fn index(self) -> usize {
        match self {
            Class::TerrainMesh => 0,
            Class::Tiles3dMesh => 1,
            Class::Drape => 2,
            Class::Overlay => 3,
        }
    }
}

/// A length as a `u32`, saturating: `as` would wrap above `u32::MAX` on a
/// 64-bit target and not on wasm32, where a `usize` is 32 bits.
pub(crate) fn sat_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// One value per [`Class`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PerClass<T>(pub [T; 4]);

impl<T: Copy> PerClass<T> {
    /// The same value for every class.
    pub const fn splat(v: T) -> Self {
        PerClass([v; 4])
    }

    /// Builds each class's value from a function of the class.
    pub fn from_fn(mut f: impl FnMut(Class) -> T) -> Self {
        PerClass(Class::ALL.map(&mut f))
    }

    /// Iterates `(class, value)` in class order.
    pub fn iter(&self) -> impl Iterator<Item = (Class, T)> + '_ {
        Class::ALL.iter().map(move |c| (*c, self.0[c.index()]))
    }
}

impl<T> Index<Class> for PerClass<T> {
    type Output = T;
    fn index(&self, c: Class) -> &T {
        &self.0[c.index()]
    }
}

impl<T> IndexMut<Class> for PerClass<T> {
    fn index_mut(&mut self, c: Class) -> &mut T {
        &mut self.0[c.index()]
    }
}

/// A byte count. Arithmetic saturates: a budget never wraps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub u64);

impl Bytes {
    /// Zero bytes.
    pub const ZERO: Bytes = Bytes(0);
    /// The unbounded budget of [`Policy::Batch`].
    pub const MAX: Bytes = Bytes(u64::MAX);

    /// `n` KiB.
    pub const fn kib(n: u64) -> Self {
        Bytes(n.saturating_mul(1 << 10))
    }
    /// `n` MiB.
    pub const fn mib(n: u64) -> Self {
        Bytes(n.saturating_mul(1 << 20))
    }
    /// `n` GiB.
    pub const fn gib(n: u64) -> Self {
        Bytes(n.saturating_mul(1 << 30))
    }
    /// Saturating sum.
    pub const fn plus(self, o: Bytes) -> Bytes {
        Bytes(self.0.saturating_add(o.0))
    }
    /// Saturating difference.
    pub const fn minus(self, o: Bytes) -> Bytes {
        Bytes(self.0.saturating_sub(o.0))
    }
    /// `self × num / den`, in 128-bit, saturating; `den == 0` gives `MAX`.
    pub const fn scaled(self, num: u64, den: u64) -> Bytes {
        if den == 0 {
            return Bytes::MAX;
        }
        let v = (self.0 as u128) * (num as u128) / (den as u128);
        if v > u64::MAX as u128 {
            Bytes::MAX
        } else {
            // In range: checked just above (`try_from` is not `const`).
            #[allow(clippy::cast_possible_truncation)]
            Bytes(v as u64)
        }
    }
}

/// A point in time or a duration, in integer microseconds.
///
/// The spec sketches `Seconds`; an integer keeps time out of float rounding,
/// so a decision is a function of its inputs on every platform. Time only
/// ever enters as an input ([`FrameInput::now`](super::FrameInput)): the
/// budget never reads a clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Micros(pub u64);

impl Micros {
    /// The origin, or a zero duration.
    pub const ZERO: Micros = Micros(0);
    /// "Never": a deadline that does not come.
    pub const NEVER: Micros = Micros(u64::MAX);

    /// `ms` milliseconds.
    pub const fn from_millis(ms: u64) -> Self {
        Micros(ms.saturating_mul(1_000))
    }
    /// `s` seconds.
    pub const fn from_secs(s: u64) -> Self {
        Micros(s.saturating_mul(1_000_000))
    }
    /// Saturating sum.
    pub const fn plus(self, o: Micros) -> Micros {
        Micros(self.0.saturating_add(o.0))
    }
    /// Saturating difference (`ZERO` if `o` is later).
    pub const fn since(self, o: Micros) -> Micros {
        Micros(self.0.saturating_sub(o.0))
    }
}

/// The generation of one load request. Every result carries the generation of
/// the request that produced it; a result whose generation is not the current
/// one is stale and is discarded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

/// An imagery layer: one source draped over the ground. Layers stack in
/// their order, 0 at the bottom (the base imagery), overlays above.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(pub u16);

/// What the budget manages: a tile's mesh, or a tile's drape — its imagery
/// of one layer at one level (D-A4). Requests, the ledger and evictions are
/// all keyed by it; the derived order is the final tie-break of every
/// decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceId {
    /// The tile's geometry.
    Mesh(TileId),
    /// The tile's imagery mosaic of one layer at one imagery level.
    Drape {
        /// The tile it drapes.
        tile: TileId,
        /// Its layer.
        layer: LayerId,
        /// Its imagery level.
        level: u32,
    },
}

impl ResourceId {
    /// A tile's drape of `layer` at `level`.
    pub const fn drape(tile: TileId, layer: LayerId, level: u32) -> Self {
        ResourceId::Drape { tile, layer, level }
    }

    /// The tile this resource belongs to.
    pub const fn tile(self) -> TileId {
        match self {
            ResourceId::Mesh(t) | ResourceId::Drape { tile: t, .. } => t,
        }
    }

    /// A drape's layer; `None` for a mesh.
    pub const fn layer(self) -> Option<LayerId> {
        match self {
            ResourceId::Mesh(_) => None,
            ResourceId::Drape { layer, .. } => Some(layer),
        }
    }
}

/// How the budget behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Policy {
    /// Viewers: bounded targets, visible-only overflow, LOD relaxation,
    /// cancellation with a grace time, retry with backoff.
    Interactive,
    /// Films and bakes: unbounded targets, nothing needed is ever evicted, a
    /// frame is drawn only when complete, no relaxation, no cancellation of
    /// needed work, a failure ends the session.
    Batch,
}

/// The platform's memory signal (iOS memory warnings, background).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pressure {
    /// No signal.
    #[default]
    Normal,
    /// A memory warning: targets shrink.
    Warning,
    /// A repeated or critical warning: targets shrink further.
    Critical,
    /// The app is in the background: keep only the floor and what is drawn.
    Background,
}

/// The platform's thermal state (`ProcessInfo.thermalState`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Thermal {
    /// Cool.
    #[default]
    Nominal,
    /// Warm, no action.
    Fair,
    /// Hot: uploads and requests slow down, the LOD coarsens.
    Serious,
    /// Very hot: more of the same.
    Critical,
}

/// Why a load failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Failure {
    /// Worth retrying (timeout, 5xx, connection reset).
    Transient,
    /// Not worth retrying soon (404, undecodable payload).
    Permanent,
}
