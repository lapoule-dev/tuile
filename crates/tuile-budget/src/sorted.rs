// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

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

    /// The entries, to change in place: kept sorted by key and distinct
    /// by whoever changes them.
    pub fn values_mut(&mut self) -> &mut Vec<(K, V)> {
        &mut self.0
    }

    /// Keeps the entries `f` keeps, in order. A map left with more than an
    /// eighth of its room unused gives back most of it.
    pub fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool) {
        self.0.retain_mut(|(k, v)| f(k, v));
        let len = self.0.len();
        if self.0.capacity() > 64 && self.0.capacity() > len + len / 8 {
            self.0.shrink_to(len + len / 16);
        }
    }

    /// Adds `new`'s entries — sorted, distinct, of keys not in the map —
    /// `new` left empty. When `spilled`, entries of new keys were also
    /// pushed at the map's end: it is sorted once, whole; otherwise `new`
    /// is merged in from the back.
    pub fn add_new(&mut self, new: &mut Vec<(K, V)>, spilled: bool)
    where
        V: Default,
    {
        if spilled {
            for e in new.drain(..) {
                push_grown(&mut self.0, e);
            }
            // Distinct keys: unstable is stable.
            self.0.sort_unstable_by_key(|a| a.0);
        } else {
            self.merge_new(new);
        }
        debug_assert!(
            self.0.windows(2).all(|w| w[0].0 < w[1].0),
            "sorted, distinct"
        );
    }

    /// Adds `new`'s entries — sorted, distinct, of keys not in the map —
    /// in one pass from the back, `new` left empty: each takes a
    /// placeholder's place as the old entries move up past it.
    fn merge_new(&mut self, new: &mut Vec<(K, V)>)
    where
        V: Default,
    {
        let Some(&(k0, _)) = new.first() else {
            return;
        };
        let (mut i, mut j) = (self.0.len(), new.len());
        grow(&mut self.0, j);
        self.0.resize_with(i + j, || (k0, V::default()));
        let mut w = self.0.len();
        while j > 0 {
            w -= 1;
            if i > 0 && self.0[i - 1].0 > new[j - 1].0 {
                i -= 1;
                self.0.swap(w, i);
            } else {
                j -= 1;
                std::mem::swap(&mut self.0[w], &mut new[j]);
            }
        }
        new.clear();
        debug_assert!(
            self.0.windows(2).all(|w| w[0].0 < w[1].0),
            "sorted, distinct"
        );
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

/// Makes room in `v` for `extra` more: by an eighth of its room at least,
/// so it grows rarely and with little slack.
fn grow<T>(v: &mut Vec<T>, extra: usize) {
    if v.len() + extra > v.capacity() {
        v.reserve_exact(extra.max(v.capacity() / 8));
    }
}

/// Pushes `x` on `v`, grown as [`grow`] does.
pub(super) fn push_grown<T>(v: &mut Vec<T>, x: T) {
    grow(v, 1);
    v.push(x);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refilled_set_is_sorted_and_distinct() {
        let mut s = SortedSet::default();
        s.refill([5, 1, 3, 1, 5]);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), [1, 3, 5]);
        assert!(s.contains(&3) && !s.contains(&2));
        s.refill([2]);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn a_sorted_map_reads_like_a_btree_map() {
        let m = SortedMap::from_sorted(vec![(1, 'a'), (3, 'b'), (7, 'c')]);
        assert_eq!(m.get(&3), Some(&'b'));
        assert!(m.get(&2).is_none() && m.contains_key(&7));
        assert_eq!(m.range(2..=7).map(|(k, _)| *k).collect::<Vec<_>>(), [3, 7]);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), [1, 3, 7]);
        assert_eq!(m.len(), 3);
        assert_eq!(m.into_buffer().capacity(), 3);
    }

    #[test]
    fn the_union_of_four_streams_is_their_sorted_distinct_members() {
        let u: Vec<_> = union4(
            [1, 4, 9].into_iter(),
            [1, 2].into_iter(),
            std::iter::empty(),
            [2, 9, 10].into_iter(),
        )
        .collect();
        assert_eq!(u, [1, 2, 4, 9, 10]);
    }
}
