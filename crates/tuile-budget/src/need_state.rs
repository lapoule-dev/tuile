// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The need the budget last read, kept from one step to the next: each
//! wanted tile's best entry, in tile order, which the step reads as it is.
//!
//! A step changes it by what changed ([`NeedDelta`]): a tile that entered
//! or changed is resolved again from its entries, one that left goes, the
//! others are not touched. A step given a whole need ([`NeedSet`]) replaces
//! it: every tile resolved again, as every step did before deltas.
//!
//! A tile's entries resolve as the whole need's did: the best entry wins
//! (the most urgent membership, then the larger quantised scores, imagery,
//! level, parent, class), the first listed of equals; the tree position the
//! budget remembers ([`TileInfo`]) is the last entry's.

#![deny(clippy::float_arithmetic)]

use std::cmp::Reverse;

use super::imagery::TexelFootprint;
use super::need::{DeltaOp, ImageryNeed, LayerReach, Membership, Need, NeedDelta, NeedSet};
use super::priority::Quantised;
use super::sorted::SortedMap;
use super::types::{sat_u32, Class, TileId};
use super::TileInfo;

/// What a step reads of a wanted tile's need: its best entry's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Wanted {
    /// The tile's parent's id, when `has_parent` (0 otherwise): packed
    /// apart from its flag, so a kept record is 80 bytes, not 88.
    parent: u64,
    /// The tile's level in its tree.
    pub level: u32,
    /// The mesh's class.
    pub class: Class,
    has_parent: bool,
    /// The imagery the tile wants.
    pub imagery: Option<WantedImagery>,
}

/// The imagery a wanted tile wants: an [`ImageryNeed`], its layers held in
/// place when they are few.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WantedImagery {
    /// How large one level-0 texel looks on screen.
    pub footprint: TexelFootprint,
    layers: Layers,
}

impl WantedImagery {
    /// The layers the tile wears, in stacking order.
    pub fn layers(&self) -> &[LayerReach] {
        self.layers.as_slice()
    }
}

/// A list of layers: up to two in place — a tile wears one or two — more
/// on the heap. A kept need holds no allocation of its own for them: a
/// boxed list for every tile costs a block each time a tile enters or its
/// layers change (measured on the tour: internal allocations per step p99
/// 38 -> 49).
#[derive(Clone, Debug, PartialEq)]
enum Layers {
    Inline(u8, [LayerReach; 2]),
    Heap(Box<[LayerReach]>),
}

const NO_LAYER: LayerReach = LayerReach {
    layer: super::types::LayerId(0),
    max_level: 0,
};

impl Layers {
    fn of(l: &[LayerReach]) -> Self {
        match l {
            [] => Layers::Inline(0, [NO_LAYER; 2]),
            [a] => Layers::Inline(1, [*a, NO_LAYER]),
            [a, b] => Layers::Inline(2, [*a, *b]),
            _ => Layers::Heap(l.into()),
        }
    }

    fn as_slice(&self) -> &[LayerReach] {
        match self {
            Layers::Inline(n, a) => &a[..usize::from(*n)],
            Layers::Heap(b) => b,
        }
    }

    /// Becomes `l`, a heap list of the same length reused.
    fn assign(&mut self, l: &[LayerReach]) {
        match self {
            Layers::Heap(b) if b.len() == l.len() => b.copy_from_slice(l),
            _ => *self = Layers::of(l),
        }
    }
}

impl Default for Wanted {
    fn default() -> Self {
        Wanted {
            parent: 0,
            level: 0,
            class: Class::TerrainMesh,
            has_parent: false,
            imagery: None,
        }
    }
}

impl Wanted {
    /// The tile's parent.
    pub fn parent(&self) -> Option<TileId> {
        self.has_parent.then_some(TileId(self.parent))
    }

    /// Becomes `n`'s.
    fn assign(&mut self, n: &Need) {
        self.level = n.level;
        self.parent = n.parent.map_or(0, |p| p.0);
        self.has_parent = n.parent.is_some();
        self.class = n.class;
        match (&mut self.imagery, &n.imagery) {
            (Some(w), Some(i)) => {
                w.footprint = i.footprint;
                w.layers.assign(&i.layers);
            }
            (w, i) => {
                *w = i.as_ref().map(|i: &ImageryNeed| WantedImagery {
                    footprint: i.footprint,
                    layers: Layers::of(&i.layers),
                })
            }
        }
    }
}

/// A tile a change named, with what the budget knew of it before: its
/// parent and level (its footprint aside), and whether it was wanted; and
/// what of it changed beyond its floats and footprint — its shape (entered,
/// left, another membership, level, class or layers: what the selection,
/// the floor and the draw items read of it) and its place in the tree
/// (parent, level, footprint). Packed: a change names every tile of a
/// moving view, most of them for their floats alone.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Named {
    pub tile: TileId,
    parent: u64,
    level: u32,
    flags: u8,
}

/// `Named::flags`: a position was known, with a parent; the tile was
/// wanted.
const KNOWN: u8 = 1;
const PARENT: u8 = 1 << 1;
const WANTED: u8 = 1 << 2;
/// `Named::flags`: its shape changed; its place did.
const SHAPED: u8 = 1 << 3;
const MOVED: u8 = 1 << 4;

impl Named {
    fn new(
        tile: TileId,
        before: Option<TileInfo>,
        wanted: bool,
        shaped: bool,
        moved: bool,
    ) -> Self {
        let parent = before.and_then(|i| i.parent());
        let flag = |b: bool, f: u8| if b { f } else { 0 };
        Named {
            tile,
            parent: parent.map_or(0, |p| p.0),
            level: before.map_or(0, |i| i.level),
            flags: flag(before.is_some(), KNOWN)
                | flag(parent.is_some(), PARENT)
                | flag(wanted, WANTED)
                | flag(shaped, SHAPED)
                | flag(moved, MOVED),
        }
    }

    /// Whether its shape changed: it entered or left, or its membership,
    /// level, class or layers changed.
    pub fn shaped(&self) -> bool {
        self.flags & SHAPED != 0
    }

    /// Whether its place in the tree changed: parent, level or footprint.
    pub fn moved(&self) -> bool {
        self.flags & MOVED != 0
    }

    /// Its tree position before the change (its footprint left out).
    pub fn before(&self) -> Option<TileInfo> {
        (self.flags & KNOWN != 0).then(|| {
            TileInfo::new(
                (self.flags & PARENT != 0).then_some(TileId(self.parent)),
                self.level,
                None,
            )
        })
    }

    /// Whether it was wanted before the change.
    pub fn was_wanted(&self) -> bool {
        self.flags & WANTED != 0
    }
}

/// One wanted tile: its best entry, and what its entries count.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TileNeed {
    /// The best entry's list.
    pub membership: Membership,
    /// The best entry's need.
    pub need: Wanted,
    /// The best entry's floats, quantised.
    pub q: Quantised,
    /// Its entries; 0 marks a record that goes at the end of a change.
    listed: u32,
    /// Its entries with a float that was not finite.
    rejected: u32,
    /// Its entries in the blockers' list.
    blockers: u32,
}

impl Default for TileNeed {
    fn default() -> Self {
        TileNeed {
            membership: Membership::Drawn,
            need: Wanted::default(),
            q: Quantised::default(),
            listed: 0,
            rejected: 0,
            blockers: 0,
        }
    }
}

// A kept record's size: the budget keeps one per wanted tile, and the step
// reads them all. It holds no `usize`, so the bound holds on wasm32 too.
const _: () = assert!(std::mem::size_of::<TileNeed>() <= 80);

/// The order of a tile's entries, larger first: the order the whole need's
/// sort gave them.
fn entry_key<'n>(m: Membership, n: &'n Need, q: Quantised) -> impl Ord + 'n {
    let img = n
        .imagery
        .as_ref()
        .map(|i| (i.footprint.ratio().to_bits(), &i.layers[..]));
    (Reverse(m), q, img, n.level, n.parent, n.class)
}

/// A tile's entries, read one by one in the need's order.
#[derive(Default)]
struct Resolve<'n> {
    best: Option<(Membership, &'n Need, Quantised)>,
    last: Option<(Membership, &'n Need)>,
    listed: u32,
    rejected: u32,
    blockers: u32,
}

impl<'n> Resolve<'n> {
    fn feed(&mut self, m: Membership, n: &'n Need) {
        let q = Quantised::of(n);
        self.listed = self.listed.saturating_add(1);
        self.rejected = self.rejected.saturating_add(u32::from(q.rejected));
        self.blockers = self
            .blockers
            .saturating_add(u32::from(m == Membership::Blocker));
        // Of equals, the first: only a strictly better entry replaces it.
        if self
            .best
            .is_none_or(|(bm, bn, bq)| entry_key(m, n, q) > entry_key(bm, bn, bq))
        {
            self.best = Some((m, n, q));
        }
        // The last entry in the need's order: the latest list, then the
        // later listed.
        if self.last.is_none_or(|(lm, _)| m >= lm) {
            self.last = Some((m, n));
        }
    }

    /// Writes the tile's record; its tree position (the last entry's).
    /// `None` when it has no entry: it is not wanted.
    fn finish(self, rec: &mut TileNeed) -> Option<TileInfo> {
        let ((m, n, q), (_, last)) = (self.best?, self.last?);
        rec.membership = m;
        rec.need.assign(n);
        rec.q = q;
        rec.listed = self.listed;
        rec.rejected = self.rejected;
        rec.blockers = self.blockers;
        Some(TileInfo::new(last.parent, last.level, last.rect))
    }
}

/// Where a change's entries come from.
#[derive(Clone, Copy)]
enum Source<'s> {
    Delta(&'s NeedDelta),
    /// A whole need, its entries grouped by tile.
    Full(&'s NeedSet, &'s [(TileId, u32)]),
}

/// The need the budget keeps, and the buffers that change it.
#[derive(Debug, Default)]
pub(crate) struct NeedState {
    tiles: SortedMap<TileId, TileNeed>,
    listed: usize,
    rejected: usize,
    blockers: usize,
    scratch: Scratch,
    /// The whole need, kept beside in debug builds, to check the state
    /// against a recomputation from it.
    #[cfg(debug_assertions)]
    shadow: NeedSet,
}

/// A change's buffers: emptied, kept across steps.
#[derive(Debug, Default)]
struct Scratch {
    /// A delta's words, sorted, when it does not come sorted.
    ops: Vec<(TileId, DeltaOp)>,
    /// The last change's tiles, in order, each with the tree position it
    /// had before (`None`: none known) and whether it was wanted before.
    named: Vec<Named>,
    /// A changed tile's layers before the change.
    layers: Vec<super::types::LayerId>,
    /// The tiles whose records go.
    gone: Vec<TileId>,
    /// The tiles a whole need no longer lists, between two it lists.
    left: Vec<TileId>,
}

impl NeedState {
    /// Each wanted tile's best entry, in tile order.
    pub fn tiles(&self) -> &SortedMap<TileId, TileNeed> {
        &self.tiles
    }

    /// What its map spent keeping its order since the last call.
    pub fn take_upkeep(&mut self) -> u64 {
        self.tiles.take_upkeep()
    }

    /// Its entries with a float that was not finite.
    pub fn rejected(&self) -> usize {
        self.rejected
    }

    /// Its entries in the blockers' list.
    pub fn blockers(&self) -> usize {
        self.blockers
    }

    /// Drops the buffers a change reuses.
    pub fn release_scratch(&mut self) {
        self.scratch = Scratch::default();
    }

    /// The tiles the last change named — entered, changed or left — in
    /// tile order, each with the tree position it had before and whether
    /// it was wanted before. A whole need names the tiles it lists and
    /// those it no longer lists.
    pub fn named(&self) -> &[Named] {
        &self.scratch.named
    }

    /// Applies what changed; `positions` gets the tree position of every
    /// tile that entered or changed. Returns the tiles it names.
    pub fn apply_delta(
        &mut self,
        d: &NeedDelta,
        positions: &mut SortedMap<TileId, TileInfo>,
    ) -> u32 {
        let named = sat_u32(d.tiles());
        match d.sorted_ops() {
            Some(ops) => self.apply(ops, Source::Delta(d), false, positions),
            None => {
                let mut ops = std::mem::take(&mut self.scratch.ops);
                d.ops_into(&mut ops);
                self.apply(ops.iter().copied(), Source::Delta(d), false, positions);
                self.scratch.ops = ops;
            }
        }
        #[cfg(debug_assertions)]
        {
            d.apply_to(&mut self.shadow);
            self.check();
        }
        named
    }

    /// Becomes the whole need `n`: every tile resolved again, those it no
    /// longer lists gone; `positions` gets every tile's tree position.
    /// Returns its entries.
    pub fn apply_full(&mut self, n: &NeedSet, positions: &mut SortedMap<TileId, TileInfo>) -> u32 {
        // A whole need is the first step's, or a resynchronisation's: its
        // grouping is not kept.
        let mut grouped = Vec::new();
        n.grouped_into(&mut grouped);
        let runs = Runs { g: &grouped, i: 0 };
        self.apply(runs, Source::Full(n, &grouped), true, positions);
        #[cfg(debug_assertions)]
        {
            self.shadow.clone_from(n);
            self.check();
        }
        sat_u32(n.len())
    }

    /// Applies `ops`, one per tile, in tile order. With `whole`, the tiles
    /// `ops` does not name leave. Each tile is found, changed, added or
    /// removed by its key: what a change costs is its own place.
    fn apply(
        &mut self,
        ops: impl Iterator<Item = (TileId, DeltaOp)>,
        src: Source<'_>,
        whole: bool,
        positions: &mut SortedMap<TileId, TileInfo>,
    ) {
        let mut named = std::mem::take(&mut self.scratch.named);
        named.clear();
        let mut old_layers = std::mem::take(&mut self.scratch.layers);
        let mut gone = std::mem::take(&mut self.scratch.gone);
        gone.clear();
        let mut left = std::mem::take(&mut self.scratch.left);
        let mut after: Option<TileId> = None;
        for (t, op) in ops {
            if whole {
                // The tiles between the last named and this one leave.
                self.tiles.keys_between(after, Some(t), &mut left);
                for &g in &left {
                    self.uncount_tile(g);
                    gone.push(g);
                    Self::name_left(g, positions, &mut named);
                }
            }
            after = Some(t);
            let place = self.tiles.locate(&t);
            let found = place.is_some();
            let mut r = Resolve::default();
            match (op, src) {
                (DeltaOp::Leave, _) => {}
                (op, Source::Delta(d)) => {
                    for (_, m, n) in d.group(op) {
                        r.feed(*m, n);
                    }
                }
                (DeltaOp::Set { start, end, .. }, Source::Full(n, grouped)) => {
                    let at = |x: u32| usize::try_from(x).unwrap_or(usize::MAX);
                    for &(_, seq) in &grouped[at(start)..at(end)] {
                        if let Some((m, _, need)) = n.entry(at(seq)) {
                            r.feed(m, need);
                        }
                    }
                }
            }
            let mut shaped = true;
            let info = if let Some(at) = place {
                let old = self.tiles.at(at);
                let (m, level, class) = (old.membership, old.need.level, old.need.class);
                let had = old.need.imagery.is_some();
                old_layers.clear();
                old_layers.extend(
                    old.need
                        .imagery
                        .iter()
                        .flat_map(|i| i.layers().iter().map(|x| x.layer)),
                );
                let rec = self.tiles.at_mut(at);
                let counts = Self::uncount(rec);
                let info = r.finish(rec);
                let new = &*rec;
                shaped = info.is_none()
                    || new.membership != m
                    || new.need.level != level
                    || new.need.class != class
                    || new.need.imagery.is_some() != had
                    || !new
                        .need
                        .imagery
                        .iter()
                        .flat_map(|i| i.layers().iter().map(|x| x.layer))
                        .eq(old_layers.iter().copied());
                let added = match info {
                    Some(_) => Self::counts(new),
                    None => {
                        gone.push(t);
                        (0, 0, 0)
                    }
                };
                self.recount(counts, added);
                info
            } else {
                let mut rec = TileNeed::default();
                let info = r.finish(&mut rec);
                if info.is_some() {
                    let n = |x: u32| usize::try_from(x).unwrap_or(usize::MAX);
                    self.listed += n(rec.listed);
                    self.rejected += n(rec.rejected);
                    self.blockers += n(rec.blockers);
                    self.tiles.insert(t, rec);
                }
                info
            };
            let pos = positions.locate(&t);
            let before = pos.map(|p| *positions.at(p));
            let place = |x: &TileInfo| (x.parent(), x.level, x.rect());
            let moved = match (&before, &info) {
                (Some(b), Some(n)) => place(b) != place(n),
                (None, Some(_)) => true,
                (_, None) => false,
            };
            named.push(Named::new(t, before, found, shaped, moved));
            if let Some(info) = info {
                match pos {
                    // What the prune counts of the tile is not the need's.
                    Some(at) => {
                        let p = positions.at_mut(at);
                        let slots = p.slots;
                        *p = info;
                        p.slots = slots;
                    }
                    None => {
                        positions.insert(t, info);
                    }
                }
            }
        }
        if whole {
            self.tiles.keys_between(after, None, &mut left);
            for &g in &left {
                self.uncount_tile(g);
                gone.push(g);
                Self::name_left(g, positions, &mut named);
            }
        }
        for g in &gone {
            self.tiles.remove(g);
        }
        self.scratch.named = named;
        self.scratch.layers = old_layers;
        self.scratch.gone = gone;
        self.scratch.left = left;
    }

    /// Names `t`, wanted, which a whole need no longer lists: it left,
    /// with the tree position it had.
    fn name_left(t: TileId, positions: &SortedMap<TileId, TileInfo>, named: &mut Vec<Named>) {
        named.push(Named::new(t, positions.get(&t).copied(), true, true, false));
    }

    /// A record's entries as counted: listed, rejected, blockers.
    fn counts(rec: &TileNeed) -> (usize, usize, usize) {
        let n = |x: u32| usize::try_from(x).unwrap_or(usize::MAX);
        (n(rec.listed), n(rec.rejected), n(rec.blockers))
    }

    /// Takes a record's entries off it (its counts returned, for
    /// [`NeedState::recount`]).
    fn uncount(rec: &mut TileNeed) -> (usize, usize, usize) {
        let c = Self::counts(rec);
        rec.listed = 0;
        c
    }

    /// The counts, a record's old entries off them, its new ones on.
    fn recount(&mut self, off: (usize, usize, usize), on: (usize, usize, usize)) {
        self.listed = self.listed - off.0 + on.0;
        self.rejected = self.rejected - off.1 + on.1;
        self.blockers = self.blockers - off.2 + on.2;
    }

    /// Takes the entries of tile `t`'s record off the counts.
    fn uncount_tile(&mut self, t: TileId) {
        if let Some(at) = self.tiles.locate(&t) {
            let off = Self::uncount(self.tiles.at_mut(at));
            self.recount(off, (0, 0, 0));
        }
    }

    /// The state against a recomputation from the whole need it stands
    /// for: every tile resolved afresh, every count summed afresh.
    #[cfg(debug_assertions)]
    fn check(&self) {
        let mut fresh = NeedState::default();
        let mut positions = SortedMap::default();
        let mut grouped = Vec::new();
        self.shadow.grouped_into(&mut grouped);
        fresh.apply(
            Runs { g: &grouped, i: 0 },
            Source::Full(&self.shadow, &grouped),
            true,
            &mut positions,
        );
        assert_eq!(
            self.tiles.iter().collect::<Vec<_>>(),
            fresh.tiles.iter().collect::<Vec<_>>(),
            "the need kept is the need recomputed"
        );
        let sum = |f: fn(&TileNeed) -> u32| {
            self.tiles
                .iter()
                .map(|(_, r)| usize::try_from(f(r)).unwrap_or(usize::MAX))
                .sum::<usize>()
        };
        assert_eq!(
            (self.listed, self.rejected, self.blockers),
            (sum(|r| r.listed), sum(|r| r.rejected), sum(|r| r.blockers)),
            "the need's counts are its tiles'"
        );
        assert_eq!(self.listed, self.shadow.len(), "every entry counted");
    }

    /// In debug builds, the tree position of every wanted tile, as the
    /// whole need gives it: for the step to check what it remembers.
    #[cfg(debug_assertions)]
    pub fn positions(&self) -> Vec<(TileId, TileInfo)> {
        let mut grouped = Vec::new();
        self.shadow.grouped_into(&mut grouped);
        let mut out = Vec::new();
        for (t, op) in (Runs { g: &grouped, i: 0 }) {
            let DeltaOp::Set { end, .. } = op else {
                continue;
            };
            // The places are sorted within a tile: the last is the last
            // in the need's order.
            let last = grouped[usize::try_from(end).unwrap_or(usize::MAX) - 1].1;
            let at = usize::try_from(last).unwrap_or(usize::MAX);
            if let Some((_, _, n)) = self.shadow.entry(at) {
                out.push((t, TileInfo::new(n.parent, n.level, n.rect)));
            }
        }
        out
    }
}

/// A whole need's tiles, as words: each tile's run of grouped entries.
struct Runs<'g> {
    g: &'g [(TileId, u32)],
    i: usize,
}

impl Iterator for Runs<'_> {
    type Item = (TileId, DeltaOp);

    fn next(&mut self) -> Option<Self::Item> {
        let t = self.g.get(self.i)?.0;
        let end = self.i + self.g[self.i..].iter().take_while(|e| e.0 == t).count();
        let op = DeltaOp::Set {
            list: 2,
            start: sat_u32(self.i),
            end: sat_u32(end),
        };
        self.i = end;
        Some((t, op))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_arithmetic)]
mod tests {
    use super::*;
    use crate::imagery::TexelFootprint;
    use crate::types::LayerId;
    use crate::LayerReach;

    /// SplitMix64.
    struct Mix(u64);
    impl Mix {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn tile(i: u64) -> TileId {
        TileId::from_terrain(3, i % 8, i / 8)
    }

    fn need_of(m: &mut Mix) -> Need {
        let mut n = Need::new(u32::try_from(m.below(4)).unwrap(), Class::TerrainMesh)
            .with_sse_excess(m.below(5) as f32 - 2.0)
            .with_centrality(m.below(3) as f32 / 2.0)
            .with_distance(m.below(3) as f32 * 100.0);
        if m.below(2) == 0 {
            n = n.with_parent(tile(m.below(4)));
        }
        if m.below(2) == 0 {
            let fp = TexelFootprint::new(1.0 + m.below(3) as f64, 1.0, 1.0, 1.0, 1.0).unwrap();
            n = n.with_imagery(ImageryNeed {
                footprint: fp,
                layers: vec![LayerReach {
                    layer: LayerId(u16::try_from(m.below(2)).unwrap()),
                    max_level: 5,
                }],
            });
        }
        n
    }

    /// A need over 16 tiles, each listed in a list or two, sometimes twice
    /// in one (ties included: a copy of the entry before), the lists
    /// shuffled.
    fn need(m: &mut Mix) -> NeedSet {
        let mut n = NeedSet::new();
        for i in 0..16 {
            for _ in 0..m.below(3) {
                let list = Membership::ALL[usize::try_from(m.below(4)).unwrap()];
                let entry = need_of(m);
                n.list_mut(list).push((tile(i), entry.clone()));
                if m.below(6) == 0 {
                    n.list_mut(list).push((tile(i), entry));
                }
            }
        }
        for list in Membership::ALL {
            let l = n.list_mut(list);
            let len = l.len();
            if len > 1 {
                for k in 0..len {
                    let j = usize::try_from(m.below(len as u64)).unwrap();
                    l.swap(k, j);
                }
            }
        }
        n
    }

    /// The state's tiles, counts and the positions its last change gave.
    fn seen(s: &NeedState) -> (Vec<(TileId, TileNeed)>, [usize; 3]) {
        (
            s.tiles().iter().map(|(t, r)| (*t, r.clone())).collect(),
            [s.listed, s.rejected(), s.blockers()],
        )
    }

    /// A sequence of needs, each a small change of the one before or a new
    /// one: told by deltas, the kept need is the one the whole needs give,
    /// tile for tile, and so is every wanted tile's tree position.
    #[test]
    fn the_need_kept_through_deltas_is_the_whole_needs() {
        let mut m = Mix(7);
        let mut by_delta = NeedState::default();
        let mut positions = SortedMap::default();
        let mut prev = NeedSet::new();
        let mut delta = NeedDelta::new();
        for step in 0..400 {
            let next = if step % 7 == 0 {
                need(&mut m)
            } else {
                let mut n = prev.clone();
                // A few entries change one field, or move, or go.
                for _ in 0..m.below(4) {
                    let list = Membership::ALL[usize::try_from(m.below(4)).unwrap()];
                    let len = n.list(list).len();
                    if len == 0 {
                        continue;
                    }
                    let i = usize::try_from(m.below(len as u64)).unwrap();
                    let fresh = need_of(&mut m);
                    let e = &mut n.list_mut(list)[i].1;
                    match m.below(6) {
                        0 => e.sse_excess = fresh.sse_excess,
                        1 => e.distance = fresh.distance,
                        2 => e.centrality = fresh.centrality,
                        3 => e.parent = fresh.parent,
                        4 => {
                            let (t, e) = n.list_mut(list).remove(i);
                            let to = Membership::ALL[usize::try_from(m.below(4)).unwrap()];
                            n.list_mut(to).push((t, e));
                        }
                        _ => {
                            n.list_mut(list).remove(i);
                        }
                    }
                }
                n
            };
            delta.diff(&prev, &next);
            by_delta.apply_delta(&delta, &mut positions);
            let mut whole = NeedState::default();
            let mut fresh = SortedMap::default();
            whole.apply_full(&next, &mut fresh);
            assert_eq!(seen(&by_delta), seen(&whole), "step {step}");
            for (t, info) in fresh.iter() {
                assert_eq!(positions.get(t), Some(info), "step {step}: {t:?}");
            }
            prev = next;
        }
    }

    /// A step told the whole need after deltas becomes it, and reports
    /// every tile.
    #[test]
    fn a_whole_need_resynchronises() {
        let mut m = Mix(11);
        let mut s = NeedState::default();
        let mut positions = SortedMap::default();
        let a = need(&mut m);
        let b = need(&mut m);
        s.apply_delta(&NeedDelta::between(&NeedSet::new(), &a), &mut positions);
        s.apply_full(&b, &mut positions);
        let mut whole = NeedState::default();
        let mut fresh = SortedMap::default();
        whole.apply_full(&b, &mut fresh);
        assert_eq!(seen(&s), seen(&whole));
        for (t, info) in fresh.iter() {
            assert_eq!(positions.get(t), Some(info));
        }
    }

    /// Of a tile's entries the best wins, the first of equals; its tree
    /// position is its last entry's.
    #[test]
    fn a_tile_resolves_as_the_whole_need_sorted_it() {
        let t = tile(1);
        let a = Need::new(2, Class::TerrainMesh).with_sse_excess(1.0);
        // Equal in the order, another footprint (not in it): a tie.
        let b = a.clone().with_rect(rect(1.0));
        let ring = Need::new(5, Class::TerrainMesh).with_sse_excess(9.0);
        let mut n = NeedSet::new();
        n.drawn.push((t, a.clone()));
        n.drawn.push((t, b.clone()));
        n.ring.push((t, ring.clone()));
        let mut s = NeedState::default();
        let mut positions = SortedMap::default();
        s.apply_full(&n, &mut positions);
        let rec = s.tiles().get(&t).unwrap();
        assert_eq!(rec.membership, Membership::Drawn);
        assert_eq!(rec.need.level, 2);
        let level = positions.get(&t).unwrap().level;
        assert_eq!(level, 5, "the ring entry, listed last");
        assert_eq!((s.listed, s.blockers()), (3, 0));
    }

    fn rect(w: f64) -> crate::GeoRect {
        crate::GeoRect {
            west: w,
            south: 0.0,
            east: w + 1.0,
            north: 1.0,
        }
    }
}
