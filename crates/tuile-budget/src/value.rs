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
//! - [`deficit_due`]: when a drawn tile's own level, a level or more from
//!   the one it wears, becomes worth its restore cost with the margin.
//!
//! The currency is the **flouz** (spec §1b): one level of detail gained
//! over one screen pixel for one step — px·lvl·step. [`Flouz`] and
//! [`FlouzPerByte`] carry it; a time is a [`Micros`] and never a value.
//!
//! Nothing here reads a clock or a global: every input is a parameter.

use crate::priority::Quantised;
use crate::priority::Urgency;
use crate::spec::{Facts, Progressive, Specification, Term};
use crate::state::Held;
use crate::stats::LEVELS;
use crate::tier::Tier;
use crate::types::{Bytes, Class, Micros, PerClass, TileId};

/// An amount of the currency, in millionths of a flouz (µflouz): px·lvl·µs
/// at one level over one pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flouz(pub u64);

impl Flouz {
    /// What releasing a resource loses (spec §1b): the quality missing
    /// while it comes back — `P × Δq × w × T_restore` — here at `P` = 1, one
    /// level over `pixels`, for the restore time `restore`. The restore cost
    /// is in the numerator: what is dear to restore is worth more held.
    pub fn loss(pixels: u64, levels: u64, restore: Micros) -> Flouz {
        Flouz(pixels.saturating_mul(levels).saturating_mul(restore.0))
    }
}

/// The currency per byte held, in a fixed-point scale of the specification
/// — `ref_bytes / ref_pixels` µflouz per byte, one unit per µs of recency
/// for a resource of the reference size: the release order compares only
/// values of one specification, so the scale is the same on both sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlouzPerByte(pub u64);

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
                    // The loss of a release, one level over one pixel
                    // (the reference size's), in µflouz.
                    let cost = Flouz::loss(1, 1, spec.params.restore.of(tier)).0;
                    v.credit[class][at][h.i()] =
                        u32::try_from(cost.saturating_mul(w) / 1_000).unwrap_or(u32::MAX);
                }
            }
        }
        v
    }
}

impl Valuation {
    /// `R`: the restore-cost credit of a resource of `class` at level
    /// index `at` holding `held`, µs for a resource of the reference size.
    pub fn credit(&self, class: Class, at: usize, held: Held) -> u32 {
        self.credit[class][at][held.i()]
    }

    /// The size the credit is given for.
    pub fn ref_bytes(&self) -> u64 {
        self.ref_bytes
    }

    /// Whether `level` is of the coarse band.
    pub fn coarse(&self, level: u32) -> bool {
        level <= self.coarse_through
    }

    /// The value of a held resource of `class` at level index `at`, last
    /// needed at `last_needed`, holding `held` copies of `bytes`, of
    /// probability of use `probability` ([`CERTAIN`] units): recency
    /// weighed by the specification, plus the restore-cost credit scaled by
    /// the reference size over its own (at most sixteen times) and by the
    /// probability — the progressive score `P × R / bytes`
    /// (`docs/explorations/budget-progressive-coarse-preload.md` §2) over
    /// GreedyDual-Size's `L`. Law 6: a dearer restore, a smaller resource
    /// or a likelier use is worth more per byte held.
    pub fn held_value(
        &self,
        class: Class,
        at: usize,
        last_needed: Micros,
        held: Option<(Held, Bytes)>,
        probability: u32,
    ) -> FlouzPerByte {
        let recency = u128::from(last_needed.0) * u128::from(self.recency[class][at]) / 1_000;
        let credit = held.map_or(0, |(h, bytes)| {
            let size = bytes.0.max(self.ref_bytes / 16).max(1);
            u128::from(self.credit[class][at][h.i()])
                * u128::from(self.ref_bytes)
                * u128::from(probability.min(CERTAIN))
                / (u128::from(size) * u128::from(CERTAIN))
        });
        FlouzPerByte(u64::try_from(recency.saturating_add(credit)).unwrap_or(u64::MAX))
    }
}

/// Certainty, the scale of a probability of use: a probability `p` is
/// `p × CERTAIN`. A multiple of 1 000, so a probability in thousandths is
/// exact on it; 2¹⁵ halvings of it reach one unit.
pub const CERTAIN: u32 = 32_000;

/// The progressive score's probability of use of a tile of `level`, the
/// view needing `need_level` at its nearest point, `away` sixteenths of a
/// tile of its own level from there, in a neighbourhood of radius `rho`
/// tiles (`docs/explorations/budget-progressive-coarse-preload.md` §2):
/// `A(ℓ, n) × D(d)`, in [`CERTAIN`] units, at least one unit.
///
/// - `A(ℓ, n)` is 1 at the need's level, `a0 × 2^−(n−1−ℓ)` above it,
///   `a↓ × 2^−(ℓ−n−1)` below it;
/// - `D(d)` is `ρ² / (ρ + d)²`: 1 at the footprint, ¼ at `d = ρ`.
pub fn progressive(p: &Progressive, level: u32, need_level: u32, away: u32, rho: u32) -> u32 {
    let (a, d) = progressive_terms(p, level, need_level, away, rho);
    product(a, d)
}

/// `A × D` in [`CERTAIN`] units, at least one unit.
pub fn product(a: u32, d: u32) -> u32 {
    let c = u64::from(CERTAIN);
    u32::try_from((u64::from(a) * u64::from(d) / c).max(1)).unwrap_or(CERTAIN)
}

/// [`progressive`]'s two terms, `A(ℓ, n)` and `D(d)`, each in [`CERTAIN`]
/// units.
pub fn progressive_terms(
    p: &Progressive,
    level: u32,
    need_level: u32,
    away: u32,
    rho: u32,
) -> (u32, u32) {
    let c = u64::from(CERTAIN);
    let a = match level.cmp(&need_level) {
        std::cmp::Ordering::Equal => c,
        std::cmp::Ordering::Less => {
            (c * u64::from(p.a0_permille) / 1_000) >> (need_level - 1 - level).min(63)
        }
        std::cmp::Ordering::Greater => {
            (c * u64::from(p.descend_permille) / 1_000) >> (level - need_level - 1).min(63)
        }
    };
    // The graded coarse levels: at least the boost at `coarse_from`, halving
    // per level finer.
    let from = u32::from(p.coarse_from);
    let a = if level >= from {
        a.max((c * u64::from(p.coarse_boost_permille) / 1_000) >> (level - from).min(63))
    } else {
        a
    };
    (
        u32::try_from(a).unwrap_or(CERTAIN),
        near(u64::from(rho) * 16, u64::from(away)),
    )
}

/// `D`'s shape, `r² / (r + d)²` in [`CERTAIN`] units: 1 at `d` = 0, ¼ at
/// `d = r`; with `r` = 0, 1 at `d` = 0 and 0 beyond. The progressive
/// score's distance term (`r` = `ρ`, `d` in sixteenths of a tile) and the
/// predicted use's time term (`r` = the horizon, `d` the slack in µs).
fn near(r: u64, d: u64) -> u32 {
    let c = u128::from(CERTAIN);
    let (r, d) = (u128::from(r), u128::from(d));
    let v = if r == 0 {
        if d == 0 {
            c
        } else {
            0
        }
    } else {
        c * r * r / ((r + d) * (r + d))
    };
    u32::try_from(v).unwrap_or(CERTAIN)
}

/// The predicted use's terms (#282's [`crate::AheadUse`]), each in
/// [`CERTAIN`] units, the progressive score's `A` and `D` read in time:
/// `A` is the prediction's probability, `D` the same `r² / (r + d)²` as
/// the distance term with `r` the horizon `H` and `d` the slack — the
/// delay before the use less the restore cost of the tier the resource
/// would come from, `(t_use − R(tier))⁺`. A use due within its load time
/// is as urgent as the present place (`D` = 1); one a horizon past it
/// weighs a quarter. The tier's restore cost is the one the budget
/// measures (`Budget::restore_costs`), when `spec` carries it.
pub fn predicted_terms(
    spec: &Specification,
    ahead: Micros,
    permille: u32,
    tier: Tier,
) -> (u32, u32) {
    let p = &spec.params;
    let a = permille.min(1_000) * (CERTAIN / 1_000);
    let slack = ahead.0.saturating_sub(p.restore.of(tier).0);
    (a, near(p.horizon.0, slack))
}

/// A want's `A` and `D` ([`probability`]'s terms), in [`CERTAIN`] units: a
/// want the traversal did not place has `D` = 1 and its flat probability
/// as `A`.
///
/// A ring tile a prediction names (`Quantised::ahead`) takes the larger of
/// its place's terms and the prediction's ([`predicted_terms`], the tier
/// `tier` it would come from): a prediction only ever raises a placed
/// preload's probability — it adds to the preload band and keeps a tile
/// predicted soon longer, never lowers what the place gives. A ring tile
/// the traversal did not place takes the prediction's terms: the flat
/// anticipation is what it takes without one.
pub fn probability_terms(
    spec: &Specification,
    rho: u32,
    u: Urgency,
    level: u32,
    q: &Quantised,
    tier: Tier,
) -> (u32, u32) {
    let placed = place_terms(spec, rho, u, level, q);
    match (u, q.ahead()) {
        (Urgency::Ring, Some((ahead, permille))) => {
            let predicted = predicted_terms(spec, ahead, permille, tier);
            // Unplaced, the flat anticipation stood in for a probability
            // the need did not give; given one, it is the prediction's.
            if !q.placed() || product(predicted.0, predicted.1) > product(placed.0, placed.1) {
                predicted
            } else {
                placed
            }
        }
        _ => placed,
    }
}

/// [`probability_terms`] without the prediction: the place's.
fn place_terms(
    spec: &Specification,
    rho: u32,
    u: Urgency,
    level: u32,
    q: &Quantised,
) -> (u32, u32) {
    let p = &spec.params;
    let permille = |x: u16| u32::from(x) * (CERTAIN / 1_000);
    match u {
        Urgency::Ring | Urgency::Ancestor if q.placed() => progressive_terms(
            &p.progressive,
            level,
            u32::from(q.need_level),
            u32::from(q.away),
            rho,
        ),
        Urgency::Ring => (permille(p.anticipation.ring_permille), CERTAIN),
        Urgency::Ancestor => (permille(p.anticipation.ancestor_permille), CERTAIN),
        Urgency::Refresh => (permille(100), CERTAIN),
        _ => (CERTAIN, CERTAIN),
    }
}

/// A want's probability of use, in [`CERTAIN`] units: the present need's
/// is certain; a preload the traversal placed against the need takes the
/// progressive score ([`progressive`]), one it did not the specification's
/// flat anticipation; a refresh a tenth.
pub fn probability(
    spec: &Specification,
    rho: u32,
    u: Urgency,
    level: u32,
    q: &Quantised,
    tier: Tier,
) -> u32 {
    let (a, d) = probability_terms(spec, rho, u, level, q, tier);
    if d == CERTAIN {
        a
    } else {
        product(a, d)
    }
}

/// What loading a missing resource of the value band gains (spec §1b), in
/// flouz: `P × Δq × w × (H ∓ T_fetch)` — its probability of use
/// `probability` ([`probability`], [`CERTAIN`] units), one level, the
/// place's screen weight (‰ of the reference pixel), over the
/// specification's horizon. For the present need, counted only from its
/// arrival on, `(H − T)⁺`: a slow fetch earns less, a local one is
/// preferred at equal worth. For a preload, `H + T`: the lead time a tier
/// needs — what the measured network makes slow to come back is started
/// sooner (`docs/explorations/budget-progressive-coarse-preload.md` §2).
pub fn load_gain(
    spec: &Specification,
    probability: u32,
    centrality: i32,
    tier: Tier,
    preload: bool,
) -> Flouz {
    let p = &spec.params;
    let screen = 500 + u64::try_from(centrality.clamp(0, 1_000)).unwrap_or(0) / 2;
    let t = p.restore.of(tier).0;
    let after = if preload {
        p.horizon.0.saturating_add(t)
    } else {
        p.horizon.0.saturating_sub(t)
    };
    Flouz(
        u64::from(probability)
            .saturating_mul(screen)
            .saturating_mul(after)
            / (u64::from(CERTAIN) * 1_000),
    )
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
    /// Whether the drape worn is finer than the level needed: a gap that
    /// costs colour consistency, not detail, weighed
    /// the specification's `finer_gap_permille` of an under-need gap.
    pub finer: bool,
    /// When the tile's need last changed.
    pub since: Micros,
    /// The facts a predicate reads (membership, band).
    pub facts: Facts,
}

/// When tile `d.tile`'s own drape becomes worth loading while it wears one
/// `d.gap` levels under its need, or over it (`d.finer`, weighed the
/// specification's `finer_gap_permille`: a drape worn finer than its need
/// shows another acquisition than its neighbours — colour consistency
/// before extra detail — but weighs less than missing detail) (spec §5.3, law 7): once `w × gap × dwell ≥
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
    let mut per_us = weight * screen * u64::from(d.gap.max(1)) / 1_000;
    if d.finer {
        per_us = per_us * u64::from(spec.params.finer_gap_permille) / 1_000;
    }
    if per_us == 0 {
        return None;
    }
    let p = &spec.params;
    let cost = u128::from(p.restore.remote.0) * u128::from(1_000 + p.margin_permille);
    let dwell = u64::try_from(cost / u128::from(per_us)).unwrap_or(u64::MAX);
    let spread = d.tile.0.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58;
    Some(
        d.since
            .plus(Micros(dwell))
            .plus(Micros::from_millis(spread)),
    )
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
            finer: false,
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

    /// The inversion the first draft of the spec had (dividing by the
    /// restore cost) is not the code: with the same recency, size and
    /// class, a resource that would come back from the network is worth
    /// more held than one that would come back from its decoded copy, and
    /// so is released after it (law 6).
    #[test]
    fn a_dear_restore_is_worth_more_held() {
        let v = Valuation::of(&Specification::interactive(), None);
        let at = crate::stats::level_index(9);
        let bytes = Bytes(256 << 10);
        let network = v.held_value(
            Class::Drape,
            at,
            Micros(1_000),
            Some((Held::Gpu, bytes)),
            CERTAIN,
        );
        let decoded = v.held_value(
            Class::Drape,
            at,
            Micros(1_000),
            Some((Held::Both, bytes)),
            CERTAIN,
        );
        assert!(network > decoded, "{network:?} {decoded:?}");
        assert!(Flouz::loss(1, 1, Micros(150_000)) > Flouz::loss(1, 1, Micros(2_000)));
    }

    /// The load side of the currency: the present need over anticipation,
    /// the centre over the edge, a local tier over the network.
    #[test]
    fn a_load_gains_by_probability_place_and_arrival() {
        let s = Specification::interactive();
        let q = Quantised::default();
        let g = |u, c, t| load_gain(&s, probability(&s, 4, u, 9, &q, t), c, t, false);
        assert!(g(Urgency::Drape, 0, Tier::Remote) > g(Urgency::Ring, 0, Tier::Remote));
        assert!(g(Urgency::Ring, 0, Tier::Remote) > g(Urgency::Ancestor, 0, Tier::Remote));
        assert!(g(Urgency::Drape, 1_000, Tier::Remote) > g(Urgency::Drape, 0, Tier::Remote));
        assert!(g(Urgency::Drape, 0, Tier::Stored) > g(Urgency::Drape, 0, Tier::Remote));
        assert!(g(Urgency::Refresh, 1_000, Tier::Decoded) < g(Urgency::Drape, 0, Tier::Remote));
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
        assert_eq!(
            at(1, 1_000),
            Some(Micros::from_millis(187).plus(Micros(500)))
        );
        assert!(at(2, 0) < at(1, 0));
        let mut none = s.clone();
        none.set_weight(Term::Deficit, 0);
        assert_eq!(deficit_due(&none, &input(1, 0)), None);
    }

    /// Colour consistency before detail: a drape worn finer than its need
    /// falls due — a gap over the need is a want — but after an under-need
    /// gap of the same size: two levels over weigh as one under.
    #[test]
    fn a_finer_gap_falls_due_after_an_under_gap_of_the_same_size() {
        let s = Specification::interactive();
        let at = |gap, finer| {
            let d = DeficitInput {
                finer,
                ..input(gap, 0)
            };
            deficit_due(&s, &d).map(|t| t.since(Micros::from_secs(10)))
        };
        assert_eq!(at(1, true), Some(Micros::from_millis(750)));
        assert!(at(1, false) < at(1, true));
        assert!(at(2, false) < at(2, true));
        assert_eq!(at(2, true), at(1, false));
    }
}
