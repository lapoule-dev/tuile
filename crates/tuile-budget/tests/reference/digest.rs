// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a step of the budget produces, as four digests: its decisions, its
//! ledger, the views it exposes, and its whole internal state
//! (`tuile_budget::snapshot`). Each is a 64-bit FNV-1a over an explicit,
//! little-endian encoding written here field by field — no derived `Hash`,
//! no `Debug` of a large structure — so a digest depends on the values
//! alone, the same on every platform and every toolchain.
//!
//! A run folds its steps' digests into four chains; the recorded reference
//! (`recorded.txt`) holds the chains every [`CHECKPOINT`] steps and at the
//! run's end.

use tuile_budget::snapshot::{ResourceSnapshot, Snapshot};
use tuile_budget::{
    AuditError, BudgetStats, Bytes, Class, Decisions, DrapeChoice, Generation, LayerId, Ledger,
    Micros, PerClass, Presence, Pressure, PriorityKey, Protection, ResourceId, ResourceState,
    SessionError, Source, Thermal, Tier, TileId, Urgency,
};

/// Steps between two recorded checkpoints of a run.
pub const CHECKPOINT: u64 = 256;

/// The four parts of a step, in the order of the chains.
pub const PARTS: [&str; 4] = ["decisions", "ledger", "views", "state"];

/// 64-bit FNV-1a.
#[derive(Clone, Copy, Debug)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    pub fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= u64::from(*x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    pub fn finish(self) -> u64 {
        self.0
    }
}

/// A value fed into a digest by an explicit encoding.
pub trait Feed {
    fn feed(&self, h: &mut Fnv);
}

macro_rules! feed_int {
    ($($t:ty),*) => {$(
        impl Feed for $t {
            fn feed(&self, h: &mut Fnv) {
                h.u64(*self as u64);
            }
        }
    )*};
}
feed_int!(u8, u16, u32, u64, usize, i32, i64);

impl Feed for bool {
    fn feed(&self, h: &mut Fnv) {
        h.u64(u64::from(*self));
    }
}

impl Feed for f64 {
    fn feed(&self, h: &mut Fnv) {
        h.u64(self.to_bits());
    }
}

impl<T: Feed> Feed for Option<T> {
    fn feed(&self, h: &mut Fnv) {
        match self {
            None => h.u64(0),
            Some(x) => {
                h.u64(1);
                x.feed(h);
            }
        }
    }
}

impl<T: Feed> Feed for [T] {
    fn feed(&self, h: &mut Fnv) {
        h.u64(self.len() as u64);
        for x in self {
            x.feed(h);
        }
    }
}

impl<T: Feed> Feed for Vec<T> {
    fn feed(&self, h: &mut Fnv) {
        self.as_slice().feed(h);
    }
}

impl<T: Feed, const N: usize> Feed for [T; N] {
    fn feed(&self, h: &mut Fnv) {
        for x in self {
            x.feed(h);
        }
    }
}

impl<A: Feed, B: Feed> Feed for (A, B) {
    fn feed(&self, h: &mut Fnv) {
        self.0.feed(h);
        self.1.feed(h);
    }
}

impl<A: Feed, B: Feed, C: Feed> Feed for (A, B, C) {
    fn feed(&self, h: &mut Fnv) {
        self.0.feed(h);
        self.1.feed(h);
        self.2.feed(h);
    }
}

impl<A: Feed, B: Feed, C: Feed, D: Feed> Feed for (A, B, C, D) {
    fn feed(&self, h: &mut Fnv) {
        self.0.feed(h);
        self.1.feed(h);
        self.2.feed(h);
        self.3.feed(h);
    }
}

impl<T: Feed> Feed for PerClass<T> {
    fn feed(&self, h: &mut Fnv) {
        self.0.feed(h);
    }
}

impl<T: Feed + ?Sized> Feed for &T {
    fn feed(&self, h: &mut Fnv) {
        (**self).feed(h);
    }
}

macro_rules! feed_newtype {
    ($($t:ty),*) => {$(
        impl Feed for $t {
            fn feed(&self, h: &mut Fnv) {
                self.0.feed(h);
            }
        }
    )*};
}
feed_newtype!(Micros, Generation, Bytes, TileId, LayerId);

// Fieldless enums, by their variant's place: the order of their
// declaration is part of the record, as it is of `Decisions::digest`.
macro_rules! feed_fieldless {
    ($($t:ty),*) => {$(
        impl Feed for $t {
            fn feed(&self, h: &mut Fnv) {
                h.u64(*self as u64);
            }
        }
    )*};
}
feed_fieldless!(Source, Urgency, Tier, Protection, Pressure, Thermal);

impl Feed for Class {
    fn feed(&self, h: &mut Fnv) {
        h.u64(self.index() as u64);
    }
}

impl Feed for Presence {
    fn feed(&self, h: &mut Fnv) {
        h.u64(match self {
            Presence::Stored => 0,
            Presence::Absent => 1,
        });
    }
}

impl Feed for ResourceId {
    fn feed(&self, h: &mut Fnv) {
        match *self {
            ResourceId::Mesh(t) => {
                h.u64(0);
                t.feed(h);
            }
            ResourceId::Drape { tile, layer, level } => {
                h.u64(1);
                tile.feed(h);
                layer.feed(h);
                level.feed(h);
            }
        }
    }
}

impl Feed for tuile_budget::cost::Cost {
    fn feed(&self, h: &mut Fnv) {
        self.gpu.feed(h);
        self.staging.feed(h);
    }
}

impl Feed for PriorityKey {
    fn feed(&self, h: &mut Fnv) {
        self.urgency.feed(h);
        self.score.0.feed(h);
        self.tier.feed(h);
        self.resource.feed(h);
    }
}

impl Feed for DrapeChoice {
    fn feed(&self, h: &mut Fnv) {
        self.resource.feed(h);
        self.layer.feed(h);
        self.level.feed(h);
        self.uv.scale.feed(h);
        self.uv.offset.feed(h);
    }
}

impl Feed for Ledger {
    fn feed(&self, h: &mut Fnv) {
        self.gpu.feed(h);
        self.decoded.feed(h);
        self.in_transit.feed(h);
    }
}

impl Feed for SessionError {
    fn feed(&self, h: &mut Fnv) {
        self.resource().feed(h);
        h.bytes(self.to_string().as_bytes());
    }
}

impl Feed for ResourceState {
    fn feed(&self, h: &mut Fnv) {
        // Its `Debug` names the variant and the source of a load: a few
        // bytes, and the enum is matched nowhere here.
        h.bytes(format!("{self:?}").as_bytes());
    }
}

impl Feed for BudgetStats {
    /// Every statistic but `work`, the cost counter the optimisations
    /// exist to lower: by its `Debug`, with `work` zeroed.
    fn feed(&self, h: &mut Fnv) {
        let mut s = self.clone();
        s.work = 0;
        h.bytes(format!("{s:?}").as_bytes());
    }
}

impl Feed for ResourceSnapshot {
    fn feed(&self, h: &mut Fnv) {
        self.id.feed(h);
        self.class.feed(h);
        self.level.feed(h);
        self.distance.feed(h);
        self.last_needed.feed(h);
        self.first_wanted.feed(h);
        self.unneeded_since.feed(h);
        self.load.feed(h);
        self.cpu.feed(h);
        self.transit.feed(h);
        self.gpu.feed(h);
        self.presence.feed(h);
        self.store_missed.feed(h);
        self.failures.feed(h);
        self.retry_at.feed(h);
        self.discards.feed(h);
        self.deliver_after.feed(h);
        self.urgency.feed(h);
        self.priority.feed(h);
    }
}

impl Feed for Snapshot {
    fn feed(&self, h: &mut Fnv) {
        self.resources.feed(h);
        self.tiles.feed(h);
        self.imagery_prev.feed(h);
        self.held_last.feed(h);
        self.drawn_last.feed(h);
        self.worn_last.feed(h);
        self.swap_last.feed(h);
        self.chain_last.feed(h);
        self.active.feed(h);
        self.floor_set.feed(h);
        self.ranked_last.feed(h);
        self.wanted_last.feed(h);
        self.recent.feed(h);
        self.epochs.feed(h);
        self.layer_epochs.feed(h);
        self.next_gen.feed(h);
        self.last_now.feed(h);
        self.scale.feed(h);
        self.scale_fine.feed(h);
        self.ledger.feed(h);
        self.in_flight.feed(h);
        self.local_reads.feed(h);
        self.underflows.feed(h);
        self.fatal.feed(h);
        self.pending.feed(h);
        self.pressure.feed(h);
        self.thermal.feed(h);
        self.factor.feed(h);
        self.steps.feed(h);
        self.events.feed(h);
    }
}

/// The digest of one value.
pub fn of<T: Feed + ?Sized>(x: &T) -> u64 {
    let mut h = Fnv::default();
    x.feed(&mut h);
    h.finish()
}

/// The decisions' part: their own digest, and what it leaves out — the
/// statistics but `work`, and the end of a session.
pub fn decisions(d: &Decisions) -> u64 {
    let mut h = Fnv::default();
    h.u64(d.digest());
    d.stats.feed(&mut h);
    d.fatal.feed(&mut h);
    h.finish()
}

/// The ledger's part: the ledger, the audit's verdict, the tracked count.
pub fn ledger(l: &Ledger, audit: &Result<(), AuditError>, tracked: usize) -> u64 {
    let mut h = Fnv::default();
    l.feed(&mut h);
    h.bytes(format!("{audit:?}").as_bytes());
    tracked.feed(&mut h);
    h.finish()
}

/// The fields of a resource view, as both engines give them.
pub type ViewFields = (
    ResourceState,
    Tier,
    Option<Generation>,
    Bytes,
    Bytes,
    Option<Protection>,
    Option<Urgency>,
    Option<PriorityKey>,
);

/// Feeds one resource's view (absent: untracked).
pub fn feed_view(h: &mut Fnv, r: ResourceId, v: Option<ViewFields>) {
    r.feed(h);
    match v {
        None => h.u64(0),
        Some((state, tier, gen, gpu, decoded, protected, urgency, priority)) => {
            h.u64(1);
            state.feed(h);
            tier.feed(h);
            gen.feed(h);
            gpu.feed(h);
            decoded.feed(h);
            protected.feed(h);
            urgency.feed(h);
            priority.feed(h);
        }
    }
}

/// Four chains folded step by step, one per part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chains(pub [u64; 4]);

impl Chains {
    /// Folds one step's (or one event's) parts in.
    pub fn fold(&mut self, parts: [u64; 4]) {
        for (c, p) in self.0.iter_mut().zip(parts) {
            let mut h = Fnv(*c);
            h.u64(p);
            *c = h.finish();
        }
    }

    /// The parts on which two chains differ.
    pub fn differing(&self, o: &Chains) -> Vec<&'static str> {
        PARTS
            .iter()
            .zip(self.0.iter().zip(o.0.iter()))
            .filter(|(_, (a, b))| a != b)
            .map(|(n, _)| *n)
            .collect()
    }
}
