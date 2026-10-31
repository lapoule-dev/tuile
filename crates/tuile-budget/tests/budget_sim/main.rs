// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget's deterministic simulation (plan Step 3): a synthetic world
//! with sea, plain and mountains, a seeded network with failures and stalls,
//! a GPU that refuses the impossible, scripted cameras, and every invariant
//! I1–I15 that applies checked at every step.
//!
//! Scenarios are named after the behaviour they pin down. The one-hour soak
//! is `#[ignore]`d and run by hand in release (no nightly job exists yet); a
//! five-minute soak runs on every PR.

#![allow(clippy::unwrap_used, clippy::panic)]
// The counting allocator (`alloc.rs`) is the one unsafe item: a
// `GlobalAlloc` that forwards to the system allocator. Test-only.
#![allow(unsafe_code)]

mod alloc;
mod detmath;
mod gpu;
mod motion;
mod net;
mod patterns;
mod ratchets;
mod rng;
mod scenarios;
mod server;
mod soak;
mod world;

/// Every allocation of this binary goes through the counter; it counts only
/// on a thread that armed it (`alloc::counted`).
#[global_allocator]
static HEAP: alloc::Counting = alloc::Counting;
