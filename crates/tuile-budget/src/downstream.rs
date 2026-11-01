// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the passes after the need keep from one step to the next (O1.4), so
//! that a step reads again only what may have changed.
//!
//! Every pass after the need is a function of units: a tile's drape levels,
//! a drawn tile's selection, a mesh's draw item, a tile's wants, a wanted
//! resource's work. Each unit's result is kept, and a unit is read again
//! when what it read may have changed:
//!
//! - its tile was named by the need's change (entered, changed, left);
//! - its tile's drawable copies changed (for a level, its copies' tiers) —
//!   the arena notes what changes of every slot (`state::Arena::changes`):
//!   an arrival, a delivery, an eviction, a release, a prune; a load or a
//!   transit concerns the work alone;
//! - what it is built of changed: a mesh's item when a tile it serves chose
//!   other levels or another selection; a tile's wants when its levels or
//!   its mesh's item did; a resource's work when its want did;
//! - it read another tile — an ancestor's drapes, a held tile's mesh, a
//!   tile's last item (`engine::Reach`) — and what it read of that tile
//!   changed: each such reading is kept (`readers`);
//! - a level that moved is read again next step (the hysteresis reads the
//!   last level); a level that asked the presence index is read again when
//!   the index says its answers may have changed (`PresenceOracle::
//!   generation`), every step if it does not say;
//! - a time it waits on fell due, through queues ordered by time rather
//!   than scans: a work's retry or delivery backoff (`wakes`), a load's
//!   timeout or an unneeded resource's grace (the cancel's `deadlines`), the
//!   30 s an empty slot backing off is kept (the prune's `kept_due`);
//! - something every unit of a pass reads moved: the LOD scale (levels),
//!   the pressure (wants), an epoch (levels, work, refreshes),
//!   Batch (every step: a film is a function of its frame), a selection
//!   capped to the floor (items).
//!
//! The rest keeps last step's result. The prune keeps its eighth-step
//! cadence but walks only the slots emptied since or whose time fell due,
//! the recent evictions in time order, and the tiles its counts of what keeps
//! each (`Support`) let go. A step's `work` counts the units read and what
//! each walked. The results live in ordered maps of sorted blocks
//! (`sorted`) changed by each pass's batch of changes — nothing allocated
//! once they have grown, the draw items' buffers recycled — each counting
//! what keeping its order costs into `work`. In a debug build every pass is read again whole, from
//! scratch, and compared (the `check_*` functions): a dependant left out of
//! the rules above shows there first.

// The decisions are integer arithmetic: floats stay in `imagery` and
// `priority`, quantised once, where bit-exactness is controlled.
#![deny(clippy::float_arithmetic)]

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use super::decisions::{DrapeChoice, DrawItem, DrawSet, Role};
use super::engine::{ItemBufs, ItemStats, Reach, TileSelection, Want, DRAPES, MESH, PLACE, WORN};
use super::held::{morton, Morton};
use super::need::Membership;
use super::need_state::{Named, TileNeed};
use super::priority::Urgency;
use super::sorted::SortedMap;
use super::state::{Advance, Change, Mark, Slot};
use super::stats::{BudgetStats, GAP_BUCKETS};
use super::tier::PresenceOracle;
use super::types::{sat_u32, LayerId, Micros, Pressure, ResourceId, TileId};
use super::TileInfo;
use super::{Budget, Policy, RECENT_CAP, THRASH_WINDOW};

/// A sorted set.
type Set<K> = SortedMap<K, ()>;

/// A batch of changes to a sorted map: a value to set, or `None` to remove.
type Changes<K, V> = Vec<(K, Option<V>)>;

/// The items read from scratch by the debug check: mesh, item, what it read.
#[cfg(debug_assertions)]
type Expected = Vec<(ResourceId, Item, Vec<(TileId, u8)>)>;

/// A unit of the passes that can read beyond its own tile: a drawn tile's
/// selection, or the draw item of a tile's mesh (every mesh drawn is a
/// tile's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Unit(TileId, u8);

impl Unit {
    const SEL: u8 = 0;
    const ITEM: u8 = 1;

    fn sel(t: TileId) -> Self {
        Unit(t, Self::SEL)
    }

    fn item(mesh: ResourceId) -> Self {
        Unit(mesh.tile(), Self::ITEM)
    }
}

/// A drawn mesh as kept: its role, the drapes it wears — the first in
/// place, the others (a second layer) apart — and what it adds to the
/// statistics. The tiles it serves are its run's (`contrib`), not kept
/// twice.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Item {
    pub role: Role,
    first: Option<DrapeChoice>,
    more: Vec<DrapeChoice>,
    stats: ItemStats,
}

/// A placeholder, for the sorted map that keeps the items.
impl Default for Item {
    fn default() -> Self {
        Item {
            role: Role::Exact,
            first: None,
            more: Vec::new(),
            stats: ItemStats::default(),
        }
    }
}

impl Item {
    /// The drapes it wears, in stacking order.
    pub fn drapes(&self) -> impl Iterator<Item = &DrapeChoice> + '_ {
        self.first.iter().chain(self.more.iter())
    }

    /// The drape it wears for `layer`, if any.
    pub fn drape(&self, layer: LayerId) -> Option<&DrapeChoice> {
        self.drapes().find(|d| d.layer == layer)
    }

    /// Whether it is `built`, with `stats`.
    fn same(&self, built: &DrawItem, stats: &ItemStats) -> bool {
        self.role == built.role && self.stats == *stats && self.drapes().eq(built.drapes.iter())
    }

    /// `built`, with `stats`, as kept.
    fn of(built: &DrawItem, stats: ItemStats) -> Self {
        Item {
            role: built.role,
            first: built.drapes.first().copied(),
            more: built.drapes.get(1..).map(<[_]>::to_vec).unwrap_or_default(),
            stats,
        }
    }
}

/// The selection's and the draw set's statistics, as sums over the units.
#[derive(Clone, Debug, Default, PartialEq)]
struct Sums {
    exact: u32,
    fallback: u32,
    lost: u32,
    /// The fallbacks' depths, with how many tiles fall back that deep.
    depths: SortedMap<u32, u32>,
    bare: u32,
    underdraped: u32,
    held: u32,
    over_terrain: [u32; GAP_BUCKETS],
}

impl Sums {
    fn selection(&mut self, c: &TileSelection, on: bool) {
        let add = |x: &mut u32, b: bool| {
            if b {
                *x = if on { *x + 1 } else { *x - 1 };
            }
        };
        add(&mut self.exact, c.exact);
        add(&mut self.fallback, c.fallback);
        add(&mut self.lost, c.lost);
        if c.depth > 0 {
            let n = self.depths.get(&c.depth).copied().unwrap_or(0);
            let n = if on { n + 1 } else { n - 1 };
            if n == 0 {
                self.depths.remove(&c.depth);
            } else {
                self.depths.insert(c.depth, n);
            }
        }
    }

    fn item(&mut self, i: &Item, on: bool) {
        let add = |x: &mut u32, b: bool| {
            if b {
                *x = if on { *x + 1 } else { *x - 1 };
            }
        };
        add(&mut self.bare, i.stats.bare);
        add(&mut self.underdraped, i.stats.underdraped);
        add(&mut self.held, i.role == Role::Held);
        if i.role == Role::Exact {
            for d in i.drapes() {
                let b = gap_bucket(d.level, i.stats.tile_level);
                self.over_terrain[b] = if on {
                    self.over_terrain[b] + 1
                } else {
                    self.over_terrain[b] - 1
                };
            }
        }
    }

    fn depth(&self) -> u32 {
        self.depths.keys().last().copied().unwrap_or(0)
    }
}

/// The bucket of a drape `level` worn on a tile of `tile_level`.
fn gap_bucket(level: u32, tile_level: u32) -> usize {
    let gap = i64::from(level) - i64::from(tile_level);
    usize::try_from((gap + 8).clamp(0, GAP_BUCKETS as i64 - 1)).unwrap_or(0)
}

/// The prune's count of what keeps each known tile: a tile is kept when it
/// is a seed — wanted, held, or holding a tracked resource — or the parent
/// of a kept tile it knows the place of. `kids` counts each tile's kept
/// children, so the prune frees what the counts let go, without a walk.
#[derive(Debug, Default)]
struct Support {
    ready: bool,
    kids: SortedMap<TileId, u32>,
    /// Tiles that stopped being kept since the prune last looked.
    unkept: Vec<TileId>,
    /// A parent link closed a cycle, or a count overflowed: the prune walks
    /// the whole tree, as before O1.4.
    whole: bool,
}

/// What the passes after the need keep from one step to the next.
#[derive(Debug, Default)]
pub(super) struct Downstream {
    /// The kept results stand for the last step: false before the first
    /// step and after `release_scratch`, and every unit is read again.
    ready: bool,
    /// Every unit is read again next step (an epoch moved).
    again: bool,
    /// The LOD scale the levels were read with, the pressure the wants;
    /// the presence index's generation when the levels asked it.
    scale: u32,
    pressure: Pressure,
    oracle: Option<u64>,
    /// Tiles whose levels asked the presence index; whose levels moved.
    levels_far: Set<TileId>,
    levels_moved: Set<TileId>,
    /// The floor tiles the need lists, and whether each is in.
    floor: SortedMap<TileId, bool>,
    floor_missing: u32,
    /// Each drawn tile's selection, uncapped: its counts; the entries of
    /// those not drawn as themselves (mesh, role), by tile.
    sel: SortedMap<TileId, TileSelection>,
    entries: Set<(TileId, ResourceId, Role)>,
    /// (mesh, tile, role) of every selection: a mesh's run is what its item
    /// serves.
    contrib: Set<(ResourceId, TileId, Role)>,
    /// What each unit read of other tiles (`engine::PLACE`, `MESH`,
    /// `DRAPES`, `WORN`), and the same by tile: who reads what.
    reach: SortedMap<(TileId, u8, TileId), u8>,
    readers: SortedMap<(TileId, TileId, u8), u8>,
    /// Tiles whose mesh's item changed this step: what of them was read,
    /// their last item, is read again next step.
    next: Vec<TileId>,
    /// Tiles whose held ancestry changed at the step's end: their
    /// selections are read again next step.
    next_sels: Vec<TileId>,
    /// The tiles coarsening: drawn by the finer tiles they held.
    coarsening: Set<TileId>,
    /// The draw set, by mesh.
    items: SortedMap<ResourceId, Item>,
    /// The draw set was built from a selection capped to the floor; the
    /// capped selection's coarsening and counts, which stand for the step's.
    capped: bool,
    capped_coarsening: Set<TileId>,
    capped_sel: Option<Sums>,
    /// The drapes worn, with how many meshes wear each.
    worn: SortedMap<ResourceId, u32>,
    /// The wants the tiles put, and the refreshes of drawn content.
    wants: SortedMap<ResourceId, Want>,
    refresh: SortedMap<ResourceId, Want>,
    /// The wanted resources with work, those waiting on a time, and those
    /// the view waits on in flight or in transit.
    jobs: Set<ResourceId>,
    /// The time each waits on; `wakes`, the same by time, read as they
    /// fall due (an entry whose time is no longer its resource's is stale).
    waiting: SortedMap<ResourceId, Micros>,
    wakes: BinaryHeap<Reverse<(Micros, ResourceId)>>,
    inflight: Set<ResourceId>,
    sums: Sums,
    /// The held set, as the held ancestry reads it (`held`): the held tiles
    /// whose walk is geometric, in Morton order; the others.
    held_regular: SortedMap<Morton, (TileId, u8)>,
    held_irregular: Set<TileId>,
    ancestry_ready: bool,
    /// How far the arena's changes were read.
    seen: usize,
    /// Resources that left the view since the newest group's time moved.
    departed: Vec<ResourceId>,
    /// Resources given a slot by the last step's loads: marked next step.
    fresh: Vec<ResourceId>,
    /// The newest group's leavers, while no touch moved the time on.
    leavers: Vec<Slot>,
    /// Whether this step's time is the newest group's.
    advanced: bool,
    /// This step plays a time that went back (`state::Arena::regress`).
    explicit: bool,
    /// The last step played a time that went back: the resources it
    /// needed may sit in the older group, and the next step touches every
    /// resource it needs, as before O1.4.
    touch_all: bool,
    /// The held set's changes this step, applied at its end.
    held_changes: Vec<(TileId, bool)>,
    support: Support,
    /// The recent evictions, oldest first (some since superseded); whether
    /// a time that went back left them out of order.
    recent: VecDeque<(Micros, ResourceId)>,
    recent_disordered: bool,
    /// Empty slots a prune kept, with the time from which it may let each
    /// go (`Micros(u64::MAX)`: when its want goes, which reads it again);
    /// `kept_due`, the same by time, looked at again as they fall due (an
    /// entry whose time is no longer its slot's is stale).
    kept_empty: SortedMap<Slot, Micros>,
    kept_due: BinaryHeap<Reverse<(Micros, Slot)>>,
    /// When a load or transit may next be cancelled: a load's timeout, an
    /// unneeded resource's grace (`Budget::cancel`), earliest first.
    deadlines: BinaryHeap<Reverse<(Micros, ResourceId)>>,
    /// The deadlines are not kept: the next cancel looks at every load.
    deadlines_lost: bool,
    /// This step's lists and batches.
    step: StepLists,
}

/// A step's lists and batches, refilled in place.
#[derive(Debug, Default)]
struct StepLists {
    /// The tiles read again: named for their shape or place, or whose
    /// drawable copies changed.
    dirty: Vec<TileId>,
    /// Every tile the need's change named (their floats and footprints:
    /// their levels and wants are read again).
    named: Vec<TileId>,
    /// The named tiles whose place or membership changed.
    relinked: Vec<Named>,
    /// The tiles whose drape levels are read again: the dirty ones, and
    /// those whose levels moved, asked a presence index whose answers may
    /// have changed, or read a scale that did.
    lev: Vec<TileId>,
    /// The tiles whose levels moved this step.
    moved: Vec<TileId>,
    /// The selections read again.
    sels: Vec<TileId>,
    /// The meshes whose items are read again.
    items: Vec<ResourceId>,
    /// The tiles whose wants are read again.
    wants_dirty: Vec<TileId>,
    /// Resources whose want changed this step.
    wants: Vec<ResourceId>,
    /// Resources whose drawn mark changed.
    drawn: Vec<ResourceId>,
    /// Resources the arena noted changed; those whose GPU copy did; tiles
    /// whose resources' tiers did.
    all: Vec<ResourceId>,
    logged: Vec<ResourceId>,
    tiered: Vec<TileId>,
    /// Tiles whose held ancestry changed this step.
    pairs: Vec<TileId>,
    marks: Vec<ResourceId>,
    slots: Vec<Slot>,
    held: Vec<TileId>,
    levels: Vec<(LayerId, u32)>,
    layers: Vec<LayerId>,
    entries: Vec<(ResourceId, Role)>,
    old_entries: Vec<(ResourceId, Role)>,
    run: Vec<(TileId, Role)>,
    reach: Reach,
    beyond: Vec<(TileId, u8)>,
    old_reach: Vec<(TileId, u8)>,
    bufs: ItemBufs,
    tile_wants: Vec<(ResourceId, u32, Want)>,
    old_wants: Vec<(ResourceId, Want)>,
    drapes: Vec<(LayerId, u32)>,
    chain: Vec<(TileId, u32)>,
    chain_now: Vec<(TileId, u32)>,
    worn: Vec<(ResourceId, i32)>,
    tiles: Vec<TileId>,
    capped: Vec<(ResourceId, TileId, Role)>,
    /// An item as built, before it is compared with the one kept.
    built: Option<DrawItem>,
    // The batches.
    c_far: Changes<TileId, ()>,
    c_moved: Changes<TileId, ()>,
    c_floor: Changes<TileId, bool>,
    c_sel: Changes<TileId, TileSelection>,
    c_entries: Changes<(TileId, ResourceId, Role), ()>,
    c_contrib: Changes<(ResourceId, TileId, Role), ()>,
    c_reach: Changes<(TileId, u8, TileId), u8>,
    c_readers: Changes<(TileId, TileId, u8), u8>,
    c_coarsening: Changes<TileId, ()>,
    c_items: Changes<ResourceId, Item>,
    c_worn: Changes<ResourceId, u32>,
    c_wants: Changes<ResourceId, Want>,
    c_set: Changes<ResourceId, ()>,
    c_wait: Changes<ResourceId, Micros>,
    c_kept: Changes<Slot, Micros>,
    c_set3: Changes<ResourceId, ()>,
    refresh: Vec<(ResourceId, Want)>,
    gone: Vec<(TileId, LayerId)>,
    done: Vec<ResourceId>,
}

impl StepLists {
    /// Each list's room brought down to what this step used of it, when it
    /// is far more (`sorted::trim`).
    fn trim(&mut self) {
        use super::sorted::trim;
        macro_rules! t {
            ($($v:ident),*) => { $( let n = self.$v.len(); trim(&mut self.$v, n); )* };
        }
        t!(
            dirty,
            named,
            relinked,
            lev,
            moved,
            sels,
            items,
            wants_dirty,
            wants,
            drawn,
            all,
            logged,
            tiered,
            pairs,
            marks,
            slots,
            held,
            levels,
            layers,
            entries,
            old_entries,
            run,
            beyond,
            old_reach,
            tile_wants,
            old_wants,
            drapes,
            chain,
            worn,
            tiles,
            capped,
            refresh,
            gone,
            done
        );
    }
}

/// The wants of a step: the tiles' and the refreshes, disjoint.
#[derive(Clone, Copy)]
pub(super) struct Wants<'a> {
    tiles: &'a SortedMap<ResourceId, Want>,
    refresh: &'a SortedMap<ResourceId, Want>,
}

impl<'a> Wants<'a> {
    pub fn get(&self, r: &ResourceId) -> Option<&'a Want> {
        self.tiles.get(r).or_else(|| self.refresh.get(r))
    }

    pub fn contains_key(&self, r: &ResourceId) -> bool {
        self.get(r).is_some()
    }

    /// Every want, the tiles' then the refreshes'.
    pub fn iter(&self) -> impl Iterator<Item = (&'a ResourceId, &'a Want)> + 'a {
        self.tiles.iter().chain(self.refresh.iter())
    }
}

/// The wants and coarsening a step reads after the passes: taken out of the
/// budget for the step, put back at its end.
#[derive(Default)]
pub(super) struct Taken {
    /// Every unit of the passes was read: so is every want's work.
    pub full: bool,
    wants: SortedMap<ResourceId, Want>,
    refresh: SortedMap<ResourceId, Want>,
    coarsening: Set<TileId>,
}

impl Taken {
    pub fn wants(&self) -> Wants<'_> {
        Wants {
            tiles: &self.wants,
            refresh: &self.refresh,
        }
    }

    pub fn coarsening(&self) -> &Set<TileId> {
        &self.coarsening
    }
}

/// The resources of `t` in a map keyed by resource: its mesh, then its
/// drapes.
fn of_tile<V>(m: &SortedMap<ResourceId, V>, t: TileId) -> impl Iterator<Item = (&ResourceId, &V)> {
    let lo = ResourceId::drape(t, LayerId(0), 0);
    let hi = ResourceId::drape(t, LayerId(u16::MAX), u32::MAX);
    m.range(ResourceId::Mesh(t)..=ResourceId::Mesh(t))
        .chain(m.range(lo..=hi))
}

/// The entries of `t`'s selection: implied when it is drawn as itself.
fn sel_entries(
    t: TileId,
    counts: Option<&TileSelection>,
    rows: &Set<(TileId, ResourceId, Role)>,
    out: &mut Vec<(ResourceId, Role)>,
) {
    out.clear();
    match counts {
        Some(c) if c.exact => out.push((ResourceId::Mesh(t), Role::Exact)),
        Some(_) => out.extend(
            rows.range(
                (t, ResourceId::Mesh(TileId(0)), Role::Exact)
                    ..=(
                        t,
                        ResourceId::drape(TileId(u64::MAX), LayerId(u16::MAX), u32::MAX),
                        Role::Fallback,
                    ),
            )
            .map(|((_, m, r), _)| (*m, *r)),
        ),
        None => {}
    }
}

/// Whether the prune keeps empty slot `s` at `now` — backing off after a
/// failure (until 30 s after its want went), or wanted — and if so, the time
/// from which it may let it go, as far as time alone tells
/// (`Micros(u64::MAX)`: only when its want goes).
/// The swap and chain marks of a resource, drawable or not, wanted at
/// urgency `u`: a drawable member of a swap the view waits on (a hole, a
/// blocker, an ancestor), and of an ancestor's chain.
fn swap_chain(drawable: bool, u: Option<Urgency>) -> (bool, bool) {
    let swap = drawable
        && matches!(
            u,
            Some(Urgency::Hole | Urgency::Blocker | Urgency::Ancestor)
        );
    (swap, drawable && u == Some(Urgency::Ancestor))
}

fn kept_until(a: &super::state::Arena, s: Slot, now: Micros, wants: Wants<'_>) -> Option<Micros> {
    let cold = a.cold(s);
    let unneeded = cold.unneeded_since();
    let mut until = None;
    if cold.backing_off(now) && unneeded.is_none_or(|u| now.since(u) < UNNEEDED_KEEP) {
        let end = unneeded.map_or(cold.retry_at, |u| cold.retry_at.min(u.plus(UNNEEDED_KEEP)));
        until = Some(end);
    }
    if wants.contains_key(&a.id(s)) && unneeded.is_none() {
        until = Some(until.unwrap_or(Micros(u64::MAX)));
    }
    until
}

/// How long the prune keeps an empty slot backing off once its want went.
const UNNEEDED_KEEP: Micros = Micros::from_secs(30);

/// Two sorted lists' differences: `f(x, false)` for what only `old` has,
/// `f(x, true)` for what only `new` has; equal entries skipped.
fn diff<T: Ord + Copy>(old: &[T], new: &[T], mut f: impl FnMut(T, bool)) {
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        match (old.get(i), new.get(j)) {
            (Some(a), Some(b)) if a == b => {
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) if a < b => {
                f(*a, false);
                i += 1;
            }
            (Some(a), None) => {
                f(*a, false);
                i += 1;
            }
            (_, Some(b)) => {
                f(*b, true);
                j += 1;
            }
            (None, None) => break,
        }
    }
}

/// Sets `k` to `v` in `m` — in place when `m` has `k`; else, an insertion
/// or a removal, in the batch `c` (applied once the pass is done).
fn put<K: Ord + Copy, V>(m: &mut SortedMap<K, V>, c: &mut Changes<K, V>, k: K, v: Option<V>) {
    match (m.get_mut(&k), v) {
        (Some(slot), Some(v)) => *slot = v,
        (Some(_), None) => c.push((k, None)),
        (None, Some(v)) => c.push((k, Some(v))),
        (None, None) => {}
    }
}

/// Adds `items` to `v`, a list read as a set: when it is full it is sorted
/// and deduplicated before it grows, so it holds about twice its distinct
/// members at most, whatever was added twice.
fn add<T: Ord + Copy>(v: &mut Vec<T>, items: impl IntoIterator<Item = T>) {
    for x in items {
        if v.len() == v.capacity() && v.len() >= 64 {
            v.sort_unstable();
            v.dedup();
        }
        v.push(x);
    }
}

impl Downstream {
    /// Every unit is read again at the next step.
    pub fn everything_again(&mut self) {
        self.again = true;
    }

    /// What its maps spent keeping their order since the last call.
    pub fn take_upkeep(&mut self) -> u64 {
        let mut n = self.sums.depths.take_upkeep();
        if let Some(c) = self.capped_sel.as_mut() {
            n = n.saturating_add(c.depths.take_upkeep());
        }
        for m in [
            &mut self.levels_far,
            &mut self.levels_moved,
            &mut self.coarsening,
            &mut self.capped_coarsening,
        ] {
            n = n.saturating_add(m.take_upkeep());
        }
        for m in [&mut self.jobs, &mut self.inflight] {
            n = n.saturating_add(m.take_upkeep());
        }
        n.saturating_add(self.entries.take_upkeep())
            .saturating_add(self.contrib.take_upkeep())
            .saturating_add(self.floor.take_upkeep())
            .saturating_add(self.sel.take_upkeep())
            .saturating_add(self.reach.take_upkeep())
            .saturating_add(self.readers.take_upkeep())
            .saturating_add(self.items.take_upkeep())
            .saturating_add(self.worn.take_upkeep())
            .saturating_add(self.wants.take_upkeep())
            .saturating_add(self.refresh.take_upkeep())
            .saturating_add(self.waiting.take_upkeep())
            .saturating_add(self.held_regular.take_upkeep())
            .saturating_add(self.held_irregular.take_upkeep())
            .saturating_add(self.kept_empty.take_upkeep())
            .saturating_add(self.support.kids.take_upkeep())
    }

    /// Drops the step's lists and batches and the indexes the next step
    /// rebuilds — who reads what, the work, the deadlines — keeping the
    /// units' results; every unit is read again at the next step.
    pub fn release_scratch(&mut self) {
        self.step = StepLists::default();
        self.reach = SortedMap::default();
        self.readers = SortedMap::default();
        self.jobs = SortedMap::default();
        self.waiting = SortedMap::default();
        self.wakes = BinaryHeap::new();
        self.inflight = SortedMap::default();
        self.deadlines = BinaryHeap::new();
        self.deadlines_lost = true;
        self.next = Vec::new();
        self.next_sels = Vec::new();
        self.again = true;
    }

    /// The draw item of `mesh`: last step's while a step reads its passes'
    /// inputs, this step's once its items are built.
    pub fn drawn(&self, mesh: ResourceId) -> Option<&Item> {
        self.items.get(&mesh)
    }

    /// The drapes `mesh` wears (last step's, or this step's once built).
    pub fn worn(&self, mesh: ResourceId) -> impl Iterator<Item = &DrapeChoice> + '_ {
        self.items.get(&mesh).into_iter().flat_map(Item::drapes)
    }

    /// The run of `mesh`: (tile served, role), from the capped selection
    /// when the draw set is drawn for it.
    fn run(&self, mesh: ResourceId) -> impl Iterator<Item = (TileId, Role)> + '_ {
        let capped = &self.step.capped;
        let (a, b) = if self.capped {
            let from = capped.partition_point(|x| x.0 < mesh);
            let to = from + capped[from..].partition_point(|x| x.0 == mesh);
            (&capped[from..to], None)
        } else {
            (&[][..], Some(mesh))
        };
        let kept = b.into_iter().flat_map(move |m| {
            self.contrib
                .range((m, TileId(0), Role::Exact)..=(m, TileId(u64::MAX), Role::Fallback))
                .map(|((_, t, r), _)| (*t, *r))
        });
        a.iter().map(|x| (x.1, x.2)).chain(kept)
    }

    /// How many tiles `mesh`'s item serves.
    pub fn serves(&self, mesh: ResourceId) -> usize {
        let mut n = 0;
        let mut last = None;
        for (t, _) in self.run(mesh) {
            if last != Some(t) {
                n += 1;
                last = Some(t);
            }
        }
        n
    }

    /// The draw set, for the step's decisions.
    pub fn draw_set(&self) -> DrawSet {
        DrawSet {
            items: self
                .items
                .iter()
                .map(|(m, i)| {
                    let mut serves: Vec<TileId> = self.run(*m).map(|x| x.0).collect();
                    serves.dedup();
                    DrawItem {
                        mesh: *m,
                        role: i.role,
                        drapes: i.drapes().copied().collect(),
                        serves,
                    }
                })
                .collect(),
        }
    }

    /// The geometric held tiles, by Morton key (`held`).
    pub fn held_regular(&self) -> &SortedMap<Morton, (TileId, u8)> {
        &self.held_regular
    }

    /// The other held tiles (`held`).
    pub fn held_irregular(&self) -> &Set<TileId> {
        &self.held_irregular
    }

    /// The work the step ranks.
    pub fn jobs(&self) -> impl Iterator<Item = ResourceId> + '_ {
        self.jobs.keys().copied()
    }

    /// The resources the tiles want, in order (the refreshes left out:
    /// they are drawn).
    pub fn wanted(&self) -> impl Iterator<Item = ResourceId> + '_ {
        self.wants.keys().copied()
    }

    /// Whether the view waits on something in flight or in transit.
    pub fn view_waits(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// The draw set's meshes and items, in mesh order.
    pub fn items(&self) -> impl Iterator<Item = (&ResourceId, &Item)> + '_ {
        self.items.iter()
    }

    /// `r` may be cancelled at `t`.
    pub fn deadline(&mut self, t: Micros, r: ResourceId) {
        self.deadlines.push(Reverse((t, r)));
    }

    /// How many deadlines are kept (some for loads long gone).
    pub fn deadlines_len(&self) -> usize {
        self.deadlines.len()
    }

    /// Forgets every deadline (to set those of the loads still going).
    pub fn clear_deadlines(&mut self) {
        self.deadlines.clear();
    }

    /// The deadlines' room brought down to what they hold, when it is far
    /// more.
    pub fn trim_deadlines(&mut self) {
        let n = self.deadlines.len();
        if self.deadlines.capacity() > 4 * n + 256 {
            self.deadlines.shrink_to(2 * n + 64);
        }
    }

    /// The deadlines due at `now`, popped; `None` when they are not kept
    /// (every load is then looked at, and its deadlines kept again).
    pub fn due(&mut self, now: Micros, out: &mut Vec<ResourceId>) -> bool {
        if std::mem::take(&mut self.deadlines_lost) {
            self.deadlines.clear();
            return false;
        }
        while let Some(&Reverse((t, r))) = self.deadlines.peek() {
            if t > now {
                break;
            }
            self.deadlines.pop();
            out.push(r);
        }
        true
    }

    /// The selection's and the draw set's statistics, into `st`.
    pub fn stats(&self, floor_in: bool, st: &mut BudgetStats) {
        let s = &self.sums;
        let sel = self.capped_sel.as_ref().unwrap_or(s);
        st.floor_in = floor_in;
        st.exact += sel.exact;
        st.fallback += sel.fallback;
        st.lost += sel.lost;
        st.fallback_depth_max = st.fallback_depth_max.max(sel.depth());
        st.bare += s.bare;
        st.underdraped += s.underdraped;
        st.held += s.held;
        for (b, n) in st.imagery_over_terrain.iter_mut().zip(&s.over_terrain) {
            *b += n;
        }
    }

    /// Whether the floor is in: every floor tile the need lists drawable
    /// with its floor drapes, and the need lists one at least.
    pub fn floor_in(&self) -> bool {
        !self.floor.is_empty() && self.floor_missing == 0
    }
}

impl Budget {
    /// The passes after the need: drape levels, the floor, the selection,
    /// the draw set, the wants and the refreshes, each unit read again only
    /// if what it reads may have changed (the module documentation).
    /// `named` are the tiles the need's change named — a whole need's
    /// too, the tiles it lists and those it dropped — with their tree
    /// positions before, whether they were wanted, and what changed.
    pub(super) fn downstream(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        named: &[Named],
        presence: &dyn PresenceOracle,
        st: &mut BudgetStats,
    ) -> Taken {
        let batch = self.cfg.policy == Policy::Batch;
        let full = !self.down.ready || self.down.again || batch;
        let mut l = std::mem::take(&mut self.down.step);

        // What changed in the arena since the passes last read it: the
        // drawable copies (every pass reads them), and the tiers (the
        // levels read them). Loads and transits are the work's alone.
        l.logged.clear();
        l.tiered.clear();
        l.all.clear();
        let changes = self.arena.changes();
        let new = &changes[self.down.seen.min(changes.len())..];
        self.walk(new.len());
        for &(r, what) in new {
            add(&mut l.all, [r]);
            if what & Change::GPU != 0 {
                l.logged.push(r);
            }
            if what & (Change::GPU | Change::CPU | Change::TIER) != 0 {
                l.tiered.push(r.tile());
            }
        }
        self.down.seen = changes.len();
        l.logged.sort_unstable();
        l.logged.dedup();

        // The tree: the held ancestry and the prune's support follow the
        // named tiles whose place or membership changed (in motion, most
        // tiles are named for their floats alone).
        l.relinked.clear();
        l.relinked.extend(
            named
                .iter()
                .filter(|x| x.moved() || x.was_wanted() != tiles.get(&x.tile).is_some()),
        );
        self.walk(named.len());
        let relinked = std::mem::take(&mut l.relinked);
        l.pairs.clear();
        self.ancestry_follow(&relinked, full, &mut l);
        self.support_follow(&relinked, tiles);
        l.relinked = relinked;

        // The tiles read again: named for their shape or place, or whose
        // drawable copies changed.
        // (Units that read a tile's last item are read again through the
        // readers, below, when it changed.)
        let oracle = presence.generation();
        let mut next = std::mem::take(&mut self.down.next);
        next.sort_unstable();
        next.dedup();
        l.dirty.clear();
        add(&mut l.dirty, l.logged.iter().map(|r| r.tile()));
        if full {
            add(&mut l.dirty, tiles.keys().copied());
            add(&mut l.dirty, self.down.sel.keys().copied());
            add(&mut l.dirty, self.down.floor.keys().copied());
            add(&mut l.dirty, self.down.wants.keys().map(|r| r.tile()));
        } else {
            add(
                &mut l.dirty,
                named
                    .iter()
                    .filter(|x| x.shaped() || x.moved())
                    .map(|x| x.tile),
            );
        }
        l.dirty.sort_unstable();
        l.dirty.dedup();
        // Their levels; those that moved (the hysteresis reads the last);
        // those that asked the presence index, when its answers may have
        // changed; every one under a new LOD scale. A level that does not
        // move changes nothing past it.
        l.lev.clear();
        add(&mut l.lev, l.dirty.iter().copied());
        // A named tile's footprint may have moved.
        l.named.clear();
        l.named.extend(named.iter().map(|x| x.tile));
        add(&mut l.lev, l.named.iter().copied());
        add(&mut l.lev, l.tiered.iter().copied());
        if !full {
            add(&mut l.lev, self.down.levels_moved.keys().copied());
            if oracle.is_none() || oracle != self.down.oracle {
                add(&mut l.lev, self.down.levels_far.keys().copied());
            }
            if self.down.scale != self.scale {
                self.walk(tiles.len());
                add(
                    &mut l.lev,
                    tiles
                        .iter()
                        .filter(|(_, tn)| Self::draped(tn))
                        .map(|(t, _)| *t),
                );
            }
        }
        l.lev.sort_unstable();
        l.lev.dedup();

        // The selections and items read again: the dirty tiles', those that
        // read what changed of a tile, those whose held ancestry changed.
        l.sels.clear();
        add(&mut l.sels, l.dirty.iter().copied());
        add(&mut l.sels, l.pairs.iter().copied());
        add(&mut l.sels, self.down.next_sels.drain(..));
        l.items.clear();
        if !full {
            let changed = l
                .logged
                .iter()
                .map(|r| match r {
                    ResourceId::Mesh(t) => (*t, MESH),
                    ResourceId::Drape { tile, .. } => (*tile, DRAPES),
                })
                .chain(next.iter().map(|t| (*t, WORN)))
                .chain(named.iter().filter(|x| x.moved()).map(|x| (x.tile, PLACE)));
            for (x, what) in changed {
                let lo = (x, TileId(0), Unit::SEL);
                let hi = (x, TileId(u64::MAX), Unit::ITEM);
                for ((_, t, tag), _) in self
                    .down
                    .readers
                    .range(lo..=hi)
                    .filter(|(_, w)| **w & what != 0)
                {
                    self.walk(1);
                    match *tag {
                        Unit::SEL => add(&mut l.sels, [*t]),
                        _ => add(&mut l.items, [ResourceId::Mesh(*t)]),
                    }
                }
            }
        }
        next.clear();
        self.down.next = next;
        l.sels.sort_unstable();
        l.sels.dedup();
        if full {
            // Every unit is read: who reads what is rebuilt with them.
            self.down.reach.clear();
            self.down.readers.clear();
        }

        #[cfg(debug_assertions)]
        let last = self.imagery_prev.clone();
        #[cfg(debug_assertions)]
        self.check_ancestry();
        self.levels_pass(tiles, presence, &mut l);
        #[cfg(debug_assertions)]
        self.check_levels(tiles, presence, &last);
        self.floor_pass(tiles, &mut l);
        self.selection_pass(tiles, full, &mut l);
        #[cfg(debug_assertions)]
        self.check_selection(tiles);
        let floor_in = self.down.floor_in();
        self.items_pass(tiles, full, floor_in, &mut l);
        let pressure_moved = self.down.pressure != self.pressure;
        self.wants_pass(tiles, full || pressure_moved, &mut l);
        self.refresh_pass(full, &mut l);
        #[cfg(debug_assertions)]
        self.check_wants(tiles);

        self.down.stats(floor_in, st);
        self.down.scale = self.scale;
        self.down.pressure = self.pressure;
        self.down.oracle = oracle;
        self.down.ready = true;
        self.down.again = false;
        self.down.step = l;
        Taken {
            full,
            wants: std::mem::take(&mut self.down.wants),
            refresh: std::mem::take(&mut self.down.refresh),
            coarsening: if self.down.capped {
                std::mem::take(&mut self.down.capped_coarsening)
            } else {
                std::mem::take(&mut self.down.coarsening)
            },
        }
    }

    /// Puts back what the step took.
    pub(super) fn put_back(&mut self, t: Taken) {
        self.down.wants = t.wants;
        self.down.refresh = t.refresh;
        if self.down.capped {
            self.down.capped_coarsening = t.coarsening;
        } else {
            self.down.coarsening = t.coarsening;
        }
    }

    /// The drape levels of the tiles in `l.lev` that ask for drapes,
    /// written to `imagery_prev`; the tiles whose levels moved, into
    /// `l.moved`.
    fn levels_pass(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        presence: &dyn PresenceOracle,
        l: &mut StepLists,
    ) {
        l.moved.clear();
        self.walk(l.lev.len());
        for i in 0..l.lev.len() {
            let t = l.lev[i];
            let (mut asked, mut moved) = (false, false);
            if let Some(tn) = tiles.get(&t).filter(|tn| Self::draped(tn)) {
                self.drape_levels(
                    t,
                    tn,
                    presence,
                    &self.imagery_prev,
                    &mut l.levels,
                    &mut asked,
                );
                for &(layer, level) in &l.levels {
                    if self.imagery_prev.insert((t, layer), level) != Some(level) {
                        moved = true;
                    }
                }
            }
            if moved {
                l.moved.push(t);
            }
            let d = &self.down;
            if d.levels_moved.contains_key(&t) != moved {
                l.c_moved.push((t, moved.then_some(())));
            }
            if d.levels_far.contains_key(&t) != asked {
                l.c_far.push((t, asked.then_some(())));
            }
        }
        self.down.levels_moved.apply(&mut l.c_moved, |_| {});
        self.down.levels_far.apply(&mut l.c_far, |_| {});
    }

    /// Whether each dirty floor tile is in.
    fn floor_pass(&mut self, tiles: &SortedMap<TileId, TileNeed>, l: &mut StepLists) {
        self.walk(l.dirty.len());
        for i in 0..l.dirty.len() {
            let t = l.dirty[i];
            let now = tiles
                .get(&t)
                .filter(|tn| self.is_floor(tn.need.level))
                .map(|tn| self.floor_ready(t, tn));
            let d = &mut self.down;
            let before = d.floor.get(&t).copied();
            if before == now {
                continue;
            }
            put(&mut d.floor, &mut l.c_floor, t, now);
            if before == Some(false) {
                d.floor_missing -= 1;
            }
            if now == Some(false) {
                d.floor_missing += 1;
            }
        }
        self.down.floor.apply(&mut l.c_floor, |_| {});
    }

    /// The layers drawn tile `t` asks for, into `out`.
    fn layers_of(
        &self,
        t: TileId,
        tn: &TileNeed,
        drapes: &mut Vec<(LayerId, u32)>,
        out: &mut Vec<LayerId>,
    ) {
        self.drapes_of(t, tn, drapes);
        out.clear();
        out.extend(drapes.iter().map(|x| x.0));
    }

    /// `u` read again: what it read of other tiles was `old`, is `new`
    /// (both by tile); the batches of who reads what follow.
    fn reread(
        u: Unit,
        old: &[(TileId, u8)],
        new: &[(TileId, u8)],
        d: &mut Downstream,
        c_reach: &mut Changes<(TileId, u8, TileId), u8>,
        c_readers: &mut Changes<(TileId, TileId, u8), u8>,
    ) {
        if old == new {
            return;
        }
        // By tile; a tile read in both, its kinds compared.
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            let (o, n) = (old.get(i), new.get(j));
            let (t, ow, nw) = match (o, n) {
                (Some(a), Some(b)) if a.0 == b.0 => {
                    i += 1;
                    j += 1;
                    (a.0, a.1, b.1)
                }
                (Some(a), Some(b)) if a.0 < b.0 => {
                    i += 1;
                    (a.0, a.1, 0)
                }
                (Some(a), None) => {
                    i += 1;
                    (a.0, a.1, 0)
                }
                (_, Some(b)) => {
                    j += 1;
                    (b.0, 0, b.1)
                }
                (None, None) => break,
            };
            if ow == nw {
                continue;
            }
            let w = (nw != 0).then_some(nw);
            put(&mut d.reach, c_reach, (u.0, u.1, t), w);
            put(&mut d.readers, c_readers, (t, u.0, u.1), w);
        }
    }

    /// What unit `u` last read of other tiles, into `out`.
    fn reach_of(&self, u: Unit, out: &mut Vec<(TileId, u8)>) {
        out.clear();
        out.extend(
            self.down
                .reach
                .range((u.0, u.1, TileId(0))..=(u.0, u.1, TileId(u64::MAX)))
                .map(|((.., t), w)| (*t, *w)),
        );
    }

    /// The selection of the tiles in `l.sels`, uncapped; the meshes whose
    /// runs changed, added to `l.items`. Every drawn tile when `full`.
    fn selection_pass(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        full: bool,
        l: &mut StepLists,
    ) {
        if full {
            add(
                &mut l.sels,
                tiles
                    .iter()
                    .filter(|(_, n)| n.membership == Membership::Drawn)
                    .map(|(t, _)| *t),
            );
            l.sels.sort_unstable();
            l.sels.dedup();
        }
        self.walk(l.sels.len());
        for i in 0..l.sels.len() {
            let t = l.sels[i];
            let new = match tiles.get(&t) {
                Some(tn) if tn.membership == Membership::Drawn => {
                    self.layers_of(t, tn, &mut l.drapes, &mut l.layers);
                    l.reach.clear();
                    let c = self.select_tile(t, &l.layers, None, &mut l.entries, &mut l.reach);
                    l.reach.beyond(t, &mut l.beyond);
                    Some(c)
                }
                _ => {
                    l.entries.clear();
                    l.beyond.clear();
                    None
                }
            };
            if full {
                l.old_reach.clear();
            } else {
                self.reach_of(Unit::sel(t), &mut l.old_reach);
            }
            Self::reread(
                Unit::sel(t),
                &l.old_reach,
                &l.beyond,
                &mut self.down,
                &mut l.c_reach,
                &mut l.c_readers,
            );
            let d = &mut self.down;
            let old = d.sel.get(&t).copied();
            sel_entries(t, old.as_ref(), &d.entries, &mut l.old_entries);
            if old == new && l.old_entries == l.entries {
                continue;
            }
            if old != new {
                put(&mut d.sel, &mut l.c_sel, t, new);
            }
            if l.old_entries != l.entries {
                let (oe, ne) = (&l.old_entries, &l.entries);
                let (c_contrib, items) = (&mut l.c_contrib, &mut l.items);
                diff(oe, ne, |(m, role), came| {
                    c_contrib.push(((m, t, role), came.then_some(())));
                    items.push(m);
                });
                // The rows of a tile not drawn as itself.
                let rows = |c: Option<&TileSelection>| c.is_some_and(|c| !c.exact);
                let c_entries = &mut l.c_entries;
                if rows(old.as_ref()) {
                    for &(m, role) in oe {
                        c_entries.push(((t, m, role), None));
                    }
                }
                if rows(new.as_ref()) {
                    for &(m, role) in ne {
                        c_entries.push(((t, m, role), Some(())));
                    }
                }
            }
            if let Some(o) = &old {
                d.sums.selection(o, false);
            }
            if let Some(n) = &new {
                d.sums.selection(n, true);
            }
            let held = new.is_some_and(|n| n.held);
            if d.coarsening.contains_key(&t) != held {
                l.c_coarsening.push((t, held.then_some(())));
            }
        }
        let d = &mut self.down;
        d.sel.apply(&mut l.c_sel, |_| {});
        d.entries.apply(&mut l.c_entries, |_| {});
        d.contrib.apply(&mut l.c_contrib, |_| {});
        d.coarsening.apply(&mut l.c_coarsening, |_| {});
        d.reach.apply(&mut l.c_reach, |_| {});
        d.readers.apply(&mut l.c_readers, |_| {});
    }

    /// The draw set: the items of the meshes whose runs changed, serving a
    /// dirty tile (its layers, its levels), of a dirty tile, or reading
    /// what changed of another; or, capped to the floor, every item from a
    /// whole selection.
    fn items_pass(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        full: bool,
        floor_in: bool,
        l: &mut StepLists,
    ) {
        // P1.1 (b): coarse ground before any refinement. While wanted
        // ground is lost and the floor not in, nothing finer than the floor
        // is drawn: the whole selection is read again, capped.
        let cap = self
            .cfg
            .pinned_level
            .filter(|_| self.down.sums.lost > 0 && !floor_in);
        let mut capped = std::mem::take(&mut l.capped);
        capped.clear();
        let capped_run = cap.map(|p| {
            let mut sums = Sums::default();
            let mut coarsening = Set::default();
            self.walk(tiles.len());
            for (t, tn) in tiles
                .iter()
                .filter(|(_, n)| n.membership == Membership::Drawn)
            {
                self.layers_of(*t, tn, &mut l.drapes, &mut l.layers);
                l.reach.clear();
                let c = self.select_tile(*t, &l.layers, Some(p), &mut l.entries, &mut l.reach);
                sums.selection(&c, true);
                if c.held {
                    coarsening.insert(*t, ());
                }
                capped.extend(l.entries.iter().map(|(m, role)| (*m, *t, *role)));
            }
            capped.sort_unstable();
            (sums, coarsening)
        });
        let rebuild_all = full || capped_run.is_some() || self.down.capped;
        let mut dirty = std::mem::take(&mut l.items);
        if rebuild_all {
            add(&mut dirty, self.down.items.keys().copied());
            if capped_run.is_some() {
                add(&mut dirty, capped.iter().map(|x| x.0));
            } else {
                add(&mut dirty, self.down.contrib.keys().map(|x| x.0));
            }
        } else {
            // A dirty tile's own mesh (its resources, its place), and those
            // serving it; a moved tile's too (a held mesh reads its levels, a
            // mesh serving it dresses for them).
            for &t in l.dirty.iter().chain(&l.moved) {
                add(&mut dirty, [ResourceId::Mesh(t)]);
                if let Some(c) = self.down.sel.get(&t) {
                    sel_entries(t, Some(c), &self.down.entries, &mut l.entries);
                    add(&mut dirty, l.entries.iter().map(|e| e.0));
                }
            }
        }
        dirty.sort_unstable();
        dirty.dedup();
        // Every item from scratch, before any is replaced: they read last
        // step's draw set.
        #[cfg(debug_assertions)]
        let expected = self.all_items(tiles, capped_run.as_ref().map(|_| &capped[..]));
        l.wants_dirty.clear();
        l.drawn.clear();
        l.worn.clear();
        self.walk(dirty.len());
        for &m in &dirty {
            l.run.clear();
            if capped_run.is_some() {
                let from = capped.partition_point(|x| x.0 < m);
                l.run.extend(
                    capped[from..]
                        .iter()
                        .take_while(|x| x.0 == m)
                        .map(|x| (x.1, x.2)),
                );
            } else {
                let lo = (m, TileId(0), Role::Exact);
                let hi = (m, TileId(u64::MAX), Role::Fallback);
                l.run.extend(
                    self.down
                        .contrib
                        .range(lo..=hi)
                        .map(|((_, t, r), _)| (*t, *r)),
                );
            }
            l.reach.clear();
            l.beyond.clear();
            let new = if l.run.is_empty() {
                None
            } else {
                let built = l.built.get_or_insert_with(|| DrawItem {
                    mesh: m,
                    role: Role::Exact,
                    drapes: Vec::new(),
                    serves: Vec::new(),
                });
                let drapes = |s: TileId, out: &mut Vec<(LayerId, u32)>| match tiles.get(&s) {
                    Some(tn) => self.drapes_of(s, tn, out),
                    None => out.clear(),
                };
                let stats = self.build_item(m, &l.run, drapes, &mut l.reach, &mut l.bufs, built);
                l.reach.beyond(m.tile(), &mut l.beyond);
                Some(stats)
            };
            if full {
                l.old_reach.clear();
            } else {
                self.reach_of(Unit::item(m), &mut l.old_reach);
            }
            Self::reread(
                Unit::item(m),
                &l.old_reach,
                &l.beyond,
                &mut self.down,
                &mut l.c_reach,
                &mut l.c_readers,
            );
            self.replace_item(m, new, l);
        }
        let d = &mut self.down;
        d.items.apply(&mut l.c_items, |_| {});
        d.reach.apply(&mut l.c_reach, |_| {});
        d.readers.apply(&mut l.c_readers, |_| {});
        self.wear(l);
        #[cfg(debug_assertions)]
        self.check_items(&expected);
        // The selection's statistics: the capped one's while capped.
        match capped_run {
            Some((sums, coarsening)) => {
                let d = &mut self.down;
                d.capped = true;
                d.capped_coarsening = coarsening;
                d.capped_sel = Some(sums);
            }
            None => {
                self.down.capped = false;
                self.down.capped_sel = None;
            }
        }
        l.items = dirty;
        l.capped = capped;
    }

    /// The item of `m` as built — `l.built` with `stats`, or none — in place
    /// of the one kept, in the batch; what changed follows: the
    /// statistics, the drapes worn, the drawn mark of `m`, the held set (at
    /// the step's end), the wants of `m`'s tile, and what read `m`'s last
    /// item (next step).
    fn replace_item(&mut self, m: ResourceId, stats: Option<ItemStats>, l: &mut StepLists) {
        let d = &mut self.down;
        let old = d.items.get(&m);
        let same = match (old, stats, &l.built) {
            (Some(a), Some(st), Some(b)) => a.same(b, &st),
            (None, None, _) => true,
            _ => false,
        };
        if same {
            return;
        }
        let new = match (stats, &l.built) {
            (Some(st), Some(b)) => Some(Item::of(b, st)),
            _ => None,
        };
        add(&mut d.next, [m.tile()]);
        l.wants_dirty.push(m.tile());
        if let Some(o) = old {
            d.sums.item(o, false);
            l.worn.extend(o.drapes().map(|x| (x.resource, -1)));
        }
        if let Some(n) = &new {
            d.sums.item(n, true);
            l.worn.extend(n.drapes().map(|x| (x.resource, 1)));
        }
        let held = |i: Option<&Item>| i.is_some_and(|i| i.role != Role::Fallback);
        if held(old) != held(new.as_ref()) {
            d.held_changes.push((m.tile(), held(new.as_ref())));
        }
        if old.is_some() != new.is_some() {
            let on = new.is_some();
            if let Some(s) = self.arena.find(m) {
                self.arena.mark(s, Mark::Drawn, on);
            }
            l.drawn.push(m);
        }
        put(&mut self.down.items, &mut l.c_items, m, new);
    }

    /// The drapes worn: this step's counts applied, each drape that starts
    /// or stops being worn marked drawn or not.
    fn wear(&mut self, l: &mut StepLists) {
        if l.worn.is_empty() {
            return;
        }
        l.worn.sort_unstable_by_key(|w| w.0);
        for run in l.worn.chunk_by(|a, b| a.0 == b.0) {
            let r = run[0].0;
            let delta: i32 = run.iter().map(|x| x.1).sum();
            if delta == 0 {
                continue;
            }
            let before = self.down.worn.get(&r).copied().unwrap_or(0);
            let after = u32::try_from(i64::from(before) + i64::from(delta)).unwrap_or(0);
            put(
                &mut self.down.worn,
                &mut l.c_worn,
                r,
                (after > 0).then_some(after),
            );
            if (before > 0) != (after > 0) {
                if let Some(s) = self.arena.find(r) {
                    self.arena.mark(s, Mark::Drawn, after > 0);
                }
                l.drawn.push(r);
            }
        }
        l.worn.clear();
        self.down.worn.apply(&mut l.c_worn, |_| {});
    }

    /// The wants of the tiles whose need, levels, mesh or mesh's item
    /// changed (every tile's when `all`); the resources whose want changed,
    /// into `l.wants`.
    fn wants_pass(&mut self, tiles: &SortedMap<TileId, TileNeed>, all: bool, l: &mut StepLists) {
        l.wants.clear();
        let meshes_in = self.meshes_in(tiles);
        let mut dirty = std::mem::take(&mut l.wants_dirty);
        add(&mut dirty, l.dirty.iter().copied());
        add(&mut dirty, l.named.iter().copied());
        add(&mut dirty, l.moved.iter().copied());
        if all {
            add(&mut dirty, tiles.keys().copied());
            add(&mut dirty, self.down.wants.keys().map(|r| r.tile()));
        }
        dirty.sort_unstable();
        dirty.dedup();
        self.walk(dirty.len());
        for &t in &dirty {
            match tiles.get(&t) {
                Some(tn) => {
                    let item = self.down.drawn(ResourceId::Mesh(t));
                    self.tile_wants(t, tn, item, meshes_in, &mut l.drapes, &mut l.tile_wants);
                }
                None => l.tile_wants.clear(),
            }
            l.old_wants.clear();
            l.old_wants
                .extend(of_tile(&self.down.wants, t).map(|(r, w)| (*r, *w)));
            self.walk(l.old_wants.len());
            // Both lists by resource: a merge.
            let (old, new) = (&l.old_wants, &l.tile_wants);
            let wants = &mut self.down.wants;
            let (mut i, mut j) = (0, 0);
            while i < old.len() || j < new.len() {
                let (r, w) = match (old.get(i), new.get(j)) {
                    (Some(a), Some(b)) if a.0 == b.0 => {
                        i += 1;
                        j += 1;
                        if a.1 == b.2 {
                            continue;
                        }
                        (b.0, Some(b.2))
                    }
                    (Some(a), Some(b)) if a.0 < b.0 => {
                        i += 1;
                        (a.0, None)
                    }
                    (Some(a), None) => {
                        i += 1;
                        (a.0, None)
                    }
                    (_, Some(b)) => {
                        j += 1;
                        (b.0, Some(b.2))
                    }
                    (None, None) => break,
                };
                put(wants, &mut l.c_wants, r, w);
                l.wants.push(r);
            }
        }
        self.down.wants.apply(&mut l.c_wants, |_| {});
        l.wants_dirty = dirty;
    }

    /// Drawn content of an older epoch is refreshed, new before old: read
    /// again whenever some GPU copy is stale, or a refresh was wanted.
    fn refresh_pass(&mut self, full: bool, l: &mut StepLists) {
        if !(full || !self.down.refresh.is_empty() || self.any_stale()) {
            return;
        }
        let mut fresh = std::mem::take(&mut l.refresh);
        fresh.clear();
        for (m, i) in self.down.items.iter() {
            self.walk(1 + i.drapes().count());
            for r in std::iter::once(*m).chain(i.drapes().map(|x| x.resource)) {
                if self.down.wants.contains_key(&r) {
                    continue;
                }
                if let Some(w) = self.refresh_of(r) {
                    fresh.push((r, w));
                }
            }
        }
        // The first refresh of a resource listed twice (a stable sort).
        fresh.sort_by_key(|e| e.0);
        fresh.dedup_by_key(|e| e.0);
        // The refreshes that came, went or changed.
        let old = &self.down.refresh;
        for (r, w) in old.iter() {
            if fresh.binary_search_by_key(r, |e| e.0).map(|i| fresh[i].1) != Ok(*w) {
                l.wants.push(*r);
            }
        }
        for (r, w) in &fresh {
            if old.get(r) != Some(w) {
                l.wants.push(*r);
            }
        }
        self.down.refresh.rebuild(fresh.iter().copied());
        l.refresh = fresh;
    }

    /// The held ancestry follows the tree positions the need changed: the
    /// held tiles whose walk passed a named tile, or are named, walk again
    /// (as the positions were, as they are); the tiles their walks reach
    /// differently, into `l.pairs`. Read whole from the held set when it is
    /// not kept.
    fn ancestry_follow(&mut self, named: &[Named], full: bool, l: &mut StepLists) {
        if full || !self.down.ancestry_ready {
            self.held_rebuild();
            return;
        }
        if named.is_empty() {
            return;
        }
        let before = |t: TileId| match named.binary_search_by_key(&t, |x| x.tile) {
            Ok(i) => named[i].before(),
            Err(_) => self.tiles.get(&t).copied(),
        };
        l.held.clear();
        for &Named { tile: x, .. } in named {
            if self.held_last.contains_key(&x) {
                l.held.push(x);
            }
            l.held.extend(self.held_under(x, before).map(|(h, _)| h));
        }
        l.held.sort_unstable();
        l.held.dedup();
        for i in 0..l.held.len() {
            let h = l.held[i];
            Self::chain_of(h, before, &mut l.chain);
            Self::chain_of(h, |t| self.tiles.get(&t).copied(), &mut l.chain_now);
            self.walk(2 + l.chain.len() + l.chain_now.len());
            // A tile whose pair with `h` came, went or changed.
            for &(p, passed) in &l.chain {
                if !l.chain_now.contains(&(p, passed)) {
                    l.pairs.push(p);
                }
            }
            for &(p, passed) in &l.chain_now {
                if !l.chain.contains(&(p, passed)) {
                    l.pairs.push(p);
                }
            }
            let was = self.held_key(h, before);
            let now = self.held_key(h, |t| self.tiles.get(&t).copied());
            self.down.held_put(h, was, false);
            self.down.held_put(h, now, true);
        }
    }

    /// The held set read whole: each held tile filed by its walk.
    fn held_rebuild(&mut self) {
        let d = &mut self.down;
        d.held_regular.clear();
        d.held_irregular.clear();
        self.walk(self.held_last.len());
        let held = std::mem::take(&mut self.held_last);
        for (h, _) in held.iter() {
            let k = self.held_key(*h, |t| self.tiles.get(&t).copied());
            self.down.held_put(*h, k, true);
        }
        self.held_last = held;
        self.down.ancestry_ready = true;
    }

    /// Where `h` is filed in the held set, by its walk as `info` tells it:
    /// geometric, by its Morton key with the level its walk reaches; else
    /// (`None`) with the others.
    fn held_key(
        &self,
        h: TileId,
        info: impl Fn(TileId) -> Option<TileInfo>,
    ) -> Option<(Morton, u8)> {
        let k = morton(h)?;
        self.geometric_walk(h, info).map(|top| (k, top))
    }

    /// A held tile `h` joins (`on`) or leaves the held set: the tiles its
    /// walk reaches read the held tiles below them again next step.
    fn ancestry_held(&mut self, h: TileId, on: bool, l: &mut StepLists) {
        if !self.down.ancestry_ready {
            return;
        }
        Self::chain_of(h, |t| self.tiles.get(&t).copied(), &mut l.chain);
        self.walk(1 + l.chain.len());
        for &(p, _) in &l.chain {
            self.down.next_pairs(p);
        }
        let k = self.held_key(h, |t| self.tiles.get(&t).copied());
        self.down.held_put(h, k, on);
    }

    // The prune's support (see `Support`).

    /// Whether `t` is a seed: wanted, held, or holding a tracked resource.
    /// `overlay`: tiles whose change is not yet counted, with their tree
    /// position and membership before it.
    fn seed(&self, t: TileId, tiles: &SortedMap<TileId, TileNeed>, overlay: &[Named]) -> bool {
        let wanted = match overlay.binary_search_by_key(&t, |x| x.tile) {
            Ok(i) => overlay[i].was_wanted(),
            Err(_) => tiles.get(&t).is_some(),
        };
        wanted || self.held_last.contains_key(&t) || self.tiles.get(&t).is_some_and(|i| i.slots > 0)
    }

    /// Where `t`'s keep counts, when it is kept and its place known, and
    /// whether it is kept.
    fn support_of(
        &self,
        t: TileId,
        tiles: &SortedMap<TileId, TileNeed>,
        overlay: &[Named],
    ) -> (Option<TileId>, bool) {
        let kept = self.seed(t, tiles, overlay) || self.down.support.kids.contains_key(&t);
        let info = match overlay.binary_search_by_key(&t, |x| x.tile) {
            Ok(i) => overlay[i].before(),
            Err(_) => self.tiles.get(&t).copied(),
        };
        (info.filter(|_| kept).and_then(|i| i.parent()), kept)
    }

    /// Whether `t`, known, is kept.
    fn kept_tile(&self, t: TileId, tiles: &SortedMap<TileId, TileNeed>) -> bool {
        self.seed(t, tiles, &[]) || self.down.support.kids.contains_key(&t)
    }

    /// `p` gains (`on`) or loses a kept child; what follows up the tree.
    fn kid(&mut self, p: TileId, on: bool, tiles: &SortedMap<TileId, TileNeed>, overlay: &[Named]) {
        let (mut p, mut on) = (p, on);
        for _ in 0..=self.tiles.len() + 1 {
            let before = self.support_of(p, tiles, overlay);
            let kids = &mut self.down.support.kids;
            let n = kids.get(&p).copied().unwrap_or(0);
            if on {
                kids.insert(p, n + 1);
            } else if n <= 1 {
                kids.remove(&p);
            } else {
                kids.insert(p, n - 1);
            }
            let after = self.support_of(p, tiles, overlay);
            if before.1 && !after.1 && self.tiles.get(&p).is_some() {
                self.down.support.unkept.push(p);
            }
            self.walk(1);
            match (before.0, after.0) {
                (None, Some(q)) => (p, on) = (q, true),
                (Some(q), None) => (p, on) = (q, false),
                _ => return,
            }
        }
        // Around a cycle: the counts no longer tell.
        self.down.support.whole = true;
    }

    /// `t`'s seed, place or kids changed: `before` is where its keep counted
    /// and whether it was kept; what follows.
    fn support_moved(
        &mut self,
        t: TileId,
        before: (Option<TileId>, bool),
        tiles: &SortedMap<TileId, TileNeed>,
        overlay: &[Named],
    ) {
        let after = self.support_of(t, tiles, overlay);
        if before.1 && !after.1 && self.tiles.get(&t).is_some() {
            self.down.support.unkept.push(t);
        }
        if before.0 != after.0 {
            if let Some(q) = before.0 {
                self.kid(q, false, tiles, overlay);
            }
            if let Some(q) = after.0 {
                self.kid(q, true, tiles, overlay);
            }
        }
    }

    /// The support follows the named tiles' new places and memberships, one
    /// tile at a time, those not yet counted seen as they were. No support
    /// kept: rebuilt.
    fn support_follow(&mut self, named: &[Named], tiles: &SortedMap<TileId, TileNeed>) {
        if !self.down.support.ready {
            self.support_rebuild(tiles);
            return;
        }
        for i in 0..named.len() {
            let t = named[i].tile;
            let before = self.support_of(t, tiles, &named[i..]);
            self.support_moved(t, before, tiles, &named[i + 1..]);
            // A new parent link may close a cycle.
            let old_parent = named[i].before().and_then(|x| x.parent());
            let new_parent = self.tiles.get(&t).and_then(|x| x.parent());
            if new_parent.is_some() && new_parent != old_parent {
                let mut x = new_parent;
                for _ in 0..=self.tiles.len() {
                    self.walk(1);
                    match x {
                        Some(y) if y == t => {
                            self.down.support.whole = true;
                            break;
                        }
                        Some(y) => x = self.tiles.get(&y).and_then(|i| i.parent()),
                        None => break,
                    }
                }
            }
        }
    }

    /// The support rebuilt: every tile's tracked resources counted, the kept
    /// tiles found from the seeds as the prune finds them, each kept known
    /// tile counted towards its parent.
    fn support_rebuild(&mut self, tiles: &SortedMap<TileId, TileNeed>) {
        self.walk(self.tiles.len() + self.arena.len());
        let mut overflow = false;
        for i in self.tiles.values_mut() {
            i.slots = 0;
        }
        for s in self.arena.iter() {
            let t = self.arena.id(s).tile();
            match self.tiles.get_mut(&t) {
                Some(i) => match i.slots.checked_add(1) {
                    Some(x) => i.slots = x,
                    None => overflow = true,
                },
                None => overflow = true,
            }
        }
        let keep = self.keep_set(tiles);
        let mut kids: SortedMap<TileId, u32> = SortedMap::default();
        let mut unkept = Vec::new();
        let mut parents: Vec<TileId> = Vec::new();
        for (t, i) in self.tiles.iter() {
            if keep.binary_search(t).is_ok() {
                parents.extend(i.parent());
            } else {
                unkept.push(*t);
            }
        }
        parents.sort_unstable();
        kids.rebuild(
            parents
                .chunk_by(|a, b| a == b)
                .map(|run| (run[0], sat_u32(run.len()))),
        );
        // A cycle among the known places: the counts would hold it up.
        let cyclic = self.tiles.iter().any(|(t, i)| {
            let mut x = i.parent();
            for _ in 0..=self.tiles.len() {
                match x {
                    Some(y) if y == *t => return true,
                    Some(y) => x = self.tiles.get(&y).and_then(|i| i.parent()),
                    None => return false,
                }
            }
            true
        });
        self.down.support = Support {
            ready: true,
            kids,
            unkept,
            whole: overflow || cyclic,
        };
    }

    /// The tiles still referenced — wanted, holding a tracked resource,
    /// held — and every ancestor of theirs, sorted: the prune's walk, as
    /// before O1.4.
    fn keep_set(&self, tiles: &SortedMap<TileId, TileNeed>) -> Vec<TileId> {
        let mut keep: Vec<TileId> = tiles.keys().copied().collect();
        keep.reserve(self.arena.len());
        keep.extend(self.arena.iter().map(|s| self.arena.id(s).tile()));
        keep.extend(self.held_last.keys().copied());
        keep.sort_unstable();
        keep.dedup();
        let seeds = keep.len();
        for i in 0..seeds {
            let mut cur = keep[i];
            let walk = keep.len();
            while let Some(p) = self.tiles.get(&cur).and_then(|i| i.parent()) {
                // A seed's ancestors are walked from the seed itself; a
                // tile met twice in one walk is a cycle.
                if keep[..seeds].binary_search(&p).is_ok() || keep[walk..].contains(&p) {
                    break;
                }
                keep.push(p);
                cur = p;
            }
        }
        keep.sort_unstable();
        keep.dedup();
        keep
    }

    /// A slot of tile `t` came (`on`) or went: its tile's count, and what
    /// follows for its keep.
    pub(super) fn tile_slot(&mut self, t: TileId, on: bool, tiles: &SortedMap<TileId, TileNeed>) {
        let before = self.support_of(t, tiles, &[]);
        let Some(i) = self.tiles.get_mut(&t) else {
            // A resource of a tile whose place is unknown: the counts no
            // longer tell.
            self.down.support.whole = true;
            return;
        };
        match if on {
            i.slots.checked_add(1)
        } else {
            i.slots.checked_sub(1)
        } {
            Some(x) => i.slots = x,
            None => {
                self.down.support.whole = true;
                return;
            }
        }
        if self.down.support.ready {
            self.support_moved(t, before, tiles, &[]);
        }
    }

    /// The mark (step 4): each resource whose want, drawn mark or slot
    /// changed — or whose tile's distance may have — is marked needed now,
    /// or unneeded since when; the swap and chain marks follow the wants
    /// and what became drawable; the arena's newest group moves on to
    /// `now`, the resources the view left leaving it.
    pub(super) fn mark(
        &mut self,
        now: Micros,
        tiles: &SortedMap<TileId, TileNeed>,
        named: &[Named],
        wants: Wants<'_>,
    ) {
        let mut l = std::mem::take(&mut self.down.step);
        l.marks.clear();
        add(&mut l.marks, l.wants.iter().copied());
        add(&mut l.marks, l.drawn.iter().copied());
        add(&mut l.marks, self.down.fresh.drain(..));
        for &Named { tile: t, .. } in named {
            if tiles.get(&t).is_some() {
                add(&mut l.marks, of_tile(wants.tiles, t).map(|(r, _)| *r));
                add(&mut l.marks, of_tile(wants.refresh, t).map(|(r, _)| *r));
            }
        }
        l.marks.sort_unstable();
        l.marks.dedup();
        self.walk(l.marks.len());
        l.slots.clear();
        let grace = self.cfg.cancel_grace;
        for &r in &l.marks {
            let Some(s) = self.arena.find(r) else {
                continue;
            };
            let a = &mut self.arena;
            let in_wants = wants.contains_key(&r);
            let in_drawn = a.marked(s, Mark::Drawn);
            if in_wants {
                let cold = a.cold_mut(s);
                cold.set_unneeded_since(None);
                cold.first_wanted_or(now);
                if let Some(tn) = tiles.get(&r.tile()) {
                    a.set_distance(s, tn.q.distance);
                }
            } else if in_drawn {
                let cold = a.cold_mut(s);
                cold.set_unneeded_since(None);
                cold.set_first_wanted(None);
            } else if a.marked(s, Mark::Wanted) {
                let cold = a.cold_mut(s);
                let since = cold.unneeded_since();
                cold.unneeded_since_or(now);
                cold.set_first_wanted(None);
                add(&mut self.down.departed, [r]);
                if self.down.kept_empty.contains_key(&s) {
                    // Kept empty while wanted: the next prune looks at it
                    // again, whatever its time (a time may go back).
                    self.down.kept_empty.insert(s, Micros::ZERO);
                    self.down.kept_due.push(Reverse((Micros::ZERO, s)));
                }
                if since.is_none() {
                    // Unneeded from now: past its grace, its load or transit
                    // is cancelled.
                    self.down.deadline(now.plus(grace), r);
                }
            }
            a.mark(s, Mark::Wanted, in_wants || in_drawn);
            if in_wants || in_drawn {
                l.slots.push(s);
            }
        }
        // Swap and chain: drawable members of the view's swaps.
        self.walk(l.wants.len() + l.logged.len());
        for &r in l.wants.iter().chain(&l.logged) {
            let Some(s) = self.arena.find(r) else {
                continue;
            };
            let u = wants.get(&r).map(|w| w.urgency);
            let (swap, chain) = swap_chain(self.arena.has_gpu(s), u);
            self.arena.mark(s, Mark::Swap, swap);
            self.arena.mark(s, Mark::Chain, chain);
        }
        // The newest group. Those that left the view since it last moved,
        // and still members, leave it.
        let mut leavers = std::mem::take(&mut self.down.leavers);
        leavers.clear();
        self.walk(self.down.departed.len());
        for &r in &self.down.departed {
            if let Some(s) = self.arena.find(r) {
                if self.arena.is_newest(s) && !self.arena.marked(s, Mark::Wanted) {
                    leavers.push(s);
                }
            }
        }
        leavers.sort_unstable();
        leavers.dedup();
        self.down.advanced = false;
        if self.arena.newest().is_some_and(|t0| now < t0) {
            // A time gone back: played as before, every resource the view
            // needs touched.
            let n = self.arena.regress();
            self.walk(n);
            self.down.explicit = true;
            let mut all = std::mem::take(&mut l.done);
            all.clear();
            all.extend(wants.iter().map(|(r, _)| *r));
            all.extend(self.down.items.keys().copied());
            all.extend(self.down.worn.keys().copied());
            all.sort_unstable();
            all.dedup();
            self.walk(all.len());
            for &r in &all {
                if let Some(s) = self.arena.find(r) {
                    self.arena.touch(s, now);
                }
            }
            l.done = all;
            self.down.advanced = true;
        } else if std::mem::take(&mut self.down.touch_all) {
            // After a time gone back: every resource the view needs.
            let mut all = std::mem::take(&mut l.done);
            all.clear();
            all.extend(wants.iter().map(|(r, _)| *r));
            all.extend(self.down.items.keys().copied());
            all.extend(self.down.worn.keys().copied());
            all.sort_unstable();
            all.dedup();
            self.walk(all.len());
            l.slots.clear();
            l.slots
                .extend(all.iter().filter_map(|r| self.arena.find(*r)));
            l.done = all;
            if !l.slots.is_empty() || self.arena.newest_len() > leavers.len() {
                if self.arena.advance(now, &leavers) == Advance::Moved {
                    self.down.departed.clear();
                }
                for &s in &l.slots {
                    self.arena.touch(s, now);
                }
                self.down.advanced = true;
            }
        } else if !l.slots.is_empty() || self.arena.newest_len() > leavers.len() {
            if self.arena.advance(now, &leavers) == Advance::Moved {
                self.down.departed.clear();
            }
            for &s in &l.slots {
                self.arena.touch(s, now);
            }
            self.down.advanced = true;
        }
        self.down.leavers = leavers;
        self.down.step = l;
    }

    /// Before a slot is made at `now`: the newest group is `now`'s (the
    /// mark touched nothing, so it did not move it on).
    pub(super) fn newest_at(&mut self, now: Micros) {
        if self.down.advanced {
            return;
        }
        if self.arena.newest() != Some(now) {
            let leavers = std::mem::take(&mut self.down.leavers);
            if self.arena.advance(now, &leavers) == Advance::Moved {
                self.down.departed.clear();
            }
            self.down.leavers = leavers;
        }
        self.down.advanced = true;
    }

    /// A slot was made for `r`, wanted, by a load: marked at the next step.
    pub(super) fn made(&mut self, r: ResourceId) {
        self.down.fresh.push(r);
    }

    /// `r` may be cancelled at `t`.
    pub(super) fn deadline(&mut self, t: Micros, r: ResourceId) {
        self.down.deadline(t, r);
    }

    /// The work: each wanted resource whose want, copies or load changed,
    /// or that waits on a time, is read again — a load, a delivery, none —
    /// and whether the view waits on it in flight.
    pub(super) fn update_jobs(&mut self, now: Micros, wants: Wants<'_>, full: bool) {
        let mut l = std::mem::take(&mut self.down.step);
        l.marks.clear();
        // What changed since the passes read the arena (the cancels), and
        // what they read.
        let changes = self.arena.changes();
        let since = &changes[self.down.seen.min(changes.len())..];
        self.walk(since.len());
        add(&mut l.marks, since.iter().map(|c| c.0));
        let read = changes.len();
        add(&mut l.marks, l.all.iter().copied());
        add(&mut l.marks, l.wants.iter().copied());
        // Those whose wait fell due: a retry, a delivery's backoff.
        while let Some(&Reverse((t, r))) = self.down.wakes.peek() {
            if t > now {
                break;
            }
            self.down.wakes.pop();
            self.walk(1);
            if self.down.waiting.get(&r) == Some(&t) {
                add(&mut l.marks, [r]);
            }
        }
        if full {
            add(&mut l.marks, wants.iter().map(|(r, _)| *r));
            add(&mut l.marks, self.down.jobs.keys().copied());
            add(&mut l.marks, self.down.inflight.keys().copied());
        }
        l.marks.sort_unstable();
        l.marks.dedup();
        self.walk(l.marks.len());
        for &r in &l.marks {
            let w = wants.get(&r);
            let (job, wait) = match w {
                Some(w) => self.job_of(r, w, now),
                None => (None, None),
            };
            let flying = w.is_some_and(|w| w.urgency <= Urgency::Drape)
                && self
                    .arena
                    .find(r)
                    .is_some_and(|s| self.arena.has_transit(s) || self.arena.loading(s));
            let d = &self.down;
            let set = |m: &Set<ResourceId>, c: &mut Changes<ResourceId, ()>, on: bool| {
                if m.contains_key(&r) != on {
                    c.push((r, on.then_some(())));
                }
            };
            set(&d.jobs, &mut l.c_set, job.is_some());
            set(&d.inflight, &mut l.c_set3, flying);
            if d.waiting.get(&r).copied() != wait {
                l.c_wait.push((r, wait));
            }
        }
        let d = &mut self.down;
        d.jobs.apply(&mut l.c_set, |_| {});
        d.inflight.apply(&mut l.c_set3, |_| {});
        for &(r, t) in &l.c_wait {
            if let Some(t) = t {
                d.wakes.push(Reverse((t, r)));
            }
        }
        d.waiting.apply(&mut l.c_wait, |_| {});
        if d.wakes.len() > 2 * d.waiting.len() + 64 {
            // Mostly stale: the waits kept, again.
            d.wakes.clear();
            d.wakes
                .extend(d.waiting.iter().map(|(r, t)| Reverse((*t, *r))));
        }
        #[cfg(debug_assertions)]
        {
            let mut jobs = Vec::new();
            let mut waiting = Vec::new();
            let mut inflight = Vec::new();
            for (r, w) in wants.iter() {
                let (job, wait) = self.job_of(*r, w, now);
                if job.is_some() {
                    jobs.push(*r);
                }
                if let Some(t) = wait {
                    waiting.push((*r, t));
                }
                if w.urgency <= Urgency::Drape
                    && self
                        .arena
                        .find(*r)
                        .is_some_and(|s| self.arena.has_transit(s) || self.arena.loading(s))
                {
                    inflight.push(*r);
                }
            }
            for v in [&mut jobs, &mut inflight] {
                v.sort_unstable();
            }
            waiting.sort_unstable();
            let keys = |m: &Set<ResourceId>| m.keys().copied().collect::<Vec<_>>();
            assert_eq!(
                keys(&self.down.jobs),
                jobs,
                "the work kept is the work found"
            );
            assert_eq!(
                self.down
                    .waiting
                    .iter()
                    .map(|(r, t)| (*r, *t))
                    .collect::<Vec<_>>(),
                waiting,
                "the waits kept are those found"
            );
            for (r, t) in self.down.waiting.iter() {
                assert!(
                    self.down.wakes.iter().any(|w| w.0 == (*t, *r)),
                    "{r:?}'s wait is queued"
                );
            }
            assert_eq!(
                keys(&self.down.inflight),
                inflight,
                "what the view waits on in flight"
            );
        }
        // The changes read are forgotten.
        self.arena.forget_changes(read);
        self.down.seen = 0;
        self.down.step = l;
    }

    /// The step's end: the held set takes this step's draw set (its
    /// ancestry and the prune's support follow), a time gone back is
    /// settled.
    pub(super) fn end_step(&mut self, tiles: &SortedMap<TileId, TileNeed>) {
        let changes = std::mem::take(&mut self.down.held_changes);
        let mut l = std::mem::take(&mut self.down.step);
        l.held.clear();
        for &(t, on) in &changes {
            let before = self.support_of(t, tiles, &[]);
            let changed = if on {
                self.held_last.insert(t, ()).is_none()
            } else {
                self.held_last.remove(&t).is_some()
            };
            if !changed {
                continue;
            }
            if !on {
                // Its kept levels may go.
                l.held.push(t);
            }
            self.ancestry_held(t, on, &mut l);
            if self.down.support.ready {
                self.support_moved(t, before, tiles, &[]);
            }
        }
        self.down.held_changes = changes;
        self.down.held_changes.clear();
        #[cfg(debug_assertions)]
        {
            let held: Vec<TileId> = self
                .down
                .items()
                .filter(|(_, i)| i.role != Role::Fallback)
                .map(|(m, _)| m.tile())
                .collect();
            assert_eq!(
                self.held_last.keys().copied().collect::<Vec<_>>(),
                held,
                "the held set is the draw set's"
            );
            self.check_ancestry();
            self.check_support(tiles);
        }
        if self.down.explicit {
            l.slots.clear();
            self.arena.settle(&mut l.slots);
            self.walk(l.slots.len());
            self.down.departed.clear();
            self.down
                .departed
                .extend(l.slots.iter().map(|&s| self.arena.id(s)));
            self.down.explicit = false;
            self.down.touch_all = true;
        }
        self.down.advanced = false;
        l.trim();
        self.down.step = l;
    }

    /// The prune's every step: the kept levels of the tiles that left the
    /// drape needs or the held set. Returns the keys looked at.
    pub(super) fn prune_levels(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        named: &[Named],
    ) -> u32 {
        let keeps = |b: &Budget, t: TileId, l: LayerId| {
            b.held_last.contains_key(&t)
                || tiles.get(&t).is_some_and(|tn| {
                    Self::draped(tn)
                        && tn
                            .need
                            .imagery
                            .as_ref()
                            .is_some_and(|i| i.layers().iter().any(|x| x.layer == l))
                })
        };
        let mut gone = std::mem::take(&mut self.down.step.gone);
        gone.clear();
        let visited;
        if self.cfg.policy == Policy::Batch || !self.down.ready {
            visited = sat_u32(self.imagery_prev.len());
            gone.extend(
                self.imagery_prev
                    .keys()
                    .filter(|k| !keeps(self, k.0, k.1))
                    .copied(),
            );
        } else {
            let mut candidates = std::mem::take(&mut self.down.step.tiles);
            candidates.clear();
            candidates.extend(named.iter().filter(|x| x.shaped()).map(|x| x.tile));
            candidates.extend_from_slice(&self.down.step.held);
            candidates.sort_unstable();
            candidates.dedup();
            let mut n = 0u32;
            for &t in &candidates {
                let lo = (t, LayerId(0));
                let hi = (t, LayerId(u16::MAX));
                for (k, _) in self.imagery_prev.range(lo..=hi) {
                    n += 1;
                    if !keeps(self, k.0, k.1) {
                        gone.push(*k);
                    }
                }
            }
            visited = n;
            self.down.step.tiles = candidates;
        }
        for k in &gone {
            self.imagery_prev.remove(k);
        }
        self.down.step.gone = gone;
        visited
    }

    /// The prune's eighth steps: the arena's empty slots that nothing
    /// keeps, the expired recent evictions, the tiles nothing references.
    /// Returns the entries looked at.
    pub(super) fn prune_tracked(
        &mut self,
        now: Micros,
        tiles: &SortedMap<TileId, TileNeed>,
        wants: Wants<'_>,
    ) -> u32 {
        let mut visited = 0u32;
        // Loads and transits: the index keeps only those still going.
        let mut done = std::mem::take(&mut self.down.step.done);
        done.clear();
        let a = &self.arena;
        done.extend(
            self.active
                .iter()
                .filter(|r| {
                    !a.find(**r)
                        .is_some_and(|s| a.has_transit(s) || a.loading(s))
                })
                .copied(),
        );
        visited = visited.saturating_add(sat_u32(self.active.len()));
        for r in &done {
            self.active.remove(r);
            if let Some(s) = self.arena.find(*r) {
                self.arena.mark(s, Mark::Active, false);
            }
        }
        self.down.step.done = done;
        // Empty slots: those emptied since, and those kept whose time fell
        // due.
        let mut slots = std::mem::take(&mut self.down.step.slots);
        self.arena.take_emptied(&mut slots);
        while let Some(&Reverse((t, s))) = self.down.kept_due.peek() {
            if t > now {
                break;
            }
            self.down.kept_due.pop();
            visited = visited.saturating_add(1);
            if self.down.kept_empty.get(&s) == Some(&t) {
                slots.push(s);
            }
        }
        slots.sort_unstable();
        slots.dedup();
        visited = visited.saturating_add(sat_u32(slots.len()));
        let mut c_kept = std::mem::take(&mut self.down.step.c_kept);
        for &s in &slots {
            let until = if self.arena.live_slot(s) && self.arena.is_empty(s) {
                match kept_until(&self.arena, s, now, wants) {
                    Some(t) => Some(t),
                    None => {
                        let t = self.arena.id(s).tile();
                        self.arena.remove(s);
                        self.tile_slot(t, false, tiles);
                        None
                    }
                }
            } else {
                None
            };
            if self.down.kept_empty.get(&s).copied() != until {
                if let Some(t) = until {
                    self.down.kept_due.push(Reverse((t, s)));
                }
                c_kept.push((s, until));
            }
        }
        self.down.kept_empty.apply(&mut c_kept, |_| {});
        self.down.step.c_kept = c_kept;
        let d = &mut self.down;
        if d.kept_due.len() > 2 * d.kept_empty.len() + 64 {
            // Mostly stale: the slots kept, again.
            d.kept_due.clear();
            d.kept_due
                .extend(d.kept_empty.iter().map(|(s, t)| Reverse((*t, *s))));
        }
        #[cfg(debug_assertions)]
        for s in self.arena.iter() {
            if self.arena.is_empty(s) {
                assert!(
                    kept_until(&self.arena, s, now, wants).is_some(),
                    "the prune frees every empty slot nothing keeps ({:?})",
                    self.arena.id(s)
                );
            }
        }
        self.down.step.slots = slots;
        // Recent evictions past the thrash window.
        visited = visited.saturating_add(self.expire_recent(now));
        // The tiles nothing references, nor any descendant.
        visited = visited.saturating_add(self.prune_tiles(tiles));
        visited
    }

    /// An eviction of `r` at `now`, for the thrash count.
    pub(super) fn evicted(&mut self, r: ResourceId, now: Micros) {
        self.recent.insert(r, now);
        let d = &mut self.down;
        if d.recent.back().is_some_and(|(t, _)| *t > now) {
            d.recent_disordered = true;
        }
        d.recent.push_back((now, r));
        // Superseded entries: dropped once they outnumber the live ones.
        if d.recent.len() > 2 * self.recent.len() + 64 {
            d.recent.retain(|(t, r)| self.recent.get(r) == Some(t));
        }
    }

    /// The recent evictions past the thrash window go; past the cap, the
    /// oldest. Returns the entries looked at.
    fn expire_recent(&mut self, now: Micros) -> u32 {
        let mut visited = 0u32;
        if self.down.recent_disordered || self.down.recent.len() < self.recent.len() {
            // Times that went back, or the queue dropped: walked whole, as
            // before O1.4, and the queue rebuilt.
            visited = sat_u32(self.recent.len());
            self.recent.retain(|_, t| now.since(*t) <= THRASH_WINDOW);
            let q = &mut self.down.recent;
            q.clear();
            q.extend(self.recent.iter().map(|(r, t)| (*t, *r)));
            q.make_contiguous().sort_unstable();
            self.down.recent_disordered = false;
        }
        while let Some(&(t, r)) = self.down.recent.front() {
            if now.since(t) <= THRASH_WINDOW {
                break;
            }
            visited += 1;
            self.down.recent.pop_front();
            if self.recent.get(&r) == Some(&t) {
                self.recent.remove(&r);
            }
        }
        if self.recent.len() > RECENT_CAP {
            // The oldest go, by (time, resource): one sort, not one scan
            // per removal.
            visited = visited.saturating_add(sat_u32(self.recent.len()));
            let q = &mut self.down.recent;
            q.clear();
            q.extend(self.recent.iter().map(|(r, t)| (*t, *r)));
            q.make_contiguous().sort_unstable();
            let excess = q.len() - RECENT_CAP;
            for (_, r) in q.drain(..excess) {
                self.recent.remove(&r);
            }
        }
        visited
    }

    /// The tiles nothing references any more, nor any descendant, go.
    /// Returns the tiles looked at.
    fn prune_tiles(&mut self, tiles: &SortedMap<TileId, TileNeed>) -> u32 {
        if self.down.support.whole || !self.down.support.ready {
            // The counts do not tell: the whole walk, as before O1.4.
            let keep = self.keep_set(tiles);
            let visited = sat_u32(self.tiles.len() + self.arena.len());
            self.tiles.retain(|t, _| keep.binary_search(t).is_ok());
            return visited;
        }
        let mut gone = std::mem::take(&mut self.down.support.unkept);
        gone.sort_unstable();
        gone.dedup();
        let visited = sat_u32(gone.len());
        gone.retain(|t| self.tiles.get(t).is_some() && !self.kept_tile(*t, tiles));
        #[cfg(debug_assertions)]
        {
            let keep = self.keep_set(tiles);
            let want: Vec<TileId> = self
                .tiles
                .keys()
                .filter(|t| keep.binary_search(t).is_err())
                .copied()
                .collect();
            assert_eq!(gone, want, "the support frees what the prune's walk frees");
        }
        for t in &gone {
            self.tiles.remove(t);
            self.down.support.kids.remove(t);
        }
        gone.clear();
        self.down.support.unkept = gone;
        visited
    }
}

impl Downstream {
    /// `h` filed at `key` (`on`), or taken out of it.
    fn held_put(&mut self, h: TileId, key: Option<(Morton, u8)>, on: bool) {
        let d = self;
        match (key.map(|k| (k.0, (h, k.1))), on) {
            (Some((k, v)), true) => {
                d.held_regular.insert(k, v);
            }
            (Some((k, _)), false) => {
                d.held_regular.remove(&k);
            }
            (None, true) => {
                d.held_irregular.insert(h, ());
            }
            (None, false) => {
                d.held_irregular.remove(&h);
            }
        }
    }

    /// The tiles above held tiles whose pairs changed at the step's end:
    /// their selections read the held tiles below them again next step.
    fn next_pairs(&mut self, p: TileId) {
        add(&mut self.next_sels, [p]);
    }
}

// The debug build's check: every unit read again from scratch, compared with
// what the step kept. A dependant the rules leave out shows here.
#[cfg(debug_assertions)]
impl Budget {
    /// The held ancestry against the closure built from the held set: the
    /// held set filed by each walk, and for every tile a walk reaches, the
    /// held tiles below it and the levels passed, as the closure has them.
    fn check_ancestry(&self) {
        if !self.down.ancestry_ready {
            return;
        }
        let info = |t: TileId| self.tiles.get(&t).copied();
        let mut closure = Vec::new();
        let mut chain = Vec::new();
        for (h, _) in self.held_last.iter() {
            Self::chain_of(*h, info, &mut chain);
            closure.extend(chain.iter().map(|&(p, passed)| (p, *h, passed)));
            match self.held_key(*h, info) {
                Some((k, top)) => assert_eq!(
                    self.down.held_regular.get(&k),
                    Some(&(*h, top)),
                    "{h:?} filed geometric"
                ),
                None => assert!(
                    self.down.held_irregular.contains_key(h),
                    "{h:?} filed apart"
                ),
            }
        }
        assert_eq!(
            self.down.held_regular.len() + self.down.held_irregular.len(),
            self.held_last.len(),
            "the held set filed, each once"
        );
        closure.sort_unstable();
        for run in closure.chunk_by(|a, b| a.0 == b.0) {
            let of = run[0].0;
            let mut found: Vec<(TileId, TileId, u32)> =
                self.held_under(of, info).map(|(h, p)| (of, h, p)).collect();
            found.sort_unstable();
            assert_eq!(found, run, "the held tiles below {of:?}");
        }
    }

    /// The support's counts against the prune's walk.
    fn check_support(&self, tiles: &SortedMap<TileId, TileNeed>) {
        let sup = &self.down.support;
        if !sup.ready || sup.whole {
            return;
        }
        let keep = self.keep_set(tiles);
        let mut parents: Vec<TileId> = self
            .tiles
            .iter()
            .filter(|(t, _)| keep.binary_search(t).is_ok())
            .filter_map(|(_, i)| i.parent())
            .collect();
        parents.sort_unstable();
        let kids: Vec<(TileId, u32)> = parents
            .chunk_by(|a, b| a == b)
            .map(|run| (run[0], sat_u32(run.len())))
            .collect();
        let kept: Vec<(TileId, u32)> = sup.kids.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(
            kept, kids,
            "the support counts each kept tile's kept children"
        );
        let mut slots: Vec<TileId> = self.arena.iter().map(|s| self.arena.id(s).tile()).collect();
        slots.sort_unstable();
        for (t, i) in self.tiles.iter() {
            let n = slots.partition_point(|x| x <= t) - slots.partition_point(|x| x < t);
            assert_eq!(
                usize::from(i.slots),
                n,
                "the tracked resources of {t:?} are counted"
            );
        }
    }

    /// The drape levels against a reading of every tile, from last step's
    /// levels: a tile that asks the presence index must be read again when
    /// its answers may have changed.
    fn check_levels(
        &self,
        tiles: &SortedMap<TileId, TileNeed>,
        presence: &dyn PresenceOracle,
        last: &std::collections::BTreeMap<(TileId, LayerId), u32>,
    ) {
        /// Asked, it answers nothing and says so.
        struct Silent(std::cell::Cell<bool>);
        impl PresenceOracle for Silent {
            fn holds(&self, _: ResourceId) -> super::tier::Presence {
                self.0.set(true);
                super::tier::Presence::Absent
            }
        }
        let mut levels = Vec::new();
        // An index that tells its generation is asked again: its answers
        // hold. One that does not is not asked twice in a step.
        let stable = presence.generation().is_some();
        for (t, tn) in tiles.iter().filter(|(_, tn)| Self::draped(tn)) {
            let far = self.down.levels_far.contains_key(t);
            if far && !stable {
                continue;
            }
            let silent = Silent(std::cell::Cell::new(false));
            let oracle: &dyn PresenceOracle = if far { presence } else { &silent };
            let mut asked = false;
            self.drape_levels(*t, tn, oracle, last, &mut levels, &mut asked);
            assert!(
                far || (!asked && !silent.0.get()),
                "{t:?} asked the presence index but is not read when it may have changed"
            );
            for (layer, level) in &levels {
                assert_eq!(
                    self.imagery_prev.get(&(*t, *layer)),
                    Some(level),
                    "the level of {t:?}'s layer {layer:?} is the one a reading gives"
                );
            }
        }
    }

    /// The floor and the selection against a reading of every drawn tile.
    fn check_selection(&self, tiles: &SortedMap<TileId, TileNeed>) {
        let floor: Vec<(TileId, bool)> = tiles
            .iter()
            .filter(|(_, tn)| self.is_floor(tn.need.level))
            .map(|(t, tn)| (*t, self.floor_ready(*t, tn)))
            .collect();
        let kept: Vec<(TileId, bool)> = self.down.floor.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(kept, floor, "the floor kept is the floor found");
        assert_eq!(
            self.down.floor_missing,
            sat_u32(floor.iter().filter(|f| !f.1).count()),
            "the floor's missing tiles counted"
        );
        let mut contrib = Vec::new();
        let mut coarsening = Vec::new();
        let mut sums = Sums::default();
        let (mut drapes, mut layers, mut entries, mut old) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut reach = Reach::default();
        let mut beyond = Vec::new();
        let mut kept_reach = Vec::new();
        let drawn: Vec<TileId> = tiles
            .iter()
            .filter(|(_, n)| n.membership == Membership::Drawn)
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(
            self.down.sel.keys().copied().collect::<Vec<_>>(),
            drawn,
            "a selection kept for each drawn tile"
        );
        for &t in &drawn {
            let tn = tiles.get(&t).unwrap_or_else(|| unreachable!());
            self.layers_of(t, tn, &mut drapes, &mut layers);
            reach.clear();
            let counts = self.select_tile(t, &layers, None, &mut entries, &mut reach);
            reach.beyond(t, &mut beyond);
            self.reach_of(Unit::sel(t), &mut kept_reach);
            assert_eq!(kept_reach, beyond, "what {t:?}'s selection reads");
            assert_eq!(self.down.sel.get(&t), Some(&counts), "{t:?}'s selection");
            sel_entries(t, Some(&counts), &self.down.entries, &mut old);
            assert_eq!(old, entries, "{t:?}'s selection's entries");
            sums.selection(&counts, true);
            if counts.held {
                coarsening.push(t);
            }
            contrib.extend(entries.iter().map(|(m, r)| (*m, t, *r)));
        }
        contrib.sort_unstable();
        assert_eq!(
            self.down.contrib.keys().copied().collect::<Vec<_>>(),
            contrib,
            "the runs kept are those found"
        );
        assert_eq!(
            self.down.coarsening.keys().copied().collect::<Vec<_>>(),
            coarsening,
            "the coarsening tiles"
        );
        let s = &self.down.sums;
        assert_eq!(
            (s.exact, s.fallback, s.lost, &s.depths),
            (sums.exact, sums.fallback, sums.lost, &sums.depths),
            "the selection's counts"
        );
    }

    /// Every item read from scratch, from the runs given (a capped
    /// selection's) or kept, with what each read of other tiles.
    fn all_items(
        &self,
        tiles: &SortedMap<TileId, TileNeed>,
        capped: Option<&[(ResourceId, TileId, Role)]>,
    ) -> Expected {
        let runs: Vec<(ResourceId, TileId, Role)> = match capped {
            Some(all) => all.to_vec(),
            None => self.down.contrib.keys().copied().collect(),
        };
        let mut items = Vec::new();
        let mut bufs = ItemBufs::default();
        for run in runs.chunk_by(|a, b| a.0 == b.0) {
            let m = run[0].0;
            let run: Vec<(TileId, Role)> = run.iter().map(|x| (x.1, x.2)).collect();
            let mut reach = Reach::default();
            let drapes = |s: TileId, out: &mut Vec<(LayerId, u32)>| match tiles.get(&s) {
                Some(tn) => self.drapes_of(s, tn, out),
                None => out.clear(),
            };
            let mut built = DrawItem {
                mesh: m,
                role: Role::Exact,
                drapes: Vec::new(),
                serves: Vec::new(),
            };
            let stats = self.build_item(m, &run, drapes, &mut reach, &mut bufs, &mut built);
            let mut beyond = Vec::new();
            reach.beyond(m.tile(), &mut beyond);
            items.push((m, Item::of(&built, stats), beyond));
        }
        items
    }

    /// The draw set kept against the one read from scratch, and what follows
    /// from it: who reads what, its statistics, the drapes worn, the drawn
    /// marks.
    fn check_items(&self, expected: &Expected) {
        let kept: Vec<(&ResourceId, &Item)> = self.down.items.iter().collect();
        let want: Vec<(&ResourceId, &Item)> = expected.iter().map(|(m, i, _)| (m, i)).collect();
        assert_eq!(kept, want, "the draw set kept is the one built");
        let mut reach = Vec::new();
        for (m, _, beyond) in expected {
            self.reach_of(Unit::item(*m), &mut reach);
            assert_eq!(&reach, beyond, "what {m:?}'s item reads");
        }
        let mut readers: Vec<((TileId, TileId, u8), u8)> = self
            .down
            .reach
            .iter()
            .map(|((ut, tag, t), w)| ((*t, *ut, *tag), *w))
            .collect();
        readers.sort_unstable();
        assert_eq!(
            self.down
                .readers
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect::<Vec<_>>(),
            readers,
            "the readers are the units' reaches"
        );
        let mut sums = Sums::default();
        let mut worn: Vec<ResourceId> = Vec::new();
        for (_, i, _) in expected {
            sums.item(i, true);
            worn.extend(i.drapes().map(|d| d.resource));
        }
        let s = &self.down.sums;
        assert_eq!(
            (s.bare, s.underdraped, s.held, s.over_terrain),
            (sums.bare, sums.underdraped, sums.held, sums.over_terrain),
            "the draw set's counts"
        );
        worn.sort_unstable();
        let worn: Vec<(ResourceId, u32)> = worn
            .chunk_by(|a, b| a == b)
            .map(|run| (run[0], sat_u32(run.len())))
            .collect();
        let kept_worn: Vec<(ResourceId, u32)> =
            self.down.worn.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(kept_worn, worn, "the drapes worn are counted");
        let a = &self.arena;
        for s in a.iter() {
            let r = a.id(s);
            let drawn =
                self.down.items.contains_key(&r) || worn.binary_search_by_key(&r, |w| w.0).is_ok();
            assert_eq!(a.marked(s, Mark::Drawn), drawn, "the drawn mark of {r:?}");
        }
    }

    /// The wants and refreshes kept against a reading of every tile and
    /// every drawn resource.
    fn check_wants(&self, tiles: &SortedMap<TileId, TileNeed>) {
        let meshes_in = self.meshes_in(tiles);
        let mut want = Vec::new();
        let (mut drapes, mut put) = (Vec::new(), Vec::new());
        for (t, tn) in tiles.iter() {
            let item = self.down.drawn(ResourceId::Mesh(*t));
            self.tile_wants(*t, tn, item, meshes_in, &mut drapes, &mut put);
            want.extend(put.iter().map(|(r, _, w)| (*r, *w)));
        }
        want.sort_unstable_by_key(|e| e.0);
        let kept: Vec<(ResourceId, Want)> = self.down.wants.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(kept, want, "the wants kept are the wants put");
        let mut refresh: Vec<(ResourceId, Want)> = Vec::new();
        for (m, i) in self.down.items.iter() {
            for r in std::iter::once(*m).chain(i.drapes().map(|x| x.resource)) {
                if want.binary_search_by_key(&r, |e| e.0).is_ok() {
                    continue;
                }
                if let Some(w) = self.refresh_of(r) {
                    refresh.push((r, w));
                }
            }
        }
        refresh.sort_by_key(|e| e.0);
        refresh.dedup_by_key(|e| e.0);
        let kept: Vec<(ResourceId, Want)> =
            self.down.refresh.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(kept, refresh, "the refreshes kept are those found");
    }

    /// The marks every slot carries against the step's wants and draw set,
    /// and the newest group against them.
    pub(super) fn check_marks(&self, wants: Wants<'_>) {
        let a = &self.arena;
        for s in a.iter() {
            let r = a.id(s);
            let w = wants.get(&r);
            let drawn = a.marked(s, Mark::Drawn);
            assert_eq!(
                a.marked(s, Mark::Wanted),
                w.is_some() || drawn,
                "the wanted mark of {r:?}"
            );
            assert_eq!(
                (a.marked(s, Mark::Swap), a.marked(s, Mark::Chain)),
                swap_chain(a.has_gpu(s), w.map(|w| w.urgency)),
                "the swap and chain marks of {r:?}"
            );
            // After a time gone back, the older group may hold what is
            // needed (the next step touches it all).
            let needed = w.is_some() || drawn;
            if !self.down.touch_all && needed {
                assert!(a.is_newest(s), "{r:?}, needed now, is of the newest group");
            } else if !self.down.touch_all && a.is_newest(s) {
                assert!(
                    self.down.departed.contains(&r),
                    "{r:?}, of the newest group but not needed, is to leave it"
                );
            }
            let active = self.active.contains(&r);
            assert_eq!(
                a.marked(s, Mark::Active),
                active,
                "the active mark of {r:?}"
            );
        }
    }
}
