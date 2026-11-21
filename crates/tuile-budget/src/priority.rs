// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Request priority: an integer lexicographic key, total and deterministic.
//!
//! 1. the urgency tier;
//! 2. the score (higher first): SSE excess weighted by centrality, plus an
//!    age bonus against starvation and a cover bonus for a coarse tile that
//!    many waiting tiles fall back on;
//! 3. the residency tier (cheaper first: it closes a hole sooner);
//! 4. the resource id.
//!
//! No float is compared: [`Need`]'s floats are quantised once.

use std::cmp::Reverse;

use super::need::Need;
use super::tier::Tier;
use super::types::{Micros, ResourceId};

/// The urgency tiers, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    /// The pinned coarse floor: global coverage.
    Floor,
    /// A tile wanted on screen that is not drawable: it shows a fallback
    /// (coarser ancestor, or finer descendants kept on a zoom-out). Its
    /// absence forces either blur or over-detail.
    Hole,
    /// A child a held parent waits on before it can refine.
    Blocker,
    /// The drape a drawn tile needs, when the one it wears is not it.
    Drape,
    /// The prefetch ring.
    Ring,
    /// A preloaded ancestor.
    Ancestor,
    /// Resident content whose upstream epoch changed: refreshed with free
    /// slots and budget only, never preempting, new before old.
    Refresh,
}

/// A need's floats, quantised once at the boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quantised {
    /// SSE excess in thousandths, clamped to ±16 000.
    pub sse_excess: i32,
    /// Centrality in thousandths, `0..=1000`.
    pub centrality: i32,
    /// Distance in whole metres.
    pub distance: u32,
    /// Whether any input float was not finite (it then counts as zero).
    pub rejected: bool,
}

impl Quantised {
    /// Quantises a need's floats.
    // The float to integer casts are the quantisation itself, each clamped
    // into its integer's range first.
    #[allow(clippy::cast_possible_truncation)]
    pub fn of(need: &Need) -> Self {
        let mut rejected = false;
        let mut finite = |v: f32| {
            if v.is_finite() {
                v
            } else {
                rejected = true;
                0.0
            }
        };
        let sse = finite(need.sse_excess);
        let centrality = finite(need.centrality);
        let distance = finite(need.distance);
        Quantised {
            sse_excess: (sse.clamp(-16.0, 16.0) * 1000.0).round() as i32,
            centrality: (centrality.clamp(0.0, 1.0) * 1000.0).round() as i32,
            distance: distance.clamp(0.0, u32::MAX as f32) as u32,
            rejected,
        }
    }
}

/// The score within an urgency tier: higher is more urgent.
///
/// `sse_excess × (0.5 + 0.5 × centrality) + age_bonus + cover_bonus`, in
/// integers: the age bonus grows linearly to 1000 over `age_full`, the cover
/// bonus is 250 per waiting descendant, capped at 4000.
pub fn score(q: Quantised, waited: Micros, age_full: Micros, waiting_descendants: u32) -> i64 {
    let sse = i64::from(q.sse_excess) * (500 + i64::from(q.centrality) / 2) / 1000;
    let age = if age_full.0 == 0 {
        1000
    } else {
        // At most 1000: `waited` is capped at `age_full`.
        i64::try_from(u128::from(waited.0.min(age_full.0)) * 1000 / u128::from(age_full.0))
            .unwrap_or(1000)
    };
    let cover = (i64::from(waiting_descendants) * 250).min(4000);
    sse + age + cover
}

/// What a load's place in the order is fixed by before any value: the
/// absolute laws' bands (`docs/explorations/budget-eviction-prefetch-spec.md`
/// §1b, §2.1) — the floor, then coverage (a hole: law 1), then a swap's
/// blockers — and below them one band ordered by the decider's value
/// alone, the present need and anticipation together (the present need
/// ranks first there by its probability, not by a tier).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Precedence {
    /// The pinned floor (law 2, coverage first).
    Floor,
    /// A wanted tile not drawable (law 1).
    Coverage,
    /// A child a held parent waits on (law 1, a swap completes).
    Swap,
    /// Everything else, by value.
    Value,
}

impl Precedence {
    /// The band of `u`.
    pub fn of(u: Urgency) -> Self {
        match u {
            Urgency::Floor => Precedence::Floor,
            Urgency::Hole => Precedence::Coverage,
            Urgency::Blocker => Precedence::Swap,
            Urgency::Drape | Urgency::Ring | Urgency::Ancestor | Urgency::Refresh => {
                Precedence::Value
            }
        }
    }
}

/// The full priority key; smaller sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PriorityKey {
    /// The absolute laws' band.
    pub precedence: Precedence,
    /// The score, reversed so that higher sorts first: in the
    /// [`Precedence::Value`] band, the load's gain in flouz
    /// (`value::load_gain`); above it, the need's score.
    pub score: Reverse<i64>,
    /// The urgency tier: what the want is (a tie-break here; the
    /// admission rules read it).
    pub urgency: Urgency,
    /// The residency tier, cheaper first.
    pub tier: Tier,
    /// The final tie-break.
    pub resource: ResourceId,
}
