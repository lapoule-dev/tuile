// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Typed errors of the budget.

use super::types::{Class, ResourceId};

/// A [`BudgetConfig`](super::BudgetConfig) that cannot run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// `low > high`, or `high > 100`.
    #[error("watermarks out of order: low {low}% high {high}%")]
    Watermarks {
        /// The low watermark.
        low: u8,
        /// The high watermark.
        high: u8,
    },
    /// A request heap of zero schedules nothing.
    #[error("the request heap is empty")]
    ZeroRequestHeap,
    /// A class with no request slot can never load.
    #[error("class {0:?} has no request slot")]
    ZeroQuota(Class),
    /// A class's hard limit is below its target.
    #[error("class {0:?} has a hard limit below its target")]
    HardBelowTarget(Class),
    /// The swap reserve is more than half the hard limit.
    #[error("swap reserve {0}% above 50%")]
    Reserve(u8),
    /// The drape side is zero or below one imagery tile.
    #[error("the largest drape side is below one imagery tile")]
    DrapeSide,
    /// The relaxation ceiling is below 1.0.
    #[error("relaxation ceiling {0}‰ is below 1000‰")]
    RelaxCeiling(u32),
    /// A zero first delay or a first delay above the ceiling.
    #[error("retry backoff is incoherent")]
    Backoff,
    /// Batch never retries: a film never ships a hole.
    #[error("a batch budget must end the session on failure")]
    BatchMustEndOnFailure,
    /// A pool whose hard limit is below its target, or whose members'
    /// floors add up to more than the whole.
    #[error("the pool's hard limit is below its target, or its floors exceed 100 %")]
    Pool,
}

/// A condition that ends a session (Batch only).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// A needed resource failed to load under [`FailurePolicy::EndSession`](super::FailurePolicy).
    #[error("{0:?} failed to load")]
    LoadFailed(ResourceId),
}

impl SessionError {
    /// The resource this end of session names, if any.
    ///
    /// The match is exhaustive here, where the enum lives: a new variant
    /// does not compile until it says which resource it names, so a caller
    /// in another crate (where `#[non_exhaustive]` forces a wildcard arm)
    /// never silently reports `None` for it.
    #[must_use]
    pub fn resource(&self) -> Option<ResourceId> {
        match self {
            Self::LoadFailed(r) => Some(*r),
        }
    }
}

/// A disagreement between the ledger and the resource states, found by
/// [`Budget::audit`](super::Budget::audit).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuditError {
    /// The GPU ledger of a class differs from the sum of its resources.
    #[error("GPU ledger of {class:?}: {ledger} bytes, states sum to {states}")]
    Gpu {
        /// The class.
        class: Class,
        /// What the ledger says.
        ledger: u64,
        /// What the states sum to.
        states: u64,
    },
    /// The decoded ledger differs from the sum of its resources.
    #[error("decoded ledger: {ledger} bytes, states sum to {states}")]
    Decoded {
        /// What the ledger says.
        ledger: u64,
        /// What the states sum to.
        states: u64,
    },
    /// The in-flight count of a class differs from its loading resources.
    #[error("in-flight count of {class:?}: {ledger}, states count {states}")]
    InFlight {
        /// The class.
        class: Class,
        /// What the counter says.
        ledger: u32,
        /// What the states count.
        states: u32,
    },
    /// A load slot (in flight, or a local read) was freed with none taken:
    /// the counter saturated at 0 instead of wrapping.
    #[error("{count} load slots freed below zero")]
    SlotUnderflow {
        /// How many times.
        count: u32,
    },
}
