// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// FROZEN REFERENCE — a mechanical copy of crates/tuile-budget/src/sorted.rs at
// commit 3d1a028e ("refactor(budget): the budget is a crate of its own").
// Test-only: never compiled into the library. Never edited, except to follow
// a deliberate, documented behaviour change of the real engine (record it in
// tests/reference/frozen/mod.rs). See tests/reference/mod.rs.

//! Sorted vectors standing in for per-step `BTreeSet`s: refilled in place
//! each step, so they keep their capacity, and read by binary search.
//! Ordered like the sets they replace: no decision depends on a hash order.

/// A sorted set without duplicates, refilled in place.
#[derive(Clone, Debug)]
pub(super) struct SortedSet<T>(Vec<T>);

impl<T> Default for SortedSet<T> {
    fn default() -> Self {
        SortedSet(Vec::new())
    }
}

impl<T: Ord + Copy> SortedSet<T> {
    /// Whether `x` is in the set.
    pub fn contains(&self, x: &T) -> bool {
        self.0.binary_search(x).is_ok()
    }

    /// The members, in order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// The set becomes `items`, sorted and deduplicated, in the same
    /// buffer.
    pub fn refill(&mut self, items: impl IntoIterator<Item = T>) {
        self.0.clear();
        self.0.extend(items);
        // Equal members are identical: an unstable sort orders them as a
        // stable one would.
        self.0.sort_unstable();
        self.0.dedup();
    }
}

impl<'a, T> IntoIterator for &'a SortedSet<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A map as a sorted vector of distinct keys, built once per step in a
/// buffer kept across steps, read by binary search. Its accessors mirror
/// `BTreeMap`'s: entries come as `(&K, &V)`, in key order.
#[derive(Clone, Debug)]
pub(super) struct SortedMap<K, V>(Vec<(K, V)>);

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        SortedMap(Vec::new())
    }
}

impl<K: Ord + Copy, V> SortedMap<K, V> {
    /// A map whose entries are `v`'s, already sorted by key and distinct.
    pub fn from_sorted(v: Vec<(K, V)>) -> Self {
        debug_assert!(v.windows(2).all(|w| w[0].0 < w[1].0), "sorted, distinct");
        SortedMap(v)
    }

    /// Its entries, as the vector they are kept in.
    pub fn into_vec(self) -> Vec<(K, V)> {
        self.0
    }

    /// Its buffer, emptied, for the next step to fill.
    pub fn into_buffer(mut self) -> Vec<(K, V)> {
        self.0.clear();
        self.0
    }

    fn find(&self, k: &K) -> Option<usize> {
        self.0.binary_search_by(|e| e.0.cmp(k)).ok()
    }

    /// The value at `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        self.find(k).map(|i| &self.0[i].1)
    }

    /// Whether `k` has a value.
    pub fn contains_key(&self, k: &K) -> bool {
        self.find(k).is_some()
    }

    /// The entries, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.0.iter().map(|(k, v)| (k, v))
    }

    /// The keys, in order.
    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.0.iter().map(|(k, _)| k)
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The entries with keys in `r`, in order.
    pub fn range(&self, r: std::ops::RangeInclusive<K>) -> impl Iterator<Item = (&K, &V)> + '_ {
        let (lo, hi) = r.into_inner();
        let from = self.0.partition_point(|e| e.0 < lo);
        self.0[from..]
            .iter()
            .take_while(move |e| e.0 <= hi)
            .map(|(k, v)| (k, v))
    }
}

impl<'a, K: Ord + Copy, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (K, V)>, fn(&'a (K, V)) -> (&'a K, &'a V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

/// `v`'s allocation, emptied, for elements of another type: a per-step
/// buffer whose elements borrow the step's input keeps its capacity across
/// steps. When the layouts differ it is simply a new, empty vector.
pub(super) fn recycle<T, U>(mut v: Vec<T>) -> Vec<U> {
    v.clear();
    // An empty source: the closure never runs. The standard library
    // collects a mapped `IntoIter` in place, in the source's allocation.
    v.into_iter().map(|_| unreachable!("empty")).collect()
}

/// The distinct members of four sorted, deduplicated streams, in order:
/// their union, without building it.
pub(super) fn union4<T: Ord + Copy>(
    a: impl Iterator<Item = T>,
    b: impl Iterator<Item = T>,
    c: impl Iterator<Item = T>,
    d: impl Iterator<Item = T>,
) -> impl Iterator<Item = T> {
    let mut a = a.peekable();
    let mut b = b.peekable();
    let mut c = c.peekable();
    let mut d = d.peekable();
    std::iter::from_fn(move || {
        let next = [a.peek(), b.peek(), c.peek(), d.peek()]
            .into_iter()
            .flatten()
            .min()
            .copied()?;
        // Every stream at that member moves past it.
        a.next_if_eq(&next);
        b.next_if_eq(&next);
        c.next_if_eq(&next);
        d.next_if_eq(&next);
        Some(next)
    })
}
