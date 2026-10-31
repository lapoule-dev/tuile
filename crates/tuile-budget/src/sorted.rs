// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A sorted vector standing in for a `BTreeMap`: read by binary search,
//! changed in place or by batches merged in one pass (`SortedMap::apply`),
//! so it keeps its room and allocates nothing once grown. Ordered like the
//! maps it replaces: no decision depends on a hash order.

/// A map as a sorted vector of distinct keys, built once per step in a
/// buffer kept across steps, read by binary search. Its accessors mirror
/// `BTreeMap`'s: entries come as `(&K, &V)`, in key order.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SortedMap<K, V>(Vec<(K, V)>);

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        SortedMap(Vec::new())
    }
}

impl<K: Ord + Copy, V> SortedMap<K, V> {
    fn find(&self, k: &K) -> Option<usize> {
        self.0.binary_search_by(|e| e.0.cmp(k)).ok()
    }

    /// The entries, to change in place: kept sorted by key and distinct
    /// by whoever changes them.
    pub fn values_mut(&mut self) -> &mut Vec<(K, V)> {
        &mut self.0
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

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value at `k`, to change in place.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.find(k).map(|i| &mut self.0[i].1)
    }

    /// Sets `k` to `v`, in place; returns the value it replaces. The
    /// entries after it move up: for maps that change by a few entries.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.0.binary_search_by(|e| e.0.cmp(&k)) {
            Ok(i) => Some(std::mem::replace(&mut self.0[i].1, v)),
            Err(i) => {
                grow(&mut self.0, 1);
                self.0.insert(i, (k, v));
                None
            }
        }
    }

    /// Removes `k`; returns its value.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.find(k).map(|i| self.0.remove(i).1)
    }

    /// Empties the map, keeping its room.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Applies `changes` — a value to set at a key, or `None` to remove the
    /// key — the last change of a key winning; `changes` is left empty.
    /// Values set in place, removed entries dropped in one pass, new keys
    /// appended and the whole sorted once: nothing allocated once the map
    /// has grown. `spent` receives each value replaced or removed.
    pub fn apply(&mut self, changes: &mut Vec<(K, Option<V>)>, mut spent: impl FnMut(V))
    where
        V: Default,
    {
        if changes.is_empty() {
            return;
        }
        let used = changes.len();
        // A stable sort: of a key's changes, the last stays last.
        changes.sort_by_key(|c| c.0);
        let n0 = self.0.len();
        let (mut removed, mut appended) = (false, false);
        // The removals kept at the front of `changes`, in key order.
        let mut gone = 0;
        let len = changes.len();
        for i in 0..len {
            if i + 1 < len && changes[i + 1].0 == changes[i].0 {
                continue;
            }
            let k = changes[i].0;
            let v = changes[i].1.take();
            match self.0[..n0].binary_search_by(|e| e.0.cmp(&k)) {
                Ok(at) => match v {
                    Some(v) => spent(std::mem::replace(&mut self.0[at].1, v)),
                    None => {
                        changes[gone].0 = k;
                        gone += 1;
                        removed = true;
                    }
                },
                Err(_) => {
                    if let Some(v) = v {
                        grow(&mut self.0, 1);
                        self.0.push((k, v));
                        appended = true;
                    }
                }
            }
        }
        if removed {
            let (mut w, mut j) = (0, 0);
            for r in 0..self.0.len() {
                if r < n0 && j < gone && self.0[r].0 == changes[j].0 {
                    j += 1;
                    spent(std::mem::take(&mut self.0[r].1));
                    continue;
                }
                self.0.swap(w, r);
                w += 1;
            }
            self.0.truncate(w);
        }
        changes.clear();
        trim(changes, used);
        if appended {
            // Distinct keys: unstable is stable.
            self.0.sort_unstable_by_key(|e| e.0);
        }
        let len = self.0.len();
        trim(&mut self.0, len);
        debug_assert!(
            self.0.windows(2).all(|w| w[0].0 < w[1].0),
            "sorted, distinct"
        );
    }
}

impl<'a, K: Ord + Copy, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (K, V)>, fn(&'a (K, V)) -> (&'a K, &'a V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

/// A map kept from one step to the next, as two columns: its keys, sorted
/// and distinct, and their values at the same places. A lookup searches the
/// keys alone, a dense column (eight bytes a tile for a `TileId`), rather
/// than strides of whole entries: the budget looks tiles up by the thousand
/// each step, and the values are large (a kept need's record, a tree
/// position). Its read accessors mirror [`SortedMap`]'s.
///
/// Entries are added at its end, then merged in place
/// ([`SortedCols::merge_tail`]): no buffer to keep for them.
#[derive(Clone, Debug)]
pub(super) struct SortedCols<K, V> {
    keys: Vec<K>,
    vals: Vec<V>,
}

impl<K, V> Default for SortedCols<K, V> {
    fn default() -> Self {
        SortedCols {
            keys: Vec::new(),
            vals: Vec::new(),
        }
    }
}

impl<K: Ord + Copy, V> SortedCols<K, V> {
    fn find(&self, k: &K) -> Option<usize> {
        self.keys.binary_search(k).ok()
    }

    /// The value at `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        self.find(k).map(|i| &self.vals[i])
    }

    /// The entries, in key order.
    pub fn iter(&self) -> std::iter::Zip<std::slice::Iter<'_, K>, std::slice::Iter<'_, V>> {
        self.keys.iter().zip(self.vals.iter())
    }

    /// The keys, in order.
    pub fn keys(&self) -> std::slice::Iter<'_, K> {
        self.keys.iter()
    }

    /// The keys, as the column they are kept in.
    pub fn key_column(&self) -> &[K] {
        &self.keys
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The value at `k`, to change in place (its key stays).
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.find(k).map(|i| &mut self.vals[i])
    }

    /// The values, in key order, to change in place (their keys stay).
    pub fn values_mut(&mut self) -> std::slice::IterMut<'_, V> {
        self.vals.iter_mut()
    }

    /// The value at place `i`.
    pub fn value_at(&self, i: usize) -> &V {
        &self.vals[i]
    }

    /// The value at place `i`, to change in place (its key stays).
    pub fn value_at_mut(&mut self, i: usize) -> &mut V {
        &mut self.vals[i]
    }

    /// Room for exactly `n` more entries.
    pub fn reserve_exact(&mut self, n: usize) {
        self.keys.reserve_exact(n);
        self.vals.reserve_exact(n);
    }

    /// Adds an entry at the end, out of order until
    /// [`SortedCols::merge_tail`]; each column grown as [`push_grown`] does.
    pub fn push(&mut self, k: K, v: V) {
        push_grown(&mut self.keys, k);
        push_grown(&mut self.vals, v);
    }

    /// Keeps the entries `f` keeps, in order. A map left with more than an
    /// eighth of its room unused gives back most of it: without that, a view
    /// that shrinks keeps the room of the largest it ever was (measured on
    /// the tour: 432 -> 436 bytes per tracked resource).
    pub fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool) {
        let mut w = 0;
        for i in 0..self.keys.len() {
            if f(&self.keys[i], &mut self.vals[i]) {
                self.keys.swap(w, i);
                self.vals.swap(w, i);
                w += 1;
            }
        }
        self.keys.truncate(w);
        self.vals.truncate(w);
        let shrink = |cap: usize| cap > 64 && cap > w + w / 8;
        if shrink(self.keys.capacity()) {
            self.keys.shrink_to(w + w / 16);
        }
        if shrink(self.vals.capacity()) {
            self.vals.shrink_to(w + w / 16);
        }
    }

    /// Merges the entries from place `mid` on — sorted, distinct, of keys
    /// not before `mid` — into the sorted entries before it, in place: no
    /// buffer, the two columns moved together.
    pub fn merge_tail(&mut self, mid: usize) {
        merge_in_place(&mut self.keys, &mut self.vals, mid);
        debug_assert!(
            self.keys.windows(2).all(|w| w[0] < w[1]),
            "sorted, distinct"
        );
    }
}

/// Merges the sorted runs `keys[..mid]` and `keys[mid..]` (distinct keys)
/// in place, `vals` moved alike: the symmetric merge by rotations
/// (Kim and Kutzner's SymMerge, in its simplest form), O(n log n) moves at
/// worst, no allocation; a short run merged into a long one costs about
/// one pass over the long one per level.
fn merge_in_place<K: Ord + Copy, V>(keys: &mut [K], vals: &mut [V], mid: usize) {
    let n = keys.len();
    if mid == 0 || mid >= n || keys[mid - 1] < keys[mid] {
        return;
    }
    // Split the longer run at its middle, the shorter where that key
    // falls; rotate the two inner parts past each other.
    let (cut_a, cut_b) = if mid >= n - mid {
        let a = mid / 2;
        let b = mid + keys[mid..].partition_point(|k| *k < keys[a]);
        (a, b)
    } else {
        let b = mid + (n - mid) / 2;
        let a = keys[..mid].partition_point(|k| *k < keys[b]);
        (a, b)
    };
    keys[cut_a..cut_b].rotate_left(mid - cut_a);
    vals[cut_a..cut_b].rotate_left(mid - cut_a);
    let new_mid = cut_a + (cut_b - mid);
    let (kl, kr) = keys.split_at_mut(new_mid);
    let (vl, vr) = vals.split_at_mut(new_mid);
    merge_in_place(kl, vl, cut_a);
    merge_in_place(kr, vr, cut_b - new_mid);
}

impl<'a, K: Ord + Copy, V> IntoIterator for &'a SortedCols<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Zip<std::slice::Iter<'a, K>, std::slice::Iter<'a, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Makes room in `v` for `extra` more: by an eighth of its room at least,
/// so it grows rarely and with little slack.
///
/// Why not `Vec`'s own doubling: the maps grown this way are kept for the
/// session, and a doubling leaves up to half of them unused. Measured on the
/// tour (`allocation_ratchets_hold`): with plain `push` in
/// [`SortedCols::push`], the bytes per tracked resource go 432 -> 433.
fn grow<T>(v: &mut Vec<T>, extra: usize) {
    if v.len() + extra > v.capacity() {
        v.reserve_exact(extra.max(v.capacity() / 8));
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

/// Pushes `x` on `v`, grown as [`grow`] does.
pub(super) fn push_grown<T>(v: &mut Vec<T>, x: T) {
    grow(v, 1);
    v.push(x);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sorted_map_reads_like_a_btree_map() {
        let mut m = SortedMap::default();
        for (k, v) in [(7, 'c'), (1, 'a'), (3, 'b')] {
            m.insert(k, v);
        }
        assert_eq!(m.get(&3), Some(&'b'));
        assert!(m.get(&2).is_none() && m.contains_key(&7));
        assert_eq!(m.range(2..=7).map(|(k, _)| *k).collect::<Vec<_>>(), [3, 7]);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), [1, 3, 7]);
        assert_eq!(m.remove(&3), Some('b'));
        assert_eq!(m.len(), 2);
    }

    /// Two sorted runs of distinct keys merge in place into one, each
    /// value still at its key, whatever the runs' lengths and interleaving.
    #[test]
    fn sorted_columns_merge_their_tail_in_place() {
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..2_000u64 {
            let len = usize::try_from(next() % 40).unwrap();
            let mut all: Vec<u64> = (0..len).map(|_| next() % 100).collect();
            all.sort_unstable();
            all.dedup();
            let mut head = Vec::new();
            let mut tail = Vec::new();
            for k in &all {
                if next() % 3 == 0 {
                    tail.push(*k);
                } else {
                    head.push(*k);
                }
            }
            let mut m: SortedCols<u64, u64> = SortedCols::default();
            for k in head.iter().chain(&tail) {
                m.push(*k, k * 10 + 1);
            }
            m.merge_tail(head.len());
            assert_eq!(m.keys().copied().collect::<Vec<_>>(), all, "round {round}");
            for (k, v) in &m {
                assert_eq!(*v, k * 10 + 1, "round {round}");
            }
            m.retain(|k, _| k.is_multiple_of(2));
            assert!(m
                .iter()
                .all(|(k, v)| k.is_multiple_of(2) && *v == k * 10 + 1));
            for k in &all {
                assert_eq!(m.get(k).is_some(), k.is_multiple_of(2), "round {round}");
            }
        }
    }

    #[test]
    fn changes_apply_as_a_btree_map_takes_them_one_by_one() {
        // Random changes against an ordered map of the same keys.
        let mut m: SortedMap<u32, u32> = SortedMap::default();
        let mut oracle = std::collections::BTreeMap::new();
        let mut x = 12345u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut changes = Vec::new();
        for _ in 0..200 {
            for _ in 0..(next() % 20) {
                let k = next() % 64;
                let v = (next() % 3 != 0).then(&mut next);
                changes.push((k, v));
            }
            for (k, v) in &changes {
                match v {
                    Some(v) => {
                        oracle.insert(*k, *v);
                    }
                    None => {
                        oracle.remove(k);
                    }
                }
            }
            m.apply(&mut changes, |_| {});
            assert!(changes.is_empty());
            assert_eq!(
                m.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
                oracle.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>()
            );
        }
    }
}
