<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — TO-BE invariants and spec of the new core

Revised after the independent survey (`graphics-budget-independent-practices.md`) and the
owner's decisions of 2026-10-17:

- **First priority: a very solid, very tested budget module.**
- **Skirts are out of scope for now.** No skirt pass, no stencil change in this branch. The
  diagnostic view stays, so the S1 reproduction can decide later between the stencil
  mechanism (AS-IS M1.1) and the gap-during-swap mechanism (survey). Removing skirts in
  favour of stitching every drawn edge is a later, separate piece of work.
- **Imagery is independent of terrain.** Each picks its level by its own criterion, inside
  the same budget.
- **One cut-over, no migration.** The old budget management is deleted the day the branch is
  merged.
- **Films use the same module**, under a batch policy with an unbounded budget.

## 1. TO-BE invariants (each one asserted per frame in simulation)

| ID | Invariant | Measure |
|---|---|---|
| I1 | Ground always covered | `lost == 0` every frame; headless clear-colour scan finds none |
| I2 | Drawn bytes never exceed the budget except by bounded, visible-only overflow | `ledger.bytes(class) ≤ target(class) + overflow(class)`; every overflow byte belongs to the protected set P |
| I3 | Convergence | camera still ⇒ within T seconds of simulated time (desktop 2 s, iOS 4 s at p95 simulated latency): `exact == selected`, `underdraped == 0`, `stand_ins == 0`, and the decision digest is constant afterwards |
| I4 | No oscillation | camera still ⇒ no tile, terrain or imagery, changes level more than once in 10 s |
| I5 | No stale work | a request whose tile left the need set is dropped from the queue in the same decision step; an in-flight load is cancelled within the grace *time*; its decode is skipped or discarded |
| I6 | No throughput decay | over 1 simulated hour: `load_latency_p95(last 5 min) ≤ 1.1 × (first 5 min)`; per-decision cost O(changes · log N), independent of session age |
| I7 | Bounded state | every map and queue has an explicit cap: request heap ≤ H, pending bytes ≤ delivery credit, ancestry ≤ k × resident |
| I8 | Adapt without holes | memory pressure ⇒ bytes ≤ new target within a bounded time, I1 holds throughout; thermal ⇒ upload budget and target fps shrink |
| I9 | Atomic swaps, both directions | refining: the parent stops being drawn only in the decision step where all its visible children are drawable; coarsening (zoom-out): the children stop being drawn only when the parent is drawable. Same for imagery: the old drape is released only after the new one is drawn |
| I10 | Imagery matches need | after convergence, for every drawn tile, `|draped_imagery_level − needed_imagery_level| ≤ 1`, independently of the tile's terrain level (within the cap of §4) |
| I11 | Protected set is never evicted | P = drawn ∪ the fallback chain (terrain **and** imagery) of every needed-but-absent tile ∪ in transit to the GPU ∪ the pinned floor |
| I12 | Determinism | the same sequence of inputs (camera states, arrivals with their generation, pressure, thermal) ⇒ the same decision stream, bit for bit (digest); no decision reads a wall clock or a hash-map iteration order |
| I13 | Batch exactness (films) | under `Policy::Batch`, each frame is drawn only once every tile and drape it needs is drawable; nothing it needs is ever evicted; the frame is a function of the frame's inputs only |

## 2. Where it lives

- **A new module `tuile_core::budget`.**
  - It is pure and deterministic: no clock, no wgpu, no tokio. It builds for wasm32.
  - It is driven by `decide(...)` and by events. Time enters only as an input
    (`FrameInput::now`, simulated or real), never read inside.
- **Why core.** `GeometryServer` (core) owns it, and core may not depend on anything heavy.
  A separate crate `tuile-budget` would be equivalent (D2: core recommended).
- **Who does what:**
  - `tuile-wgpu` executes the decisions: uploads in plan order within the plan's bytes,
    reports actual bytes, drops on evict;
  - `tuile-planetary` builds drapes at the level the budget asks;
  - `tuile-viewer` supplies profile, pressure and thermal;
  - `tuile-bake` and the film renderer use `Policy::Batch`.

## 3. Types and API (sketch)

```rust
pub enum Class { TerrainMesh, Tiles3dMesh, Drape, Overlay, StandIn }
pub struct Bytes(pub u64);

pub enum Policy {
    /// Viewers: bounded targets, overflow for the visible set, LOD relaxation, cancellation.
    Interactive,
    /// Films and bakes: unbounded targets, nothing needed is ever evicted, a frame is drawn
    /// only when complete, no relaxation, no cancellation of needed work.
    Batch,
}

pub struct BudgetConfig {
    pub policy: Policy,
    pub target: PerClass<Bytes>,           // trim-to; Batch: Bytes::MAX
    pub overflow: PerClass<Bytes>,         // visible-only headroom; Batch: unused
    pub max_in_flight: PerClass<u32>,      // request quotas per class (meshes cannot starve drapes)
    pub request_heap: u32,                 // bounded wish list actually scheduled
    pub upload_bytes_per_frame: Bytes,     // deterministic; Batch: Bytes::MAX
    pub delivery_credit: Bytes,            // decoded bytes in transit to the consumer
    pub cancel_grace: Seconds,             // time, not frames
    pub relax: Relaxation,                 // LOD relaxation rate per second up/down, with hysteresis
    pub pinned_level: Option<u32>,         // the coarse floor, terrain and imagery
    pub imagery_over_terrain_cap: u32,     // max imagery levels above the tile's terrain level (3)
    pub failure: FailurePolicy,            // EndSession (Batch) | RetryBackoff (Interactive)
}
pub enum Pressure { Normal, Warning, Critical, Background }
pub enum Thermal { Nominal, Fair, Serious, Critical }

/// Built from the traversal each decision step; the only input about the view.
pub struct NeedSet {
    pub drawn: Vec<(TileId, Need)>,      // terrain/3D tiles the traversal wants drawn
    pub blockers: Vec<(TileId, Need)>,   // what a held REPLACE waits on
    pub preload: Vec<(TileId, Need)>,    // ancestors, ring
}
pub struct Need {
    pub level: u32,
    pub parent: Option<TileId>,
    pub sse_excess: f32,                 // log2(sse / τ_eff), clamped
    pub centrality: f32,                 // 1 − off-axis, [0, 1]
    pub distance: f32,
    pub imagery: ImageryNeed,            // see §4 — independent of `level`
}
pub struct ImageryNeed { pub level: u32, pub texel_ratio: f32 }

pub struct Budget { /* ledger, per-resource states, request heap, generations, relax state */ }

impl Budget {
    pub fn new(cfg: BudgetConfig) -> Self;
    pub fn decide(&mut self, f: FrameInput<'_>) -> Decisions;
    pub fn on_loaded(&mut self, r: ResourceId, gen: Generation, est: Cost);
    pub fn on_failed(&mut self, r: ResourceId, gen: Generation, transient: bool);
    pub fn on_drawable(&mut self, r: ResourceId, gen: Generation, actual: Cost);
    pub fn on_discarded(&mut self, r: ResourceId, gen: Generation);
    pub fn set_pressure(&mut self, p: Pressure);
    pub fn set_thermal(&mut self, t: Thermal);
    pub fn release_beyond(&mut self, target: Bytes);   // Batch: between frames, deterministic
}

/// A resource is a terrain/3D tile's mesh, or a drape (a tile's imagery at one level).
pub enum ResourceId { Mesh(TileId), Drape(TileId, u32 /* imagery level */) }

pub struct FrameInput<'a> { pub frame: u64, pub now: Seconds, pub need: &'a NeedSet, pub drawn_last: &'a [TileId] }
pub struct Decisions {
    pub start: Vec<(ResourceId, Generation)>,
    pub cancel: Vec<ResourceId>,
    pub deliver: Vec<ResourceId>,        // within delivery credit, priority order
    pub evict: Vec<ResourceId>,
    pub upload_plan: UploadPlan,         // ordered resources, byte cap, swap groups
    pub draw: DrawSet,                   // per drawn tile: mesh + drape (own or ancestor's, with UV transform)
    pub sse_scale: f32,                  // LOD relaxation fed back to the traversal (Interactive only)
    pub stats: BudgetStats,
}
```

**Resource states**
- `Wanted → Queued → Loading(gen) → Loaded → InTransit → Drawable → Evicting`, plus
  `Failed { retries, next_at }`.
- Every result carries the generation of the request that produced it. A stale generation
  is discarded.
- Arrivals are applied at the next `decide` call, never mid-step.

**Traversal residency**
- `Interactive`: `Drawable` (acked), so the selection is drawable by construction.
- `Batch`: `Loaded`, as today; the frame waits until everything needed is drawable.

**Priority**
- An integer lexicographic key, fully deterministic:
  1. tier: Floor, Hole (no drawable fallback), Blocker, Drawn-refine, Drape-up, Ring,
     Preload-ancestor, Refresh;
  2. score: `sse_excess × (0.5 + 0.5 × centrality) + age_bonus(time waiting) +
     cover_bonus(waiting descendants)`, quantised to an integer;
  3. resource id.
- Requests are deduplicated by `ResourceId`.
- `max_in_flight` is applied per class.

**Eviction**
- Never touches P (I11).
- Otherwise, in order: time since last needed (descending), then level (descending, so the
  deepest go first and ancestors are kept), then distance (descending), then id.
- Sweep to `target` with high/low watermarks.

**Overflow and relaxation (Interactive)**
- When P alone exceeds `target + overflow`, `sse_scale` rises at `relax.up` per second.
- Under `target`, it decays at `relax.down` per second back to 1, with hysteresis between
  the two thresholds.
- It is never per frame, so it does not depend on the frame rate.

**Costs**
- Charged from a deterministic estimator that both sides share, in GPU bytes:
  - mesh: vertices × stride + indices × 4 + the per-tile uniform + the CPU copy kept for
    stitching;
  - drape: w × h × 4 × 4/3, compressed sizes when a GPU format is used.
- Staging and in-transit bytes are counted against `delivery_credit`.
- Actual bytes come back in the ack. Invariant: drift < 5 %, otherwise the estimator is
  wrong.

**Upload plan**
- Priority order, capped in **bytes** only, so it is deterministic. The first item always
  goes.
- A wall-clock guard may only *shorten* a plan in the executor, and is reported as a
  deviation. It never feeds back into decisions.
- Swap groups (I9) are completed together when possible.

## 4. Imagery independent of terrain

- **Terrain level:** chosen by geometric screen-space error, as today.
- **Imagery need, per drawn tile, each decision step:**
  - computed from the projected size of one imagery texel at the tile's closest visible point
    (distance and obliquity) against the target pixels per texel;
  - hysteresis of ±0.5 level, so it does not flap;
  - clamped to `[pinned_level, terrain_level + imagery_over_terrain_cap]` and to the source's
    maximum level.
  - It is **not** derived from the terrain's geometric error, and not from a global altitude.
- **Drape:** a tile's drape at level L is a resource of class `Drape`, the mosaic of the
  imagery tiles covering it at L, as `tuile-planetary` builds today. It is budgeted and
  prioritised like any resource, with its own request quota.
- **Fallback:**
  - if the needed drape is not drawable, the tile draws the best drawable drape among
    itself at a coarser level or its ancestors, through a UV transform;
  - that drape is in P until the needed one is drawn (new before old).
- **Up and down:**
  - zooming back in raises the need, and the sharper drape is requested at the `Drape-up`
    tier;
  - under pressure, a drape above `need + 1` may be replaced by a coarser one, built first.
- **Replaces:** `ImageryDetail` (the global altitude texel target) and the load-time freeze.

## 5. Interfaces with GeometryStream and ContentPump

**Protocol (protocol.rs)**
- `Ack { resource, gen, bytes }`.
- New messages:
  - `Credit { bytes }`;
  - `Discarded { resource, gen }`;
  - `Pressure(..)` and `Thermal(..)`, which replace `Trim { budget_bytes }`;
  - `ReleaseBeyond { bytes }`, for Batch.
- `Content` carries `ResourceId` and `gen`. Drapes travel as their own content, so a tile's
  imagery can change without resending its mesh.
- `Select` carries the `DrawSet`, which says which drape each tile draws, and the swap
  groups.
- `InProcessStream`: content flows within credit; `Select` is coalesced (latest wins).

**Runtime (GeometryServer)**
- Owns `Budget`, and calls `decide()` once per coalesced client step (Interactive) or once per
  frame (Batch).
- Arrivals no longer re-run the traversal one by one.
- The `camera_moved` cancellation is gone. Need-set membership plus the grace time replace
  it.

**Offload**
- Jobs carry a cancel token and a generation. A cancelled or stale job is skipped before
  decode.

**ContentPump (tuile-wgpu)**
- `pending` is keyed by the plan's priority.
- One entry point, `advance(plan, …)`.
- `ancestry` is pruned to the closure of prepared ∪ selected.
- O(1) counters replace the per-upload pool scans.
- Drawing follows the `DrawSet`: the drape per tile and the UV transform for an ancestor's
  drape.

## 6. Films and video: `Policy::Batch`, the "infinite" budget

The film renderer (bake → packs → render on the farm) runs the **same** module, with:
- **unbounded targets** (`Bytes::MAX`) and an unbounded upload plan: farm GPUs are sized for
  it;
- **no relaxation, no overflow logic, no cancellation of needed work.** A frame is drawn
  only when every mesh and drape it needs is drawable (I13). The current "hold until
  complete" behaviour, now stated as an invariant;
- **`release_beyond` between frames only.** It is deterministic: what the next frames need
  stays;
- **`FailurePolicy::EndSession`**, as today: a film never ships a hole;
- **the same imagery rule as the viewers (§4).**

**This changes films on purpose.** Today a film's imagery level comes from the global
altitude target. With per-tile need, low oblique shots get sharper imagery near the camera
and lighter imagery far away. Consequences:
- the film goldens and the `det!` digests are re-recorded, **once**, in a dedicated commit;
- an A/B film (main versus branch) of the reference scenes is rendered on the farm, opened
  and compared, and its parameters are recorded in `videos/README.md`;
- any other film difference is a bug.

What must stay identical, and is tested:
- selection and request digests for **terrain** under Batch on the golden scenes;
- frame independence from how frames are split across machines.

## 7. What is deleted at merge (one branch, no flag, no shadow)

**tuile-core**
- `src/cache.rs` entirely:
  - `ResidentCache`;
  - `HIGH_WATER` 31, `LOW_WATER` 38;
  - `release_beyond` 300, `release_all_but` 316, `trim_except` 375.
- `src/traversal.rs`:
  - the `Config` fields `resident_budget_bytes` 291, `maximum_simultaneous_fetches` 293,
    `pinned_level` 318, `resident_tile_limit` 347 (moved to `BudgetConfig`);
  - the budget numbers in `interactive_globe` 388-399;
  - `ResidencyView` 428-454 becomes a view exported by `Budget`.
- `src/runtime.rs`:
  - fields `cache` 118, `in_flight` 120, `started` 124, `acked` 142, `filled` 145,
    `view_moved` 155;
  - the `Cancel`/`Trim` handling 386-411;
  - protection and trim 592-667;
  - cancellation 701-745; spawn loops 752-785;
  - the scheduling half of `refresh` 848-876;
  - insert/evict in `on_load_done` 1023-1063;
  - the per-arrival `retraverse` 270-276.
- `src/protocol.rs`:
  - `ClientMessage::Trim` 52;
  - the bare `Ack` 43;
  - the unbounded pair 272-307.
- `src/offload.rs`: the cancel-blind `run` 193-206.

**tuile-wgpu**
- `src/pump.rs`:
  - FIFO `pending` 28;
  - `UPLOADS_PER_FRAME` 89;
  - `pump`, `pump_while`, `advance`, `advance_with`, `advance_while` 227-397;
  - the unpruned `ancestry` 56;
  - the per-upload `live_by_level` 287-297;
  - `push_front` fills 480;
  - the `retain/contains` Evict 530.
- (Renderer skirt and stencil changes are **not** part of this branch.)

**tuile-planetary**
- `ImageryDetail` 290-312 and its use at 1043-1060 (the load-time level choice), replaced by
  the drape resource at the level the budget asks.

**tuile-viewer**
- `src/profile.rs`:
  - `UploadBudget` 58-79;
  - the `Profile` budget fields 84-98;
  - `traversal()` 190-197;
  - `trim_on_warning` and `trim_in_background` 217-225.

  These are replaced by `Profile::budget() -> BudgetConfig`.
- `src/app/lifecycle.rs`: `Step::Trim` 49, 114, 154-155 → `Step::Pressure`.
- `src/app/gestures.rs` 111-114.
- `src/app/frame.rs`:
  - 113-115 (global texel target);
  - 142-164 (budget closure).

**Downstream changes in the same commit**
- `tuile-bake/src/session.rs`: 1051, 1254-1262, 1529-1533, on `BudgetConfig::batch()`.
- `tuile-web/src/worker.rs` 129, which is already broken per PR #60.
- `examples/orbit-probe`, `examples/globe-bulk`, `tuile-wgpu/examples/snapshot.rs`.
- Tests:
  - `tuile-wgpu/tests/{eager_batches,fast_zoom_never_empties,no_black_ground,no_seams,no_two_surfaces,stand_ins}.rs`;
  - `tuile-planetary/tests/selection_covers_the_ground.rs`;
  - `tuile-viewer/src/app/stall.rs`.
- The downstream host (films): no API change beyond the re-recorded goldens. Its own film
  goldens move with ours.

## 8. Test strategy

**Unit and property tests (proptest)**
- P is never evicted, including the ancestor drapes in use.
- Priority is a total order, stable under permutation of the inputs, and independent of
  hash order.
- The ledger equals the sum of the states.
- A cancel or a stale generation is never delivered.
- Swap groups are atomic in both directions.
- Pressure never yields `lost > 0`.
- The imagery need is monotone in distance and obliquity, its hysteresis holds, and it stays
  within the cap.
- Batch: nothing needed is evicted, and a frame is drawn only when complete.

**Deterministic simulation (`tuile-core/tests/budget_sim/`)**
- A synthetic globe with terrain and imagery pyramids of different depths: plains (shallow
  terrain, deep imagery), mountains, coast.
- `SimNet`: latency distribution, jitter, failures, stalls, bandwidth, per-class servers.
- `SimGpu`: byte counters per class.
- Scripted cameras:
  - zoom out to 200 km and back;
  - 180° rotation in 1 s;
  - pan along a coast;
  - back-and-forth;
  - still for 10 min;
  - memory warning mid-flight;
  - thermal serious.
- Every invariant I1–I13, asserted every step.

**Soak**
- One simulated hour, about 216 k steps, under Interactive. Assert I6 and I7 on the slopes.
- A full film under Batch. Assert I13.

**Golden decision traces**
- One digest per step for each scenario, stored. A change requires a reviewed update.

**Real GPU, headless (`tuile-wgpu/tests`)**
- `still_camera_cancels_nothing`;
- `zoom_back_redrapes_sharp`;
- `rotation_converges`;
- `coast_imagery_sharper_than_terrain`;
- `upload_plan_respects_bytes`;
- every existing test kept.

**Devices**
- The Mac replays and the iPhone touch scripts from AS-IS §4.2, with the JSONL observer.
- A memory warning from Xcode, and thermal "Serious".

**Film**
- Golden scenes rendered on main and on the branch, opened and compared.
- The terrain digests identical.
- The imagery change reviewed and accepted.

## 9. Metrics and observability
- `BudgetStats` per step:
  - bytes and overflow per class; `sse_scale`;
  - started, cancelled, delivered, discarded, evicted, redraped;
  - heap and credit usage;
  - time since the camera stopped, and convergence;
  - underdraped tiles;
  - the imagery-over-terrain histogram.
- `BudgetObserver` in tuile-wgpu, watch-only (the pattern of
  `tuile-film-native/src/observe.rs`), with sinks to JSONL (`TUILE_BUDGET_LOG`) and
  `/metrics`.
- A viewer debug overlay: bars per class against target and overflow; pending, in-flight
  and credit; counts of drawn exact, fallback and stand-in; the imagery level per tile as a
  colour.

## 10. Owner decisions

| | Status | Decision |
|---|---|---|
| **D1** | Proposed | Interactive load failure ⇒ retry with backoff; Batch ⇒ end session |
| **D2** | Recommended | The module lives in `tuile-core` |
| **D3** | Open | Stand-ins: keep them as a cheap class at first; drop them if the simulation shows they are unneeded once the selection is drawable by construction |
| **D4** | Later | Imagery texture arrays or atlases (phase 2) |
| **D5** | Open | iOS budget fractions, credit size, imagery-over-terrain cap (3 proposed) |
| **Skirts** | Deferred | Skirts are deferred (see the header) |
