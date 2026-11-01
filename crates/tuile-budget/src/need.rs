// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`NeedSet`]: what the traversal wants this step, the budget's only input
//! about the view.

use super::imagery::{GeoRect, TexelFootprint};
use super::types::{Class, LayerId, TileId};

/// What the traversal wants: the whole need of one decision step.
///
/// A tile may appear in several lists; the most urgent membership wins. The
/// order of the entries does not matter: the budget's decisions are stable
/// under any permutation of them, but for a tile listed twice in one list
/// with needs that tie, where the first listed wins.
///
/// A step gives the budget either the whole need or what changed in it
/// since the step before ([`NeedDelta`]); a delta built from two needs
/// ([`NeedDelta::diff`]) decides exactly as the second need would.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NeedSet {
    /// The tiles the traversal wants drawn this step.
    pub drawn: Vec<(TileId, Need)>,
    /// The children a held REPLACE parent waits on before it can refine:
    /// all of them, resident or not, so that the resident ones are kept
    /// while their siblings load (a swap group).
    pub blockers: Vec<(TileId, Need)>,
    /// Ancestors preloaded so that a zoom-out finds them resident.
    pub ancestors: Vec<(TileId, Need)>,
    /// A coarse prefetch ring around the frustum (rotation).
    pub ring: Vec<(TileId, Need)>,
}

impl NeedSet {
    /// An empty need.
    pub fn new() -> Self {
        NeedSet::default()
    }

    /// How many entries it lists, every list counted.
    pub fn len(&self) -> usize {
        self.drawn.len() + self.blockers.len() + self.ancestors.len() + self.ring.len()
    }

    /// Whether it lists nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The list of `m`.
    pub fn list(&self, m: Membership) -> &Vec<(TileId, Need)> {
        match m {
            Membership::Drawn => &self.drawn,
            Membership::Blocker => &self.blockers,
            Membership::Ancestor => &self.ancestors,
            Membership::Ring => &self.ring,
        }
    }

    /// The list of `m`, to change.
    pub fn list_mut(&mut self, m: Membership) -> &mut Vec<(TileId, Need)> {
        match m {
            Membership::Drawn => &mut self.drawn,
            Membership::Blocker => &mut self.blockers,
            Membership::Ancestor => &mut self.ancestors,
            Membership::Ring => &mut self.ring,
        }
    }

    /// The entry at `seq` in [`NeedSet::entries`]' order.
    pub(crate) fn entry(&self, seq: usize) -> Option<(Membership, TileId, &Need)> {
        let mut i = seq;
        for m in Membership::ALL {
            let l = self.list(m);
            if let Some((t, n)) = l.get(i) {
                return Some((m, *t, n));
            }
            i -= l.len();
        }
        None
    }

    /// Every entry's tile and its place in [`NeedSet::entries`], by tile,
    /// then in that order: each tile's entries side by side, as listed.
    pub(crate) fn grouped_into(&self, out: &mut Vec<(TileId, u32)>) {
        out.clear();
        out.extend(
            self.entries()
                .enumerate()
                .map(|(seq, (_, t, _))| (t, u32::try_from(seq).unwrap_or(u32::MAX))),
        );
        // Each place once: unstable is stable here.
        out.sort_unstable();
    }

    /// Every entry with its membership, most urgent list first.
    pub fn entries(&self) -> impl Iterator<Item = (Membership, TileId, &Need)> + '_ {
        let lists: [(Membership, &'_ [(TileId, Need)]); 4] = [
            (Membership::Drawn, &self.drawn),
            (Membership::Blocker, &self.blockers),
            (Membership::Ancestor, &self.ancestors),
            (Membership::Ring, &self.ring),
        ];
        lists
            .into_iter()
            .flat_map(|(m, l)| l.iter().map(move |(t, n)| (m, *t, n)))
    }
}

/// Which list of the [`NeedSet`] an entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Membership {
    /// Wanted on screen.
    Drawn,
    /// Waited on by a held parent.
    Blocker,
    /// A preloaded ancestor.
    Ancestor,
    /// The prefetch ring.
    Ring,
}

impl Membership {
    /// Every membership, most urgent first: the order of the lists.
    pub const ALL: [Membership; 4] = [
        Membership::Drawn,
        Membership::Blocker,
        Membership::Ancestor,
        Membership::Ring,
    ];
}

/// What changed in the need since the step before, tile by tile: the tiles
/// that entered it, those whose entries changed, those that left.
///
/// The budget keeps the need it last read; a step given a delta
/// ([`FrameInput::from_delta`](crate::FrameInput::from_delta)) applies it to
/// that need instead of reading a whole one, and decides exactly as it
/// would on the whole need the delta leads to.
///
/// The unit of change is the tile. A tile that entered or changed is listed
/// with every entry it now has — each `(tile, membership, need)` of it, side
/// by side, in the need's order (a list's entries as listed); a tile that
/// left is named. A tile changed when any of its entries did: one more or
/// one less, another membership, any field of a need (a float compared bit
/// for bit, [`Need::same_as`]): its SSE, distance or centrality, its
/// parent, its imagery or footprint.
///
/// [`NeedDelta::diff`] builds one from two whole needs. Applied to the
/// first ([`NeedDelta::apply_to`]), it gives the second, entry for entry; only
/// the order of the tiles within a list may differ, on which nothing
/// depends.
///
/// Built by hand ([`NeedDelta::enter`], [`NeedDelta::change`],
/// [`NeedDelta::leave`]), a tile is meant to appear once. If it appears more
/// than once, its last word counts, in the order: left, entered, changed —
/// within one list, the later. Entering or changing a tile with no entry
/// records nothing (a tile goes by [`NeedDelta::leave`]); a tile that left
/// and was not there stays away, one that "changed" and was not there
/// enters.
#[derive(Default)]
pub struct NeedDelta {
    /// The entries of the tiles that entered: the first `entered_len` of
    /// `entered`. The rest are entries of earlier deltas, kept for their
    /// buffers (a need's layers): `diff` writes into them in place, so a
    /// delta built every step allocates nothing once its lists have grown.
    entered: Vec<(TileId, Membership, Need)>,
    entered_len: usize,
    changed: Vec<(TileId, Membership, Need)>,
    changed_len: usize,
    left: Vec<TileId>,
    /// `diff`'s buffers: each need's entries grouped by tile.
    grouped: (Grouped, Grouped),
}

/// A need's entries grouped by tile: each entry's tile and its place
/// ([`NeedSet::grouped_into`]).
type Grouped = Vec<(TileId, u32)>;

impl Clone for NeedDelta {
    fn clone(&self) -> Self {
        NeedDelta {
            entered: self.entered().to_vec(),
            entered_len: self.entered_len,
            changed: self.changed().to_vec(),
            changed_len: self.changed_len,
            left: self.left.clone(),
            grouped: Default::default(),
        }
    }
}

impl std::fmt::Debug for NeedDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NeedDelta")
            .field("entered", &self.entered())
            .field("changed", &self.changed())
            .field("left", &self.left)
            .finish()
    }
}

/// What a delta says: its three lists.
impl PartialEq for NeedDelta {
    fn eq(&self, o: &Self) -> bool {
        self.entered() == o.entered() && self.changed() == o.changed() && self.left == o.left
    }
}

/// Writes `(t, m, n)` as the `len`th entry of `list`, over an old entry when
/// there is one (its need's buffers reused), else at the end.
fn put(
    list: &mut Vec<(TileId, Membership, Need)>,
    len: &mut usize,
    t: TileId,
    m: Membership,
    n: &Need,
) {
    match list.get_mut(*len) {
        Some(e) => {
            e.0 = t;
            e.1 = m;
            e.2.clone_from(n);
        }
        None => list.push((t, m, n.clone())),
    }
    *len += 1;
}

/// One tile's last word in a [`NeedDelta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeltaOp {
    /// It left.
    Leave,
    /// Its entries now: `entered` (`list` 0) or `changed` (1), `start..end`;
    /// for a whole need's run (2), `start..end` of its grouped entries.
    Set { list: u8, start: u32, end: u32 },
}

impl NeedDelta {
    /// No change.
    pub fn new() -> Self {
        NeedDelta::default()
    }

    /// What changed from `before` to `after`.
    pub fn between(before: &NeedSet, after: &NeedSet) -> Self {
        let mut d = NeedDelta::new();
        d.diff(before, after);
        d
    }

    /// Becomes what changed from `before` to `after`, in its own buffers:
    /// nothing allocated once they have grown to the view (an entry's need
    /// is copied over an earlier delta's, its layers' buffer reused). The
    /// tiles come in order, each tile's entries in the need's order.
    pub fn diff(&mut self, before: &NeedSet, after: &NeedSet) {
        self.clear();
        let (mut a, mut b) = std::mem::take(&mut self.grouped);
        before.grouped_into(&mut a);
        after.grouped_into(&mut b);
        let run = |g: &[(TileId, u32)], i: usize| {
            let t = g[i].0;
            i + g[i..].iter().take_while(|e| e.0 == t).count()
        };
        fn entry(n: &NeedSet, seq: u32) -> (Membership, TileId, &Need) {
            usize::try_from(seq)
                .ok()
                .and_then(|s| n.entry(s))
                .expect("a place listed")
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            let ta = a.get(i).map(|e| e.0);
            let tb = b.get(j).map(|e| e.0);
            match (ta, tb) {
                (Some(x), Some(y)) if x == y => {
                    let (ia, jb) = (run(&a, i), run(&b, j));
                    let same = ia - i == jb - j
                        && a[i..ia].iter().zip(&b[j..jb]).all(|(p, q)| {
                            let (pm, _, pn) = entry(before, p.1);
                            let (qm, _, qn) = entry(after, q.1);
                            pm == qm && pn.same_as(qn)
                        });
                    if !same {
                        for e in &b[j..jb] {
                            let (m, t, n) = entry(after, e.1);
                            put(&mut self.changed, &mut self.changed_len, t, m, n);
                        }
                    }
                    (i, j) = (ia, jb);
                }
                (Some(x), y) if y.is_none_or(|y| x < y) => {
                    self.left.push(x);
                    i = run(&a, i);
                }
                _ => {
                    let jb = run(&b, j);
                    for e in &b[j..jb] {
                        let (m, t, n) = entry(after, e.1);
                        put(&mut self.entered, &mut self.entered_len, t, m, n);
                    }
                    j = jb;
                }
            }
        }
        self.grouped = (a, b);
    }

    /// `tile` entered the need, with these entries.
    pub fn enter(&mut self, tile: TileId, entries: impl IntoIterator<Item = (Membership, Need)>) {
        for (m, n) in entries {
            put(&mut self.entered, &mut self.entered_len, tile, m, &n);
        }
    }

    /// `tile`'s entries changed: these are all of them now.
    pub fn change(&mut self, tile: TileId, entries: impl IntoIterator<Item = (Membership, Need)>) {
        for (m, n) in entries {
            put(&mut self.changed, &mut self.changed_len, tile, m, &n);
        }
    }

    /// `tile` left the need.
    pub fn leave(&mut self, tile: TileId) {
        self.left.push(tile);
    }

    /// No change, its buffers kept (the entries' needs too, for their
    /// layers' buffers: the next change writes over them).
    pub fn clear(&mut self) {
        self.entered_len = 0;
        self.changed_len = 0;
        self.left.clear();
    }

    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.entered_len == 0 && self.changed_len == 0 && self.left.is_empty()
    }

    /// The entries of the tiles that entered.
    pub fn entered(&self) -> &[(TileId, Membership, Need)] {
        &self.entered[..self.entered_len]
    }

    /// The entries, all of them now, of the tiles that changed.
    pub fn changed(&self) -> &[(TileId, Membership, Need)] {
        &self.changed[..self.changed_len]
    }

    /// The tiles that left.
    pub fn left(&self) -> &[TileId] {
        &self.left
    }

    /// The tiles it names: entered, changed or left.
    pub fn tiles(&self) -> usize {
        let runs = |l: &[(TileId, Membership, Need)]| {
            l.iter()
                .enumerate()
                .filter(|(i, e)| *i == 0 || l[i - 1].0 != e.0)
                .count()
        };
        runs(self.entered()) + runs(self.changed()) + self.left.len()
    }

    /// Applies it to `need`: the whole need it leads to. Each tile it names
    /// loses every entry it had, then gets its entries now, at the end of
    /// their lists.
    pub fn apply_to(&self, need: &mut NeedSet) {
        let mut ops = Vec::new();
        self.ops_into(&mut ops);
        let named: Vec<TileId> = ops.iter().map(|o| o.0).collect();
        for m in Membership::ALL {
            need.list_mut(m)
                .retain(|(t, _)| named.binary_search(t).is_err());
        }
        for (_, op) in &ops {
            for (t, m, n) in self.group(*op) {
                need.list_mut(*m).push((*t, n.clone()));
            }
        }
    }

    /// Each tile's word in tile order, read off the lists as they are: when
    /// each list comes sorted by tile and no tile is in two of them (as
    /// [`NeedDelta::diff`] makes them). `None` otherwise
    /// ([`NeedDelta::ops_into`] sorts them).
    pub(crate) fn sorted_ops(&self) -> Option<impl Iterator<Item = (TileId, DeltaOp)> + '_> {
        let sorted = |l: &[(TileId, Membership, Need)]| l.windows(2).all(|w| w[0].0 <= w[1].0);
        let ok = self.left.windows(2).all(|w| w[0] < w[1])
            && sorted(self.entered())
            && sorted(self.changed());
        if !ok {
            return None;
        }
        let mut check = Words::new(self);
        check.by_ref().for_each(drop);
        (!check.clash).then(|| Words::new(self))
    }

    /// Each tile's last word, in tile order, into `out` (its buffer kept).
    pub(crate) fn ops_into(&self, out: &mut Vec<(TileId, DeltaOp)>) {
        out.clear();
        out.extend(self.left.iter().map(|t| (*t, DeltaOp::Leave)));
        for (list, l) in [(0u8, self.entered()), (1, self.changed())] {
            let mut i = 0;
            while i < l.len() {
                let t = l[i].0;
                let end = i + l[i..].iter().take_while(|e| e.0 == t).count();
                let (start, end) = (sat(i), sat(end));
                out.push((t, DeltaOp::Set { list, start, end }));
                i = usize::try_from(end).unwrap_or(usize::MAX).max(i + 1);
            }
        }
        // By tile, then in the order the words count: left, entered,
        // changed, each list as written. Only the last word of a tile stays.
        // Keys distinct but for a tile left twice, two equal words.
        out.sort_unstable_by_key(|(t, op)| {
            (
                *t,
                match *op {
                    DeltaOp::Leave => (0u8, 0u32),
                    DeltaOp::Set { list, start, .. } => (1 + list, start),
                },
            )
        });
        out.reverse();
        out.dedup_by_key(|(t, _)| *t);
        out.reverse();
    }

    /// The entries of a tile's word.
    pub(crate) fn group(&self, op: DeltaOp) -> &[(TileId, Membership, Need)] {
        match op {
            DeltaOp::Leave => &[],
            DeltaOp::Set { list, start, end } => {
                let l = if list == 0 {
                    self.entered()
                } else {
                    self.changed()
                };
                let at = |x: u32| usize::try_from(x).unwrap_or(usize::MAX).min(l.len());
                &l[at(start)..at(end)]
            }
        }
    }
}

/// A sorted delta's words, by tile: its three lists merged.
struct Words<'d> {
    d: &'d NeedDelta,
    l: usize,
    e: usize,
    c: usize,
    /// Whether two lists named one tile.
    clash: bool,
}

impl<'d> Words<'d> {
    fn new(d: &'d NeedDelta) -> Self {
        Words {
            d,
            l: 0,
            e: 0,
            c: 0,
            clash: false,
        }
    }
}

impl Iterator for Words<'_> {
    type Item = (TileId, DeltaOp);

    fn next(&mut self) -> Option<Self::Item> {
        let (entered, changed) = (self.d.entered(), self.d.changed());
        let heads = [
            self.d.left.get(self.l).copied(),
            entered.get(self.e).map(|x| x.0),
            changed.get(self.c).map(|x| x.0),
        ];
        let t = heads.iter().flatten().min().copied()?;
        self.clash |= heads.iter().filter(|h| **h == Some(t)).count() > 1;
        let run = |l: &[(TileId, Membership, Need)], i: usize| {
            i + l[i..].iter().take_while(|x| x.0 == t).count()
        };
        Some(if heads[0] == Some(t) {
            self.l += 1;
            (t, DeltaOp::Leave)
        } else if heads[1] == Some(t) {
            let end = run(entered, self.e);
            let op = DeltaOp::Set {
                list: 0,
                start: sat(self.e),
                end: sat(end),
            };
            self.e = end;
            (t, op)
        } else {
            let end = run(changed, self.c);
            let op = DeltaOp::Set {
                list: 1,
                start: sat(self.c),
                end: sat(end),
            };
            self.c = end;
            (t, op)
        })
    }
}

/// A position as a `u32`, saturating.
fn sat(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// One tile's need, from the traversal.
///
/// Floats are quantised once when the budget reads them; a non-finite value
/// counts as zero and is reported in the step's statistics.
#[derive(Debug, PartialEq)]
#[non_exhaustive]
pub struct Need {
    /// The tile's level in its tree.
    pub level: u32,
    /// The tile's parent, which is how the budget knows the fallback chain.
    pub parent: Option<TileId>,
    /// The mesh's class: [`Class::TerrainMesh`] or [`Class::Tiles3dMesh`].
    pub class: Class,
    /// `log2(sse / τ_eff)`: how much the tile is wanted beyond its threshold.
    pub sse_excess: f32,
    /// `1 − off-axis`, in `[0, 1]`: 1 at the centre of the view.
    pub centrality: f32,
    /// Metres from the camera to the tile.
    pub distance: f32,
    /// The imagery the tile wants, chosen independently of `level`. `None`
    /// for a mesh that carries its own textures.
    pub imagery: Option<ImageryNeed>,
    /// The tile's footprint in longitude/latitude, for drape UV transforms.
    pub rect: Option<GeoRect>,
}

// By hand for `clone_from`, which keeps the layers' buffer: the budget
// copies the needs it is told about into the ones it keeps.
impl Clone for Need {
    fn clone(&self) -> Self {
        Need {
            level: self.level,
            parent: self.parent,
            class: self.class,
            sse_excess: self.sse_excess,
            centrality: self.centrality,
            distance: self.distance,
            imagery: self.imagery.clone(),
            rect: self.rect,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.level = source.level;
        self.parent = source.parent;
        self.class = source.class;
        self.sse_excess = source.sse_excess;
        self.centrality = source.centrality;
        self.distance = source.distance;
        self.imagery.clone_from(&source.imagery);
        self.rect = source.rect;
    }
}

impl Need {
    /// Whether `self` and `other` are the same need, their floats compared
    /// bit for bit: `-0.0` is not `0.0`, and a NaN is itself. What a
    /// [`NeedDelta`] calls a change.
    pub fn same_as(&self, other: &Need) -> bool {
        let rect_bits = |r: &GeoRect| [r.west, r.south, r.east, r.north].map(f64::to_bits);
        let imagery_same = match (&self.imagery, &other.imagery) {
            (Some(a), Some(b)) => {
                a.footprint.ratio().to_bits() == b.footprint.ratio().to_bits()
                    && a.layers == b.layers
            }
            (None, None) => true,
            _ => false,
        };
        self.level == other.level
            && self.parent == other.parent
            && self.class == other.class
            && self.sse_excess.to_bits() == other.sse_excess.to_bits()
            && self.centrality.to_bits() == other.centrality.to_bits()
            && self.distance.to_bits() == other.distance.to_bits()
            && imagery_same
            && self.rect.as_ref().map(rect_bits) == other.rect.as_ref().map(rect_bits)
    }

    /// A need for a tile at `level`, of mesh class `class`, with neutral
    /// scores.
    pub fn new(level: u32, class: Class) -> Self {
        Need {
            level,
            parent: None,
            class,
            sse_excess: 0.0,
            centrality: 1.0,
            distance: 0.0,
            imagery: None,
            rect: None,
        }
    }

    /// Sets the parent.
    pub fn with_parent(mut self, parent: TileId) -> Self {
        self.parent = Some(parent);
        self
    }
    /// Sets the SSE excess.
    pub fn with_sse_excess(mut self, v: f32) -> Self {
        self.sse_excess = v;
        self
    }
    /// Sets the centrality.
    pub fn with_centrality(mut self, v: f32) -> Self {
        self.centrality = v;
        self
    }
    /// Sets the distance.
    pub fn with_distance(mut self, v: f32) -> Self {
        self.distance = v;
        self
    }
    /// Sets the imagery need.
    pub fn with_imagery(mut self, v: ImageryNeed) -> Self {
        self.imagery = Some(v);
        self
    }
    /// Sets the footprint.
    pub fn with_rect(mut self, v: GeoRect) -> Self {
        self.rect = Some(v);
        self
    }
}

/// How deep one imagery layer reaches over a tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerReach {
    /// The layer.
    pub layer: LayerId,
    /// Its source's deepest level over this tile.
    pub max_level: u32,
}

/// The imagery a tile wants (D-A4): one texel footprint on screen,
/// geometric and common to every layer, and how deep each layer it wears
/// reaches. Each layer picks its level, falls back and refreshes on its
/// own.
#[derive(Debug, PartialEq)]
pub struct ImageryNeed {
    /// How large one level-0 texel looks on screen, at the tile's closest
    /// visible point.
    pub footprint: TexelFootprint,
    /// The layers the tile wears, in stacking order.
    pub layers: Vec<LayerReach>,
}

// By hand for `clone_from`, which keeps the layers' buffer.
impl Clone for ImageryNeed {
    fn clone(&self) -> Self {
        ImageryNeed {
            footprint: self.footprint,
            layers: self.layers.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.footprint = source.footprint;
        self.layers.clone_from(&source.layers);
    }
}

impl ImageryNeed {
    /// One layer, the base imagery (`LayerId(0)`), reaching `max_level`.
    pub fn base(footprint: TexelFootprint, max_level: u32) -> Self {
        ImageryNeed {
            footprint,
            layers: vec![LayerReach {
                layer: LayerId(0),
                max_level,
            }],
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tile(i: u64) -> TileId {
        TileId::from_terrain(2, i % 4, i / 4)
    }

    fn need() -> NeedSet {
        let n = |l| Need::new(l, Class::TerrainMesh).with_sse_excess(0.5);
        NeedSet {
            drawn: vec![(tile(0), n(2)), (tile(1), n(2))],
            blockers: vec![(tile(2), n(3))],
            ancestors: vec![(tile(3), n(1))],
            ring: vec![(tile(4), n(2)), (tile(0), n(2))],
        }
    }

    /// Each list's entries in a canonical order: what a need is, up to the
    /// order of its tiles.
    fn canon(n: &NeedSet) -> Vec<(Membership, TileId, Vec<String>)> {
        let mut out: Vec<(Membership, TileId, Vec<String>)> = Vec::new();
        for (m, t, e) in n.entries() {
            match out.iter_mut().find(|x| x.0 == m && x.1 == t) {
                Some(x) => x.2.push(format!("{e:?}")),
                None => out.push((m, t, vec![format!("{e:?}")])),
            }
        }
        out.sort_by_key(|a| (a.0, a.1));
        out
    }

    /// Every kind of change is one: a float (bit for bit, `-0.0` against
    /// `0.0`), the parent, the level, the imagery, a move between lists,
    /// an entry more, a tile gone, a tile new.
    #[test]
    fn every_kind_of_change_is_seen_and_applied() {
        let base = need();
        type Edit = Box<dyn Fn(&mut NeedSet)>;
        let edits: Vec<(&str, Edit)> = vec![
            ("sse", Box::new(|n| n.drawn[0].1.sse_excess = 0.75)),
            ("distance", Box::new(|n| n.drawn[0].1.distance = 3.0)),
            ("centrality", Box::new(|n| n.drawn[0].1.centrality = 0.5)),
            ("signed zero", Box::new(|n| n.drawn[1].1.distance = -0.0)),
            (
                "parent",
                Box::new(|n| n.blockers[0].1.parent = Some(tile(9))),
            ),
            ("level", Box::new(|n| n.ancestors[0].1.level = 7)),
            (
                "imagery",
                Box::new(|n| {
                    let fp = crate::TexelFootprint::new(2.0, 1.0, 1.0, 1.0, 1.0).unwrap();
                    n.ring[0].1.imagery = Some(ImageryNeed::base(fp, 4));
                }),
            ),
            (
                "membership",
                Box::new(|n| {
                    let e = n.drawn.remove(1);
                    n.ancestors.push(e);
                }),
            ),
            (
                "an entry more",
                Box::new(|n| {
                    let e = n.drawn[1].clone();
                    n.drawn.push(e);
                }),
            ),
            ("gone", Box::new(|n| n.blockers.clear())),
            (
                "new",
                Box::new(|n| n.ring.push((tile(12), Need::new(2, Class::TerrainMesh)))),
            ),
        ];
        for (what, edit) in edits {
            let mut after = base.clone();
            edit(&mut after);
            let d = NeedDelta::between(&base, &after);
            assert_eq!(d.tiles(), 1, "{what}: one tile changed");
            let mut applied = base.clone();
            d.apply_to(&mut applied);
            assert_eq!(canon(&applied), canon(&after), "{what}");
        }
        assert!(
            NeedDelta::between(&base, &base).is_empty(),
            "no change, no delta"
        );
    }

    /// A delta between two needs leads from one to the other, whatever
    /// they are; a tile's entries come side by side, in the need's order.
    #[test]
    fn a_delta_leads_from_one_need_to_the_other() {
        let a = need();
        let mut b = need();
        b.drawn.reverse();
        b.drawn[0].1.sse_excess = 2.0;
        b.ring.push((tile(4), Need::new(9, Class::TerrainMesh)));
        b.ancestors.clear();
        b.blockers
            .push((tile(13), Need::new(3, Class::TerrainMesh)));
        let d = NeedDelta::between(&a, &b);
        assert_eq!(d.left(), &[tile(3)]);
        assert_eq!(
            d.entered().iter().map(|e| e.0).collect::<Vec<_>>(),
            [tile(13)]
        );
        // `tile(1)`'s need changed; `tile(4)` has an entry more.
        let mut changed: Vec<_> = d.changed().iter().map(|e| (e.0, e.1)).collect();
        changed.dedup();
        changed.sort();
        let mut want = vec![(tile(1), Membership::Drawn), (tile(4), Membership::Ring)];
        want.sort();
        assert_eq!(changed, want);
        assert_eq!(d.tiles(), 4);
        let mut x = a.clone();
        d.apply_to(&mut x);
        assert_eq!(canon(&x), canon(&b));
    }

    /// A delta built again says only what changed since: its entries are
    /// written over the earlier delta's (their buffers kept), and those
    /// beyond this change are not its. Red with the earlier entries still
    /// listed.
    #[test]
    fn a_delta_built_again_says_only_what_changed_since() {
        let a = need();
        let mut b = need();
        for (_, n) in b.drawn.iter_mut().chain(b.ring.iter_mut()) {
            n.sse_excess = 3.0;
        }
        b.blockers
            .push((tile(13), Need::new(3, Class::TerrainMesh)));
        let mut d = NeedDelta::new();
        d.diff(&a, &b);
        let (entered, changed) = (d.entered().len(), d.changed().len());
        assert!(
            entered > 0 && changed > 1,
            "{entered} entered, {changed} changed"
        );
        // Nothing changed: the delta says so, whatever it said before.
        d.diff(&a, &a);
        assert!(d.is_empty());
        assert_eq!((d.tiles(), d.entered().len(), d.changed().len()), (0, 0, 0));
        // One change: the delta is that change alone.
        let mut c = need();
        c.ancestors[0].1.level = 9;
        d.diff(&a, &c);
        assert_eq!(d.tiles(), 1);
        assert_eq!(d.entered(), &[]);
        assert_eq!(d.changed().len(), 1);
        assert_eq!(d.changed()[0].0, tile(3));
        assert_eq!(d, NeedDelta::between(&a, &c), "built again, the same delta");
        let mut applied = a.clone();
        d.apply_to(&mut applied);
        assert_eq!(canon(&applied), canon(&c));
    }

    /// By hand, a tile's last word counts: left, then entered, then
    /// changed; a word with no entry is none.
    #[test]
    fn a_tiles_last_word_counts() {
        let n = || Need::new(1, Class::TerrainMesh);
        let mut d = NeedDelta::new();
        d.leave(tile(0));
        d.enter(tile(0), [(Membership::Drawn, n())]);
        d.change(tile(1), [(Membership::Ring, n())]);
        d.enter(tile(1), [(Membership::Drawn, n())]);
        d.change(tile(2), []);
        let mut x = NeedSet::new();
        x.ancestors.push((tile(2), n()));
        d.apply_to(&mut x);
        assert_eq!(x.drawn.iter().map(|e| e.0).collect::<Vec<_>>(), [tile(0)]);
        assert_eq!(x.ring.iter().map(|e| e.0).collect::<Vec<_>>(), [tile(1)]);
        assert_eq!(x.ancestors.len(), 1, "an empty word changes nothing");
        assert_eq!(d.tiles(), 4);
    }
}
