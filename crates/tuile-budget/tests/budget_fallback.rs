// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery layer substitution: the fallback specification's choice of layer
//! (`tuile_budget::fallback`), and the budget wearing a substitute like any
//! layer — its own resource, dressed meanwhile by its family, swapped new
//! before old. Design: `docs/explorations/imagery-layer-substitution.md`.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use budget_common::*;
use tuile_budget::{
    Class, FallbackContext, FallbackRule, FallbackSpec, FallbackState, ImageryNeed, LayerId,
    LayerReach, Micros, Need, NeedSet, ResourceId, Role, TileId,
};

const SUB: u16 = LayerId::SUBSTITUTE;

fn mesh(id: TileId) -> ResourceId {
    ResourceId::Mesh(id)
}

/// A drawn need for `id` at terrain level 20 whose imagery needs exactly
/// `level` (source reaching 22).
fn at(_id: TileId, level: u32) -> Need {
    Need::new(20, Class::TerrainMesh).with_imagery(ImageryNeed::base(footprint(level), 22))
}

fn tile(i: u64) -> TileId {
    t(20, i, 0)
}

/// Drawn tiles `0..`, each needing the level given.
fn view(levels: &[u32]) -> NeedSet {
    NeedSet {
        drawn: levels
            .iter()
            .enumerate()
            .map(|(i, l)| (tile(i as u64), at(tile(i as u64), *l)))
            .collect(),
        ..NeedSet::default()
    }
}

fn ctx(batch: bool, now: u64) -> FallbackContext {
    FallbackContext {
        pinned_level: None,
        over_terrain_cap: 30,
        batch,
        now: Micros(now),
    }
}

/// The layer each drawn tile wears after the rule, in order.
fn worn(need: &NeedSet) -> Vec<LayerId> {
    need.drawn
        .iter()
        .map(|(_, n)| n.imagery.as_ref().unwrap().layers[0].layer)
        .collect()
}

fn rule(gap: u32, h: u32) -> FallbackState {
    let mut s = FallbackState::new();
    s.set_spec(FallbackSpec::one(
        FallbackRule::new(LayerId(0))
            .with_gap(gap)
            .with_hysteresis(h),
    ));
    s
}

const SRC: LayerId = LayerId(0);
const SUBST: LayerId = LayerId(SUB);

mod choice {
    use super::*;

    /// The substitute is worn exactly when the gap to the reference level
    /// (the finest drawn, 16 here) is at least `G`: 12 is 4 below, 13 is 3.
    #[test]
    fn the_substitute_is_chosen_exactly_when_the_gap_reaches_g() {
        let mut s = rule(4, 1);
        let mut need = view(&[16, 12, 13, 11]);
        let r = s.apply(&mut need, |_, _| Some(21), ctx(false, 0));
        assert_eq!(s.reference(SRC), Some(16));
        assert_eq!(worn(&need), vec![SRC, SUBST, SRC, SUBST]);
        assert_eq!(r.substituted, 2);
        // gap 4 at index 12, gap 5 at 13.
        assert_eq!((r.to_substitute[12], r.to_substitute[13]), (1, 1));
        let sub = &need.drawn[1].1.imagery.as_ref().unwrap().layers[0];
        assert_eq!(
            *sub,
            LayerReach {
                layer: SUBST,
                max_level: 21
            },
            "the substitute's own reach"
        );
    }

    /// The default: no rule, nothing read, nothing changed — not even the
    /// loader asked whether it has a substitute.
    #[test]
    fn the_default_substitutes_nothing_and_asks_nothing() {
        let mut s = FallbackState::new();
        let mut need = view(&[16, 2]);
        let before = need.clone();
        let asked = std::cell::Cell::new(0);
        s.apply(
            &mut need,
            |_, _| {
                asked.set(asked.get() + 1);
                Some(21)
            },
            ctx(false, 0),
        );
        assert_eq!(need, before);
        assert_eq!(asked.get(), 0, "no substitute asked for");
        assert!(!s.active());
    }

    /// Without a substitute over the tile, past the substitute's reach, or
    /// at the floor, the source stays.
    #[test]
    fn no_substitute_no_reach_or_the_floor_keep_the_source() {
        let mut s = rule(2, 1);
        let mut need = view(&[16, 10, 10]);
        s.apply(&mut need, |t, _| (t != tile(1)).then_some(9), ctx(false, 0));
        assert_eq!(worn(&need), vec![SRC, SRC, SRC], "10 is past the reach 9");
        let mut need = view(&[16, 10]);
        need.drawn[1].1.level = 3;
        let mut c = ctx(false, 0);
        c.pinned_level = Some(3);
        let mut s = rule(2, 1);
        s.apply(&mut need, |_, _| Some(21), c);
        assert_eq!(worn(&need), vec![SRC, SRC], "the floor keeps the source");
    }

    /// Hysteresis on the reference: a near tile hesitating between 16 and
    /// 15 keeps the reference at 16, and the tile at 12 stays on the
    /// substitute. Per tile: a far tile hesitating between 12 and 13 stays.
    /// Red with no hysteresis (`H = 0`): both flap.
    #[test]
    fn hysteresis_keeps_a_tile_on_one_layer_at_the_boundary() {
        for (h, flaps) in [(1, false), (0, true)] {
            let mut s = rule(4, h);
            let mut changes = 0;
            let mut last = None;
            for step in 0..20u32 {
                let near = if step % 2 == 0 { 16 } else { 15 };
                let far = if step % 3 == 0 { 12 } else { 13 };
                let mut need = view(&[near, far]);
                s.apply(&mut need, |_, _| Some(21), ctx(false, u64::from(step)));
                let w = worn(&need)[1];
                if last.is_some_and(|l| l != w) {
                    changes += 1;
                }
                last = Some(w);
            }
            assert_eq!(changes > 0, flaps, "hysteresis {h}: {changes} changes");
        }
    }

    /// The reference falls only when the near level drops by more than the
    /// hysteresis.
    #[test]
    fn the_reference_falls_only_past_the_hysteresis() {
        let mut s = rule(4, 1);
        for (near, expect) in [(16, 16), (15, 16), (16, 16), (14, 14), (15, 15)] {
            let mut need = view(&[near]);
            s.apply(&mut need, |_, _| Some(21), ctx(false, 0));
            assert_eq!(s.reference(SRC), Some(expect), "near {near}");
        }
    }

    /// Batch has no hysteresis: the choice is a function of the frame.
    #[test]
    fn batch_chooses_from_the_frame_alone() {
        let mut s = rule(4, 1);
        let mut first = view(&[16, 12]);
        s.apply(&mut first, |_, _| Some(21), ctx(true, 0));
        let mut next = view(&[15, 12]);
        s.apply(&mut next, |_, _| Some(21), ctx(true, 1));
        let mut fresh = view(&[15, 12]);
        rule(4, 1).apply(&mut fresh, |_, _| Some(21), ctx(true, 1));
        assert_eq!(next, fresh, "the same frame, the same choice");
        assert_eq!(worn(&next), vec![SRC, SRC]);
    }

    /// An unreachable substitute backs off: three failures in a row suspend
    /// the rule (tiles back to the source), for 5 s, then 10 s; a success
    /// resets it.
    #[test]
    fn an_unreachable_substitute_is_backed_off() {
        let mut s = rule(4, 1);
        let sec = 1_000_000;
        let step = |s: &mut FallbackState, now| {
            let mut need = view(&[16, 12]);
            let r = s.apply(&mut need, |_, _| Some(21), ctx(false, now));
            (worn(&need)[1], r.backoffs)
        };
        assert_eq!(step(&mut s, 0), (SUBST, 0));
        for _ in 0..3 {
            s.on_failed(SUBST, Micros(sec));
        }
        assert!(s.suspended(SRC, Micros(sec)));
        assert_eq!(step(&mut s, sec), (SRC, 1), "suspended: the source");
        assert_eq!(step(&mut s, 7 * sec), (SUBST, 0), "5 s later: again");
        for _ in 0..3 {
            s.on_failed(SUBST, Micros(7 * sec));
        }
        assert!(s.suspended(SRC, Micros(16 * sec)), "doubled: 10 s");
        assert!(!s.suspended(SRC, Micros(18 * sec)));
        s.on_loaded(SUBST);
        s.on_failed(SUBST, Micros(18 * sec));
        assert!(
            !s.suspended(SRC, Micros(18 * sec)),
            "one failure is not a pattern"
        );
        // A source layer's failure is not the substitute's.
        for _ in 0..5 {
            s.on_failed(SRC, Micros(18 * sec));
        }
        assert!(!s.suspended(SRC, Micros(18 * sec)));
    }

    /// Removing the specification returns every tile to the source.
    #[test]
    fn removing_the_specification_returns_to_the_source() {
        let mut s = rule(4, 1);
        let mut need = view(&[16, 12]);
        s.apply(&mut need, |_, _| Some(21), ctx(false, 0));
        assert_eq!(worn(&need)[1], SUBST);
        s.set_spec(FallbackSpec::none());
        let mut need = view(&[16, 12]);
        s.apply(&mut need, |_, _| Some(21), ctx(false, 1));
        assert_eq!(worn(&need), vec![SRC, SRC]);
        assert_eq!(s.reference(SRC), None);
    }
}

mod budget {
    use super::*;

    /// A substitute sorts right after its base layer: an overlay above the
    /// base stays above the base's substitute.
    #[test]
    fn a_substitute_stacks_where_its_base_layer_does() {
        assert!(LayerId(0) < LayerId(SUB));
        assert!(LayerId(SUB) < LayerId(1));
        assert!(LayerId(1) < LayerId(1 | SUB));
        assert_eq!(LayerId(SUB).base(), LayerId(0));
        assert_eq!(LayerId(0).sibling(), LayerId(SUB));
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let d = r.settle(&drawn_layered(&[a], 3, &[SUB, 1]), 30);
        let item = d.draw.get(mesh(a)).expect("drawn");
        assert_eq!(
            item.drapes.iter().map(|x| x.resource).collect::<Vec<_>>(),
            vec![dl(a, SUB, 3), dl(a, 1, 3)],
            "the base's substitute under the overlay"
        );
    }

    /// A new tile whose substitute is held nowhere is dressed by an
    /// ancestor's **source** drape — the family stands in — and drawn as
    /// itself. Red when a layer falls back on its own drapes only: the tile
    /// and its ancestor are undressable and the ground is lost.
    #[test]
    fn a_tile_waiting_for_its_substitute_is_dressed_by_the_source() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        let a = children(root)[3];
        r.settle(&drawn_layered(&[root], 0, &[0]), 20);
        let mut need = drawn_layered(&[a], 1, &[SUB]);
        need.ancestors.push((root, layered(root, 0, &[0])));
        // Only meshes arrive: the substitute's drapes are never loaded.
        let mut d = r.step(&need);
        for _ in 0..20 {
            r.load_where(|x| matches!(x, ResourceId::Mesh(_)));
            r.upload(&d);
            d = r.step(&need);
        }
        assert_eq!(d.stats.lost, 0, "never black ground");
        assert_eq!(d.stats.bare, 0);
        let item = d.draw.get(mesh(a)).expect("the tile drawn");
        assert_eq!(item.role, Role::Exact);
        let worn = item
            .drape(LayerId(SUB))
            .expect("dressed for the substitute");
        assert_eq!(worn.resource, dl(root, 0, 0), "by its ancestor's source");
        assert!(
            r.pending.contains_key(&dl(a, SUB, 1)),
            "and its substitute still wanted"
        );
    }

    /// Source → substitute → source, at the same level: every step the tile
    /// wears a drawable drape, the drape it now wants is asked for even
    /// though the one it wears is at the right level (a stand-in of the
    /// other layer is always off), and the old one is replaced only once
    /// the new one is drawable. Red without the stand-in rule: the switch
    /// never happens.
    #[test]
    fn a_switch_either_way_never_leaves_a_step_without_a_drape() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.settle(&drawn_layered(&[a], 3, &[0]), 20);
        for (from, to) in [(0, SUB), (SUB, 0)] {
            let need = drawn_layered(&[a], 3, &[to]);
            let mut d = r.step(&need);
            // Asked for, or still held from the way there and worn at once.
            let worn_now = d
                .draw
                .get(mesh(a))
                .and_then(|i| i.drapes.first())
                .map(|x| x.resource);
            assert!(
                d.start.iter().any(|s| s.resource == dl(a, to, 3))
                    || worn_now == Some(dl(a, to, 3)),
                "the new layer's drape is asked for"
            );
            let mut swapped = false;
            for _ in 0..30 {
                let item = d.draw.get(mesh(a)).expect("drawn every step");
                let w = item.drapes.first().expect("dressed every step").resource;
                assert!(w == dl(a, from, 3) || w == dl(a, to, 3));
                if w == dl(a, to, 3) {
                    swapped = true;
                } else {
                    assert!(
                        !d.evict.contains(&dl(a, from, 3)),
                        "the old drape is kept while worn"
                    );
                }
                assert_eq!((d.stats.bare, d.stats.lost), (0, 0));
                r.complete(&d);
                d = r.step(&need);
            }
            assert!(swapped, "{from} → {to} completed");
        }
    }

    /// The substitute is a resource of its own: loaded beside the source
    /// drape of the same tile and level, and charged as a second drape.
    #[test]
    fn the_budget_charges_a_substitute_drape_apart() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.settle(&drawn_layered(&[a], 3, &[0]), 20);
        let before = r.b.ledger().gpu[Class::Drape];
        let d = r.step(&drawn_layered(&[a], 3, &[SUB]));
        assert!(d.start.iter().any(|s| s.resource == dl(a, SUB, 3)));
        r.complete(&d);
        let d = r.step(&drawn_layered(&[a], 3, &[SUB]));
        r.complete(&d);
        let after = r.b.ledger().gpu[Class::Drape];
        assert_eq!(
            after.0,
            before.0 + drape().0,
            "two drapes held, each charged"
        );
    }
}
