// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The maps the budget keeps from one step to the next, changed by what a
//! step touches.
//!
//! [`SortedMap`] is an ordered map whose change costs its own path in the
//! tree: a B-tree whose nodes live in a slab (`btree-slab`), so a node freed
//! is the next one taken and nothing is allocated once the maps have grown
//! to the view. A sorted vector (what it replaced) keeps its room too, but
//! an insertion or a removal shifts every entry after it: O(view) per
//! change, which the step's `work` did not count.
//!
//! Why `btree-slab`, pre-1.0, in the wasm path (rule 8). The standard
//! library's `BTreeMap` allocates a node on a split and frees one on a
//! merge: measured on the tour, the internal allocations per step go
//! p99 31 -> 335. No 1.0 crate keeps a B-tree's nodes in a pool. Of the
//! pooled ones, `btree-slab` (MIT OR Apache-2.0, 6.6 M downloads, last
//! release 2023-10) builds on `slab` and `smallvec`, both established; its
//! 28 `unsafe` are its items' `MaybeUninit`, the re-borrows of its mutable
//! iterators and one pointer re-borrow in its entry API.
//! `btree-plus-store` (0.2.1) carries 166 `unsafe` and an arena crate of
//! its own (`rustc-arena-modified`). Two bugs found in 0.6.1 — `range` on a
//! bound that is not a key ran off the tree, `retain` stopped early — are
//! fixed in lapoule-dev's fork, which also grows the slab by eighths (the
//! map's room stays within an eighth of its nodes) and is pinned by rev.
//! The map is checked against the standard library's by random sequences
//! (the tests below), the fork's own tests likewise.
//!
//! Every map counts what keeping its order cost ([`upkeep`]), for the
//! step's `work`.

thread_local! {
    /// The entries this thread's maps passed over to keep their order
    /// ([`upkeep`]).
    static UPKEEP: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts `n` entries passed over to keep a map in order.
fn kept(n: usize) {
    let _ = UPKEEP.try_with(|c| c.set(c.get().saturating_add(n as u64)));
}

/// What this thread's maps have passed over to keep their order so far: a
/// change's path in a tree, a batch's pass, a merge's rotations. A step
/// adds what it spent to its `work`.
pub(super) fn upkeep() -> u64 {
    UPKEEP.try_with(std::cell::Cell::get).unwrap_or(0)
}

/// A change's path in a tree of `n` entries: ⌈log2(n + 2)⌉.
fn path(n: usize) -> usize {
    (usize::BITS - (n + 1).leading_zeros()) as usize
}

/// The slab a map's nodes live in.
type Nodes<K, V> = btree_slab::SlabByEighths<btree_slab::generic::Node<K, V>>;

/// An ordered map kept from one step to the next. Its accessors mirror
/// `BTreeMap`'s: entries come as `(&K, &V)`, in key order.
pub(super) struct SortedMap<K, V>(btree_slab::BTreeMapByEighths<K, V>);

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        SortedMap(btree_slab::BTreeMapByEighths::default())
    }
}

impl<K: Ord + Copy + std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for SortedMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.0.iter()).finish()
    }
}

impl<K: Ord + Copy, V: Clone> Clone for SortedMap<K, V> {
    fn clone(&self) -> Self {
        SortedMap(self.0.iter().map(|(k, v)| (*k, v.clone())).collect())
    }
}

impl<K: Ord + Copy, V: PartialEq> PartialEq for SortedMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.0.iter().eq(other.0.iter())
    }
}

impl<K: Ord + Copy, V> SortedMap<K, V> {
    /// The value at `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        self.0.get(k)
    }

    /// Whether `k` has a value.
    pub fn contains_key(&self, k: &K) -> bool {
        self.0.contains_key(k)
    }

    /// The entries, in key order.
    pub fn iter(&self) -> btree_slab::generic::map::Iter<'_, K, V, Nodes<K, V>> {
        self.0.iter()
    }

    /// The keys, in order.
    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.0.keys()
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The entries with keys in `r`, in order.
    pub fn range(&self, r: std::ops::RangeInclusive<K>) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.0.range(r)
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value at `k`, to change in place.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.0.get_mut(k)
    }

    /// Sets `k` to `v`; returns the value it replaces. A new key costs its
    /// path in the tree; a value replaced, none beyond finding it.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        if let Some(x) = self.0.get_mut(&k) {
            return Some(std::mem::replace(x, v));
        }
        kept(path(self.0.len()));
        self.0.insert(k, v)
    }

    /// Removes `k`; returns its value.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let v = self.0.remove(k);
        if v.is_some() {
            kept(path(self.0.len()));
        }
        v
    }

    /// Empties the map, keeping its nodes' room.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The map made of `entries`, in place of its own: what a step that
    /// rebuilds it whole passes over.
    pub fn rebuild(&mut self, entries: impl IntoIterator<Item = (K, V)>) {
        self.0.clear();
        for (k, v) in entries {
            kept(path(self.0.len()));
            self.0.insert(k, v);
        }
    }

    /// The values, in key order, to change in place (their keys stay).
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> + '_ {
        self.0.values_mut()
    }

    /// Keeps the entries `f` keeps: a pass over every entry.
    pub fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool) {
        kept(self.0.len());
        self.0.retain(|k, v| f(k, v));
    }

    /// The keys strictly between `after` and `before` (`None`: unbounded),
    /// in order, into `out`.
    pub fn keys_between(&self, after: Option<K>, before: Option<K>, out: &mut Vec<K>) {
        use std::ops::Bound::{Excluded, Unbounded};
        out.clear();
        let lo = after.map_or(Unbounded, Excluded);
        let hi = before.map_or(Unbounded, Excluded);
        if let (Excluded(l), Excluded(h)) = (lo, hi) {
            if l >= h {
                return;
            }
        }
        out.extend(self.0.range((lo, hi)).map(|(k, _)| *k));
    }

    /// Applies `changes` — a value to set at a key, or `None` to remove the
    /// key — in order, the last change of a key winning; `changes` is left
    /// empty. `spent` receives each value replaced or removed.
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
}

impl<'a, K: Ord + Copy, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = btree_slab::generic::map::Iter<'a, K, V, Nodes<K, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
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
            8 => (0u16..300, any::<u32>()).prop_map(|(k, v)| Op::Insert(k, v)),
            6 => (0u16..300).prop_map(Op::Remove),
            4 => prop::collection::vec((0u16..300, prop::option::of(any::<u32>())), 0..40)
                .prop_map(Op::Apply),
            1 => prop::collection::btree_set(0u16..300, 0..80)
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
            probes in prop::collection::vec((0u16..320, 0u16..40), 8),
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
