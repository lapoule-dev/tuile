// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The step inputs the host measures or predicts
//! (`docs/explorations/budget-eviction-prefetch-spec.md` §4, §5): the
//! device's memory and the cost of each tier ([`Measured`]), and when each
//! tile will be used next ([`Prediction`]).
//!
//! They enter a step only as fields of its [`FrameInput`](crate::FrameInput)
//! and are recorded with it ([`StepRecord`](crate::StepRecord)), so a replay
//! feeds the same numbers back and decides the same (law 5). Nothing here is
//! measured inside the budget; the smoothing of the raw samples is the
//! budget's own, deterministic over them ([`Smoothed`]).

use super::types::{Bytes, Micros, TileId};

/// One measurement interval's samples of one tier's access cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TierSample {
    /// The median latency of the interval's fetches, µs (0: no sample).
    pub latency_us: u32,
    /// The throughput over the interval, bytes per millisecond.
    pub bytes_per_ms: u32,
    /// Failed fetches per thousand attempted.
    pub failures_permille: u16,
    /// How many fetches the interval saw (0: no sample).
    pub samples: u16,
}

/// What the host measured, once per measurement interval; the latest
/// interval's numbers are given with every step until the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Measured {
    /// Which interval the samples are of: the smoothing takes a sample in
    /// once, when this changes.
    pub interval: u64,
    /// GPU bytes the device reports allocated by the whole process.
    pub gpu_allocated: Bytes,
    /// The device's GPU limit for the process (0: unknown).
    pub gpu_limit: Bytes,
    /// System memory available to the process (0: unknown).
    pub ram_available: Bytes,
    /// The local store (native).
    pub store: TierSample,
    /// The browser's HTTP cache (web).
    pub http_cache: TierSample,
    /// The network.
    pub network: TierSample,
}

/// When a tile is expected to be used next, and how likely: the need says
/// what (its level, parent, imagery), a prediction says when.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Prediction {
    /// The tile; to be loadable when not held, it is listed in the need
    /// (its ring or ancestors).
    pub tile: TileId,
    /// The expected delay to its next use.
    pub next_use_in: Micros,
    /// Thousandths.
    pub probability: u16,
}

/// A fixed-point exponential moving average, in integers: `α = dt / (τ +
/// dt)` in Q16, applied once per new sample. Deterministic on every target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ema {
    /// The value, Q16.
    value: u64,
    primed: bool,
}

impl Ema {
    /// Takes in `sample`, `dt` after the last one, with time constant `tau`.
    pub fn take(&mut self, sample: u64, dt: Micros, tau: Micros) {
        let s = sample.saturating_mul(1 << 16);
        if !self.primed {
            self.value = s;
            self.primed = true;
            return;
        }
        let den = u128::from(tau.0) + u128::from(dt.0);
        let alpha = (u128::from(dt.0) * (1 << 16))
            .checked_div(den)
            .unwrap_or(1 << 16);
        let v = u128::from(self.value);
        let s = u128::from(s);
        let next = if s >= v {
            v + (s - v) * alpha / (1 << 16)
        } else {
            v - (v - s) * alpha / (1 << 16)
        };
        self.value = u64::try_from(next).unwrap_or(u64::MAX);
    }

    /// The smoothed value, if any sample was taken.
    pub fn get(&self) -> Option<u64> {
        self.primed.then_some(self.value >> 16)
    }
}

/// The time constants of the smoothing (spec §4.2).
pub const TAU_MEMORY: Micros = Micros::from_secs(4);
/// Latency and throughput.
pub const TAU_COST: Micros = Micros::from_secs(8);
/// Failures rising.
pub const TAU_FAIL_UP: Micros = Micros::from_secs(2);
/// Failures falling.
pub const TAU_FAIL_DOWN: Micros = Micros::from_secs(16);

/// One tier's smoothed cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SmoothedTier {
    /// Latency, µs.
    pub latency_us: Ema,
    /// Throughput, bytes per ms.
    pub bytes_per_ms: Ema,
    /// Failures, ‰.
    pub failures_permille: Ema,
}

impl SmoothedTier {
    fn take(&mut self, s: &TierSample, dt: Micros) {
        if s.samples == 0 {
            return;
        }
        self.latency_us
            .take(u64::from(s.latency_us), dt, TAU_COST);
        self.bytes_per_ms
            .take(u64::from(s.bytes_per_ms), dt, TAU_COST);
        let f = u64::from(s.failures_permille);
        let tau = match self.failures_permille.get() {
            Some(v) if f < v => TAU_FAIL_DOWN,
            _ => TAU_FAIL_UP,
        };
        self.failures_permille.take(f, dt, tau);
    }

    /// The cost of restoring `bytes` from this tier, µs, if it has
    /// samples: latency plus transfer, a failure costing a retry.
    pub fn cost(&self, bytes: Bytes) -> Option<Micros> {
        let lat = self.latency_us.get()?;
        let transfer = self
            .bytes_per_ms
            .get()
            .filter(|t| *t > 0)
            .map_or(0, |t| bytes.0.saturating_mul(1_000) / t);
        let fail = self.failures_permille.get().unwrap_or(0).min(900);
        let base = lat.saturating_add(transfer);
        Some(Micros(base.saturating_mul(1_000) / (1_000 - fail)))
    }
}

/// The measured inputs as the budget smooths them, from the recorded
/// samples only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Smoothed {
    last: Option<(u64, Micros)>,
    /// GPU allocated by the process.
    pub gpu_allocated: Ema,
    /// The device's GPU limit.
    pub gpu_limit: Ema,
    /// Available system memory.
    pub ram_available: Ema,
    /// The local store.
    pub store: SmoothedTier,
    /// The browser's HTTP cache.
    pub http_cache: SmoothedTier,
    /// The network.
    pub network: SmoothedTier,
}

impl Smoothed {
    /// Takes in `m` at `now`, once per interval.
    pub fn take(&mut self, m: &Measured, now: Micros) {
        if self.last.is_some_and(|(i, _)| i == m.interval) {
            return;
        }
        let dt = self.last.map_or(Micros::ZERO, |(_, t)| now.since(t));
        self.last = Some((m.interval, now));
        for (e, v) in [
            (&mut self.gpu_allocated, m.gpu_allocated),
            (&mut self.gpu_limit, m.gpu_limit),
            (&mut self.ram_available, m.ram_available),
        ] {
            if v != Bytes::ZERO {
                e.take(v.0, dt, TAU_MEMORY);
            }
        }
        self.store.take(&m.store, dt);
        self.http_cache.take(&m.http_cache, dt);
        self.network.take(&m.network, dt);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_average_follows_a_step_with_its_time_constant() {
        let mut e = Ema::default();
        assert_eq!(e.get(), None);
        e.take(1_000, Micros::ZERO, TAU_COST);
        assert_eq!(e.get(), Some(1_000));
        // One τ in one sample: half way.
        e.take(3_000, TAU_COST, TAU_COST);
        assert_eq!(e.get(), Some(2_000));
        // Noise of one second barely moves it.
        let before = e.get().unwrap();
        e.take(10_000, Micros::from_secs(1), TAU_COST);
        let moved = e.get().unwrap() - before;
        assert!(moved < 1_000, "{moved}");
    }

    #[test]
    fn a_sample_is_taken_once_per_interval() {
        let mut s = Smoothed::default();
        let mut m = Measured {
            interval: 1,
            network: TierSample {
                latency_us: 100_000,
                bytes_per_ms: 2_000,
                failures_permille: 0,
                samples: 10,
            },
            ..Measured::default()
        };
        s.take(&m, Micros::ZERO);
        let once = s;
        s.take(&m, Micros::from_millis(16));
        assert_eq!(s, once, "the same interval is not taken twice");
        m.interval = 2;
        m.network.latency_us = 300_000;
        s.take(&m, Micros::from_secs(1));
        assert!(s.network.latency_us.get().unwrap() > 100_000);
    }

    #[test]
    fn failures_rise_fast_and_fall_slowly() {
        let mut t = SmoothedTier::default();
        let at = |f| TierSample {
            latency_us: 1,
            bytes_per_ms: 1,
            failures_permille: f,
            samples: 1,
        };
        t.take(&at(0), Micros::ZERO);
        t.take(&at(500), Micros::from_secs(1));
        let up = t.failures_permille.get().unwrap();
        t.take(&at(0), Micros::from_secs(1));
        let down = up - t.failures_permille.get().unwrap();
        assert!(up > 100 && down < up / 4, "{up} {down}");
    }

    #[test]
    fn a_tiers_cost_is_latency_transfer_and_retries() {
        let mut t = SmoothedTier::default();
        assert_eq!(t.cost(Bytes(1_000)), None);
        t.take(
            &TierSample {
                latency_us: 50_000,
                bytes_per_ms: 1_000,
                failures_permille: 500,
                samples: 4,
            },
            Micros::ZERO,
        );
        // 50 ms + 256 KiB at 1 MB/s (262 ms) = 312 ms, doubled by a 50 % failure.
        assert_eq!(t.cost(Bytes(256 << 10)), Some(Micros(624_288)));
    }
}
