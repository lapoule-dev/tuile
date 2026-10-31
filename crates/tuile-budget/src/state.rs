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
//!
//! The arena also keeps the eviction order up to date, rather than have a
//! step rebuild it (O1.2). A resource's eviction key — last needed, then
//! finer, then farther, then the resource — moves when the view touches it,
//! which is every step for everything drawn; so the arena files only what
//! the view left alone. The resources needed at the two most recent times
//! are *grouped*: listed by the time they were last needed, outside the
//! index. Every other resource holding a copy outside transit is *filed*: in
//! an ordered set per class and per copies held (both, GPU only, decoded
//! only), by its key, and its GPU bytes counted in its class's idle bytes
//! when it is not of the floor. When a third time comes, the older group's
//! resources not needed since are filed; a resource needed again leaves the
//! index for the newest group. So a step that draws what it drew moves
//! nothing, and one that turns files only what it left. A walk of the order
//! merges the index with the groups, sorted when a walk needs them.

// The decisions are integer arithmetic: floats stay in `imagery` and
// `priority`, quantised once, where bit-exactness is controlled.
#![deny(clippy::float_arithmetic)]

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::hash::BuildHasher;

use foldhash::fast::FixedState;
use hashbrown::HashTable;

use super::cost::Cost;
use super::priority::{PriorityKey, Urgency};
use super::tier::Tier;
use super::tier::{Presence, Source};
use super::types::ResourceId;
use super::types::{Bytes, Class, Generation, Micros, PerClass};
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
/// Needed at one of the two most recent times: listed in that time's group,
/// not filed in the eviction index.
const GROUPED: u8 = 1 << 7;

/// The eviction order: least recently needed first, then finer, then
/// farther; the resource last, so the order is total.
pub(super) type EvictKey = (Micros, Reverse<u32>, Reverse<u32>, ResourceId);

/// The copies a resource outside transit holds, as the eviction index files
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Held {
    /// A GPU copy and a decoded one.
    Both,
    /// A GPU copy only.
    Gpu,
    /// A decoded copy only.
    Cpu,
}

impl Held {
    /// Those with a GPU copy: what eviction walks.
    pub const GPU: [Held; 2] = [Held::Both, Held::Gpu];
    /// Every one.
    pub const ALL: [Held; 3] = [Held::Both, Held::Gpu, Held::Cpu];

    fn i(self) -> usize {
        match self {
            Held::Both => 0,
            Held::Gpu => 1,
            Held::Cpu => 2,
        }
    }
}

/// The resources last needed at one time.
#[derive(Debug, Default)]
struct Group {
    at: Option<Micros>,
    /// Each slot listed when it joined; a slot that left since (needed
    /// again later, freed) stays listed and is told apart by its time.
    slots: Vec<Slot>,
}

/// No slot: the end of a branch of the eviction index.
const NIL: u32 = u32::MAX;

/// A filed slot's place in its treap's heap order: a fixed hash of its
/// number, the number breaking ties. It shapes the tree, never the order.
fn heap_rank(n: u32) -> (u32, u32) {
    let mut x = n.wrapping_mul(0x9e37_79b9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 13;
    (x, n)
}

/// The eviction order, kept up to date (see the module documentation).
#[derive(Debug)]
struct EvictIndex {
    /// The filed resources, by class and copies held: the root of a treap
    /// keyed by `(EvictKey, Slot)`, whose links are a column of the arena.
    roots: PerClass<[u32; 3]>,
    /// By class, the GPU bytes of the filed resources outside the floor:
    /// none of them was needed this step, so all of them can be evicted.
    idle: PerClass<Bytes>,
    /// The newest group, and the one before.
    cur: Group,
    prev: Group,
}

impl Default for EvictIndex {
    fn default() -> Self {
        EvictIndex {
            roots: PerClass::splat([NIL; 3]),
            idle: PerClass::splat(Bytes::ZERO),
            cur: Group::default(),
            prev: Group::default(),
        }
    }
}

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
    /// A filed slot's children in its treap of the eviction index (`NIL`
    /// for none): the index costs its slots eight bytes each, and nothing
    /// to allocate as resources come and go.
    links: Vec<[u32; 2]>,
    /// The drapes with a GPU copy, in resource order: a tile's drawable
    /// drapes of one layer are a range of it, by level.
    gpu_drapes: BTreeSet<ResourceId>,
    /// The floor's last level (`BudgetConfig::pinned_level`).
    pinned: Option<u32>,
    evict: EvictIndex,
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
    + std::mem::size_of::<Cold>()
    + std::mem::size_of::<[u32; 2]>();
const _: () = assert!(SLOT_BYTES <= MAX_SLOT_BYTES);
const MAX_SLOT_BYTES: usize = 219;

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
            links: Vec::new(),
            gpu_drapes: BTreeSet::new(),
            pinned: None,
            evict: EvictIndex::default(),
        }
    }
}

impl Arena {
    /// An empty arena for a budget whose floor ends at `pinned`.
    pub fn new(pinned: Option<u32>) -> Self {
        Arena {
            pinned,
            ..Arena::default()
        }
    }

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
        // The group of `now` first: making it may file an older group, which
        // the new slot is not part of.
        self.group_at(now);
        let s = match self.free.pop() {
            Some(s) => {
                let i = s.i();
                self.id[i] = r;
                self.class[i] = class;
                self.flags[i] = LIVE | GROUPED;
                self.order[i] = order;
                self.gpu[i] = Gpu::default();
                self.transit[i] = Transit::default();
                self.cpu[i] = CpuPayload::default();
                self.load[i] = LoadPayload::default();
                self.urgency[i] = None;
                self.priority[i] = None;
                self.cold[i] = Cold::default();
                self.links[i] = [NIL; 2];
                s
            }
            None => {
                // Slots are numbered by `u32`: four billion tracked
                // resources is far past any memory a budget manages.
                let s = Slot(u32::try_from(self.id.len()).unwrap_or(u32::MAX));
                self.grow();
                self.id.push(r);
                self.class.push(class);
                self.flags.push(LIVE | GROUPED);
                self.order.push(order);
                self.gpu.push(Gpu::default());
                self.transit.push(Transit::default());
                self.cpu.push(CpuPayload::default());
                self.load.push(LoadPayload::default());
                self.urgency.push(None);
                self.priority.push(None);
                self.cold.push(Cold::default());
                self.links.push([NIL; 2]);
                s
            }
        };
        let Arena {
            hasher, index, id, ..
        } = self;
        index.insert_unique(hasher.hash_one(r), s, |x| hasher.hash_one(id[x.i()]));
        self.live += 1;
        self.join(s, now);
        s
    }

    /// Makes room for one more slot in every column. A column grows by an
    /// eighth when full, not twice: slots are never given back, so a
    /// doubling would leave up to half of every column unused for the rest
    /// of the session.
    fn grow(&mut self) {
        fn grow<T>(v: &mut Vec<T>) {
            if v.len() == v.capacity() {
                v.reserve_exact((v.capacity() / 8).max(64));
            }
        }
        grow(&mut self.id);
        grow(&mut self.class);
        grow(&mut self.flags);
        grow(&mut self.order);
        grow(&mut self.gpu);
        grow(&mut self.transit);
        grow(&mut self.cpu);
        grow(&mut self.load);
        grow(&mut self.urgency);
        grow(&mut self.priority);
        grow(&mut self.cold);
        grow(&mut self.links);
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

    #[cfg(test)]
    pub fn last_needed(&self, s: Slot) -> Micros {
        self.order[s.i()].last_needed
    }

    pub fn set_distance(&mut self, s: Slot, d: u32) {
        if self.order[s.i()].distance == d {
            return;
        }
        self.unfile(s);
        self.order[s.i()].distance = d;
        self.file(s);
    }

    /// The resource of `s` was needed at `t`: it joins the group of `t`,
    /// leaving the index if it was filed.
    pub fn touch(&mut self, s: Slot, t: Micros) {
        self.group_at(t);
        if self.grouped(s) {
            if self.order[s.i()].last_needed == t {
                return;
            }
        } else {
            self.unfile(s);
            self.set(s, GROUPED, true);
        }
        self.order[s.i()].last_needed = t;
        self.join(s, t);
    }

    /// Lists `s` in the group of `t`, which is one of the two.
    fn join(&mut self, s: Slot, t: Micros) {
        let e = &mut self.evict;
        if e.cur.at == Some(t) {
            e.cur.slots.push(s);
        } else {
            e.prev.slots.push(s);
        }
    }

    /// Makes `t` one of the two group times: a new time files the older
    /// group's resources still last needed at its time, and the newer group
    /// becomes the older.
    fn group_at(&mut self, t: Micros) {
        let e = &mut self.evict;
        if e.cur.at == Some(t) || e.prev.at == Some(t) {
            return;
        }
        let old = std::mem::take(&mut e.prev);
        if let Some(at) = old.at {
            for &s in &old.slots {
                if self.has(s, LIVE) && self.grouped(s) && self.order[s.i()].last_needed == at {
                    self.set(s, GROUPED, false);
                    self.file(s);
                }
            }
        }
        let mut slots = old.slots;
        slots.clear();
        let cur = std::mem::replace(&mut self.evict.cur, Group { at: Some(t), slots });
        self.evict.prev = cur;
    }

    /// The eviction key of the resource of `s`.
    pub fn evict_key(&self, s: Slot) -> EvictKey {
        let o = self.order[s.i()];
        (
            o.last_needed,
            Reverse(o.level),
            Reverse(o.distance),
            self.id[s.i()],
        )
    }

    /// Whether the resource of `s` is of the floor.
    pub fn is_floor(&self, s: Slot) -> bool {
        self.pinned.is_some_and(|p| self.order[s.i()].level <= p)
    }

    /// Whether `s` is grouped (needed at one of the two most recent times)
    /// rather than filed.
    pub fn grouped(&self, s: Slot) -> bool {
        self.has(s, GROUPED)
    }

    /// The grouped slots, unordered, a slot possibly more than once.
    pub fn grouped_slots(&self) -> impl Iterator<Item = Slot> + '_ {
        [&self.evict.cur, &self.evict.prev]
            .into_iter()
            .flat_map(move |g| {
                g.slots.iter().copied().filter(move |&s| {
                    self.has(s, LIVE)
                        && self.grouped(s)
                        && Some(self.order[s.i()].last_needed) == g.at
                })
            })
    }

    /// The copies `s` holds as the index files them; none in transit.
    fn held(&self, s: Slot) -> Option<Held> {
        let f = self.flags[s.i()];
        if f & TRANSIT != 0 {
            return None;
        }
        match (f & GPU != 0, f & CPU != 0) {
            (true, true) => Some(Held::Both),
            (true, false) => Some(Held::Gpu),
            (false, true) => Some(Held::Cpu),
            (false, false) => None,
        }
    }

    /// The GPU bytes a filed `s` holding `h` counts as idle.
    fn idle_bytes(&self, s: Slot, h: Held) -> Bytes {
        if h == Held::Cpu || self.is_floor(s) {
            Bytes::ZERO
        } else {
            self.gpu[s.i()].bytes
        }
    }

    /// Takes a filed `s` out of the index, before its copies or its key
    /// change.
    fn unfile(&mut self, s: Slot) {
        if self.grouped(s) {
            return;
        }
        let Some(h) = self.held(s) else {
            return;
        };
        let (k, class, idle) = (self.node_key(s.0), self.class(s), self.idle_bytes(s, h));
        let root = self.evict.roots[class][h.i()];
        self.evict.roots[class][h.i()] = self.tree_remove(root, &k);
        let e = &mut self.evict;
        e.idle[class] = e.idle[class].minus(idle);
    }

    /// Files `s` again, once its copies or its key changed.
    fn file(&mut self, s: Slot) {
        if self.grouped(s) {
            return;
        }
        let Some(h) = self.held(s) else {
            return;
        };
        let (k, class, idle) = (self.node_key(s.0), self.class(s), self.idle_bytes(s, h));
        let root = self.evict.roots[class][h.i()];
        self.evict.roots[class][h.i()] = self.tree_insert(root, s.0, &k);
        let e = &mut self.evict;
        e.idle[class] = e.idle[class].plus(idle);
    }

    /// The idle bytes of `class`: on the GPU, filed, outside the floor.
    pub fn idle(&self, class: Class) -> Bytes {
        self.evict.idle[class]
    }

    /// The first filed resource after `after` in the eviction order, of a
    /// class `class` takes, holding one of `held`.
    pub fn next_filed(
        &self,
        after: Option<&(EvictKey, Slot)>,
        class: impl Fn(Class) -> bool,
        held: &[Held],
    ) -> Option<(EvictKey, Slot)> {
        let mut best: Option<(EvictKey, Slot)> = None;
        for c in Class::ALL.into_iter().filter(|c| class(*c)) {
            for h in held {
                let n = self.tree_next(self.evict.roots[c][h.i()], after);
                if let Some(n) = n.filter(|n| best.as_ref().is_none_or(|b| n < b)) {
                    best = Some(n);
                }
            }
        }
        best
    }

    // The treaps: binary search trees on `(EvictKey, Slot)`, heaps on
    // `heap_rank`, linked through `links`. A filed slot's key does not move
    // while it is filed (`unfile` comes first), so a tree stays ordered.

    fn node_key(&self, n: u32) -> (EvictKey, Slot) {
        let s = Slot(n);
        (self.evict_key(s), s)
    }

    fn link(&self, n: u32, side: usize) -> u32 {
        self.links[Slot(n).i()][side]
    }

    fn set_link(&mut self, n: u32, side: usize, to: u32) {
        self.links[Slot(n).i()][side] = to;
    }

    /// Splits the tree at `t` into the nodes below `k` and those above
    /// (`k` is in neither).
    fn tree_split(&mut self, t: u32, k: &(EvictKey, Slot)) -> (u32, u32) {
        if t == NIL {
            return (NIL, NIL);
        }
        if self.node_key(t) < *k {
            let (l, r) = self.tree_split(self.link(t, 1), k);
            self.set_link(t, 1, l);
            (t, r)
        } else {
            let (l, r) = self.tree_split(self.link(t, 0), k);
            self.set_link(t, 0, r);
            (l, t)
        }
    }

    /// Joins two trees, every node of `a` below every node of `b`.
    fn tree_merge(&mut self, a: u32, b: u32) -> u32 {
        if a == NIL {
            return b;
        }
        if b == NIL {
            return a;
        }
        if heap_rank(a) > heap_rank(b) {
            let m = self.tree_merge(self.link(a, 1), b);
            self.set_link(a, 1, m);
            a
        } else {
            let m = self.tree_merge(a, self.link(b, 0));
            self.set_link(b, 0, m);
            b
        }
    }

    /// Puts `n`, of key `k`, into the tree at `t`; returns its root.
    fn tree_insert(&mut self, t: u32, n: u32, k: &(EvictKey, Slot)) -> u32 {
        if t == NIL {
            self.links[Slot(n).i()] = [NIL; 2];
            return n;
        }
        if heap_rank(n) > heap_rank(t) {
            let (l, r) = self.tree_split(t, k);
            self.links[Slot(n).i()] = [l, r];
            return n;
        }
        let side = usize::from(self.node_key(t) < *k);
        let c = self.tree_insert(self.link(t, side), n, k);
        self.set_link(t, side, c);
        t
    }

    /// Takes the node of key `k` out of the tree at `t`; returns its root.
    fn tree_remove(&mut self, t: u32, k: &(EvictKey, Slot)) -> u32 {
        if t == NIL {
            debug_assert!(false, "a filed slot is in the index");
            return NIL;
        }
        match k.cmp(&self.node_key(t)) {
            std::cmp::Ordering::Equal => self.tree_merge(self.link(t, 0), self.link(t, 1)),
            o => {
                let side = usize::from(o == std::cmp::Ordering::Greater);
                let c = self.tree_remove(self.link(t, side), k);
                self.set_link(t, side, c);
                t
            }
        }
    }

    /// The least node of the tree at `t` above `after` (any, if none).
    fn tree_next(&self, mut t: u32, after: Option<&(EvictKey, Slot)>) -> Option<(EvictKey, Slot)> {
        let mut best = None;
        while t != NIL {
            let k = self.node_key(t);
            if after.is_none_or(|a| k > *a) {
                best = Some(k);
                t = self.link(t, 0);
            } else {
                t = self.link(t, 1);
            }
        }
        best
    }

    /// Appends the tree at `t` in order to `out`, if it is a treap: ordered,
    /// and each node above its children in the heap order.
    fn tree_walk(&self, t: u32, out: &mut Vec<(EvictKey, Slot)>) -> bool {
        if t == NIL {
            return true;
        }
        let [l, r] = self.links[Slot(t).i()];
        let heap = [l, r]
            .iter()
            .all(|c| *c == NIL || heap_rank(*c) < heap_rank(t));
        heap && self.tree_walk(l, out) && {
            out.push(self.node_key(t));
            self.tree_walk(r, out)
        }
    }

    /// The eviction index is exactly the filed slots holding a copy outside
    /// transit, each in its class's and holdings' treap by its key, the idle
    /// bytes their sum, and every grouped slot listed in the group of its
    /// time: checked by `audit` in debug builds against a recomputation.
    pub fn evict_index_consistent(&self) -> bool {
        let mut listed: Vec<Slot> = self.grouped_slots().collect();
        listed.sort_unstable();
        listed.dedup();
        let mut idle = PerClass::splat(Bytes::ZERO);
        let mut want: PerClass<[Vec<(EvictKey, Slot)>; 3]> = PerClass::default();
        for s in self.iter() {
            let class = self.class(s);
            if self.grouped(s) {
                if listed.binary_search(&s).is_err() {
                    return false;
                }
            } else if let Some(h) = self.held(s) {
                want[class][h.i()].push(self.node_key(s.0));
                idle[class] = idle[class].plus(self.idle_bytes(s, h));
            }
        }
        let mut got = Vec::new();
        for c in Class::ALL {
            for h in Held::ALL {
                got.clear();
                if !self.tree_walk(self.evict.roots[c][h.i()], &mut got) {
                    return false;
                }
                want[c][h.i()].sort_unstable();
                if got != want[c][h.i()] {
                    return false;
                }
            }
        }
        idle == self.evict.idle
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
        self.unfile(s);
        self.drop_cpu(s, l);
        l.decoded = l.decoded.plus(c.cost.gpu);
        self.cpu[s.i()] = CpuPayload {
            gen: c.gen,
            cost: c.cost,
            epoch: c.epoch,
        };
        self.set(s, CPU, true);
        self.set(s, DELIVERED, c.delivered_before);
        self.file(s);
    }

    pub fn take_cpu(&mut self, s: Slot, l: &mut Ledger) -> Option<CpuCopy> {
        if !self.has_cpu(s) {
            return None;
        }
        self.unfile(s);
        let c = self.drop_cpu(s, l);
        self.file(s);
        c
    }

    fn drop_cpu(&mut self, s: Slot, l: &mut Ledger) -> Option<CpuCopy> {
        let c = self.cpu(s)?;
        l.decoded = l.decoded.minus(c.cost.gpu);
        self.set(s, CPU, false);
        self.set(s, DELIVERED, false);
        Some(c)
    }

    pub fn put_transit(&mut self, s: Slot, l: &mut Ledger, t: Transit) {
        self.unfile(s);
        self.drop_transit(s, l);
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].plus(t.cost.gpu);
        l.in_transit = l.in_transit.plus(t.cost.staging);
        self.transit[s.i()] = t;
        self.set(s, TRANSIT, true);
    }

    pub fn take_transit(&mut self, s: Slot, l: &mut Ledger) -> Option<Transit> {
        let t = self.drop_transit(s, l)?;
        self.file(s);
        Some(t)
    }

    fn drop_transit(&mut self, s: Slot, l: &mut Ledger) -> Option<Transit> {
        let t = self.transit(s)?;
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].minus(t.cost.gpu);
        l.in_transit = l.in_transit.minus(t.cost.staging);
        self.set(s, TRANSIT, false);
        Some(t)
    }

    pub fn put_gpu(&mut self, s: Slot, l: &mut Ledger, g: Gpu) {
        self.unfile(s);
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
        self.file(s);
    }

    pub fn take_gpu(&mut self, s: Slot, l: &mut Ledger) -> Option<Gpu> {
        let g = self.gpu(s)?;
        self.unfile(s);
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].minus(g.bytes);
        self.set(s, GPU, false);
        if let r @ ResourceId::Drape { .. } = self.id(s) {
            self.gpu_drapes.remove(&r);
        }
        self.file(s);
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

    /// Every filed resource of `class` holding one of `held`, in order.
    fn filed(a: &Arena, class: Class, held: &[Held]) -> Vec<ResourceId> {
        let mut out = Vec::new();
        let mut after = None;
        while let Some(n) = a.next_filed(after.as_ref(), |c| c == class, held) {
            out.push(n.0 .3);
            after = Some(n);
        }
        out
    }

    #[test]
    fn the_eviction_index_files_what_the_view_left_alone() {
        let mut a = Arena::new(Some(1));
        let mut l = Ledger::default();
        let g = |b| Gpu {
            gen: Generation(1),
            bytes: Bytes(b),
            epoch: 0,
        };
        let c = CpuCopy {
            gen: Generation(1),
            cost: Cost::flat(Bytes(5)),
            epoch: 0,
            delivered_before: false,
        };
        // Four meshes needed at 10: one of the floor, one decoded too.
        let s: Vec<Slot> = (1..=4)
            .map(|n| {
                a.find_or_insert(
                    mesh(n),
                    Class::TerrainMesh,
                    if n == 1 { 1 } else { 5 },
                    Micros(10),
                )
            })
            .collect();
        for (i, &x) in s.iter().enumerate() {
            a.put_gpu(x, &mut l, g(10 * (i as u64 + 1)));
        }
        a.put_cpu(s[2], &mut l, c);
        assert!(s.iter().all(|x| a.grouped(*x)), "needed at the newest time");
        assert_eq!(a.idle(Class::TerrainMesh), Bytes::ZERO);
        // At 20 the view keeps mesh 4; at 30, nothing: the group of 10 is
        // filed then, mesh 4 still grouped (20 is one of the two times).
        a.touch(s[3], Micros(20));
        a.touch(s[3], Micros(20));
        assert!(filed(&a, Class::TerrainMesh, &Held::ALL).is_empty());
        let x = a.find_or_insert(mesh(9), Class::TerrainMesh, 5, Micros(30));
        assert!(a.grouped(x));
        assert_eq!(
            filed(&a, Class::TerrainMesh, &Held::GPU),
            vec![mesh(2), mesh(3), mesh(1)],
            "finer first among equals"
        );
        assert_eq!(filed(&a, Class::TerrainMesh, &[Held::Both]), vec![mesh(3)]);
        assert!(!a.grouped(s[0]) && a.grouped(s[3]));
        assert_eq!(
            a.idle(Class::TerrainMesh),
            Bytes(20 + 30),
            "the floor is not idle"
        );
        assert!(a.evict_index_consistent());
        // A key that moves, copies that change: refiled in place.
        a.set_distance(s[1], 7);
        a.take_gpu(s[2], &mut l);
        assert_eq!(filed(&a, Class::TerrainMesh, &[Held::Cpu]), vec![mesh(3)]);
        assert_eq!(a.idle(Class::TerrainMesh), Bytes(20));
        a.put_transit(
            s[1],
            &mut l,
            Transit {
                gen: Generation(2),
                cost: Cost::flat(Bytes(20)),
                epoch: 0,
            },
        );
        assert_eq!(filed(&a, Class::TerrainMesh, &Held::GPU), vec![mesh(1)]);
        assert_eq!(a.idle(Class::TerrainMesh), Bytes::ZERO);
        a.take_transit(s[1], &mut l);
        assert_eq!(a.idle(Class::TerrainMesh), Bytes(20));
        assert!(a.evict_index_consistent());
        // Needed again: grouped, out of the index.
        a.touch(s[1], Micros(30));
        assert_eq!(filed(&a, Class::TerrainMesh, &Held::GPU), vec![mesh(1)]);
        assert_eq!(a.idle(Class::TerrainMesh), Bytes::ZERO);
        assert!(a.evict_index_consistent());
    }

    #[test]
    fn the_eviction_index_keeps_its_order_through_any_churn() {
        // Slots filed and unfiled in a scrambled order, against an ordered
        // set of the same keys.
        let mut a = Arena::new(None);
        let mut l = Ledger::default();
        let mut oracle = BTreeSet::new();
        let n = 300u64;
        let slots: Vec<Slot> = (0..n)
            .map(|i| {
                let t = Micros((i * 7919) % 97);
                let s = a.find_or_insert(mesh(i), Class::Drape, u32::try_from(i % 5).unwrap(), t);
                a.put_gpu(
                    s,
                    &mut l,
                    Gpu {
                        gen: Generation(1),
                        bytes: Bytes(1),
                        epoch: 0,
                    },
                );
                s
            })
            .collect();
        // Two new times file every slot.
        a.find_or_insert(mesh(n), Class::Drape, 0, Micros(1_000));
        a.find_or_insert(mesh(n + 1), Class::Drape, 0, Micros(1_001));
        for &s in &slots {
            oracle.insert((a.evict_key(s), s));
        }
        for (k, &s) in slots.iter().enumerate() {
            if k % 3 == 0 {
                oracle.remove(&(a.evict_key(s), s));
                a.take_gpu(s, &mut l);
            } else if k % 3 == 1 {
                oracle.remove(&(a.evict_key(s), s));
                a.set_distance(s, u32::try_from(k).unwrap());
                oracle.insert((a.evict_key(s), s));
            }
        }
        let walked: Vec<_> = {
            let mut out = Vec::new();
            let mut after = None;
            while let Some(e) = a.next_filed(after.as_ref(), |_| true, &Held::GPU) {
                out.push(e);
                after = Some(e);
            }
            out
        };
        assert_eq!(walked, oracle.into_iter().collect::<Vec<_>>());
        assert!(a.evict_index_consistent());
    }
}
