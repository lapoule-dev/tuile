// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The decider's one currency (`docs/explorations/budget-eviction-prefetch-spec.md`
//! §1b, laws 6, 7 and 9): what a resource is worth, from the specification
//! in force, in integers.
//!
//! - [`Valuation`]: the specification's terms tabulated per class, level
//!   and copies held, so the arena's held-side index reads a resource's
//!   value without evaluating predicates; the restore-cost credit of
//!   GreedyDual-Size (the tier a release sends it back to, per byte).
//! - [`deficit_due`]: when a drawn tile's own level, a level or more over
//!   the one it wears, becomes worth its restore cost with the margin.
//!
//! Nothing here reads a clock or a global: every input is a parameter.

use crate::spec::{Facts, Specification, Term};
use crate::state::Held;
use crate::stats::LEVELS;
use crate::tier::Tier;
use crate::types::{Bytes, Class, Micros, PerClass, TileId};

/// The specification's terms of a held resource's value, tabulated for the
/// arena's key (`Specification` evaluated once per class, level and copies
/// held, not per key): what the key reads of the specification, so a key
/// filed is the key found as long as the table stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Valuation {
    /// Levels up to this are the coarse band.
    coarse_through: u32,
    /// The restore-cost credit, µs for a resource of `ref_bytes`, by class,
    /// level (`level_index`) and copies held: what releasing it would cost
    /// to undo (GreedyDual-Size's credit).
    credit: PerClass<[[u32; 3]; LEVELS]>,
    /// The recency weight, ‰, by class and level.
    recency: PerClass<[u32; LEVELS]>,
    /// The size the credit is given for.
    ref_bytes: u64,
}

impl Default for Valuation {
    /// Recency alone, no credit, no coarse band: least recently needed
    /// first.
    fn default() -> Self {
        Valuation {
            coarse_through: 0,
            credit: PerClass::splat([[0; 3]; LEVELS]),
            recency: PerClass::splat([1_000; LEVELS]),
            ref_bytes: 1,
        }
    }
}

impl Valuation {
    /// The table of `spec`, for a floor ending at `pinned`.
    pub fn of(spec: &crate::spec::Specification, pinned: Option<u32>) -> Self {
        use crate::spec::{Facts, Term};
        let mut v = Valuation {
            coarse_through: spec.params.coarse_through,
            ref_bytes: spec.params.ref_bytes.0.max(1),
            ..Valuation::default()
        };
        for class in Class::ALL {
            for at in 0..LEVELS {
                let level = u32::try_from(at).unwrap_or(u32::MAX);
                let band = spec.band(class, level, pinned);
                let facts = |tier| Facts {
                    class,
                    level,
                    tier,
                    band,
                    membership: None,
                    predicted: false,
                };
                v.recency[class][at] = spec.weight(Term::Recency, &facts(Tier::Remote));
                for h in Held::ALL {
                    // Releasing the GPU copy of one held twice demotes it to
                    // its decoded copy; any other release sends it back to
                    // the network.
                    let tier = match h {
                        Held::Both => Tier::Decoded,
                        Held::Gpu | Held::Cpu => Tier::Remote,
                    };
                    let w = u64::from(spec.weight(Term::RestoreCost, &facts(tier)));
                    let cost = spec.params.restore.of(tier).0;
                    v.credit[class][at][h.i()] =
                        u32::try_from(cost.saturating_mul(w) / 1_000).unwrap_or(u32::MAX);
                }
            }
        }
        v
    }
}


impl Valuation {
    /// Whether `level` is of the coarse band.
    pub fn coarse(&self, level: u32) -> bool {
        level <= self.coarse_through
    }

    /// The value of a held resource of `class` at level index `at`, last
    /// needed at `last_needed`, holding `held` copies of `bytes`: recency
    /// weighed by the specification, plus the restore-cost credit scaled by
    /// the reference size over its own (at most sixteen times). Law 6: a
    /// dearer restore or a smaller resource is worth more per byte held.
    pub fn held_value(
        &self,
        class: Class,
        at: usize,
        last_needed: Micros,
        held: Option<(Held, Bytes)>,
    ) -> u64 {
        let recency = u128::from(last_needed.0) * u128::from(self.recency[class][at]) / 1_000;
        let credit = held.map_or(0, |(h, bytes)| {
            let size = bytes.0.max(self.ref_bytes / 16).max(1);
            u128::from(self.credit[class][at][h.i()]) * u128::from(self.ref_bytes)
                / u128::from(size)
        });
        u64::try_from(recency.saturating_add(credit)).unwrap_or(u64::MAX)
    }
}

/// What [`deficit_due`] reads of a drawn tile.
#[derive(Clone, Copy, Debug)]
pub struct DeficitInput {
    /// The tile, for the spread of due times.
    pub tile: TileId,
    /// Its centrality, thousandths (`Quantised::centrality`).
    pub centrality: i32,
    /// Levels between the drape worn and the level needed.
    pub gap: u32,
    /// When the tile's need last changed.
    pub since: Micros,
    /// The facts a predicate reads (membership, band).
    pub facts: Facts,
}

/// When tile `d.tile`'s own drape becomes worth loading while it wears one
/// `d.gap` levels under its need (spec §5.3, law 7): once `w × gap × dwell ≥
/// cost × (1 + margin)`, the dwell counted from the last change of the
/// tile's need (a moving camera is not expected to keep the level), `w` the
/// place's screen weight times the specification's deficit weight, `cost`
/// the network's restore cost (the dearest tier: an upper bound until the
/// tiers are measured). Spread over 64 ms by the tile, so the tiles a stop
/// brought to rest together fall due over a few steps (§6.3, no peaks).
/// `None` when the specification gives the deficit no weight.
pub fn deficit_due(spec: &Specification, d: &DeficitInput) -> Option<Micros> {
    let weight = u64::from(spec.weight(Term::Deficit, &d.facts));
    // The screen weight, ‰: half for a tile at the edge, all at the centre
    // (the priority's own weighting).
    let screen = 500 + u64::try_from(d.centrality.clamp(0, 1_000)).unwrap_or(0) / 2;
    let per_us = weight * screen * u64::from(d.gap.max(1)) / 1_000;
    if per_us == 0 {
        return None;
    }
    let p = &spec.params;
    let cost = u128::from(p.restore.remote.0) * u128::from(1_000 + p.margin_permille);
    let dwell = u64::try_from(cost / u128::from(per_us)).unwrap_or(u64::MAX);
    let spread = d.tile.0.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58;
    Some(d.since.plus(Micros(dwell)).plus(Micros::from_millis(spread)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::need::Membership;
    use crate::spec::Band;

    fn input(gap: u32, centrality: i32) -> DeficitInput {
        DeficitInput {
            tile: TileId(0),
            centrality,
            gap,
            since: Micros::from_secs(10),
            facts: Facts {
                class: Class::Drape,
                level: 9,
                tier: Tier::Remote,
                band: Band::Fine,
                membership: Some(Membership::Drawn),
                predicted: false,
            },
        }
    }

    /// Law 9 made a number: with the interactive defaults (network 150 ms,
    /// margin 25 %, deficit ×1), an edge tile one level short is wanted
    /// 375 ms after its need settled, a centre one sooner, two levels short
    /// sooner still; a specification without the deficit never wants it.
    #[test]
    fn the_deficit_falls_due_sooner_for_more_quality() {
        let s = Specification::interactive();
        let at = |gap, c| deficit_due(&s, &input(gap, c)).map(|t| t.since(Micros::from_secs(10)));
        // TileId(0) spreads by nothing.
        assert_eq!(at(1, 0), Some(Micros::from_millis(375)));
        assert_eq!(at(1, 1_000), Some(Micros::from_millis(187).plus(Micros(500))));
        assert!(at(2, 0) < at(1, 0));
        let mut none = s.clone();
        none.set_weight(Term::Deficit, 0);
        assert_eq!(deficit_due(&none, &input(1, 0)), None);
    }
}
