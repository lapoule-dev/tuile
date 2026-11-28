// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The preload envelope (`docs/explorations/budget-regulator.md`, E1–E3):
//! a preload lives only in what the present need leaves unused, its
//! reserve counted. One mechanism, for every resource a preload takes:
//!
//! - **E1.** For each resource — a pool class's GPU bytes, the decoded
//!   bytes, a class's network slots — `present + preloaded + reserve ≤
//!   capacity` at every step.
//! - **E2.** The reserve is what the present need has asked for and not yet
//!   received: its loads queued and in flight, its copies decoded and not
//!   delivered — Little's `L`, the work in the system, counted directly
//!   rather than estimated as `λ·W`. It rises at once and falls by a
//!   fraction a step, so it is not handed back to the preloads between two
//!   bursts of one gesture. Nothing at rest: a converged view asks nothing.
//! - **E3.** A preload enters only within the envelope (started, delivered),
//!   and leaves it first when it tightens (the engine's release).
//!
//! This module holds the filter and the arithmetic, pure and in integers;
//! the engine applies them. Its signals are the decider's own state (its
//! queue, its loads, its decoded copies): functions of the recorded inputs,
//! never a measurement of the executor.

#![deny(clippy::float_arithmetic)]

use super::types::{Bytes, Class, PerClass};

/// The envelope's numbers (a part of the specification's `Params`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EnvelopeParams {
    /// How far the reserve falls towards the present need's pending bytes
    /// in one step, ‰ (it rises to them at once). 1000 follows them
    /// exactly.
    pub fall_permille: u16,
}

/// The default: a tenth a step, a half-life of about seven steps.
pub const ENVELOPE: EnvelopeParams = EnvelopeParams { fall_permille: 100 };

/// The reserve after one step: up to `pending` at once, down towards it by
/// `fall_permille` of the gap (at least a byte, so it reaches it).
fn filter(x: u64, pending: u64, p: &EnvelopeParams) -> u64 {
    if pending >= x {
        return pending;
    }
    let fall = ((x - pending) * u64::from(p.fall_permille.min(1_000)) / 1_000).max(1);
    x - fall.min(x - pending)
}

/// The present need's reserve, per class, and the sizes a load is estimated
/// at before it is loaded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Envelope {
    /// The bytes reserved for the present need.
    reserve: PerClass<u64>,
    /// The mean size of a delivered copy, per class (0: none seen yet).
    mean: PerClass<u64>,
}

impl Envelope {
    /// Reads one step's present need: the bytes it has pending per class.
    pub fn observe(&mut self, p: &EnvelopeParams, pending: PerClass<Bytes>) {
        for c in Class::ALL {
            self.reserve[c] = filter(self.reserve[c], pending[c].0, p);
        }
    }

    /// Learns the size of a delivered copy of `class`: an eighth of the way
    /// from the mean (the first sets it).
    pub fn learn(&mut self, class: Class, bytes: Bytes) {
        let m = self.mean[class];
        self.mean[class] = if m == 0 {
            bytes.0
        } else if bytes.0 >= m {
            m + (bytes.0 - m) / 8
        } else {
            m - (m - bytes.0) / 8
        };
    }

    /// The bytes reserved for the present need in `class`.
    pub fn reserve(&self, class: Class) -> Bytes {
        Bytes(self.reserve[class])
    }

    /// A load's bytes before it is loaded: the class's mean delivered size,
    /// `fallback` before any was seen.
    pub fn estimate(&self, class: Class, fallback: Bytes) -> Bytes {
        match self.mean[class] {
            0 => fallback,
            m => Bytes(m),
        }
    }
}

/// Whether `bytes` more fit the envelope of one resource: `used` (present,
/// preloads and what is pending) plus `bytes` within `capacity − reserve`.
pub fn fits(used: Bytes, bytes: Bytes, capacity: Bytes, reserve: Bytes) -> bool {
    used.plus(bytes) <= capacity.minus(reserve)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(env: &mut Envelope, p: &EnvelopeParams, bytes: u64, n: usize) {
        let mut d = PerClass::splat(Bytes::ZERO);
        d[Class::Drape] = Bytes(bytes);
        for _ in 0..n {
            env.observe(p, d);
        }
    }

    #[test]
    fn nothing_is_reserved_at_rest() {
        let mut env = Envelope::default();
        steps(&mut env, &ENVELOPE, 0, 3);
        for c in Class::ALL {
            assert_eq!(env.reserve(c), Bytes::ZERO);
        }
    }

    /// Up at once: the step the present need asks, its bytes are reserved.
    #[test]
    fn the_reserve_rises_to_the_pending_bytes_at_once() {
        let mut env = Envelope::default();
        steps(&mut env, &ENVELOPE, 1_000_000, 1);
        assert_eq!(env.reserve(Class::Drape), Bytes(1_000_000));
        assert_eq!(env.reserve(Class::TerrainMesh), Bytes::ZERO);
    }

    /// Down slowly: ten steps after the need is served, about a third is
    /// still reserved; it reaches zero.
    #[test]
    fn the_reserve_falls_slowly_to_zero() {
        let mut env = Envelope::default();
        steps(&mut env, &ENVELOPE, 1_000_000, 1);
        steps(&mut env, &ENVELOPE, 0, 10);
        let after = env.reserve(Class::Drape).0;
        assert!((340_000..=350_000).contains(&after), "{after}");
        steps(&mut env, &ENVELOPE, 0, 400);
        assert_eq!(env.reserve(Class::Drape), Bytes::ZERO);
    }

    #[test]
    fn a_full_fall_follows_the_pending_bytes() {
        let p = EnvelopeParams {
            fall_permille: 1_000,
        };
        let mut env = Envelope::default();
        steps(&mut env, &p, 1_000_000, 1);
        steps(&mut env, &p, 10, 1);
        assert_eq!(env.reserve(Class::Drape), Bytes(10));
    }

    #[test]
    fn the_estimate_learns_the_mean_size() {
        let mut env = Envelope::default();
        assert_eq!(env.estimate(Class::Drape, Bytes(7)), Bytes(7));
        env.learn(Class::Drape, Bytes(800));
        assert_eq!(env.estimate(Class::Drape, Bytes(7)), Bytes(800));
        env.learn(Class::Drape, Bytes(0));
        assert_eq!(env.estimate(Class::Drape, Bytes(7)), Bytes(700));
    }

    #[test]
    fn fits_counts_the_reserve() {
        assert!(fits(Bytes(50), Bytes(10), Bytes(100), Bytes(40)));
        assert!(!fits(Bytes(51), Bytes(10), Bytes(100), Bytes(40)));
        assert!(!fits(Bytes(0), Bytes(1), Bytes(100), Bytes(200)));
    }
}
