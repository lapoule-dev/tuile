// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`Specification`]: the policy of the one decider that chooses both what
//! to release and what to load ahead, as data
//! (`docs/explorations/budget-eviction-prefetch-spec.md` §3).
//!
//! A specification is a set of [`Rule`]s — each a value [`Term`] under a
//! [`Predicate`] over a resource's facts, with a weight — and the numbers of
//! the choice laws ([`Params`]). Predicates compose (`Not`, `All`, `Any`):
//! the Specification pattern. Film, known path, interactive and FPV use are
//! specifications of the same decider, built by [`Specification::interactive`],
//! [`Specification::known_path`], [`Specification::film`],
//! [`Specification::fpv`] and [`Specification::fpv_film`]; none is a mode,
//! and the decision code never branches on a specification's
//! [`SpecKind`], which is a label for telemetry and menus.
//!
//! The absolute laws — never a hole, coarse first, the hard limit, the
//! present need before anticipation, reproducibility — stand outside every
//! specification: no rule reaches the protected set or the order of the
//! bands, so a bad specification costs reloads, never a hole.
//!
//! Everything is an integer: weights in thousandths (‰), times in
//! [`Micros`], so a specification is `Eq + Hash`, digests the same on
//! every target, and replays exactly.

use std::fmt::Write as _;

use super::need::Membership;
use super::tier::Tier;
use super::types::{Bytes, Class, Micros, Policy};

/// What a specification is for: a label for telemetry and menus, never
/// branched on by the decision code (law 11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpecKind {
    /// A viewer or a game: probabilistic, short horizon.
    #[default]
    Interactive,
    /// A Glide flight or a tape replay: near-exact next use.
    KnownPath,
    /// A film: the exact schedule, no speculation.
    Film,
    /// First-person-view flight, interactive: low, fast, forward.
    Fpv,
    /// First-person-view flight in a film: the plan's exact schedule.
    FpvFilm,
    /// Anything else a host composes.
    Custom,
}

impl SpecKind {
    /// Every kind, in order.
    pub const ALL: [SpecKind; 6] = [
        SpecKind::Interactive,
        SpecKind::KnownPath,
        SpecKind::Film,
        SpecKind::Fpv,
        SpecKind::FpvFilm,
        SpecKind::Custom,
    ];

    /// Its label (`spec` in telemetry, `spec=` in a query).
    pub fn label(self) -> &'static str {
        match self {
            SpecKind::Interactive => "interactive",
            SpecKind::KnownPath => "known_path",
            SpecKind::Film => "film",
            SpecKind::Fpv => "fpv",
            SpecKind::FpvFilm => "fpv_film",
            SpecKind::Custom => "custom",
        }
    }

    /// The kind of `label`, if any.
    pub fn from_label(label: &str) -> Option<Self> {
        SpecKind::ALL.into_iter().find(|k| k.label() == label)
    }
}

/// The level band of a resource (law 2): coarse first, lexicographically,
/// whatever the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// The pinned floor: never released.
    Floor,
    /// Levels up to [`Params::coarse_through`].
    Coarse,
    /// The rest.
    Fine,
}

/// What a predicate reads of a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Facts {
    /// Its class.
    pub class: Class,
    /// Its tile's level.
    pub level: u32,
    /// Where it would be restored from, were it released.
    pub tier: Tier,
    /// Its band.
    pub band: Band,
    /// The membership of its tile in the need, if it is wanted.
    pub membership: Option<Membership>,
    /// Whether a prediction names it this step.
    pub predicted: bool,
}

/// A predicate over a resource's [`Facts`]: the Specification pattern
/// proper, composable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Predicate {
    /// Every resource.
    Always,
    /// Of this class.
    Class(Class),
    /// Of a level at most this.
    LevelAtMost(u32),
    /// Of a level at least this.
    LevelAtLeast(u32),
    /// Restored from this tier.
    Tier(Tier),
    /// In this band.
    Band(Band),
    /// Wanted under this membership.
    Membership(Membership),
    /// Named by a prediction.
    Predicted,
    /// Not the inner one.
    Not(Box<Predicate>),
    /// All of them (and; true when empty).
    All(Vec<Predicate>),
    /// Any of them (or; false when empty).
    Any(Vec<Predicate>),
}

/// `!p`: not `p`.
impl std::ops::Not for Predicate {
    type Output = Predicate;

    fn not(self) -> Predicate {
        Predicate::Not(Box::new(self))
    }
}

impl Predicate {
    /// Whether `f` satisfies it.
    pub fn holds(&self, f: &Facts) -> bool {
        match self {
            Predicate::Always => true,
            Predicate::Class(c) => f.class == *c,
            Predicate::LevelAtMost(l) => f.level <= *l,
            Predicate::LevelAtLeast(l) => f.level >= *l,
            Predicate::Tier(t) => f.tier == *t,
            Predicate::Band(b) => f.band == *b,
            Predicate::Membership(m) => f.membership == Some(*m),
            Predicate::Predicted => f.predicted,
            Predicate::Not(p) => !p.holds(f),
            Predicate::All(ps) => ps.iter().all(|p| p.holds(f)),
            Predicate::Any(ps) => ps.iter().any(|p| p.holds(f)),
        }
    }


    /// Its nesting depth (a leaf is 1).
    fn depth(&self) -> usize {
        match self {
            Predicate::Not(p) => 1 + p.depth(),
            Predicate::All(ps) | Predicate::Any(ps) => {
                1 + ps.iter().map(Predicate::depth).max().unwrap_or(0)
            }
            _ => 1,
        }
    }
}

/// A term of a resource's value, in microseconds of "worth keeping" — one
/// unit across terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Term {
    /// The time it was last needed (LRU's key; GreedyDual's `L`).
    Recency,
    /// Its restore cost by tier, per reference size (GreedyDual-Size's
    /// credit): `cost[tier] × ref_bytes / size`.
    RestoreCost,
    /// Its known or predicted next use: `p × (horizon − delay)⁺`.
    NextUse,
    /// The quality its load brings: `w × |need − worn| × dwell` (the load
    /// side of the deficit).
    Deficit,
    /// Its place's screen weight: large on screen first.
    ScreenSize,
}

impl Term {
    /// Every term, in order.
    pub const ALL: [Term; 5] = [
        Term::Recency,
        Term::RestoreCost,
        Term::NextUse,
        Term::Deficit,
        Term::ScreenSize,
    ];
}

/// One term of the value, applied to the resources its predicate selects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rule {
    /// The resources it applies to.
    pub when: Predicate,
    /// What it adds.
    pub term: Term,
    /// Thousandths: 1000 adds the term as is.
    pub weight: u32,
}

impl Rule {
    /// `term` × `weight`‰ for every resource.
    pub fn always(term: Term, weight: u32) -> Self {
        Rule {
            when: Predicate::Always,
            term,
            weight,
        }
    }
}

/// The cost of restoring a released resource, by the tier it would come
/// back from: the common base of every specification (GreedyDual's restore
/// cost per tier). Measured costs replace them once a tier has samples
/// (spec §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RestoreCosts {
    /// Decoded in memory: an upload.
    pub decoded: Micros,
    /// A local store: a read and a decode.
    pub stored: Micros,
    /// The browser's HTTP cache.
    pub http_cache: Micros,
    /// The network.
    pub remote: Micros,
}

impl RestoreCosts {
    /// The cost from `tier` (nothing for what is on the GPU).
    pub fn of(&self, tier: Tier) -> Micros {
        match tier {
            Tier::Gpu => Micros::ZERO,
            Tier::Decoded => self.decoded,
            Tier::Stored => self.stored,
            Tier::Remote => self.remote,
        }
    }
}

/// The quotas of speculation (law 8), in thousandths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Speculation {
    /// Of each class's load slots.
    pub slots_permille: u16,
    /// Of the effective target, held by speculative resources.
    pub memory_permille: u16,
}

/// The probability of use the value band gives anticipation (law 4: a
/// share, never a starvation), thousandths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Anticipation {
    /// A tile of the prefetch ring.
    pub ring_permille: u16,
    /// A preloaded ancestor.
    pub ancestor_permille: u16,
}

/// The defaults: the ring before the ancestors, as the tiers were.
const ANTICIPATION: Anticipation = Anticipation {
    ring_permille: 500,
    ancestor_permille: 250,
};

/// The numbers of the choice laws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Params {
    /// Levels up to this are the coarse band (law 2).
    pub coarse_through: u32,
    /// What a load's value must exceed the value it displaces by, and a
    /// deficit's quality its restore cost by, ‰ (law 7).
    pub margin_permille: u32,
    /// The least time a loaded resource is held, unless the hard limit
    /// forces it (law 7, open question Q1).
    pub min_dwell: Micros,
    /// The horizon of predictions and of the deficit's dwell.
    pub horizon: Micros,
    /// The restore costs per tier.
    pub restore: RestoreCosts,
    /// The reference size the restore cost is given for.
    pub ref_bytes: Bytes,
    /// Speculation's quotas.
    pub speculation: Speculation,
    /// How long a re-specification blends (law 12).
    pub blend: Micros,
    /// Anticipation's probabilities in the value band.
    pub anticipation: Anticipation,
}

/// The policy of the decider: rules and numbers. See the [module
/// documentation](self).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Specification {
    /// What it is for (a label).
    pub kind: SpecKind,
    /// The value's terms.
    pub rules: Vec<Rule>,
    /// The choice laws' numbers.
    pub params: Params,
}

/// Why a specification was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpecError {
    /// A number outside its bounds.
    #[error("`{0}` is out of bounds")]
    OutOfBounds(&'static str),
    /// Too many rules, or predicates nested too deep.
    #[error("the rules are too many or nested too deep")]
    TooComplex,
    /// A query named an unknown specification.
    #[error("unknown specification `{0}`")]
    UnknownKind(String),
    /// A query carried an unknown key.
    #[error("unknown parameter `{0}`")]
    UnknownKey(String),
    /// A query's value is not an integer.
    #[error("`{0}` is not a whole number")]
    NotANumber(String),
}

/// The most rules a specification holds.
pub const MAX_RULES: usize = 64;
/// The deepest a predicate nests.
pub const MAX_DEPTH: usize = 8;
/// The heaviest weight, ‰.
pub const MAX_WEIGHT: u32 = 16_000;

/// The defaults every specification starts from: an upload, a store read,
/// a browser-cache read and a network fetch of a 256 KiB tile, the order of
/// the O-B measurements, to be replaced by them.
const RESTORE: RestoreCosts = RestoreCosts {
    decoded: Micros::from_millis(2),
    stored: Micros::from_millis(6),
    http_cache: Micros::from_millis(15),
    remote: Micros::from_millis(150),
};

impl Specification {
    /// A viewer's or a game's: recency, restore cost, a short probabilistic
    /// next use, the deficit, a quarter of the slots and a fifth of the
    /// target for speculation.
    pub fn interactive() -> Self {
        Specification {
            kind: SpecKind::Interactive,
            rules: vec![
                Rule::always(Term::Recency, 1_000),
                Rule::always(Term::RestoreCost, 1_000),
                Rule::always(Term::NextUse, 1_000),
                Rule::always(Term::Deficit, 1_000),
                Rule::always(Term::ScreenSize, 1_000),
            ],
            params: Params {
                // The coarse band is a parameter for the search (PR 6): by
                // default only the floor is a band of its own — measured on
                // the five-minute soak, a band through level 5 lengthened
                // the mesh waits' p95 (law 2 per place is kept by the
                // fallback chain's protection and the finer-first ties).
                coarse_through: 0,
                margin_permille: 250,
                min_dwell: Micros::from_secs(1),
                horizon: Micros::from_secs(1),
                restore: RESTORE,
                ref_bytes: Bytes(256 << 10),
                speculation: Speculation {
                    slots_permille: 250,
                    memory_permille: 200,
                },
                blend: Micros::from_secs(2),
                anticipation: ANTICIPATION,
            },
        }
    }

    /// A Glide flight's or a tape replay's: the path's next use weighs
    /// twice, over eight seconds.
    pub fn known_path() -> Self {
        let mut s = Specification::interactive();
        s.kind = SpecKind::KnownPath;
        s.set_weight(Term::NextUse, 2_000);
        s.params.horizon = Micros::from_secs(8);
        s.params.speculation = Speculation {
            slots_permille: 500,
            memory_permille: 300,
        };
        s
    }

    /// A film's: Bélády weighted by restore cost — the exact next use, the
    /// restore cost for ties, no speculation, no margin. Recency stands in
    /// for the next use until the film's schedule is given as predictions
    /// (PR 8 of the plan): what the last frames needed is what the next
    /// ones need most likely.
    pub fn film() -> Self {
        Specification {
            kind: SpecKind::Film,
            rules: vec![
                Rule::always(Term::Recency, 1_000),
                Rule::always(Term::RestoreCost, 1_000),
                Rule::always(Term::NextUse, 4_000),
            ],
            params: Params {
                // The coarse band is a parameter for the search (PR 6): by
                // default only the floor is a band of its own — measured on
                // the five-minute soak, a band through level 5 lengthened
                // the mesh waits' p95 (law 2 per place is kept by the
                // fallback chain's protection and the finer-first ties).
                coarse_through: 0,
                margin_permille: 0,
                min_dwell: Micros::ZERO,
                horizon: Micros::from_secs(10),
                restore: RESTORE,
                ref_bytes: Bytes(256 << 10),
                speculation: Speculation {
                    slots_permille: 0,
                    memory_permille: 0,
                },
                blend: Micros::ZERO,
                anticipation: ANTICIPATION,
            },
        }
    }

    /// First-person-view flight, interactive (spec §3.7): what lies along
    /// the predicted forward path weighs most, the near field and its drapes
    /// are favoured, what left the corridor loses value fast, speculation
    /// gets more room.
    pub fn fpv() -> Self {
        let near = Predicate::All(vec![
            Predicate::Membership(Membership::Drawn),
            Predicate::Band(Band::Fine),
        ]);
        Specification {
            kind: SpecKind::Fpv,
            rules: vec![
                Rule {
                    when: !Predicate::Predicted,
                    term: Term::Recency,
                    weight: 250,
                },
                Rule {
                    when: Predicate::Predicted,
                    term: Term::Recency,
                    weight: 1_000,
                },
                Rule::always(Term::RestoreCost, 1_000),
                Rule::always(Term::NextUse, 3_000),
                Rule {
                    when: near,
                    term: Term::Deficit,
                    weight: 2_000,
                },
                Rule::always(Term::ScreenSize, 2_000),
            ],
            params: Params {
                // The coarse band is a parameter for the search (PR 6): by
                // default only the floor is a band of its own — measured on
                // the five-minute soak, a band through level 5 lengthened
                // the mesh waits' p95 (law 2 per place is kept by the
                // fallback chain's protection and the finer-first ties).
                coarse_through: 0,
                margin_permille: 150,
                min_dwell: Micros::from_millis(500),
                horizon: Micros::from_secs(2),
                restore: RESTORE,
                ref_bytes: Bytes(256 << 10),
                speculation: Speculation {
                    slots_permille: 450,
                    memory_permille: 350,
                },
                blend: Micros::from_secs(1),
                anticipation: Anticipation {
                    ring_permille: 700,
                    ancestor_permille: 250,
                },
            },
        }
    }

    /// First-person-view flight in a film: the plan's exact schedule,
    /// nothing speculative.
    pub fn fpv_film() -> Self {
        let mut s = Specification::film();
        s.kind = SpecKind::FpvFilm;
        s.rules.push(Rule::always(Term::ScreenSize, 1_000));
        s
    }

    /// The named default of `kind` (`Custom` starts from the interactive
    /// one).
    pub fn of_kind(kind: SpecKind) -> Self {
        match kind {
            SpecKind::Interactive => Specification::interactive(),
            SpecKind::KnownPath => Specification::known_path(),
            SpecKind::Film => Specification::film(),
            SpecKind::Fpv => Specification::fpv(),
            SpecKind::FpvFilm => Specification::fpv_film(),
            SpecKind::Custom => {
                let mut s = Specification::interactive();
                s.kind = SpecKind::Custom;
                s
            }
        }
    }

    /// The specification a budget of `policy` starts on.
    pub fn for_policy(policy: Policy) -> Self {
        match policy {
            Policy::Interactive => Specification::interactive(),
            Policy::Batch => Specification::film(),
        }
    }

    /// The summed weight, ‰, of the rules of `term` whose predicate `f`
    /// satisfies.
    pub fn weight(&self, term: Term, f: &Facts) -> u32 {
        self.rules
            .iter()
            .filter(|r| r.term == term && r.when.holds(f))
            .fold(0u32, |a, r| a.saturating_add(r.weight))
    }

    /// Sets every rule of `term` to `weight`‰ (adds one for every resource
    /// if it has none).
    pub fn set_weight(&mut self, term: Term, weight: u32) {
        let mut found = false;
        for r in self.rules.iter_mut().filter(|r| r.term == term) {
            r.weight = weight;
            found = true;
        }
        if !found {
            self.rules.push(Rule::always(term, weight));
        }
    }

    /// The band of a resource of `class` at `level`, under the floor
    /// `pinned`.
    pub fn band(&self, class: Class, level: u32, pinned: Option<u32>) -> Band {
        if class.is_ground() && pinned.is_some_and(|p| level <= p) {
            Band::Floor
        } else if level <= self.params.coarse_through {
            Band::Coarse
        } else {
            Band::Fine
        }
    }

    /// Checks every number is within its bounds.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.rules.len() > MAX_RULES || self.rules.iter().any(|r| r.when.depth() > MAX_DEPTH) {
            return Err(SpecError::TooComplex);
        }
        if self.rules.iter().any(|r| r.weight > MAX_WEIGHT) {
            return Err(SpecError::OutOfBounds("weight"));
        }
        let p = &self.params;
        let bounded = [
            ("margin", u64::from(p.margin_permille), 4_000),
            ("min_dwell_ms", p.min_dwell.0 / 1_000, 30_000),
            ("horizon_ms", p.horizon.0 / 1_000, 60_000),
            ("blend_ms", p.blend.0 / 1_000, 30_000),
            (
                "speculation_slots",
                u64::from(p.speculation.slots_permille),
                1_000,
            ),
            (
                "speculation_memory",
                u64::from(p.speculation.memory_permille),
                1_000,
            ),
            ("coarse_through", u64::from(p.coarse_through), 30),
            ("restore_remote_ms", p.restore.remote.0 / 1_000, 60_000),
            ("p_ring", u64::from(p.anticipation.ring_permille), 1_000),
            ("p_ancestor", u64::from(p.anticipation.ancestor_permille), 1_000),
        ];
        if let Some((name, ..)) = bounded.iter().find(|(_, v, max)| v > max) {
            return Err(SpecError::OutOfBounds(name));
        }
        if p.ref_bytes == Bytes::ZERO {
            return Err(SpecError::OutOfBounds("ref_bytes"));
        }
        Ok(())
    }

    /// A digest of the whole specification, FNV-1a over its canonical text:
    /// the same on every target.
    pub fn digest(&self) -> u64 {
        let text = format!("{self:?}");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    /// The query string a steering URL (`tuile://budget?…`) or the control
    /// API carries: the kind, then every parameter and every term's weight.
    /// A term whose rules weigh differently by predicate is left out: it
    /// keeps its kind's rules. [`Specification::from_query`] reads it back
    /// to an equal specification when the rules are the kind's own.
    pub fn to_query(&self) -> String {
        let p = &self.params;
        let ms = |m: Micros| m.0 / 1_000;
        let mut q = format!(
            "spec={}&margin={}&min_dwell_ms={}&horizon_ms={}&blend_ms={}\
             &speculation_slots={}&speculation_memory={}&coarse_through={}\
             &restore_decoded_ms={}&restore_stored_ms={}&restore_http_cache_ms={}\
             &restore_remote_ms={}&p_ring={}&p_ancestor={}",
            self.kind.label(),
            p.margin_permille,
            ms(p.min_dwell),
            ms(p.horizon),
            ms(p.blend),
            p.speculation.slots_permille,
            p.speculation.memory_permille,
            p.coarse_through,
            ms(p.restore.decoded),
            ms(p.restore.stored),
            ms(p.restore.http_cache),
            ms(p.restore.remote),
            p.anticipation.ring_permille,
            p.anticipation.ancestor_permille,
        );
        // A term's weight when all its rules share one; a term weighed
        // differently by predicate (FPV's recency) keeps its kind's rules.
        for t in Term::ALL {
            let mut of = self.rules.iter().filter(|r| r.term == t).map(|r| r.weight);
            if let Some(w) = of.next() {
                if of.all(|x| x == w) {
                    let _ = write!(q, "&w_{}={w}", term_key(t));
                }
            }
        }
        q
    }

    /// A specification from a query: `spec=<kind>` (default `interactive`)
    /// then any of [`Specification::to_query`]'s keys as overrides. A
    /// `w_<term>` override sets every rule of that term. Validated.
    pub fn from_query(query: &str) -> Result<Self, SpecError> {
        let pairs: Vec<(&str, &str)> = query
            .trim_start_matches('?')
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| kv.split_once('=').unwrap_or((kv, "")))
            .collect();
        let kind = match pairs.iter().find(|(k, _)| *k == "spec") {
            Some((_, v)) => {
                SpecKind::from_label(v).ok_or_else(|| SpecError::UnknownKind((*v).to_owned()))?
            }
            None => SpecKind::Interactive,
        };
        let mut s = Specification::of_kind(kind);
        for (k, v) in pairs.iter().filter(|(k, _)| *k != "spec") {
            let n: u64 = v
                .parse()
                .map_err(|_| SpecError::NotANumber((*k).to_owned()))?;
            let n32 = u32::try_from(n).map_err(|_| SpecError::OutOfBounds("value"))?;
            let n16 = u16::try_from(n).map_err(|_| SpecError::OutOfBounds("value"))?;
            let millis = Micros::from_millis(n);
            let p = &mut s.params;
            match *k {
                "margin" => p.margin_permille = n32,
                "min_dwell_ms" => p.min_dwell = millis,
                "horizon_ms" => p.horizon = millis,
                "blend_ms" => p.blend = millis,
                "speculation_slots" => p.speculation.slots_permille = n16,
                "speculation_memory" => p.speculation.memory_permille = n16,
                "coarse_through" => p.coarse_through = n32,
                "restore_decoded_ms" => p.restore.decoded = millis,
                "restore_stored_ms" => p.restore.stored = millis,
                "restore_http_cache_ms" => p.restore.http_cache = millis,
                "restore_remote_ms" => p.restore.remote = millis,
                "p_ring" => p.anticipation.ring_permille = n16,
                "p_ancestor" => p.anticipation.ancestor_permille = n16,
                _ => match k
                    .strip_prefix("w_")
                    .and_then(|t| Term::ALL.into_iter().find(|x| term_key(*x) == t))
                {
                    Some(t) => s.set_weight(t, n32),
                    None => return Err(SpecError::UnknownKey((*k).to_owned())),
                },
            }
        }
        s.validate()?;
        Ok(s)
    }
}

impl Default for Specification {
    fn default() -> Self {
        Specification::interactive()
    }
}

/// A term's key in a query (`w_<key>`).
fn term_key(t: Term) -> &'static str {
    match t {
        Term::Recency => "recency",
        Term::RestoreCost => "restore",
        Term::NextUse => "next_use",
        Term::Deficit => "deficit",
        Term::ScreenSize => "screen",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn facts(level: u32, predicted: bool) -> Facts {
        Facts {
            class: Class::TerrainMesh,
            level,
            tier: Tier::Remote,
            band: Band::Fine,
            membership: Some(Membership::Drawn),
            predicted,
        }
    }

    #[test]
    fn every_default_is_valid_and_round_trips_through_its_query() {
        for k in SpecKind::ALL {
            let s = Specification::of_kind(k);
            s.validate().unwrap();
            let back = Specification::from_query(&s.to_query()).unwrap();
            assert_eq!(back, s, "{}", k.label());
            assert_eq!(back.digest(), s.digest());
        }
    }

    #[test]
    fn a_query_overrides_the_named_default() {
        let s = Specification::from_query("spec=fpv&margin=300&horizon_ms=3000&w_screen=1500")
            .unwrap();
        assert_eq!(s.kind, SpecKind::Fpv);
        assert_eq!(s.params.margin_permille, 300);
        assert_eq!(s.params.horizon, Micros::from_secs(3));
        assert_eq!(s.weight(Term::ScreenSize, &facts(10, false)), 1_500);
        assert_ne!(s.digest(), Specification::fpv().digest());
    }

    #[test]
    fn a_bad_query_is_refused() {
        let e = |q| Specification::from_query(q).unwrap_err();
        assert!(matches!(e("spec=warp"), SpecError::UnknownKind(_)));
        assert!(matches!(e("margin=lots"), SpecError::NotANumber(_)));
        assert!(matches!(e("speed=3"), SpecError::UnknownKey(_)));
        assert!(matches!(e("margin=999999"), SpecError::OutOfBounds(_)));
        assert!(matches!(e("w_recency=99999"), SpecError::OutOfBounds(_)));
    }

    #[test]
    fn predicates_compose() {
        let p = Predicate::All(vec![
            Predicate::LevelAtLeast(8),
            Predicate::Any(vec![Predicate::Predicted, Predicate::Class(Class::Drape)]),
        ]);
        assert!(p.holds(&facts(9, true)));
        assert!(!p.holds(&facts(9, false)));
        assert!(!p.holds(&facts(7, true)));
        assert!((!p.clone()).holds(&facts(7, true)));
        assert!(Predicate::All(vec![]).holds(&facts(0, false)));
        assert!(!Predicate::Any(vec![]).holds(&facts(0, false)));
    }

    #[test]
    fn fpv_sinks_what_left_the_corridor() {
        let s = Specification::fpv();
        let ahead = s.weight(Term::Recency, &facts(12, true));
        let behind = s.weight(Term::Recency, &facts(12, false));
        assert!(ahead > behind, "{ahead} {behind}");
        assert_eq!(s.weight(Term::Deficit, &facts(12, false)), 2_000);
    }

    #[test]
    fn the_bands_put_the_floor_and_the_coarse_first() {
        let mut s = Specification::interactive();
        s.params.coarse_through = 5;
        assert_eq!(s.band(Class::TerrainMesh, 2, Some(2)), Band::Floor);
        assert_eq!(s.band(Class::Tiles3dMesh, 2, Some(2)), Band::Coarse);
        assert_eq!(s.band(Class::Drape, 5, Some(2)), Band::Coarse);
        assert_eq!(s.band(Class::Drape, 6, Some(2)), Band::Fine);
        assert!(Band::Floor < Band::Coarse && Band::Coarse < Band::Fine);
    }

    #[test]
    fn too_deep_a_predicate_is_refused() {
        let mut p = Predicate::Always;
        for _ in 0..MAX_DEPTH {
            p = !p;
        }
        let mut s = Specification::interactive();
        s.rules.push(Rule {
            when: p,
            term: Term::Recency,
            weight: 1,
        });
        assert_eq!(s.validate(), Err(SpecError::TooComplex));
    }
}
