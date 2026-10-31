// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The maps the budget keeps from one step to the next, changed by what a
//! step touches.
//!
//! [`SortedMap`] is an ordered map kept as sorted blocks of at most
//! [`BLOCK`] contiguous entries, with the first key of each block in a
//! sorted index of its own. A lookup is two binary searches over contiguous
//! memory, as in the single sorted vector it replaces; but a change shifts
//! the entries after it in its block only, and the index when a block
//! splits, merges or goes. A single sorted vector shifted every entry after
//! a change — the whole map for a batch — whatever the batch's size: on the
//! settling steps of the simulation, 1 to 54 changes moved 250 to 6 000
//! entries per map, outside the step's `work`.
//!
//! Blocks emptied are kept in a pool and taken again before any is
//! allocated, so nothing is allocated once the maps have grown to the
//! view. Adjacent blocks hold more than half a block between them (a block
//! that falls under it merges with a neighbour), so the index stays at most
//! four entries per [`BLOCK`] entries of the map.
//!
//! Every map counts what keeping its order costs — the entries it shifts
//! or moves, within a block, between blocks and in the index, and every
//! entry of a pass over the whole map — in a counter of its own that its
//! owner takes ([`SortedMap::take_upkeep`]) into the step's `work`.

/// The most entries a block holds. 64: measured against 32 and 128 on the
/// simulation's tour and `decide`'s bench (see the roadmap's O1 section).
pub(super) const BLOCK: usize = 64;

/// Two neighbours holding this many entries or fewer become one block.
const MERGE: usize = BLOCK - BLOCK / 8;

/// An ordered map kept from one step to the next (see the module
/// documentation). Its accessors mirror `BTreeMap`'s: entries come as
/// `(&K, &V)`, in key order.
pub(super) struct SortedMap<K, V> {
    /// The blocks, in key order, each sorted, none empty.
    blocks: Vec<Vec<(K, V)>>,
    /// The first key of each block.
    heads: Vec<K>,
    len: usize,
    /// Blocks emptied, their room kept.
    pool: Vec<Vec<(K, V)>>,
    /// What keeping the order cost since the owner last took it.
    upkeep: u64,
}

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        SortedMap {
            blocks: Vec::new(),
            heads: Vec::new(),
            len: 0,
            pool: Vec::new(),
            upkeep: 0,
        }
    }
}

impl<K: Ord + Copy + std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for SortedMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Ord + Copy, V: Clone> Clone for SortedMap<K, V> {
    fn clone(&self) -> Self {
        SortedMap {
            blocks: self.blocks.clone(),
            heads: self.heads.clone(),
            len: self.len,
            pool: Vec::new(),
            upkeep: 0,
        }
    }
}

impl<K: Ord + Copy, V: PartialEq> PartialEq for SortedMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

/// A map's entries, in key order, with their count: what is collected
/// from it is allocated once.
pub(super) struct Iter<'a, K, V> {
    blocks: std::slice::Iter<'a, Vec<(K, V)>>,
    block: std::slice::Iter<'a, (K, V)>,
    left: usize,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(e) = self.block.next() {
                self.left -= 1;
                return Some((&e.0, &e.1));
            }
            self.block = self.blocks.next()?.iter();
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}

impl<K: Ord + Copy, V> SortedMap<K, V> {
    /// Counts `n` entries shifted, moved or passed to keep the order.
    fn kept(&mut self, n: usize) {
        self.upkeep = self.upkeep.saturating_add(n as u64);
    }

    /// What keeping the order cost since the last call: for the step's
    /// `work`.
    pub fn take_upkeep(&mut self) -> u64 {
        std::mem::take(&mut self.upkeep)
    }

    /// The block `k` belongs in: the last whose first key is not after it,
    /// the first when every block's is. `None` when there is none.
    fn block_of(&self, k: &K) -> Option<usize> {
        if self.blocks.is_empty() {
            return None;
        }
        Some(self.heads.partition_point(|h| h <= k).saturating_sub(1))
    }

    /// Where `k` is: its block, and its place in the block or where it
    /// would go.
    fn find(&self, k: &K) -> Option<(usize, Result<usize, usize>)> {
        let b = self.block_of(k)?;
        Some((b, self.blocks[b].binary_search_by(|e| e.0.cmp(k))))
    }

    /// The value at `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        match self.find(k)? {
            (b, Ok(i)) => Some(&self.blocks[b][i].1),
            _ => None,
        }
    }

    /// Whether `k` has a value.
    pub fn contains_key(&self, k: &K) -> bool {
        matches!(self.find(k), Some((_, Ok(_))))
    }

    /// The value at `k`, to change in place.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        match self.find(k)? {
            (b, Ok(i)) => Some(&mut self.blocks[b][i].1),
            _ => None,
        }
    }

    /// The entries, in key order.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            blocks: self.blocks.iter(),
            block: [].iter(),
            left: self.len,
        }
    }

    /// The keys, in order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + '_ {
        self.iter().map(|e| e.0)
    }

    /// The values, in key order, to change in place (their keys stay).
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> + '_ {
        self.blocks.iter_mut().flatten().map(|e| &mut e.1)
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entries with keys in `r`, in order.
    pub fn range(&self, r: std::ops::RangeInclusive<K>) -> impl Iterator<Item = (&K, &V)> + '_ {
        let (lo, hi) = r.into_inner();
        let (b, i) = match self.find(&lo) {
            Some((b, Ok(i) | Err(i))) => (b, i),
            None => (0, 0),
        };
        self.blocks
            .get(b..)
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .flat_map(move |(j, blk)| blk[if j == 0 { i } else { 0 }..].iter())
            .take_while(move |e| e.0 <= hi)
            .map(|e| (&e.0, &e.1))
    }

    /// The keys strictly between `after` and `before` (`None`: unbounded),
    /// in order, into `out`: an entry passed for each.
    pub fn keys_between(&mut self, after: Option<K>, before: Option<K>, out: &mut Vec<K>) {
        out.clear();
        let (b, i) = match after.and_then(|a| self.find(&a)) {
            Some((b, Ok(i))) => (b, i + 1),
            Some((b, Err(i))) => (b, i),
            None => (0, 0),
        };
        let blocks = self.blocks.get(b..).unwrap_or(&[]);
        out.extend(
            blocks
                .iter()
                .enumerate()
                .flat_map(|(j, blk)| blk[if j == 0 { i.min(blk.len()) } else { 0 }..].iter())
                .map(|e| e.0)
                .take_while(|k| before.is_none_or(|x| *k < x)),
        );
        self.kept(out.len());
    }

    /// A block from the pool, or a new one.
    fn fresh(&mut self) -> Vec<(K, V)> {
        self.pool.pop().unwrap_or_default()
    }

    /// Sets `k` to `v`; returns the value it replaces. A value replaced in
    /// place costs nothing beyond finding it; a new key, the entries it
    /// shifts in its block, and the block split and the index's entries
    /// shifted when its block is full.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let Some((b, at)) = self.find(&k) else {
            let mut blk = self.fresh();
            blk.push((k, v));
            self.blocks.push(blk);
            self.heads.push(k);
            self.len = 1;
            self.kept(1);
            return None;
        };
        let i = match at {
            Ok(i) => return Some(std::mem::replace(&mut self.blocks[b][i].1, v)),
            Err(i) => i,
        };
        self.len += 1;
        if self.blocks[b].len() < BLOCK {
            self.kept(self.blocks[b].len() - i + 1);
            self.blocks[b].insert(i, (k, v));
            if i == 0 {
                self.heads[b] = k;
            }
            return None;
        }
        // Full, a neighbour with room: one entry passes to it, and the key
        // goes in (blocks stay nearly full, as a single vector's room is).
        if b + 1 < self.blocks.len() && self.blocks[b + 1].len() < BLOCK {
            if i == BLOCK {
                self.kept(self.blocks[b + 1].len() + 1);
                self.blocks[b + 1].insert(0, (k, v));
            } else {
                let Some(last) = self.blocks[b].pop() else {
                    unreachable!("a full block has a last entry")
                };
                self.kept(self.blocks[b + 1].len() + 1 + BLOCK - i);
                self.blocks[b + 1].insert(0, last);
                self.blocks[b].insert(i, (k, v));
                self.heads[b] = self.blocks[b][0].0;
            }
            self.heads[b + 1] = self.blocks[b + 1][0].0;
            return None;
        }
        if b > 0 && self.blocks[b - 1].len() < BLOCK && i > 0 {
            let first = self.blocks[b].remove(0);
            self.kept(BLOCK + 1 + i);
            self.blocks[b - 1].push(first);
            self.blocks[b].insert(i - 1, (k, v));
            self.heads[b] = self.blocks[b][0].0;
            return None;
        }
        // Full, the key past its last: a new block for it, this one left
        // full (keys that come in order fill their blocks).
        if i == BLOCK {
            let mut blk = self.fresh();
            blk.push((k, v));
            self.kept(1 + self.blocks.len() - b);
            self.heads.insert(b + 1, k);
            self.blocks.insert(b + 1, blk);
            return None;
        }
        // Full: its upper half goes to a new block after it.
        let mut upper = self.fresh();
        upper.extend(self.blocks[b].drain(BLOCK / 2..));
        self.kept(BLOCK - BLOCK / 2 + self.blocks.len() - b);
        self.heads.insert(b + 1, upper[0].0);
        self.blocks.insert(b + 1, upper);
        let (into, i) = if i <= BLOCK / 2 {
            (b, i)
        } else {
            (b + 1, i - BLOCK / 2)
        };
        self.kept(self.blocks[into].len() - i + 1);
        self.blocks[into].insert(i, (k, v));
        self.heads[into] = self.blocks[into][0].0;
        None
    }

    /// Removes `k`; returns its value. Costs the entries it shifts in its
    /// block, and a merge's entries and the index's when its block falls
    /// under half a block with a neighbour.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let (b, Ok(i)) = self.find(k)? else {
            return None;
        };
        self.len -= 1;
        self.kept(self.blocks[b].len() - i);
        let (_, v) = self.blocks[b].remove(i);
        if self.blocks[b].is_empty() {
            self.drop_block(b);
            return Some(v);
        }
        self.heads[b] = self.blocks[b][0].0;
        // Two neighbours that fit in half a block become one.
        let fits = MERGE;
        if b + 1 < self.blocks.len() && self.blocks[b].len() + self.blocks[b + 1].len() <= fits {
            self.merge(b);
        } else if b > 0 && self.blocks[b - 1].len() + self.blocks[b].len() <= fits {
            self.merge(b - 1);
        }
        Some(v)
    }

    /// Block `b + 1` appended to block `b`, and dropped.
    fn merge(&mut self, b: usize) {
        let mut next = std::mem::take(&mut self.blocks[b + 1]);
        self.kept(next.len());
        self.blocks[b].append(&mut next);
        self.blocks[b + 1] = next;
        self.drop_block(b + 1);
    }

    /// Empty block `b` out of the map, into the pool.
    fn drop_block(&mut self, b: usize) {
        self.kept(self.blocks.len() - b);
        let mut blk = self.blocks.remove(b);
        self.heads.remove(b);
        blk.clear();
        if self.pool.len() < self.pool_room() {
            self.pool.push(blk);
        }
    }

    /// The pool's bound: two blocks, and an eighth of the map's — a map
    /// that shrank gives the rest back.
    fn pool_room(&self) -> usize {
        1
    }

    /// Empties the map, its blocks kept in the pool.
    pub fn clear(&mut self) {
        self.kept(self.blocks.len());
        for mut blk in self.blocks.drain(..) {
            blk.clear();
            self.pool.push(blk);
        }
        self.heads.clear();
        self.len = 0;
    }

    /// The map made of `entries` — sorted by key, distinct — in place of its
    /// own: each entry passed once.
    pub fn rebuild(&mut self, entries: impl IntoIterator<Item = (K, V)>) {
        self.clear();
        for e in entries {
            if self.blocks.last().is_none_or(|b| b.len() == BLOCK) {
                let blk = self.fresh();
                self.heads.push(e.0);
                self.blocks.push(blk);
            }
            if let Some(last) = self.blocks.last_mut() {
                debug_assert!(last.last().is_none_or(|x| x.0 < e.0), "sorted, distinct");
                last.push(e);
            }
            self.len += 1;
            self.kept(1);
        }
        let room = self.pool_room();
        self.pool.truncate(room);
    }

    /// Keeps the entries `f` keeps: a pass over every entry, the blocks
    /// left empty dropped.
    pub fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool) {
        self.kept(self.len);
        let mut b = 0;
        while b < self.blocks.len() {
            self.blocks[b].retain_mut(|e| f(&e.0, &mut e.1));
            if self.blocks[b].is_empty() {
                self.drop_block(b);
            } else {
                self.heads[b] = self.blocks[b][0].0;
                b += 1;
            }
        }
        self.len = self.blocks.iter().map(Vec::len).sum();
    }

    /// Applies `changes` — a value to set at a key, or `None` to remove the
    /// key — in order, the last change of a key winning; `changes` is left
    /// empty. `spent` receives each value replaced or removed. Each change
    /// costs what an insertion or a removal does: a batch costs its changes.
    pub fn apply(&mut self, changes: &mut Vec<(K, Option<V>)>, mut spent: impl FnMut(V)) {
        if changes.is_empty() {
            return;
        }
        let used = changes.len();
        for (k, v) in changes.drain(..) {
            let old = match v {
                Some(v) => self.insert(k, v),
                None => self.remove(&k),
            };
            if let Some(o) = old {
                spent(o);
            }
        }
        trim(changes, used);
    }

    /// The map's invariants: blocks sorted and chained, none empty or over
    /// a block, the index their first keys, neighbours over half a block
    /// between them, the length their sum.
    #[cfg(test)]
    fn consistent(&self) -> bool {
        let sorted = self.keys().zip(self.keys().skip(1)).all(|(a, b)| a < b);
        let sizes = self
            .blocks
            .iter()
            .all(|b| !b.is_empty() && b.len() <= BLOCK);
        let heads = self.heads.len() == self.blocks.len()
            && self
                .heads
                .iter()
                .zip(&self.blocks)
                .all(|(h, b)| *h == b[0].0);
        let len = self.len == self.blocks.iter().map(Vec::len).sum::<usize>();
        sorted && sizes && heads && len
    }
}

impl<'a, K: Ord + Copy, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Gives back most of `v`'s room when it is four times what was `used` of
/// it: a buffer grown by one large step does not stay that large, and one
/// used about as much each step is left alone.
pub(super) fn trim<T>(v: &mut Vec<T>, used: usize) {
    if v.capacity() > 4 * used + 256 {
        v.shrink_to(2 * used + 64);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    /// One operation on a map of small keys.
    #[derive(Clone, Debug)]
    enum Op {
        Insert(u16, u32),
        Remove(u16),
        Apply(Vec<(u16, Option<u32>)>),
        Rebuild(Vec<u16>),
        Retain(u16),
        Clear,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            8 => (0u16..600, any::<u32>()).prop_map(|(k, v)| Op::Insert(k, v)),
            6 => (0u16..600).prop_map(Op::Remove),
            4 => prop::collection::vec((0u16..600, prop::option::of(any::<u32>())), 0..120)
                .prop_map(Op::Apply),
            1 => prop::collection::btree_set(0u16..600, 0..400)
                .prop_map(|s| Op::Rebuild(s.into_iter().collect())),
            1 => (1u16..7).prop_map(Op::Retain),
            1 => Just(Op::Clear),
        ]
    }

    proptest! {
        /// Any sequence of changes leaves the map as the standard library's
        /// ordered map: its entries, lookups, ranges and the keys between
        /// two bounds, and the values a change replaces or removes.
        #[test]
        fn a_sorted_map_is_the_standard_ordered_map(
            ops in prop::collection::vec(op(), 1..120),
            probes in prop::collection::vec((0u16..620, 0u16..200), 8),
        ) {
            let mut m: SortedMap<u16, u32> = SortedMap::default();
            let mut o: BTreeMap<u16, u32> = BTreeMap::new();
            for op in ops {
                match op {
                    Op::Insert(k, v) => prop_assert_eq!(m.insert(k, v), o.insert(k, v)),
                    Op::Remove(k) => prop_assert_eq!(m.remove(&k), o.remove(&k)),
                    Op::Apply(mut c) => {
                        let mut want = Vec::new();
                        for (k, v) in &c {
                            let old = match v {
                                Some(v) => o.insert(*k, *v),
                                None => o.remove(k),
                            };
                            want.extend(old);
                        }
                        let mut spent = Vec::new();
                        m.apply(&mut c, |v| spent.push(v));
                        prop_assert!(c.is_empty());
                        prop_assert_eq!(spent, want);
                    }
                    Op::Rebuild(keys) => {
                        m.rebuild(keys.iter().map(|k| (*k, u32::from(*k))));
                        o = keys.iter().map(|k| (*k, u32::from(*k))).collect();
                    }
                    Op::Retain(d) => {
                        m.retain(|k, _| k % d == 0);
                        o.retain(|k, _| k % d == 0);
                    }
                    Op::Clear => {
                        m.clear();
                        o.clear();
                    }
                }
                prop_assert_eq!(m.len(), o.len());
                prop_assert!(m.iter().eq(o.iter()));
                prop_assert!(m.consistent());
            }
            let mut between = Vec::new();
            for (lo, span) in probes {
                let hi = lo + span;
                prop_assert_eq!(m.get(&lo), o.get(&lo));
                prop_assert_eq!(m.contains_key(&hi), o.contains_key(&hi));
                prop_assert!(m.range(lo..=hi).eq(o.range(lo..=hi)));
                m.keys_between(Some(lo), Some(hi), &mut between);
                let want: Vec<u16> = if lo < hi {
                    o.range((std::ops::Bound::Excluded(lo), std::ops::Bound::Excluded(hi)))
                        .map(|(k, _)| *k)
                        .collect()
                } else {
                    Vec::new()
                };
                prop_assert_eq!(&between, &want);
                m.keys_between(None, Some(hi), &mut between);
                prop_assert!(between.iter().eq(o.range(..hi).map(|(k, _)| k)));
                m.keys_between(Some(lo), None, &mut between);
                prop_assert!(between.iter().eq(o.range((std::ops::Bound::Excluded(lo), std::ops::Bound::Unbounded)).map(|(k, _)| k)));
            }
        }
    }
}
