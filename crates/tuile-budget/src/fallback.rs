// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Fallback specifications: which layer a tile's imagery is drawn from.
//!
//! An imagery layer may have a **substitute** — the same ground from another
//! source (`LayerId::substitute`). A [`FallbackRule`] says where the
//! substitute is worn in the layer's place: on the tiles at least `gap`
//! levels coarser than the **reference level**, the finest imagery level
//! drawn this step. Far tiles then keep the colours of the near ones where
//! the source's coarse levels are another acquisition.
//!
//! **Optional, and declared.** The default specification is empty, and an
//! empty specification changes nothing: [`FallbackState::apply`] returns
//! before reading the need. A host declares one; removing it is the
//! default again.
//!
//! The choice is an input to the want computation, not a mode: a rule
//! rewrites a far tile's [`LayerReach`] for the layer into its substitute's
//! before the budget reads the need, and from there the substitute drape is
//! a resource like any other (its own identity, charged and evicted as any
//! drape), dressed meanwhile by its family (`engine::family_drapes`).
//!
//! Integers and power-of-two comparisons only ([`needed_level`]): the same
//! choice on every platform. Design: `docs/explorations/imagery-layer-substitution.md`.

use std::collections::{BTreeMap, BTreeSet};

use super::imagery::{needed_level, LevelClamp};
use super::need::{LayerReach, Membership, NeedSet};
use super::types::{LayerId, Micros, TileId};

/// One rule: dress `layer` from its substitute where a tile is at least
/// `gap` levels coarser than the reference level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FallbackRule {
    /// The base layer it applies to.
    pub layer: LayerId,
    /// `G`: how many levels below the reference level a tile must be to be
    /// drawn from the substitute. At least 1.
    pub gap: u32,
    /// `H`: levels of hysteresis, on the reference level (it falls only
    /// when the measured level is more than `H` below it) and per tile (a
    /// tile on the substitute leaves it only at a gap below `G − H`).
    /// Ignored by Batch, whose choice is a function of the frame.
    pub hysteresis: u32,
    /// How many consecutive failed substitute loads suspend the rule.
    pub failures: u32,
    /// How long the first suspension lasts; each one in a row doubles it,
    /// up to 64 times.
    pub backoff: Micros,
}

impl FallbackRule {
    /// The default gap: texels sixteen times the nearest ground's.
    pub const GAP: u32 = 4;
    /// The default hysteresis: one level.
    pub const HYSTERESIS: u32 = 1;
    /// The default number of failures before backing off.
    pub const FAILURES: u32 = 3;
    /// The default first back-off.
    pub const BACKOFF: Micros = Micros(5_000_000);

    /// The rule for `layer` with the default numbers.
    pub fn new(layer: LayerId) -> Self {
        FallbackRule {
            layer,
            gap: Self::GAP,
            hysteresis: Self::HYSTERESIS,
            failures: Self::FAILURES,
            backoff: Self::BACKOFF,
        }
    }

    /// The same rule with gap `gap`.
    #[must_use]
    pub fn with_gap(mut self, gap: u32) -> Self {
        self.gap = gap;
        self
    }

    /// The same rule with hysteresis `h`.
    #[must_use]
    pub fn with_hysteresis(mut self, h: u32) -> Self {
        self.hysteresis = h;
        self
    }
}

/// A specification invalid as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FallbackError {
    /// A number out of its bounds.
    #[error("fallback rule: {0}")]
    Bound(&'static str),
    /// A query that does not parse.
    #[error("fallback query: {0}")]
    Query(String),
}

/// What the host declares: the fallback rules in force. Empty — the
/// default — is no substitution at all.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FallbackSpec {
    /// The rules, at most one per layer.
    pub rules: Vec<FallbackRule>,
}

impl FallbackSpec {
    /// The most rules a specification holds.
    pub const MAX_RULES: usize = 8;

    /// No substitution: the default.
    pub fn none() -> Self {
        FallbackSpec::default()
    }

    /// One rule.
    pub fn one(rule: FallbackRule) -> Self {
        FallbackSpec { rules: vec![rule] }
    }

    /// Whether it substitutes nothing.
    pub fn is_none(&self) -> bool {
        self.rules.is_empty()
    }

    /// Bounds every number: `1 ≤ gap ≤ 16`, `hysteresis < gap`, a base
    /// layer, at most one rule per layer and [`Self::MAX_RULES`] rules,
    /// `1 ≤ failures ≤ 100`, a back-off up to ten minutes.
    pub fn validate(&self) -> Result<(), FallbackError> {
        if self.rules.len() > Self::MAX_RULES {
            return Err(FallbackError::Bound("more than 8 rules"));
        }
        let mut seen = BTreeSet::new();
        for r in &self.rules {
            if r.layer.is_substitute() {
                return Err(FallbackError::Bound("a rule applies to a base layer"));
            }
            if !seen.insert(r.layer) {
                return Err(FallbackError::Bound("two rules for one layer"));
            }
            if !(1..=16).contains(&r.gap) {
                return Err(FallbackError::Bound("gap outside 1..=16"));
            }
            if r.hysteresis >= r.gap {
                return Err(FallbackError::Bound("hysteresis not below the gap"));
            }
            if !(1..=100).contains(&r.failures) {
                return Err(FallbackError::Bound("failures outside 1..=100"));
            }
            if r.backoff.0 > 600_000_000 {
                return Err(FallbackError::Bound("back-off above ten minutes"));
            }
        }
        Ok(())
    }

    /// The serial form: one rule as `layer=0&gap=4&hysteresis=1`
    /// (`failures` and `backoff_ms` when not the defaults), rules joined by
    /// `;`. Empty for none.
    pub fn to_query(&self) -> String {
        self.rules
            .iter()
            .map(|r| {
                let mut q = format!(
                    "layer={}&gap={}&hysteresis={}",
                    r.layer.0, r.gap, r.hysteresis
                );
                if r.failures != FallbackRule::FAILURES {
                    q.push_str(&format!("&failures={}", r.failures));
                }
                if r.backoff != FallbackRule::BACKOFF {
                    q.push_str(&format!("&backoff_ms={}", r.backoff.0 / 1_000));
                }
                q
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Reads [`Self::to_query`]'s form, validated. `""` and `"off"` are
    /// none; a rule's missing numbers are the defaults.
    pub fn from_query(q: &str) -> Result<Self, FallbackError> {
        let q = q.trim();
        if q.is_empty() || q == "off" || q == "none" {
            return Ok(FallbackSpec::none());
        }
        let mut spec = FallbackSpec::none();
        for part in q.split(';').filter(|p| !p.trim().is_empty()) {
            let mut rule = FallbackRule::new(LayerId(0));
            for pair in part.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| FallbackError::Query(format!("`{pair}` is not key=value")))?;
                let n: u64 = v
                    .trim()
                    .parse()
                    .map_err(|_| FallbackError::Query(format!("`{k}`: `{v}` is not a number")))?;
                let small = |n: u64| {
                    u32::try_from(n).map_err(|_| FallbackError::Query(format!("`{k}` too large")))
                };
                match k.trim() {
                    "layer" => {
                        rule.layer =
                            LayerId(u16::try_from(n).map_err(|_| {
                                FallbackError::Query("`layer` too large".to_owned())
                            })?)
                    }
                    "gap" => rule.gap = small(n)?,
                    "hysteresis" => rule.hysteresis = small(n)?,
                    "failures" => rule.failures = small(n)?,
                    "backoff_ms" => rule.backoff = Micros(n.saturating_mul(1_000)),
                    other => return Err(FallbackError::Query(format!("unknown key `{other}`"))),
                }
            }
            spec.rules.push(rule);
        }
        spec.validate()?;
        Ok(spec)
    }

    /// A digest of the specification, FNV-1a over its serial form: equal
    /// specifications give equal digests on every platform.
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.to_query().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    fn rule(&self, layer: LayerId) -> Option<&FallbackRule> {
        self.rules.iter().find(|r| r.layer == layer)
    }
}

/// What a step's [`FallbackState::apply`] reads beyond the need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackContext {
    /// The budget's pinned floor: tiles at or above it keep the source.
    pub pinned_level: Option<u32>,
    /// How many levels imagery may sit above terrain
    /// ([`crate::BudgetConfig::imagery_cap`]).
    pub over_terrain_cap: u32,
    /// Batch: no hysteresis, the choice a function of the frame.
    pub batch: bool,
    /// The step's time.
    pub now: Micros,
}

/// The gap a switch was decided at, clamped to the `gap` label's range
/// (−8 to 8, [`crate::observe::labels::GAP`]).
pub const GAP_LABELS: usize = 17;

/// What one step decided, for the telemetry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FallbackReport {
    /// Tiles that moved to a substitute this step, by gap (index `gap + 8`).
    pub to_substitute: [u32; GAP_LABELS],
    /// Tiles that moved back to the source, by gap.
    pub to_source: [u32; GAP_LABELS],
    /// Tiles drawn from a substitute this step.
    pub substituted: u32,
    /// Suspensions started since the last report.
    pub backoffs: u32,
}

impl FallbackReport {
    fn note(into: &mut [u32; GAP_LABELS], gap: i64) {
        let i = usize::try_from(gap.clamp(-8, 8) + 8).unwrap_or(0);
        into[i] = into[i].saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Health {
    /// Consecutive failed substitute loads.
    failures: u32,
    /// Suspensions in a row: the back-off's exponent.
    suspensions: u32,
    /// Until when the rule is suspended.
    until: Option<Micros>,
}

/// The fallback specification in force and what it remembers between
/// steps: each layer's reference level, the tiles on a substitute, the
/// substitute's health.
#[derive(Clone, Debug, Default)]
pub struct FallbackState {
    spec: FallbackSpec,
    reference: BTreeMap<LayerId, u32>,
    on: BTreeSet<(TileId, LayerId)>,
    health: BTreeMap<LayerId, Health>,
    backoffs: u32,
}

impl FallbackState {
    /// No specification: substitutes nothing.
    pub fn new() -> Self {
        FallbackState::default()
    }

    /// The specification in force.
    pub fn spec(&self) -> &FallbackSpec {
        &self.spec
    }

    /// Whether a rule is in force (declared; it may be suspended).
    pub fn active(&self) -> bool {
        !self.spec.is_none()
    }

    /// Puts `spec` in force from the next step. What a rule kept is kept for
    /// a layer still under one (a re-specification moves no tile by
    /// itself); a layer whose rule is gone forgets it, and its tiles return
    /// to the source — swapped new before old, by the budget.
    pub fn set_spec(&mut self, spec: FallbackSpec) {
        let keep = |l: &LayerId| spec.rule(*l).is_some();
        self.reference.retain(|l, _| keep(l));
        self.health.retain(|l, _| keep(l));
        self.on.retain(|(_, l)| keep(l));
        self.spec = spec;
    }

    /// The reference level of `layer`, as of the last step.
    pub fn reference(&self, layer: LayerId) -> Option<u32> {
        self.reference.get(&layer).copied()
    }

    /// Whether `layer`'s rule is suspended at `now`: its substitute failed
    /// too often in a row.
    pub fn suspended(&self, layer: LayerId, now: Micros) -> bool {
        self.health
            .get(&layer.base())
            .and_then(|h| h.until)
            .is_some_and(|u| now < u)
    }

    /// Whether any rule is suspended at `now`.
    pub fn any_suspended(&self, now: Micros) -> bool {
        self.spec.rules.iter().any(|r| self.suspended(r.layer, now))
    }

    /// A load of a substitute drape of `layer` failed (not an absence: an
    /// absent tile is stood over by the loader). The rule's `failures` in a
    /// row suspend it for its back-off, doubled each time in a row.
    pub fn on_failed(&mut self, layer: LayerId, now: Micros) {
        if !layer.is_substitute() {
            return;
        }
        let base = layer.base();
        let Some(rule) = self.spec.rule(base).copied() else {
            return;
        };
        let h = self.health.entry(base).or_default();
        h.failures = h.failures.saturating_add(1);
        if h.failures >= rule.failures {
            let factor = 1u64 << h.suspensions.min(6);
            h.until = Some(Micros(
                now.0.saturating_add(rule.backoff.0.saturating_mul(factor)),
            ));
            h.suspensions = h.suspensions.saturating_add(1);
            h.failures = 0;
            self.backoffs = self.backoffs.saturating_add(1);
        }
    }

    /// A substitute drape of `layer` loaded: the count of failures starts
    /// again, and so does the back-off.
    pub fn on_loaded(&mut self, layer: LayerId) {
        if layer.is_substitute() {
            if let Some(h) = self.health.get_mut(&layer.base()) {
                h.failures = 0;
                h.suspensions = 0;
            }
        }
    }

    /// Rewrites `need` for the rules in force: each tile far enough below
    /// its layer's reference level wears the substitute instead, where the
    /// loader has one (`substitute(tile, layer)` answers its deepest level
    /// over the tile, or `None`). Returns what it decided.
    ///
    /// Nothing is read, nothing is changed, when no rule is in force.
    pub fn apply(
        &mut self,
        need: &mut NeedSet,
        substitute: impl Fn(TileId, LayerId) -> Option<u32>,
        ctx: FallbackContext,
    ) -> FallbackReport {
        let mut report = FallbackReport {
            backoffs: std::mem::take(&mut self.backoffs),
            ..FallbackReport::default()
        };
        if self.spec.is_none() {
            return report;
        }
        let floor = ctx.pinned_level.unwrap_or(0);
        let level_of = |level: u32, reach: &LayerReach, need: &super::need::Need| {
            let img = need.imagery.as_ref()?;
            Some(needed_level(
                None,
                img.footprint,
                LevelClamp {
                    floor,
                    terrain_level: level,
                    over_terrain_cap: ctx.over_terrain_cap,
                    source_max: reach.max_level,
                },
            ))
        };
        let rules = self.spec.rules.clone();
        let mut on = BTreeSet::new();
        let mut left = BTreeSet::new();
        for rule in rules {
            let layer = rule.layer;
            if self.suspended(layer, ctx.now) {
                self.reference.remove(&layer);
                continue;
            }
            // The reference: the finest level a drawn tile wearing the
            // layer needs, unrelaxed.
            let measured = need
                .drawn
                .iter()
                .filter_map(|(_, n)| {
                    let reach = n
                        .imagery
                        .as_ref()?
                        .layers
                        .iter()
                        .find(|r| r.layer == layer)?;
                    level_of(n.level, reach, n)
                })
                .max();
            let Some(measured) = measured else {
                self.reference.remove(&layer);
                continue;
            };
            let reference = match self.reference.get(&layer) {
                Some(&r) if !ctx.batch && measured <= r && measured + rule.hysteresis >= r => r,
                _ => measured,
            };
            self.reference.insert(layer, reference);
            let lists = [
                (Membership::Drawn, &mut need.drawn),
                (Membership::Blocker, &mut need.blockers),
                (Membership::Ancestor, &mut need.ancestors),
                (Membership::Ring, &mut need.ring),
            ];
            for (_, list) in lists {
                for (t, n) in list.iter_mut() {
                    // The floor keeps the source: the guarantee against
                    // black ground does not depend on an optional source.
                    if n.level <= floor && ctx.pinned_level.is_some() {
                        continue;
                    }
                    let Some(img) = n.imagery.as_ref() else {
                        continue;
                    };
                    let Some(i) = img.layers.iter().position(|r| r.layer == layer) else {
                        continue;
                    };
                    let Some(level) = level_of(n.level, &img.layers[i], n) else {
                        continue;
                    };
                    let Some(sub_max) = substitute(*t, layer) else {
                        continue;
                    };
                    let gap = i64::from(reference) - i64::from(level);
                    let was = self.on.contains(&(*t, layer));
                    let enter = i64::from(rule.gap);
                    let stay = enter - i64::from(rule.hysteresis);
                    let wants = level <= sub_max
                        && if was && !ctx.batch {
                            gap >= stay
                        } else {
                            gap >= enter
                        };
                    if wants {
                        if on.insert((*t, layer)) {
                            if !was {
                                FallbackReport::note(&mut report.to_substitute, gap);
                            }
                            if let Some(img) = n.imagery.as_mut() {
                                img.layers[i] = LayerReach {
                                    layer: layer.substitute(),
                                    max_level: sub_max,
                                };
                            }
                        }
                    } else if was && left.insert((*t, layer)) {
                        FallbackReport::note(&mut report.to_source, gap);
                    }
                }
            }
            report.substituted = report.substituted.saturating_add(
                u32::try_from(
                    need.drawn
                        .iter()
                        .filter(|(t, _)| on.contains(&(*t, layer)))
                        .count(),
                )
                .unwrap_or(u32::MAX),
            );
        }
        // A tile listed nowhere this step is forgotten: it has left.
        self.on = on;
        report
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_query_round_trips_and_is_bounded() {
        let spec = FallbackSpec::one(FallbackRule::new(LayerId(0)).with_gap(5));
        assert_eq!(spec.to_query(), "layer=0&gap=5&hysteresis=1");
        assert_eq!(FallbackSpec::from_query(&spec.to_query()).unwrap(), spec);
        assert_eq!(
            FallbackSpec::from_query("off").unwrap(),
            FallbackSpec::none()
        );
        assert!(FallbackSpec::from_query("gap=0").is_err());
        assert!(FallbackSpec::from_query("gap=2&hysteresis=2").is_err());
        assert!(FallbackSpec::from_query("layer=0;layer=0").is_err());
        assert!(FallbackSpec::from_query("colour=4").is_err());
        assert_ne!(spec.digest(), FallbackSpec::none().digest());
    }
}
