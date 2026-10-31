// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Residency tiers: the cheapest place a resource already lives in.
//!
//! The budget never asks for more than the step from there to "drawn": no
//! network request for what a local store holds, no re-decode of what is
//! decoded, no re-upload of what is on the GPU.

use super::types::ResourceId;

/// Where a resource already lives, cheapest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Uploaded and drawable: nothing to do.
    Gpu,
    /// Decoded in memory, not on the GPU: upload only.
    Decoded,
    /// In a local store (tile store, HTTP disk cache): local read and decode.
    Stored,
    /// Nowhere local: network fetch and decode.
    Remote,
}

/// A store's answer about a resource, from its index, without reading the
/// payload. A hint, never a promise: a wrong `Stored` degrades to a network
/// fetch through [`Budget::on_store_miss`](super::Budget::on_store_miss).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Presence {
    /// Some local store holds it.
    Stored,
    /// No local store holds it.
    Absent,
}

/// The stores' presence index, asked once per resource it is about to
/// schedule. Implemented over the tile store and the HTTP disk cache.
///
/// Must be deterministic for the budget's decisions to be.
pub trait PresenceOracle {
    /// Where `r` is held locally, if anywhere.
    fn holds(&self, r: ResourceId) -> Presence;

    /// A value that changes whenever an answer may: equal from one step to
    /// the next, every answer is the same, and the budget need not ask
    /// again what it asked already. `None` (the default) promises nothing:
    /// the budget asks again every step.
    fn generation(&self) -> Option<u64> {
        None
    }
}

/// No local store: everything not decoded is remote.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoStore;

impl PresenceOracle for NoStore {
    fn holds(&self, _: ResourceId) -> Presence {
        Presence::Absent
    }

    fn generation(&self) -> Option<u64> {
        Some(0)
    }
}

/// Where a started load reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// A local store, under the local-read quota.
    Local,
    /// The network, under the class's network quota.
    Network,
}
