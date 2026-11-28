// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One decision step, in order:
//!
//! 1. apply the recorded arrivals, in a canonical order;
//! 2. read the need: the need kept from the last step, changed by what
//!    changed (or replaced by a whole need) — each tile that entered or
//!    changed quantised and merged again (`need_state`); then choose each
//!    tile's imagery level (with hysteresis, Interactive only);
//! 3. build the draw set from acknowledged resources only: exact, held finer
//!    descendants on a zoom-out, the nearest drawable ancestor otherwise, and
//!    each mesh's best drawable drape;
//! 4. the protected set: drawn, in transit, the floor;
//! 5. cancel what left the need past its grace, and loads past their timeout;
//! 6. rank the work by an integer key; start loads within per-class quotas,
//!    local reads under their own;
//! 7. deliver decoded copies within the credit, the target, the hard limit
//!    and its reserve;
//! 8. plan the consumer's uploads, byte-capped, swap groups together;
//! 9. evict outside the protected set down to the low watermark; release
//!    decoded copies over their budget;
//! 10. relax the LOD with time, never with frames.
//!
//! Every container that is iterated is ordered (`BTreeMap`, `BTreeSet`,
//! sorted `Vec`, the arena's slots, whose finds are sorted by a total key
//! before they decide anything): no decision depends on a hash order. The
//! arena's hash index is only probed.
//!
//! Eviction walks an order the arena keeps up to date (`state`'s eviction
//! index, merged with the recently needed resources sorted once a step), and
//! knows whether room can be made from the bytes it counts, without a scan.

// The decisions are integer arithmetic: floats stay in `imagery` and
// `priority`, quantised once, where bit-exactness is controlled.
#![deny(clippy::float_arithmetic)]

use std::cmp::Reverse;
use std::sync::atomic::Ordering::Relaxed;

use smallvec::SmallVec;

use super::blocked::{Blocked, BLOCKED};
use super::config::FailurePolicy;
use super::cost::within_drift;
use super::decisions::{Decisions, DrapeChoice, DrawItem, Role, Start, UploadPlan};
use super::downstream::{Item, Wants};
use super::error::{AuditError, SessionError};
use super::imagery::{
    drape_rank, ideal_level, needed_level, DrapeCandidate, GeoRect, LevelClamp, UvTransform,
};
use super::need::{LayerReach, Membership};
use super::need_state::{NeedState, TileNeed, WantedImagery};
use super::origin::Origin;
use super::priority::{score, PriorityKey, Quantised, Urgency};
use super::score::{Reason, ScoreEvent, Side, Terms};
use super::sorted::SortedMap;
use super::state::{Arena, CpuCopy, EvictKey, Gpu, Held, Load, Mark, Rank, Slot, Transit};
use super::stats::{
    layer_index, level_index, prefetch_index, BudgetStats, CancelReason, EvictReason,
    LATENCY_BUCKETS,
};
use super::sweep::Verdict;
use super::tier::{Presence, PresenceOracle, Source, Tier};
use super::types::{
    sat_u32, Bytes, Class, Failure, Generation, LayerId, Micros, PerClass, Policy, Pressure,
    ResourceId, Thermal, TileId,
};
use super::view::{Protection, ResourceState, ResourceView};
use super::{Budget, Event, FrameInput, Ledger, NeedInput, TileInfo, THRASH_WINDOW};

/// The step's effective numbers, after pressure, thermal and retarget.
struct Eff {
    target: PerClass<Bytes>,
    overflow: PerClass<Bytes>,
    hard: PerClass<Bytes>,
    reserve: PerClass<Bytes>,
    decoded: Bytes,
    quota: PerClass<u32>,
    local: u32,
    upload: Bytes,
    /// The plan's encode lane, blocks a step (CB5).
    encode: u64,
    batch: bool,
    /// The pooled ledger's numbers (D-A3); `None` with no member, and
    /// under Batch.
    pool: Option<PoolEff>,
    /// The preload envelope (`regulator`, E1–E2): the bytes and network
    /// slots each class reserves for the present need, and the decoded
    /// bytes; all zero under Batch.
    env: PerClass<Bytes>,
    env_slots: PerClass<u32>,
    env_decoded: Bytes,
}

impl Eff {
    /// The line a class's preloads fit under, with the present need's
    /// bytes: its target less its reserve.
    fn line(&self, c: Class) -> Bytes {
        self.target[c].minus(self.env[c])
    }

    /// The pool members' reserves together.
    fn pool_env(&self, p: &PoolEff) -> Bytes {
        Class::ALL
            .iter()
            .filter(|c| p.members[**c])
            .fold(Bytes::ZERO, |a, c| a.plus(self.env[*c]))
    }
}

/// Whether work of `urgency` on `r` is a preload (`regulator`): the ring, a
/// refresh, an ancestor's own drapes (E4) — never what the view draws, the
/// chain of meshes it falls back on, or a drape a drawn tile wears.
pub(super) fn is_preload(urgency: Urgency, r: ResourceId) -> bool {
    matches!(urgency, Urgency::Ring | Urgency::Refresh)
        || (urgency == Urgency::Ancestor && matches!(r, ResourceId::Drape { .. }))
}

/// What a step's preloads may still take (E1, E3): per class the GPU bytes
/// held outside the cache (what nothing wants, which any admission takes
/// first) with what the preloads have pending — in flight at their
/// estimate, decoded at their cost — and the decoded bytes with the loads
/// in flight.
struct EnvUse {
    gpu: PerClass<Bytes>,
    pool: Bytes,
    decoded: Bytes,
}

impl EnvUse {
    /// Whether a preload of `bytes` of class `c` fits every envelope it
    /// will take: its class's, the pool's, the decoded tier's.
    fn admits(&self, eff: &Eff, c: Class, bytes: Bytes) -> bool {
        use super::regulator::fits;
        fits(self.gpu[c], bytes, eff.target[c], eff.env[c])
            && eff.pool.filter(|p| p.members[c]).is_none_or(|p| {
                fits(self.pool, bytes, p.target, eff.pool_env(&p))
            })
            && fits(self.decoded, bytes, eff.decoded, eff.env_decoded)
    }

    /// Counts a preload of `bytes` of class `c` started.
    fn take(&mut self, eff: &Eff, c: Class, bytes: Bytes) {
        self.gpu[c] = self.gpu[c].plus(bytes);
        if eff.pool.is_some_and(|p| p.members[c]) {
            self.pool = self.pool.plus(bytes);
        }
        self.decoded = self.decoded.plus(bytes);
    }
}

/// The pool's numbers for a step (D-A3): its totals after pressure and
/// `retarget`, each member's floor and its ceilings.
#[derive(Clone, Copy)]
struct PoolEff {
    members: PerClass<bool>,
    target: Bytes,
    overflow: Bytes,
    hard: Bytes,
    floor_percent: PerClass<u8>,
    /// Each member's own target and hard limit: its ceilings.
    ceil_target: PerClass<Bytes>,
    ceil_overflow: PerClass<Bytes>,
    ceil_hard: PerClass<Bytes>,
}

impl PoolEff {
    /// What the other members use.
    fn others(&self, c: Class, used: &PerClass<Bytes>) -> Bytes {
        Class::ALL
            .iter()
            .filter(|o| **o != c && self.members[**o])
            .fold(Bytes::ZERO, |a, o| a.plus(used[*o]))
    }

    /// A member's floor, of the pool's `total`.
    fn floor(&self, c: Class, total: Bytes) -> Bytes {
        total.scaled(u64::from(self.floor_percent[c]), 100)
    }

    /// A member's target, given what every member uses: what the pool
    /// leaves once the others are counted, between its floor and its
    /// ceiling.
    fn target(&self, c: Class, used: &PerClass<Bytes>) -> Bytes {
        self.target
            .minus(self.others(c, used))
            .max(self.floor(c, self.target))
            .min(self.ceil_target[c])
    }

    /// Its overflow, in proportion to its target.
    fn overflow(&self, c: Class, target: Bytes) -> Bytes {
        self.overflow
            .scaled(target.0, self.target.0.max(1))
            .min(self.ceil_overflow[c])
    }

    /// Its hard limit: what the pool's hard limit leaves once the others
    /// are counted, between its floor and its ceiling, never under its
    /// target.
    fn hard(&self, c: Class, used: &PerClass<Bytes>, target: Bytes) -> Bytes {
        self.hard
            .minus(self.others(c, used))
            .max(self.floor(c, self.hard))
            .min(self.ceil_hard[c])
            .max(target)
    }
}

/// One wanted resource this step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Want {
    pub urgency: Urgency,
    pub q: Quantised,
    pub class: Class,
    pub level: u32,
}

/// A placeholder, for the sorted maps that keep wants.
impl Default for Want {
    fn default() -> Self {
        Want {
            urgency: Urgency::Refresh,
            q: Quantised::default(),
            class: Class::Drape,
            level: 0,
        }
    }
}

/// What a drawn tile's selection adds to the step's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TileSelection {
    /// Drawn as itself.
    pub exact: bool,
    /// Drawn by an ancestor alone.
    pub fallback: bool,
    /// Ground drawn by nothing (a 3D Tiles tile not drawn is never lost:
    /// the ground under it is drawn).
    pub lost: bool,
    /// Drawn by the finer tiles it held last step: coarsening.
    pub held: bool,
    /// How far up its ancestor is, when one stands in.
    pub depth: u32,
}

/// What a unit of the passes read beyond its own tile's mesh and drapes:
/// the tiles whose place, mesh, drapes or last item it read. A change to
/// any of them reads the unit again (`downstream`).
#[derive(Debug, Default)]
pub(super) struct Reach {
    pub tiles: Vec<(TileId, u8)>,
}

/// What a unit read of a tile ([`Reach`]): its place in the tree (parent,
/// level, footprint), whether its mesh is drawable, which of its drapes
/// are, the drapes its mesh wore last step.
pub(super) const PLACE: u8 = 1;
pub(super) const MESH: u8 = 1 << 1;
pub(super) const DRAPES: u8 = 1 << 2;
pub(super) const WORN: u8 = 1 << 3;

/// A map by (tile, layer) of what a step keeps about each drape level: the
/// level chosen, a stand-in's own, a gap, a dwell's start. Sorted blocks —
/// the passes read it tile by tile in key order — whose upkeep is not
/// counted into `work` (see `Budget::imagery_prev`).
pub(super) type LevelMap<V> = super::sorted::SortedMap<(TileId, LayerId), V>;

/// The imagery levels kept from the step before: the level chosen, and,
/// where a stand-in was worn in its place, the level the hysteresis carries
/// (`Budget::drape_levels`).
pub(super) type KeptLevels<'a> = (&'a LevelMap<DrapeLevel>, &'a LevelMap<u32>);

impl Reach {
    /// It read `what` of `t`.
    pub fn saw(&mut self, t: TileId, what: u8) {
        self.tiles.push((t, what));
    }

    /// Starts over.
    pub fn clear(&mut self) {
        self.tiles.clear();
    }

    /// What it read beyond what every unit of `own` reads — `own`'s place,
    /// mesh and drapes — by tile, sorted, each tile once, into `out`.
    pub fn beyond(&mut self, own: TileId, out: &mut Vec<(TileId, u8)>) {
        self.tiles.sort_unstable();
        out.clear();
        for &(t, w) in &self.tiles {
            let w = if t == own { w & WORN } else { w };
            if w == 0 {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.0 == t => last.1 |= w,
                _ => out.push((t, w)),
            }
        }
    }
}

/// What a drawn tile's layer was last read as: the imagery level it asks
/// for, and the levels the terrain's level denies the tile's imagery
/// ([`DrapeChoice::capped`], the same for each of its layers). The second
/// moves with the footprint while the first stays, and a change of either
/// is a level that moved: the items that say it are built again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DrapeLevel {
    pub level: u32,
    pub capped: u32,
}

/// A draw item's buffers while it is built.
#[derive(Debug, Default)]
pub(super) struct ItemBufs {
    needs: Vec<(LayerId, u32)>,
    own: Vec<(LayerId, u32)>,
}

/// What a draw item adds to the step's statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ItemStats {
    /// A layer it needs is worn by no drape.
    pub bare: bool,
    /// Exact, but some layer worn more than a level off its need.
    pub underdraped: bool,
    /// Exact: the levels its drapes are worn under their need, summed over
    /// its layers (the quality deficit's imagery part).
    pub under_levels: u16,
    /// Exact: the levels its drapes are worn over their need, summed.
    pub over_levels: u16,
    /// Its tile's level, for the imagery-over-terrain histogram.
    pub tile_level: u32,
}

/// What a walk of the levels must agree with: the deficit the header
/// counts, and the selection's exact, fallback, held and lost tiles. The
/// per-level `under_need` is the deficit by level, so the two are read off
/// the same step.
fn walk_counts(st: &BudgetStats) -> (super::stats::Deficit, [u32; 4]) {
    (st.deficit, [st.exact, st.fallback, st.held, st.lost])
}

/// A tile under its need, waiting on `r` for `why`, counted at level `at`
/// of class `c`: dated by the step it has been waited on since (`prev`,
/// sorted by resource; `now` for a new one), listed for the next walk.
#[allow(clippy::too_many_arguments)]
fn note_blocked(
    w: &mut WalkedLevels,
    prev: &[(ResourceId, Micros)],
    next: &mut Vec<(ResourceId, Micros)>,
    r: ResourceId,
    c: Class,
    at: usize,
    why: Blocked,
    now: Micros,
) {
    let since = prev
        .binary_search_by_key(&r, |e| e.0)
        .map_or(now, |i| prev[i].1);
    next.push((r, since));
    let k = why.index();
    let first = &mut w.blocked_since[c][at][k];
    if w.blocked[c][at][k] == 0 || since < *first {
        *first = since;
    }
    w.blocked[c][at][k] += 1;
}

/// What `Budget::level_stats` walks for: kept from one step to the next.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct WalkedLevels {
    selected: PerClass<[u32; super::stats::LEVELS]>,
    drawn: PerClass<[u32; super::stats::LEVELS]>,
    under_need: PerClass<[u32; super::stats::LEVELS]>,
    /// The tiles of `under_need` by why each waits, and the earliest
    /// step each reason's tiles have been under their need since.
    blocked: PerClass<[[u32; BLOCKED]; super::stats::LEVELS]>,
    blocked_since: PerClass<[[Micros; BLOCKED]; super::stats::LEVELS]>,
    layer_drawn: [u32; super::stats::LAYERS],
}

/// A step's buffers, kept across steps so that a step allocates nothing
/// once they have grown to the view.
#[derive(Debug, Default)]
pub(super) struct Scratch {
    /// The slots of an origin that came back, read by `retry_origin`.
    pub(super) origin_due: Vec<Slot>,
    /// The walked part of the last step's statistics per level and
    /// layer (`level_stats`), kept for the still steps that read it again.
    walked_levels: Option<WalkedLevels>,
    /// The resources the tiles under their need wait on, sorted, with the
    /// step each has been waited on since (`walk_levels`); and the buffer
    /// the next walk writes.
    blocked_since: Vec<(ResourceId, Micros)>,
    blocked_next: Vec<(ResourceId, Micros)>,
    walked_scale: u32,
    /// The deficit and the selection's counts the last walk was taken
    /// with: a step that moves them is not still for the walk, whatever
    /// else it decided (a drape swapped for one already resident moves
    /// the draw set and nothing else).
    walked_counts: Option<(super::stats::Deficit, [u32; 4])>,
    /// A drawn tile's chosen drape levels, for the step per level
    /// (`level_stats`).
    chosen: Vec<(LayerId, u32)>,
    /// The work ranked: each job's key (its resource in it), what it is,
    /// its want.
    jobs: Vec<(PriorityKey, Job, Want)>,
    /// The loads and transits whose deadline passed.
    due: Vec<ResourceId>,
    /// This step's room: built at its first use, dropped with the step's
    /// evictions. Only a step under pressure builds it.
    room: Option<Room>,
    /// The loads, then the deliveries, picked from `jobs`: their places;
    /// then the plan's transits by group.
    picked: Vec<u32>,
    /// The plan's: the copies in transit with their key (their resource in
    /// it), bytes and group, and whether the plan took them.
    transit: Vec<(PriorityKey, Bytes, (u8, TileId), bool)>,
    /// What the step's read-only passes walk, added to its `work`.
    walked: std::sync::atomic::AtomicU32,
    /// The copies the compression pass visits, and those it finds
    /// eligible (CB9).
    pub(super) compress_visit: Vec<ResourceId>,
    pub(super) compress_found: Vec<super::compress_pass::Found>,
}

/// What a step can evict beyond the arena's idle bytes: the grouped GPU
/// residents (needed at one of the two most recent times) outside the kept
/// set, the floor and transit, in eviction order, and their bytes by what
/// the step wants of them. Built at its first use in a step, after the mark:
/// the keys no longer move, and the set only shrinks (evictions, copies put
/// in transit), so later uses skip what left it. Each key with its slot: no
/// slot is freed within a step (`prune` runs last).
#[derive(Debug)]
pub(super) struct Room {
    grouped: Vec<(EvictKey, Slot)>,
    /// By class, the GPU bytes of `grouped` still evictable, by want:
    /// [`want_bucket`].
    bytes: PerClass<[Bytes; WANT_BUCKETS]>,
}

/// Not wanted, then one bucket per urgency.
const WANT_BUCKETS: usize = 8;

/// A resource's bucket in [`Room::bytes`]: 0 if the step does not want it,
/// else [`urgency_bucket`].
fn want_bucket(w: Option<&Want>) -> usize {
    w.map_or(0, |w| urgency_bucket(w.urgency))
}

/// The bucket of a want of urgency `u`: one past it, in urgency order.
fn urgency_bucket(u: Urgency) -> usize {
    1 + u as usize
}

/// Whether `room` holds grouped bytes of `class` wanted at a preload's
/// urgency (`is_preload`; an ancestor's meshes counted with its drapes):
/// what the envelope's release must walk the grouped copies for (E3).
fn holds_preloads(room: &Room, class: Class) -> bool {
    [Urgency::Ring, Urgency::Ancestor, Urgency::Refresh]
        .iter()
        .any(|u| room.bytes[class][urgency_bucket(*u)] > Bytes::ZERO)
}

/// A walk of the eviction order: the arena's filed resources, by key,
/// merged with a step's grouped ones, sorted. The two are disjoint (a
/// resource is filed or grouped), and both stay ordered while the walk
/// evicts what it visits.
#[derive(Default)]
struct Walk {
    /// The last filed entry visited.
    after: Option<(EvictKey, Slot)>,
    /// The next grouped entry.
    next: usize,
}

impl Walk {
    /// The next entry of a class `class` takes, holding one of `held` if
    /// filed: the grouped ones are taken as they are, their holdings left to
    /// the caller.
    fn next(
        &mut self,
        a: &Arena,
        grouped: &[(EvictKey, Slot)],
        class: impl Fn(Class) -> bool + Copy,
        held: &[Held],
    ) -> Option<(EvictKey, Slot)> {
        while grouped
            .get(self.next)
            .is_some_and(|(_, s)| !class(a.class(*s)))
        {
            self.next += 1;
        }
        let g = grouped.get(self.next).copied();
        let f = a.next_filed(self.after.as_ref(), class, held);
        match (f, g) {
            (Some(f), Some(g)) if g < f => {
                self.next += 1;
                Some(g)
            }
            (Some(f), _) => {
                self.after = Some(f);
                Some(f)
            }
            (None, Some(g)) => {
                self.next += 1;
                Some(g)
            }
            (None, None) => None,
        }
    }
}

/// Work on a wanted resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Job {
    Load,
    Deliver,
}

/// The layers a need asks for, in stacking order.
fn layers(img: &WantedImagery) -> &[LayerReach] {
    img.layers()
}

/// The layers `img` asks for, each once, in layer order.
fn layers_of_need(img: &WantedImagery) -> SmallVec<[LayerId; 4]> {
    let mut v: SmallVec<[LayerId; 4]> = layers(img).iter().map(|x| x.layer).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The floor swap group `r` belongs to (D-A1), if any: a floor tile's mesh
/// and its floor drape of every layer it wears, in stacking order.
fn floor_group<'t>(
    tiles: &'t SortedMap<TileId, TileNeed>,
    r: ResourceId,
    is_floor: impl Fn(Class, u32) -> bool,
) -> Option<impl Iterator<Item = ResourceId> + Clone + 't> {
    let t = r.tile();
    let tn = tiles.get(&t)?;
    let img = tn.need.imagery.as_ref()?;
    let level = tn.need.level;
    if !is_floor(tn.need.class, level) {
        return None;
    }
    let drapes = layers(img)
        .iter()
        .map(move |x| ResourceId::drape(t, x.layer, level));
    let member = r == ResourceId::Mesh(t) || drapes.clone().any(|m| m == r);
    member.then(|| std::iter::once(ResourceId::Mesh(t)).chain(drapes))
}

/// Frees one load slot of `count`. A slot freed twice would wrap a `u32`
/// to `u32::MAX` and freeze its quota (or panic in a debug build): it
/// saturates at 0 instead, and `underflows` records it for `audit`.
fn release(count: &mut u32, underflows: &mut u32) {
    match count.checked_sub(1) {
        Some(n) => *count = n,
        None => *underflows = underflows.saturating_add(1),
    }
}

fn backoff(policy: FailurePolicy, failures: u32, why: Failure) -> Micros {
    match policy {
        FailurePolicy::RetryBackoff { first, max } => match why {
            Failure::Permanent => max,
            Failure::Transient => {
                let shift = failures.saturating_sub(1).min(30);
                Micros(first.0.saturating_mul(1 << shift).min(max.0))
            }
        },
        _ => Micros::ZERO,
    }
}

fn latency_bucket(d: Micros) -> usize {
    let ms = d.0 / 1_000;
    let i = (64 - ms.leading_zeros()) as usize;
    i.min(LATENCY_BUCKETS - 1)
}

impl Budget {
    /// Counts what the kept maps spent keeping their order since the last
    /// step took it.
    pub(super) fn walk_upkeep(&mut self) {
        let n = self
            .tiles
            .take_upkeep()
            .saturating_add(self.held_last.take_upkeep())
            .saturating_add(self.arena.take_upkeep())
            .saturating_add(self.need.take_upkeep())
            .saturating_add(self.down.take_upkeep());
        self.walk(usize::try_from(n).unwrap_or(usize::MAX));
    }

    /// Counts `n` more elements walked by the step.
    pub(super) fn walk(&self, n: usize) {
        // Relaxed: a counter the step alone reads, through `&self` (a
        // `Budget` stays `Sync`).
        let w = &self.scratch.walked;
        let n = sat_u32(n);
        w.store(w.load(Relaxed).saturating_add(n), Relaxed);
    }

    fn class_of(&self, r: ResourceId, mesh_class: Class) -> Class {
        match r {
            ResourceId::Mesh(_) => mesh_class,
            ResourceId::Drape { .. } => Class::Drape,
        }
    }

    /// The content epoch `r` must be of: its class's, plus its layer's for
    /// a drape (D-A4: a layer refreshes on its own).
    fn epoch(&self, r: ResourceId, class: Class) -> u64 {
        self.epochs[class]
            + r.layer()
                .map_or(0, |l| self.layer_epochs.get(&l).copied().unwrap_or(0))
    }

    /// Whether a resource of `class` at `level` is of the floor: the
    /// ground's levels up to `pinned_level` ([`Class::is_ground`]).
    pub(super) fn is_floor(&self, class: Class, level: u32) -> bool {
        class.is_ground() && self.cfg.pinned_level.is_some_and(|p| level <= p)
    }

    fn eff(&self) -> Eff {
        let batch = self.cfg.policy == Policy::Batch;
        let (num, den) = self.factor;
        let pct = if batch {
            100
        } else {
            match self.pressure {
                Pressure::Normal => 100,
                Pressure::Warning => u64::from(self.cfg.pressure.warning_percent),
                Pressure::Critical => u64::from(self.cfg.pressure.critical_percent),
                Pressure::Background => u64::from(self.cfg.pressure.background_percent),
            }
        };
        let scale = |b: Bytes, p: u64| {
            if b == Bytes::MAX {
                b
            } else {
                b.scaled(num, den).scaled(p, 100)
            }
        };
        let thermal_div = if batch {
            1
        } else {
            match self.thermal {
                Thermal::Nominal | Thermal::Fair => 1,
                Thermal::Serious => 2,
                Thermal::Critical => 4,
            }
        };
        let mut hard = PerClass::from_fn(|c| scale(self.cfg.hard_limit[c], 100));
        let mut target = PerClass::from_fn(|c| scale(self.cfg.target[c], pct));
        let mut overflow = PerClass::from_fn(|c| scale(self.cfg.overflow[c], pct));
        let cp = self.cfg.pool;
        let pool = (!batch && Class::ALL.iter().any(|c| cp.members[*c])).then(|| {
            let p = PoolEff {
                members: cp.members,
                target: scale(cp.target, pct),
                overflow: scale(cp.overflow, pct),
                hard: scale(cp.hard_limit, 100),
                floor_percent: cp.floor_percent,
                ceil_target: target,
                ceil_overflow: overflow,
                ceil_hard: hard,
            };
            for c in Class::ALL.into_iter().filter(|c| p.members[*c]) {
                let t = p.target(c, &self.ledger.gpu);
                target[c] = t;
                overflow[c] = p.overflow(c, t);
                hard[c] = p.hard(c, &self.ledger.gpu, t);
            }
            p
        });
        // What the executor holds outside any resource (the encoder's
        // scratch) is off the limits it would otherwise count against —
        // the pool's included, for its members'. Nothing is taken when
        // nothing is reserved: limits as they were.
        let reserved = self.cfg.reserved;
        let off = |b: Bytes, r: Bytes| if b == Bytes::MAX { b } else { b.minus(r) };
        let mut pool = pool;
        if let Some(p) = pool.as_mut() {
            let members = Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |a, c| a.plus(reserved[*c]));
            p.target = off(p.target, members);
            p.overflow = off(p.overflow, members);
            p.hard = off(p.hard, members);
            for c in Class::ALL {
                p.ceil_target[c] = off(p.ceil_target[c], reserved[c]);
                p.ceil_overflow[c] = off(p.ceil_overflow[c], reserved[c]);
                p.ceil_hard[c] = off(p.ceil_hard[c], reserved[c]);
            }
        }
        for c in Class::ALL {
            target[c] = off(target[c], reserved[c]);
            overflow[c] = off(overflow[c], reserved[c]);
            hard[c] = off(hard[c], reserved[c]);
        }
        Eff {
            // Read after the ranking (`Budget::envelope_step`).
            env: PerClass::splat(Bytes::ZERO),
            env_slots: PerClass::splat(0),
            env_decoded: Bytes::ZERO,
            pool,
            target,
            overflow,
            reserve: PerClass::from_fn(|c| {
                if hard[c] == Bytes::MAX {
                    Bytes::ZERO
                } else {
                    hard[c].scaled(u64::from(self.cfg.hard_reserve_percent), 100)
                }
            }),
            hard,
            // The decoded tier is a target like the GPU's: retargeted with
            // them and scaled by pressure. The copies an eviction demotes
            // to are tracked state, bounded by this.
            decoded: off(
                scale(self.cfg.decoded_target, pct),
                self.cfg.decoded_reserved,
            ),
            quota: PerClass::from_fn(|c| (self.cfg.max_in_flight[c] / thermal_div).max(1)),
            local: self.cfg.max_local_reads,
            encode: if self.cfg.encode_blocks_per_step == u64::MAX {
                u64::MAX
            } else {
                self.cfg.encode_blocks_per_step / u64::from(thermal_div)
            },
            upload: if self.cfg.upload_bytes_per_frame == Bytes::MAX {
                Bytes::MAX
            } else {
                Bytes(self.cfg.upload_bytes_per_frame.0 / u64::from(thermal_div))
            },
            batch,
        }
    }

    // 1. Arrivals.
    pub(super) fn apply_events(&mut self, now: Micros, st: &mut BudgetStats) {
        let mut events = std::mem::take(&mut self.events);
        // Events are compared whole: equal ones are identical, and an
        // unstable sort orders them as a stable one would.
        events.sort_unstable();
        // The origins that answered after they were down.
        let mut back: Vec<Origin> = Vec::new();
        for &ev in &events {
            match ev {
                Event::Loaded(r, g, cost) => {
                    let Some(s) = self.arena.find(r) else {
                        st.stale += 1;
                        continue;
                    };
                    match self.arena.load(s) {
                        Load::Loading {
                            gen,
                            source,
                            started,
                            epoch,
                        } if gen == g => {
                            // The answer of a down origin: its resources
                            // that back off are retried (`origin`). Not an
                            // expired copy's load, which the store may have
                            // served for an origin that cannot answer.
                            if source == Source::Network
                                && !self.arena.cold(s).store_missed
                                && self.origins.answered(Origin::of(r, self.arena.class(s)))
                            {
                                back.push(Origin::of(r, self.arena.class(s)));
                            }
                            self.arena.set_load(s, Load::Idle);
                            match source {
                                Source::Network => release(
                                    &mut self.in_flight[self.arena.class(s)],
                                    &mut self.underflows,
                                ),
                                Source::Local => {
                                    release(&mut self.local_reads, &mut self.underflows);
                                    st.network_bytes_avoided =
                                        st.network_bytes_avoided.plus(cost.gpu);
                                }
                            }
                            self.arena.put_cpu(
                                s,
                                &mut self.ledger,
                                CpuCopy {
                                    gen,
                                    cost,
                                    epoch,
                                    delivered_before: false,
                                },
                            );
                            let cold = self.arena.cold_mut(s);
                            cold.failures = 0;
                            cold.store_missed = false;
                            st.completed += 1;
                            let (c, at) = (self.arena.class(s), level_index(self.arena.level(s)));
                            st.levels.completions[c][at] += 1;
                            if let Some(l) = self.arena.id(s).layer() {
                                st.layers.completions[layer_index(l)] += 1;
                            }
                            st.latency_ms[latency_bucket(now.since(started))] += 1;
                        }
                        _ => st.stale += 1,
                    }
                }
                Event::Failed(r, g, why) => {
                    let Some(s) = self.arena.find(r) else {
                        continue;
                    };
                    if let Load::Loading { gen, source, .. } = self.arena.load(s) {
                        if gen == g {
                            self.arena.set_load(s, Load::Idle);
                            let class = self.arena.class(s);
                            match source {
                                Source::Network => {
                                    release(&mut self.in_flight[class], &mut self.underflows)
                                }
                                Source::Local => {
                                    release(&mut self.local_reads, &mut self.underflows)
                                }
                            }
                            st.failed += 1;
                            st.failures[class][usize::from(why == Failure::Permanent)] += 1;
                            if let Some(l) = r.layer() {
                                st.layers.failures[layer_index(l)] += 1;
                            }
                            if self.cfg.policy == Policy::Batch {
                                self.fatal.get_or_insert(SessionError::LoadFailed(r));
                            } else {
                                if source == Source::Network {
                                    self.origins.failed(
                                        Origin::of(r, class),
                                        now,
                                        &self.cfg.origin,
                                    );
                                }
                                let cold = self.arena.cold_mut(s);
                                cold.failures += 1;
                                cold.retry_at =
                                    now.plus(backoff(self.cfg.failure, cold.failures, why));
                            }
                        }
                    }
                }
                Event::StoreMiss(r, g) => {
                    let Some(s) = self.arena.find(r) else {
                        continue;
                    };
                    if let Load::Loading {
                        gen,
                        source: Source::Local,
                        ..
                    } = self.arena.load(s)
                    {
                        if gen == g {
                            self.arena.set_load(s, Load::Idle);
                            release(&mut self.local_reads, &mut self.underflows);
                            let cold = self.arena.cold_mut(s);
                            cold.store_missed = true;
                            cold.presence = Some(Presence::Absent);
                            self.arena.note_tier(s);
                            st.store_misses += 1;
                        }
                    }
                }
                Event::Drawable(r, g, actual) => {
                    let Some(s) = self.arena.find(r) else {
                        // Uploaded although its delivery was withdrawn and
                        // its slot is gone: the GPU holds a copy nothing
                        // tracks; the step evicts it (a consumer a message
                        // late, the web's worker, uploads what it was handed
                        // before it reads the withdrawal).
                        st.stale += 1;
                        if self.cfg.policy == Policy::Interactive {
                            self.orphans.push(r);
                        }
                        continue;
                    };
                    // The epoch now, for a copy whose delivery is forgotten.
                    let epoch_now = self.epoch(r, self.arena.class(s));
                    let a = &mut self.arena;
                    // The cost it was delivered at, once drawable: what
                    // the compression reads of it (CB9).
                    let mut landed = None;
                    if a.transit(s).is_some_and(|t| t.gen == g) {
                        if let Some(t) = a.take_transit(s, &mut self.ledger) {
                            landed = Some(t.cost);
                            if t.cost.is_fallback(actual) {
                                // An encode that fell back to the form it
                                // crossed in: charged as it is held, and
                                // not the estimator's error.
                                st.fallbacks[a.class(s)] += 1;
                            } else if !within_drift(t.cost.gpu, actual) {
                                st.drift_exceeded[a.class(s)] += 1;
                            }
                            a.put_gpu(
                                s,
                                &mut self.ledger,
                                Gpu {
                                    gen: g,
                                    bytes: actual,
                                    epoch: t.epoch,
                                },
                            );
                        }
                        a.mark_delivered(s, g);
                    } else if !a.has_transit(s) && !a.has_gpu(s) {
                        // Uploaded although its delivery was withdrawn: the
                        // GPU holds it, so the ledger does — its decoded
                        // copy kept or not (released since the withdrawal,
                        // the copy on the GPU is the same content: of the
                        // epoch the copy had, or the epoch now).
                        let same = a.cpu(s).filter(|c| c.gen == g);
                        let epoch = same.map_or(epoch_now, |c| c.epoch);
                        landed = same.map(|c| c.cost);
                        a.put_gpu(
                            s,
                            &mut self.ledger,
                            Gpu {
                                gen: g,
                                bytes: actual,
                                epoch,
                            },
                        );
                    } else {
                        st.stale += 1;
                    }
                    if let Some(cost) = landed {
                        self.compress_admit(r, g, cost, actual);
                    }
                }
                Event::Compressed(r, g, actual) => {
                    self.compress_done(r, g, actual, st);
                }
                Event::Discarded(r, g) => {
                    let Some(s) = self.arena.find(r) else {
                        continue;
                    };
                    if self.arena.transit(s).is_some_and(|t| t.gen == g) {
                        self.arena.take_transit(s, &mut self.ledger);
                        let cold = self.arena.cold_mut(s);
                        cold.discards += 1;
                        cold.deliver_after =
                            now.plus(backoff(self.cfg.failure, cold.discards, Failure::Transient));
                    }
                }
            }
        }
        events.clear();
        self.events = events;
        for o in back {
            self.retry_origin(o, now);
        }
    }

    /// `o` answered after it was down: every resource of it backing off is
    /// due at `now` — wanted now or not yet (a drape whose tile's mesh is
    /// still on its way) — its work read again this step, and the prune
    /// free to let go of the ones the view left. Its failure count stays
    /// (the load timeout it earned, the next backoff). A walk of every
    /// slot, once per origin that comes back.
    fn retry_origin(&mut self, o: Origin, now: Micros) {
        let mut due = std::mem::take(&mut self.scratch.origin_due);
        due.clear();
        due.extend(self.arena.iter().filter(|&s| {
            self.arena.backing_off(s, now) && Origin::of(self.arena.id(s), self.arena.class(s)) == o
        }));
        self.walk(self.arena.len());
        for &s in &due {
            self.arena.cold_mut(s).retry_at = now;
            self.arena.note_work(s);
        }
        self.scratch.origin_due = due;
    }

    /// Whether `r`'s load waits for its origin, down and between two probes
    /// (`origin`). A resource whose stored copy expired is never held back:
    /// its load serves that copy when the origin cannot answer.
    fn origin_holds_back(&self, r: ResourceId, class: Class, now: Micros) -> bool {
        self.origins.any_down()
            && self.origins.blocks(Origin::of(r, class), now)
            && !self
                .arena
                .find(r)
                .is_some_and(|s| self.arena.cold(s).store_missed)
    }

    // 2. The need: the one kept, changed by what the step is told.
    fn read_need(&mut self, need: &mut NeedState, input: &FrameInput<'_>, st: &mut BudgetStats) {
        // The tree positions of the tiles that entered or changed; what a
        // tile that did not change told the budget is already known.
        let named = match input.need {
            NeedInput::Full(n) => need.apply_full(n, &mut self.tiles),
            NeedInput::Delta(d) => need.apply_delta(d, &mut self.tiles),
        };
        #[cfg(debug_assertions)]
        for (t, info) in need.positions() {
            assert_eq!(
                self.tiles.get(&t),
                Some(&info),
                "the tree position of {t:?} is the whole need's"
            );
        }
        st.rejected_inputs = st.rejected_inputs.saturating_add(sat_u32(need.rejected()));
        st.work = st.work.saturating_add(named);
    }

    /// Whether `tn` asks for drapes at its own levels this step, with
    /// imagery: drawn or blocking, or a preload of the interactive view
    /// ([`Self::preload`]): a preloaded mesh comes with its drape (E4,
    /// `docs/explorations/budget-regulator.md`). A preload's drapes are
    /// preloads too, of the tile's own urgency and score, at the level it
    /// needs once drawn, and they enter only the preload envelope
    /// (`regulator`): the chain's drapes asked at their places were a load
    /// per ancestor and per layer at the finest levels, still views that
    /// never converged (`budget_sim`), and before the envelope they were
    /// admitted to the load and refused at the delivery (#364).
    ///
    /// What a drawn tile wears while its own drape loads is asked of its
    /// parent at the present need's urgency, and only while a child needs
    /// it ([`Self::stand_ins_of`]).
    pub(super) fn draped(&self, tn: &TileNeed) -> bool {
        tn.need.imagery.is_some()
            && (matches!(tn.membership, Membership::Drawn | Membership::Blocker)
                || self.preload(tn))
    }

    /// Whether `tn` is a preload of the interactive view: a ring or
    /// ancestor tile above the floor. Its drape levels are the ones it
    /// needs once drawn
    /// ([`TexelFootprint::at_threshold`](super::imagery::TexelFootprint::at_threshold)),
    /// not the ones its place asks now (an ancestor under the eye would ask
    /// the finest its terrain allows). Batch preloads meshes only: a
    /// frame's chain holds no drape back.
    pub(super) fn preload(&self, tn: &TileNeed) -> bool {
        self.cfg.policy != Policy::Batch
            && matches!(tn.membership, Membership::Ring | Membership::Ancestor)
            && !self.is_floor(tn.need.class, tn.need.level)
    }

    /// The drapes ancestor `p` (`pn` its need) asks for as its drawn
    /// children's stand-in, into `out`: for each layer, its drape one level under the finest need of
    /// its drawn children that wear two levels or more under their need,
    /// or nothing — the pinned floor's at best, a flat colour over the
    /// ground, wherever the view turns faster than the fine drapes come.
    /// One parent's drape dresses its four children at half their
    /// resolution, a quarter of their loads. A child dressed at that level
    /// or finer (its own drape on arrival) asks nothing: a dressed view
    /// asks for no stand-in, and a coarse ancestor further up is never
    /// asked (the parent's drape is the one that comes as soon as the
    /// children's). Reads at most the four children's levels and items
    /// (the wants pass reads a drawn tile's parent again with it).
    pub(super) fn stand_ins_of(
        &self,
        p: TileId,
        pn: &TileNeed,
        tiles: &SortedMap<TileId, TileNeed>,
        out: &mut SmallVec<[ResourceId; 4]>,
    ) {
        out.clear();
        let Some(img) = pn.need.imagery.as_ref() else {
            return;
        };
        let Some(kids) = super::held::geometric_children(p) else {
            return;
        };
        let floor = self.cfg.pinned_level.unwrap_or(0);
        for reach in layers(img) {
            let mut finest: Option<u32> = None;
            for c in kids {
                self.walk(1);
                let drawn = tiles
                    .get(&c)
                    .is_some_and(|cn| cn.membership == Membership::Drawn);
                if !drawn || self.tiles.get(&c).and_then(|i| i.parent()) != Some(p) {
                    continue;
                }
                // Its level, chosen by the levels pass.
                let Some(n) = self.imagery_prev.get(&(c, reach.layer)).map(|x| x.level) else {
                    continue;
                };
                let Some(level) = n.checked_sub(1).filter(|l| *l >= floor) else {
                    continue;
                };
                let dressed = self
                    .down
                    .drawn(ResourceId::Mesh(c))
                    .and_then(|i| i.drape(reach.layer))
                    .is_some_and(|w| w.resource.layer() == Some(reach.layer) && w.level >= level);
                if !dressed {
                    finest = Some(finest.map_or(n, |f| f.max(n)));
                }
            }
            if let Some(n) = finest {
                out.push(ResourceId::drape(p, reach.layer, n - 1));
            }
        }
    }

    /// The imagery level of each layer tile `t` asks for this step, into
    /// `out` in layer order with the level the hysteresis carries (the
    /// footprint's, the same unless a stand-in is taken), a layer listed
    /// twice at its last listing's levels. `last` holds the levels kept
    /// from the step before: `imagery_prev`, and `imagery_stood_in` (the
    /// hysteresis' level where a stand-in was worn). `asked` is set when
    /// the presence index was
    /// asked: the levels then hold for this step only.
    ///
    /// The level follows the texel footprint (relaxed with the LOD, with
    /// hysteresis). Under a relaxation (`Budget::relaxing`), when that
    /// level is held nowhere locally, a level already held (GPU, decoded,
    /// store) is taken instead if it lies within one level of the window
    /// between the relaxed and the unrelaxed need: a relaxation never sends
    /// to the network what a local tier can show (I14, I15). With none in
    /// force the level is the need's own, fetched: a held neighbour of it
    /// dresses the tile meanwhile, as for any load, and never settles in
    /// its place — else a tile stood in for stayed a level coarser than its
    /// neighbours for good. Batch takes the exact level, a function of the
    /// frame.
    /// Returns the levels the terrain's level denies the tile's imagery
    /// ([`Self::capped_of`]), read in the same pass.
    pub(super) fn drape_levels(
        &self,
        t: TileId,
        tn: &TileNeed,
        presence: &dyn PresenceOracle,
        last: KeptLevels<'_>,
        out: &mut Vec<(LayerId, u32, u32, u32)>,
        asked: &mut bool,
    ) -> u32 {
        out.clear();
        let Some(img) = tn.need.imagery.as_ref() else {
            return 0;
        };
        let floor = self.cfg.pinned_level.unwrap_or(0);
        let batch = self.cfg.policy == Policy::Batch;
        let cap = self.cfg.imagery_cap();
        // A preload's footprint is the one it will have once drawn.
        let footprint = if self.preload(tn) {
            img.footprint.at_threshold(tn.q.sse_excess)
        } else {
            img.footprint
        };
        for reach in layers(img) {
            self.walk(1);
            let key = (t, reach.layer);
            let drape = |l| ResourceId::drape(t, reach.layer, l);
            let clamp = LevelClamp {
                floor,
                terrain_level: tn.need.level,
                over_terrain_cap: cap,
                source_max: reach.max_level,
            };
            if batch {
                let level = needed_level(None, footprint, clamp);
                out.push((reach.layer, level, level, level));
                continue;
            }
            // The hysteresis reads the footprint's last level, never a
            // stand-in worn in its place.
            let prev = last
                .1
                .get(&key)
                .copied()
                .or_else(|| last.0.get(&key).map(|p| p.level));
            let relaxed = needed_level(prev, footprint.relaxed(self.scale), clamp);
            // What the picture needs at scale 1, for the quality deficit.
            let full = needed_level(prev, footprint, clamp);
            let mut level = relaxed;
            let tier = |r: ResourceId, asked: &mut bool| {
                self.tier_asked(r, self.arena.find(r), Class::Drape, presence, asked)
            };
            if self.relaxing() && tier(drape(relaxed), asked) == Tier::Remote {
                let ceiling = tn.need.level.saturating_add(cap).min(reach.max_level);
                let lo = relaxed.min(full).saturating_sub(1).max(floor);
                let hi = relaxed.max(full).saturating_add(1).min(ceiling);
                let best = (lo..=hi)
                    .map(|l| {
                        self.walk(1);
                        let tier = tier(drape(l), asked);
                        ((tier, l.abs_diff(relaxed), Reverse(l)), l)
                    })
                    .min();
                if let Some(((tier, ..), l)) = best {
                    if tier != Tier::Remote {
                        level = l;
                    }
                }
            }
            out.push((reach.layer, level, relaxed, full));
        }
        // The layers in order, a layer listed twice in the order listed
        // (a stable sort); then the last listing's levels.
        out.sort_by_key(|e| e.0);
        out.dedup_by(|later, earlier| {
            let same = later.0 == earlier.0;
            if same {
                earlier.1 = later.1;
                earlier.2 = later.2;
                earlier.3 = later.3;
            }
            same
        });
        self.capped_of(tn)
    }

    /// The levels a step keeps for the next: the chosen ones
    /// (`imagery_prev`), and the hysteresis' own where a stand-in was worn
    /// (`imagery_stood_in`).
    pub(super) fn kept_levels(&self) -> KeptLevels<'_> {
        (&self.imagery_prev, &self.imagery_stood_in)
    }

    /// Whether a relaxation is in force: the LOD scale above one. A local
    /// stand-in is taken for a drape level under one only
    /// (`Budget::drape_levels`). A memory pressure relaxes through the
    /// scale (its targets shrink, the scale climbs); every level is read
    /// again when the scale moves, so the stand-ins follow it.
    pub(super) fn relaxing(&self) -> bool {
        self.scale > 1_000
    }

    /// Tile `t`'s drape needs this step — each layer it asks for, in layer
    /// order, at the level chosen — when it asks for drapes; into `out`.
    pub(super) fn drapes_of(&self, t: TileId, tn: &TileNeed, out: &mut Vec<(LayerId, u32)>) {
        out.clear();
        if !self.draped(tn) {
            return;
        }
        let Some(img) = tn.need.imagery.as_ref() else {
            return;
        };
        out.extend(layers(img).iter().map(|x| (x.layer, 0)));
        out.sort_unstable_by_key(|e| e.0);
        out.dedup_by_key(|e| e.0);
        for (layer, level) in out.iter_mut() {
            debug_assert!(
                self.imagery_prev.contains_key(&(t, *layer)),
                "a drape need's level is chosen before it is read"
            );
            *level = self.imagery_prev.get(&(t, *layer)).map_or(0, |p| p.level);
        }
    }

    /// The levels the terrain's level denies tile `tn`'s imagery, the most
    /// over its layers: how far the level the footprint alone asks (the
    /// source's deepest at most) lies above the tile's terrain level plus
    /// the imagery cap ([`DrapeChoice::capped`]). Relaxation and hysteresis
    /// play no part: it measures the ceiling, not the step.
    pub(super) fn capped_of(&self, tn: &TileNeed) -> u32 {
        let Some(img) = tn.need.imagery.as_ref() else {
            return 0;
        };
        let ideal = ideal_level(img.footprint);
        let ceiling = tn.need.level.saturating_add(self.cfg.imagery_cap());
        layers(img)
            .iter()
            .map(|r| ideal.min(r.max_level).saturating_sub(ceiling))
            .max()
            .unwrap_or(0)
    }

    /// Whether floor tile `t` is drawable with its floor drape of every
    /// layer it wears (P1.1).
    pub(super) fn floor_ready(&self, t: TileId, tn: &TileNeed) -> bool {
        self.drawable(ResourceId::Mesh(t))
            && tn
                .need
                .imagery
                .as_ref()
                .map(layers)
                .unwrap_or(&[])
                .iter()
                .all(|r| self.drawable(ResourceId::drape(t, r.layer, tn.need.level)))
    }

    /// Kept from eviction this step: drawn, or in a swap the view waits on.
    fn kept(&self, s: Slot) -> bool {
        self.arena.marked(s, Mark::Drawn) || self.arena.marked(s, Mark::Swap)
    }

    /// The drapes `mesh` wore last step.
    fn worn_last_of(&self, mesh: ResourceId) -> impl Iterator<Item = &DrapeChoice> + '_ {
        self.down.worn(mesh)
    }

    fn drawable(&self, r: ResourceId) -> bool {
        self.arena.find(r).is_some_and(|s| self.arena.has_gpu(s))
    }

    /// The pairs (ancestor, held tile) a held tile adds to the ancestry: for
    /// each ancestor its walk from the held tile reaches, the lowest level
    /// it passed before reaching it — `held` is a descendant of `of` exactly
    /// when that level is above `of`'s own (or `of`'s is unknown). At most
    /// 64 ancestors, the first visit of each (a cycle visits one twice).
    /// `info` gives each tile's tree position.
    pub(super) fn chain_of(
        h: TileId,
        info: impl Fn(TileId) -> Option<TileInfo>,
        out: &mut Vec<(TileId, u32)>,
    ) {
        out.clear();
        let mut t = h;
        let mut passed = u32::MAX;
        for _ in 0..64 {
            let Some(i) = info(t) else {
                break;
            };
            let Some(p) = i.parent() else {
                break;
            };
            if !out.iter().any(|x| x.0 == p) {
                out.push((p, passed));
            }
            passed = passed.min(i.level);
            t = p;
        }
    }

    /// The held tiles below `of`, in no particular order (`held`).
    fn held_below(&self, of: TileId) -> impl Iterator<Item = TileId> + '_ {
        let target = self.tiles.get(&of).map(|i| i.level);
        self.held_under(of, |t| self.tiles.get(&t).copied())
            .filter(move |(_, passed)| target.is_none_or(|l| *passed > l))
            .map(|(h, _)| h)
    }

    /// The tile's ancestors, nearest first, as far as the budget knows them:
    /// at most 64, lazily, so a caller that stops at the first hit walks no
    /// further.
    pub(super) fn ancestors(&self, t: TileId) -> impl Iterator<Item = TileId> + '_ {
        std::iter::successors(self.tiles.get(&t).and_then(|i| i.parent()), move |p| {
            self.tiles.get(p).and_then(|i| i.parent())
        })
        .take(64)
        .inspect(|_| self.walk(1))
    }

    /// The UV mapping of `tile`'s mesh into the drapes of `x`, the tile
    /// itself (`up == 0`) or one of its ancestors: `None` when either
    /// footprint is unknown.
    fn uv_into(&self, rect: Option<GeoRect>, up: usize, x: TileId) -> Option<UvTransform> {
        if up == 0 {
            return Some(UvTransform::IDENTITY);
        }
        match (rect, self.tiles.get(&x).and_then(|i| i.rect())) {
            (Some(a), Some(b)) => UvTransform::between(a, b),
            _ => None,
        }
    }

    /// The drawable drapes of `layer`'s family on `x`, with their levels
    /// and whether each is of the other layer of the family: `layer`'s own,
    /// then its sibling's ([`LayerId::sibling`]). A tile waiting for its
    /// substitute is dressed by the source it wore, or the other way round
    /// (never black ground); without a fallback rule no substitute drape
    /// exists and the sibling's range is empty.
    fn family_drapes(
        &self,
        x: TileId,
        layer: LayerId,
    ) -> impl Iterator<Item = (ResourceId, u32, bool)> + '_ {
        self.drawable_drapes(x, layer)
            .map(|(r, l)| (r, l, false))
            .chain(
                self.drawable_drapes(x, layer.sibling())
                    .map(|(r, l)| (r, l, true)),
            )
    }

    /// The drawable drapes of `layer` on `x`, with their levels.
    fn drawable_drapes(
        &self,
        x: TileId,
        layer: LayerId,
    ) -> impl Iterator<Item = (ResourceId, u32)> + '_ {
        // Only this layer's drapes: a layer never falls back on another's
        // but its family's (`family_drapes`).
        // Those on the GPU, by level: a range of the arena's ordered index.
        let lo = ResourceId::drape(x, layer, 0);
        let hi = ResourceId::drape(x, layer, u32::MAX);
        self.arena
            .gpu_drapes(lo..=hi)
            .inspect(|_| self.walk(1))
            .filter_map(|r| match r {
                ResourceId::Drape { level, .. } => Some((r, level)),
                ResourceId::Mesh(_) => None,
            })
    }

    /// The drape `tile`'s mesh wears for `layer`, needed at `need`; `reach`
    /// gets the tiles read beyond the tile's own drape at `need`.
    fn best_drape_for(
        &self,
        tile: TileId,
        layer: LayerId,
        need: u32,
        reach: &mut Reach,
    ) -> Option<DrapeChoice> {
        // The tile's own drape at exactly its need is the best there is:
        // the coarsest at or above the need, and the closest owner.
        let own = ResourceId::drape(tile, layer, need);
        if self.drawable(own) {
            return Some(DrapeChoice {
                resource: own,
                layer,
                level: need,
                need,
                capped: 0,
                uv: UvTransform::IDENTITY,
            });
        }
        let rect = self.tiles.get(&tile).and_then(|i| i.rect());
        // `best_drape`'s fold, walked lazily: the first of equal ranks wins,
        // and of equal ranks the wanted layer's before its sibling's.
        let mut best: Option<(DrapeCandidate, bool, ResourceId, UvTransform)> = None;
        for (up, x) in std::iter::once(tile)
            .chain(self.ancestors(tile))
            .enumerate()
        {
            reach.saw(x, PLACE | DRAPES);
            let Some(uv) = self.uv_into(rect, up, x) else {
                continue;
            };
            for (r, level, other) in self.family_drapes(x, layer) {
                let c = DrapeCandidate {
                    level,
                    up: sat_u32(up),
                };
                if best.is_none_or(|(b, bo, ..)| {
                    (drape_rank(need, c), other) < (drape_rank(need, b), bo)
                }) {
                    best = Some((c, other, r, uv));
                }
            }
            // A drape of the wanted layer at exactly the need: nothing
            // farther up ranks before it (the closer owner wins a tie), so
            // the walk ends — the same answer, fewer ancestors read.
            if best.is_some_and(|(b, other, ..)| b.level == need && !other) {
                break;
            }
        }
        best.map(|(c, _, resource, uv)| DrapeChoice {
            resource,
            layer,
            level: c.level,
            need,
            capped: 0,
            uv,
        })
    }

    /// Whether `tile` has any drawable drape of `layer`, on itself or an
    /// ancestor it maps into: `best_drape_for(tile, layer, _).is_some()`,
    /// stopping at the first found. `reach` gets the tile and the ancestors
    /// read when the tile's own drapes did not answer.
    fn has_drape(&self, tile: TileId, layer: LayerId, reach: &mut Reach) -> bool {
        // Its own first: they need no mapping.
        if self.family_drapes(tile, layer).next().is_some() {
            return true;
        }
        let rect = self.tiles.get(&tile).and_then(|i| i.rect());
        self.ancestors(tile).enumerate().any(|(up, x)| {
            reach.saw(x, PLACE | DRAPES);
            self.family_drapes(x, layer).next().is_some() && self.uv_into(rect, up + 1, x).is_some()
        })
    }

    /// Whether `tile`'s mesh can be drawn dressed for `layers`: each layer
    /// has a drawable drape on the tile or one of its ancestors, or — a
    /// layer switch in progress — the mesh still wears a layer it no longer
    /// needs. A mesh that cannot is not drawn: bare ground is black ground
    /// (I1, D-A1).
    fn dressable(&self, tile: TileId, layers: &[LayerId], reach: &mut Reach) -> bool {
        let missing = layers.iter().any(|l| !self.has_drape(tile, *l, reach));
        if !missing {
            return true;
        }
        // Last step's item of the tile's mesh, and the drapes it wore.
        reach.saw(tile, WORN);
        self.worn_last_of(ResourceId::Mesh(tile)).any(|d| {
            reach.saw(d.resource.tile(), DRAPES);
            !layers.contains(&d.layer) && self.drawable(d.resource)
        })
    }

    /// The meshes that stand for drawn tile `t`, at levels up to `cap`, into
    /// `out` as (mesh, role): the tile itself when drawable and dressed for
    /// the layers of `needs` (its drape needs, (layer, level)); else the
    /// finer tiles it held last step (zoom-out); else its nearest drawable,
    /// dressed ancestor. `reach` gets the tiles read beyond the tile's own
    /// mesh and drapes (the held tiles below it, read from the ancestry, are
    /// its own: a change to them reads it again). A tile drawn by nothing is
    /// lost only if it is ground (`class`, P1.1): where a 3D Tiles tile is
    /// not drawn yet, the ground under it is.
    ///
    /// Two rules of replacement (Laurent, 2026-11-24) bound "drawn as
    /// itself":
    ///
    /// - all or nothing on a refine: while its parent can stand in, a tile
    ///   is not drawn as itself unless every wanted sibling can be
    ///   ([`Self::siblings_drawable`]): the parent stands for all of them,
    ///   then gives way to all of them in one step — never two levels inside
    ///   one parent;
    /// - the fine until its replacement on a zoom-out: while finer tiles
    ///   are held under it, a tile replaces them only once it wears, for
    ///   every layer, a drape at its need ([`Self::replaces_held`]); until
    ///   then they stay, its nearest drawable ancestor standing for what
    ///   they do not cover.
    ///
    /// All or nothing applies to a refine only: a tile drawn last step is
    /// never taken back because a sibling entered the need (a view that
    /// widens or turns keeps what it drew).
    ///
    /// Neither costs ground: a tile with no parent to stand for it and its
    /// siblings is drawn as soon as it can be, and a tile with nothing held
    /// under it is drawn in whatever drape it has.
    // The selection's inputs, as each caller holds them: no struct of its
    // own for three callers.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn select_tile(
        &self,
        t: TileId,
        class: Class,
        needs: &[(LayerId, u32)],
        tiles: &SortedMap<TileId, TileNeed>,
        cap: Option<u32>,
        out: &mut Vec<(ResourceId, Role)>,
        reach: &mut Reach,
    ) -> TileSelection {
        out.clear();
        let mut sel = TileSelection::default();
        let own: SmallVec<[LayerId; 4]> = needs.iter().map(|x| x.0).collect();
        let shown = |x: TileId, reach: &mut Reach| self.shown(x, &own, cap, reach);
        // All or nothing applies to a refine: a tile drawn last step (as
        // itself or held) is never taken back for a sibling.
        let refining = !self.held_last.contains_key(&t);
        // Held back only where an ancestor can stand for what the held
        // tiles do not cover: never a hole.
        if shown(t, reach)
            && (!refining || self.siblings_drawable(t, &own, tiles, cap, reach))
            && (!self
                .held_below(t)
                .fold(false, |any, h| shown(h, reach) | any)
                || self.replaces_held(t, needs, reach)
                || !self.ancestors(t).any(|a| shown(a, reach)))
        {
            out.push((ResourceId::Mesh(t), Role::Exact));
            sel.exact = true;
            return sel;
        }
        let mut held = false;
        for h in self.held_below(t) {
            if shown(h, reach) {
                out.push((ResourceId::Mesh(h), Role::Held));
                held = true;
            }
        }
        sel.held = held;
        let anc = self.ancestors(t).find(|a| shown(*a, reach));
        if let Some(a) = anc {
            out.push((ResourceId::Mesh(a), Role::Fallback));
            let level = |x: TileId| self.tiles.get(&x).map_or(0, |i| i.level);
            sel.depth = level(t).saturating_sub(level(a));
            sel.fallback = !held;
        } else {
            sel.lost = !held && class.is_ground();
        }
        // By mesh: an entry once (a cycle could name a tile twice).
        out.sort_unstable();
        out.dedup();
        sel
    }

    /// Whether `x` can be drawn, up to level `cap`: its mesh drawable and
    /// dressed for `layers`. `reach` gets what it read.
    fn shown(&self, x: TileId, layers: &[LayerId], cap: Option<u32>, reach: &mut Reach) -> bool {
        reach.saw(x, PLACE | MESH | DRAPES);
        let level = self.tiles.get(&x).map_or(0, |i| i.level);
        cap.is_none_or(|c| level <= c)
            && self.drawable(ResourceId::Mesh(x))
            && self.dressable(x, layers, reach)
    }

    /// All or nothing on a refine: whether drawn tile `t` may be drawn as
    /// itself as far as its siblings tell — its parent cannot stand in
    /// (not drawn last step, not drawable and dressed for `layers`, or not
    /// the quadtree's), or
    /// every sibling the need wants drawn can be drawn too. A 3D Tiles
    /// tile, whose siblings no quadtree names, is not held back. `reach`
    /// gets the parent and the siblings read; a sibling's need is read
    /// again through the selection pass (`downstream`: the siblings of a
    /// tile named for its shape, place or membership are read again).
    fn siblings_drawable(
        &self,
        t: TileId,
        layers: &[LayerId],
        tiles: &SortedMap<TileId, TileNeed>,
        cap: Option<u32>,
        reach: &mut Reach,
    ) -> bool {
        let Some(p) = self.tiles.get(&t).and_then(|i| i.parent()) else {
            return true;
        };
        if super::held::geometric_parent(t) != Some(p) {
            return true;
        }
        let Some(kids) = super::held::geometric_children(p) else {
            return true;
        };
        // The parent stays (Laurent's words): it holds its children back
        // only while it is drawn — a parent no longer on screen is not
        // brought back for them.
        if !self.held_last.contains_key(&p) || !self.shown(p, layers, cap, reach) {
            return true;
        }
        kids.into_iter().filter(|s| *s != t).all(|s| {
            self.walk(1);
            let Some(tn) = tiles
                .get(&s)
                .filter(|tn| tn.membership == Membership::Drawn)
            else {
                return true;
            };
            if self.tiles.get(&s).and_then(|i| i.parent()) != Some(p) {
                return true;
            }
            let own: SmallVec<[LayerId; 4]> = if self.draped(tn) {
                tn.need
                    .imagery
                    .as_ref()
                    .map(layers_of_need)
                    .unwrap_or_default()
            } else {
                SmallVec::new()
            };
            self.shown(s, &own, cap, reach)
        })
    }

    /// The fine until its replacement: whether drawn tile `t` wears, for
    /// every layer of `needs`, a drawable drape at its need or finer — on
    /// itself or an ancestor, the drape `best_drape_for` would choose — so
    /// that it may replace the finer tiles held under it. `reach` gets the
    /// ancestors read.
    fn replaces_held(&self, t: TileId, needs: &[(LayerId, u32)], reach: &mut Reach) -> bool {
        needs.iter().all(|&(layer, n)| {
            self.best_drape_for(t, layer, n, reach)
                .is_some_and(|d| d.level >= n)
        })
    }

    /// The draw item of `mesh`, drawn for `run` — (tile served, role) by
    /// tile, at least one — into `out` (its buffers reused), with what it
    /// adds to the step's statistics. `drapes` gives a served tile's drape
    /// needs, and the levels the terrain denies its imagery
    /// ([`Self::capped_of`]), told to every drape chosen (the most over the
    /// tiles served). `reach` gets what it read beyond its own tile's drapes at the
    /// levels needed (an ancestor's drapes; its own last item, in a layer
    /// switch).
    pub(super) fn build_item(
        &self,
        mesh: ResourceId,
        run: &[(TileId, Role)],
        drapes: impl Fn(TileId, &mut Vec<(LayerId, u32)>) -> u32,
        reach: &mut Reach,
        bufs: &mut ItemBufs,
        out: &mut DrawItem,
    ) -> ItemStats {
        self.walk(run.len());
        let role = run.iter().map(|x| x.1).min().unwrap_or(Role::Exact);
        out.mesh = mesh;
        out.role = role;
        out.serves.clear();
        out.serves.extend(run.iter().map(|x| x.0));
        out.serves.dedup();
        let tile = mesh.tile();
        // The level each layer is needed at: the tile's own need when
        // exact; a held tile keeps its own last level; a fallback dresses
        // for the finest of the tiles it serves.
        let ItemBufs { needs, own } = bufs;
        needs.clear();
        let mut capped = 0;
        for s in &out.serves {
            capped = capped.max(drapes(*s, own));
            for (layer, n) in own.iter() {
                match needs.binary_search_by_key(layer, |x| x.0) {
                    Ok(i) => needs[i].1 = needs[i].1.max(*n),
                    Err(i) => needs.insert(i, (*layer, *n)),
                }
            }
        }
        if role == Role::Held {
            for (layer, n) in needs.iter_mut() {
                if let Some(p) = self.imagery_prev.get(&(tile, *layer)) {
                    *n = p.level;
                }
            }
        }
        let needed = |l: LayerId| needs.iter().any(|x| x.0 == l);
        let chosen = &mut out.drapes;
        chosen.clear();
        chosen.extend(
            needs
                .iter()
                .filter_map(|(layer, n)| self.best_drape_for(tile, *layer, *n, reach)),
        );
        if chosen.len() < needs.len() {
            // A layer switch in progress: a layer the tiles now ask for
            // is drawable nowhere yet, so the layers this mesh wore last
            // step and no longer needs stay worn until it is.
            reach.saw(tile, WORN);
            for old in self.worn_last_of(mesh) {
                reach.saw(old.resource.tile(), DRAPES);
                if !needed(old.layer) && self.drawable(old.resource) {
                    chosen.push(*old);
                }
            }
            // One drape per layer: an unstable sort is the stable one.
            chosen.sort_unstable_by_key(|d| d.layer);
        }
        for c in chosen.iter_mut() {
            c.capped = capped;
        }
        // Bare: a layer the tiles need is worn by no drape, and no old
        // layer stands in for it during a switch.
        let missing = needs
            .iter()
            .any(|(l, _)| !chosen.iter().any(|d| d.layer == *l));
        let switching = chosen.iter().any(|d| !needed(d.layer));
        let mut stats = ItemStats {
            bare: missing && !switching,
            underdraped: false,
            under_levels: 0,
            over_levels: 0,
            tile_level: self.tiles.get(&tile).map(|i| i.level).unwrap_or(0),
        };
        if role == Role::Exact {
            stats.underdraped = needs.iter().any(|(layer, n)| {
                !chosen
                    .iter()
                    .any(|d| d.layer == *layer && d.level.abs_diff(*n) <= 1)
            });
            // The deficit: each layer's worn level against the level the
            // step decided for it (what the decided level is short of the
            // unrelaxed need is counted apart, `imagery_short`). A layer
            // worn by nothing is bare, not a deficit.
            let (mut under, mut over) = (0u32, 0u32);
            for (layer, n) in needs.iter() {
                if let Some(d) = chosen.iter().find(|d| d.layer == *layer) {
                    under = under.saturating_add(n.saturating_sub(d.level));
                    over = over.saturating_add(d.level.saturating_sub(*n));
                }
            }
            stats.under_levels = u16::try_from(under).unwrap_or(u16::MAX);
            stats.over_levels = u16::try_from(over).unwrap_or(u16::MAX);
        }
        stats
    }

    /// What tile `t` puts in the wants, into `out` merged — per resource the
    /// most urgent, then the best scored; of equals, the first put — in
    /// resource order. `item` is the draw item of its mesh, `meshes_in`
    /// whether Batch may want its drapes (always under Interactive).
    /// Returns whether they read the pressure: a drape worn one level finer
    /// than its need is wanted coarser at once under pressure, after the
    /// deficit's dwell otherwise (colour consistency), and a change of
    /// pressure to or from `Normal` moves no other want.
    // The buffers a step reuses, and the re-read time it hands back: no
    // struct of its own for one caller and the debug check.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tile_wants(
        &self,
        t: TileId,
        tn: &TileNeed,
        tiles: &SortedMap<TileId, TileNeed>,
        item: Option<&Item>,
        meshes_in: bool,
        drapes: &mut Vec<(LayerId, u32)>,
        out: &mut Vec<(ResourceId, u32, Want)>,
        reread: &mut Option<Micros>,
    ) -> bool {
        out.clear();
        *reread = None;
        let mut reads_pressure = false;
        let batch = self.cfg.policy == Policy::Batch;
        // Every want put, in order; merged below.
        let put = |out: &mut Vec<(ResourceId, u32, Want)>, r: ResourceId, w: Want| {
            let seq = u32::try_from(out.len()).unwrap_or(u32::MAX);
            out.push((r, seq, w));
        };
        let floor = self.is_floor(tn.need.class, tn.need.level);
        let mesh_drawable = self.drawable(ResourceId::Mesh(t));
        let urgency = if floor {
            Urgency::Floor
        } else {
            match tn.membership {
                Membership::Drawn => Urgency::Hole,
                Membership::Blocker => Urgency::Blocker,
                Membership::Ring => Urgency::Ring,
                Membership::Ancestor => Urgency::Ancestor,
            }
        };
        put(
            out,
            ResourceId::Mesh(t),
            Want {
                urgency,
                q: tn.q,
                class: tn.need.class,
                level: tn.need.level,
            },
        );
        if floor {
            for reach in tn.need.imagery.as_ref().map(layers).unwrap_or(&[]) {
                put(
                    out,
                    ResourceId::drape(t, reach.layer, tn.need.level),
                    Want {
                        urgency: Urgency::Floor,
                        q: tn.q,
                        class: Class::Drape,
                        level: tn.need.level,
                    },
                );
            }
        }
        if meshes_in {
            self.drapes_of(t, tn, drapes);
            for &(layer, n) in drapes.iter() {
                self.walk(1);
                let drape = ResourceId::drape(t, layer, n);
                let du = match tn.membership {
                    Membership::Blocker => Some(Urgency::Blocker),
                    _ if !mesh_drawable => Some(urgency),
                    // Its mesh drawn only as a stand-in for other tiles
                    // while it is held back itself (the fine until its
                    // replacement): its own drape is wanted now.
                    Membership::Drawn if item.is_some_and(|i| i.role == Role::Fallback) => {
                        Some(Urgency::Drape)
                    }
                    // A ring tile not drawn (the neighbourhood, the views
                    // ahead): its drapes are preloads too, of the tile's own
                    // score — never the present need's.
                    Membership::Ring if item.is_none() => Some(urgency),
                    // A preload drawn for another tile (an ancestor standing
                    // in for its children): its own drapes stay preloads;
                    // what its children wear meanwhile is their stand-in,
                    // asked below at the present need's urgency.
                    _ if self.preload(tn) => Some(urgency),
                    _ => {
                        let worn = item.and_then(|i| i.drape(layer).copied());
                        let off = match worn {
                            None => true,
                            Some(w) if batch => w.resource != drape,
                            // A stand-in of the other layer of the family
                            // (a switch to or from a substitute): its own
                            // is wanted, whatever its level.
                            Some(w) if w.resource.layer() != Some(layer) => true,
                            Some(w) if w.level.abs_diff(n) > 1 => true,
                            Some(w) if w.level == n => false,
                            Some(w) => {
                                // A level under its need: wanted once the
                                // quality it brings over the time it is
                                // expected to serve pays for its restore
                                // cost (spec §5.3); until then, read again
                                // when it will. A level over it — a finer
                                // drape kept from a zoom-out — is another
                                // acquisition of the imagery than its
                                // neighbours' own levels, a patch of another
                                // colour: its own level is wanted the same
                                // way, the gap weighed less than a missing
                                // level's (`Params::finer_gap_permille`), and
                                // at once under pressure (coarser is lighter).
                                let finer = w.level > n;
                                reads_pressure |= finer;
                                if finer && self.pressure != Pressure::Normal {
                                    true
                                } else {
                                    let gap = w.level.abs_diff(n);
                                    let at = self.deficit_due(t, layer, tn, gap, finer);
                                    if let Some(at) = at.filter(|at| *at > self.now) {
                                        *reread = Some(reread.map_or(at, |r| r.min(at)));
                                    }
                                    at.is_some_and(|at| at <= self.now)
                                }
                            }
                        };
                        off.then_some(if floor {
                            Urgency::Floor
                        } else {
                            Urgency::Drape
                        })
                    }
                };
                if let Some(du) = du {
                    put(
                        out,
                        drape,
                        Want {
                            urgency: du,
                            q: tn.q,
                            class: Class::Drape,
                            level: tn.need.level,
                        },
                    );
                }
            }
        }
        // An ancestor's drape, as its drawn children's stand-in while their
        // own load: the present need's, as theirs (a child wearing the floor
        // is ground on screen), and never under Batch, which draws exact
        // levels only.
        if meshes_in && !batch && !floor && tn.membership == Membership::Ancestor {
            let mut stand_ins = SmallVec::new();
            self.stand_ins_of(t, tn, tiles, &mut stand_ins);
            for drape in stand_ins {
                put(
                    out,
                    drape,
                    Want {
                        urgency: Urgency::Drape,
                        q: tn.q,
                        class: Class::Drape,
                        level: tn.need.level,
                    },
                );
            }
        }
        // Per resource the most urgent, then the best scored; of equals,
        // the first put.
        out.sort_unstable_by_key(|(r, seq, w)| (*r, w.urgency, Reverse(w.q), *seq));
        out.dedup_by_key(|e| e.0);
        reads_pressure
    }

    /// [`super::value::deficit_due`] for tile `t`'s layer `layer`.
    pub(super) fn deficit_due(
        &self,
        t: TileId,
        layer: LayerId,
        tn: &TileNeed,
        gap: u32,
        finer: bool,
    ) -> Option<Micros> {
        let level = tn.need.level;
        let input = super::value::DeficitInput {
            tile: t,
            centrality: tn.q.centrality,
            gap,
            finer,
            since: self
                .level_since
                .get(&(t, layer))
                .copied()
                .unwrap_or(Micros::ZERO),
            facts: super::spec::Facts {
                class: Class::Drape,
                level,
                tier: Tier::Remote,
                band: self.spec.band(Class::Drape, level, self.cfg.pinned_level),
                membership: Some(tn.membership),
                predicted: false,
            },
        };
        super::value::deficit_due(&self.costed, &input)
    }

    /// Whether Batch may want drapes this step: every mesh the frame draws
    /// and waits on is loaded. Under Interactive, always.
    pub(super) fn meshes_in(&self, tiles: &SortedMap<TileId, TileNeed>) -> bool {
        if self.cfg.policy != Policy::Batch {
            return true;
        }
        tiles
            .iter()
            .filter(|(_, tn)| matches!(tn.membership, Membership::Drawn | Membership::Blocker))
            .inspect(|_| self.walk(1))
            .all(|(t, _)| {
                let a = &self.arena;
                a.find(ResourceId::Mesh(*t))
                    .is_some_and(|s| a.has_gpu(s) || a.has_cpu(s))
            })
    }

    /// The refresh `r`, drawn, asks for, if its GPU copy is of an older
    /// epoch.
    pub(super) fn refresh_of(&self, r: ResourceId) -> Option<Want> {
        let s = self.arena.find(r)?;
        let class = self.arena.class(s);
        self.arena.stale_gpu(s, self.epoch(r, class)).then(|| Want {
            urgency: Urgency::Refresh,
            q: Quantised::default(),
            class,
            level: self.arena.level(s),
        })
    }

    /// Whether some GPU copy is of an older epoch than its class's (and
    /// layer's): only then can a refresh be wanted.
    pub(super) fn any_stale(&self) -> bool {
        self.arena.gpu_epochs().any(|((class, layer, epoch), _)| {
            let now = self.epochs[class]
                + layer.map_or(0, |l| self.layer_epochs.get(&l).copied().unwrap_or(0));
            epoch < now
        })
    }

    /// What protects `r`, whose slot is `s`.
    fn protection_of(&self, r: ResourceId, s: Slot) -> Option<Protection> {
        let a = &self.arena;
        if self.is_floor(a.class(s), a.level(s)) && (a.has_gpu(s) || a.has_transit(s)) {
            return Some(Protection::Floor);
        }
        if a.marked(s, Mark::Drawn) {
            return Some(Protection::Drawn);
        }
        if a.has_transit(s) {
            return Some(Protection::InTransit);
        }
        if a.marked(s, Mark::Swap) {
            return Some(match r {
                ResourceId::Mesh(_) if a.marked(s, Mark::Chain) => Protection::Chain,
                _ => Protection::Swap,
            });
        }
        None
    }

    fn tier(
        &self,
        r: ResourceId,
        e: Option<Slot>,
        class: Class,
        presence: &dyn PresenceOracle,
    ) -> Tier {
        self.tier_asked(r, e, class, presence, &mut false)
    }

    /// `tier`, `asked` set when the presence index answered.
    fn tier_asked(
        &self,
        r: ResourceId,
        e: Option<Slot>,
        class: Class,
        presence: &dyn PresenceOracle,
        asked: &mut bool,
    ) -> Tier {
        let a = &self.arena;
        let epoch = self.epoch(r, class);
        match e {
            Some(s) if a.gpu(s).is_some_and(|g| g.epoch == epoch) => Tier::Gpu,
            Some(s) if a.fresh_cpu(s, epoch).is_some() => Tier::Decoded,
            // A wrong hint is believed once; otherwise the index is asked
            // afresh, since a store fills as the session runs.
            Some(s) if a.cold(s).store_missed => Tier::Remote,
            _ => {
                *asked = true;
                match presence.holds(r) {
                    Presence::Stored => Tier::Stored,
                    Presence::Absent => Tier::Remote,
                }
            }
        }
    }

    /// The work `r`, wanted as `w`, asks for now — a load or a delivery —
    /// and, asking for none, the time it waits on (a delivery's backoff or a
    /// retry's) that will change that. `e` is `r`'s slot, found by the
    /// caller: one that reads the slot again looks it up once.
    pub(super) fn job_of_at(
        &self,
        r: ResourceId,
        w: &Want,
        now: Micros,
        e: Option<Slot>,
    ) -> (Option<Job>, Option<Micros>) {
        let class = self.class_of(r, w.class);
        let epoch = self.epoch(r, class);
        let a = &self.arena;
        match e {
            Some(s) if a.gpu(s).is_some_and(|g| g.epoch == epoch) => (None, None),
            Some(s) if a.has_transit(s) => (None, None),
            Some(s) if a.fresh_cpu(s, epoch).is_some() => {
                let after = a.cold(s).deliver_after;
                if after <= now {
                    (Some(Job::Deliver), None)
                } else {
                    (None, Some(after))
                }
            }
            Some(s) if a.loading(s) => (None, None),
            Some(s) if a.backing_off(s, now) => (None, Some(a.cold(s).retry_at)),
            _ => (Some(Job::Load), None),
        }
    }

    /// The priority key of `r`, wanted as `w`, now: its urgency (a stale
    /// GPU copy's refresh), score and tier.
    fn key_of(
        &self,
        r: ResourceId,
        w: &Want,
        now: Micros,
        coarsening: &dyn Fn(TileId) -> bool,
        presence: &dyn PresenceOracle,
    ) -> PriorityKey {
        self.key_of_at(r, w, now, coarsening, presence, self.arena.find(r))
    }

    /// [`Budget::key_of`], `r`'s slot found already (`e`).
    fn key_of_at(
        &self,
        r: ResourceId,
        w: &Want,
        now: Micros,
        coarsening: &dyn Fn(TileId) -> bool,
        presence: &dyn PresenceOracle,
        e: Option<Slot>,
    ) -> PriorityKey {
        let class = self.class_of(r, w.class);
        let epoch = self.epoch(r, class);
        let a = &self.arena;
        let mut urgency = w.urgency;
        if e.is_some_and(|s| a.stale_gpu(s, epoch)) {
            urgency = Urgency::Refresh;
        }
        let waited = e
            .and_then(|s| a.cold(s).first_wanted())
            .map(|f| now.since(f))
            .unwrap_or(Micros::ZERO);
        // How many wanted tiles it stands in for, as a fallback.
        let cover = match self.down.drawn(r) {
            Some(i) if i.role == Role::Fallback => sat_u32(self.down.serves(r)),
            _ => 0,
        };
        let tier = self.tier(r, e, class, presence);
        let precedence = super::priority::Precedence::of(urgency);
        let sc = if precedence >= super::priority::Precedence::Value {
            // The one currency (spec §1b): what loading it gains, from its
            // arrival on.
            let p =
                super::value::probability(&self.spec, self.reach_tiles(), urgency, w.level, &w.q);
            let preload = urgency >= Urgency::Ring;
            let gain = super::value::load_gain(&self.costed, p, w.q.centrality, tier, preload);
            i64::try_from(gain.0).unwrap_or(i64::MAX)
        } else {
            let mut sc = score(w.q, waited, self.cfg.age_full, cover);
            if urgency == Urgency::Hole && coarsening(r.tile()) {
                // At the ceiling a coarser covering tile frees bytes: first.
                sc += 1_000_000;
            }
            sc
        };
        PriorityKey {
            precedence,
            urgency,
            score: Reverse(sc),
            tier,
            resource: r,
        }
    }

    /// The step, into `d` (emptied by the caller, its buffers kept).
    pub(super) fn step(&mut self, input: FrameInput<'_>, d: &mut Decisions) {
        let now = input.now;
        self.take_inputs(now);
        let mut st = std::mem::take(&mut self.pending);
        let mut eff = self.eff();
        self.scratch.room = None;
        let mut ranked = std::mem::take(&mut self.ranked_last);
        self.walk(ranked.len());
        for r in &ranked {
            if let Some(s) = self.arena.find(*r) {
                self.arena.set_rank(s, None, None);
            }
        }
        ranked.clear();
        self.ranked_last = ranked;
        self.steps += 1;
        // A step is a camera pose: the sweep's horizon counts them.
        self.arena
            .set_pose(self.steps, self.spec.params.sweep_keep_poses);

        let arrivals = self.events.len();
        self.apply_events(now, &mut st);
        // What the GPU holds untracked goes first; nothing of it is
        // delivered this step (`deliver_one`).
        self.orphans_now.clear();
        for r in std::mem::take(&mut self.orphans) {
            let tracked = self
                .arena
                .find(r)
                .is_some_and(|s| self.arena.has_gpu(s) || self.arena.has_transit(s));
            if !tracked && !self.orphans_now.contains(&r) {
                d.evict.push(r);
                self.orphans_now.push(r);
            }
        }
        let mut need = std::mem::take(&mut self.need);
        self.read_need(&mut need, &input, &mut st);
        let tiles = need.tiles();
        let named = need.named();
        let full = self.cfg.policy == Policy::Batch;

        // 2–3. Drape levels, the floor, the selection, the draw set, the
        // wants: what may have changed read again (`downstream`).
        let taken = self.downstream(tiles, named, input.presence, &mut st);
        let wants = taken.wants();
        let coarsening = taken.coarsening();

        // 4. Mark: needed now, or unneeded since when; the swap and chain
        // sets; the eviction order's newest group.
        self.mark(now, tiles, named, wants);

        // 5. Cancellations: grace and timeouts; then what an epoch left
        // behind.
        self.cancel(now, &eff, d, &mut st);
        if !eff.batch {
            self.recycle(now, d, &mut st);
        }

        // 6. Rank the work: the wanted resources with a load or a delivery
        // to start, kept up to date.
        // A time gone back reads every want's work again: a wait that ended
        // may have begun again.
        let back = self.last_now.is_some_and(|l| now < l);
        self.update_jobs(now, wants, full || taken.full || back);
        let mut jobs = std::mem::take(&mut self.scratch.jobs);
        jobs.clear();
        let is_coarsening = |t: TileId| coarsening.contains_key(&t);
        for r in self.down.jobs() {
            let Some(w) = wants.get(&r) else {
                continue;
            };
            // The slot looked up once for both.
            let e = self.arena.find(r);
            let (Some(job), _) = self.job_of_at(r, w, now, e) else {
                continue;
            };
            let key = self.key_of_at(r, w, now, &is_coarsening, input.presence, e);
            jobs.push((
                key,
                job,
                Want {
                    urgency: key.urgency,
                    ..*w
                },
            ));
        }
        // Keys end in the resource: total, so unstable is stable.
        jobs.sort_unstable_by_key(|j| j.0);
        self.walk(jobs.len());
        self.ranked_last.extend(jobs.iter().map(|j| j.0.resource));
        // A burst of work does not keep its room once it is done.
        let used = self.ranked_last.len();
        super::sorted::trim(&mut self.ranked_last, used);
        for (k, _, w) in &jobs {
            if let Some(s) = self.arena.find(k.resource) {
                self.arena.set_rank(s, Some(w.urgency), Some(Rank::of(k)));
            }
        }
        // The key of a wanted resource, for those in flight or in transit.
        let key = |b: &Budget, r: ResourceId| {
            wants
                .get(&r)
                .map(|w| b.key_of(r, w, now, &is_coarsening, input.presence))
        };

        // The envelope (E2): what the present need has pending, reserved.
        if !eff.batch {
            self.envelope_step(&mut eff, &jobs, wants, &mut st);
        }

        let mut picked = std::mem::take(&mut self.scratch.picked);
        picked.clear();
        // Places: a `u32` each, as many as the jobs.
        let place = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
        // A load whose origin is down waits for its probe, out of the
        // heap, so that it holds no place work of another origin could take.
        picked.extend(
            jobs.iter()
                .enumerate()
                .filter(|(_, j)| {
                    j.1 == Job::Load
                        && !(j.0.tier != Tier::Stored
                            && self.origin_holds_back(
                                j.0.resource,
                                self.class_of(j.0.resource, j.2.class),
                                now,
                            ))
                })
                .take(self.cfg.request_heap as usize)
                .map(|(i, _)| place(i)),
        );
        st.heap_len = sat_u32(picked.len());
        self.walk(picked.len());
        self.start(
            now,
            &eff,
            &jobs,
            &picked,
            &key,
            wants,
            input.presence,
            tiles,
            d,
            &mut st,
        );
        // The loads ranked and not started: the queue, by level.
        for (k, job, w) in &jobs {
            let loading = self
                .arena
                .find(k.resource)
                .is_some_and(|s| self.arena.loading(s));
            if *job == Job::Load && !loading {
                st.levels.queued[w.class][level_index(w.level)][prefetch_index(Some(w.urgency))] +=
                    1;
            }
        }

        // 7. Deliveries.
        picked.clear();
        picked.extend(
            jobs.iter()
                .enumerate()
                .filter(|(_, j)| j.1 == Job::Deliver)
                .map(|(i, _)| place(i)),
        );
        self.walk(jobs.len() + picked.len());
        let (blocked, strained) = self.deliver(
            now, &eff, &jobs, &picked, tiles, coarsening, wants, d, &mut st,
        );

        // 8. The upload plan (`picked` lent for its order by group).
        self.scratch.picked = picked;
        self.plan(&eff, &key, tiles, &mut d.upload_plan);
        picked = std::mem::take(&mut self.scratch.picked);
        st.work = st.work.saturating_add(sat_u32(self.arena.transits_len()));

        // 9. Compression before eviction (CB9, law 4), then eviction and
        // release. Both policies: in a film a decision is a frame (law 2);
        // nothing runs without a compressible copy.
        {
            let (pressed, hard) = self.compress_limits(&eff);
            // The compressions in flight: the specification's tiles, scaled
            // with the thermal state as the encode lane is (CB9, law 6).
            let tiles = u64::from(self.costed.params.compress.flight_tiles);
            let flight = tiles.saturating_mul(super::cost::chain_blocks(256, 256));
            let lane = if eff.encode == u64::MAX || self.cfg.encode_blocks_per_step == 0 {
                flight
            } else {
                flight.saturating_mul(eff.encode) / self.cfg.encode_blocks_per_step
            };
            // A step whose need changed nothing: the view at rest (law 2).
            let still = named.is_empty();
            self.compress_pass(still, lane, &hard, &pressed, d, &mut st);
        }
        if !eff.batch {
            self.evict(now, &eff, wants, d, &mut st);
        }

        // 10. Relaxation.
        let protected = self.protected_bytes();
        self.relax(now, &eff, &protected, blocked, strained);
        let thermal_floor = match (eff.batch, self.thermal) {
            (true, _) => 1_000,
            (_, Thermal::Serious) => 1_500,
            (_, Thermal::Critical) => 2_000,
            _ => 1_000,
        };
        d.sse_scale_milli = self.scale.max(thermal_floor);
        self.measure_uploads(now);
        st.envelope = eff.env;
        st.envelope_slots = eff.env_slots;
        self.last_now = Some(now);

        // Batch completeness. What the view waits on: work ranked, or in
        // flight or in transit — since the ranking, as nothing in flight
        // for the view lands or is cancelled in between, and what the step
        // started or delivered was ranked.
        self.walk(jobs.len());
        let pending_view =
            jobs.iter().any(|j| j.2.urgency <= Urgency::Drape) || self.down.view_waits();
        if eff.batch {
            self.walk(tiles.len());
            let complete = need.blockers() == 0
                && tiles
                    .iter()
                    .filter(|(_, n)| n.membership == Membership::Drawn)
                    .all(|(t, tn)| {
                        let item = self.down.drawn(ResourceId::Mesh(*t));
                        let exact = item.is_some_and(|i| i.role == Role::Exact);
                        let mut drapes = Vec::new();
                        self.drapes_of(*t, tn, &mut drapes);
                        let drape_ok = drapes.iter().all(|&(l, n)| {
                            item.and_then(|i| i.drape(l))
                                .is_some_and(|x| x.resource == ResourceId::drape(*t, l, n))
                        });
                        exact && drape_ok
                    });
            d.frame_ready = complete && self.fatal.is_none();
        }
        d.fatal = self.fatal.clone();
        st.converged = if eff.batch {
            d.frame_ready
        } else {
            st.lost == 0 && st.fallback == 0 && st.held == 0 && st.underdraped == 0 && !pending_view
        };

        // Bookkeeping for the next step: the held set, then the prune.
        self.end_step(tiles);
        let pruned = self.prune(now, tiles, named, wants);
        st.work = st.work.saturating_add(pruned);
        #[cfg(debug_assertions)]
        self.check_marks(wants);
        jobs.clear();
        self.scratch.jobs = jobs;
        picked.clear();
        self.scratch.picked = picked;
        self.down.draw_set_into(&mut d.draw);
        // A step that changed nothing the walk reads: no need named, no
        // arrival, no decision, the same scale.
        let quiet = named.is_empty()
            && arrivals == 0
            && d.start.is_empty()
            && d.deliver.is_empty()
            && d.evict.is_empty()
            && d.release.is_empty()
            && d.cancel.is_empty()
            && d.upload_plan.items.is_empty()
            && d.sse_scale_milli == self.scratch.walked_scale
            && self.scratch.walked_counts == Some(walk_counts(&st));
        self.scratch.walked_scale = d.sse_scale_milli;
        self.scratch.walked_counts = Some(walk_counts(&st));
        self.level_stats(tiles, quiet, d, wants, now, &mut st);
        self.put_back(taken);
        self.need = need;

        // Statistics.
        st.gpu = self.ledger.gpu;
        st.drawable_by_level = *self.arena.gpu_by_level();
        st.target = eff.target;
        st.overflow = eff.overflow;
        st.hard_limit = eff.hard;
        if let Some(p) = eff.pool {
            // The pool's shares as the step leaves them.
            for c in Class::ALL.into_iter().filter(|c| p.members[*c]) {
                let t = p.target(c, &self.ledger.gpu);
                st.target[c] = t;
                st.overflow[c] = p.overflow(c, t);
            }
        }
        st.decoded = self.ledger.decoded;
        st.in_transit = self.ledger.in_transit;
        st.sse_scale_milli = d.sse_scale_milli;
        st.in_flight = self.in_flight;
        st.local_reads = self.local_reads;
        st.tracked = sat_u32(self.arena.len());
        self.walk(self.arena.transits_len());
        for r in self.arena.transits() {
            if let Some(s) = self.arena.find(r) {
                if let Some(t) = self.arena.transit(s) {
                    let c = self.arena.class(s);
                    st.transit[c] = st.transit[c].plus(t.held());
                    st.transient[c] = st.transient[c].plus(t.cost.transient);
                    st.transfer[c] = st.transfer[c].plus(t.cost.transfer);
                    let at = super::stats::level_index(self.arena.level(s));
                    st.transit_by_level[c][at] = st.transit_by_level[c][at].plus(t.held());
                }
            }
        }
        st.protected = protected;
        // What keeping the kept maps in order cost, this step and the
        // events applied since the last.
        self.walk_upkeep();
        st.work = st.work.saturating_add(self.scratch.walked.swap(0, Relaxed));
        self.score_stats(&d.scores, &mut st);
        d.stats = st;
    }

    /// The step's [`LevelStats`] and [`super::stats::LayerStats`] that are
    /// read off its results: the copies held (kept with the ledger), the
    /// loads in flight, and — walked over the selection, the draw set and
    /// the GPU drapes — what is drawn and selected, what is under its need,
    /// and the layers' copies and drapes worn. The walk is taken again only
    /// when the step may have changed it (`quiet` false): a still view's
    /// step reads the last one's. Statistics only: not the step's work.
    fn level_stats(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        quiet: bool,
        d: &Decisions,
        wants: Wants<'_>,
        now: Micros,
        st: &mut BudgetStats,
    ) {
        if !(quiet && self.scratch.walked_levels.is_some()) {
            let walked = self.walk_levels(tiles, d, wants, now);
            self.scratch.walked_levels = Some(walked);
        }
        let Some(w) = self.scratch.walked_levels.as_ref() else {
            return;
        };
        let a = &self.arena;
        let l = &mut st.levels;
        l.gpu_tiles = *a.gpu_tiles_by_level();
        let (bytes, count) = a.cpu_by_level();
        l.decoded = *bytes;
        l.decoded_tiles = *count;
        l.selected = w.selected;
        l.drawn = w.drawn;
        l.under_need = w.under_need;
        l.blocked = w.blocked;
        for c in Class::ALL {
            for (lv, since) in w.blocked_since[c].iter().enumerate() {
                for (k, at) in since.iter().enumerate() {
                    l.blocked_oldest_ms[c][lv][k] = if w.blocked[c][lv][k] > 0 {
                        u32::try_from(now.since(*at).0 / 1_000).unwrap_or(u32::MAX)
                    } else {
                        0
                    };
                }
            }
        }
        let y = &mut st.layers;
        y.drawn = w.layer_drawn;
        let (layer_tiles, layer_bytes) = a.gpu_by_layer();
        y.gpu_tiles = *layer_tiles;
        y.gpu_bytes = *layer_bytes;
        for &s in a.active() {
            if a.loading(s) {
                l.in_flight[a.class(s)][level_index(a.level(s))][prefetch_index(a.urgency(s))] += 1;
                if let Some(x) = a.id(s).layer() {
                    y.in_flight[layer_index(x)] += 1;
                }
            }
        }
    }

    /// Why the tile waiting on `r` is not drawn at its need
    /// ([`crate::blocked`]): read off `r`'s state as the step leaves it.
    /// `mesh_class` is a mesh's class; `by_one`, a drape worn one level
    /// under the level wanted, which waits for its deficit's dwell before
    /// it is wanted (`tile_wants`).
    fn blocked_of(
        &self,
        r: ResourceId,
        mesh_class: Class,
        wants: Wants<'_>,
        now: Micros,
        by_one: bool,
    ) -> Blocked {
        let a = &self.arena;
        let class = self.class_of(r, mesh_class);
        let epoch = self.epoch(r, class);
        if let Some(s) = a.find(r) {
            if let Load::Loading { source, .. } = a.load(s) {
                return match source {
                    Source::Local => Blocked::FetchingStore,
                    Source::Network => Blocked::FetchingNetwork,
                };
            }
            if a.has_transit(s) {
                return Blocked::Uploading;
            }
            if a.gpu(s).is_some_and(|g| g.epoch == epoch) {
                return Blocked::Swap;
            }
            if a.fresh_cpu(s, epoch).is_some() {
                return Blocked::Decoded;
            }
            if a.backing_off(s, now) {
                return Blocked::Backoff;
            }
        }
        if wants.get(&r).is_none() {
            return if by_one {
                Blocked::Deferred
            } else {
                Blocked::NotRequested
            };
        }
        if self.origin_holds_back(r, class, now) {
            return Blocked::OriginDown;
        }
        Blocked::Queued
    }

    /// The walked part of [`Self::level_stats`].
    fn walk_levels(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed>,
        d: &Decisions,
        wants: Wants<'_>,
        now: Micros,
    ) -> WalkedLevels {
        let mut chosen = std::mem::take(&mut self.scratch.chosen);
        let since = std::mem::take(&mut self.scratch.blocked_since);
        let mut next = std::mem::take(&mut self.scratch.blocked_next);
        next.clear();
        let mut w = WalkedLevels::default();
        let a = &self.arena;
        for (t, n) in tiles.iter() {
            if n.membership == Membership::Drawn {
                w.selected[n.need.class][level_index(n.need.level)] += 1;
                // The drape level the selection chose for each layer.
                self.drapes_of(*t, n, &mut chosen);
                for (_, level) in &chosen {
                    w.selected[Class::Drape][level_index(*level)] += 1;
                }
            }
        }
        for item in &d.draw.items {
            if let Some(s) = a.find(item.mesh) {
                w.drawn[a.class(s)][level_index(a.level(s))] += 1;
            }
            for x in &item.drapes {
                w.drawn[Class::Drape][level_index(x.level)] += 1;
                w.layer_drawn[layer_index(x.layer)] += 1;
            }
            if item.role == Role::Fallback {
                for t in item.serves.iter() {
                    if let Some(n) = tiles.get(t) {
                        let at = level_index(n.need.level);
                        w.under_need[n.need.class][at] += 1;
                        let r = ResourceId::Mesh(*t);
                        let why = self.blocked_of(r, n.need.class, wants, now, false);
                        note_blocked(&mut w, &since, &mut next, r, n.need.class, at, why, now);
                    }
                }
            }
        }
        for (k, i) in self.down.items() {
            let stats = i.item_stats();
            if i.role == Role::Exact && stats.under_levels > 0 {
                let at = level_index(stats.tile_level);
                w.under_need[Class::Drape][at] += 1;
                // The drape it waits on: its first layer worn under the
                // level decided for it, at that level.
                let t = k.tile();
                if let Some(tn) = tiles.get(&t) {
                    self.drapes_of(t, tn, &mut chosen);
                }
                let short = chosen.iter().find_map(|&(layer, n)| {
                    let worn = i.drape(layer).map(|x| x.level);
                    worn.is_none_or(|l| l < n).then_some((layer, n, worn))
                });
                let (r, by_one) = match short {
                    Some((layer, n, worn)) => (
                        ResourceId::drape(t, layer, n),
                        worn.is_some_and(|l| l.saturating_add(1) == n),
                    ),
                    None => (*k, false),
                };
                let why = self.blocked_of(r, Class::Drape, wants, now, by_one);
                note_blocked(&mut w, &since, &mut next, r, Class::Drape, at, why, now);
            }
        }
        next.sort_unstable_by_key(|e| e.0);
        next.dedup_by_key(|e| e.0);
        self.scratch.blocked_since = next;
        self.scratch.blocked_next = since;
        // The layers' GPU copies: kept by the arena (`gpu_by_layer`).

        self.scratch.chosen = chosen;
        w
    }

    /// What a new epoch left of the old content, let go: the drape loads
    /// still in flight for it cancelled, its decoded copies released, and its GPU
    /// copies evicted — but those drawn or in a swap this step, which stay
    /// worn until their refresh is drawable (new before old), and those of
    /// the floor, which are wanted and refreshed in place. Looks at every
    /// slot on the step after an epoch moved, then, while some GPU copy is
    /// still of an older epoch, at those the step no longer keeps.
    fn recycle(&mut self, now: Micros, d: &mut Decisions, st: &mut BudgetStats) {
        let first = std::mem::take(&mut self.recycle);
        if !first && !self.any_stale() {
            return;
        }
        let a = &self.arena;
        let mut old: Vec<(ResourceId, Slot)> = a
            .iter()
            .filter(|&s| {
                let epoch = self.epoch(a.id(s), a.class(s));
                let loading = matches!(a.load(s), Load::Loading { epoch: e, .. } if e < epoch)
                    && a.class(s) == Class::Drape;
                let cpu = a.cpu(s).is_some_and(|c| c.epoch < epoch) && !a.has_transit(s);
                (first && (loading || cpu)) || a.stale_gpu(s, epoch)
            })
            .map(|s| (a.id(s), s))
            .collect();
        self.walk(self.arena.len());
        // By resource: the order the consumer is told in.
        old.sort_unstable();
        for (r, s) in old {
            let class = self.arena.class(s);
            let epoch = self.epoch(r, class);
            if let Load::Loading {
                gen,
                source,
                epoch: e,
                ..
            } = self.arena.load(s)
            {
                // A drape's only: what a mesh load answers across a mesh
                // epoch is refreshed like any copy, and its slot is kept.
                if e < epoch && class == Class::Drape {
                    self.arena.set_load(s, Load::Idle);
                    match source {
                        Source::Network => {
                            release(&mut self.in_flight[class], &mut self.underflows)
                        }
                        Source::Local => release(&mut self.local_reads, &mut self.underflows),
                    }
                    d.cancel.push((r, gen));
                    st.cancelled += 1;
                    st.recycled[class][0] += 1;
                    st.wasted_starts[class] += 1;
                }
            }
            let keep = self.kept(s) || self.is_floor(class, self.arena.level(s));
            if self.arena.stale_gpu(s, epoch) && !self.arena.has_transit(s) && !keep {
                let r = self.arena.id(s);
                d.scores
                    .push(self.held_event(s, Side::Evict, Reason::Recycled));
                self.arena.take_gpu(s, &mut self.ledger);
                d.evict.push(r);
                st.evicted += 1;
                st.recycled[class][1] += 1;
                st.evictions_by_level[class][super::stats::level_index(self.arena.level(s))] += 1;
                self.evicted(r, now);
            }
            if self.arena.cpu(s).is_some_and(|c| c.epoch < epoch) && !self.arena.has_transit(s) {
                self.release_one(s, d, st);
                st.recycled[class][2] += 1;
            }
        }
    }

    /// What the compression pass weighs per class (CB9): whether the class
    /// is over its low watermark — where a sweep or a delivery's room
    /// starts evicting — once the compressions in flight are acknowledged,
    /// or the platform signals pressure: then compressing comes before
    /// evicting (law 4). And its hard limit, which no compression's
    /// transient crosses.
    fn compress_limits(&self, eff: &Eff) -> (PerClass<bool>, PerClass<Bytes>) {
        // Where the release starts (E3): the target less the reserve.
        let settled = self.settled();
        let signalled = self.pressure != Pressure::Normal;
        let pool_over = eff.pool.is_some_and(|p| {
            let used = Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |a, c| a.plus(settled.gpu[*c]));
            used > p.target.minus(eff.pool_env(&p))
        });
        let pressed = PerClass::from_fn(|c| {
            signalled
                || match eff.pool {
                    Some(p) if p.members[c] => {
                        pool_over || settled.gpu[c] > p.ceil_target[c].minus(eff.env[c])
                    }
                    _ => eff.target[c] != Bytes::MAX && settled.gpu[c] > eff.line(c),
                }
        });
        let hard = PerClass::from_fn(|c| match eff.pool {
            Some(p) if p.members[c] => {
                let target = p.target(c, &self.ledger.gpu);
                p.hard(c, &self.ledger.gpu, target)
            }
            _ => eff.hard[c],
        });
        (pressed, hard)
    }

    fn protected_bytes(&self) -> PerClass<Bytes> {
        self.arena.protected()
    }

    /// How long a load of `failures` timed-out attempts before may take: the
    /// configured timeout, doubled for each attempt it already lost to the
    /// timeout, up to eight times. An origin slower than the timeout is
    /// otherwise cancelled at the same instant on every retry, and what it
    /// would have served never lands: the drape loads stay in flight, cancelled
    /// and restarted, and the ground is never dressed. Doubling bounds the
    /// waste — the same attempt restarted at most three times before it is
    /// given eight times as long — and leaves a healthy origin's timeout as
    /// it was (no failure, no change).
    fn load_timeout_after(timeout: Micros, failures: u32) -> Micros {
        Micros(timeout.0.saturating_mul(1u64 << failures.min(3)))
    }

    fn cancel(&mut self, now: Micros, eff: &Eff, d: &mut Decisions, st: &mut BudgetStats) {
        let grace = self.cfg.cancel_grace;
        let timeout = self.cfg.load_timeout;
        let batch = eff.batch;
        let failure = self.cfg.failure;
        let mut fatal = None;
        // Only a load or transit whose grace or timeout ran out since the
        // last step can be cancelled: those whose deadline passed. A time
        // gone back looks at every one, as before O1.4.
        let mut due = std::mem::take(&mut self.scratch.due);
        due.clear();
        let back = self.last_now.is_some_and(|l| now < l);
        if !back && self.down.due(now, &mut due) {
            due.sort_unstable();
            due.dedup();
            due.retain(|r| {
                self.arena
                    .find(*r)
                    .is_some_and(|s| self.arena.marked(s, Mark::Active))
            });
            if self.down.deadlines_len() > 2 * self.arena.active().len() + 64 {
                // Mostly deadlines of loads gone: those of the loads still
                // going, again.
                self.down.clear_deadlines();
                for &s in self.arena.active() {
                    let r = self.arena.id(s);
                    if let Load::Loading { started, .. } = self.arena.load(s) {
                        let t = Self::load_timeout_after(timeout, self.arena.cold(s).failures);
                        self.down.deadline(started.plus(t), r);
                    }
                    if let Some(u) = self.arena.cold(s).unneeded_since() {
                        self.down.deadline(u.plus(grace), r);
                    }
                }
                self.down.trim_deadlines();
            }
        } else {
            // Every load and transit, in resource order (the index is by
            // slot).
            due.clear();
            due.extend(self.arena.active().iter().map(|s| self.arena.id(*s)));
            due.sort_unstable();
            if !back {
                // The deadlines were dropped: every load's, again.
                for r in &due {
                    let Some(s) = self.arena.find(*r) else {
                        continue;
                    };
                    if let Load::Loading { started, .. } = self.arena.load(s) {
                        let t = Self::load_timeout_after(timeout, self.arena.cold(s).failures);
                        self.down.deadline(started.plus(t), *r);
                    }
                    if let Some(u) = self.arena.cold(s).unneeded_since() {
                        self.down.deadline(u.plus(grace), *r);
                    }
                }
            }
        }
        self.walk(due.len());
        for r in &due {
            let Some(s) = self.arena.find(*r) else {
                continue;
            };
            let class = self.arena.class(s);
            if let Load::Loading {
                gen,
                source,
                started,
                ..
            } = self.arena.load(s)
            {
                let unneeded = self
                    .arena
                    .cold(s)
                    .unneeded_since()
                    .is_some_and(|u| now.since(u) >= grace);
                let timed_out = now.since(started)
                    >= Self::load_timeout_after(timeout, self.arena.cold(s).failures);
                if (unneeded && !batch) || timed_out {
                    self.arena.set_load(s, Load::Idle);
                    match source {
                        Source::Network => {
                            release(&mut self.in_flight[class], &mut self.underflows)
                        }
                        Source::Local => release(&mut self.local_reads, &mut self.underflows),
                    }
                    d.cancel.push((*r, gen));
                    st.cancelled += 1;
                    if unneeded && !batch {
                        st.cancels[CancelReason::Grace as usize] += 1;
                        st.wasted_starts[class] += 1;
                        st.levels.wasted[class][level_index(self.arena.level(s))] += 1;
                    } else {
                        st.cancels[CancelReason::Timeout as usize] += 1;
                    }
                    if timed_out && !unneeded {
                        if batch {
                            fatal.get_or_insert(SessionError::LoadFailed(*r));
                        } else {
                            let cold = self.arena.cold_mut(s);
                            cold.failures += 1;
                            cold.retry_at =
                                now.plus(backoff(failure, cold.failures, Failure::Transient));
                        }
                    }
                }
            }
            if !batch {
                if let Some(t) = self.arena.transit(s) {
                    if self
                        .arena
                        .cold(s)
                        .unneeded_since()
                        .is_some_and(|u| now.since(u) >= grace)
                        && !self.arena.marked(s, Mark::Drawn)
                    {
                        self.arena.take_transit(s, &mut self.ledger);
                        d.cancel.push((*r, t.gen));
                        st.cancelled += 1;
                        st.cancels[CancelReason::Grace as usize] += 1;
                    }
                }
            }
        }
        if let Some(f) = fatal {
            self.fatal.get_or_insert(f);
        }
        self.scratch.due = due;
    }

    #[allow(clippy::too_many_arguments)]
    /// The envelope's reserve this step (E2), into `eff`: per class, what
    /// the present need has pending — its loads ranked and in flight at
    /// their estimate, its decoded copies at their cost — through the
    /// filter; the decoded tier's, their sum; and the network slots its
    /// ranked loads wait for.
    fn envelope_step(
        &mut self,
        eff: &mut Eff,
        jobs: &[(PriorityKey, Job, Want)],
        wants: Wants<'_>,
        st: &mut BudgetStats,
    ) {
        let ref_bytes = self.spec.params.ref_bytes;
        let mut pending = PerClass::splat(Bytes::ZERO);
        let mut waiting = PerClass::splat(0u32);
        let a = &self.arena;
        for (key, job, w) in jobs {
            let r = key.resource;
            if is_preload(w.urgency, r) {
                continue;
            }
            let c = self.class_of(r, w.class);
            match job {
                // A queued ancestor waits on the score's admission, as a
                // preload does: counted once it is in flight.
                Job::Load if w.urgency >= Urgency::Ancestor => {}
                Job::Load => {
                    pending[c] = pending[c].plus(self.envelope.estimate(c, ref_bytes));
                    if key.tier != Tier::Stored {
                        waiting[c] = waiting[c].saturating_add(1);
                    }
                }
                Job::Deliver => {
                    if let Some(cpu) = a.find(r).and_then(|s| a.fresh_cpu(s, self.epoch(r, c))) {
                        pending[c] = pending[c].plus(cpu.cost.gpu);
                    }
                }
            }
        }
        for &s in a.active() {
            let r = a.id(s);
            if a.loading(s) && wants.get(&r).is_some_and(|w| !is_preload(w.urgency, r)) {
                let c = a.class(s);
                pending[c] = pending[c].plus(self.envelope.estimate(c, ref_bytes));
            }
        }
        st.work = st
            .work
            .saturating_add(sat_u32(jobs.len() + a.active().len()));
        self.envelope.observe(&self.spec.params.envelope, pending);
        self.ahead = pending;
        eff.env = PerClass::from_fn(|c| self.envelope.reserve(c));
        eff.env_decoded = eff.env.iter().fold(Bytes::ZERO, |x, (_, b)| x.plus(b));
        eff.env_slots = PerClass::from_fn(|c| waiting[c].min(eff.quota[c]));
    }

    /// The step's [`EnvUse`], with its [`Room`] (kept for the step's
    /// admissions).
    fn envelope_use(
        &mut self,
        eff: &Eff,
        jobs: &[(PriorityKey, Job, Want)],
        wants: Wants<'_>,
        st: &mut BudgetStats,
    ) -> EnvUse {
        let room = match self.scratch.room.take() {
            Some(room) => room,
            None => self.room(wants, st),
        };
        let ref_bytes = self.spec.params.ref_bytes;
        // What a start cannot count on: the GPU less the cache (unwanted)
        // and less the ring's and refreshes' evictable preloads, which a
        // stronger preload may take the place of at its delivery (the
        // margin's swap, which holds E1 there).
        let mut gpu = PerClass::from_fn(|c| {
            self.ledger.gpu[c]
                .minus(self.arena.idle(c))
                .minus(room.bytes[c][0])
                .minus(room.bytes[c][urgency_bucket(Urgency::Ring)])
                .minus(room.bytes[c][urgency_bucket(Urgency::Refresh)])
        });
        // The decoded tier: what is on its way to the GPU — the present
        // need's in its reserve, the preloads' counted below. The rest is
        // its cache (copies of residents, demoted copies), released for
        // room down to its target.
        let mut decoded = Bytes::ZERO;
        let a = &self.arena;
        st.work = st.work.saturating_add(sat_u32(a.active().len()));
        for &vs in a.active() {
            let vr = a.id(vs);
            if !a.loading(vs) || !wants.get(&vr).is_some_and(|w| is_preload(w.urgency, vr)) {
                continue;
            }
            let c = a.class(vs);
            let est = self.envelope.estimate(c, ref_bytes);
            gpu[c] = gpu[c].plus(est);
            decoded = decoded.plus(est);
        }
        for (key, job, w) in jobs {
            let r = key.resource;
            if *job != Job::Deliver || !is_preload(w.urgency, r) {
                continue;
            }
            if let Some(s) = a.find(r) {
                let c = a.class(s);
                if let Some(cpu) = a.fresh_cpu(s, self.epoch(r, c)) {
                    gpu[c] = gpu[c].plus(cpu.cost.gpu);
                    decoded = decoded.plus(cpu.cost.gpu);
                }
            }
        }
        st.work = st.work.saturating_add(sat_u32(jobs.len()));
        let pool = eff.pool.map_or(Bytes::ZERO, |p| {
            Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |x, c| x.plus(gpu[*c]))
        });
        self.scratch.room = Some(room);
        EnvUse { gpu, pool, decoded }
    }

    fn start(
        &mut self,
        now: Micros,
        eff: &Eff,
        jobs: &[(PriorityKey, Job, Want)],
        loads: &[u32],
        key_now: &dyn Fn(&Budget, ResourceId) -> Option<PriorityKey>,
        wants: Wants<'_>,
        presence: &dyn PresenceOracle,
        tiles: &SortedMap<TileId, TileNeed>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let decoded_full = self.ledger.decoded >= eff.decoded;
        // Stored resources left unstarted for any reason but their own
        // quota: a network load of lower priority starting after them is a
        // local wait (I15).
        let mut skipped_stored = 0u32;
        // The envelope (E1, E3), read at the first preload.
        let mut env: Option<EnvUse> = None;
        let ref_bytes = self.spec.params.ref_bytes;
        for (key, _, w) in loads.iter().filter_map(|i| jobs.get(*i as usize)) {
            let r = &key.resource;
            let class = self.class_of(*r, w.class);
            let preload = is_preload(w.urgency, *r);
            if preload && !eff.batch {
                // A preload starts only into room it will find at its
                // delivery, and into the slots the present need leaves.
                let est = self.envelope.estimate(class, ref_bytes);
                if env.is_none() {
                    env = Some(self.envelope_use(eff, jobs, wants, st));
                }
                let roomy = env.as_ref().is_some_and(|u| u.admits(eff, class, est));
                let slots = key.tier == Tier::Stored
                    || self.in_flight[class].saturating_add(eff.env_slots[class]) < eff.quota[class];
                if !roomy || !slots {
                    if key.tier == Tier::Stored {
                        skipped_stored += 1;
                    }
                    continue;
                }
            }
            if decoded_full && w.urgency >= Urgency::Ring {
                // Backpressure: preload that could neither be delivered nor
                // kept decoded is not fetched.
                if key.tier == Tier::Stored {
                    skipped_stored += 1;
                }
                continue;
            }
            let source = if key.tier == Tier::Stored {
                Source::Local
            } else {
                Source::Network
            };
            match source {
                Source::Local => {
                    if self.local_reads >= eff.local {
                        continue;
                    }
                }
                Source::Network => {
                    if self.origin_holds_back(*r, class, now) {
                        continue;
                    }
                    if self.in_flight[class] >= eff.quota[class] {
                        // The present need never waits behind a preload
                        // (law 8): up to a drawn tile's drape, it takes a
                        // preload's slot.
                        if w.urgency > Urgency::Drape || preload {
                            if key.tier == Tier::Stored {
                                skipped_stored += 1;
                            }
                            continue;
                        }
                        // Preempt the least urgent in-flight preload. Every
                        // load is indexed in `active` (`start`).
                        st.work = st.work.saturating_add(sat_u32(self.arena.active().len()));
                        let victim = self
                            .arena
                            .active()
                            .iter()
                            .filter_map(|&vs| {
                                let a = &self.arena;
                                let vr = a.id(vs);
                                (a.class(vs) == class
                                    && matches!(
                                        a.load(vs),
                                        Load::Loading {
                                            source: Source::Network,
                                            ..
                                        }
                                    )
                                    && (wants.get(&vr).is_some_and(|vw| {
                                        is_preload(vw.urgency, vr)
                                            || (w.urgency <= Urgency::Hole
                                                && vw.urgency == Urgency::Ancestor)
                                    }) || a.urgency(vs) == Some(Urgency::Refresh)))
                                .then_some(vr)
                            })
                            .filter_map(|vr| key_now(self, vr).map(|k| (k, vr)))
                            .max();
                        let Some((_, vr)) = victim else { continue };
                        if let Some(vs) = self.arena.find(vr) {
                            if let Load::Loading { gen, .. } = self.arena.load(vs) {
                                self.arena.set_load(vs, Load::Idle);
                                release(&mut self.in_flight[class], &mut self.underflows);
                                d.cancel.push((vr, gen));
                                st.cancelled += 1;
                                st.cancels[CancelReason::Preempted as usize] += 1;
                            }
                        }
                    }
                    // A down origin's probe: the next waits for its delay.
                    self.origins
                        .started(Origin::of(*r, class), now, &self.cfg.origin);
                }
            }
            let gen = Generation(self.next_gen);
            self.next_gen += 1;
            let epoch = self.epoch(*r, class);
            let s = match self.arena.find(*r) {
                Some(s) => s,
                None => {
                    // A new slot joins the newest group, `now`'s.
                    self.newest_at(now);
                    let s = self.arena.find_or_insert(*r, class, w.level, now);
                    self.arena.mark(s, Mark::Wanted, true);
                    self.made(*r);
                    self.tile_slot(r.tile(), true, tiles);
                    s
                }
            };
            if self.arena.loading(s) {
                st.duplicates_in_flight += 1;
                continue;
            }
            let cold = self.arena.cold_mut(s);
            if cold.presence.is_none() && !cold.store_missed {
                cold.presence = Some(presence.holds(*r));
            }
            cold.first_wanted_or(now);
            cold.set_unneeded_since(None);
            self.arena.touch(s, now);
            self.arena.set_rank(s, Some(w.urgency), Some(Rank::of(key)));
            self.arena.set_load(
                s,
                Load::Loading {
                    gen,
                    source,
                    started: now,
                    epoch,
                },
            );
            if self.is_floor(w.class, w.level) {
                self.arena.mark(s, Mark::FloorSet, true);
            }
            self.arena.list_active(s);
            st.levels.starts[class][level_index(w.level)] += 1;
            if let Some(l) = r.layer() {
                st.layers.requests[layer_index(l)][usize::from(source == Source::Network)] += 1;
            }
            let timeout =
                Self::load_timeout_after(self.cfg.load_timeout, self.arena.cold(s).failures);
            self.deadline(now.plus(timeout), *r);
            if w.urgency >= Urgency::Ring {
                d.scores.push(self.load_event(*r, class, w, key.tier));
            }
            match source {
                Source::Network => {
                    self.in_flight[class] += 1;
                    st.started_network += 1;
                    st.starts[class][1] += 1;
                    st.local_waits += skipped_stored;
                    skipped_stored = 0;
                }
                Source::Local => {
                    self.local_reads += 1;
                    st.started_local += 1;
                    st.starts[class][0] += 1;
                }
            }
            if let Some(t) = self.recent.remove(r) {
                // An entry behind the horizon has left the window already.
                if t >= self.recent_horizon && now.since(t) <= THRASH_WINDOW {
                    // Back from a local store is a demotion paying off; back
                    // from the network is thrash.
                    match source {
                        Source::Network => st.thrash += 1,
                        Source::Local => st.rereads += 1,
                    }
                }
            }
            if preload {
                if let Some(u) = env.as_mut() {
                    u.take(eff, class, self.envelope.estimate(class, ref_bytes));
                }
            }
            d.start.push(Start {
                resource: *r,
                gen,
                class,
                source,
            });
        }
    }

    /// Returns whether the hard limit held anything back.
    /// Evicts unprotected residents of `class` worth less than work of
    /// `urgency`, in eviction order, until `bytes` more fit under `limit`.
    /// Admission preempts lower-value residents rather than refusing work.
    #[allow(clippy::too_many_arguments)]
    fn make_room(
        &mut self,
        now: Micros,
        class: Class,
        bytes: Bytes,
        limit: Bytes,
        urgency: Urgency,
        prob: u32,
        wants: Wants<'_>,
        d: &mut Decisions,
        st: &mut BudgetStats,
        strained: &mut bool,
    ) -> bool {
        if self.ledger.gpu[class].plus(bytes) <= limit {
            return true;
        }
        let mut room = match self.scratch.room.take() {
            Some(room) => room,
            None => self.room(wants, st),
        };
        // The victims now: still evictable, and worth less than the work.
        let victim = |b: &Budget, r: &ResourceId, s: Slot| -> Option<Bytes> {
            let a = &b.arena;
            let worth_less = wants
                .get(r)
                .is_none_or(|w| urgency < Urgency::Ring && w.urgency > urgency);
            (!a.has_transit(s) && worth_less).then_some(a.gpu(s)?.bytes)
        };
        let freeable = self.freeable(&room, class, urgency);
        debug_assert_eq!(
            freeable,
            self.arena
                .iter()
                .filter(|&s| {
                    let a = &self.arena;
                    a.class(s) == class
                        && a.has_gpu(s)
                        && !self.kept(s)
                        && !self.is_floor(a.class(s), a.level(s))
                })
                .filter_map(|s| victim(self, &self.arena.id(s), s))
                .fold(Bytes::ZERO, |a, b| a.plus(b)),
            "the bytes counted evictable are those a scan finds"
        );
        // A preload's swap (`docs/explorations/budget-progressive-coarse-preload.md`
        // §3): another preload, not drawn, whose probability of use is
        // under this one's by more than the margin, makes room for it —
        // the progressive score's hysteresis.
        let margin = u64::from(self.spec.params.margin_permille);
        let swaps = |b: &Budget, r: &ResourceId, s: Slot| -> bool {
            urgency >= Urgency::Ring
                && prob > 0
                && wants.get(r).is_some_and(|w| w.urgency >= Urgency::Ring)
                && {
                    let (_, old, _) = b.arena.place(s);
                    u64::from(old) * (1_000 + margin) < u64::from(prob) * 1_000
                }
        };
        let mut swapping = false;
        if self.ledger.gpu[class].minus(freeable).plus(bytes) > limit {
            if urgency >= Urgency::Ring && prob > 0 {
                let a = &self.arena;
                // Lowest scores first, until enough: the scan stops as soon
                // as the room is found (a step's work stays bounded by it).
                let need = self.ledger.gpu[class]
                    .minus(freeable)
                    .plus(bytes)
                    .minus(limit);
                let mut extra = Bytes::ZERO;
                let mut looked = 0u32;
                for &((.., r), s) in &room.grouped {
                    if extra >= need {
                        break;
                    }
                    looked += 1;
                    let ok = a.class(s) == class
                        && a.has_gpu(s)
                        && !a.has_transit(s)
                        && !self.kept(s)
                        && !self.is_floor(class, a.level(s))
                        && victim(self, &r, s).is_none()
                        && swaps(self, &r, s);
                    if ok {
                        extra = extra.plus(a.gpu(s).map_or(Bytes::ZERO, |g| g.bytes));
                    }
                }
                st.work = st.work.saturating_add(looked);
                swapping = extra >= need;
            }
            if !swapping {
                *strained = true;
                self.scratch.room = Some(room);
                return false;
            }
        }
        let mut walk = Walk::default();
        while let Some(((.., r), s)) =
            walk.next(&self.arena, &room.grouped, |c| c == class, &Held::GPU)
        {
            if self.ledger.gpu[class].plus(bytes) <= limit {
                break;
            }
            st.work = st.work.saturating_add(1);
            // The order holds neither the kept set nor the floor.
            if self.kept(s) || self.is_floor(self.arena.class(s), self.arena.level(s)) {
                continue;
            }
            let (b, reason) = match victim(self, &r, s) {
                Some(b) => (b, Reason::Admission),
                None if swapping && !self.arena.has_transit(s) && swaps(self, &r, s) => {
                    let Some(g) = self.arena.gpu(s) else {
                        continue;
                    };
                    (g.bytes, Reason::Swap)
                }
                None => continue,
            };
            if wants.contains_key(&r) {
                // Wanted work displaces wanted work: the view asks for more
                // than the budget holds.
                *strained = true;
            }
            if self.arena.grouped(s) {
                let i = want_bucket(wants.get(&r));
                room.bytes[class][i] = room.bytes[class][i].minus(b);
            }
            d.scores.push(self.held_event(s, Side::Evict, reason));
            self.arena.take_gpu(s, &mut self.ledger);
            d.evict.push(r);
            st.evicted += 1;
            st.evictions[class][EvictReason::Admission as usize] += 1;
            st.evictions_by_level[class][super::stats::level_index(self.arena.level(s))] += 1;
            self.evicted(r, now);
        }
        self.scratch.room = Some(room);
        true
    }

    /// This step's [`Room`], built at its first use, after the mark: the
    /// grouped GPU residents outside the kept set, the floor and transit.
    fn room(&self, wants: Wants<'_>, st: &mut BudgetStats) -> Room {
        let a = &self.arena;
        let mut grouped: Vec<(EvictKey, Slot)> = a
            .grouped_slots()
            .filter(|&s| {
                a.has_gpu(s)
                    && !a.has_transit(s)
                    && !self.kept(s)
                    && !self.is_floor(a.class(s), a.level(s))
            })
            .map(|s| (a.evict_key(s), s))
            .collect();
        // Keys end in the resource: total, so unstable is stable, and the
        // slots' order — not the resources' — is forgotten. A slot listed
        // twice comes out once.
        grouped.sort_unstable();
        grouped.dedup();
        st.work = st.work.saturating_add(sat_u32(grouped.len()));
        let mut bytes = PerClass::splat([Bytes::ZERO; WANT_BUCKETS]);
        for &((.., r), s) in &grouped {
            let (c, i) = (a.class(s), want_bucket(wants.get(&r)));
            bytes[c][i] = bytes[c][i].plus(a.gpu(s).map_or(Bytes::ZERO, |g| g.bytes));
        }
        Room { grouped, bytes }
    }

    /// What `make_room` can evict of `class` for work of `urgency`: every
    /// idle byte (nothing filed is wanted this step), and of the grouped
    /// ones those not wanted, or wanted less than the work when the work is
    /// the view's.
    fn freeable(&self, room: &Room, class: Class, urgency: Urgency) -> Bytes {
        let b = &room.bytes[class];
        let wanted_less = if urgency < Urgency::Ring {
            b[urgency_bucket(urgency) + 1..]
                .iter()
                .fold(Bytes::ZERO, |a, x| a.plus(*x))
        } else {
            Bytes::ZERO
        };
        self.arena.idle(class).plus(b[0]).plus(wanted_less)
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &mut self,
        now: Micros,
        eff: &Eff,
        jobs: &[(PriorityKey, Job, Want)],
        picked: &[u32],
        tiles: &SortedMap<TileId, TileNeed>,
        coarsening: &SortedMap<TileId, ()>,
        wants: Wants<'_>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) -> (bool, bool) {
        let mut flags = (false, false);
        for (key, _, w) in picked.iter().filter_map(|i| jobs.get(*i as usize)) {
            let r = &key.resource;
            let Some(group) = floor_group(tiles, *r, |c, l| self.is_floor(c, l)) else {
                self.deliver_one(now, eff, *r, w, coarsening, wants, d, st, &mut flags);
                continue;
            };
            // A floor mesh and its drapes are one swap group (D-A1): none
            // goes until every one is loaded, then they go together.
            let a = &self.arena;
            let ready = group.clone().all(|m| {
                a.find(m).is_some_and(|s| {
                    a.has_gpu(s)
                        || a.has_transit(s)
                        || a.fresh_cpu(s, self.epoch(m, a.class(s))).is_some()
                })
            });
            if !ready {
                continue;
            }
            for m in group {
                let mw = wants.get(&m).copied().unwrap_or(*w);
                self.deliver_one(now, eff, m, &mw, coarsening, wants, d, st, &mut flags);
            }
        }
        flags
    }

    /// Delivers `r` if it is decoded, not yet on its way, and fits the
    /// credit and its class's limit. `flags`: (blocked, strained).
    #[allow(clippy::too_many_arguments)]
    fn deliver_one(
        &mut self,
        now: Micros,
        eff: &Eff,
        r: ResourceId,
        w: &Want,
        coarsening: &SortedMap<TileId, ()>,
        wants: Wants<'_>,
        d: &mut Decisions,
        st: &mut BudgetStats,
        flags: &mut (bool, bool),
    ) {
        let (blocked, strained) = (&mut flags.0, &mut flags.1);
        let Some(s) = self.arena.find(r) else {
            return;
        };
        if self.orphans_now.contains(&r) {
            return;
        }
        let class = self.arena.class(s);
        let epoch = self.epoch(r, class);
        // Already on its way, or on the GPU at the current epoch (a stale
        // GPU copy is redelivered: new before old).
        if self.arena.has_transit(s) || self.arena.gpu(s).is_some_and(|g| g.epoch == epoch) {
            return;
        }
        let Some(c) = self.arena.fresh_cpu(s, epoch) else {
            return;
        };
        let credit_ok = self.ledger.in_transit == Bytes::ZERO
            || self.ledger.in_transit.plus(c.cost.staging) <= self.cfg.delivery_credit;
        if !credit_ok {
            return;
        }
        if !eff.batch {
            let shrinking = matches!(r, ResourceId::Mesh(t) if coarsening.contains_key(&t));
            let urgent = matches!(w.urgency, Urgency::Floor | Urgency::Hole | Urgency::Drape);
            // A preload (`is_preload`) enters only the envelope: under the
            // target less the present need's reserve (E1, E3); the chain of
            // ancestors' meshes is what is drawn's fallback (coverage) and
            // takes the room the present need takes.
            let preload = is_preload(w.urgency, r);
            let mut limit = if shrinking {
                eff.hard[class]
            } else if urgent {
                eff.hard[class].minus(eff.reserve[class])
            } else if preload {
                eff.line(class)
            } else {
                eff.target[class].min(eff.hard[class].minus(eff.reserve[class]))
            };
            if let Some(p) = eff.pool.filter(|p| p.members[class]) {
                // A member also stays within what the pool leaves it now,
                // the other members' deliveries of this step counted.
                let reserve = p.hard.scaled(u64::from(self.cfg.hard_reserve_percent), 100);
                let whole = if shrinking {
                    p.hard
                } else if urgent {
                    p.hard.minus(reserve)
                } else if preload {
                    p.target.minus(eff.pool_env(&p))
                } else {
                    p.target.min(p.hard.minus(reserve))
                };
                limit = limit.min(whole.minus(p.others(class, &self.ledger.gpu)));
            }
            if preload && (*strained || *blocked) {
                return;
            }
            // Speculation waits while the specification's fixed headroom
            // is short (0 by default) — it never evicts to make that room.
            if preload {
                let h = u64::from(self.spec.params.headroom_permille.min(1_000));
                let line = eff.target[class].scaled(1_000 - h, 1_000);
                if h > 0 && self.ledger.gpu[class].plus(c.cost.gpu) > line {
                    return;
                }
            }
            let mut strain = false;
            let prob =
                super::value::probability(&self.spec, self.reach_tiles(), w.urgency, w.level, &w.q);
            // Room for its peak: resident, and what an encode on the GPU
            // holds beside it until it has run.
            let ok = self.make_room(
                now,
                class,
                c.cost.gpu.plus(c.cost.transient),
                limit,
                w.urgency,
                prob,
                wants,
                d,
                st,
                &mut strain,
            );
            if strain && w.urgency <= Urgency::Blocker {
                *strained = true;
            }
            if !ok {
                if urgent || shrinking {
                    *blocked = true;
                }
                return;
            }
        }
        let reused = c.delivered_before;
        // A copy on its way again was on the GPU only if stale, and a stale
        // copy the step's room could count — not drawn, not in a swap, not
        // of the floor — was evicted by this step's recycle (`recycle`,
        // before the deliveries): none of the room's bytes leaves with it.
        debug_assert!(
            self.scratch.room.is_none() || {
                let a = &self.arena;
                !(a.grouped(s)
                    && a.has_gpu(s)
                    && !self.kept(s)
                    && !self.is_floor(a.class(s), a.level(s)))
            },
            "{r:?}: a stale copy the room counts is delivered again"
        );
        // `make_room` frees no slot: `s` is still `r`'s.
        self.arena.list_active(s);
        self.arena.put_transit(
            s,
            &mut self.ledger,
            Transit {
                gen: c.gen,
                cost: c.cost,
                epoch: c.epoch,
            },
        );
        if reused {
            st.network_bytes_avoided = st.network_bytes_avoided.plus(c.cost.gpu);
            if let Some(l) = r.layer() {
                st.layers.ram_hits[layer_index(l)] += 1;
            }
        }
        st.delivered += 1;
        self.delivered_bytes = self.delivered_bytes.plus(c.cost.gpu);
        self.envelope.learn(class, c.cost.gpu);
        if !is_preload(w.urgency, r) {
            // Delivered: no longer room to make ahead of it.
            self.ahead[class] = self.ahead[class].minus(c.cost.gpu);
        }
        d.deliver.push((r, c.gen));
    }

    /// The consumer's uploads this frame, into `plan`: the copies in
    /// transit in priority order, grouped — the blockers of one parent
    /// (meshes and drapes), else a tile's mesh and its drape — each group
    /// going whole at its first member, byte-capped (the first item always
    /// goes). Nothing allocated once the step's buffers have grown: the
    /// transits and their order by group are step buffers, the plan's
    /// groups overwritten in place.
    fn plan(
        &mut self,
        eff: &Eff,
        key: &dyn Fn(&Budget, ResourceId) -> Option<PriorityKey>,
        tiles: &SortedMap<TileId, TileNeed>,
        plan: &mut UploadPlan,
    ) {
        plan.clear();
        let group_of = |r: ResourceId| -> (u8, TileId) {
            let t = r.tile();
            match tiles.get(&t) {
                Some(tn) if tn.membership == Membership::Blocker => match tn.need.parent() {
                    Some(p) => (0, p),
                    None => (1, t),
                },
                _ => (1, t),
            }
        };
        let mut transit = std::mem::take(&mut self.scratch.transit);
        let mut by_group = std::mem::take(&mut self.scratch.picked);
        transit.clear();
        for r in self.arena.transits() {
            let Some(t) = self.arena.find(r).and_then(|s| self.arena.transit(s)) else {
                continue;
            };
            let k = key(self, r).unwrap_or(PriorityKey {
                precedence: super::priority::Precedence::Anticipation,
                urgency: Urgency::Refresh,
                score: Reverse(i64::MIN),
                tier: Tier::Remote,
                resource: r,
            });
            // What the upload moves across, not what it will hold: a
            // texture the GPU encodes crosses as RGBA8.
            transit.push((k, t.cost.transfer, group_of(r), false));
        }
        // Keys end in the resource: total, so unstable is stable.
        transit.sort_unstable_by_key(|a| a.0);
        // Each group's members as a run, in transit order within the group:
        // the places of `transit`, by (group, place).
        by_group.clear();
        by_group.extend((0..transit.len()).map(|i| u32::try_from(i).unwrap_or(u32::MAX)));
        by_group.sort_unstable_by_key(|&i| (transit[i as usize].2, i));
        let mut groups = 0;
        // The blocks a member's GPU encode writes, looked up only while
        // something in transit encodes (CB5): nothing to read without
        // compression.
        let encoding = self.arena.encode_blocks_in_transit() > 0;
        let a = &self.arena;
        let blocks_of = |r: ResourceId| -> u64 {
            if !encoding {
                return 0;
            }
            a.find(r)
                .and_then(|s| a.transit(s))
                .map_or(0, |t| t.cost.encode_blocks)
        };
        for i in 0..transit.len() {
            // A group goes whole, at its first member in transit order: a
            // later member finds it gone.
            if transit[i].3 {
                continue;
            }
            let (k, b, g, _) = transit[i];
            let from = by_group.partition_point(|&j| transit[j as usize].2 < g);
            let to = from + by_group[from..].partition_point(|&j| transit[j as usize].2 == g);
            let members = &by_group[from..to];
            let bytes = members
                .iter()
                .fold(Bytes::ZERO, |a, &j| a.plus(transit[j as usize].1));
            let blocks = members.iter().fold(0u64, |a, &j| {
                a.saturating_add(blocks_of(transit[j as usize].0.resource))
            });
            let over = plan.bytes.plus(bytes) > eff.upload
                || plan.encode_blocks.saturating_add(blocks) > eff.encode;
            if plan.items.is_empty() && over {
                // The first item always goes, alone if its group is too big.
                plan.items.push(k.resource);
                plan.bytes = b;
                plan.encode_blocks = blocks_of(k.resource);
                break;
            }
            if over {
                break;
            }
            plan.items
                .extend(members.iter().map(|&j| transit[j as usize].0.resource));
            plan.bytes = plan.bytes.plus(bytes);
            plan.encode_blocks = plan.encode_blocks.saturating_add(blocks);
            for &j in members {
                transit[j as usize].3 = true;
            }
            if members.len() > 1 {
                let out = plan.group(groups);
                out.extend(members.iter().map(|&j| transit[j as usize].0.resource));
                out.sort_unstable();
                groups += 1;
            }
        }
        plan.groups.truncate(groups);
        debug_assert!(
            plan.groups
                .iter()
                .all(|g| g.members.len() > 1 && g.members.iter().all(|m| plan.items.contains(m))),
            "every group kept is a group of the plan's items"
        );
        self.scratch.transit = transit;
        self.scratch.picked = by_group;
    }

    /// The step's room against a recount, before the sweeps (which walk it
    /// without its bytes): per class, the GPU bytes of its grouped
    /// residents still on the GPU and not in transit. `None` when they
    /// agree, or when the step built no room.
    fn room_drift(&self) -> Option<(Class, Bytes, Bytes)> {
        let room = self.scratch.room.as_ref()?;
        let a = &self.arena;
        Class::ALL.into_iter().find_map(|c| {
            let counted = room.bytes[c].iter().fold(Bytes::ZERO, |x, y| x.plus(*y));
            let found = room
                .grouped
                .iter()
                .filter(|&&(_, s)| a.class(s) == c && !a.has_transit(s))
                .filter_map(|&(_, s)| a.gpu(s))
                .fold(Bytes::ZERO, |x, g| x.plus(g.bytes));
            (counted != found).then_some((c, counted, found))
        })
    }

    fn evict(
        &mut self,
        now: Micros,
        eff: &Eff,
        wants: Wants<'_>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        debug_assert_eq!(
            self.room_drift(),
            None,
            "the room counts the bytes of its residents still evictable"
        );
        if let Some(p) = eff.pool {
            self.evict_pooled(now, p, wants, d, st);
        }
        for class in Class::ALL {
            if eff.pool.is_some_and(|p| p.members[class]) {
                continue;
            }
            // What the compressions in flight give back counts as given
            // back (CB9, law 4): the sweep evicts what they cannot free.
            // E3: back to the target less what the present need has
            // pending now — its room made ahead, the preloads leaving it
            // first; not less the reserve, which bounds what enters (a
            // preload is not evicted for bytes already delivered).
            let used = self.settled().gpu[class];
            let high = eff.target[class];
            let low = high.minus(self.ahead[class]);
            if used <= low {
                continue;
            }
            let room = match self.scratch.room.take() {
                Some(room) => room,
                None => self.room(wants, st),
            };
            let mut law = self.sweep_law();
            let grouped: &[_] = if law.walks_grouped() || holds_preloads(&room, class) {
                &room.grouped
            } else {
                &[]
            };
            let mut walk = Walk::default();
            while let Some(((.., r), s)) =
                walk.next(&self.arena, grouped, |c| c == class, &Held::GPU)
            {
                if self.settled().gpu[class] <= low {
                    break;
                }
                st.work = st.work.saturating_add(1);
                // The order holds neither the kept set nor the floor.
                if self.kept(s) || self.is_floor(self.arena.class(s), self.arena.level(s)) {
                    continue;
                }
                // Still on the GPU, and not on its way again.
                let evictable = self.arena.has_gpu(s) && !self.arena.has_transit(s);
                if !evictable {
                    continue;
                }
                // Not what the view wants now or wanted within the horizon;
                // a preload leaves the envelope first (E3).
                let preload = wants.get(&r).is_some_and(|w| is_preload(w.urgency, r));
                if !preload && wants.contains_key(&r) && !law.walks_grouped() {
                    // Walked for the preloads among the grouped copies: one
                    // the view wants now is passed over, uncounted.
                    continue;
                }
                if !preload {
                    match law.judge(wants.contains_key(&r), self.arena.poses_since_wanted(s)) {
                        Verdict::Release => {}
                        Verdict::Keep => continue,
                        Verdict::Stop => break,
                    }
                }
                let why = preload.then_some(Reason::Envelope);
                self.evict_as(now, s, class, why, d, st);
            }
            // Still over the target: back within it, the law's keepers
            // included (`Reason::Fit`).
            let mut walk = Walk::default();
            while let Some((_, s)) =
                walk.next(&self.arena, &room.grouped, |c| c == class, &Held::GPU)
            {
                if self.ledger.gpu[class] <= high {
                    break;
                }
                st.work = st.work.saturating_add(1);
                let a = &self.arena;
                if self.kept(s)
                    || self.is_floor(a.class(s), a.level(s))
                    || !a.has_gpu(s)
                    || a.has_transit(s)
                {
                    continue;
                }
                self.evict_as(now, s, class, Some(Reason::Fit), d, st);
            }
            self.scratch.room = Some(room);
        }
        // Only a step under pressure built one: nothing kept across steps.
        self.scratch.room = None;
        self.release_below_floor_value(now, d, st);
        // The decoded tier: over its target, down to it — a copy demoted
        // from the GPU is what a turn back finds without the network, so it
        // is not released below the target for room's sake alone.
        if self.ledger.decoded > eff.decoded {
            self.release_decoded(eff.decoded, d, st);
        }
    }

    /// Releases decoded copies outside transit, down to `target`. Release
    /// order: copies of protected GPU residents first (they will not be
    /// evicted soon), then of evictable GPU residents (their copy is what an
    /// eviction demotes to), then copies already demoted (they save a
    /// re-read when the view comes back); each in eviction order.
    fn release_decoded(&mut self, target: Bytes, d: &mut Decisions, st: &mut BudgetStats) {
        let a = &self.arena;
        let held = |s: Slot| a.has_cpu(s) && !a.has_transit(s);
        // The protected first: the kept set, sorted by key. Drawn or in a
        // swap, it was needed this step: grouped.
        let mut kept: Vec<(EvictKey, Slot)> = a
            .grouped_slots()
            .filter(|&s| self.kept(s) && held(s) && a.has_gpu(s))
            .map(|s| (a.evict_key(s), s))
            .collect();
        kept.sort_unstable();
        kept.dedup();
        // The grouped holders, sorted, merged with the filed ones below.
        let mut grouped: Vec<(EvictKey, Slot)> = a
            .grouped_slots()
            .filter(|&s| held(s))
            .map(|s| (a.evict_key(s), s))
            .collect();
        grouped.sort_unstable();
        grouped.dedup();
        st.work = st
            .work
            .saturating_add(sat_u32(kept.len()))
            .saturating_add(sat_u32(grouped.len()));
        for &(_, s) in &kept {
            if self.ledger.decoded <= target {
                return;
            }
            self.release_one(s, d, st);
        }
        // Then the evictable GPU residents' copies, then the demoted ones.
        for (held, gpu) in [(&Held::GPU[..1], true), (&[Held::Cpu][..], false)] {
            let mut walk = Walk::default();
            while let Some((_, s)) = walk.next(&self.arena, &grouped, |_| true, held) {
                st.work = st.work.saturating_add(1);
                let a = &self.arena;
                if !a.has_cpu(s) || a.has_gpu(s) != gpu || (gpu && self.kept(s)) {
                    continue;
                }
                if self.ledger.decoded <= target {
                    return;
                }
                self.release_one(s, d, st);
            }
        }
    }

    /// Whether `s`, no longer wanted, is of a level more than one finer than
    /// what the view needs now: nothing selects it soon, it goes once out
    /// of the need for the thrash window, whatever it was.
    fn far_finer(&self, s: Slot) -> bool {
        self.need_level
            .is_some_and(|n| self.arena.level(s) > n.saturating_add(1))
    }

    /// The dynamic headroom, bytes: the smoothed upload rate over the
    /// specification's horizon, plus what is in transit.
    pub(super) fn dynamic_headroom(&self) -> Bytes {
        let rate = self.upload_rate.get().unwrap_or(0);
        let over = u128::from(rate) * u128::from(self.costed.params.horizon.0) / 1_000_000;
        Bytes(u64::try_from(over).unwrap_or(u64::MAX)).plus(self.ledger.in_transit)
    }

    /// Takes the step's delivered bytes into the upload rate.
    pub(super) fn measure_uploads(&mut self, now: Micros) {
        let dt = self.last_now.map_or(Micros::ZERO, |l| now.since(l));
        let bytes = std::mem::take(&mut self.delivered_bytes);
        if dt.0 == 0 {
            return;
        }
        let rate = u128::from(bytes.0) * 1_000_000 / u128::from(dt.0);
        self.upload_rate.take(
            u64::try_from(rate).unwrap_or(u64::MAX),
            dt,
            Micros::from_millis(500),
        );
    }

    /// Releases, from the bottom of the release order, the GPU copies no
    /// longer wanted whose probability of use fell under the
    /// specification's floor value, room or not: far, fine, out of the ring.
    /// At most the specification's `release_visits` copies looked at a
    /// step: a frame's
    /// releases stay small; the next step goes on.
    fn release_below_floor_value(&mut self, now: Micros, d: &mut Decisions, st: &mut BudgetStats) {
        let floor =
            super::value::CERTAIN / 1_000 * u32::from(self.spec.params.floor_value_permille);
        if floor == 0 {
            return;
        }
        let mut after: Option<(EvictKey, Slot)> = None;
        let mut low: Vec<(Slot, Class)> = Vec::new();
        let visits = self.spec.params.release_visits;
        let mut looked = 0u32;
        for _ in 0..visits {
            let Some(e) = self.arena.next_filed(after.as_ref(), |_| true, &Held::GPU) else {
                break;
            };
            looked += 1;
            after = Some(e);
            let s = e.1;
            let a = &self.arena;
            let (_, p, _) = a.place(s);
            let long_gone = a
                .cold(s)
                .unneeded_since()
                .is_some_and(|t| now.since(t) >= super::THRASH_WINDOW);
            if (p < floor || self.far_finer(s))
                && long_gone
                && a.has_gpu(s)
                && !a.has_transit(s)
                && !self.kept(s)
                && !self.is_floor(a.class(s), a.level(s))
            {
                low.push((s, a.class(s)));
            }
        }
        // The decoded copies of the same kind, held without a GPU copy.
        let mut after: Option<(EvictKey, Slot)> = None;
        let mut low_cpu: Vec<Slot> = Vec::new();
        for _ in 0..visits {
            let Some(e) = self
                .arena
                .next_filed(after.as_ref(), |_| true, &[Held::Cpu])
            else {
                break;
            };
            looked += 1;
            after = Some(e);
            let s = e.1;
            let a = &self.arena;
            let (_, p, _) = a.place(s);
            let long_gone = a
                .cold(s)
                .unneeded_since()
                .is_some_and(|t| now.since(t) >= super::THRASH_WINDOW);
            if (p < floor || self.far_finer(s)) && long_gone && a.has_cpu(s) && !a.has_transit(s) {
                low_cpu.push(s);
            }
        }
        st.work = st.work.saturating_add(looked);
        for s in low_cpu {
            d.scores
                .push(self.held_event(s, Side::Decoded, Reason::BelowFloorValue));
            self.arena.take_cpu(s, &mut self.ledger);
            d.release.push(self.arena.id(s));
            st.released += 1;
            st.releases[self.arena.class(s)] += 1;
            st.levels.releases[self.arena.class(s)][level_index(self.arena.level(s))] += 1;
        }
        for (s, class) in low {
            d.scores
                .push(self.held_event(s, Side::Evict, Reason::BelowFloorValue));
            let r = self.arena.id(s);
            self.arena.take_gpu(s, &mut self.ledger);
            d.evict.push(r);
            st.evicted += 1;
            st.evictions[class][EvictReason::Sweep as usize] += 1;
            st.evictions_by_level[class][level_index(self.arena.level(s))] += 1;
            self.evicted(r, now);
        }
    }

    /// Releases the decoded copy of `s`, and counts it.
    fn release_one(&mut self, s: Slot, d: &mut Decisions, st: &mut BudgetStats) {
        d.scores
            .push(self.held_event(s, Side::Decoded, Reason::Sweep));
        self.arena.take_cpu(s, &mut self.ledger);
        d.release.push(self.arena.id(s));
        st.released += 1;
        st.releases[self.arena.class(s)] += 1;
        st.levels.releases[self.arena.class(s)][level_index(self.arena.level(s))] += 1;
    }

    /// D-A3: one sweep over the pool's members, in the one eviction order,
    /// down to the pool's target less what its members' present need has
    /// pending, no member below its
    /// floor except to keep I2, none left above its ceiling.
    fn evict_pooled(
        &mut self,
        now: Micros,
        p: PoolEff,
        wants: Wants<'_>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let used = |l: &Ledger| {
            Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |a, c| a.plus(l.gpu[*c]))
        };
        let floor = |c: Class| p.floor(c, p.target);
        // A member over its own ceiling (its own target) is trimmed to its
        // line, as a class outside the pool is (E3).
        let ceiling_high = |c: Class| p.ceil_target[c];
        let ahead = self.ahead;
        let ceiling_low = |c: Class| p.ceil_target[c].minus(ahead[c]);
        // What the compressions in flight give back counts as given back
        // (CB9, law 4).
        let high = p.target;
        let low = p.target.minus(
            Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |a, c| a.plus(ahead[*c])),
        );
        let pool_over = used(&self.settled()) > low;
        if !pool_over
            && Class::ALL
                .iter()
                .all(|c| !(p.members[*c] && self.settled().gpu[*c] > ceiling_low(*c)))
        {
            return;
        }
        // I2 (Laurent, 2026-10-21): what is not protected fits in the
        // member's target. The sweep evicts only unprotected residents, so
        // the protected bytes do not move while it runs; the target does, as
        // the other members shrink.
        let protected = self.protected_bytes();
        let short = |l: &Ledger, c: Class| {
            p.members[c] && l.gpu[c].minus(protected[c]) > p.target(c, &l.gpu)
        };
        let room = match self.scratch.room.take() {
            Some(room) => room,
            None => self.room(wants, st),
        };
        let member = |c: Class| p.members[c];
        // A candidate: a member's GPU resident outside the kept set, the
        // floor and transit.
        let candidate = |b: &Budget, s: Slot| {
            let a = &b.arena;
            a.has_gpu(s) && !a.has_transit(s) && !b.kept(s) && !b.is_floor(a.class(s), a.level(s))
        };
        // Two passes in the one order. The first is proactive: down to the
        // low watermarks, under the law (`sweep`) — not what the view wants
        // now or wanted within the horizon. The second brings what is still
        // over its high watermark (the target) back within it, the law's
        // keepers included: they fit the target (`Reason::Fit`), as I2's
        // pass below.
        let mut law = self.sweep_law();
        for lawful in [true, false] {
            let line = |c: Class| if lawful { ceiling_low(c) } else { ceiling_high(c) };
            let mut over: PerClass<bool> =
                PerClass::from_fn(|c| p.members[c] && self.settled().gpu[c] > line(c));
            let pool_over = used(&self.settled()) > if lawful { low } else { high };
            if !pool_over && Class::ALL.iter().all(|c| !over[*c]) {
                break;
            }
            let pool_bound = if lawful { low } else { high };
            let ceiling = |c: Class| {
                if lawful {
                    ceiling_low(c)
                } else {
                    ceiling_high(c)
                }
            };
            let preloads = Class::ALL
                .iter()
                .any(|c| p.members[*c] && holds_preloads(&room, *c));
            let grouped: &[_] = if !lawful || law.walks_grouped() || preloads {
                &room.grouped
            } else {
                &[]
            };
            let mut walk = Walk::default();
            while let Some(((.., r), s)) = walk.next(&self.arena, grouped, member, &Held::GPU) {
                st.work = st.work.saturating_add(1);
                if !candidate(self, s) {
                    continue;
                }
                let preload = wants.get(&r).is_some_and(|w| is_preload(w.urgency, r));
                if lawful && !preload && wants.contains_key(&r) && !law.walks_grouped() {
                    continue;
                }
                if lawful && !preload {
                    match law.judge(wants.contains_key(&r), self.arena.poses_since_wanted(s)) {
                        Verdict::Release => {}
                        Verdict::Keep => continue,
                        Verdict::Stop => break,
                    }
                }
                let (class, bytes) = (
                    self.arena.class(s),
                    self.arena.gpu(s).map_or(Bytes::ZERO, |g| g.bytes),
                );
                let settled = self.settled();
                for c in Class::ALL {
                    over[c] &= settled.gpu[c] > ceiling(c);
                }
                let pool_trim = pool_over && used(&settled) > pool_bound;
                if !pool_trim && Class::ALL.iter().all(|c| !over[*c]) {
                    break;
                }
                // Over its ceiling it goes; for the pool's sake, only down to
                // its floor.
                let goes =
                    over[class] || (pool_trim && settled.gpu[class].minus(bytes) >= floor(class));
                if goes {
                    let why = if !lawful {
                        Some(Reason::Fit)
                    } else {
                        preload.then_some(Reason::Envelope)
                    };
                    self.evict_as(now, s, class, why, d, st);
                }
            }
        }
        // The floor yields to I2 (Laurent, 2026-10-21), and only as far as
        // I2 needs: a member whose unprotected bytes are still over its
        // target once the pool is trimmed goes below its floor, in the same
        // order, until they fit.
        let mut walk = Walk::default();
        while let Some((_, s)) = walk.next(&self.arena, &room.grouped, member, &Held::GPU) {
            if Class::ALL.iter().all(|c| !short(&self.ledger, *c)) {
                break;
            }
            st.work = st.work.saturating_add(1);
            let class = self.arena.class(s);
            if candidate(self, s) && short(&self.ledger, class) {
                self.evict_as(now, s, class, Some(Reason::Fit), d, st);
            }
        }
        self.scratch.room = Some(room);
    }

    /// The proactive sweep's law (`sweep`): its horizon in camera poses,
    /// bound but under a memory signal.
    fn sweep_law(&self) -> super::sweep::SweepLaw {
        super::sweep::SweepLaw::new(
            self.spec.params.sweep_keep_poses,
            self.spec.params.release_visits,
            self.pressure == Pressure::Normal,
        )
    }

    /// Evicts the resource of `s`, of `class`, from the GPU, and counts it:
    /// for the sweep (or a memory signal's) when `reason` is `None`.
    fn evict_as(
        &mut self,
        now: Micros,
        s: Slot,
        class: Class,
        reason: Option<Reason>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let r = self.arena.id(s);
        // A compression in flight goes with its copy.
        self.compress.flights.remove(&r);
        let why = if self.pressure == Pressure::Normal {
            EvictReason::Sweep
        } else {
            EvictReason::Pressure
        };
        let reason = reason.unwrap_or(match why {
            EvictReason::Sweep => Reason::Sweep,
            _ => Reason::Pressure,
        });
        d.scores.push(self.held_event(s, Side::Evict, reason));
        self.arena.take_gpu(s, &mut self.ledger);
        d.evict.push(r);
        st.evicted += 1;
        st.evictions[class][why as usize] += 1;
        st.evictions_by_level[class][super::stats::level_index(self.arena.level(s))] += 1;
        self.evicted(r, now);
    }

    fn relax(
        &mut self,
        now: Micros,
        eff: &Eff,
        protected: &PerClass<Bytes>,
        blocked: bool,
        strained: bool,
    ) {
        if eff.batch {
            self.scale = 1_000;
            return;
        }
        let dt = self
            .last_now
            .map(|l| now.since(l))
            .unwrap_or(Micros::ZERO)
            .0;
        let mut over = strained
            || Class::ALL
                .iter()
                .any(|&c| protected[c] > eff.target[c].plus(eff.overflow[c]));
        if let Some(p) = eff.pool {
            // The pool's members relax on the pool's protected bytes too.
            let pooled = Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |a, c| a.plus(protected[*c]));
            // And each member on its own share, as before.
            over |= pooled > p.target.plus(p.overflow);
        }
        let at_hard = blocked
            || Class::ALL.iter().any(|&c| {
                eff.hard[c] != Bytes::MAX && self.ledger.gpu[c] >= eff.hard[c].minus(eff.reserve[c])
            });
        // The scale is kept in millionths of a thousandth, so no step's
        // rounding is lost: a rate is the same at 30 and at 120 fps.
        let r = self.cfg.relax;
        let one = 1_000u64 * 1_000_000;
        let max = u64::from(r.max_scale) * 1_000_000;
        if over || at_hard {
            let rate = u64::from(r.up_per_second) * if at_hard { 4 } else { 1 };
            self.scale_fine = self
                .scale_fine
                .saturating_add(rate.saturating_mul(dt))
                .min(max);
        } else {
            // Calm when what must be held — the protected bytes: drawn, in a
            // swap, the floor — fits under the low watermarks again. The
            // copies nobody draws are a cache the sweep and the admission
            // let go of as the detail asks for room: counting them kept a
            // class between its watermarks relaxed for ever (Laurent's stall,
            // 2026-11-23: drapes at 97–99 % of target, imagery under need,
            // nothing loading).
            let mut calm = Class::ALL
                .iter()
                .all(|&c| protected[c] <= eff.line(c) && self.ledger.gpu[c] <= eff.target[c]);
            if let Some(p) = eff.pool {
                let pooled = Class::ALL
                    .iter()
                    .filter(|c| p.members[**c])
                    .fold(Bytes::ZERO, |a, c| a.plus(protected[*c]));
                calm &= pooled <= p.target.minus(eff.pool_env(&p));
            }
            if calm {
                let down = u64::from(r.down_per_second).saturating_mul(dt);
                self.scale_fine = self.scale_fine.saturating_sub(down).max(one);
            }
        }
        // At most `max_scale`, a u32: `scale_fine` is capped at it.
        self.scale = u32::try_from(self.scale_fine / 1_000_000).unwrap_or(u32::MAX);
    }

    /// The prune: every step the kept levels of the tiles that left the
    /// drape needs or the held set; every eighth step, amortised, the empty
    /// slots, the expired evictions and the unreferenced tiles. Returns the
    /// entries looked at.
    fn prune(
        &mut self,
        now: Micros,
        tiles: &SortedMap<TileId, TileNeed>,
        named: &[super::need_state::Named],
        wants: Wants<'_>,
    ) -> u32 {
        let mut visited = self.prune_levels(tiles, named);
        if self.steps.is_multiple_of(8) {
            visited = visited.saturating_add(self.prune_tracked(now, tiles, wants));
        }
        visited.saturating_add(self.drain_recent(super::RECENT_DRAIN))
    }

    pub(super) fn release_beyond_impl(&mut self, target: Bytes) -> super::Released {
        let total = |l: &Ledger| {
            Class::ALL
                .iter()
                .fold(Bytes::ZERO, |a, &c| a.plus(l.gpu[c]))
        };
        // Whatever holds a copy — on the GPU, or only decoded (a copy an
        // earlier release or sweep demoted) — and was not needed last.
        let candidate = |b: &Budget, s: Slot| {
            let a = &b.arena;
            (a.has_gpu(s) || a.has_cpu(s))
                && !a.has_transit(s)
                && !a.marked(s, Mark::Wanted)
                && !b.is_floor(a.class(s), a.level(s))
        };
        let a = &self.arena;
        let mut grouped: Vec<(EvictKey, Slot)> = a
            .grouped_slots()
            .filter(|&s| candidate(self, s))
            .map(|s| (a.evict_key(s), s))
            .collect();
        // Keys end in the resource: total, so the slots' order is
        // forgotten; a slot listed twice comes out once.
        grouped.sort_unstable();
        grouped.dedup();
        let mut out = super::Released::default();
        let mut walk = Walk::default();
        while let Some(((.., r), s)) = walk.next(&self.arena, &grouped, |_| true, &Held::ALL) {
            if total(&self.ledger) <= target && self.ledger.decoded <= target {
                break;
            }
            if !candidate(self, s) {
                continue;
            }
            // Whole: both copies, in one place in the one order.
            if self.arena.take_gpu(s, &mut self.ledger).is_some() {
                out.gpu.push(r);
            }
            if self.arena.take_cpu(s, &mut self.ledger).is_some() {
                out.decoded.push(r);
            }
        }
        out
    }

    pub(super) fn release_source_impl(&mut self, tag: u8) -> super::Released {
        // Whatever holds a copy outside transit and was not wanted last.
        let a = &self.arena;
        let mut slots: Vec<(ResourceId, Slot)> = a
            .iter()
            .filter(|&s| {
                a.id(s).tile().tag() == tag
                    && (a.has_gpu(s) || a.has_cpu(s))
                    && !a.has_transit(s)
                    && !a.marked(s, Mark::Wanted)
            })
            .map(|s| (a.id(s), s))
            .collect();
        // By resource: the order the consumer is told in.
        slots.sort_unstable();
        let mut out = super::Released::default();
        for (r, s) in slots {
            if self.arena.take_gpu(s, &mut self.ledger).is_some() {
                out.gpu.push(r);
            }
            if self.arena.take_cpu(s, &mut self.ledger).is_some() {
                out.decoded.push(r);
            }
        }
        out
    }

    pub(super) fn view(&self, r: ResourceId) -> Option<ResourceView> {
        let a = &self.arena;
        let s = a.find(r)?;
        let cold = a.cold(s);
        let now = self.last_now.unwrap_or(Micros::ZERO);
        let load = a.load(s);
        let state = match load {
            Load::Loading { source, .. } => ResourceState::Loading { source },
            Load::Idle if a.has_transit(s) => ResourceState::InTransit,
            Load::Idle if a.has_gpu(s) => ResourceState::Drawable,
            Load::Idle if a.has_cpu(s) => ResourceState::Loaded,
            Load::Idle if cold.backing_off(now) => ResourceState::Failed {
                retries: cold.failures,
                next_at: cold.retry_at,
            },
            Load::Idle => ResourceState::Wanted,
        };
        let gen = match load {
            Load::Loading { gen, .. } => Some(gen),
            Load::Idle => a
                .transit(s)
                .map(|t| t.gen)
                .or(a.gpu(s).map(|g| g.gen))
                .or(a.cpu(s).map(|c| c.gen)),
        };
        let epoch = self.epoch(r, a.class(s));
        let tier = if a.has_gpu(s) {
            Tier::Gpu
        } else if a.fresh_cpu(s, epoch).is_some() {
            Tier::Decoded
        } else if cold.presence == Some(Presence::Stored) && !cold.store_missed {
            Tier::Stored
        } else {
            Tier::Remote
        };
        Some(ResourceView {
            state,
            tier,
            gen,
            gpu: a.gpu(s).map(|g| g.bytes).unwrap_or(Bytes::ZERO),
            decoded: a.cpu(s).map(|c| c.cost.transfer).unwrap_or(Bytes::ZERO),
            protected: self.protection_of(r, s),
            urgency: a.urgency(s),
            priority: a.priority(s).map(|p| p.key(r)),
        })
    }

    pub(super) fn audit_impl(&self) -> Result<(), AuditError> {
        if self.underflows > 0 {
            return Err(AuditError::SlotUnderflow {
                count: self.underflows,
            });
        }
        let mut l = Ledger::default();
        let mut flight = PerClass::splat(0u32);
        let a = &self.arena;
        debug_assert!(
            a.gpu_drapes_consistent(),
            "the ordered index of GPU drapes drifted from the slots"
        );
        debug_assert!(
            a.evict_index_consistent(),
            "the eviction index drifted from the slots"
        );
        debug_assert!(
            a.sums_consistent(),
            "the protected bytes or the GPU copies by epoch drifted from the slots"
        );
        debug_assert!(
            a.active_consistent(),
            "the active slots drifted from their places or their runs"
        );
        debug_assert!(
            a.extras_consistent(),
            "a decoded copy's cost extras outlive it, or name an identity cost"
        );
        for s in a.iter() {
            let class = a.class(s);
            if let Some(g) = a.gpu(s) {
                l.gpu[class] = l.gpu[class].plus(g.bytes);
            }
            if let Some(t) = a.transit(s) {
                l.gpu[class] = l.gpu[class].plus(t.held());
                l.in_transit = l.in_transit.plus(t.cost.staging);
            }
            if let Some(c) = a.cpu(s) {
                l.decoded = l.decoded.plus(c.cost.transfer);
            }
            if let Load::Loading {
                source: Source::Network,
                ..
            } = a.load(s)
            {
                flight[class] += 1;
            }
        }
        for c in Class::ALL {
            if l.gpu[c] != self.ledger.gpu[c] {
                return Err(AuditError::Gpu {
                    class: c,
                    ledger: self.ledger.gpu[c].0,
                    states: l.gpu[c].0,
                });
            }
            if flight[c] != self.in_flight[c] {
                return Err(AuditError::InFlight {
                    class: c,
                    ledger: self.in_flight[c],
                    states: flight[c],
                });
            }
        }
        if l.decoded != self.ledger.decoded {
            return Err(AuditError::Decoded {
                ledger: self.ledger.decoded.0,
                states: l.decoded.0,
            });
        }
        Ok(())
    }
}

impl Budget {
    /// The score of the GPU copy of `s` as the release key reads it, as a
    /// decision record of `side` for `reason`
    /// (`docs/explorations/budget-progressive-coarse-preload.md` §7). Read
    /// before the copy goes.
    pub(super) fn held_event(&self, s: Slot, side: Side, reason: Reason) -> ScoreEvent {
        let a = &self.arena;
        ScoreEvent {
            resource: a.id(s),
            class: a.class(s),
            level: a.level(s),
            band: u8::try_from(a.place(s).2).unwrap_or(u8::MAX),
            side,
            reason,
            terms: self.held_terms(s),
        }
    }

    /// The terms of the score of the copies `s` holds.
    pub(super) fn held_terms(&self, s: Slot) -> Terms {
        let a = &self.arena;
        let v = a.valuation();
        let (need, p, _) = a.place(s);
        let certain = super::value::CERTAIN;
        // `A` from the level and the need's level as the specification
        // weighs them now; `D` what the probability leaves of it.
        let pa = match need {
            Some(n) => {
                super::value::progressive_terms(
                    &self.spec.params.progressive,
                    a.level(s),
                    n,
                    0,
                    self.reach_tiles(),
                )
                .0
            }
            None => p,
        };
        let d = if pa == 0 {
            0
        } else {
            u32::try_from(u64::from(p) * u64::from(certain) / u64::from(pa))
                .unwrap_or(certain)
                .min(certain)
        };
        let at = level_index(a.level(s));
        let held = a.held_copies(s).unwrap_or(Held::Gpu);
        let bytes = match held {
            Held::Cpu => a.cpu(s).map_or(0, |c| c.cost.gpu.0),
            Held::Both | Held::Gpu => a.gpu(s).map_or(0, |g| g.bytes.0),
        };
        Terms::new(
            pa,
            d,
            p,
            u64::from(v.credit(a.class(s), at, held)),
            bytes,
            v.ref_bytes(),
        )
    }

    /// A preload started, as a decision record: its want's terms, `R` the
    /// restore cost of the tier it comes from, its bytes the reference size
    /// (a load does not know its own yet).
    pub(super) fn load_event(
        &self,
        r: ResourceId,
        class: Class,
        w: &Want,
        tier: Tier,
    ) -> ScoreEvent {
        let (pa, pd) = super::value::probability_terms(
            &self.spec,
            self.reach_tiles(),
            w.urgency,
            w.level,
            &w.q,
        );
        let p = if pd == super::value::CERTAIN {
            pa
        } else {
            super::value::product(pa, pd)
        };
        let ref_bytes = self.spec.params.ref_bytes.0.max(1);
        ScoreEvent {
            resource: r,
            class,
            level: w.level,
            band: u8::try_from(super::score::distance_band(w.q.away)).unwrap_or(u8::MAX),
            side: Side::Preload,
            reason: Reason::Preload,
            terms: Terms::new(
                pa,
                pd,
                p,
                self.costed.params.restore.of(tier).0,
                ref_bytes,
                ref_bytes,
            ),
        }
    }

    /// The score's parameters in force.
    pub(super) fn score_params(&self) -> super::score::ScoreParams {
        let p = &self.spec.params;
        super::score::ScoreParams {
            a0_permille: u32::from(p.progressive.a0_permille),
            descend_permille: u32::from(p.progressive.descend_permille),
            coarse_from: u32::from(p.progressive.coarse_from),
            coarse_boost_permille: u32::from(p.progressive.coarse_boost_permille),
            rho: self.reach_tiles(),
            margin_permille: p.margin_permille,
            floor_level: self.cfg.pinned_level,
            preload_slots_permille: u32::from(p.speculation.slots_permille),
            need_level: self.need_level,
            headroom_permille: u32::from(p.headroom_permille),
            floor_value_permille: u32::from(p.floor_value_permille),
            restore_us: {
                let r = &self.costed.params.restore;
                [r.decoded.0, r.stored.0, r.http_cache.0, r.remote.0]
            },
            restore_measured: {
                let s = &self.smoothed;
                let m = |t: &super::inputs::SmoothedTier| t.latency_us.get().is_some();
                [
                    false,
                    m(&s.store) || m(&s.http_cache),
                    m(&s.http_cache),
                    m(&s.network),
                ]
            },
        }
    }

    /// The step's score statistics read off the arena: the preloads in
    /// flight by level and band, the evictions of `scores`.
    pub(super) fn score_stats(&mut self, scores: &[ScoreEvent], st: &mut BudgetStats) {
        let rekeys = self.arena.take_rekeys();
        let places = std::mem::take(&mut self.places);
        let a = &self.arena;
        let mut out = super::score::ScoreStats {
            params: self.score_params(),
            places,
            rekeys,
            headroom_bytes: self.dynamic_headroom().0,
            ..Default::default()
        };
        for &s in a.active() {
            if a.loading(s) && a.urgency(s).is_some_and(|u| u >= Urgency::Ring) {
                let band = a.place(s).2.min(super::score::DISTANCE_BANDS - 1);
                out.preload_in_flight[level_index(a.level(s))][band] += 1;
            }
        }
        for e in scores {
            let band = usize::from(e.band).min(super::score::DISTANCE_BANDS - 1);
            if e.side == Side::Evict {
                out.evictions[level_index(e.level)][band] += 1;
            }
            if matches!(e.side, Side::Evict | Side::Decoded) {
                if let Some(i) = super::score::release_reason(e.reason) {
                    out.released[i][level_index(e.level)][band] += 1;
                }
            }
        }
        st.score = out;
    }

    /// [`Budget::held_report`]: the GPU copies by score, a walk of the
    /// arena — statistics, never a decision's work.
    pub(super) fn held_report_impl(&self) -> super::score::HeldReport {
        use super::score::{HeldReport, HeldState, HeldTile, HELD_LISTED};
        let a = &self.arena;
        let mut report = HeldReport {
            step: self.steps,
            ..Default::default()
        };
        let mut tiles: Vec<HeldTile> = Vec::new();
        for s in a.iter().filter(|&s| a.has_gpu(s)) {
            let terms = self.held_terms(s);
            let (_, _, band) = a.place(s);
            let level = a.level(s);
            report
                .board
                .put(level_index(level), band, Side::Held, &terms);
            let state = if self.is_floor(a.class(s), level) {
                HeldState::Floor
            } else if self.kept(s) {
                HeldState::Kept
            } else if a.marked(s, Mark::Wanted) {
                HeldState::Wanted
            } else {
                HeldState::Left
            };
            if state != HeldState::Floor {
                tiles.push(HeldTile {
                    resource: a.id(s),
                    class: a.class(s),
                    level,
                    band: u8::try_from(band).unwrap_or(u8::MAX),
                    terms,
                    state,
                });
            }
        }
        let key = |t: &HeldTile| (t.terms.score, t.resource);
        let n = HELD_LISTED.min(tiles.len());
        if n > 0 {
            if tiles.len() > n {
                tiles.select_nth_unstable_by_key(n - 1, key);
            }
            let mut low = tiles[..n].to_vec();
            low.sort_unstable_by_key(key);
            report.lowest = low;
            let len = tiles.len();
            if len > n {
                tiles.select_nth_unstable_by_key(len - n, key);
            }
            let mut high = tiles[len - n..].to_vec();
            high.sort_unstable_by_key(|t| std::cmp::Reverse(key(t)));
            report.highest = high;
        }
        report
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cost::Cost;
    use crate::{BudgetConfig, Need, NeedSet};

    #[test]
    fn a_slot_freed_twice_saturates_and_is_recorded() {
        let (mut n, mut under) = (2u32, 0u32);
        release(&mut n, &mut under);
        assert_eq!((n, under), (1, 0));
        release(&mut n, &mut under);
        release(&mut n, &mut under);
        assert_eq!((n, under), (0, 1), "saturates at 0, the underflow counted");
    }

    #[test]
    fn the_audit_reports_a_slot_underflow() {
        let mut b = Budget::new(BudgetConfig::interactive(Bytes::mib(64))).unwrap();
        assert_eq!(b.audit(), Ok(()));
        release(&mut b.local_reads, &mut b.underflows);
        assert_eq!(b.local_reads, 0);
        assert_eq!(b.audit(), Err(AuditError::SlotUnderflow { count: 1 }));
    }

    /// O1.2's debt, closed by the recycle: a stale GPU copy redelivered in
    /// a step whose room is built would leave the room, its bytes still
    /// counted as evictable while on their way, and a later `make_room` of
    /// the step would free bytes that are not there. Since an epoch's
    /// recycle evicts every stale copy the room can count before the
    /// deliveries, it never happens. Here a ring resident `r` and an idle
    /// one `v`, both stale after an epoch, are evicted on the epoch's step,
    /// and `r` is delivered again fresh. Without the recycle this is the
    /// old scenario — `r1`'s delivery builds the room evicting `v`, then
    /// `r`'s stale copy is delivered again — and `deliver_one`'s debug
    /// assertion fires on `r`.
    #[test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "the room is checked by a debug assertion"
    )]
    fn a_stale_copy_never_reaches_the_steps_room() {
        let t3d = Class::Tiles3dMesh;
        let mut cfg = BudgetConfig::interactive(Bytes::mib(64));
        cfg.pinned_level = None;
        cfg.target[t3d] = Bytes::mib(4);
        cfg.hard_limit[t3d] = Bytes::mib(6);
        let mut b = Budget::new(cfg).unwrap();
        // The recycle, not the envelope: the present need's reserve follows
        // its pending bytes, so the ring is not held back after `v`.
        let mut spec = crate::spec::Specification::interactive();
        spec.params.envelope.fall_permille = 1_000;
        b.specify(spec).unwrap();
        let tile = |x| TileId::from_terrain(1, x, 0);
        let (v, r, r1) = (tile(0), tile(1), tile(2));
        let want = |t: TileId, sse: f32| (t, Need::new(1, t3d).with_sse_excess(sse));
        let need = |drawn: &[TileId], ring: &[(TileId, f32)]| NeedSet {
            drawn: drawn.iter().map(|t| want(*t, 1.0)).collect(),
            ring: ring.iter().map(|(t, s)| want(*t, *s)).collect(),
            ..NeedSet::default()
        };
        let mut now = Micros::ZERO;
        let mut step = |b: &mut Budget, n: &NeedSet| {
            now = now.plus(Micros(16_667));
            b.decide(crate::FrameInput::new(0, now, n))
        };
        let load = |b: &mut Budget, d: &Decisions, t: TileId, mib: u64| {
            let s = d.start.iter().find(|s| s.resource == ResourceId::Mesh(t));
            let s = s.unwrap_or_else(|| panic!("{t:?} not started: {d:?}"));
            b.on_loaded(s.resource, s.gen, Cost::flat(Bytes::mib(mib)));
        };
        let ack = |b: &mut Budget, d: &Decisions, t: TileId, mib: u64| {
            let (res, g) = d
                .deliver
                .iter()
                .find(|x| x.0 == ResourceId::Mesh(t))
                .unwrap();
            b.on_drawable(*res, *g, Bytes::mib(mib));
        };
        // `r` resident from the ring, then `v` drawn beside it, then left.
        let ring = need(&[], &[(r, 0.0)]);
        let d = step(&mut b, &ring);
        load(&mut b, &d, r, 1);
        let d = step(&mut b, &ring);
        ack(&mut b, &d, r, 1);
        let with_v = need(&[v], &[(r, 0.0)]);
        let d = step(&mut b, &with_v);
        load(&mut b, &d, v, 3);
        let d = step(&mut b, &with_v);
        ack(&mut b, &d, v, 3);
        step(&mut b, &ring);
        step(&mut b, &ring);
        // A new epoch: the copies of `r` (ring) and `v` (left) are stale,
        // and neither is drawn: both recycled on the step that sees the
        // epoch; `r` and a fresher `r1` load.
        b.new_epoch(t3d, None);
        let both = need(&[], &[(r, 0.0), (r1, 3.0)]);
        let d = step(&mut b, &both);
        assert!(d.evict.contains(&ResourceId::Mesh(r)), "{d:?}");
        assert!(d.evict.contains(&ResourceId::Mesh(v)), "{d:?}");
        assert_eq!(d.stats.recycled[t3d][1], 2);
        load(&mut b, &d, r, 1);
        load(&mut b, &d, r1, 1);
        // Both delivered, `r` fresh: no stale copy of it left to be on its
        // way again (`deliver_one`'s assertion holds).
        let d = step(&mut b, &both);
        let mut delivered: Vec<_> = d.deliver.iter().map(|x| x.0).collect();
        delivered.sort_unstable();
        assert_eq!(delivered, [ResourceId::Mesh(r), ResourceId::Mesh(r1)]);
        assert!(b
            .resource(ResourceId::Mesh(r))
            .is_some_and(|x| x.gpu == Bytes::ZERO));
    }
}
