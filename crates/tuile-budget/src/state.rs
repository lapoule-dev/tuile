// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The per-resource state machine, the ledger it keeps exact, and the arena
//! that holds it.
//!
//! A resource's copies are independent: a load in flight, a decoded CPU
//! copy, a copy in transit to the consumer, a copy on the GPU. Each carries
//! the generation of the load that produced it and the content epoch it was
//! loaded under. Every change to a copy goes through the methods here, which
//! update the [`Ledger`](super::Ledger) in the same breath; `audit` checks
//! the two never drift.
//!
//! The resources live in a dense [`Arena`]: each tracked resource has a
//! [`Slot`], and its state is kept as columns indexed by slot — what every
//! full scan reads (class, which copies are held, the eviction key) in
//! small contiguous columns, each copy's payload in a column of its own, the
//! rarely read rest apart. A resource finds its slot through a hash index
//! with a fixed seed that is never iterated: every decision that walks the
//! resources walks the slots and orders what it finds by a total key, or —
//! a drape's drawable levels — reads an ordered index kept with the GPU
//! copies. Slots are assigned and reused deterministically (a freed slot is
//! the next one taken), though no decision depends on a slot's number.

// The decisions are integer arithmetic: floats stay in `imagery` and
// `priority`, quantised once, where bit-exactness is controlled.
#![deny(clippy::float_arithmetic)]

use std::collections::BTreeSet;
use std::hash::BuildHasher;

use foldhash::fast::FixedState;
use hashbrown::HashTable;

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Transit {
    pub gen: Generation,
    pub cost: Cost,
    pub epoch: u64,
}

/// A copy on the GPU, acknowledged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Gpu {
    pub gen: Generation,
    pub bytes: Bytes,
    pub epoch: u64,
}

/// A priority key without its resource, which the slot's id already is:
/// 16 bytes instead of 32 in every tracked slot.
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

/// A tracked resource's place in the [`Arena`]: an index into its columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Slot(u32);

impl Slot {
    /// Its column index. A `u32` widens into a `usize` on every target the
    /// crate builds for (64-bit hosts, wasm32).
    fn i(self) -> usize {
        self.0 as usize
    }
}

// Which copies a slot holds, and what its payload columns mean: one byte
// per slot, read before any payload.
/// The slot holds a tracked resource (a freed slot does not).
const LIVE: u8 = 1;
/// A decoded copy: `cpu` holds it.
const CPU: u8 = 1 << 1;
/// The decoded copy reached the GPU once (`CpuCopy::delivered_before`).
const DELIVERED: u8 = 1 << 2;
/// A copy in transit: `transit` holds it.
const TRANSIT: u8 = 1 << 3;
/// A copy on the GPU: `gpu` holds it.
const GPU: u8 = 1 << 4;
/// A load in flight: `load` holds it.
const LOADING: u8 = 1 << 5;
/// The load in flight reads a local store (`Source::Local`); else the
/// network.
const LOCAL: u8 = 1 << 6;

/// What the eviction order reads, together.
#[derive(Clone, Copy, Debug, Default)]
struct Order {
    last_needed: Micros,
    /// The tile's level.
    level: u32,
    /// The tile's last known distance, metres.
    distance: u32,
}

/// A decoded copy's payload; whether it was delivered is a flag.
#[derive(Clone, Copy, Debug, Default)]
struct CpuPayload {
    gen: Generation,
    cost: Cost,
    epoch: u64,
}

/// A load's payload; its source is a flag.
#[derive(Clone, Copy, Debug, Default)]
struct LoadPayload {
    gen: Generation,
    started: Micros,
    epoch: u64,
}

/// What a slot keeps that no full scan reads.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Cold {
    first_wanted: Micros,
    unneeded_since: Micros,
    pub retry_at: Micros,
    pub deliver_after: Micros,
    pub failures: u32,
    pub discards: u32,
    pub presence: Option<Presence>,
    pub store_missed: bool,
    /// Which of `first_wanted`, `unneeded_since` hold a time.
    has: u8,
}

const FIRST_WANTED: u8 = 1;
const UNNEEDED: u8 = 1 << 1;

impl Cold {
    fn time(&self, bit: u8, t: Micros) -> Option<Micros> {
        (self.has & bit != 0).then_some(t)
    }

    pub fn first_wanted(&self) -> Option<Micros> {
        self.time(FIRST_WANTED, self.first_wanted)
    }

    pub fn unneeded_since(&self) -> Option<Micros> {
        self.time(UNNEEDED, self.unneeded_since)
    }

    pub fn set_first_wanted(&mut self, t: Option<Micros>) {
        self.first_wanted = t.unwrap_or(Micros::ZERO);
        self.has = (self.has & !FIRST_WANTED) | if t.is_some() { FIRST_WANTED } else { 0 };
    }

    pub fn set_unneeded_since(&mut self, t: Option<Micros>) {
        self.unneeded_since = t.unwrap_or(Micros::ZERO);
        self.has = (self.has & !UNNEEDED) | if t.is_some() { UNNEEDED } else { 0 };
    }

    /// `first_wanted.get_or_insert(now)`.
    pub fn first_wanted_or(&mut self, now: Micros) {
        if self.first_wanted().is_none() {
            self.set_first_wanted(Some(now));
        }
    }

    /// `unneeded_since.get_or_insert(now)`.
    pub fn unneeded_since_or(&mut self, now: Micros) {
        if self.unneeded_since().is_none() {
            self.set_unneeded_since(Some(now));
        }
    }

    /// Failed and waiting for its retry.
    pub fn backing_off(&self, now: Micros) -> bool {
        self.failures > 0 && self.retry_at > now
    }
}

/// The hash index's seed: fixed, so a run is the same on every target and
/// in every process. The index is only ever probed, never iterated.
const SEED: u64 = 0x7475_696c_655f_6275; // "tuile_bu"

/// Every tracked resource's state, by slot (see the module documentation).
#[derive(Debug)]
pub(super) struct Arena {
    hasher: FixedState,
    /// Resource → slot. Probed, never iterated.
    index: HashTable<Slot>,
    /// Freed slots, the last freed taken first.
    free: Vec<Slot>,
    /// Live slots.
    live: usize,
    id: Vec<ResourceId>,
    class: Vec<Class>,
    flags: Vec<u8>,
    order: Vec<Order>,
    gpu: Vec<Gpu>,
    transit: Vec<Transit>,
    cpu: Vec<CpuPayload>,
    load: Vec<LoadPayload>,
    /// This step's urgency and priority, if it has work.
    urgency: Vec<Option<Urgency>>,
    priority: Vec<Option<Rank>>,
    cold: Vec<Cold>,
    /// The drapes with a GPU copy, in resource order: a tile's drawable
    /// drapes of one layer are a range of it, by level.
    gpu_drapes: BTreeSet<ResourceId>,
}

/// The bytes one slot holds across the columns, its index and free-list
/// entries aside. It holds no `usize`, so the bound holds on wasm32 too:
/// every full scan walks the slots, and they are the bulk of the bytes per
/// tracked resource (240 B as one entry of an ordered map, its key apart).
pub(super) const SLOT_BYTES: usize = std::mem::size_of::<ResourceId>()
    + std::mem::size_of::<Class>()
    + std::mem::size_of::<u8>()
    + std::mem::size_of::<Order>()
    + std::mem::size_of::<Gpu>()
    + std::mem::size_of::<Transit>()
    + std::mem::size_of::<CpuPayload>()
    + std::mem::size_of::<LoadPayload>()
    + std::mem::size_of::<Option<Urgency>>()
    + std::mem::size_of::<Option<Rank>>()
    + std::mem::size_of::<Cold>();
const _: () = assert!(SLOT_BYTES <= MAX_SLOT_BYTES);
const MAX_SLOT_BYTES: usize = 211;

impl Default for Arena {
    fn default() -> Self {
        Arena {
            hasher: FixedState::with_seed(SEED),
            index: HashTable::new(),
            free: Vec::new(),
            live: 0,
            id: Vec::new(),
            class: Vec::new(),
            flags: Vec::new(),
            order: Vec::new(),
            gpu: Vec::new(),
            transit: Vec::new(),
            cpu: Vec::new(),
            load: Vec::new(),
            urgency: Vec::new(),
            priority: Vec::new(),
            cold: Vec::new(),
            gpu_drapes: BTreeSet::new(),
        }
    }
}

impl Arena {
    /// How many resources are tracked.
    pub fn len(&self) -> usize {
        self.live
    }

    /// The slot of `r`, if tracked.
    pub fn find(&self, r: ResourceId) -> Option<Slot> {
        let h = self.hasher.hash_one(r);
        self.index.find(h, |s| self.id[s.i()] == r).copied()
    }

    /// The slot of `r`, tracked from now on if it was not, as a resource of
    /// `class` at `level`, first needed `now`, holding nothing.
    pub fn find_or_insert(&mut self, r: ResourceId, class: Class, level: u32, now: Micros) -> Slot {
        if let Some(s) = self.find(r) {
            return s;
        }
        let order = Order {
            last_needed: now,
            level,
            distance: 0,
        };
        let s = match self.free.pop() {
            Some(s) => {
                let i = s.i();
                self.id[i] = r;
                self.class[i] = class;
                self.flags[i] = LIVE;
                self.order[i] = order;
                self.gpu[i] = Gpu::default();
                self.transit[i] = Transit::default();
                self.cpu[i] = CpuPayload::default();
                self.load[i] = LoadPayload::default();
                self.urgency[i] = None;
                self.priority[i] = None;
                self.cold[i] = Cold::default();
                s
            }
            None => {
                // Slots are numbered by `u32`: four billion tracked
                // resources is far past any memory a budget manages.
                let s = Slot(u32::try_from(self.id.len()).unwrap_or(u32::MAX));
                self.id.push(r);
                self.class.push(class);
                self.flags.push(LIVE);
                self.order.push(order);
                self.gpu.push(Gpu::default());
                self.transit.push(Transit::default());
                self.cpu.push(CpuPayload::default());
                self.load.push(LoadPayload::default());
                self.urgency.push(None);
                self.priority.push(None);
                self.cold.push(Cold::default());
                s
            }
        };
        let Arena {
            hasher, index, id, ..
        } = self;
        index.insert_unique(hasher.hash_one(r), s, |x| hasher.hash_one(id[x.i()]));
        self.live += 1;
        s
    }

    /// Stops tracking the resource of `s`, which holds nothing; its slot
    /// is the next one taken.
    pub fn remove(&mut self, s: Slot) {
        debug_assert!(self.is_empty(s), "a slot is freed holding nothing");
        let h = self.hasher.hash_one(self.id[s.i()]);
        if let Ok(e) = self.index.find_entry(h, |x| *x == s) {
            e.remove();
        }
        self.flags[s.i()] = 0;
        self.free.push(s);
        self.live -= 1;
    }

    /// Every slot, live or not: `0..slots()`; [`Arena::live`] tells.
    pub fn slots(&self) -> u32 {
        u32::try_from(self.flags.len()).unwrap_or(u32::MAX)
    }

    /// The live slot of number `n`, if it is live.
    pub fn live(&self, n: u32) -> Option<Slot> {
        let s = Slot(n);
        (self.flags[s.i()] & LIVE != 0).then_some(s)
    }

    /// Every live slot, in slot order (not resource order).
    pub fn iter(&self) -> impl Iterator<Item = Slot> + '_ {
        (0..self.slots()).filter_map(|n| self.live(n))
    }

    pub fn id(&self, s: Slot) -> ResourceId {
        self.id[s.i()]
    }

    pub fn class(&self, s: Slot) -> Class {
        self.class[s.i()]
    }

    pub fn level(&self, s: Slot) -> u32 {
        self.order[s.i()].level
    }

    pub fn distance(&self, s: Slot) -> u32 {
        self.order[s.i()].distance
    }

    pub fn last_needed(&self, s: Slot) -> Micros {
        self.order[s.i()].last_needed
    }

    pub fn set_distance(&mut self, s: Slot, d: u32) {
        self.order[s.i()].distance = d;
    }

    pub fn set_last_needed(&mut self, s: Slot, t: Micros) {
        self.order[s.i()].last_needed = t;
    }

    pub fn cold(&self, s: Slot) -> &Cold {
        &self.cold[s.i()]
    }

    pub fn cold_mut(&mut self, s: Slot) -> &mut Cold {
        &mut self.cold[s.i()]
    }

    pub fn urgency(&self, s: Slot) -> Option<Urgency> {
        self.urgency[s.i()]
    }

    pub fn priority(&self, s: Slot) -> Option<Rank> {
        self.priority[s.i()]
    }

    /// This step's urgency and priority.
    pub fn set_rank(&mut self, s: Slot, urgency: Option<Urgency>, priority: Option<Rank>) {
        self.urgency[s.i()] = urgency;
        self.priority[s.i()] = priority;
    }

    fn has(&self, s: Slot, bit: u8) -> bool {
        self.flags[s.i()] & bit != 0
    }

    fn set(&mut self, s: Slot, bit: u8, on: bool) {
        let f = &mut self.flags[s.i()];
        *f = if on { *f | bit } else { *f & !bit };
    }

    pub fn has_gpu(&self, s: Slot) -> bool {
        self.has(s, GPU)
    }

    pub fn has_cpu(&self, s: Slot) -> bool {
        self.has(s, CPU)
    }

    pub fn has_transit(&self, s: Slot) -> bool {
        self.has(s, TRANSIT)
    }

    pub fn loading(&self, s: Slot) -> bool {
        self.has(s, LOADING)
    }

    pub fn gpu(&self, s: Slot) -> Option<Gpu> {
        self.has_gpu(s).then(|| self.gpu[s.i()])
    }

    pub fn transit(&self, s: Slot) -> Option<Transit> {
        self.has_transit(s).then(|| self.transit[s.i()])
    }

    pub fn cpu(&self, s: Slot) -> Option<CpuCopy> {
        self.has_cpu(s).then(|| {
            let c = self.cpu[s.i()];
            CpuCopy {
                gen: c.gen,
                cost: c.cost,
                epoch: c.epoch,
                delivered_before: self.has(s, DELIVERED),
            }
        })
    }

    pub fn load(&self, s: Slot) -> Load {
        if !self.loading(s) {
            return Load::Idle;
        }
        let l = self.load[s.i()];
        Load::Loading {
            gen: l.gen,
            source: if self.has(s, LOCAL) {
                Source::Local
            } else {
                Source::Network
            },
            started: l.started,
            epoch: l.epoch,
        }
    }

    pub fn set_load(&mut self, s: Slot, load: Load) {
        match load {
            Load::Idle => {
                self.set(s, LOADING, false);
                self.set(s, LOCAL, false);
            }
            Load::Loading {
                gen,
                source,
                started,
                epoch,
            } => {
                self.load[s.i()] = LoadPayload {
                    gen,
                    started,
                    epoch,
                };
                self.set(s, LOADING, true);
                self.set(s, LOCAL, source == Source::Local);
            }
        }
    }

    /// The decoded copy, of generation `gen`, reached the GPU.
    pub fn mark_delivered(&mut self, s: Slot, gen: Generation) {
        if self.has_cpu(s) && self.cpu[s.i()].gen == gen {
            self.set(s, DELIVERED, true);
        }
    }

    /// Nothing held, nothing in flight.
    pub fn is_empty(&self, s: Slot) -> bool {
        self.flags[s.i()] & (CPU | TRANSIT | GPU | LOADING) == 0
    }

    pub fn put_cpu(&mut self, s: Slot, l: &mut Ledger, c: CpuCopy) {
        self.take_cpu(s, l);
        l.decoded = l.decoded.plus(c.cost.gpu);
        self.cpu[s.i()] = CpuPayload {
            gen: c.gen,
            cost: c.cost,
            epoch: c.epoch,
        };
        self.set(s, CPU, true);
        self.set(s, DELIVERED, c.delivered_before);
    }

    pub fn take_cpu(&mut self, s: Slot, l: &mut Ledger) -> Option<CpuCopy> {
        let c = self.cpu(s)?;
        l.decoded = l.decoded.minus(c.cost.gpu);
        self.set(s, CPU, false);
        self.set(s, DELIVERED, false);
        Some(c)
    }

    pub fn put_transit(&mut self, s: Slot, l: &mut Ledger, t: Transit) {
        self.take_transit(s, l);
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].plus(t.cost.gpu);
        l.in_transit = l.in_transit.plus(t.cost.staging);
        self.transit[s.i()] = t;
        self.set(s, TRANSIT, true);
    }

    pub fn take_transit(&mut self, s: Slot, l: &mut Ledger) -> Option<Transit> {
        let t = self.transit(s)?;
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].minus(t.cost.gpu);
        l.in_transit = l.in_transit.minus(t.cost.staging);
        self.set(s, TRANSIT, false);
        Some(t)
    }

    pub fn put_gpu(&mut self, s: Slot, l: &mut Ledger, g: Gpu) {
        let class = self.class(s);
        match self.gpu(s) {
            Some(old) => l.gpu[class] = l.gpu[class].minus(old.bytes),
            None => {
                if let r @ ResourceId::Drape { .. } = self.id(s) {
                    self.gpu_drapes.insert(r);
                }
            }
        }
        l.gpu[class] = l.gpu[class].plus(g.bytes);
        self.gpu[s.i()] = g;
        self.set(s, GPU, true);
    }

    pub fn take_gpu(&mut self, s: Slot, l: &mut Ledger) -> Option<Gpu> {
        let g = self.gpu(s)?;
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].minus(g.bytes);
        self.set(s, GPU, false);
        if let r @ ResourceId::Drape { .. } = self.id(s) {
            self.gpu_drapes.remove(&r);
        }
        Some(g)
    }

    /// The decoded copy, if it is of the current epoch.
    pub fn fresh_cpu(&self, s: Slot, epoch: u64) -> Option<CpuCopy> {
        self.cpu(s).filter(|c| c.epoch == epoch)
    }

    /// Whether the GPU copy is of an older epoch.
    pub fn stale_gpu(&self, s: Slot, epoch: u64) -> bool {
        self.gpu(s).is_some_and(|g| g.epoch < epoch)
    }

    /// Failed and waiting for its retry.
    pub fn backing_off(&self, s: Slot, now: Micros) -> bool {
        self.cold(s).backing_off(now)
    }

    /// The drapes with a GPU copy in `range`, in resource order.
    pub fn gpu_drapes(
        &self,
        range: std::ops::RangeInclusive<ResourceId>,
    ) -> impl Iterator<Item = ResourceId> + '_ {
        self.gpu_drapes.range(range).copied()
    }

    /// The ordered index of GPU drapes is exactly the live drapes holding
    /// a GPU copy: checked by `audit` in debug builds.
    pub fn gpu_drapes_consistent(&self) -> bool {
        let mut n = 0usize;
        for s in self.iter() {
            if let r @ ResourceId::Drape { .. } = self.id(s) {
                if self.has_gpu(s) {
                    n += 1;
                    if !self.gpu_drapes.contains(&r) {
                        return false;
                    }
                }
            }
        }
        n == self.gpu_drapes.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::TileId;

    fn mesh(n: u64) -> ResourceId {
        ResourceId::Mesh(TileId(n))
    }

    #[test]
    fn a_freed_slot_is_the_next_taken_and_starts_empty() {
        let mut a = Arena::default();
        let mut l = Ledger::default();
        let s0 = a.find_or_insert(mesh(1), Class::TerrainMesh, 3, Micros(5));
        let s1 = a.find_or_insert(mesh(2), Class::TerrainMesh, 4, Micros(5));
        assert_ne!(s0, s1);
        assert_eq!(
            a.find_or_insert(mesh(1), Class::Drape, 9, Micros(9)),
            s0,
            "found, not inserted again"
        );
        assert_eq!(a.len(), 2);
        a.put_gpu(
            s0,
            &mut l,
            Gpu {
                gen: Generation(1),
                bytes: Bytes(10),
                epoch: 0,
            },
        );
        a.take_gpu(s0, &mut l);
        a.cold_mut(s0).failures = 3;
        a.remove(s0);
        assert_eq!(a.find(mesh(1)), None);
        assert_eq!(a.len(), 1);
        let s2 = a.find_or_insert(mesh(3), Class::Drape, 7, Micros(8));
        assert_eq!(s2, s0, "the freed slot is reused");
        assert_eq!(a.id(s2), mesh(3));
        assert_eq!(a.class(s2), Class::Drape);
        assert_eq!(a.level(s2), 7);
        assert_eq!(a.last_needed(s2), Micros(8));
        assert!(a.is_empty(s2));
        assert_eq!(a.cold(s2).failures, 0, "the old state does not leak");
        assert_eq!(a.find(mesh(2)), Some(s1));
        assert_eq!(a.iter().count(), 2);
    }

    #[test]
    fn the_flags_carry_the_optional_fields() {
        let mut a = Arena::default();
        let mut l = Ledger::default();
        let s = a.find_or_insert(mesh(1), Class::TerrainMesh, 3, Micros(5));
        let load = Load::Loading {
            gen: Generation(4),
            source: Source::Local,
            started: Micros(2),
            epoch: 1,
        };
        a.set_load(s, load);
        assert_eq!(a.load(s), load);
        a.set_load(s, Load::Idle);
        assert_eq!(a.load(s), Load::Idle);
        let c = CpuCopy {
            gen: Generation(4),
            cost: Cost::flat(Bytes(8)),
            epoch: 1,
            delivered_before: false,
        };
        a.put_cpu(s, &mut l, c);
        a.mark_delivered(s, Generation(3));
        assert_eq!(a.cpu(s), Some(c), "another generation's delivery");
        a.mark_delivered(s, Generation(4));
        assert_eq!(
            a.cpu(s),
            Some(CpuCopy {
                delivered_before: true,
                ..c
            })
        );
        assert_eq!(l.decoded, Bytes(8));
        a.take_cpu(s, &mut l);
        assert_eq!((a.cpu(s), l.decoded), (None, Bytes::ZERO));
        let cold = a.cold_mut(s);
        assert_eq!(cold.first_wanted(), None);
        cold.first_wanted_or(Micros(7));
        cold.first_wanted_or(Micros(9));
        assert_eq!(cold.first_wanted(), Some(Micros(7)));
        cold.set_unneeded_since(Some(Micros::ZERO));
        assert_eq!(cold.unneeded_since(), Some(Micros::ZERO));
        cold.set_first_wanted(None);
        assert_eq!(cold.first_wanted(), None);
        assert_eq!(cold.unneeded_since(), Some(Micros::ZERO));
    }

    #[test]
    fn the_gpu_drapes_follow_the_gpu_copies() {
        let mut a = Arena::default();
        let mut l = Ledger::default();
        let g = Gpu {
            gen: Generation(1),
            bytes: Bytes(4),
            epoch: 0,
        };
        let d = |lv| ResourceId::drape(TileId(1), crate::LayerId(0), lv);
        for lv in [5, 2, 9] {
            let s = a.find_or_insert(d(lv), Class::Drape, 1, Micros(0));
            a.put_gpu(s, &mut l, g);
        }
        let m = a.find_or_insert(mesh(1), Class::TerrainMesh, 1, Micros(0));
        a.put_gpu(m, &mut l, g);
        let all = || a.gpu_drapes(d(0)..=d(u32::MAX)).collect::<Vec<_>>();
        assert_eq!(all(), vec![d(2), d(5), d(9)], "by level, drapes only");
        let s5 = a.find(d(5)).unwrap();
        a.put_gpu(s5, &mut l, g);
        a.take_gpu(s5, &mut l);
        assert_eq!(a.gpu_drapes(d(0)..=d(u32::MAX)).count(), 2);
        assert!(a.gpu_drapes_consistent());
        assert_eq!(l.gpu[Class::Drape], Bytes(8));
    }
}
