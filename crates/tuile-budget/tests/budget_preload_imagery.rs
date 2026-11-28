// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A preload carries its imagery: a tile the budget preloads (the
//! preload score's candidates around the pose, the views ahead, the chain above what is drawn)
//! is wanted with the drapes its need lists, so that when the view draws
//! it, its ground arrives dressed at its own level and not as geometry under
//! a coarser ancestor's drape.
//!
//! The need is the one the geometry server's traversal builds
//! (`tuile_core::runtime::need`, `need_of`): every entry, preload or not,
//! carries its imagery need.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use std::collections::BTreeSet;

use budget_common::*;
use tuile_budget::{AheadUse, Micros, Need, NeedSet, ResourceId, TileId};

/// The drawn ground: the level-13 tiles around `(x, y)` within one tile.
fn drawn_around(x: u64, y: u64) -> Vec<TileId> {
    let mut out = Vec::new();
    for dx in 0..2 {
        for dy in 0..2 {
            out.push(t(13, x + dx, y + dy));
        }
    }
    out
}

/// The need of a still view drawing `drawn` at level 13, every entry
/// draped at its own level: the chain of the drawn tiles as ancestors, and
/// preload candidates, level-13 neighbours two tiles out. `ahead` marks the candidates as
/// named by the views ahead.
fn still_view(drawn: &[TileId], ahead: Option<AheadUse>) -> NeedSet {
    let set: BTreeSet<TileId> = drawn.iter().copied().collect();
    let mut ns = NeedSet::default();
    for &d in drawn {
        ns.drawn.push((d, draped(d, 13)));
    }
    let mut chain = BTreeSet::new();
    for &d in drawn {
        let mut p = parent(d);
        while let Some(a) = p {
            chain.insert(a);
            p = parent(a);
        }
    }
    for a in chain {
        let (l, ..) = a.terrain_coord();
        ns.ancestors.push((a, draped(a, l)));
    }
    let (_, x0, y0) = drawn[0].terrain_coord();
    for x in x0 - 2..x0 + 4 {
        for y in y0 - 2..y0 + 4 {
            let id = t(13, x, y);
            if set.contains(&id) {
                continue;
            }
            let mut n = draped(id, 13);
            if let Some(a) = ahead {
                n = n.with_ahead(a);
            }
            ns.ring.push((id, n));
        }
    }
    ns
}

/// Every resource started while stepping on `need` with every load and
/// upload completed, until a step asks for nothing (at most 600 steps).
fn started_settling(r: &mut Rig, need: &NeedSet) -> BTreeSet<ResourceId> {
    let mut started = BTreeSet::new();
    for _ in 0..600 {
        let d = r.step(need);
        started.extend(d.start.iter().map(|s| s.resource));
        let idle = d.start.is_empty() && d.deliver.is_empty() && d.upload_plan.items.is_empty();
        if idle {
            break;
        }
        r.complete(&d);
    }
    started
}

/// The tiles of `list` whose mesh was started but none of whose drapes was.
fn bare_preloads(list: &[(TileId, Need)], started: &BTreeSet<ResourceId>) -> Vec<TileId> {
    list.iter()
        .map(|(id, _)| *id)
        .filter(|id| started.contains(&ResourceId::Mesh(*id)))
        .filter(|id| {
            !started
                .iter()
                .any(|r| matches!(r, ResourceId::Drape { tile, .. } if tile == id))
        })
        .collect()
}

/// The preload score's candidates (no prediction) and the chain above what is
/// drawn are preloaded with their drapes. With room for everything and a
/// still view (512 MiB, no pinned floor), every candidate and ancestor mesh the budget loads has a drape
/// loaded with it.
///
/// Red on main: `Budget::draped` (engine.rs) lets only drawn tiles and
/// blockers ask for drapes, so a candidate or ancestor tile wants its mesh alone
/// (`tile_wants`' arm for an undrawn candidate never runs).
#[test]
fn the_candidates_and_the_chains_preloads_carry_their_drapes() {
    let mut r = Rig::interactive();
    let drawn = drawn_around(4_100, 2_900);
    let need = still_view(&drawn, None);
    let started = started_settling(&mut r, &need);
    for &d in &drawn {
        assert!(
            started.contains(&ResourceId::Mesh(d)) && started.contains(&dr(d, 13)),
            "the fixture is wrong: drawn {d:?} not loaded with its drape"
        );
    }
    let candidates = bare_preloads(&need.ring, &started);
    let chain = bare_preloads(&need.ancestors, &started);
    let candidate_meshes = need
        .ring
        .iter()
        .filter(|(id, _)| started.contains(&ResourceId::Mesh(*id)))
        .count();
    assert!(
        candidate_meshes > 0,
        "the fixture is wrong: no candidate mesh preloaded"
    );
    assert!(
        candidates.is_empty() && chain.is_empty(),
        "preloaded without imagery: {} of {} candidate meshes, {} of {} ancestors",
        candidates.len(),
        candidate_meshes,
        chain.len(),
        need.ancestors.len()
    );
}

/// Candidates the views ahead name is preloaded with its drapes too: the
/// prediction that loads a mesh ahead of the camera loads its imagery.
///
/// Red on main for the same reason (`Budget::draped`); the case
/// `feat/budget-reads-ahead` (#355) covers, where `draped` admits a candidate
/// tile with `Need.ahead`.
#[test]
fn predicted_candidates_are_preloaded_with_their_drapes() {
    let mut r = Rig::interactive();
    let drawn = drawn_around(4_100, 2_900);
    let ahead = AheadUse {
        next_use_in: Micros::from_millis(500),
        probability: 900,
    };
    let need = still_view(&drawn, Some(ahead));
    let started = started_settling(&mut r, &need);
    let candidate_meshes = need
        .ring
        .iter()
        .filter(|(id, _)| started.contains(&ResourceId::Mesh(*id)))
        .count();
    assert!(
        candidate_meshes > 0,
        "the fixture is wrong: no candidate mesh preloaded"
    );
    let bare = bare_preloads(&need.ring, &started);
    assert!(
        bare.is_empty(),
        "predicted and preloaded without imagery: {} of {} candidate meshes",
        bare.len(),
        candidate_meshes
    );
}
