// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The held ancestry, as the held set itself.
//!
//! A tile's selection reads the held tiles below it: those whose walk up
//! the parent links the need tells reaches it, with the lowest level each
//! walk passed on the way (`Budget::chain_of`). Kept as the closure — every
//! (ancestor, held tile) pair — one held tile coming or going changed a
//! pair per ancestor, about eighteen. Kept as the held set, it changes by
//! one entry, and the questions are answered by walks:
//!
//! - which held tiles lie below `of`: the held tiles whose walk is
//!   *geometric* — every parent link the quadtree's own (a terrain id's
//!   parent at the level above, `x / 2`, `y / 2`) — are kept in Morton
//!   order, where a tile's quadtree descendants are one interval
//!   ([`subtree`]): the interval of `of` holds every geometric held tile
//!   whose walk can reach it, and each is walked to see whether it does.
//!   The other held tiles — a 3D Tiles arena id, a parent the need tells
//!   that is not the quadtree's, a cycle — are few, kept apart, and always
//!   walked;
//! - whose held tiles changed: the tiles a held tile's walk reaches, read
//!   by walking up from it.
//!
//! Every walk counts its steps in the step's `work`.

use super::tile_id::TileId;
use super::Budget;
use super::TileInfo;

/// The deepest level a Morton key resolves: a terrain id's coordinates
/// hold 27 bits, so its level is at most 27.
const DEPTH: u32 = 28;

/// A tile's place in Morton order: its source tag, its quadtree path to
/// [`DEPTH`] (two bits a level, the levels below its own zero), its level.
/// A tile's quadtree descendants follow it, before any tile that is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Morton {
    tag: u8,
    path: u64,
    level: u8,
}

/// The bits of `v` (32 at most), each followed by a zero.
fn spread(v: u64) -> u64 {
    let mut x = v & 0xFFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    (x | (x << 1)) & 0x5555_5555_5555_5555
}

/// The quadtree coordinate a terrain id reads as, when it is one: its
/// level within [`DEPTH`], its x and y within its level.
fn coord(t: TileId) -> Option<(u32, u64, u64)> {
    let (l, x, y) = t.terrain_coord();
    (l < DEPTH && x >> l == 0 && y >> l == 0).then_some((l, x, y))
}

/// `t`'s Morton key, when its id is a quadtree coordinate.
pub(super) fn morton(t: TileId) -> Option<Morton> {
    let (l, x, y) = coord(t)?;
    Some(Morton {
        tag: t.tag(),
        path: ((spread(x) << 1) | spread(y)) << (2 * (DEPTH - l)),
        level: u8::try_from(l).ok()?,
    })
}

/// The Morton keys of `t` and of every quadtree descendant of it, as an
/// interval.
pub(super) fn subtree(t: TileId) -> Option<std::ops::RangeInclusive<Morton>> {
    let m = morton(t)?;
    let span = 1u64 << (2 * (DEPTH - u32::from(m.level)));
    let last = Morton {
        tag: m.tag,
        path: m.path + (span - 1),
        level: u8::MAX,
    };
    Some(m..=last)
}

/// The quadtree's parent of `t`, when its id is a coordinate below the
/// root.
pub(super) fn geometric_parent(t: TileId) -> Option<TileId> {
    let (l, x, y) = coord(t)?;
    (l > 0).then(|| TileId::from_terrain(l - 1, x / 2, y / 2).with_tag(t.tag()))
}

impl Budget {
    /// Whether the walk from `h` up the parent links `info` tells is
    /// geometric — each link the quadtree's own, each tile's level its id's
    /// — and if so, the level of the highest tile it reaches (`u8::MAX`: it
    /// reaches none). Such a walk reaches `of` exactly when `of` is a
    /// quadtree ancestor of `h` no higher than that tile.
    pub(super) fn geometric_walk(
        &self,
        h: TileId,
        info: impl Fn(TileId) -> Option<TileInfo>,
    ) -> Option<u8> {
        let mut t = h;
        let mut top = u8::MAX;
        for _ in 0..64 {
            self.walk(1);
            let Some(i) = info(t) else {
                return Some(top);
            };
            if coord(t).map(|c| c.0) != Some(i.level) {
                return None;
            }
            let Some(p) = i.parent() else {
                return Some(top);
            };
            let gp = geometric_parent(t)?;
            if gp != p {
                return None;
            }
            top = morton(p)?.level;
            t = p;
        }
        Some(top)
    }

    /// Where the walk from `h` first reaches `of`: the lowest level it
    /// passed before it (`chain_of`'s pair for `of`), if it does.
    pub(super) fn reaches(
        &self,
        h: TileId,
        of: TileId,
        info: impl Fn(TileId) -> Option<TileInfo>,
    ) -> Option<u32> {
        let mut t = h;
        let mut passed = u32::MAX;
        for _ in 0..64 {
            self.walk(1);
            let i = info(t)?;
            let p = i.parent()?;
            if p == of {
                return Some(passed);
            }
            passed = passed.min(i.level);
            t = p;
        }
        None
    }

    /// The held tiles whose walk reaches `of`, with the lowest level each
    /// passed before it, in no particular order: the geometric ones in
    /// `of`'s Morton interval, the others all, each walked. `info` gives
    /// the tree positions the held set's geometry was read with.
    pub(super) fn held_under<'a, F>(
        &'a self,
        of: TileId,
        info: F,
    ) -> impl Iterator<Item = (TileId, u32)> + 'a
    where
        F: Fn(TileId) -> Option<TileInfo> + Copy + 'a,
    {
        // A geometric walk passes the levels of the quadtree: from `h`
        // to `of`, the lowest before `of`'s child is the level below it
        // (none when `h` is its child).
        let level = morton(of).map(|m| m.level);
        let regular = subtree(of).into_iter().flat_map(move |r| {
            self.down
                .held_regular()
                .range(r)
                .filter_map(move |(k, (h, top))| {
                    self.walk(1);
                    let l = level?;
                    (*h != of && *top <= l).then(|| {
                        let passed = if k.level == l + 1 {
                            u32::MAX
                        } else {
                            u32::from(l) + 2
                        };
                        (*h, passed)
                    })
                })
        });
        let irregular = self
            .down
            .held_irregular()
            .keys()
            .filter_map(move |h| self.reaches(*h, of, info).map(|p| (*h, p)));
        regular.chain(irregular)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_subtree_is_a_morton_interval() {
        let t = |l, x, y| TileId::from_terrain(l, x, y);
        let a = t(3, 5, 2);
        let r = subtree(a).unwrap();
        // Every descendant down to level 9 is in the interval, nothing else.
        for l in 0..10u32 {
            for x in 0..(1u64 << l).min(64) {
                for y in 0..(1u64 << l).min(64) {
                    let d = t(l, x, y);
                    let below = l >= 3 && (x >> (l - 3), y >> (l - 3)) == (5, 2);
                    assert_eq!(r.contains(&morton(d).unwrap()), below, "{l} {x} {y}");
                }
            }
        }
        assert_eq!(geometric_parent(t(4, 11, 5)), Some(a));
        assert_eq!(geometric_parent(t(0, 0, 0)), None);
        // Another source's tiles are elsewhere.
        assert!(!r.contains(&morton(t(4, 11, 5).with_tag(1)).unwrap()));
        // Not a coordinate: no key.
        assert_eq!(morton(t(2, 4, 0)), None);
    }
}
