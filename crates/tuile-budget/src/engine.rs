// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One decision step, in order:
//!
//! 1. apply the recorded arrivals, in a canonical order;
//! 2. read the need: quantise, merge duplicates, choose each tile's imagery
//!    level (with hysteresis, Interactive only);
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
use std::collections::{BTreeMap, BTreeSet};

use super::config::FailurePolicy;
use super::cost::within_drift;
use super::decisions::{
    Decisions, DrapeChoice, DrawItem, DrawSet, Role, Start, SwapGroup, UploadPlan,
};
use super::error::{AuditError, SessionError};
use super::imagery::{drape_rank, needed_level, DrapeCandidate, GeoRect, LevelClamp, UvTransform};
use super::need::{ImageryNeed, LayerReach, Membership, Need};
use super::priority::{score, PriorityKey, Quantised, Urgency};
use super::sorted::{recycle, union4, SortedMap};
use super::state::{Arena, CpuCopy, EvictKey, Gpu, Held, Load, Rank, Slot, Transit};
use super::stats::{BudgetStats, CancelReason, EvictReason, GAP_BUCKETS, LATENCY_BUCKETS};
use super::tier::{Presence, PresenceOracle, Source, Tier};
use super::types::{
    sat_u32, Bytes, Class, Failure, Generation, LayerId, Micros, PerClass, Policy, Pressure,
    ResourceId, Thermal, TileId,
};
use super::view::{Protection, ResourceState, ResourceView};
use super::{Budget, Event, FrameInput, Ledger, TileInfo, RECENT_CAP, THRASH_WINDOW};

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
#[derive(Clone, Copy, Debug)]
struct Want {
    urgency: Urgency,
    q: Quantised,
    class: Class,
    level: u32,
}

/// One wanted tile's need, merged: borrowed from the step's input.
#[derive(Clone, Copy, Debug)]
struct TileNeed<'a> {
    membership: Membership,
    need: &'a Need,
    q: Quantised,
    /// Its place in the need's entries: of equal entries, the first wins.
    seq: u32,
}

/// A step's buffers, kept across steps so that a step allocates nothing
/// once they have grown to the view.
#[derive(Debug, Default)]
pub(super) struct Scratch {
    tiles: Vec<(TileId, TileNeed<'static>)>,
    drapes: Vec<((TileId, LayerId), u32)>,
    /// Wants before their merge: (resource, order put, want).
    wants_raw: Vec<(ResourceId, u32, Want)>,
    wants: Vec<(ResourceId, Want)>,
    keys: Vec<(ResourceId, PriorityKey)>,
    ancestry: Vec<(TileId, TileId, u32)>,
    jobs: Vec<(PriorityKey, ResourceId, Job, Want)>,
    /// This step's room: built at its first use, dropped with the step's
    /// evictions. Only a step under pressure builds it.
    room: Option<Room>,
    /// The loads, then the deliveries, picked from `jobs`.
    picked: Vec<(PriorityKey, ResourceId, Job, Want)>,
}

/// The meshes standing for the wanted tiles, before their drapes.
#[derive(Default)]
struct Selection {
    /// (mesh, a tile it serves, the role it serves it in), unsorted: a mesh
    /// is drawn in the least of its roles, for every tile it serves.
    items: Vec<(ResourceId, TileId, Role)>,
    coarsening: BTreeSet<TileId>,
    exact: u32,
    fallback: u32,
    lost: u32,
    depth: u32,
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
enum Job {
    Load,
    Deliver,
}

fn sort_key<'n>(n: &'n TileNeed<'_>) -> impl Ord + 'n {
    let img = n
        .need
        .imagery
        .as_ref()
        .map(|i| (i.footprint.ratio().to_bits(), &i.layers));
    (
        Reverse(n.membership),
        n.q,
        img,
        n.need.level,
        n.need.parent,
        n.need.class,
    )
}

/// The layers a need asks for, in stacking order.
fn layers(img: &ImageryNeed) -> &[LayerReach] {
    &img.layers
}

/// Every layer of `tile`, as a range of a map keyed by `(tile, layer)`.
fn tile_layers(tile: TileId) -> std::ops::RangeInclusive<(TileId, LayerId)> {
    (tile, LayerId(0))..=(tile, LayerId(u16::MAX))
}

/// The floor swap group `r` belongs to (D-A1), if any: a floor tile's mesh
/// and its floor drape of every layer it wears, in stacking order.
fn floor_group<'t>(
    tiles: &'t SortedMap<TileId, TileNeed<'_>>,
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

    fn is_floor(&self, level: u32) -> bool {
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
            decoded: if self.cfg.decoded_target == Bytes::MAX {
                Bytes::MAX
            } else {
                self.cfg.decoded_target.scaled(pct, 100)
            },
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

    // 2. The need.
    fn read_need<'a>(
        &mut self,
        input: &FrameInput<'a>,
        st: &mut BudgetStats,
    ) -> SortedMap<TileId, TileNeed<'a>> {
        let mut tiles: Vec<(TileId, TileNeed<'a>)> =
            recycle(std::mem::take(&mut self.scratch.tiles));
        for (seq, (m, t, n)) in input.need.entries().enumerate() {
            let q = Quantised::of(n);
            if q.rejected {
                st.rejected_inputs += 1;
            }
            tiles.push((
                t,
                TileNeed {
                    membership: m,
                    need: n,
                    q,
                    seq: u32::try_from(seq).unwrap_or(u32::MAX),
                },
            ));
            self.tiles.insert(
                t,
                TileInfo {
                    parent: n.parent,
                    level: n.level,
                    rect: n.rect,
                },
            );
        }
        // Per tile, the best entry first; of equals, the first listed.
        tiles.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| sort_key(&b.1).cmp(&sort_key(&a.1)))
                .then(a.1.seq.cmp(&b.1.seq))
        });
        tiles.dedup_by_key(|e| e.0);
        SortedMap::from_sorted(tiles)
    }

    /// How cheaply a drape could be drawn: on the GPU, decoded, in a local
    /// store, or only remotely.
    fn drape_tier(&self, r: ResourceId, presence: &dyn PresenceOracle) -> Tier {
        self.tier(r, self.arena.find(r), Class::Drape, presence)
    }

    /// Each wanted tile's imagery level this step.
    ///
    /// The level follows the texel footprint (relaxed with the LOD, with
    /// hysteresis). When that level is held nowhere locally, a level already
    /// held (GPU, decoded, store) is taken instead if it lies within one
    /// level of the window between the relaxed and the unrelaxed need: a
    /// relaxation never sends to the network what a local tier can show
    /// (I14, I15). Batch takes the exact level, a function of the frame.
    fn drape_needs(
        &mut self,
        tiles: &SortedMap<TileId, TileNeed<'_>>,
        presence: &dyn PresenceOracle,
    ) -> SortedMap<(TileId, LayerId), u32> {
        let mut out = std::mem::take(&mut self.scratch.drapes);
        out.clear();
        let floor = self.cfg.pinned_level.unwrap_or(0);
        let batch = self.cfg.policy == Policy::Batch;
        let cap = self.cfg.imagery_cap();
        for (t, tn) in tiles {
            let draped = matches!(tn.membership, Membership::Drawn | Membership::Blocker);
            let Some(img) = tn.need.imagery.as_ref().filter(|_| draped) else {
                continue;
            };
            for reach in layers(img) {
                let key = (*t, reach.layer);
                let drape = |l| ResourceId::drape(*t, reach.layer, l);
                let clamp = LevelClamp {
                    floor,
                    terrain_level: tn.need.level,
                    over_terrain_cap: cap,
                    source_max: reach.max_level,
                };
                if batch {
                    out.push((key, needed_level(None, img.footprint, clamp)));
                    continue;
                }
                let prev = self.imagery_prev.get(&key).copied();
                let relaxed = needed_level(prev, img.footprint.relaxed(self.scale), clamp);
                let mut level = relaxed;
                if self.drape_tier(drape(relaxed), presence) == Tier::Remote {
                    let full = needed_level(prev, img.footprint, clamp);
                    let ceiling = tn.need.level.saturating_add(cap).min(reach.max_level);
                    let lo = relaxed.min(full).saturating_sub(1).max(floor);
                    let hi = relaxed.max(full).saturating_add(1).min(ceiling);
                    let best = (lo..=hi)
                        .map(|l| {
                            let tier = self.drape_tier(drape(l), presence);
                            ((tier, l.abs_diff(relaxed), Reverse(l)), l)
                        })
                        .min();
                    if let Some(((tier, ..), l)) = best {
                        if tier != Tier::Remote {
                            level = l;
                        }
                    }
                }
                out.push((key, level));
            }
            // This tile's layers in order, a layer listed twice in the order
            // listed; the tiles come in order.
            let from = out.partition_point(|e| e.0 .0 < *t);
            out[from..].sort_by_key(|e| e.0);
        }
        // A layer listed twice: the last listing's level.
        out.dedup_by(|later, earlier| {
            let same = later.0 == earlier.0;
            if same {
                earlier.1 = later.1;
            }
            same
        });
        for (k, l) in &out {
            self.imagery_prev.insert(*k, *l);
        }
        SortedMap::from_sorted(out)
    }

    /// Every floor tile the need lists is drawable with its floor drape
    /// of every layer it wears (P1.1), and the need lists one at least.
    fn floor_in(&self, tiles: &SortedMap<TileId, TileNeed<'_>>) -> bool {
        let mut any = false;
        let all = tiles
            .iter()
            .filter(|(_, n)| self.is_floor(n.need.level))
            .all(|(t, n)| {
                any = true;
                self.drawable(ResourceId::Mesh(*t))
                    && n.need
                        .imagery
                        .as_ref()
                        .map(layers)
                        .unwrap_or(&[])
                        .iter()
                        .all(|r| self.drawable(ResourceId::drape(*t, r.layer, n.need.level)))
            });
        any && all
    }

    /// Kept from eviction this step: drawn, or in a swap the view waits on.
    fn kept(&self, r: &ResourceId) -> bool {
        self.drawn_last.contains(r) || self.swap_last.contains(r)
    }

    /// The drapes `mesh` wore last step.
    fn worn_last_of(&self, mesh: ResourceId) -> impl Iterator<Item = &DrapeChoice> + '_ {
        let from = self.worn_last.partition_point(|w| w.0 < mesh);
        self.worn_last[from..]
            .iter()
            .take_while(move |w| w.0 == mesh)
            .map(|w| &w.1)
    }

    fn drawable(&self, r: ResourceId) -> bool {
        self.arena.find(r).is_some_and(|s| self.arena.has_gpu(s))
    }

    /// Every (ancestor, held tile) pair of last step's held tiles, sorted,
    /// with the lowest level the walk from the held tile passed before
    /// reaching that ancestor: `held` is a descendant of `of` exactly when
    /// that level is above `of`'s own (or `of`'s is unknown). One walk per
    /// held tile, once per step, instead of one per held tile and missing
    /// tile.
    fn held_ancestry(&self, out: &mut Vec<(TileId, TileId, u32)>) {
        out.clear();
        for &h in &self.held_last {
            let mut t = h;
            let mut passed = u32::MAX;
            for _ in 0..64 {
                let Some(info) = self.tiles.get(&t) else {
                    break;
                };
                let Some(p) = info.parent else {
                    break;
                };
                out.push((p, h, passed));
                passed = passed.min(info.level);
                t = p;
            }
        }
        // A cycle visits an ancestor twice: its first visit, the highest
        // level passed, is the one that counts.
        out.sort_unstable_by_key(|a| (a.0, a.1, Reverse(a.2)));
        out.dedup_by(|b, a| (a.0, a.1) == (b.0, b.1));
    }

    /// The held tiles below `of`, in tile order.
    fn held_below<'h>(
        &self,
        ancestry: &'h [(TileId, TileId, u32)],
        of: TileId,
    ) -> impl Iterator<Item = TileId> + 'h {
        let target = self.tiles.get(&of).map(|i| i.level);
        let from = ancestry.partition_point(|a| a.0 < of);
        ancestry[from..]
            .iter()
            .take_while(move |a| a.0 == of)
            .filter(move |a| target.is_none_or(|l| a.2 > l))
            .map(|a| a.1)
    }

    /// The tile's ancestors, nearest first, as far as the budget knows them:
    /// at most 64, lazily, so a caller that stops at the first hit walks no
    /// further.
    fn ancestors(&self, t: TileId) -> impl Iterator<Item = TileId> + '_ {
        std::iter::successors(self.tiles.get(&t).and_then(|i| i.parent), move |p| {
            self.tiles.get(p).and_then(|i| i.parent)
        })
        .take(64)
    }

    /// The UV mapping of `tile`'s mesh into the drapes of `x`, the tile
    /// itself (`up == 0`) or one of its ancestors: `None` when either
    /// footprint is unknown.
    fn uv_into(&self, rect: Option<GeoRect>, up: usize, x: TileId) -> Option<UvTransform> {
        if up == 0 {
            return Some(UvTransform::IDENTITY);
        }
        match (rect, self.tiles.get(&x).and_then(|i| i.rect)) {
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
        self.arena.gpu_drapes(lo..=hi).filter_map(|r| match r {
            ResourceId::Drape { level, .. } => Some((r, level)),
            ResourceId::Mesh(_) => None,
        })
    }

    fn best_drape_for(&self, tile: TileId, layer: LayerId, need: u32) -> Option<DrapeChoice> {
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
        let rect = self.tiles.get(&tile).and_then(|i| i.rect);
        // `best_drape`'s fold, walked lazily: the first of equal ranks wins.
        let mut best: Option<(DrapeCandidate, ResourceId, UvTransform)> = None;
        for (up, x) in std::iter::once(tile)
            .chain(self.ancestors(tile))
            .enumerate()
        {
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
    /// stopping at the first found.
    fn has_drape(&self, tile: TileId, layer: LayerId) -> bool {
        let rect = self.tiles.get(&tile).and_then(|i| i.rect);
        std::iter::once(tile)
            .chain(self.ancestors(tile))
            .enumerate()
            .any(|(up, x)| {
                self.drawable_drapes(x, layer).next().is_some()
                    && self.uv_into(rect, up, x).is_some()
            })
    }

    /// Whether `tile`'s mesh can be drawn dressed for `layers`: each layer
    /// has a drawable drape on the tile or one of its ancestors, or — a
    /// layer switch in progress — the mesh still wears a layer it no longer
    /// needs. A mesh that cannot is not drawn: bare ground is black ground
    /// (I1, D-A1).
    fn dressable(&self, tile: TileId, layers: &[LayerId]) -> bool {
        let missing = layers.iter().any(|l| !self.has_drape(tile, *l));
        !missing
            || self
                .worn_last_of(ResourceId::Mesh(tile))
                .any(|d| !layers.contains(&d.layer) && self.drawable(d.resource))
    }

    /// The meshes that stand for the wanted tiles, at levels up to `cap`:
    /// each tile itself when drawable and dressed; else the finer tiles it
    /// held last step (zoom-out); else its nearest drawable, dressed
    /// ancestor.
    fn select(
        &self,
        tiles: &SortedMap<TileId, TileNeed<'_>>,
        drapes: &SortedMap<(TileId, LayerId), u32>,
        ancestry: &[(TileId, TileId, u32)],
        cap: Option<u32>,
    ) -> Selection {
        let mut sel = Selection::default();
        // The layers each wanted tile's drape need asks for, in layer order.
        let mut layers: Vec<LayerId> = Vec::new();
        let shown = |t: TileId, layers: &[LayerId]| {
            let level = self.tiles.get(&t).map_or(0, |i| i.level);
            cap.is_none_or(|c| level <= c)
                && self.drawable(ResourceId::Mesh(t))
                && self.dressable(t, layers)
        };
        for (t, _) in tiles
            .iter()
            .filter(|(_, n)| n.membership == Membership::Drawn)
        {
            layers.clear();
            layers.extend(drapes.range(tile_layers(*t)).map(|((_, l), _)| *l));
            if shown(*t, &layers) {
                sel.items.push((ResourceId::Mesh(*t), *t, Role::Exact));
                sel.exact += 1;
                continue;
            }
            let mut held = false;
            for h in self.held_below(ancestry, *t) {
                if shown(h, &layers) {
                    sel.items.push((ResourceId::Mesh(h), *t, Role::Held));
                    held = true;
                }
            }
            if held {
                sel.coarsening.insert(*t);
            }
            let anc = self.ancestors(*t).find(|a| shown(*a, &layers));
            if let Some(a) = anc {
                sel.items.push((ResourceId::Mesh(a), *t, Role::Fallback));
                let level = |x: TileId| self.tiles.get(&x).map_or(0, |i| i.level);
                sel.depth = sel.depth.max(level(*t).saturating_sub(level(a)));
                if !held {
                    sel.fallback += 1;
                }
            } else if !held {
                sel.lost += 1;
            }
        }
        sel
    }

    // 3. The draw set.
    fn build_draw(
        &self,
        tiles: &SortedMap<TileId, TileNeed<'_>>,
        drapes: &SortedMap<(TileId, LayerId), u32>,
        ancestry: &[(TileId, TileId, u32)],
        floor_in: bool,
        st: &mut BudgetStats,
    ) -> (DrawSet, BTreeSet<TileId>) {
        let mut sel = self.select(tiles, drapes, ancestry, None);
        if sel.lost > 0 && !floor_in {
            // P1.1 (b): coarse ground before any refinement. While wanted
            // ground is lost and the floor not in, nothing finer than the
            // floor is drawn.
            if let Some(p) = self.cfg.pinned_level {
                sel = self.select(tiles, drapes, ancestry, Some(p));
            }
        }
        st.exact += sel.exact;
        st.fallback += sel.fallback;
        st.lost += sel.lost;
        st.fallback_depth_max = st.fallback_depth_max.max(sel.depth);
        let Selection {
            mut items,
            coarsening,
            ..
        } = sel;
        // By mesh, then by tile served: each mesh's run is one draw item.
        items.sort_unstable();
        let meshes = items.chunk_by(|a, b| a.0 == b.0).count();
        let mut out = Vec::with_capacity(meshes);
        // The level each layer is needed at, by layer.
        let mut needs: Vec<(LayerId, u32)> = Vec::new();
        for run in items.chunk_by(|a, b| a.0 == b.0) {
            let mesh = run[0].0;
            let role = run.iter().map(|x| x.2).min().unwrap_or(Role::Exact);
            let mut serves: Vec<TileId> = Vec::with_capacity(run.len());
            serves.extend(run.iter().map(|x| x.1));
            serves.dedup();
            let tile = mesh.tile();
            // The level each layer is needed at: the tile's own need when
            // exact; a held tile keeps its own last level; a fallback
            // dresses for the finest of the tiles it serves.
            needs.clear();
            for s in &serves {
                for ((_, layer), n) in drapes.range(tile_layers(*s)) {
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
            let mut chosen: Vec<DrapeChoice> = Vec::with_capacity(needs.len());
            chosen.extend(
                needs
                    .iter()
                    .filter_map(|(layer, n)| self.best_drape_for(tile, *layer, *n)),
            );
            if chosen.len() < needs.len() {
                // A layer switch in progress: a layer the tiles now ask for
                // is drawable nowhere yet, so the layers this mesh wore last
                // step and no longer needs stay worn until it is.
                for old in self.worn_last_of(mesh) {
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
            if missing && !switching {
                st.bare += 1;
            }
            if role == Role::Exact {
                let off = needs.iter().any(|(layer, n)| {
                    !chosen
                        .iter()
                        .any(|d| d.layer == *layer && d.level.abs_diff(*n) <= 1)
                });
                if off {
                    st.underdraped += 1;
                }
                for d in &chosen {
                    let gap = i64::from(d.level)
                        - i64::from(self.tiles.get(&tile).map(|i| i.level).unwrap_or(0));
                    let i =
                        usize::try_from((gap + 8).clamp(0, GAP_BUCKETS as i64 - 1)).unwrap_or(0);
                    st.imagery_over_terrain[i] += 1;
                }
            }
            if role == Role::Held {
                st.held += 1;
            }
            out.push(DrawItem {
                mesh,
                role,
                drapes: chosen,
                serves,
            });
        }
        (DrawSet { items: out }, coarsening)
    }

    /// What the step wants loaded or delivered.
    fn wants(
        &self,
        tiles: &SortedMap<TileId, TileNeed<'_>>,
        drapes: &SortedMap<(TileId, LayerId), u32>,
        draw: &DrawSet,
        raw: &mut Vec<(ResourceId, u32, Want)>,
        mut merged: Vec<(ResourceId, Want)>,
    ) -> SortedMap<ResourceId, Want> {
        raw.clear();
        merged.clear();
        let batch = self.cfg.policy == Policy::Batch;
        // Every want put, in order; merged below.
        let put = |out: &mut Vec<(ResourceId, u32, Want)>, r: ResourceId, w: Want| {
            let seq = u32::try_from(out.len()).unwrap_or(u32::MAX);
            out.push((r, seq, w));
        };
        // Batch: a frame's drapes are those of what it draws. Batch never
        // cancels a load it started (I13), so a drape started for a tile the
        // selection is still settling past — its mesh not in, the tile then
        // replaced by its children — would be read whatever the frame draws,
        // and what a bake reads would follow the order its meshes came in.
        // So no drape but the floor's own is wanted until every mesh the
        // frame draws and waits on is loaded: the selection it then wears
        // drapes for is the one the frame draws. A floor tile is no
        // exception past its own drape: drawn while the selection settles,
        // the finer drape it asks for is as much a pass's as any other's.
        let meshes_in = !batch
            || tiles
                .iter()
                .filter(|(_, tn)| matches!(tn.membership, Membership::Drawn | Membership::Blocker))
                .all(|(t, _)| {
                    let a = &self.arena;
                    a.find(ResourceId::Mesh(*t))
                        .is_some_and(|s| a.has_gpu(s) || a.has_cpu(s))
                });
        for (t, tn) in tiles {
            let floor = self.is_floor(tn.need.level);
            let mesh_drawable = self.drawable(ResourceId::Mesh(*t));
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
                raw,
                ResourceId::Mesh(*t),
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
                        raw,
                        ResourceId::drape(*t, reach.layer, tn.need.level),
                        Want {
                            urgency: Urgency::Floor,
                            q: tn.q,
                            class: Class::Drape,
                            level: tn.need.level,
                        },
                    );
                }
            }
            if !meshes_in {
                continue;
            }
            for ((_, layer), &n) in drapes.range(tile_layers(*t)) {
                let drape = ResourceId::drape(*t, *layer, n);
                let du = match tn.membership {
                    Membership::Blocker => Some(Urgency::Blocker),
                    _ if !mesh_drawable => Some(urgency),
                    _ => {
                        let worn = draw
                            .get(ResourceId::Mesh(*t))
                            .and_then(|i| i.drape(*layer).copied());
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
                        raw,
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
        raw.sort_unstable_by_key(|(r, seq, w)| (*r, w.urgency, Reverse(w.q), *seq));
        raw.dedup_by_key(|e| e.0);
        merged.extend(raw.iter().map(|(r, _, w)| (*r, *w)));
        SortedMap::from_sorted(merged)
    }

    /// What protects `r`, whose slot is `s`.
    fn protection_of(&self, r: ResourceId, s: Slot) -> Option<Protection> {
        let a = &self.arena;
        if self.is_floor(a.level(s)) && (a.has_gpu(s) || a.has_transit(s)) {
            return Some(Protection::Floor);
        }
        if self.drawn_last.contains(&r) {
            return Some(Protection::Drawn);
        }
        if a.has_transit(s) {
            return Some(Protection::InTransit);
        }
        if self.swap_last.contains(&r) {
            return Some(match r {
                ResourceId::Mesh(_) if self.chain_last.contains(&r) => Protection::Chain,
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
        let a = &self.arena;
        let epoch = self.epoch(r, class);
        match e {
            Some(s) if a.gpu(s).is_some_and(|g| g.epoch == epoch) => Tier::Gpu,
            Some(s) if a.fresh_cpu(s, epoch).is_some() => Tier::Decoded,
            // A wrong hint is believed once; otherwise the index is asked
            // afresh, since a store fills as the session runs.
            Some(s) if a.cold(s).store_missed => Tier::Remote,
            _ => match presence.holds(r) {
                Presence::Stored => Tier::Stored,
                Presence::Absent => Tier::Remote,
            },
        }
    }

    /// The step.
    pub(super) fn step(&mut self, input: FrameInput<'_>) -> Decisions {
        let now = input.now;
        let mut st = std::mem::take(&mut self.pending);
        let mut d = Decisions::default();
        let eff = self.eff();
        self.scratch.room = None;
        let mut ranked = std::mem::take(&mut self.ranked_last);
        for r in &ranked {
            if let Some(s) = self.arena.find(*r) {
                self.arena.set_rank(s, None, None);
            }
        }
        ranked.clear();
        self.ranked_last = ranked;
        self.steps += 1;

        self.apply_events(now, &mut st);
        let tiles = self.read_need(&input, &mut st);
        let listed = input.need.drawn.len()
            + input.need.blockers.len()
            + input.need.ancestors.len()
            + input.need.ring.len();
        st.work = st.work.saturating_add(sat_u32(listed));
        let drapes = self.drape_needs(&tiles, input.presence);
        st.floor_in = self.floor_in(&tiles);
        let mut ancestry = std::mem::take(&mut self.scratch.ancestry);
        self.held_ancestry(&mut ancestry);
        let (draw, coarsening) = self.build_draw(&tiles, &drapes, &ancestry, st.floor_in, &mut st);
        self.scratch.ancestry = ancestry;
        let mut raw = std::mem::take(&mut self.scratch.wants_raw);
        let merged = std::mem::take(&mut self.scratch.wants);
        let wants = self.wants(&tiles, &drapes, &draw, &mut raw, merged);
        // Drawn content of an older epoch is refreshed, new before old.
        raw.clear();
        for i in &draw.items {
            for r in std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource)) {
                if let Some(s) = self.arena.find(r) {
                    let class = self.arena.class(s);
                    if self.arena.stale_gpu(s, self.epoch(r, class)) && !wants.contains_key(&r) {
                        let seq = u32::try_from(raw.len()).unwrap_or(u32::MAX);
                        raw.push((
                            r,
                            seq,
                            Want {
                                urgency: Urgency::Refresh,
                                q: Quantised::default(),
                                class,
                                level: self.arena.level(s),
                            },
                        ));
                    }
                }
            }
        }
        let wants = if raw.is_empty() {
            wants
        } else {
            // The first refresh of a resource listed twice.
            raw.sort_unstable_by_key(|(r, seq, _)| (*r, *seq));
            raw.dedup_by_key(|e| e.0);
            let mut all = wants.into_vec();
            all.extend(raw.iter().map(|(r, _, w)| (*r, *w)));
            all.sort_unstable_by_key(|e| e.0);
            SortedMap::from_sorted(all)
        };
        self.scratch.wants_raw = raw;

        // 4. Mark: needed now, or unneeded since when. What is drawn, the
        // drapes each mesh wears, and the swap and chain sets are kept for
        // the rest of the step and the next, in buffers refilled in place.
        self.drawn_last.refill(
            draw.items
                .iter()
                .flat_map(|i| std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource))),
        );
        // Items are sorted by mesh: so is this.
        self.worn_last.clear();
        self.worn_last.extend(
            draw.items
                .iter()
                .flat_map(|i| i.drapes.iter().map(move |x| (i.mesh, *x))),
        );
        let mut chain = std::mem::take(&mut self.chain_last);
        chain.refill(
            wants
                .iter()
                .filter(|(r, w)| w.urgency == Urgency::Ancestor && self.drawable(**r))
                .map(|(r, _)| *r),
        );
        self.chain_last = chain;
        let mut swap = std::mem::take(&mut self.swap_last);
        swap.refill(
            wants
                .iter()
                .filter(|(r, w)| {
                    matches!(
                        w.urgency,
                        Urgency::Hole | Urgency::Blocker | Urgency::Ancestor
                    ) && self.drawable(**r)
                })
                .map(|(r, _)| *r),
        );
        self.swap_last = swap;
        // Only what the view touched this step or the last: everything else
        // was already marked unneeded.
        // A merge of the three sorted sources, not a set built of them.
        let Budget {
            arena,
            drawn_last,
            wanted_last,
            ..
        } = self;
        let touched = union4(
            wants.keys().copied(),
            drawn_last.iter().copied(),
            wanted_last.iter().copied(),
            std::iter::empty(),
        );
        for r in touched {
            let Some(s) = arena.find(r) else {
                continue;
            };
            if wants.contains_key(&r) {
                arena.touch(s, now);
                let cold = arena.cold_mut(s);
                cold.set_unneeded_since(None);
                cold.first_wanted_or(now);
                if let Some(tn) = tiles.get(&r.tile()) {
                    arena.set_distance(s, tn.q.distance);
                }
            } else if drawn_last.contains(&r) {
                arena.touch(s, now);
                let cold = arena.cold_mut(s);
                cold.set_unneeded_since(None);
                cold.set_first_wanted(None);
            } else {
                let cold = arena.cold_mut(s);
                cold.unneeded_since_or(now);
                cold.set_first_wanted(None);
            }
        }

        // 5. Cancellations: grace and timeouts.
        self.cancel(now, &eff, &mut d, &mut st);

        // 6. Rank the work.
        // Wants come in order: so do their keys.
        let mut keys = std::mem::take(&mut self.scratch.keys);
        keys.clear();
        let mut jobs = std::mem::take(&mut self.scratch.jobs);
        jobs.clear();
        for (r, w) in &wants {
            let class = self.class_of(*r, w.class);
            let epoch = self.epoch(*r, class);
            let a = &self.arena;
            let e = a.find(*r);
            let mut urgency = w.urgency;
            let job = match e {
                Some(s) if a.gpu(s).is_some_and(|g| g.epoch == epoch) => None,
                Some(s) if a.has_transit(s) => None,
                Some(s) if a.fresh_cpu(s, epoch).is_some() => {
                    (a.cold(s).deliver_after <= now).then_some(Job::Deliver)
                }
                Some(s) if a.loading(s) => None,
                Some(s) if a.backing_off(s, now) => None,
                _ => Some(Job::Load),
            };
            if e.is_some_and(|s| a.stale_gpu(s, epoch)) {
                urgency = Urgency::Refresh;
            }
            let waited = e
                .and_then(|s| a.cold(s).first_wanted())
                .map(|f| now.since(f))
                .unwrap_or(Micros::ZERO);
            // How many wanted tiles it stands in for, as a fallback.
            let cover = draw
                .get(*r)
                .filter(|i| i.role == Role::Fallback)
                .map_or(0, |i| sat_u32(i.serves.len()));
            let mut sc = score(w.q, waited, self.cfg.age_full, cover);
            if urgency == Urgency::Hole && coarsening.contains(&r.tile()) {
                // At the ceiling a coarser covering tile frees bytes: first.
                sc += 1_000_000;
            }
            let key = PriorityKey {
                urgency,
                score: Reverse(sc),
                tier: self.tier(*r, e, class, input.presence),
                resource: *r,
            };
            keys.push((*r, key));
            if let Some(job) = job {
                jobs.push((key, *r, job, Want { urgency, ..*w }));
            }
        }
        let keys = SortedMap::from_sorted(keys);
        // Keys end in the resource: total, so unstable is stable.
        jobs.sort_unstable_by_key(|j| j.0);
        st.work = st
            .work
            .saturating_add(sat_u32(wants.len()))
            .saturating_add(sat_u32(jobs.len()));
        self.ranked_last.extend(jobs.iter().map(|j| j.1));
        for (k, r, _, w) in &jobs {
            if let Some(s) = self.arena.find(*r) {
                self.arena.set_rank(s, Some(w.urgency), Some(Rank::of(k)));
            }
        }

        let mut picked = std::mem::take(&mut self.scratch.picked);
        picked.clear();
        picked.extend(
            jobs.iter()
                .filter(|j| j.2 == Job::Load)
                .take(self.cfg.request_heap as usize)
                .copied(),
        );
        st.heap_len = sat_u32(picked.len());
        self.start(
            now,
            &eff,
            &picked,
            &keys,
            &wants,
            input.presence,
            &mut d,
            &mut st,
        );

        // 7. Deliveries.
        picked.clear();
        picked.extend(jobs.iter().filter(|j| j.2 == Job::Deliver).copied());
        let (blocked, strained) = self.deliver(
            now,
            &eff,
            &picked,
            &tiles,
            &coarsening,
            &wants,
            &mut d,
            &mut st,
        );

        // 8. The upload plan.
        d.upload_plan = self.plan(&eff, &keys, &tiles);
        // The plan scans the loads and transits for those in transit.
        st.work = st.work.saturating_add(sat_u32(self.active.len()));

        // 9. Eviction and release.
        if !eff.batch {
            self.evict(now, &eff, &wants, &mut d, &mut st);
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

        // Batch completeness.
        let pending_view = jobs.iter().any(|j| j.3.urgency <= Urgency::Drape)
            || wants.iter().any(|(r, w)| {
                w.urgency <= Urgency::Drape
                    && self
                        .arena
                        .find(*r)
                        .is_some_and(|s| self.arena.has_transit(s) || self.arena.loading(s))
            });
        if eff.batch {
            let complete = input.need.blockers.is_empty()
                && tiles
                    .iter()
                    .filter(|(_, n)| n.membership == Membership::Drawn)
                    .all(|(t, _)| {
                        let item = draw.get(ResourceId::Mesh(*t));
                        let exact = item.is_some_and(|i| i.role == Role::Exact);
                        let drape_ok = drapes.range(tile_layers(*t)).all(|((_, l), &n)| {
                            item.and_then(|i| i.drape(*l))
                                .is_some_and(|x| x.resource == ResourceId::drape(*t, *l, n))
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

        // Bookkeeping for the next step.
        self.held_last.refill(
            draw.items
                .iter()
                .filter(|i| i.role != Role::Fallback)
                .map(|i| i.mesh.tile()),
        );
        let mut wanted = std::mem::take(&mut self.wanted_last);
        wanted.refill(wants.keys().chain(self.drawn_last.iter()).copied());
        self.wanted_last = wanted;
        let pruned = self.prune(now, &tiles, &wants, &drapes);
        st.work = st.work.saturating_add(pruned);
        self.scratch.tiles = recycle(tiles.into_buffer());
        self.scratch.drapes = drapes.into_buffer();
        self.scratch.wants = wants.into_buffer();
        self.scratch.keys = keys.into_buffer();
        jobs.clear();
        self.scratch.jobs = jobs;
        picked.clear();
        self.scratch.picked = picked;

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
        for r in &self.active {
            if let Some(s) = self.arena.find(*r) {
                if let Some(t) = self.arena.transit(s) {
                    let c = self.arena.class(s);
                    st.transit[c] = st.transit[c].plus(t.cost.gpu);
                }
            }
        }
        st.protected = protected;
        d.draw = draw;
        d.stats = st;
        d
    }

    fn protected_bytes(&self) -> PerClass<Bytes> {
        let mut p = PerClass::splat(Bytes::ZERO);
        let candidates = union4(
            self.drawn_last.iter().copied(),
            self.swap_last.iter().copied(),
            self.active.iter().copied(),
            self.floor_set.iter().copied(),
        );
        for r in candidates {
            let Some(s) = self.arena.find(r) else {
                continue;
            };
            if self.protection_of(r, s).is_some() {
                let a = &self.arena;
                let b = a
                    .gpu(s)
                    .map(|g| g.bytes)
                    .unwrap_or(Bytes::ZERO)
                    .plus(a.transit(s).map(|t| t.cost.gpu).unwrap_or(Bytes::ZERO));
                p[a.class(s)] = p[a.class(s)].plus(b);
            }
        }
        p
    }

    fn cancel(&mut self, now: Micros, eff: &Eff, d: &mut Decisions, st: &mut BudgetStats) {
        let grace = self.cfg.cancel_grace;
        let timeout = self.cfg.load_timeout;
        let batch = eff.batch;
        let failure = self.cfg.failure;
        let mut fatal = None;
        let active: Vec<ResourceId> = self.active.iter().copied().collect();
        for r in &active {
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
                        && !self.drawn_last.contains(r)
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
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        now: Micros,
        eff: &Eff,
        loads: &[(PriorityKey, ResourceId, Job, Want)],
        keys: &SortedMap<ResourceId, PriorityKey>,
        wants: &SortedMap<ResourceId, Want>,
        presence: &dyn PresenceOracle,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let decoded_full = self.ledger.decoded >= eff.decoded;
        // Stored resources left unstarted for any reason but their own
        // quota: a network load of lower priority starting after them is a
        // local wait (I15).
        let mut skipped_stored = 0u32;
        for (key, r, _, w) in loads {
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
                            .filter_map(|vr| keys.get(vr).map(|k| (*k, *vr)))
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
            let s = self.arena.find_or_insert(*r, class, w.level, now);
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
                self.floor_set.insert(*r);
            }
            self.active.insert(*r);
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
        wants: &SortedMap<ResourceId, Want>,
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
                        && !self.kept(&a.id(s))
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
            if self.kept(&r) || self.is_floor(self.arena.level(s)) {
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
            self.recent.insert(r, now);
        }
        self.scratch.room = Some(room);
        true
    }

    /// This step's [`Room`], built at its first use, after the mark: the
    /// grouped GPU residents outside the kept set, the floor and transit.
    fn room(&self, wants: &SortedMap<ResourceId, Want>, st: &mut BudgetStats) -> Room {
        let a = &self.arena;
        let mut grouped: Vec<(EvictKey, Slot)> = a
            .grouped_slots()
            .filter(|&s| {
                a.has_gpu(s)
                    && !a.has_transit(s)
                    && !self.kept(&a.id(s))
                    && !self.is_floor(a.level(s))
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
        jobs: &[(PriorityKey, ResourceId, Job, Want)],
        tiles: &SortedMap<TileId, TileNeed<'_>>,
        coarsening: &BTreeSet<TileId>,
        wants: &SortedMap<ResourceId, Want>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) -> (bool, bool) {
        let mut flags = (false, false);
        for (_, r, _, w) in jobs {
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
        coarsening: &BTreeSet<TileId>,
        wants: &SortedMap<ResourceId, Want>,
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
            let shrinking = matches!(r, ResourceId::Mesh(t) if coarsening.contains(&t));
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
            a.grouped(s) && a.has_gpu(s) && !self.kept(&r) && !self.is_floor(a.level(s))
        };
        if let (true, Some(room), Some(g)) =
            (in_room, self.scratch.room.as_mut(), self.arena.gpu(s))
        {
            let i = want_bucket(wants.get(&r));
            room.bytes[class][i] = room.bytes[class][i].minus(g.bytes);
        }
        // `make_room` frees no slot: `s` is still `r`'s.
        self.active.insert(r);
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

    fn plan(
        &self,
        eff: &Eff,
        keys: &SortedMap<ResourceId, PriorityKey>,
        tiles: &SortedMap<TileId, TileNeed<'_>>,
    ) -> UploadPlan {
        // Group: the blockers of one parent (meshes and drapes), else a
        // tile's mesh and its drape.
        let group_of = |r: ResourceId| -> (u8, TileId) {
            let t = r.tile();
            match tiles.get(&t) {
                Some(tn) if tn.membership == Membership::Blocker => match tn.need.parent {
                    Some(p) => (0, p),
                    None => (1, t),
                },
                _ => (1, t),
            }
        };
        // Every copy in transit is indexed in `active` (`deliver_one`).
        let mut transit: Vec<(PriorityKey, ResourceId, Bytes)> = self
            .active
            .iter()
            .filter_map(|r| {
                let t = self.arena.transit(self.arena.find(*r)?)?;
                Some((*r, t.cost.gpu))
            })
            .map(|(r, b)| {
                let k = keys.get(&r).copied().unwrap_or(PriorityKey {
                    urgency: Urgency::Refresh,
                    score: Reverse(i64::MIN),
                    tier: Tier::Remote,
                    resource: r,
                });
                (k, r, b)
            })
            .collect();
        transit.sort_unstable();
        let mut plan = UploadPlan::default();
        // Each group's members with their bytes, in transit order.
        let mut groups: BTreeMap<(u8, TileId), Vec<(ResourceId, Bytes)>> = BTreeMap::new();
        for (_, r, b) in &transit {
            groups.entry(group_of(*r)).or_default().push((*r, *b));
        }
        for (_, r, _) in &transit {
            // A group goes whole, at its first member in transit order: a
            // later member finds it gone.
            let Some(mut members) = groups.remove(&group_of(*r)) else {
                continue;
            };
            let bytes = members.iter().fold(Bytes::ZERO, |a, m| a.plus(m.1));
            if plan.items.is_empty() && plan.bytes.plus(bytes) > eff.upload {
                // The first item always goes, alone if its group is too big.
                plan.items.push(*r);
                plan.bytes = members
                    .iter()
                    .find(|m| m.0 == *r)
                    .map_or(Bytes::ZERO, |m| m.1);
                break;
            }
            if plan.bytes.plus(bytes) > eff.upload {
                break;
            }
            plan.items.extend(members.iter().map(|m| m.0));
            plan.bytes = plan.bytes.plus(bytes);
            if members.len() > 1 {
                members.sort_unstable_by_key(|m| m.0);
                plan.groups.push(SwapGroup {
                    members: members.into_iter().map(|m| m.0).collect(),
                });
            }
        }
        plan
    }

    fn evict(
        &mut self,
        now: Micros,
        eff: &Eff,
        wants: &SortedMap<ResourceId, Want>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
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
            while let Some(((.., r), s)) =
                walk.next(&self.arena, &room.grouped, |c| c == class, &Held::GPU)
            {
                if self.ledger.gpu[class] <= low {
                    break;
                }
                st.work = st.work.saturating_add(1);
                // The order holds neither the kept set nor the floor.
                if self.kept(&r) || self.is_floor(self.arena.level(s)) {
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
        // The protected first: the kept set, sorted by key.
        let mut kept: Vec<(EvictKey, Slot)> = union4(
            self.drawn_last.iter().copied(),
            self.swap_last.iter().copied(),
            std::iter::empty(),
            std::iter::empty(),
        )
        .filter_map(|r| a.find(r))
        .filter(|&s| held(s) && a.has_gpu(s))
        .map(|s| (a.evict_key(s), s))
        .collect();
        kept.sort_unstable();
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
            while let Some(((.., r), s)) = walk.next(&self.arena, &grouped, |_| true, held) {
                st.work = st.work.saturating_add(1);
                let a = &self.arena;
                if !a.has_cpu(s) || a.has_gpu(s) != gpu || (gpu && self.kept(&r)) {
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
        wants: &SortedMap<ResourceId, Want>,
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
        let candidate = |b: &Budget, r: &ResourceId, s: Slot| {
            let a = &b.arena;
            a.has_gpu(s) && !a.has_transit(s) && !b.kept(r) && !b.is_floor(a.level(s))
        };
        let mut walk = Walk::default();
        while let Some(((.., r), s)) = walk.next(&self.arena, &room.grouped, member, &Held::GPU) {
            st.work = st.work.saturating_add(1);
            if !candidate(self, &r, s) {
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
        while let Some(((.., r), s)) = walk.next(&self.arena, &room.grouped, member, &Held::GPU) {
            if Class::ALL.iter().all(|c| !short(&self.ledger, *c)) {
                break;
            }
            st.work = st.work.saturating_add(1);
            let class = self.arena.class(s);
            if candidate(self, &r, s) && short(&self.ledger, class) {
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
        self.recent.insert(r, now);
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

    fn prune(
        &mut self,
        now: Micros,
        tiles: &SortedMap<TileId, TileNeed<'_>>,
        wants: &SortedMap<ResourceId, Want>,
        drapes: &SortedMap<(TileId, LayerId), u32>,
    ) -> u32 {
        // Amortised: emptied entries and unreferenced tiles go every eighth
        // step, so a step does not scan all it tracks. Returns the entries
        // visited.
        let visited = sat_u32(self.imagery_prev.len());
        if !self.steps.is_multiple_of(8) {
            let held = &self.held_last;
            self.imagery_prev
                .retain(|k, _| drapes.contains_key(k) || held.contains(&k.0));
            return visited;
        }
        let visited = visited
            .saturating_add(sat_u32(self.arena.len()))
            .saturating_add(sat_u32(self.tiles.len()));
        let a = &self.arena;
        self.active
            .retain(|r| a.find(*r).is_some_and(|s| a.has_transit(s) || a.loading(s)));
        let kept = |a: &Arena, s: Slot| {
            if !a.is_empty(s) {
                return true;
            }
            let cold = a.cold(s);
            if cold.backing_off(now)
                && cold
                    .unneeded_since()
                    .is_none_or(|u| now.since(u) < Micros::from_secs(30))
            {
                return true;
            }
            wants.contains_key(&a.id(s)) && cold.unneeded_since().is_none()
        };
        for n in 0..self.arena.slots() {
            if let Some(s) = self.arena.live(n) {
                if !kept(&self.arena, s) {
                    self.arena.remove(s);
                }
            }
        }
        let a = &self.arena;
        self.floor_set.retain(|r| a.find(*r).is_some());
        // Keep only what is wanted this step among idle, never-started entries.
        self.recent.retain(|_, t| now.since(*t) <= THRASH_WINDOW);
        if self.recent.len() > RECENT_CAP {
            // The oldest go, by (time, resource): one sort, not one scan
            // per removal.
            let mut by_age: Vec<(Micros, ResourceId)> =
                self.recent.iter().map(|(r, t)| (*t, *r)).collect();
            by_age.sort_unstable();
            for (_, r) in &by_age[..by_age.len() - RECENT_CAP] {
                self.recent.remove(r);
            }
        }
        // The tiles still referenced and every ancestor of theirs, as one
        // sorted vector rather than a set grown node by node.
        let mut keep: Vec<TileId> = tiles.keys().copied().collect();
        // As many as are tracked: one growth, as the ordered map's exact
        // length gave before.
        keep.reserve(self.arena.len());
        keep.extend(self.arena.iter().map(|s| self.arena.id(s).tile()));
        keep.extend(self.held_last.iter().copied());
        keep.sort_unstable();
        keep.dedup();
        let seeds = keep.len();
        for i in 0..seeds {
            let mut cur = keep[i];
            let walk = keep.len();
            while let Some(p) = self.tiles.get(&cur).and_then(|i| i.parent) {
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
        self.tiles.retain(|t, _| keep.binary_search(t).is_ok());
        let held = &self.held_last;
        self.imagery_prev
            .retain(|k, _| drapes.contains_key(k) || held.contains(&k.0));
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
                && !b.wanted_last.contains(&a.id(s))
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
    use crate::BudgetConfig;

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
}
