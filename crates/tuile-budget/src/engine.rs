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
use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

use super::config::FailurePolicy;
use super::cost::within_drift;
use super::decisions::{Decisions, DrapeChoice, DrawItem, Role, Start, UploadPlan};
use super::downstream::{Item, Wants};
use super::error::{AuditError, SessionError};
use super::imagery::{drape_rank, needed_level, DrapeCandidate, GeoRect, LevelClamp, UvTransform};
use super::need::{LayerReach, Membership};
use super::need_state::{NeedState, TileNeed, WantedImagery};
use super::priority::{score, PriorityKey, Quantised, Urgency};
use super::sorted::SortedMap;
use super::state::{Arena, CpuCopy, EvictKey, Gpu, Held, Load, Mark, Rank, Slot, Transit};
use super::stats::{BudgetStats, CancelReason, EvictReason, LATENCY_BUCKETS};
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
    batch: bool,
    /// The pooled ledger's numbers (D-A3); `None` with no member, and
    /// under Batch.
    pool: Option<PoolEff>,
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
    /// Drawn by nothing.
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
    /// Its tile's level, for the imagery-over-terrain histogram.
    pub tile_level: u32,
}

/// A step's buffers, kept across steps so that a step allocates nothing
/// once they have grown to the view.
#[derive(Debug, Default)]
pub(super) struct Scratch {
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

/// The floor swap group `r` belongs to (D-A1), if any: a floor tile's mesh
/// and its floor drape of every layer it wears, in stacking order.
fn floor_group<'t>(
    tiles: &'t SortedMap<TileId, TileNeed>,
    r: ResourceId,
    is_floor: impl Fn(u32) -> bool,
) -> Option<impl Iterator<Item = ResourceId> + Clone + 't> {
    let t = r.tile();
    let tn = tiles.get(&t)?;
    let img = tn.need.imagery.as_ref()?;
    let level = tn.need.level;
    if !is_floor(level) {
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

    pub(super) fn is_floor(&self, level: u32) -> bool {
        self.cfg.pinned_level.is_some_and(|p| level <= p)
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
        Eff {
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
            decoded: scale(self.cfg.decoded_target, pct),
            quota: PerClass::from_fn(|c| (self.cfg.max_in_flight[c] / thermal_div).max(1)),
            local: self.cfg.max_local_reads,
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
                            if self.cfg.policy == Policy::Batch {
                                self.fatal.get_or_insert(SessionError::LoadFailed(r));
                            } else {
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
                        st.stale += 1;
                        continue;
                    };
                    let a = &mut self.arena;
                    if a.transit(s).is_some_and(|t| t.gen == g) {
                        if let Some(t) = a.take_transit(s, &mut self.ledger) {
                            if !within_drift(t.cost.gpu, actual) {
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
                    } else if !a.has_transit(s)
                        && !a.has_gpu(s)
                        && a.cpu(s).is_some_and(|c| c.gen == g)
                    {
                        // Uploaded although its delivery was withdrawn: the
                        // GPU holds it, so the ledger does.
                        let epoch = a.cpu(s).map(|c| c.epoch).unwrap_or(0);
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

    /// Whether `tn` asks for drapes this step: drawn or blocking, with
    /// imagery.
    pub(super) fn draped(tn: &TileNeed) -> bool {
        matches!(tn.membership, Membership::Drawn | Membership::Blocker)
            && tn.need.imagery.is_some()
    }

    /// The imagery level of each layer tile `t` asks for this step, into
    /// `out` in layer order, a layer listed twice at its last listing's
    /// level. `asked` is set when the presence index was asked: the levels
    /// then hold for this step only.
    ///
    /// The level follows the texel footprint (relaxed with the LOD, with
    /// hysteresis). When that level is held nowhere locally, a level already
    /// held (GPU, decoded, store) is taken instead if it lies within one
    /// level of the window between the relaxed and the unrelaxed need: a
    /// relaxation never sends to the network what a local tier can show
    /// (I14, I15). Batch takes the exact level, a function of the frame.
    pub(super) fn drape_levels(
        &self,
        t: TileId,
        tn: &TileNeed,
        presence: &dyn PresenceOracle,
        last: &BTreeMap<(TileId, LayerId), u32>,
        out: &mut Vec<(LayerId, u32)>,
        asked: &mut bool,
    ) {
        out.clear();
        let Some(img) = tn.need.imagery.as_ref() else {
            return;
        };
        let floor = self.cfg.pinned_level.unwrap_or(0);
        let batch = self.cfg.policy == Policy::Batch;
        let cap = self.cfg.imagery_cap();
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
                out.push((reach.layer, needed_level(None, img.footprint, clamp)));
                continue;
            }
            let prev = last.get(&key).copied();
            let relaxed = needed_level(prev, img.footprint.relaxed(self.scale), clamp);
            let mut level = relaxed;
            let tier = |r: ResourceId, asked: &mut bool| {
                self.tier_asked(r, self.arena.find(r), Class::Drape, presence, asked)
            };
            if tier(drape(relaxed), asked) == Tier::Remote {
                let full = needed_level(prev, img.footprint, clamp);
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
            out.push((reach.layer, level));
        }
        // The layers in order, a layer listed twice in the order listed
        // (a stable sort); then the last listing's level.
        out.sort_by_key(|e| e.0);
        out.dedup_by(|later, earlier| {
            let same = later.0 == earlier.0;
            if same {
                earlier.1 = later.1;
            }
            same
        });
    }

    /// Tile `t`'s drape needs this step — each layer it asks for, in layer
    /// order, at the level chosen — when it asks for drapes; into `out`.
    pub(super) fn drapes_of(&self, t: TileId, tn: &TileNeed, out: &mut Vec<(LayerId, u32)>) {
        out.clear();
        if !Self::draped(tn) {
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
            *level = self.imagery_prev.get(&(t, *layer)).copied().unwrap_or(0);
        }
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

    /// The drawable drapes of `layer` on `x`, with their levels.
    fn drawable_drapes(
        &self,
        x: TileId,
        layer: LayerId,
    ) -> impl Iterator<Item = (ResourceId, u32)> + '_ {
        // Only this layer's drapes: a layer never falls back on another's.
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
                uv: UvTransform::IDENTITY,
            });
        }
        let rect = self.tiles.get(&tile).and_then(|i| i.rect());
        // `best_drape`'s fold, walked lazily: the first of equal ranks wins.
        let mut best: Option<(DrapeCandidate, ResourceId, UvTransform)> = None;
        for (up, x) in std::iter::once(tile)
            .chain(self.ancestors(tile))
            .enumerate()
        {
            reach.saw(x, PLACE | DRAPES);
            let Some(uv) = self.uv_into(rect, up, x) else {
                continue;
            };
            for (r, level) in self.drawable_drapes(x, layer) {
                let c = DrapeCandidate {
                    level,
                    up: sat_u32(up),
                };
                if best.is_none_or(|(b, ..)| drape_rank(need, c) < drape_rank(need, b)) {
                    best = Some((c, r, uv));
                }
            }
            // A drape at exactly the need: nothing farther up ranks before
            // it (the closer owner wins a tie), so the walk ends — the same
            // answer, fewer ancestors read.
            if best.is_some_and(|(b, ..)| b.level == need) {
                break;
            }
        }
        best.map(|(c, resource, uv)| DrapeChoice {
            resource,
            layer,
            level: c.level,
            uv,
        })
    }

    /// Whether `tile` has any drawable drape of `layer`, on itself or an
    /// ancestor it maps into: `best_drape_for(tile, layer, _).is_some()`,
    /// stopping at the first found. `reach` gets the tile and the ancestors
    /// read when the tile's own drapes did not answer.
    fn has_drape(&self, tile: TileId, layer: LayerId, reach: &mut Reach) -> bool {
        // Its own first: they need no mapping.
        if self.drawable_drapes(tile, layer).next().is_some() {
            return true;
        }
        let rect = self.tiles.get(&tile).and_then(|i| i.rect());
        self.ancestors(tile).enumerate().any(|(up, x)| {
            reach.saw(x, PLACE | DRAPES);
            self.drawable_drapes(x, layer).next().is_some()
                && self.uv_into(rect, up + 1, x).is_some()
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
    /// `layers`; else the finer tiles it held last step (zoom-out); else its
    /// nearest drawable, dressed ancestor. `reach` gets the tiles read
    /// beyond the tile's own mesh and drapes (the held tiles below it, read
    /// from the ancestry, are its own: a change to them reads it again).
    pub(super) fn select_tile(
        &self,
        t: TileId,
        layers: &[LayerId],
        cap: Option<u32>,
        out: &mut Vec<(ResourceId, Role)>,
        reach: &mut Reach,
    ) -> TileSelection {
        out.clear();
        let mut sel = TileSelection::default();
        let shown = |x: TileId, reach: &mut Reach| {
            reach.saw(x, PLACE | MESH | DRAPES);
            let level = self.tiles.get(&x).map_or(0, |i| i.level);
            cap.is_none_or(|c| level <= c)
                && self.drawable(ResourceId::Mesh(x))
                && self.dressable(x, layers, reach)
        };
        if shown(t, reach) {
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
            sel.lost = !held;
        }
        // By mesh: an entry once (a cycle could name a tile twice).
        out.sort_unstable();
        out.dedup();
        sel
    }

    /// The draw item of `mesh`, drawn for `run` — (tile served, role) by
    /// tile, at least one — into `out` (its buffers reused), with what it
    /// adds to the step's statistics. `drapes` gives a served tile's drape
    /// needs. `reach` gets what it read beyond its own tile's drapes at the
    /// levels needed (an ancestor's drapes; its own last item, in a layer
    /// switch).
    pub(super) fn build_item(
        &self,
        mesh: ResourceId,
        run: &[(TileId, Role)],
        drapes: impl Fn(TileId, &mut Vec<(LayerId, u32)>),
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
        for s in &out.serves {
            drapes(*s, own);
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
                    *n = *p;
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
        // Bare: a layer the tiles need is worn by no drape, and no old
        // layer stands in for it during a switch.
        let missing = needs
            .iter()
            .any(|(l, _)| !chosen.iter().any(|d| d.layer == *l));
        let switching = chosen.iter().any(|d| !needed(d.layer));
        let mut stats = ItemStats {
            bare: missing && !switching,
            underdraped: false,
            tile_level: self.tiles.get(&tile).map(|i| i.level).unwrap_or(0),
        };
        if role == Role::Exact {
            stats.underdraped = needs.iter().any(|(layer, n)| {
                !chosen
                    .iter()
                    .any(|d| d.layer == *layer && d.level.abs_diff(*n) <= 1)
            });
        }
        stats
    }

    /// What tile `t` puts in the wants, into `out` merged — per resource the
    /// most urgent, then the best scored; of equals, the first put — in
    /// resource order. `item` is the draw item of its mesh, `meshes_in`
    /// whether Batch may want its drapes (always under Interactive).
    pub(super) fn tile_wants(
        &self,
        t: TileId,
        tn: &TileNeed,
        item: Option<&Item>,
        meshes_in: bool,
        drapes: &mut Vec<(LayerId, u32)>,
        out: &mut Vec<(ResourceId, u32, Want)>,
    ) {
        out.clear();
        let batch = self.cfg.policy == Policy::Batch;
        // Every want put, in order; merged below.
        let put = |out: &mut Vec<(ResourceId, u32, Want)>, r: ResourceId, w: Want| {
            let seq = u32::try_from(out.len()).unwrap_or(u32::MAX);
            out.push((r, seq, w));
        };
        let floor = self.is_floor(tn.need.level);
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
                    _ => {
                        let worn = item.and_then(|i| i.drape(layer).copied());
                        let off = match worn {
                            None => true,
                            Some(w) if batch => w.resource != drape,
                            Some(w) => {
                                w.level.abs_diff(n) > 1
                                    || (self.pressure != Pressure::Normal && w.level > n)
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
        // Per resource the most urgent, then the best scored; of equals,
        // the first put.
        out.sort_unstable_by_key(|(r, seq, w)| (*r, w.urgency, Reverse(w.q), *seq));
        out.dedup_by_key(|e| e.0);
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
        if self.is_floor(a.level(s)) && (a.has_gpu(s) || a.has_transit(s)) {
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
    /// retry's) that will change that.
    pub(super) fn job_of(
        &self,
        r: ResourceId,
        w: &Want,
        now: Micros,
    ) -> (Option<Job>, Option<Micros>) {
        let class = self.class_of(r, w.class);
        let epoch = self.epoch(r, class);
        let a = &self.arena;
        match a.find(r) {
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
        let class = self.class_of(r, w.class);
        let epoch = self.epoch(r, class);
        let a = &self.arena;
        let e = a.find(r);
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
        let mut sc = score(w.q, waited, self.cfg.age_full, cover);
        if urgency == Urgency::Hole && coarsening(r.tile()) {
            // At the ceiling a coarser covering tile frees bytes: first.
            sc += 1_000_000;
        }
        PriorityKey {
            urgency,
            score: Reverse(sc),
            tier: self.tier(r, e, class, presence),
            resource: r,
        }
    }

    /// The step, into `d` (emptied by the caller, its buffers kept).
    pub(super) fn step(&mut self, input: FrameInput<'_>, d: &mut Decisions) {
        let now = input.now;
        let mut st = std::mem::take(&mut self.pending);
        let eff = self.eff();
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

        self.apply_events(now, &mut st);
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

        // 5. Cancellations: grace and timeouts.
        self.cancel(now, &eff, d, &mut st);

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
            let (Some(job), _) = self.job_of(r, w, now) else {
                continue;
            };
            let key = self.key_of(r, w, now, &is_coarsening, input.presence);
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

        let mut picked = std::mem::take(&mut self.scratch.picked);
        picked.clear();
        // Places: a `u32` each, as many as the jobs.
        let place = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
        picked.extend(
            jobs.iter()
                .enumerate()
                .filter(|(_, j)| j.1 == Job::Load)
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

        // 9. Eviction and release.
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
        self.put_back(taken);
        self.need = need;

        // Statistics.
        st.gpu = self.ledger.gpu;
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
                    st.transit[c] = st.transit[c].plus(t.cost.gpu);
                }
            }
        }
        st.protected = protected;
        // What keeping the kept maps in order cost, this step and the
        // events applied since the last.
        self.walk_upkeep();
        st.work = st.work.saturating_add(self.scratch.walked.swap(0, Relaxed));
        d.stats = st;
    }

    fn protected_bytes(&self) -> PerClass<Bytes> {
        self.arena.protected()
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
            due.retain(|r| self.active.contains(r));
            if self.down.deadlines_len() > 2 * self.active.len() + 64 {
                // Mostly deadlines of loads gone: those of the loads still
                // going, again.
                self.down.clear_deadlines();
                for &r in &self.active {
                    let Some(s) = self.arena.find(r) else {
                        continue;
                    };
                    if let Load::Loading { started, .. } = self.arena.load(s) {
                        self.down.deadline(started.plus(timeout), r);
                    }
                    if let Some(u) = self.arena.cold(s).unneeded_since() {
                        self.down.deadline(u.plus(grace), r);
                    }
                }
                self.down.trim_deadlines();
            }
        } else {
            due.clear();
            due.extend(self.active.iter().copied());
            if !back {
                // The deadlines were dropped: every load's, again.
                for r in &due {
                    let Some(s) = self.arena.find(*r) else {
                        continue;
                    };
                    if let Load::Loading { started, .. } = self.arena.load(s) {
                        self.down.deadline(started.plus(timeout), *r);
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
                let timed_out = now.since(started) >= timeout;
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
        for (key, _, w) in loads.iter().filter_map(|i| jobs.get(*i as usize)) {
            let r = &key.resource;
            let class = self.class_of(*r, w.class);
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
                    if self.in_flight[class] >= eff.quota[class] {
                        if w.urgency > Urgency::Hole {
                            if key.tier == Tier::Stored {
                                skipped_stored += 1;
                            }
                            continue;
                        }
                        // Preempt the least urgent in-flight preload. Every
                        // load is indexed in `active` (`start`).
                        st.work = st.work.saturating_add(sat_u32(self.active.len()));
                        let victim = self
                            .active
                            .iter()
                            .filter(|vr| {
                                let a = &self.arena;
                                a.find(**vr).is_some_and(|vs| {
                                    a.class(vs) == class
                                        && matches!(
                                            a.load(vs),
                                            Load::Loading {
                                                source: Source::Network,
                                                ..
                                            }
                                        )
                                        && (wants.get(vr).is_some_and(|vw| {
                                            matches!(vw.urgency, Urgency::Ring | Urgency::Ancestor)
                                        }) || a.urgency(vs) == Some(Urgency::Refresh))
                                })
                            })
                            .filter_map(|vr| key_now(self, *vr).map(|k| (k, *vr)))
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
            if self.cfg.pinned_level.is_some_and(|p| w.level <= p) {
                self.arena.mark(s, Mark::FloorSet, true);
            }
            self.active.insert(*r);
            self.arena.mark(s, Mark::Active, true);
            self.deadline(now.plus(self.cfg.load_timeout), *r);
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
                if now.since(t) <= THRASH_WINDOW {
                    // Back from a local store is a demotion paying off; back
                    // from the network is thrash.
                    match source {
                        Source::Network => st.thrash += 1,
                        Source::Local => st.rereads += 1,
                    }
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
                        && !self.is_floor(a.level(s))
                })
                .filter_map(|s| victim(self, &self.arena.id(s), s))
                .fold(Bytes::ZERO, |a, b| a.plus(b)),
            "the bytes counted evictable are those a scan finds"
        );
        if self.ledger.gpu[class].minus(freeable).plus(bytes) > limit {
            *strained = true;
            self.scratch.room = Some(room);
            return false;
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
            if self.kept(s) || self.is_floor(self.arena.level(s)) {
                continue;
            }
            let Some(b) = victim(self, &r, s) else {
                continue;
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
            self.arena.take_gpu(s, &mut self.ledger);
            d.evict.push(r);
            st.evicted += 1;
            st.evictions[class][EvictReason::Admission as usize] += 1;
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
                a.has_gpu(s) && !a.has_transit(s) && !self.kept(s) && !self.is_floor(a.level(s))
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
            let Some(group) = floor_group(tiles, *r, |l| self.is_floor(l)) else {
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
            let preload = w.urgency >= Urgency::Ring;
            let mut limit = if shrinking {
                eff.hard[class]
            } else if urgent {
                eff.hard[class].minus(eff.reserve[class])
            } else if preload {
                // Preload only into room the view does not use, and it
                // never displaces anything wanted.
                eff.target[class].scaled(u64::from(self.cfg.watermarks.low_percent), 100)
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
                    p.target
                        .scaled(u64::from(self.cfg.watermarks.low_percent), 100)
                } else {
                    p.target.min(p.hard.minus(reserve))
                };
                limit = limit.min(whole.minus(p.others(class, &self.ledger.gpu)));
            }
            if preload && (*strained || *blocked) {
                return;
            }
            let mut strain = false;
            let ok = self.make_room(
                now,
                class,
                c.cost.gpu,
                limit,
                w.urgency,
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
        // A stale GPU copy on its way again leaves the step's room.
        let in_room = self.scratch.room.is_some() && {
            let a = &self.arena;
            a.grouped(s) && a.has_gpu(s) && !self.kept(s) && !self.is_floor(a.level(s))
        };
        if let (true, Some(room), Some(g)) =
            (in_room, self.scratch.room.as_mut(), self.arena.gpu(s))
        {
            let i = want_bucket(wants.get(&r));
            room.bytes[class][i] = room.bytes[class][i].minus(g.bytes);
        }
        // `make_room` frees no slot: `s` is still `r`'s.
        self.active.insert(r);
        self.arena.mark(s, Mark::Active, true);
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
        }
        st.delivered += 1;
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
                urgency: Urgency::Refresh,
                score: Reverse(i64::MIN),
                tier: Tier::Remote,
                resource: r,
            });
            transit.push((k, t.cost.gpu, group_of(r), false));
        }
        // Keys end in the resource: total, so unstable is stable.
        transit.sort_unstable_by_key(|a| a.0);
        // Each group's members as a run, in transit order within the group:
        // the places of `transit`, by (group, place).
        by_group.clear();
        by_group.extend((0..transit.len()).map(|i| u32::try_from(i).unwrap_or(u32::MAX)));
        by_group.sort_unstable_by_key(|&i| (transit[i as usize].2, i));
        let mut groups = 0;
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
            if plan.items.is_empty() && plan.bytes.plus(bytes) > eff.upload {
                // The first item always goes, alone if its group is too big.
                plan.items.push(k.resource);
                plan.bytes = b;
                break;
            }
            if plan.bytes.plus(bytes) > eff.upload {
                break;
            }
            plan.items
                .extend(members.iter().map(|&j| transit[j as usize].0.resource));
            plan.bytes = plan.bytes.plus(bytes);
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
        let w = self.cfg.watermarks;
        if let Some(p) = eff.pool {
            self.evict_pooled(now, p, wants, d, st);
        }
        for class in Class::ALL {
            if eff.pool.is_some_and(|p| p.members[class]) {
                continue;
            }
            let used = self.ledger.gpu[class];
            let high = eff.target[class].scaled(u64::from(w.high_percent), 100);
            if used <= high {
                continue;
            }
            let low = eff.target[class].scaled(u64::from(w.low_percent), 100);
            let room = match self.scratch.room.take() {
                Some(room) => room,
                None => self.room(wants, st),
            };
            let mut walk = Walk::default();
            while let Some((_, s)) =
                walk.next(&self.arena, &room.grouped, |c| c == class, &Held::GPU)
            {
                if self.ledger.gpu[class] <= low {
                    break;
                }
                st.work = st.work.saturating_add(1);
                // The order holds neither the kept set nor the floor.
                if self.kept(s) || self.is_floor(self.arena.level(s)) {
                    continue;
                }
                // Still on the GPU, and not on its way again.
                let evictable = self.arena.has_gpu(s) && !self.arena.has_transit(s);
                if !evictable {
                    continue;
                }
                self.evict_one(now, s, class, d, st);
            }
            self.scratch.room = Some(room);
        }
        // Only a step under pressure built one: nothing kept across steps.
        self.scratch.room = None;
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

    /// Releases the decoded copy of `s`, and counts it.
    fn release_one(&mut self, s: Slot, d: &mut Decisions, st: &mut BudgetStats) {
        self.arena.take_cpu(s, &mut self.ledger);
        d.release.push(self.arena.id(s));
        st.released += 1;
        st.releases[self.arena.class(s)] += 1;
    }

    /// D-A3: one sweep over the pool's members, in the one eviction order,
    /// down to the low watermark of the pool's target, no member below its
    /// floor except to keep I2, none left above its ceiling.
    fn evict_pooled(
        &mut self,
        now: Micros,
        p: PoolEff,
        wants: Wants<'_>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let w = self.cfg.watermarks;
        let used = |l: &Ledger| {
            Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .fold(Bytes::ZERO, |a, c| a.plus(l.gpu[*c]))
        };
        let pct = |b: Bytes, percent: u8| b.scaled(u64::from(percent), 100);
        let floor = |c: Class| p.floor(c, p.target);
        // A member over its own ceiling (its own high watermark) is trimmed
        // to its own low one, as a class outside the pool is.
        let ceiling_high = |c: Class| pct(p.ceil_target[c], w.high_percent);
        let ceiling_low = |c: Class| pct(p.ceil_target[c], w.low_percent);
        let mut over: PerClass<bool> =
            PerClass::from_fn(|c| p.members[c] && self.ledger.gpu[c] > ceiling_high(c));
        let high = pct(p.target, w.high_percent);
        let pool_over = used(&self.ledger) > high;
        if !pool_over && Class::ALL.iter().all(|c| !over[*c]) {
            return;
        }
        let low = pct(p.target, w.low_percent);
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
            a.has_gpu(s) && !a.has_transit(s) && !b.kept(s) && !b.is_floor(a.level(s))
        };
        let mut walk = Walk::default();
        while let Some((_, s)) = walk.next(&self.arena, &room.grouped, member, &Held::GPU) {
            st.work = st.work.saturating_add(1);
            if !candidate(self, s) {
                continue;
            }
            let (class, bytes) = (
                self.arena.class(s),
                self.arena.gpu(s).map_or(Bytes::ZERO, |g| g.bytes),
            );
            for c in Class::ALL {
                over[c] &= self.ledger.gpu[c] > ceiling_low(c);
            }
            let pool_trim = pool_over && used(&self.ledger) > low;
            if !pool_trim && Class::ALL.iter().all(|c| !over[*c]) {
                break;
            }
            // Over its ceiling it goes; for the pool's sake, only down to
            // its floor.
            let goes =
                over[class] || (pool_trim && self.ledger.gpu[class].minus(bytes) >= floor(class));
            if goes {
                self.evict_one(now, s, class, d, st);
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
                self.evict_one(now, s, class, d, st);
            }
        }
        self.scratch.room = Some(room);
    }

    /// Evicts the resource of `s`, of `class`, from the GPU, and counts it.
    fn evict_one(
        &mut self,
        now: Micros,
        s: Slot,
        class: Class,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let r = self.arena.id(s);
        self.arena.take_gpu(s, &mut self.ledger);
        d.evict.push(r);
        st.evicted += 1;
        let why = if self.pressure == Pressure::Normal {
            EvictReason::Sweep
        } else {
            EvictReason::Pressure
        };
        st.evictions[class][why as usize] += 1;
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
            let calm = Class::ALL.iter().all(|&c| {
                self.ledger.gpu[c]
                    <= eff.target[c].scaled(u64::from(self.cfg.watermarks.low_percent), 100)
            });
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
        visited
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
                && !b.is_floor(a.level(s))
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
            decoded: a.cpu(s).map(|c| c.cost.gpu).unwrap_or(Bytes::ZERO),
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
        for s in a.iter() {
            let class = a.class(s);
            if let Some(g) = a.gpu(s) {
                l.gpu[class] = l.gpu[class].plus(g.bytes);
            }
            if let Some(t) = a.transit(s) {
                l.gpu[class] = l.gpu[class].plus(t.cost.gpu);
                l.in_transit = l.in_transit.plus(t.cost.staging);
            }
            if let Some(c) = a.cpu(s) {
                l.decoded = l.decoded.plus(c.cost.gpu);
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

    /// O1.2's debt: a stale GPU copy redelivered in a step whose room is
    /// built leaves the room, its bytes taken off its bucket. Here a ring
    /// resident `r`, stale after an epoch, is redelivered after a fresher
    /// ring tile's delivery built the room (evicting an idle resident):
    /// without the bucket's update, the room counts `r`'s bytes as
    /// evictable while they are on their way, and a later `make_room` of
    /// the step would free bytes that are not there. The check is
    /// `evict`'s debug assertion of the room against a recount.
    #[test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "the room is checked by a debug assertion"
    )]
    fn a_stale_copy_redelivered_leaves_the_steps_room() {
        let t3d = Class::Tiles3dMesh;
        let mut cfg = BudgetConfig::interactive(Bytes::mib(64));
        cfg.pinned_level = None;
        cfg.target[t3d] = Bytes::mib(4);
        cfg.hard_limit[t3d] = Bytes::mib(6);
        let mut b = Budget::new(cfg).unwrap();
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
        // A new epoch: `r`'s copy is stale; `r` and a fresher `r1` load.
        b.new_epoch(t3d, None);
        let both = need(&[], &[(r, 0.0), (r1, 3.0)]);
        let d = step(&mut b, &both);
        load(&mut b, &d, r, 1);
        load(&mut b, &d, r1, 1);
        let d = step(&mut b, &both);
        // `r1` first: room built, `v` evicted; then `r`, redelivered.
        assert!(d.evict.contains(&ResourceId::Mesh(v)), "{d:?}");
        let order: Vec<_> = d.deliver.iter().map(|x| x.0).collect();
        assert_eq!(order, [ResourceId::Mesh(r1), ResourceId::Mesh(r)]);
        // `evict` asserts the room against a recount, which counted `r`'s
        // stale copy, on its way again, without the update.
    }
}
