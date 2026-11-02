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
//! the next one taken), though no decision depends on a slot's number:
//! when the tracked resources fall far under the columns' room, the arena
//! renumbers its live slots in order and gives the room back
//! ([`Arena::compact`]), every decision and the whole internal state
//! unchanged.
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
//!
//! The newest group is not rewritten each step either (O1.4): its members
//! carry a mark, "needed at the newest time", rather than the time itself,
//! so a step that needs what it needed changes none of them. When the time
//! moves on, the caller names the members that the new step no longer
//! needs ([`Arena::advance`]): they alone take the old time and become the
//! older group. A time that goes back is played as before, every member
//! touched one by one ([`Arena::regress`]).
//!
//! Beyond the copies, a slot carries the marks of the sets a step keeps from
//! one step to the next — drawn, swap, chain, in flight or in transit, of
//! the floor's loads, wanted — so that the protected bytes are kept up to
//! date as copies and marks change, and every change of a slot's copies or
//! load is noted, for the step to know what changed since it last looked.

// The decisions are integer arithmetic: floats stay in `imagery` and
// `priority`, quantised once, where bit-exactness is controlled.
#![deny(clippy::float_arithmetic)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::BuildHasher;

use foldhash::fast::FixedState;
use hashbrown::HashTable;

use super::cost::Cost;
use super::priority::{PriorityKey, Urgency};
use super::sorted::SortedMap;
use super::tier::Tier;
use super::tier::{Presence, Source};
use super::types::ResourceId;
use super::types::{sat_u32, Bytes, Class, Generation, LayerId, Micros, PerClass};
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

// Which copies a slot holds, and what its payload columns mean, then the
// sets the step keeps it in: two bytes per slot, read before any payload.
/// The slot holds a tracked resource (a freed slot does not).
const LIVE: u16 = 1;
/// A decoded copy: `cpu` holds it.
const CPU: u16 = 1 << 1;
/// The decoded copy reached the GPU once (`CpuCopy::delivered_before`).
const DELIVERED: u16 = 1 << 2;
/// A copy in transit: `transit` holds it.
const TRANSIT: u16 = 1 << 3;
/// A copy on the GPU: `gpu` holds it.
const GPU: u16 = 1 << 4;
/// A load in flight: `load` holds it.
const LOADING: u16 = 1 << 5;
/// The load in flight reads a local store (`Source::Local`); else the
/// network.
const LOCAL: u16 = 1 << 6;
/// Needed at one of the two most recent times: listed in that time's group,
/// not filed in the eviction index.
const GROUPED: u16 = 1 << 7;

/// The sets a step keeps a resource in, as marks on its slot ([`Mark`]).
const DRAWN: u16 = 1 << 8;
const SWAP: u16 = 1 << 9;
const CHAIN: u16 = 1 << 10;
const ACTIVE: u16 = 1 << 11;
const FLOOR_SET: u16 = 1 << 12;
const WANTED: u16 = 1 << 13;

/// The sets a step keeps from one step to the next, as marks on the slots
/// of their members: what protects a resource, and what the view wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mark {
    /// Drawn: a mesh of the draw set, or a drape one wears.
    Drawn,
    /// The drawable half of a swap the view waits on.
    Swap,
    /// An ancestor of the drawn cut, drawable (a swap's member too).
    Chain,
    /// A load in flight or a copy in transit, until the prune notes it is
    /// neither ([`Arena::active`]).
    Active,
    /// A load started at the floor's levels (`Budget::floor_set`).
    FloorSet,
    /// Wanted or drawn at the last step (`Budget::wanted_last`).
    Wanted,
}

impl Mark {
    fn bit(self) -> u16 {
        match self {
            Mark::Drawn => DRAWN,
            Mark::Swap => SWAP,
            Mark::Chain => CHAIN,
            Mark::Active => ACTIVE,
            Mark::FloorSet => FLOOR_SET,
            Mark::Wanted => WANTED,
        }
    }
}

/// The time of the newest group, standing for it in a member's
/// `last_needed`: the group's members do not move when the time does.
const CUR: Micros = Micros(u64::MAX);

/// What changed of a resource ([`Arena::changes`]), as bits.
pub(super) struct Change;

impl Change {
    /// Its GPU copy: whether it is drawable, and its tier.
    pub const GPU: u8 = 1;
    /// Its decoded copy: its tier.
    pub const CPU: u8 = 1 << 1;
    /// Its load, its transit, its slot made: its work only.
    pub const LOAD: u8 = 1 << 2;
    /// Its tier otherwise: a store missed it, its slot went.
    pub const TIER: u8 = 1 << 3;
}

/// What [`Arena::advance`] did with a new time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Advance {
    /// The groups moved on: the leavers left.
    Moved,
    /// The newest group's own time: nothing moved, the leavers stay.
    Same,
    /// An earlier time: nothing moved; [`Arena::regress`] plays it.
    Back,
}

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
    /// again later, freed) stays listed and is told apart by its time —
    /// [`CUR`] for the newest group's members.
    slots: Vec<Slot>,
}

/// The eviction order, kept up to date (see the module documentation).
#[derive(Debug)]
struct EvictIndex {
    /// The filed resources, by class and copies held, each set ordered by
    /// `(EvictKey, Slot)`: the standard library's ordered set, no
    /// structure of our own.
    filed: PerClass<[BTreeSet<(EvictKey, Slot)>; 3]>,
    /// By class, the GPU bytes of the filed resources outside the floor:
    /// none of them was needed this step, so all of them can be evicted.
    idle: PerClass<Bytes>,
    /// The newest group, and the one before.
    cur: Group,
    prev: Group,
    /// The newest group's members (live, marked [`CUR`]).
    cur_len: usize,
    /// While a time that went back is played: members carry their times,
    /// the newest group's too, as before the mark (`regress`).
    explicit: bool,
}

impl Default for EvictIndex {
    fn default() -> Self {
        EvictIndex {
            filed: PerClass::default(),
            idle: PerClass::splat(Bytes::ZERO),
            cur: Group::default(),
            prev: Group::default(),
            cur_len: 0,
            explicit: false,
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
    /// Its place in [`Arena::active`] while it is marked `Active`. In the
    /// room the other fields leave (`Cold` stays 48 bytes).
    active_at: u32,
}

const _: () = assert!(std::mem::size_of::<Cold>() == 48);

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
    /// The slots marked `Active` — a load in flight or a copy in transit,
    /// until the prune notes they are neither — in two runs: those still
    /// loading or in transit (`..active_busy`), then those that are
    /// neither, moved there the moment their last load or transit ended
    /// (each slot knows its place, `Cold::active_at`). The prune drops the
    /// second run whole, without a scan; no decision reads the order.
    active: Vec<Slot>,
    active_busy: usize,
    /// Live slots.
    live: usize,
    id: Vec<ResourceId>,
    class: Vec<Class>,
    flags: Vec<u16>,
    order: Vec<Order>,
    gpu: Vec<Gpu>,
    /// The copies in transit, by slot: few at a time, so kept apart.
    transit: SortedMap<u32, Transit>,
    cpu: Vec<CpuPayload>,
    /// The loads in flight, by slot: few at a time, so kept apart.
    load: SortedMap<u32, LoadPayload>,
    /// This step's urgency and priority of the slots with work, by slot:
    /// few, so kept apart rather than in two columns.
    ranks: SortedMap<u32, (Urgency, Rank)>,
    cold: Vec<Cold>,
    /// The drapes with a GPU copy, in resource order: a tile's drawable
    /// drapes of one layer are a range of it, by level.
    gpu_drapes: BTreeSet<ResourceId>,
    /// The floor's last level (`BudgetConfig::pinned_level`).
    pinned: Option<u32>,
    evict: EvictIndex,
    /// Every resource whose copies, load or tier changed, in order, with
    /// what changed ([`Change`]), since the step last took them
    /// ([`Arena::changes`]); a resource may come twice.
    changed: Vec<(ResourceId, u8)>,
    /// By class, the bytes of the protected resources (`protection_of`):
    /// kept up to date as copies and marks change.
    protected: PerClass<Bytes>,
    /// The GPU copies by class, layer and content epoch: whether any is of
    /// an older epoch than its own, without a scan.
    gpu_epochs: BTreeMap<(Class, Option<LayerId>, u64), u32>,
    /// Slots that held nothing at some point since the prune last looked:
    /// the only ones a prune can free.
    emptied: Vec<Slot>,
}

/// The bytes one slot holds across the columns, its index and free-list
/// entries aside. It holds no `usize`, so the bound holds on wasm32 too:
/// every full scan walks the slots, and they are the bulk of the bytes per
/// tracked resource (240 B as one entry of an ordered map, its key apart).
pub(super) const SLOT_BYTES: usize = std::mem::size_of::<ResourceId>()
    + std::mem::size_of::<Class>()
    + std::mem::size_of::<u16>()
    + std::mem::size_of::<Order>()
    + std::mem::size_of::<Gpu>()
    + std::mem::size_of::<CpuPayload>()
    + std::mem::size_of::<Cold>();
const _: () = assert!(SLOT_BYTES <= MAX_SLOT_BYTES);
const MAX_SLOT_BYTES: usize = 139;

/// [`Arena::compact`]'s number for a slot that held nothing.
pub(super) const DEAD: u32 = u32::MAX;

/// The arena gives its columns' room back ([`Arena::compact`]) when the
/// tracked resources used less than a quarter of it ...
pub(super) const SHRINK_BELOW: usize = 4;
/// ... at this many prunes in a row (one every eighth step: 64 steps,
/// about a second at 60 Hz, so that a view passing through does not
/// compact). Measured: the five-minute soak, the tour, the pressure
/// scenario and P6.4's run never compact; the flight that grows the
/// tracked set to 4 889 and falls back to 995 compacts once ...
pub(super) const SHRINK_AFTER: u32 = 8;
/// ... down to twice the tracked resources: the columns grow by an eighth
/// when full, so the room given back is taken again only once the tracked
/// set has doubled, and a set that shrank by four is not compacted again
/// before it shrinks by two more.
pub(super) const SHRINK_TO: usize = 2;
/// Never below this many slots: a small arena is not worth compacting.
pub(super) const SHRINK_FLOOR: usize = 256;

impl Default for Arena {
    fn default() -> Self {
        Arena {
            hasher: FixedState::with_seed(SEED),
            index: HashTable::new(),
            free: Vec::new(),
            active: Vec::new(),
            active_busy: 0,
            live: 0,
            id: Vec::new(),
            class: Vec::new(),
            flags: Vec::new(),
            order: Vec::new(),
            gpu: Vec::new(),
            transit: SortedMap::default(),
            cpu: Vec::new(),
            load: SortedMap::default(),
            ranks: SortedMap::default(),
            cold: Vec::new(),
            gpu_drapes: BTreeSet::new(),
            pinned: None,
            evict: EvictIndex::default(),
            changed: Vec::new(),
            protected: PerClass::splat(Bytes::ZERO),
            gpu_epochs: BTreeMap::new(),
            emptied: Vec::new(),
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

    /// What its maps spent keeping their order since the last call.
    pub fn take_upkeep(&mut self) -> u64 {
        self.transit
            .take_upkeep()
            .saturating_add(self.load.take_upkeep())
            .saturating_add(self.ranks.take_upkeep())
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
    /// `class` at `level`, first needed `now`, holding nothing. The newest
    /// group is `now`'s ([`Arena::advance`]).
    pub fn find_or_insert(&mut self, r: ResourceId, class: Class, level: u32, now: Micros) -> Slot {
        if let Some(s) = self.find(r) {
            return s;
        }
        let explicit = self.evict.explicit;
        let order = Order {
            last_needed: if explicit { now } else { CUR },
            level,
            distance: 0,
        };
        if explicit {
            // The group of `now` first: making it may file an older group,
            // which the new slot is not part of.
            self.group_at(now);
        } else {
            debug_assert_eq!(
                self.evict.cur.at,
                Some(now),
                "a slot joins the newest group"
            );
        }
        let s = match self.free.pop() {
            Some(s) => {
                let i = s.i();
                self.id[i] = r;
                self.class[i] = class;
                self.flags[i] = LIVE | GROUPED;
                self.order[i] = order;
                self.gpu[i] = Gpu::default();
                self.cpu[i] = CpuPayload::default();
                self.cold[i] = Cold::default();
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
                self.cpu.push(CpuPayload::default());
                self.cold.push(Cold::default());
                s
            }
        };
        let Arena {
            hasher, index, id, ..
        } = self;
        index.insert_unique(hasher.hash_one(r), s, |x| hasher.hash_one(id[x.i()]));
        self.live += 1;
        if explicit {
            self.join(s, now);
        } else {
            self.evict.cur.slots.push(s);
            self.evict.cur_len += 1;
        }
        self.changed.push((r, Change::LOAD));
        self.emptied.push(s);
        s
    }

    /// Makes room for one more slot in every column. A column grows by an
    /// eighth when full, not twice: the room is given back only once the
    /// tracked resources fell under a quarter of it ([`Arena::compact`]),
    /// so a doubling would leave up to half of every column unused until
    /// then.
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
        grow(&mut self.cpu);
        grow(&mut self.cold);
    }

    /// Whether the columns' room is far more than the tracked resources
    /// use: over [`SHRINK_FLOOR`] slots, and more than [`SHRINK_BELOW`]
    /// times the live ones.
    pub fn sparse(&self) -> bool {
        let room = self.room();
        room > SHRINK_FLOOR && room > SHRINK_BELOW.saturating_mul(self.live)
    }

    /// The slots the columns have room for before they grow.
    pub fn room(&self) -> usize {
        self.id.capacity()
    }

    /// Gives the columns' room back: the live slots moved down, in order,
    /// to `0..len()`, every column cut to [`SHRINK_TO`] times as many
    /// slots ([`SHRINK_FLOOR`] at least), the hash index rebuilt at that
    /// size, the free list emptied. `map` gets each old slot's new number,
    /// [`DEAD`] for a slot that held nothing, for the holders of slots
    /// outside the arena to follow ([`Arena::moved`]). Returns the entries
    /// moved: the slots walked, the index's entries, the filed sets' and
    /// the lists' (the maps keyed by slot count their own upkeep).
    ///
    /// The live slots keep their order, so every walk in slot order — the
    /// columns, the sets keyed by slot, the eviction index's ties (none:
    /// an [`EvictKey`] ends with the resource) — finds them in the same
    /// order. The slots taken next differ (the end of the columns, not the
    /// last freed): no decision reads a slot's number.
    pub fn compact(&mut self, map: &mut Vec<u32>) -> u64 {
        let n = self.id.len();
        map.clear();
        map.reserve_exact(n);
        let mut moved = n as u64;
        let mut w = 0usize;
        for i in 0..n {
            if self.flags[i] & LIVE == 0 {
                map.push(DEAD);
                continue;
            }
            map.push(u32::try_from(w).unwrap_or(DEAD));
            if w != i {
                self.id.swap(w, i);
                self.class.swap(w, i);
                self.flags.swap(w, i);
                self.order.swap(w, i);
                self.gpu.swap(w, i);
                self.cpu.swap(w, i);
                self.cold.swap(w, i);
            }
            w += 1;
        }
        debug_assert_eq!(w, self.live, "every live slot moved");
        let room = SHRINK_TO.saturating_mul(w).max(SHRINK_FLOOR);
        fn cut<T>(v: &mut Vec<T>, len: usize, room: usize) {
            v.truncate(len);
            v.shrink_to(room);
        }
        cut(&mut self.id, w, room);
        cut(&mut self.class, w, room);
        cut(&mut self.flags, w, room);
        cut(&mut self.order, w, room);
        cut(&mut self.gpu, w, room);
        cut(&mut self.cpu, w, room);
        cut(&mut self.cold, w, room);
        // No slot is free: the list is emptied and its room given back
        // with the columns' (keeping room for every live slot to be freed
        // costs 4 B a slot, which the shrunk budget's ratchet counts; the
        // list grows again by doubling, a handful of allocations over a
        // session, none on a still step).
        self.free.clear();
        self.free.shrink_to_fit();
        let Arena { hasher, id, .. } = &*self;
        let mut index = HashTable::with_capacity(room);
        for (i, r) in id.iter().enumerate() {
            let s = Slot(u32::try_from(i).unwrap_or(DEAD));
            index.insert_unique(hasher.hash_one(*r), s, |x| hasher.hash_one(id[x.i()]));
        }
        moved += w as u64;
        self.index = index;
        // The sets keyed by slot: renumbered in place, their order kept
        // (each map counts the pass in its upkeep).
        let to = |n: &u32| map.get(*n as usize).copied().unwrap_or(DEAD);
        self.transit.rekey(to);
        self.load.rekey(to);
        self.ranks.rekey(to);
        for c in Class::ALL {
            for set in &mut self.evict.filed[c] {
                moved += set.len() as u64;
                *set = std::mem::take(set)
                    .into_iter()
                    .map(|(k, s)| (k, Slot(map[s.i()])))
                    .collect();
            }
        }
        // The lists: the slots that held nothing dropped (they name no
        // resource; a list's filter would skip them).
        moved += Self::moved(map, &mut self.evict.cur.slots);
        moved += Self::moved(map, &mut self.evict.prev.slots);
        moved += Self::moved(map, &mut self.emptied);
        // An active slot is live (only an empty slot is freed): none is
        // dropped, so each keeps its place.
        moved += Self::moved(map, &mut self.active);
        debug_assert!(
            self.active_consistent(),
            "the active slots keep their places"
        );
        moved
    }

    /// `slots` renumbered by a [`Arena::compact`]'s `map`, in order, the
    /// slots that held nothing dropped; its room cut to [`SHRINK_TO`] times
    /// what it holds. Returns the entries walked.
    pub fn moved(map: &[u32], slots: &mut Vec<Slot>) -> u64 {
        let walked = slots.len() as u64;
        slots.retain_mut(|s| match map.get(s.i()) {
            Some(&n) if n != DEAD => {
                *s = Slot(n);
                true
            }
            _ => false,
        });
        slots.shrink_to(SHRINK_TO.saturating_mul(slots.len()));
        walked
    }

    /// The new number of `s` after a [`Arena::compact`]'s `map`, if it
    /// held a resource.
    pub fn moved_slot(map: &[u32], s: Slot) -> Option<Slot> {
        map.get(s.i()).filter(|n| **n != DEAD).map(|n| Slot(*n))
    }

    /// Stops tracking the resource of `s`, which holds nothing; its slot
    /// is the next one taken.
    pub fn remove(&mut self, s: Slot) {
        debug_assert!(self.is_empty(s), "a slot is freed holding nothing");
        let h = self.hasher.hash_one(self.id[s.i()]);
        if let Ok(e) = self.index.find_entry(h, |x| *x == s) {
            e.remove();
        }
        if self.is_cur(s) {
            self.evict.cur_len -= 1;
        }
        self.ranks.remove(&s.0);
        // Untracked, it is asked of the presence index again.
        self.changed.push((self.id[s.i()], Change::TIER));
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

    /// When the resource of `s` was last needed.
    pub fn last_needed(&self, s: Slot) -> Micros {
        match self.order[s.i()].last_needed {
            CUR => self.evict.cur.at.unwrap_or(Micros::ZERO),
            t => t,
        }
    }

    /// Whether `s` is a member of the newest group.
    fn is_cur(&self, s: Slot) -> bool {
        let f = self.flags[s.i()];
        f & LIVE != 0 && f & GROUPED != 0 && self.order[s.i()].last_needed == CUR
    }

    /// The newest group's time, once a step made one.
    pub fn newest(&self) -> Option<Micros> {
        self.evict.cur.at
    }

    /// Whether `s` is a member of the newest group.
    pub fn is_newest(&self, s: Slot) -> bool {
        self.is_cur(s)
    }

    /// How many members the newest group has.
    pub fn newest_len(&self) -> usize {
        self.evict.cur_len
    }

    /// Whether `s` holds a tracked resource.
    pub fn live_slot(&self, s: Slot) -> bool {
        self.has(s, LIVE)
    }

    /// Level, distance and last needed (a newest group's member at the
    /// group's time), for the test-only snapshot.
    pub fn order_fields(&self, s: Slot) -> (u32, u32, Micros) {
        let o = self.order[s.i()];
        (o.level, o.distance, self.last_needed(s))
    }

    pub fn set_distance(&mut self, s: Slot, d: u32) {
        if self.order[s.i()].distance == d {
            return;
        }
        self.unfile(s);
        self.order[s.i()].distance = d;
        self.file(s);
    }

    /// The resource of `s` was needed at `t`, the newest group's time (once
    /// [`Arena::advance`] made it so): it joins the newest group, leaving
    /// the index if it was filed. A member stays as it is.
    pub fn touch(&mut self, s: Slot, t: Micros) {
        if self.evict.explicit {
            self.touch_explicit(s, t);
            return;
        }
        debug_assert_eq!(self.evict.cur.at, Some(t), "touched at the newest time");
        if self.is_cur(s) {
            return;
        }
        if !self.grouped(s) {
            self.unfile(s);
            self.set(s, GROUPED, true);
        }
        self.order[s.i()].last_needed = CUR;
        self.evict.cur.slots.push(s);
        self.evict.cur_len += 1;
    }

    /// `touch` with the time written in the slot: the group of `t` made
    /// first, the slot joining the group of its time.
    fn touch_explicit(&mut self, s: Slot, t: Micros) {
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

    /// The newest time is `t`. Later than the newest group's: the older
    /// group's resources not needed since are filed; `leavers` — the newest
    /// group's members a step at `t` no longer needs, every one of them —
    /// become the older group, at the newest group's time; the other
    /// members stay, now members at `t`, without a write. The newest
    /// group's time itself: nothing moves. Earlier: nothing moves either,
    /// and [`Arena::regress`] must play the step.
    pub fn advance(&mut self, t: Micros, leavers: &[Slot]) -> Advance {
        debug_assert!(!self.evict.explicit, "a time gone back is settled first");
        let at = self.evict.cur.at;
        match at {
            Some(a) if t == a => return Advance::Same,
            Some(a) if t < a => return Advance::Back,
            _ => {}
        }
        let old = std::mem::take(&mut self.evict.prev);
        if let Some(pat) = old.at {
            for &s in &old.slots {
                if self.has(s, LIVE) && self.grouped(s) && self.order[s.i()].last_needed == pat {
                    self.set(s, GROUPED, false);
                    self.file(s);
                }
            }
        }
        let mut slots = old.slots;
        slots.clear();
        if let Some(a) = at {
            for &s in leavers {
                debug_assert!(self.is_cur(s), "a leaver is a member of the newest group");
                self.order[s.i()].last_needed = a;
                slots.push(s);
            }
            self.evict.cur_len -= leavers.len();
        } else {
            debug_assert!(leavers.is_empty(), "no group, no member");
        }
        self.evict.prev = Group { at, slots };
        self.evict.cur.at = Some(t);
        // The newest group's list keeps the members that left it: dropped
        // once they outnumber the members.
        if self.evict.cur.slots.len() > 2 * self.evict.cur_len + 64 {
            self.compact_newest();
        }
        Advance::Moved
    }

    /// The newest group's list, its members only, each once.
    fn compact_newest(&mut self) {
        let mut slots = std::mem::take(&mut self.evict.cur.slots);
        slots.retain(|&s| self.is_cur(s));
        slots.sort_unstable();
        slots.dedup();
        self.evict.cur.slots = slots;
    }

    /// A step at a time earlier than the newest group's: every member of the
    /// newest group takes its time, and until [`Arena::settle`] the groups
    /// are kept as before the mark — the caller touches every resource the
    /// step needs. Returns the work: the members written.
    pub fn regress(&mut self) -> usize {
        let at = self.evict.cur.at.unwrap_or(Micros::ZERO);
        let slots = std::mem::take(&mut self.evict.cur.slots);
        for &s in &slots {
            if self.is_cur(s) {
                self.order[s.i()].last_needed = at;
            }
        }
        let n = slots.len();
        self.evict.cur.slots = slots;
        self.evict.cur_len = 0;
        self.evict.explicit = true;
        n
    }

    /// Ends a step [`Arena::regress`] began: the newest group's resources
    /// marked members again. Returns them, for the caller to know whom the
    /// next step may leave.
    pub fn settle(&mut self, out: &mut Vec<Slot>) {
        if !self.evict.explicit {
            return;
        }
        let at = self.evict.cur.at;
        let mut slots = std::mem::take(&mut self.evict.cur.slots);
        slots.retain(|&s| {
            self.has(s, LIVE) && self.grouped(s) && Some(self.order[s.i()].last_needed) == at
        });
        slots.sort_unstable();
        slots.dedup();
        for &s in &slots {
            self.order[s.i()].last_needed = CUR;
        }
        self.evict.cur_len = slots.len();
        out.extend_from_slice(&slots);
        self.evict.cur.slots = slots;
        self.evict.explicit = false;
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
            self.last_needed(s),
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
        let e = &self.evict;
        let newest = e.cur.slots.iter().copied().filter(move |&s| {
            self.has(s, LIVE) && self.grouped(s) && {
                let t = self.order[s.i()].last_needed;
                t == CUR || (e.explicit && Some(t) == e.cur.at)
            }
        });
        let older = e.prev.slots.iter().copied().filter(move |&s| {
            self.has(s, LIVE) && self.grouped(s) && Some(self.order[s.i()].last_needed) == e.prev.at
        });
        newest.chain(older)
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
        let e = &mut self.evict;
        let was = e.filed[class][h.i()].remove(&k);
        debug_assert!(was, "a filed slot is in the index");
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
        let e = &mut self.evict;
        e.filed[class][h.i()].insert(k);
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
                let set = &self.evict.filed[c][h.i()];
                let n = match after {
                    Some(a) => set
                        .range((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded))
                        .next(),
                    None => set.first(),
                }
                .copied();
                if let Some(n) = n.filter(|n| best.as_ref().is_none_or(|b| n < b)) {
                    best = Some(n);
                }
            }
        }
        best
    }

    fn node_key(&self, n: u32) -> (EvictKey, Slot) {
        let s = Slot(n);
        (self.evict_key(s), s)
    }

    /// The eviction index is exactly the filed slots holding a copy outside
    /// transit, each in its class's and holdings' ordered set by its key, the idle
    /// bytes their sum, and every grouped slot listed in the group of its
    /// time: checked by `audit` in debug builds against a recomputation.
    pub fn evict_index_consistent(&self) -> bool {
        let mut listed: Vec<Slot> = self.grouped_slots().collect();
        listed.sort_unstable();
        listed.dedup();
        // The newest group's members are counted, and listed.
        let members = self.iter().filter(|&s| self.is_cur(s)).count();
        if members != self.evict.cur_len
            || self
                .iter()
                .any(|s| self.is_cur(s) && !self.evict.cur.slots.contains(&s))
        {
            return false;
        }
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
        for c in Class::ALL {
            for h in Held::ALL {
                want[c][h.i()].sort_unstable();
                if !self.evict.filed[c][h.i()].iter().eq(want[c][h.i()].iter()) {
                    return false;
                }
            }
        }
        idle == self.evict.idle
    }

    /// The protected bytes and the GPU copies by epoch are those a scan
    /// finds: checked by `audit` in debug builds.
    pub fn sums_consistent(&self) -> bool {
        let mut protected = PerClass::splat(Bytes::ZERO);
        let mut epochs: BTreeMap<(Class, Option<LayerId>, u64), u32> = BTreeMap::new();
        for s in self.iter() {
            let c = self.class(s);
            protected[c] = protected[c].plus(self.protection_bytes(s));
            if let Some(g) = self.gpu(s) {
                *epochs.entry(self.epoch_key(s, g.epoch)).or_insert(0) += 1;
            }
        }
        let transits = self.iter().filter(|&s| self.has_transit(s)).map(|s| s.0);
        let loads = self.iter().filter(|&s| self.loading(s)).map(|s| s.0);
        protected == self.protected
            && epochs == self.gpu_epochs
            && transits.eq(self.transit.keys().copied())
            && loads.eq(self.load.keys().copied())
    }

    pub fn cold(&self, s: Slot) -> &Cold {
        &self.cold[s.i()]
    }

    pub fn cold_mut(&mut self, s: Slot) -> &mut Cold {
        &mut self.cold[s.i()]
    }

    pub fn urgency(&self, s: Slot) -> Option<Urgency> {
        self.ranks.get(&s.0).map(|r| r.0)
    }

    pub fn priority(&self, s: Slot) -> Option<Rank> {
        self.ranks.get(&s.0).map(|r| r.1)
    }

    /// This step's urgency and priority.
    pub fn set_rank(&mut self, s: Slot, urgency: Option<Urgency>, priority: Option<Rank>) {
        match (urgency, priority) {
            (Some(u), Some(p)) => {
                self.ranks.insert(s.0, (u, p));
            }
            _ => {
                self.ranks.remove(&s.0);
            }
        }
    }

    fn has(&self, s: Slot, bit: u16) -> bool {
        self.flags[s.i()] & bit != 0
    }

    fn set(&mut self, s: Slot, bit: u16, on: bool) {
        let f = &mut self.flags[s.i()];
        *f = if on { *f | bit } else { *f & !bit };
        if bit & (LOADING | TRANSIT) != 0 && *f & ACTIVE != 0 {
            self.place_active(s);
        }
    }

    /// Lists `s` as active (marked `Active`), in the run its load and
    /// transit put it in; a slot listed already stays where it is.
    pub fn list_active(&mut self, s: Slot) {
        if self.has(s, ACTIVE) {
            return;
        }
        self.mark(s, Mark::Active, true);
        self.cold[s.i()].active_at = sat_u32(self.active.len());
        self.active.push(s);
        self.place_active(s);
    }

    /// Moves the active slot `s` into the run its load and transit say.
    fn place_active(&mut self, s: Slot) {
        let busy = self.has(s, LOADING) || self.has(s, TRANSIT);
        let at = self.cold[s.i()].active_at as usize;
        if busy && at >= self.active_busy {
            self.swap_active(at, self.active_busy);
            self.active_busy += 1;
        } else if !busy && at < self.active_busy {
            self.active_busy -= 1;
            self.swap_active(at, self.active_busy);
        }
    }

    fn swap_active(&mut self, i: usize, j: usize) {
        self.active.swap(i, j);
        self.cold[self.active[i].i()].active_at = sat_u32(i);
        self.cold[self.active[j].i()].active_at = sat_u32(j);
    }

    /// The slots marked `Active`, in no order a decision may read.
    pub fn active(&self) -> &[Slot] {
        &self.active
    }

    /// Drops from the active slots those neither loading nor in transit,
    /// their marks taken off, into `out`; a list emptied gives its room
    /// back, and one a burst left larger than twice its use, the excess.
    pub fn drop_idle_active(&mut self, out: &mut Vec<Slot>) {
        out.clear();
        out.extend_from_slice(&self.active[self.active_busy..]);
        self.active.truncate(self.active_busy);
        for &s in out.iter() {
            self.mark(s, Mark::Active, false);
        }
        if out.is_empty() {
            return;
        }
        if self.active.is_empty() {
            self.active = Vec::new();
        } else {
            let n = self.active.len();
            super::sorted::trim(&mut self.active, n);
        }
    }

    /// Whether every active slot is marked, knows its place, and sits in
    /// the run its load and transit say.
    pub fn active_consistent(&self) -> bool {
        self.active.iter().enumerate().all(|(i, &s)| {
            self.has(s, ACTIVE)
                && self.cold[s.i()].active_at as usize == i
                && (i < self.active_busy) == (self.has(s, LOADING) || self.has(s, TRANSIT))
        }) && self.active_busy <= self.active.len()
    }

    /// Whether `s` carries `m`.
    pub fn marked(&self, s: Slot, m: Mark) -> bool {
        self.has(s, m.bit())
    }

    /// Puts `m` on `s`, or takes it off; the protected bytes follow.
    pub fn mark(&mut self, s: Slot, m: Mark, on: bool) {
        if self.has(s, m.bit()) == on {
            return;
        }
        let before = self.protection_bytes(s);
        self.set(s, m.bit(), on);
        self.reprotect(s, before);
    }

    /// The bytes `s` adds to its class's protected bytes: its GPU and
    /// in-transit bytes when it is protected — a member of the drawn, swap,
    /// active or floor sets, and drawn, in a swap, in transit, or of the
    /// floor with a copy on or bound for the GPU (`Budget::protection_of`).
    fn protection_bytes(&self, s: Slot) -> Bytes {
        let f = self.flags[s.i()];
        if f & (DRAWN | SWAP | ACTIVE | FLOOR_SET) == 0 {
            return Bytes::ZERO;
        }
        let floor = self.is_floor(s) && f & (GPU | TRANSIT) != 0;
        if !floor && f & (DRAWN | SWAP | TRANSIT) == 0 {
            return Bytes::ZERO;
        }
        let gpu = if f & GPU != 0 {
            self.gpu[s.i()].bytes
        } else {
            Bytes::ZERO
        };
        let transit = if f & TRANSIT != 0 {
            self.transit.get(&s.0).map_or(Bytes::ZERO, |t| t.cost.gpu)
        } else {
            Bytes::ZERO
        };
        gpu.plus(transit)
    }

    /// `s` changed: its protected bytes were `before`.
    fn reprotect(&mut self, s: Slot, before: Bytes) {
        let after = self.protection_bytes(s);
        if after != before {
            let c = self.class(s);
            self.protected[c] = self.protected[c].minus(before).plus(after);
        }
    }

    /// By class, the bytes of the protected resources.
    pub fn protected(&self) -> PerClass<Bytes> {
        self.protected
    }

    /// The resources with a copy in transit, in order.
    pub fn transits(&self) -> impl Iterator<Item = ResourceId> + '_ {
        self.transit.keys().map(|n| self.id[Slot(*n).i()])
    }

    /// How many resources have a copy in transit.
    pub fn transits_len(&self) -> usize {
        self.transit.len()
    }

    /// The resources whose copies, load or tier changed since the step
    /// last took them, in order, with what changed (a resource may come
    /// twice).
    pub fn changes(&self) -> &[(ResourceId, u8)] {
        &self.changed
    }

    /// The tier of the resource of `s` changed outside its copies: a store
    /// missed it.
    pub fn note_tier(&mut self, s: Slot) {
        self.note(s, Change::TIER);
    }

    /// Forgets the changes up to `n`.
    pub fn forget_changes(&mut self, n: usize) {
        self.changed.drain(..n.min(self.changed.len()));
    }

    /// The slots emptied since the last call, sorted, each once, as many
    /// as are still live and empty: the only ones a prune can free.
    pub fn take_emptied(&mut self, out: &mut Vec<Slot>) {
        out.clear();
        out.extend(
            self.emptied
                .drain(..)
                .filter(|&s| self.flags[s.i()] & LIVE != 0),
        );
        out.retain(|&s| self.is_empty(s));
        out.sort_unstable();
        out.dedup();
    }

    /// The GPU copies of a content key (class, and layer for drapes), by
    /// epoch, with how many hold it.
    pub fn gpu_epochs(&self) -> impl Iterator<Item = ((Class, Option<LayerId>, u64), u32)> + '_ {
        self.gpu_epochs.iter().map(|(k, n)| (*k, *n))
    }

    /// The content key the GPU copy of `s` is counted under.
    fn epoch_key(&self, s: Slot, epoch: u64) -> (Class, Option<LayerId>, u64) {
        (self.class(s), self.id(s).layer(), epoch)
    }

    /// A GPU copy of epoch `epoch` came (`+1`) or went (`-1`).
    fn count_epoch(&mut self, s: Slot, epoch: u64, came: bool) {
        let k = self.epoch_key(s, epoch);
        if came {
            *self.gpu_epochs.entry(k).or_insert(0) += 1;
        } else if let Some(n) = self.gpu_epochs.get_mut(&k) {
            *n -= 1;
            if *n == 0 {
                self.gpu_epochs.remove(&k);
            }
        }
    }

    /// `what` changed of `s`: noted, and kept for the prune if it now holds
    /// nothing.
    fn note(&mut self, s: Slot, what: u8) {
        self.changed.push((self.id[s.i()], what));
        if self.is_empty(s) {
            self.emptied.push(s);
        }
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
        self.transit.get(&s.0).copied()
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
        let Some(&l) = self.load.get(&s.0) else {
            return Load::Idle;
        };
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
        self.note(s, Change::LOAD);
        match load {
            Load::Idle => {
                self.load.remove(&s.0);
                self.set(s, LOADING, false);
                self.set(s, LOCAL, false);
                if self.is_empty(s) {
                    self.emptied.push(s);
                }
            }
            Load::Loading {
                gen,
                source,
                started,
                epoch,
            } => {
                self.load.insert(
                    s.0,
                    LoadPayload {
                        gen,
                        started,
                        epoch,
                    },
                );
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
        self.note(s, Change::CPU);
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
        self.note(s, Change::CPU);
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
        self.note(s, Change::LOAD);
        let before = self.protection_bytes(s);
        self.unfile(s);
        self.drop_transit(s, l);
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].plus(t.cost.gpu);
        l.in_transit = l.in_transit.plus(t.cost.staging);
        self.transit.insert(s.0, t);
        self.set(s, TRANSIT, true);
        self.reprotect(s, before);
    }

    pub fn take_transit(&mut self, s: Slot, l: &mut Ledger) -> Option<Transit> {
        let before = self.protection_bytes(s);
        let t = self.drop_transit(s, l)?;
        self.file(s);
        self.reprotect(s, before);
        self.note(s, Change::LOAD);
        Some(t)
    }

    fn drop_transit(&mut self, s: Slot, l: &mut Ledger) -> Option<Transit> {
        let t = self.transit(s)?;
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].minus(t.cost.gpu);
        l.in_transit = l.in_transit.minus(t.cost.staging);
        self.set(s, TRANSIT, false);
        self.transit.remove(&s.0);
        Some(t)
    }

    pub fn put_gpu(&mut self, s: Slot, l: &mut Ledger, g: Gpu) {
        self.note(s, Change::GPU);
        let before = self.protection_bytes(s);
        self.unfile(s);
        let class = self.class(s);
        match self.gpu(s) {
            Some(old) => {
                l.gpu[class] = l.gpu[class].minus(old.bytes);
                self.count_epoch(s, old.epoch, false);
            }
            None => {
                if let r @ ResourceId::Drape { .. } = self.id(s) {
                    self.gpu_drapes.insert(r);
                }
            }
        }
        l.gpu[class] = l.gpu[class].plus(g.bytes);
        self.gpu[s.i()] = g;
        self.set(s, GPU, true);
        self.count_epoch(s, g.epoch, true);
        self.file(s);
        self.reprotect(s, before);
    }

    pub fn take_gpu(&mut self, s: Slot, l: &mut Ledger) -> Option<Gpu> {
        let g = self.gpu(s)?;
        let before = self.protection_bytes(s);
        self.unfile(s);
        let class = self.class(s);
        l.gpu[class] = l.gpu[class].minus(g.bytes);
        self.set(s, GPU, false);
        self.count_epoch(s, g.epoch, false);
        if let r @ ResourceId::Drape { .. } = self.id(s) {
            self.gpu_drapes.remove(&r);
        }
        self.file(s);
        self.reprotect(s, before);
        self.note(s, Change::GPU);
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
        a.advance(Micros(5), &[]);
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
        a.advance(Micros(8), &[s1]);
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
        a.advance(Micros(5), &[]);
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
        a.advance(Micros(0), &[]);
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
        a.advance(Micros(10), &[]);
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
        assert_eq!(a.advance(Micros(20), &s[..3]), Advance::Moved);
        a.touch(s[3], Micros(20));
        a.touch(s[3], Micros(20));
        assert_eq!(a.advance(Micros(20), &[]), Advance::Same);
        assert!(filed(&a, Class::TerrainMesh, &Held::ALL).is_empty());
        assert_eq!(a.last_needed(s[0]), Micros(10));
        assert_eq!(a.last_needed(s[3]), Micros(20));
        a.advance(Micros(30), &[s[3]]);
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

    /// The loads and transits that ended are apart when the prune comes, in
    /// any order they ended or started again: the prune takes exactly the
    /// slots neither loading nor in transit, their marks off, and walks
    /// nothing else. Red with the slot not moved when its load ends (the
    /// prune keeps an idle slot) and when its load starts again (it drops
    /// a busy one).
    #[test]
    fn the_loads_that_ended_are_apart_for_the_prune() {
        let mut a = Arena::new(None);
        let mut l = Ledger::default();
        a.advance(Micros(1), &[]);
        let load = Load::Loading {
            gen: Generation(1),
            source: Source::Network,
            started: Micros(1),
            epoch: 0,
        };
        let transit = Transit {
            gen: Generation(1),
            cost: Cost::flat(Bytes(10)),
            epoch: 0,
        };
        let slots: Vec<Slot> = (0..40u64)
            .map(|i| {
                let s = a.find_or_insert(mesh(i), Class::TerrainMesh, 3, Micros(1));
                a.list_active(s);
                if i % 3 == 0 {
                    a.put_transit(s, &mut l, transit);
                } else {
                    a.set_load(s, load);
                }
                s
            })
            .collect();
        assert!(a.active_consistent());
        // Ended in a scattered order; one ends then starts again; one
        // listed while idle.
        let mut ended = Vec::new();
        for i in [7usize, 0, 39, 12, 25, 1, 30] {
            let s = slots[i];
            if i % 3 == 0 {
                a.take_transit(s, &mut l);
            } else {
                a.set_load(s, Load::Idle);
            }
            ended.push(s);
            assert!(a.active_consistent());
        }
        let again = slots[25];
        a.set_load(again, load);
        ended.retain(|s| *s != again);
        let idle = a.find_or_insert(mesh(99), Class::TerrainMesh, 3, Micros(1));
        a.list_active(idle);
        ended.push(idle);
        assert!(a.active_consistent());
        let mut out = Vec::new();
        a.drop_idle_active(&mut out);
        out.sort_unstable();
        ended.sort_unstable();
        assert_eq!(out, ended, "the prune takes exactly the slots that ended");
        assert!(out.iter().all(|s| !a.marked(*s, Mark::Active)));
        assert_eq!(a.active().len(), 41 - ended.len());
        assert!(a
            .active()
            .iter()
            .all(|s| a.marked(*s, Mark::Active) && (a.loading(*s) || a.has_transit(*s))));
        assert!(a.active_consistent());
    }

    #[test]
    fn a_compacted_arena_keeps_every_resource_and_its_order() {
        let mut a = Arena::new(Some(1));
        let mut l = Ledger::default();
        let g = |b| Gpu {
            gen: Generation(1),
            bytes: Bytes(b),
            epoch: 0,
        };
        a.advance(Micros(1), &[]);
        let n = 2_000u64;
        let slots: Vec<Slot> = (0..n)
            .map(|i| {
                let s = a.find_or_insert(
                    mesh(i),
                    Class::TerrainMesh,
                    u32::try_from(i % 7).unwrap(),
                    Micros(1),
                );
                if i % 2 == 0 {
                    a.put_gpu(s, &mut l, g(i + 1));
                }
                s
            })
            .collect();
        // Time moves on twice: every slot filed in the eviction index.
        a.advance(Micros(2), &slots);
        a.advance(Micros(3), &[]);
        // The maps keyed by slot: a transit, a load, a rank on some of the
        // slots that stay (and on some that go).
        let transit = |i: u64| Transit {
            gen: Generation(2),
            cost: Cost::flat(Bytes(i + 7)),
            epoch: 1,
        };
        let load = |i: u64| Load::Loading {
            gen: Generation(3),
            source: Source::Network,
            started: Micros(i),
            epoch: 2,
        };
        let rank = |i: u64| Rank {
            urgency: Urgency::Hole,
            score: Reverse(i64::try_from(i).unwrap()),
            tier: Tier::Remote,
        };
        for (i, &s) in slots.iter().enumerate() {
            let i = i as u64;
            if i.is_multiple_of(30) {
                a.put_transit(s, &mut l, transit(i));
            }
            if i % 20 == 10 {
                a.set_load(s, load(i));
            }
            if i.is_multiple_of(5) {
                a.set_rank(s, Some(Urgency::Hole), Some(rank(i)));
            }
        }
        let gpu_before: Vec<Option<Gpu>> = slots.iter().map(|&s| a.gpu(s)).collect();
        let ledger_before = l;
        let filed_before: Vec<ResourceId> = {
            let mut out = Vec::new();
            let mut after = None;
            while let Some(e) = a.next_filed(after.as_ref(), |_| true, &Held::ALL) {
                out.push(e.0 .3);
                after = Some(e);
            }
            out
        };
        // Most go: a sparse arena.
        for (i, &s) in slots.iter().enumerate() {
            if i % 10 != 0 {
                a.take_gpu(s, &mut l);
                a.take_transit(s, &mut l);
                a.set_load(s, Load::Idle);
                a.set_rank(s, None, None);
                a.remove(s);
            }
        }
        assert!(a.sparse(), "200 of 2 000 slots live");
        let room_before = a.room();
        let mut map = Vec::new();
        let moved = a.compact(&mut map);
        assert!(!a.sparse(), "the room given back");
        assert_eq!(a.len(), 200);
        assert_eq!(a.room(), SHRINK_TO * 200);
        let n_slots = usize::try_from(n).unwrap();
        assert!(room_before >= n_slots && moved >= n + 200);
        assert_eq!(map.len(), n_slots);
        // Every resource found again, in slot order as before, with its
        // copies, its transit, its load and its rank at its new slot.
        let ids: Vec<ResourceId> = a.iter().map(|s| a.id(s)).collect();
        let want: Vec<ResourceId> = (0..n).filter(|i| i % 10 == 0).map(mesh).collect();
        assert_eq!(ids, want);
        for (at, &old) in slots.iter().enumerate() {
            let i = at as u64;
            let r = mesh(i);
            if !i.is_multiple_of(10) {
                assert_eq!(a.find(r), None);
                assert_eq!(Arena::moved_slot(&map, old), None);
                continue;
            }
            let s = a.find(r).unwrap();
            assert_eq!(a.id(s), r);
            assert_eq!(Arena::moved_slot(&map, old), Some(s));
            assert_eq!(s, Slot(u32::try_from(i / 10).unwrap()), "in order, dense");
            assert_eq!(a.gpu(s), gpu_before[at]);
            assert_eq!(a.transit(s), i.is_multiple_of(30).then(|| transit(i)));
            assert_eq!(a.load(s), if i % 20 == 10 { load(i) } else { Load::Idle });
            assert_eq!(a.priority(s), i.is_multiple_of(5).then(|| rank(i)));
            assert_eq!(a.urgency(s), i.is_multiple_of(5).then_some(Urgency::Hole));
        }
        assert!(l.gpu[Class::TerrainMesh] < ledger_before.gpu[Class::TerrainMesh]);
        let filed_after: Vec<ResourceId> = {
            let mut out = Vec::new();
            let mut after = None;
            while let Some(e) = a.next_filed(after.as_ref(), |_| true, &Held::ALL) {
                out.push(e.0 .3);
                after = Some(e);
            }
            out
        };
        let kept: Vec<ResourceId> = filed_before
            .into_iter()
            .filter(|r| want.contains(r) && a.gpu(a.find(*r).unwrap()).is_some())
            .collect();
        assert_eq!(filed_after, kept, "the eviction order kept");
        assert!(a.evict_index_consistent() && a.sums_consistent() && a.gpu_drapes_consistent());
        // A new resource takes the next slot at the end.
        let s = a.find_or_insert(mesh(n), Class::TerrainMesh, 0, Micros(3));
        assert_eq!(a.id(s), mesh(n));
        assert_eq!(a.len(), 201);
    }

    #[test]
    fn the_eviction_index_keeps_its_order_through_any_churn() {
        // Slots filed and unfiled in a scrambled order, against an ordered
        // set of the same keys: times that go back, played one by one.
        let mut a = Arena::new(None);
        let mut l = Ledger::default();
        a.regress();
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
