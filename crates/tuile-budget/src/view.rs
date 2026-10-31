// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Read-only views of the budget: the traversal's residency, and one
//! resource's state for tests and the control API.

use super::priority::{PriorityKey, Urgency};
use super::tier::{Source, Tier};
use super::types::{Bytes, Generation, Micros};

/// A resource's state, as the control API reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResourceState {
    /// Wanted, not started.
    Wanted,
    /// Loading.
    Loading {
        /// From where.
        source: Source,
    },
    /// Decoded in memory.
    Loaded,
    /// Handed to the consumer, not yet acknowledged.
    InTransit,
    /// On the GPU.
    Drawable,
    /// Failed; retried at `next_at`.
    Failed {
        /// Failures so far.
        retries: u32,
        /// When it may be retried.
        next_at: Micros,
    },
}

/// Why a resource may not be evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protection {
    /// The pinned floor.
    Floor,
    /// In the draw set (exact, held, fallback, or a drape worn).
    Drawn,
    /// In transit to the GPU.
    InTransit,
    /// An ancestor of the drawn cut: the traversal walks through it to
    /// reach what is drawn, and it is the fallback chain should a drawn tile
    /// go.
    Chain,
    /// Drawable half of a swap still in progress: a missing tile's drape
    /// waiting for its mesh, or a held parent's child waiting for its
    /// siblings. Evicting it would restart the swap.
    Swap,
}

/// One resource, as the budget sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResourceView {
    /// Its state.
    pub state: ResourceState,
    /// The cheapest place it lives.
    pub tier: Tier,
    /// The generation of its current load or content.
    pub gen: Option<Generation>,
    /// GPU bytes charged.
    pub gpu: Bytes,
    /// Decoded bytes charged.
    pub decoded: Bytes,
    /// Why it is protected, if it is.
    pub protected: Option<Protection>,
    /// Its urgency this step, if wanted.
    pub urgency: Option<Urgency>,
    /// Its priority key this step, if wanted.
    pub priority: Option<PriorityKey>,
}
