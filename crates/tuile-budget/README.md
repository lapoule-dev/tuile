<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-budget

The graphics budget: one pure, deterministic crate that decides what is
loaded, delivered, uploaded, drawn and evicted, for viewers
([`Policy::Interactive`]) and films ([`Policy::Batch`]) alike.

It reads no clock, spawns nothing, touches no GPU and iterates no hash
map: time, the view, arrivals and platform signals are inputs, and the
same inputs give the same [`Decisions`], bit for bit
([`Decisions::digest`]). It builds for `wasm32-unknown-unknown`.

See `docs/explorations/graphics-budget-to-be-and-spec.md` for the
invariants I1–I15 it upholds.

## The step

Between two calls to [`Budget::decide`], arrivals are recorded
([`Budget::on_loaded`], [`Budget::on_drawable`], …) but not applied; the
next `decide` applies them in a canonical order, whatever order they came
in, then decides.

## Where it sits

At the bottom of the workspace: it depends on no other tuile crate, and on
no GPU API, async runtime or I/O. It owns the tile handle, [`TileId`]
([`tile_id`]), which the tile sources share with it.

`tuile-core` depends on it and re-exports it whole as `tuile_core::budget`
(and [`TileId`] as `tuile_core::source::TileId`), so a host that already
depends on `tuile-core` reaches the budget through it. The executors —
`tuile-core`'s geometry server, `tuile-wgpu`, `tuile-film-gpu` — read its
[`Decisions`] and report arrivals back; nothing they measure enters
[`Budget::decide`].

Its lints are part of its contract: no iteration over a hash container and
no narrowing `as` anywhere in the crate (the same digest on every target),
no float arithmetic in the decision engine and its state.

## Example

One interactive step: a single terrain tile is wanted, and the budget asks
for it to be loaded.

```rust
use tuile_budget::{
    Budget, BudgetConfig, Bytes, Class, FrameInput, Micros, Need, NeedSet, ResourceId, TileId,
};

let mut budget = Budget::new(BudgetConfig::interactive(Bytes::mib(512)))?;

let tile = TileId::from_terrain(0, 0, 0);
let mut need = NeedSet::new();
need.drawn.push((tile, Need::new(0, Class::TerrainMesh)));

let decisions = budget.decide(FrameInput::new(0, Micros::from_millis(16), &need));
assert!(decisions
    .start
    .iter()
    .any(|s| s.resource == ResourceId::Mesh(tile)));

// The same inputs give the same decisions, bit for bit.
let mut again = Budget::new(BudgetConfig::interactive(Bytes::mib(512)))?;
let replay = again.decide(FrameInput::new(0, Micros::from_millis(16), &need));
assert_eq!(decisions.digest(), replay.digest());
# Ok::<(), tuile_budget::ConfigError>(())
```

## Features

None. Wasm-clean; its only dependency is `thiserror`.

## Tests

- `tests/budget_core.rs`, `tests/budget_signals.rs` — the decision rules;
- `tests/budget_properties.rs` — properties over random needs and arrivals
  (proptest);
- `tests/budget_sim/` — a simulated world, network and GPU driving the
  budget through scenarios, with golden decision digests (`goldens.txt`,
  `pattern_goldens.txt`) and allocation ratchets (`alloc_ratchets.txt`).
  The scenarios run in release only:
  `cargo test --release -p tuile-budget --test budget_sim`;
- `benches/budget_decide.rs` — what one `decide` costs:
  `cargo bench -p tuile-budget --bench budget_decide`.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
- [`docs/explorations/graphics-budget-roadmap.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/explorations/graphics-budget-roadmap.md)
