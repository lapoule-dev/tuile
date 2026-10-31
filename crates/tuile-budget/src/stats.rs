// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`BudgetStats`]: one step's numbers, the raw material of the
//! `BudgetObserver` (JSONL and `/metrics`) and of the device audits: bytes per
//! class against the target, cancel/complete ratio, latency histogram,
//! duplicates in flight, holes, underdraped tiles, convergence.

use super::types::{Bytes, PerClass};

/// Buckets of the load-latency histogram: bucket `i` counts loads that took
/// `[2^(i-1), 2^i)` milliseconds (bucket 0: under 1 ms; the last: the rest).
pub const LATENCY_BUCKETS: usize = 18;

/// Span of the imagery-over-terrain histogram: index `gap + 8`, clamped.
pub const GAP_BUCKETS: usize = 17;

/// Why a load or a delivery was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CancelReason {
    /// Out of the need for longer than the grace (I5).
    Grace,
    /// Unanswered past the load timeout (§11.3).
    Timeout,
    /// Its slot taken by urgent work (§11.3).
    Preempted,
}

impl CancelReason {
    /// Every reason, in index order.
    pub const ALL: [CancelReason; 3] = [
        CancelReason::Grace,
        CancelReason::Timeout,
        CancelReason::Preempted,
    ];
}

/// Why a GPU copy was evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvictReason {
    /// The sweep to the low watermark.
    Sweep,
    /// The sweep, under a memory signal.
    Pressure,
    /// Room made for a delivery (admission preempts lower-value residents).
    Admission,
}

impl EvictReason {
    /// Every reason, in index order.
    pub const ALL: [EvictReason; 3] = [
        EvictReason::Sweep,
        EvictReason::Pressure,
        EvictReason::Admission,
    ];
}

/// One step's statistics. Counters count this step only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetStats {
    /// GPU bytes per class (drawable plus in transit).
    pub gpu: PerClass<Bytes>,
    /// The protected part of `gpu`.
    pub protected: PerClass<Bytes>,
    /// The effective target per class, after pressure.
    pub target: PerClass<Bytes>,
    /// The effective overflow per class, after pressure.
    pub overflow: PerClass<Bytes>,
    /// The effective hard limit per class, after `retarget`.
    pub hard_limit: PerClass<Bytes>,
    /// The part of `gpu` in transit to the GPU (delivered, not yet
    /// acknowledged), per class.
    pub transit: PerClass<Bytes>,
    /// Loads started per class, from a local store (`[0]`) or the network
    /// (`[1]`).
    pub starts: PerClass<[u32; 2]>,
    /// Loads started, then cancelled for leaving the need before they were
    /// delivered (P2.2).
    pub wasted_starts: PerClass<u32>,
    /// Stored resources left unstarted while a lower-priority network load
    /// started (must be 0, I15).
    pub local_waits: u32,
    /// Cancels by reason, in [`CancelReason::ALL`]'s order.
    pub cancels: [u32; 3],
    /// GPU evictions per class, by reason in [`EvictReason::ALL`]'s order.
    pub evictions: PerClass<[u32; 3]>,
    /// Decoded copies released per class.
    pub releases: PerClass<u32>,
    /// Failures per class, transient (`[0]`) and permanent (`[1]`).
    pub failures: PerClass<[u32; 2]>,
    /// Acknowledgements more than 5 % off their estimate, per class.
    pub drift_exceeded: PerClass<u32>,
    /// The deepest climb this step from a wanted tile to the ancestor that
    /// stands for it, in levels (P2, P4.1).
    pub fallback_depth_max: u32,
    /// Resources the step visited: need entries, wants, ranked work and
    /// tracked entries scanned. Deterministic: P6.4's work counter.
    pub work: u32,
    /// Decoded CPU bytes held.
    pub decoded: Bytes,
    /// Bytes delivered and not yet acknowledged.
    pub in_transit: Bytes,
    /// The LOD relaxation, thousandths.
    pub sse_scale_milli: u32,
    /// Loads started over the network.
    pub started_network: u32,
    /// Loads started from a local store.
    pub started_local: u32,
    /// Loads cancelled.
    pub cancelled: u32,
    /// Loads completed (fresh generation).
    pub completed: u32,
    /// Results discarded because their generation was stale.
    pub stale: u32,
    /// Resources delivered.
    pub delivered: u32,
    /// GPU copies evicted.
    pub evicted: u32,
    /// Decoded copies released.
    pub released: u32,
    /// Loads failed.
    pub failed: u32,
    /// Local reads that missed after a positive presence hint.
    pub store_misses: u32,
    /// Network bytes avoided by local tiers this step.
    pub network_bytes_avoided: Bytes,
    /// Resources fetched again from the network within 10 s of their
    /// eviction (what I14 forbids in spirit).
    pub thrash: u32,
    /// Resources read again from a local store within 10 s of their
    /// eviction: the eviction demoted them to the Stored tier.
    pub rereads: u32,
    /// Entries of the bounded wish list.
    pub heap_len: u32,
    /// Network loads in flight per class.
    pub in_flight: PerClass<u32>,
    /// Local reads in flight.
    pub local_reads: u32,
    /// Resources with more than one load in flight (must be 0).
    pub duplicates_in_flight: u32,
    /// Resources the budget tracks (bounded state).
    pub tracked: u32,
    /// Wanted tiles drawn as themselves.
    pub exact: u32,
    /// Finer meshes kept for a wanted coarser tile.
    pub held: u32,
    /// Wanted tiles standing on an ancestor.
    pub fallback: u32,
    /// Wanted tiles with nothing to draw (must be 0 once the floor is in).
    pub lost: u32,
    /// Drawn items whose tiles ask for imagery and that do not wear a
    /// drawable drape for every layer they need, own or ancestor's (must be
    /// 0: bare ground is black ground, I1 as extended by D-A1).
    pub bare: u32,
    /// The floor is in: every floor tile the need lists has its mesh and,
    /// for every layer it wears, its floor drape drawable (P1.1).
    pub floor_in: bool,
    /// Drawn tiles whose drape is more than one level from their need.
    pub underdraped: u32,
    /// Drawn tiles per imagery-minus-terrain level gap (`gap + 8`).
    pub imagery_over_terrain: [u32; GAP_BUCKETS],
    /// Load latencies of the loads completed this step.
    pub latency_ms: [u32; LATENCY_BUCKETS],
    /// Need entries with a non-finite float.
    pub rejected_inputs: u32,
    /// Every wanted tile drawn exactly with its needed drape, and no work
    /// left for the view.
    pub converged: bool,
}

impl BudgetStats {
    /// A digest of every field, FNV-1a, the same on every platform: a device
    /// run's per-step statistics compare to the simulation's.
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut put = |v: u64| {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for per in [
            &self.gpu,
            &self.protected,
            &self.target,
            &self.overflow,
            &self.hard_limit,
            &self.transit,
        ] {
            for (_, b) in per.iter() {
                put(b.0);
            }
        }
        for v in [
            self.decoded.0,
            self.in_transit.0,
            u64::from(self.sse_scale_milli),
            u64::from(self.started_network),
            u64::from(self.started_local),
            u64::from(self.cancelled),
            u64::from(self.completed),
            u64::from(self.stale),
            u64::from(self.delivered),
            u64::from(self.evicted),
            u64::from(self.released),
            u64::from(self.failed),
            u64::from(self.store_misses),
            self.network_bytes_avoided.0,
            u64::from(self.thrash),
            u64::from(self.rereads),
            u64::from(self.heap_len),
            u64::from(self.local_reads),
            u64::from(self.duplicates_in_flight),
            u64::from(self.tracked),
            u64::from(self.exact),
            u64::from(self.held),
            u64::from(self.fallback),
            u64::from(self.lost),
            u64::from(self.bare),
            u64::from(self.floor_in),
            u64::from(self.underdraped),
            u64::from(self.rejected_inputs),
            u64::from(self.converged),
        ] {
            put(v);
        }
        for (_, n) in self.in_flight.iter() {
            put(u64::from(n));
        }
        for (_, [a, b]) in self.starts.iter().chain(self.failures.iter()) {
            put(u64::from(a));
            put(u64::from(b));
        }
        for (_, n) in self
            .wasted_starts
            .iter()
            .chain(self.releases.iter())
            .chain(self.drift_exceeded.iter())
        {
            put(u64::from(n));
        }
        for (_, e) in self.evictions.iter() {
            for n in e {
                put(u64::from(n));
            }
        }
        for n in self.cancels {
            put(u64::from(n));
        }
        put(u64::from(self.local_waits));
        put(u64::from(self.fallback_depth_max));
        put(u64::from(self.work));
        for n in self.imagery_over_terrain.iter().chain(&self.latency_ms) {
            put(u64::from(*n));
        }
        h
    }
}
