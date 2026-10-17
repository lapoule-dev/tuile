<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — TO-BE invariants and spec of the new core

## 1. TO-BE invariants (each one asserted per frame in simulation)

| ID | Invariant | Measure |
|---|---|---|
| I1 | Ground always covered | `lost == 0` every frame; magenta-free "clear colour" scan headless |
| I2 | Drawn set never exceeds the budget except by bounded, visible-only overflow | `ledger.bytes(class) ≤ budget(class) + overflow(class)`; overflow bytes ⊆ protected set |
| I3 | Convergence | camera still ⇒ `exact == selected`, `underdraped == 0`, `stand_ins == 0` within N frames (N = 120 desktop, 240 iOS on the simulated network at p95 latency) and the frame decision digest constant afterwards |
| I4 | No oscillation | still camera ⇒ no tile switches level more than once in 600 frames |
| I5 | No stale request | an in-flight load whose tile left the need set is cancelled within `grace = 2 frames` and its decode job is never started (or is discarded on start) |
| I6 | No throughput decay | over 1 simulated hour: `load_latency_p95(last 5 min) ≤ 1.1 × (first 5 min)`; per-upload CPU cost independent of session age (O(log N)) |
| I7 | Bounded state | every map/queue has an explicit cap: pending bytes ≤ credit, ancestry ≤ k × resident, request heap ≤ H |
| I8 | Adapt without holes | memory warning ⇒ bytes ≤ new target within M frames, I1 holds throughout; thermal ⇒ upload budget and target fps shrink |
| I9 | Atomic region swap | a REPLACE parent is undrawn only in the frame all its visible children are drawable (terrain + imagery at their needed level − 1) |
| I10 | Skirts invisible over covered ground | 0 skirt pixels where any surface (exact or fallback) is in front |
| I11 | Imagery matches need | `|draped_level − needed_level| ≤ 1` for drawn tiles after convergence |
| I12 | Determinism | same event sequence (frames, arrivals, pressure) ⇒ same decision stream (digest) |

## 2. Where it lives

- **A new module `tuile_core::budget`.** It is pure, deterministic, and has no clock, no
  wgpu and no tokio. It builds for wasm32. It is driven by `frame(...)` and events.
- **Why core rather than a new crate.** `GeometryServer` (core) must call it, and the
  dependency rules forbid core → anything heavy. A separate crate `tuile-budget` that core
  depends on is equivalent. *Decision D2.*
- `tuile-wgpu` executes the plans: upload in the plan's order and within its bytes and ms,
  report actual bytes, drop on Evict. `tuile-viewer` supplies the inputs: profile, pressure,
  thermal.

## 3. Types and API (sketch)

```rust
pub enum Class { Mesh, Imagery, Overlay, StandIn }
pub struct Bytes(pub u64);

pub struct BudgetConfig {
    pub target: PerClass<Bytes>,          // trim-to
    pub overflow: PerClass<Bytes>,        // visible-only headroom
    pub max_in_flight: u32,
    pub request_heap: u32,                // bounded wish list actually scheduled
    pub upload_bytes_per_frame: Bytes,
    pub upload_micros_per_frame: Option<u32>,
    pub delivery_credit: Bytes,           // max decoded bytes in transit to the consumer
    pub cancel_grace_frames: u32,
    pub pinned_level: Option<u32>,
    pub policy: Policy,                   // Interactive | Batch (film/bake, exact & unbounded)
    pub failure: FailurePolicy,           // EndSession (film) | RetryBackoff (interactive) — D1
}
pub enum Pressure { Normal, Warning, Critical, Background }
pub enum Thermal { Nominal, Fair, Serious, Critical }

/// Built from traversal output each pass; the only input about the view.
pub struct NeedSet {
    pub drawn: Vec<(TileId, Need)>,          // frontier the traversal wants drawn
    pub blockers: Vec<(TileId, Need)>,       // what a held REPLACE waits on
    pub preload: Vec<(TileId, Need)>,        // ancestors, ring
}
pub struct Need {
    pub level: u32, pub parent: Option<TileId>,
    pub sse_excess: f32,      // log2(sse/τ_eff), clamped
    pub centrality: f32,      // 1 - off_axis, [0,1]
    pub distance: f32,
    pub imagery_level: u32,   // needed drape level for THIS tile (couples terrain/imagery)
}

pub struct Budget { /* ledger, states per tile, heap, frame no., τ_adj */ }

impl Budget {
    pub fn new(cfg: BudgetConfig) -> Self;
    pub fn frame(&mut self, f: FrameInput<'_>) -> FrameDecisions;
    pub fn on_loaded(&mut self, t: TileId, est: Cost) ;          // decoded, ready to deliver
    pub fn on_failed(&mut self, t: TileId, transient: bool);
    pub fn on_drawable(&mut self, t: TileId, actual: Cost);     // consumer ack w/ bytes
    pub fn on_discarded(&mut self, t: TileId);                  // consumer dropped before upload
    pub fn set_pressure(&mut self, p: Pressure);
    pub fn set_thermal(&mut self, t: Thermal);
    pub fn release_beyond(&mut self, target: Bytes);            // batch "trim between frames"
}

pub struct FrameInput<'a> { pub frame: u64, pub need: &'a NeedSet, pub drawn_last: &'a [TileId] }
pub struct FrameDecisions {
    pub start: Vec<TileId>, pub cancel: Vec<TileId>,
    pub deliver: Vec<TileId>,          // within delivery credit, priority order
    pub redrape: Vec<(TileId, u32)>,   // imagery upgrade/downgrade as same-id Content
    pub evict: Vec<TileId>,
    pub upload_plan: UploadPlan,       // ordered ids + byte/µs caps + swap groups
    pub sse_scale: f32,                // memory-adjusted τ multiplier fed back to traversal
    pub stats: BudgetStats,
}
```

- **Tile states:** `Wanted → Loading → Loaded (server-side) → InTransit → Drawable(acked) →
  Evicting`, plus `Failed { retries, next_frame }`.
- **Traversal residency** becomes `Drawable` (acked), not `Loaded`. Selection is then
  drawable by construction. The fallback walk and stand-ins become rare transients.

**Priority** (lexicographic key, ints, deterministic):
1. class: Floor, Hole (no drawable ancestor), Blocker, Drawn-refine, Redrape-up, Ring,
   Preload-ancestor, Refresh;
2. score = `sse_excess × (0.5 + 0.5 × centrality) + age_bonus(frames waiting) +
   cover_bonus(waiting descendants)`;
3. id.

**Eviction:**
- Never: drawn ∪ fallback chain of drawn up to the nearest drawable ancestor and the floor
  ∪ blockers ∪ in-transit ∪ pinned.
- Otherwise, by `frames_since_needed` descending, then level descending, then distance
  descending, then id.
- The sweep runs to `target`. Over `target + overflow`, the protected set alone ⇒
  `sse_scale ×= 1.02` per frame. Under `target`, ÷1.02 back to 1.

**Costs:**
- Charged on delivery from a deterministic estimator that both sides share:
  - mesh = vertices × 32 + indices × 4 + 8 272;
  - texture = w × h × 4 × 4/3, once per `(source, coord)`.
- Actual bytes are reported in the ack and observed. Invariant: drift < 5 %.

**Upload plan:**
- Priority order; stops at bytes or µs (first item always goes).
- Swap groups: the visible children of one REPLACE parent are completed together when
  possible.
- Stale entries are dropped and reported as `Discarded`.

**Imagery coupling:**
- `Need.imagery_level` is computed per tile from its own error and distance. This replaces
  the global altitude `ImageryDetail`.
- If draped < needed − 1 ⇒ `redrape` (a same-id `Content`, new before old, the existing
  refresh path).
- If draped > needed + 1 and under pressure ⇒ a downgrade redrape.

## 4. Skirts never show (renderer contract, tuile-wgpu)

1. `PreparedMesh` keeps `surface: Range<u32>` and `skirts: Range<u32>`. Terrain already
   appends skirts last (mesh.rs 303-308), so this is just a recorded offset.
2. Pass order:
   1. backdrop;
   2. Solid **surface ranges** (depth write, stencil OWNED);
   3. Fallback **surface ranges** (stencil ≠ OWNED, depth test **and write**; safe now that
      owned pixels are already drawn);
   4. **Skirt pass**: the skirts of all drawn tiles, depth test `Less`, depth write, **no
      stencil write**.

   A skirt is then visible only through a real crack. This fixes M1.1 and M1.2.
3. A per-edge skirt mask and depth in the tile uniform, from the stitch plan:
   - an edge welded to a same-level drawn neighbour has its skirt collapsed (depth 0);
   - otherwise the depth is the skirt height at the coarser neighbour's level, replacing the
     fixed 16× rule for drawn edges.
4. Atomic region swap (I9): the budget publishes a REPLACE region's children as drawable
   only when all of them are Drawable with imagery at least needed − 1. Otherwise the parent
   stays drawn. New before old, per region.

## 5. Interfaces with GeometryStream and ContentPump

**Protocol (protocol.rs)**
- `Ack { tile }` → `Ack { tile, bytes: u64 }`.
- New messages:
  - `Credit { bytes }`: consumer → server flow control, which replaces the unbounded
    content backlog;
  - `Discarded { tile }`;
  - `Pressure(Pressure)` and `Thermal(Thermal)`, which replace `Trim { budget_bytes }`;
  - `ReleaseBeyond { bytes }`, kept for batch.
- `Select` gains `swap_groups` and the per-tile priority. `ancestry` stays.
- `InProcessStream`:
  - content flows only within credit;
  - `Select` is coalesced (latest wins);
  - the channel stays unbounded only for small control messages, now bounded in count by
    construction.

**Runtime (GeometryServer)**
- Owns `Budget`, and calls `frame()` once per coalesced client burst.
- Arrivals update states but do **not** re-run the traversal each time. The traversal reruns
  at most once per client frame, or on an arrival that unblocks a hold. This removes M2.1.
- `camera_moved` is replaced by need-set membership plus grace.

**Offload**
- `Job` gains a cancel token, so a cancelled load's decode is skipped.

**ContentPump (tuile-wgpu)**
- `pending` becomes a priority structure keyed by the plan.
- `advance(plan, …)` replaces `pump/pump_while/advance/advance_with/advance_while`.
- `ancestry` is pruned to the closure of prepared ∪ selected.
- The per-upload pool scans move to O(1) counters.
- `gpu_bytes` becomes the actual-bytes report.

**Film path**
- Bake uses `Policy::Batch`:
  - unbounded targets;
  - the same request order and cancellation rule as today;
  - `ReleaseBeyond` between frames = current `release_beyond`;
  - `FailurePolicy::EndSession`.
- `traverse()` is unchanged under `exact_traversal`. Its residency input in Batch is
  `Loaded` (no consumer ack exists in bake).
- **Required result:** byte-identical `det!` pass digests (`sel_digest`, `req_digest`,
  evict digests) on recorded golden scenes, and an identical film. Any deviation is listed.

## 6. What is deleted at merge (one branch, no flag, no shadow)

**tuile-core**
- `src/cache.rs` entirely:
  - `ResidentCache`;
  - `HIGH_WATER` 31, `LOW_WATER` 38;
  - `release_beyond` 300, `release_all_but` 316, `trim_except` 375.
- `src/traversal.rs`:
  - the `Config` fields `resident_budget_bytes` 291, `maximum_simultaneous_fetches` 293,
    `pinned_level` 318, `resident_tile_limit` 347 (move to `BudgetConfig`);
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
- `src/renderer.rs`:
  - Fallback depth-write false 192;
  - the single-range draws 401-502, which become surface/skirt ranges.

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

**tuile-planetary**
- `ImageryDetail` 290-312 and its use at 1055-1060, replaced by `Need.imagery_level` passed
  to `load`/`redrape`.

**Downstream changes that must land in the same commit**
- `tuile-bake/src/session.rs`:
  - 1051 (`in_process_with` + `BudgetConfig::batch()`);
  - 1254-1262 (`ReleaseBeyond`);
  - 1529-1533 (`exact_traversal`, budget fields removed).
- `tuile-web/src/worker.rs` 129. It is already broken per PR #60; fix it here.
- `examples/orbit-probe`, `examples/globe-bulk`, `tuile-wgpu/examples/snapshot.rs`.
- Tests:
  - `tuile-wgpu/tests/{eager_batches,fast_zoom_never_empties,no_black_ground,no_seams,no_two_surfaces,stand_ins}.rs`;
  - `tuile-planetary/tests/selection_covers_the_ground.rs`;
  - `tuile-viewer/src/app/stall.rs`.
- tuile-hydra and tuile-film-native: these consume bake only, so there is no API change; the
  golden check covers them.

## 7. Test strategy

**`tuile-core/tests/budget_sim/`, deterministic and seeded**
- A synthetic quadtree globe with terrain and imagery levels.
- `SimNet`: latency distribution, jitter, failure rate, stalls, bandwidth.
- `SimGpu`: byte counters per class; upload cost = a + b × bytes.
- Scripted cameras: zoom out, 180° rotation in 1 s, pan, back-and-forth, still for 10 min,
  memory warning mid-flight.
- Every invariant I1–I12 asserted every frame.

**Property tests (proptest)**
- Eviction never selects protected tiles.
- Priority is a total order and stable under id permutation.
- The ledger equals the sum of states.
- Cancel ⇒ no later deliver.
- Swap groups are atomic.
- Pressure never yields `lost > 0`.

**Soak**
- One simulated hour, about 216 k frames, for each policy. Assert I6 and I7 on the slopes.

**Golden traces**
- The decision digest per frame for each scenario, stored. A change requires a reviewed
  update.
- Film: `det!` pass digests from `main` recorded before the branch.

**Real GPU, headless (`tuile-wgpu/tests`)**
- New tests: `skirt_never_shows_through_fallback`, `still_camera_cancels_nothing`,
  `zoom_back_redrapes_sharp`, `upload_plan_respects_bytes`.
- Every existing test kept.

**Devices**
- Mac: the replays from AS-IS §4.2 with the JSONL observer.
- iPhone: `TUILE_TOUCH_SCRIPT` tour, a memory warning (simulated from Xcode), thermal
  "Serious".

## 8. Metrics and observability
- `BudgetStats` per frame:
  - bytes and overflow per class; `sse_scale`;
  - started, cancelled, delivered, discarded, evicted, redraped;
  - heap and credit usage;
  - the convergence frame counter; underdraped.
- `BudgetObserver` in tuile-wgpu, watch-only (pattern of `tuile-film-native/src/observe.rs`),
  with sinks to JSONL and `/metrics`.
- Viewer debug overlay (key `G`): bars per class versus target and overflow; a pending /
  in-flight / credit sparkline; counts of drawn exact, fallback and stand-in; skirt
  diagnostic toggle.

## 9. Implementation plan (one branch; buildable at every commit; the old core gone at the cut-over)

1. **(S)** Repro tests and the observer on current code. The three headless symptom tests are
   committed red (documented), and film golden `det!` traces are recorded from `main`.
2. **(M)** `tuile_core::budget` types, ledger, priority, eviction, upload planner, as pure
   code with its own unit and proptest suite. It is unused, so the build stays green and no
   runtime path exists.
3. **(M)** Simulation harness and soak. The budget is driven by the sim's own server model
   until I1–I12 hold.
4. **(L)** **Cut-over commit:**
   - protocol changes; runtime on `Budget`;
   - offload cancel tokens;
   - pump `advance(plan)`;
   - viewer `Profile::budget()` and pressure/thermal;
   - bake and web adapted.
   - Every item of §6 is deleted in this same commit.
5. **(M)** Renderer: surface/skirt ranges, fallback depth write, skirt pass, per-edge skirt
   mask from the stitch plan. Symptom S1 test goes green.
6. **(M)** Per-tile imagery need: `ImageryDetail` removed, redrape path. Symptom S3 test goes
   green.
7. **(S)** Region swap groups (I9) and stand-in policy (D3).
8. **(S)** Tuning per profile, then the device sessions.

**Merge gate (all required)**
- the simulation, property and soak suite green;
- the real-GPU headless suite green;
- the three symptom tests red on step 1 and green now;
- film `det!` digests byte-identical, or each deviation listed and accepted;
- a Mac session and an iPhone session checked by eye (videos in `videos/` with their
  parameters).

**Risks**
- The cut-over commit is large (L): review it in pieces on the branch.
- Changing the traversal residency to Drawable may change film behaviour. Batch keeps Loaded.
- Fallback depth write changes relief occlusion; covered by `no_two_surfaces`.

**Owner decisions**
- **D1:** interactive load failure policy (retry with backoff instead of ending the session).
- **D2:** a module in core, or a crate.
- **D3:** keep stand-ins (a cheap class) or drop them once the selection is drawable by
  construction.
- **D4:** imagery texture arrays or atlases (phase 2).
- **D5:** iOS budget fractions and the credit size.
