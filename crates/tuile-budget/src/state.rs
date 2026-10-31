// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The per-resource state machine and the ledger it keeps exact.
//!
//! A resource's copies are independent: a load in flight, a decoded CPU
//! copy, a copy in transit to the consumer, a copy on the GPU. Each carries
//! the generation of the load that produced it and the content epoch it was
//! loaded under. Every change to a copy goes through the methods here, which
//! update the [`Ledger`](super::Ledger) in the same breath; `audit` checks
//! the two never drift.

// The decisions are integer arithmetic: floats stay in `imagery` and
// `priority`, quantised once, where bit-exactness is controlled.
#![deny(clippy::float_arithmetic)]

use super::cost::Cost;
use super::priority::{PriorityKey, Urgency};
use super::tier::Tier;
use super::tier::{Presence, Source};
use super::types::ResourceId;
use super::types::{Bytes, Class, Generation, Micros};
use super::Ledger;

/// A load in flight, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Load {
    Idle,
    Loading {
        gen: Generation,
        source: Source,
        started: Micros,
        epoch: u64,
    },
}

/// A decoded CPU copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CpuCopy {
    pub gen: Generation,
    pub cost: Cost,
    pub epoch: u64,
    /// It reached the GPU once: delivering it again saves a fetch.
    pub delivered_before: bool,
}

/// A copy handed to the consumer, not yet acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Transit {
    pub gen: Generation,
    pub cost: Cost,
    pub epoch: u64,
}

/// A copy on the GPU, acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Gpu {
    pub gen: Generation,
    pub bytes: Bytes,
    pub epoch: u64,
}

/// Everything the budget knows about one resource.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub class: Class,
    /// The tile's level.
    pub level: u32,
    /// The tile's last known distance, metres.
    pub distance: u32,
    pub last_needed: Micros,
    pub first_wanted: Option<Micros>,
    pub unneeded_since: Option<Micros>,
    pub load: Load,
    pub cpu: Option<CpuCopy>,
    pub transit: Option<Transit>,
    pub gpu: Option<Gpu>,
    pub presence: Option<Presence>,
    pub store_missed: bool,
    pub failures: u32,
    pub retry_at: Micros,
    pub discards: u32,
    pub deliver_after: Micros,
    /// This step's urgency and priority, if it has work.
    pub urgency: Option<Urgency>,
    pub priority: Option<Rank>,
}

/// A priority key without its resource, which the entry's map key already
/// is: 16 bytes instead of 32 in every tracked entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rank {
    urgency: Urgency,
    score: std::cmp::Reverse<i64>,
    tier: Tier,
}

impl Rank {
    pub fn of(k: &PriorityKey) -> Self {
        Rank {
            urgency: k.urgency,
            score: k.score,
            tier: k.tier,
        }
    }

    /// The full key, for `resource`.
    pub fn key(self, resource: ResourceId) -> PriorityKey {
        PriorityKey {
            urgency: self.urgency,
            score: self.score,
            tier: self.tier,
            resource,
        }
    }
}

// A tracked resource's state, kept small: every full scan walks it, and
// it is the bulk of the bytes per tracked resource. It holds no `usize`,
// so the bound holds on wasm32 too.
const _: () = assert!(std::mem::size_of::<Entry>() <= ENTRY_BYTES);
const ENTRY_BYTES: usize = 240;

impl Entry {
    pub fn new(class: Class, level: u32, now: Micros) -> Self {
        Entry {
            class,
            level,
            distance: 0,
            last_needed: now,
            first_wanted: None,
            unneeded_since: None,
            load: Load::Idle,
            cpu: None,
            transit: None,
            gpu: None,
            presence: None,
            store_missed: false,
            failures: 0,
            retry_at: Micros::ZERO,
            discards: 0,
            deliver_after: Micros::ZERO,
            urgency: None,
            priority: None,
        }
    }

    /// Nothing held, nothing in flight.
    pub fn is_empty(&self) -> bool {
        self.load == Load::Idle
            && self.cpu.is_none()
            && self.transit.is_none()
            && self.gpu.is_none()
    }

    pub fn put_cpu(&mut self, l: &mut Ledger, c: CpuCopy) {
        self.take_cpu(l);
        l.decoded = l.decoded.plus(c.cost.gpu);
        self.cpu = Some(c);
    }

    pub fn take_cpu(&mut self, l: &mut Ledger) -> Option<CpuCopy> {
        let c = self.cpu.take()?;
        l.decoded = l.decoded.minus(c.cost.gpu);
        Some(c)
    }

    pub fn put_transit(&mut self, l: &mut Ledger, t: Transit) {
        self.take_transit(l);
        l.gpu[self.class] = l.gpu[self.class].plus(t.cost.gpu);
        l.in_transit = l.in_transit.plus(t.cost.staging);
        self.transit = Some(t);
    }

    pub fn take_transit(&mut self, l: &mut Ledger) -> Option<Transit> {
        let t = self.transit.take()?;
        l.gpu[self.class] = l.gpu[self.class].minus(t.cost.gpu);
        l.in_transit = l.in_transit.minus(t.cost.staging);
        Some(t)
    }

    pub fn put_gpu(&mut self, l: &mut Ledger, g: Gpu) {
        self.take_gpu(l);
        l.gpu[self.class] = l.gpu[self.class].plus(g.bytes);
        self.gpu = Some(g);
    }

    pub fn take_gpu(&mut self, l: &mut Ledger) -> Option<Gpu> {
        let g = self.gpu.take()?;
        l.gpu[self.class] = l.gpu[self.class].minus(g.bytes);
        Some(g)
    }

    /// The decoded copy, if it is of the current epoch.
    pub fn fresh_cpu(&self, epoch: u64) -> Option<CpuCopy> {
        self.cpu.filter(|c| c.epoch == epoch)
    }

    /// Whether the GPU copy is of an older epoch.
    pub fn stale_gpu(&self, epoch: u64) -> bool {
        self.gpu.is_some_and(|g| g.epoch < epoch)
    }

    /// Failed and waiting for its retry.
    pub fn backing_off(&self, now: Micros) -> bool {
        self.failures > 0 && self.retry_at > now
    }
}
