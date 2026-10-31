// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A test-only view of the budget's whole internal state, in a canonical
//! form that does not depend on how the engine stores it: the tests compare
//! it step by step against a reference (the lockstep, `tests/reference/`),
//! so a change that alters internal state without changing a decision is
//! seen too. Not part of the API: hidden from the documentation, never read
//! by a decision, and free to change with the engine.

use super::cost::Cost;
use super::priority::{PriorityKey, Urgency};
use super::stats::BudgetStats;
use super::tier::{Presence, Source};
use super::types::{
    Bytes, Class, Generation, LayerId, Micros, PerClass, Pressure, ResourceId, Thermal, TileId,
};
use super::{DrapeChoice, Ledger, SessionError};

/// One tracked resource's state, every field the engine keeps of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceSnapshot {
    /// The resource.
    pub id: ResourceId,
    /// Its class.
    pub class: Class,
    /// Its tile's level.
    pub level: u32,
    /// Its tile's last known distance, metres.
    pub distance: u32,
    /// When it was last needed.
    pub last_needed: Micros,
    /// When it was first wanted, if it is wanted.
    pub first_wanted: Option<Micros>,
    /// Since when it is not needed, if it is not.
    pub unneeded_since: Option<Micros>,
    /// Its load in flight: generation, source, start, epoch.
    pub load: Option<(Generation, Source, Micros, u64)>,
    /// Its decoded copy: generation, cost, epoch, delivered before.
    pub cpu: Option<(Generation, Cost, u64, bool)>,
    /// Its copy in transit: generation, cost, epoch.
    pub transit: Option<(Generation, Cost, u64)>,
    /// Its GPU copy: generation, bytes, epoch.
    pub gpu: Option<(Generation, Bytes, u64)>,
    /// The stores' presence answer, cached.
    pub presence: Option<Presence>,
    /// A local read missed.
    pub store_missed: bool,
    /// Failures in a row.
    pub failures: u32,
    /// When it may be retried.
    pub retry_at: Micros,
    /// Uploads dropped.
    pub discards: u32,
    /// Not delivered before.
    pub deliver_after: Micros,
    /// This step's urgency.
    pub urgency: Option<Urgency>,
    /// This step's priority key.
    pub priority: Option<PriorityKey>,
}

/// A tile's place in its tree as the budget remembers it: parent, level,
/// and its footprint's bounds as bits (west, south, east, north).
pub type TileSnapshot = (TileId, Option<TileId>, u32, Option<[u64; 4]>);

/// The budget's whole internal state (see the module documentation).
/// Every collection is in a canonical order: by resource, tile or layer, or
/// as the engine keeps it when its order is part of the state.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Every tracked resource, by resource.
    pub resources: Vec<ResourceSnapshot>,
    /// Every tile whose place the budget knows, by tile.
    pub tiles: Vec<TileSnapshot>,
    /// The imagery level each drawn tile's layer wore last step.
    pub imagery_prev: Vec<((TileId, LayerId), u32)>,
    /// The tiles held last step.
    pub held_last: Vec<TileId>,
    /// The meshes drawn last step.
    pub drawn_last: Vec<ResourceId>,
    /// The drapes each drawn mesh wore last step, in order.
    pub worn_last: Vec<(ResourceId, DrapeChoice)>,
    /// The swap set of last step.
    pub swap_last: Vec<ResourceId>,
    /// The fallback chains of last step.
    pub chain_last: Vec<ResourceId>,
    /// The resources loading or in transit.
    pub active: Vec<ResourceId>,
    /// The floor's resources.
    pub floor_set: Vec<ResourceId>,
    /// The resources ranked last step, in the order ranked.
    pub ranked_last: Vec<ResourceId>,
    /// The resources wanted last step.
    pub wanted_last: Vec<ResourceId>,
    /// The recently evicted, and when.
    pub recent: Vec<(ResourceId, Micros)>,
    /// The content epoch of each class.
    pub epochs: PerClass<u64>,
    /// The content epoch of each imagery layer.
    pub layer_epochs: Vec<(LayerId, u64)>,
    /// The next generation handed out.
    pub next_gen: u64,
    /// The last step's time.
    pub last_now: Option<Micros>,
    /// The SSE scale, thousandths.
    pub scale: u32,
    /// The SSE scale, fine.
    pub scale_fine: u64,
    /// The ledger.
    pub ledger: Ledger,
    /// Loads in flight, per class.
    pub in_flight: PerClass<u32>,
    /// Local reads in flight.
    pub local_reads: u32,
    /// Load slots freed below zero.
    pub underflows: u32,
    /// The end of the session, if it ended.
    pub fatal: Option<SessionError>,
    /// The statistics of arrivals applied ahead of their step.
    pub pending: BudgetStats,
    /// The memory signal.
    pub pressure: Pressure,
    /// The thermal state.
    pub thermal: Thermal,
    /// The retarget factor.
    pub factor: (u64, u64),
    /// Steps taken.
    pub steps: u64,
    /// Arrivals recorded, not applied yet.
    pub events: usize,
}

impl Snapshot {
    /// The names of the parts on which `self` and `o` differ, the first
    /// differing resource or tile named.
    pub fn diff(&self, o: &Snapshot) -> Vec<String> {
        let mut out = Vec::new();
        if self.resources != o.resources {
            let first = self
                .resources
                .iter()
                .zip(&o.resources)
                .find(|(a, b)| a != b)
                .map_or_else(
                    || {
                        format!(
                            "resources (tracked {} against {})",
                            self.resources.len(),
                            o.resources.len()
                        )
                    },
                    |(a, b)| format!("resources (first: {a:?} against {b:?})"),
                );
            out.push(first);
        }
        if self.tiles != o.tiles {
            let first = self
                .tiles
                .iter()
                .zip(&o.tiles)
                .find(|(a, b)| a != b)
                .map_or_else(
                    || format!("tiles ({} against {})", self.tiles.len(), o.tiles.len()),
                    |(a, b)| format!("tiles (first: {a:?} against {b:?})"),
                );
            out.push(first);
        }
        let mut eq = |name: &str, same: bool| {
            if !same {
                out.push(name.to_owned());
            }
        };
        eq("imagery_prev", self.imagery_prev == o.imagery_prev);
        eq("held_last", self.held_last == o.held_last);
        eq("drawn_last", self.drawn_last == o.drawn_last);
        eq("worn_last", self.worn_last == o.worn_last);
        eq("swap_last", self.swap_last == o.swap_last);
        eq("chain_last", self.chain_last == o.chain_last);
        eq("active", self.active == o.active);
        eq("floor_set", self.floor_set == o.floor_set);
        eq("ranked_last", self.ranked_last == o.ranked_last);
        eq("wanted_last", self.wanted_last == o.wanted_last);
        eq("recent", self.recent == o.recent);
        eq("epochs", self.epochs == o.epochs);
        eq("layer_epochs", self.layer_epochs == o.layer_epochs);
        eq("next_gen", self.next_gen == o.next_gen);
        eq("last_now", self.last_now == o.last_now);
        eq("scale", self.scale == o.scale);
        eq("scale_fine", self.scale_fine == o.scale_fine);
        eq("ledger", self.ledger == o.ledger);
        eq("in_flight", self.in_flight == o.in_flight);
        eq("local_reads", self.local_reads == o.local_reads);
        eq("underflows", self.underflows == o.underflows);
        eq("fatal", self.fatal == o.fatal);
        eq("pending", self.pending == o.pending);
        eq("pressure", self.pressure == o.pressure);
        eq("thermal", self.thermal == o.thermal);
        eq("factor", self.factor == o.factor);
        eq("steps", self.steps == o.steps);
        eq("events", self.events == o.events);
        out
    }
}

/// A footprint's bounds as bits, for a bit-for-bit comparison.
pub fn rect_bits(r: super::GeoRect) -> [u64; 4] {
    [
        r.west.to_bits(),
        r.south.to_bits(),
        r.east.to_bits(),
        r.north.to_bits(),
    ]
}
