// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget's simulator as a library: the predefined, versioned scenario
//! catalogue the eviction/prefetch decider is judged on, the objective `J`,
//! and the tables the parameter search writes
//! (`docs/explorations/budget-eviction-prefetch-spec.md` §9.3).
//!
//! - [`catalogue`]: scripted camera paths × input profiles (device budget,
//!   network), each with its acceptance bounds; [`CATALOGUE_VERSION`].
//! - [`objective`]: one run of the budget headless over a scenario under a
//!   [`Specification`](tuile_budget::Specification), and its [`Outcome`]:
//!   the time-integrated deficit, holes (infinite), congestion, settling.
//! - [`table`]: outcomes as a Parquet table — never CSV.
//!
//! The world, the network, the GPU and the server are the budget's own
//! simulation (`crates/tuile-budget/tests/budget_sim`), compiled here as they
//! are in `budget_decide`'s bench: one simulation, two users. Moving those
//! modules into this crate, with the test binary a thin caller of it, is the
//! next step of the module map (spec §8b) — listed, not done here.

#![allow(dead_code, clippy::unwrap_used, clippy::panic)]
// The simulation's modules come in whole, its counting allocator with them
// (never installed here: nothing is counted).
#![allow(unsafe_code)]

#[path = "../../tuile-budget/tests/budget_sim/alloc.rs"]
mod alloc;
#[path = "../../tuile-budget/tests/budget_sim/detmath.rs"]
mod detmath;
#[path = "../../tuile-budget/tests/budget_sim/gpu.rs"]
mod gpu;
#[path = "../../tuile-budget/tests/budget_sim/net.rs"]
mod net;
#[path = "../../tuile-budget/tests/reference/mod.rs"]
mod reference;
#[path = "../../tuile-budget/tests/budget_sim/rng.rs"]
mod rng;
#[path = "../../tuile-budget/tests/budget_sim/server.rs"]
mod server;
#[path = "../../tuile-budget/tests/budget_sim/world.rs"]
mod world;

pub mod catalogue;
pub mod objective;
pub mod table;

pub use catalogue::{Device, Network, Scenario, CATALOGUE_VERSION};
pub use objective::{run, Outcome};
