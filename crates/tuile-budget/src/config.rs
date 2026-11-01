// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`BudgetConfig`]: every number the budget runs on, validated once.

use super::error::ConfigError;
use super::types::{Bytes, Class, Micros, PerClass, Policy};

/// What a failed load does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailurePolicy {
    /// Batch: a film never ships a hole, so the first failure ends the session.
    EndSession,
    /// Interactive (decision D1): retry after `first`, doubling up to `max`.
    /// The fallback stays drawn meanwhile, and the failed load holds no slot.
    RetryBackoff {
        /// The first delay.
        first: Micros,
        /// The ceiling of the doubling.
        max: Micros,
    },
}

/// Time-based LOD relaxation (`sse_scale`), in thousandths per second, so it
/// behaves the same at 30 and at 120 frames per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Relaxation {
    /// Rise of the scale per second while the protected set exceeds
    /// `target + overflow` (thousandths: 1000 = +1.0 per second).
    pub up_per_second: u32,
    /// Decay per second while every class is under its low watermark.
    pub down_per_second: u32,
    /// The ceiling of the scale (thousandths: 8000 = ×8).
    pub max_scale: u32,
}

/// Eviction hysteresis, in percent of the target: a sweep starts above `high`
/// and stops at `low`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Watermarks {
    /// Percent of the target above which a sweep starts.
    pub high_percent: u8,
    /// Percent of the target a sweep trims down to.
    pub low_percent: u8,
}

/// The fractions of the configured targets kept under each [`Pressure`](super::Pressure)
/// level, in percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PressureFractions {
    /// Under a memory warning.
    pub warning_percent: u8,
    /// Under a critical warning.
    pub critical_percent: u8,
    /// In the background.
    pub background_percent: u8,
}

/// The pooled ledger (D-A3, decided 2026-10-20): classes that share one pool
/// of bytes, so that the split between them follows the view instead of a
/// fixed ratio. The members' bytes together are held to the pool's target,
/// overflow and hard limit; each member between a floor (a percent of the
/// pool) and a ceiling (its own `target`, `overflow` and `hard_limit`). A
/// member's effective target is what the pool leaves once the other
/// members' bytes are counted, clamped to its floor and its ceiling; one
/// eviction sweep runs over the members in the one eviction order; the
/// relaxation reads the pool's protected bytes. Classes outside the pool
/// keep their own targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pool {
    /// The classes that share the pool.
    pub members: PerClass<bool>,
    /// The members' bytes together are trimmed to this.
    pub target: Bytes,
    /// Visible-only headroom above the pool's target.
    pub overflow: Bytes,
    /// The members' bytes together never cross this.
    pub hard_limit: Bytes,
    /// Each member's floor, percent of the pool's target and hard limit.
    pub floor_percent: PerClass<u8>,
}

impl Pool {
    /// No pool: every class keeps its own target.
    pub const NONE: Pool = Pool {
        members: PerClass([false; 4]),
        target: Bytes::ZERO,
        overflow: Bytes::ZERO,
        hard_limit: Bytes::ZERO,
        floor_percent: PerClass([0; 4]),
    };

    /// Whether `c` shares the pool.
    pub fn holds(&self, c: Class) -> bool {
        self.members[c]
    }
}

/// Every number the budget runs on.
///
/// Built from [`BudgetConfig::interactive`] or [`BudgetConfig::batch`], then
/// adjusted field by field; [`Budget::new`](super::Budget::new) validates it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetConfig {
    /// Interactive or Batch.
    pub policy: Policy,
    /// GPU bytes each class is trimmed down to. Batch: [`Bytes::MAX`].
    pub target: PerClass<Bytes>,
    /// Visible-only headroom above the target, per class. Batch: unused.
    pub overflow: PerClass<Bytes>,
    /// The ceiling no delivery ever crosses, per class, whatever its
    /// urgency: the guard against a jetsam or out-of-memory kill. Set by the
    /// profile from the device. Batch: [`Bytes::MAX`].
    pub hard_limit: PerClass<Bytes>,
    /// The part of `hard_limit`, in percent, kept for swaps that shrink the
    /// set: a coarser parent replacing its drawn children. Other work stops
    /// below it, so a zoom-out at the ceiling can always complete.
    pub hard_reserve_percent: u8,
    /// Decoded CPU bytes kept off the GPU (the second tier), all classes.
    /// A target like the GPU's: retargeted with them
    /// ([`Budget::retarget`](super::Budget::retarget)) and scaled by
    /// pressure. Batch: [`Bytes::MAX`].
    pub decoded_target: Bytes,
    /// Network loads in flight per class.
    pub max_in_flight: PerClass<u32>,
    /// Local-store reads in flight, all classes: a separate quota, so local
    /// reads never wait behind network work.
    pub max_local_reads: u32,
    /// The bounded wish list actually scheduled in one step.
    pub request_heap: u32,
    /// GPU upload bytes per frame. Batch: [`Bytes::MAX`].
    pub upload_bytes_per_frame: Bytes,
    /// Decoded bytes in transit to the consumer, not yet acknowledged.
    pub delivery_credit: Bytes,
    /// How long a load may stay unneeded before it is cancelled.
    pub cancel_grace: Micros,
    /// A load unanswered this long is cancelled and handled as a transient
    /// failure, so a stalled host never holds a slot for ever. Batch: the
    /// failure ends the session.
    ///
    /// A tuning value, the host's to set: it must cover the whole retry
    /// envelope of the client the loads go through (every attempt and
    /// backoff of one tile), or a load the client would still have saved is
    /// cut short. `batch` and `interactive` start from a default.
    pub load_timeout: Micros,
    /// LOD relaxation under overflow.
    pub relax: Relaxation,
    /// Eviction hysteresis.
    pub watermarks: Watermarks,
    /// Target fractions under memory pressure.
    pub pressure: PressureFractions,
    /// The coarse floor (terrain and imagery): levels `0..=pinned_level` are
    /// requested first and never evicted.
    pub pinned_level: Option<u32>,
    /// The most imagery levels a drape may sit above its tile's terrain level
    /// (a profile parameter: desktop 3, handheld 2, low handheld 1).
    pub imagery_over_terrain_cap: u32,
    /// The largest drape texture side, texels (desktop 2048, handheld 1024):
    /// beyond it the drape is composed at the deepest level that fits.
    pub max_drape_side: u32,
    /// The side of one imagery tile, texels.
    pub imagery_tile_px: u32,
    /// What a failed load does.
    pub failure: FailurePolicy,
    /// A load that has waited this long gets the full age bonus.
    pub age_full: Micros,
    /// The pooled ledger (D-A3): the ground's terrain and imagery, in a
    /// viewer.
    pub pool: Pool,
}

impl BudgetConfig {
    /// A viewer's budget for `gpu` bytes in total.
    ///
    /// The ground — terrain and imagery — is one pool of 82 % of the device
    /// (D-A3, decided 2026-10-20): terrain between 25 % and 60 % of it,
    /// imagery between 40 % and 75 %, the split following the view. 3D Tiles
    /// keep 15 % and overlays 3 % of their own. Every other number is a
    /// first proposal (decision D5), tuned per profile from simulated curves.
    pub fn interactive(gpu: Bytes) -> Self {
        let share = |pct: u64| gpu.scaled(pct, 100);
        let ground = share(82);
        let mut members = PerClass::splat(false);
        members[Class::TerrainMesh] = true;
        members[Class::Drape] = true;
        let mut floor_percent = PerClass::splat(0u8);
        floor_percent[Class::TerrainMesh] = 25;
        floor_percent[Class::Drape] = 40;
        let pool = Pool {
            members,
            target: ground.scaled(70, 100),
            overflow: ground.scaled(15, 100),
            hard_limit: ground,
            floor_percent,
        };
        // The members' own limits are their ceilings.
        let hard_limit = PerClass([
            ground.scaled(60, 100),
            share(15),
            ground.scaled(75, 100),
            share(3),
        ]);
        BudgetConfig {
            policy: Policy::Interactive,
            target: PerClass::from_fn(|c| hard_limit[c].scaled(70, 100)),
            overflow: PerClass::from_fn(|c| hard_limit[c].scaled(15, 100)),
            hard_limit,
            hard_reserve_percent: 15,
            decoded_target: gpu.scaled(1, 4),
            max_in_flight: PerClass([16, 8, 16, 4]),
            max_local_reads: 8,
            request_heap: 256,
            upload_bytes_per_frame: Bytes::mib(8),
            delivery_credit: Bytes::mib(32),
            cancel_grace: Micros::from_millis(500),
            load_timeout: Micros::from_secs(4),
            relax: Relaxation {
                up_per_second: 1_000,
                down_per_second: 250,
                max_scale: 8_000,
            },
            watermarks: Watermarks {
                high_percent: 100,
                low_percent: 90,
            },
            pressure: PressureFractions {
                warning_percent: 50,
                critical_percent: 25,
                background_percent: 0,
            },
            pinned_level: Some(2),
            imagery_over_terrain_cap: 3,
            max_drape_side: 2_048,
            imagery_tile_px: 256,
            failure: FailurePolicy::RetryBackoff {
                first: Micros::from_millis(500),
                max: Micros::from_secs(30),
            },
            age_full: Micros::from_secs(2),
            pool,
        }
    }

    /// A film's or a bake's budget: unbounded, exact, never cancelling.
    pub fn batch() -> Self {
        BudgetConfig {
            policy: Policy::Batch,
            target: PerClass::splat(Bytes::MAX),
            overflow: PerClass::splat(Bytes::ZERO),
            hard_limit: PerClass::splat(Bytes::MAX),
            hard_reserve_percent: 0,
            decoded_target: Bytes::MAX,
            max_in_flight: PerClass([64, 32, 64, 8]),
            max_local_reads: 32,
            request_heap: 4_096,
            upload_bytes_per_frame: Bytes::MAX,
            delivery_credit: Bytes::MAX,
            cancel_grace: Micros::NEVER,
            load_timeout: Micros::from_secs(120),
            relax: Relaxation {
                up_per_second: 0,
                down_per_second: 0,
                max_scale: 1_000,
            },
            watermarks: Watermarks {
                high_percent: 100,
                low_percent: 100,
            },
            pressure: PressureFractions {
                warning_percent: 100,
                critical_percent: 100,
                background_percent: 100,
            },
            pinned_level: Some(2),
            imagery_over_terrain_cap: 3,
            max_drape_side: 2_048,
            imagery_tile_px: 256,
            failure: FailurePolicy::EndSession,
            age_full: Micros::from_secs(2),
            pool: Pool::NONE,
        }
    }

    /// The most levels a drape may climb above its terrain: the configured
    /// cap, and no more than the largest drape side allows.
    pub fn imagery_cap(&self) -> u32 {
        let mut fit = 0;
        while fit < 16
            && u64::from(self.imagery_tile_px) << (fit + 1) <= u64::from(self.max_drape_side)
        {
            fit += 1;
        }
        self.imagery_over_terrain_cap.min(fit)
    }

    /// Checks the numbers are coherent.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let w = self.watermarks;
        if w.low_percent > w.high_percent || w.high_percent > 100 {
            return Err(ConfigError::Watermarks {
                low: w.low_percent,
                high: w.high_percent,
            });
        }
        if self.request_heap == 0 {
            return Err(ConfigError::ZeroRequestHeap);
        }
        for c in Class::ALL {
            if self.max_in_flight[c] == 0 {
                return Err(ConfigError::ZeroQuota(c));
            }
            if self.hard_limit[c] < self.target[c] {
                return Err(ConfigError::HardBelowTarget(c));
            }
        }
        if self.hard_reserve_percent > 50 {
            return Err(ConfigError::Reserve(self.hard_reserve_percent));
        }
        if self.imagery_tile_px == 0 || self.max_drape_side < self.imagery_tile_px {
            return Err(ConfigError::DrapeSide);
        }
        if self.relax.max_scale < 1_000 {
            return Err(ConfigError::RelaxCeiling(self.relax.max_scale));
        }
        if let FailurePolicy::RetryBackoff { first, max } = self.failure {
            if first > max || first == Micros::ZERO {
                return Err(ConfigError::Backoff);
            }
        }
        if self.policy == Policy::Batch && self.failure != FailurePolicy::EndSession {
            return Err(ConfigError::BatchMustEndOnFailure);
        }
        let p = &self.pool;
        if Class::ALL.iter().any(|c| p.members[*c]) {
            let floors: u32 = Class::ALL
                .iter()
                .filter(|c| p.members[**c])
                .map(|c| u32::from(p.floor_percent[*c]))
                .sum();
            if p.hard_limit < p.target || floors > 100 {
                return Err(ConfigError::Pool);
            }
        }
        Ok(())
    }
}
