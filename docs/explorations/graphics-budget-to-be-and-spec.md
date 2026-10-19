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

Amended on 2026-10-19 (Phase 1), after the architecture decisions
(`graphics-budget-architecture.md` §6, D-A0 to D-A8):

- six motion patterns P1–P6, each a family of camera scripts with checkable obligations
  (§12);
- I1 extended (every drawn item wears its drapes) and three invariants for stereo and the
  display, I16–I18 (§1);
- the lens — viewport, field of view, scale, per eye — replaces the fixed focal length (§13);
- the platforms and their profiles (§14);
- the API decided, as target signatures (§15). §3 and §5 stay as the sketch of 2026-10-17;
  where they differ from §15, §15 wins;
- the observability contract: every invariant and obligation has a named signal that tests
  and production read the same way, one observer in the core, its sinks, the dashboard,
  the alerts and the field telemetry (§16).

The test plan is `graphics-budget-test-plan.md`.

## 1. TO-BE invariants (each one asserted per frame in simulation)

| ID | Invariant | Measure |
|---|---|---|
| I1 | Ground always covered **and dressed** (extended by D-A1, 2026-10-19) | once the floor is in, every step: `lost == 0` (every wanted tile covered) and `bare == 0` (every drawn item whose need asks for imagery wears a drawable drape for each layer it needs, its own or an ancestor's). The floor's mesh and its drape form one swap group, so the floor is in only when both are drawable; on a layer switch the old layer's drape stays worn until the new one is drawable. Headless and device clear-colour scans find none |
| I2 | Drawn bytes never exceed the budget except by bounded, visible-only overflow | unprotected bytes ≤ `target(class)`, so every overflow byte belongs to the protected set P; and no class ever crosses `hard_limit(class)` (§11.2) |
| I3 | Convergence | camera still ⇒ within T seconds of simulated time (desktop 2 s, iOS 4 s at p95 simulated latency): `exact == selected`, `underdraped == 0`, `stand_ins == 0`, and the decision digest is constant afterwards |
| I4 | No oscillation | camera still ⇒ no tile, terrain or imagery, changes level more than once in 10 s |
| I5 | No stale work | a request whose tile left the need set is dropped from the queue in the same decision step; an in-flight load is cancelled within the grace *time*; its decode is skipped or discarded |
| I6 | No throughput decay | over 1 simulated hour: `load_latency_p95(last 5 min) ≤ 1.1 × (first 5 min)`; per-decision cost O(changes · log N), independent of session age — a requirement of P6 since D-A7, no longer a follow-up (§12.7) |
| I7 | Bounded state | every map and queue has an explicit cap: request heap ≤ H, pending bytes ≤ delivery credit, ancestry ≤ k × resident |
| I8 | Adapt without holes | memory pressure ⇒ bytes ≤ new target within a bounded time, I1 holds throughout; thermal ⇒ upload budget and target fps shrink |
| I9 | Atomic swaps, both directions | refining: the parent stops being drawn only in the decision step where all its visible children are drawable; coarsening (zoom-out): the children stop being drawn only when the parent is drawable. Same for imagery: the old drape is released only after the new one is drawn |
| I10 | Imagery matches need | after convergence, for every drawn tile, `|draped_imagery_level − needed_imagery_level| ≤ 1`, independently of the tile's terrain level (within the cap of §4) |
| I11 | Protected set is never evicted | P = drawn ∪ the fallback chain (terrain **and** imagery) of every needed-but-absent tile ∪ in transit to the GPU ∪ the pinned floor ∪ the drawn cut's ancestor chain ∪ the drawable half of a swap in progress (§11.1) |
| I12 | Determinism | the same sequence of inputs (camera states, arrivals with their generation, pressure, thermal) ⇒ the same decision stream, bit for bit (digest); no decision reads a wall clock or a hash-map iteration order |
| I13 | Batch exactness (films) | under `Policy::Batch`, each frame is drawn only once every tile and drape it needs is drawable; nothing it needs is ever evicted; the frame is a function of the frame's inputs only |
| I14 | Nothing already held is fetched again | no network request is ever issued for a resource present in a local tier (GPU, CPU-decoded, local store); a resource present on the GPU is never re-uploaded; a resource decoded in memory is never re-decoded; an eviction demotes to a local tier, so what comes back within 10 s comes from it (`thrash` counts network refetches of the evicted, `rereads` local ones) |
| I15 | Local before remote | a resource held in the local store is served from it within its own quota, without consuming a network slot; local reads never wait behind network work |
| I16 | One charge per resource, for all eyes (2026-10-19) | any number of views — a stereo rig, a multi-viewport host — gives one `NeedSet`, the union of the views with the largest need per tile (§13.3), and one `decide`; each `ResourceId` is requested, decoded, uploaded and charged once however many views see it. The ledger of a two-eye run equals the ledger of its union need, step by step; `duplicates_in_flight == 0`; one upload per resource per residency |
| I17 | All eyes draw from one decision (2026-10-19) | every eye of a render frame draws the same `DrawSet` (one step, one generation), each culling it by its own frustum. Since the need is the union, no eye shows a hole or a bare item that another lacks: I1 is checked per eye against that eye's frustum (simulation) and by a per-eye black scan (headless, devices) |
| I18 | Per-frame deadline (2026-10-19) | per render frame the executor uploads, in the plan's order, at most `FrameDeadline::bytes`, derived from the display rate (§15.4), and never past a class's hard limit; the first item always goes. What it defers is counted as a deviation and reaches the budget only as later acks, never as an input of `decide`. `upload_bytes_per_frame` stays the plan's deterministic per-step cap. Measure: bytes uploaded per frame ≤ the deadline except a lone first item; I1 holds under any deferral pattern |

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

The sketch of 2026-10-17. The API decided on 2026-10-19 is §15; where they differ, §15
wins.

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
    pub blockers: Vec<(TileId, Need)>,   // ALL the children a held REPLACE waits on, resident or not (§11.1)
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
  1. urgency: Floor, Hole (a wanted tile not drawable: it shows a fallback or finer
     descendants, so its absence forces blur or over-detail), Blocker, Drape (one tier for
     the drape a drawn tile needs: "Drawn-refine" and "Drape-up" merged), Ring, Ancestor,
     Refresh (lowest: free slots and budget only, never preempts, new before old);
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
- When P alone exceeds `target + overflow`, or admission is *strained* (wanted work
  displaces wanted work, or is refused), `sse_scale` rises at `relax.up` per second; four
  times faster at the hard limit (§11.2).
- The imagery need relaxes with it: the texel target grows with `sse_scale` (§11.4).
- Under `target`, it decays at `relax.down` per second back to 1, with hysteresis between
  the two thresholds.
- It is never per frame, so it does not depend on the frame rate.

**Costs**
- Charged from a deterministic estimator that both sides share, in GPU bytes:
  - mesh: vertex and index buffers aligned to 4, the per-tile uniform aligned to 256, the
    CPU copy kept for stitching (resident, not staged); staging rows of textures aligned to
    256 bytes (`COPY_BYTES_PER_ROW_ALIGNMENT`), mip by mip;
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

## 4b. What is already held: residency tiers

The budget knows, for every resource, the cheapest place it already lives in, and never asks
for more than the step from there to "drawn".

```rust
pub enum Tier {
    Gpu,      // uploaded and drawable: nothing to do
    Decoded,  // decoded in memory, not on the GPU: upload only
    Stored,   // in a local store (tile store, HTTP disk cache): local read + decode
    Remote,   // nowhere local: network fetch + decode
}
```

- **Where the tier comes from:**
  - `Gpu` and `Decoded` from the budget's own state machine;
  - `Stored` from a cheap, non-loading presence query on the stores, through a trait the
    fetchers and stores implement: `fn holds(&self, key) -> Presence`. It answers from the
    store's index, never by reading the payload, and is batched per decision step.
    The local tile store (foyer) and the HTTP disk cache both answer it.
  - A `Stored` answer that turns out wrong on read degrades to `Remote` and is counted as a
    store miss: the presence answer is a hint, never a promise.
- **What the tier changes:**
  - the cost (bytes to move, expected latency) and the work to schedule;
  - a request is created only for the steps that are missing (I14);
  - **separate quotas:** `max_local_reads` for `Stored` work, distinct from the network
    `max_in_flight`. Local reads never wait behind network work (I15), and never count
    against the network quota;
  - priority: at equal tier and score, a cheaper tier goes first, because it closes a hole
    sooner.
- **After a relaunch:** a store that survived (desktop exit, iOS suspend) puts its contents
  back at `Stored`. A relaunch then fills the view from disk without network, which is what
  made iOS cover its first view in 6 s instead of 35 s.
- **Stats:**
  - per tier: resources served, bytes and latency;
  - store hit rate, and store misses after a positive presence answer;
  - network bytes avoided.

## 4c. The headless viewer and its control API (for tests)

The tests drive **the real viewer**, the same app on the same frame loop, not a test-only
renderer.

**Headless mode**
- `tuile-viewer --headless`: no window. It renders into an offscreen surface of a given size
  and scale factor, with the same `Profile` (desktop or handheld, selectable, so iOS
  budgets can be exercised on a Mac or Linux runner), the same pump, budget and renderer.
- **A stepped clock:** in headless mode, time advances only when told (`step`), so a test is
  deterministic. A real-time mode exists for soak and latency measurements.
- **The network** can be the real fetchers, a recorded trace, or a simulated network (the
  `SimNet` of §8) injected through the same loader trait.

**Control API**
- It is in-process (a Rust API for tests in the workspace), and the same commands are
  available over a local socket for scripts and agents: Unix socket, or 127.0.0.1 HTTP with
  JSON, **localhost only**.
- It is versioned, and documented with its schema.

| Kind | Commands |
|---|---|
| **Read the budget** | `budget()` returns `BudgetStats` and the ledger per class and tier; `resource(id)` returns state, tier, generation, bytes, priority and the reason it is protected or not; `need()` returns the current `NeedSet` and `DrawSet` |
| **Read the GPU** | `gpu()`: bytes per class, texture and buffer counts, uploads this frame (count, bytes, µs), drift between estimated and actual cost |
| **Read the picture** | `frame()`: framebuffer readback, PNG or raw RGBA. `frame(layer)`: diagnostic layers, such as the tile terrain level, the imagery level, exact, fallback or stand-in, and the tier as colour. `coverage()`: lost pixels and holes |
| **Move** | `set_view(camera)`, `fly_to(target, duration)`, `orbit(angle, duration)`, `zoom(factor, duration)`, `look(heading, pitch)`; touch-equivalent gestures for the handheld profile |
| **Series** | `run(script)`: a list of timed commands, or a `tuile-tape` file. `step(frames)` and `advance(seconds)` move the stepped clock. `wait_until(condition, timeout)`, with conditions `converged`, `idle`, `no_holes` and `bytes_below(class, n)` |
| **Inject** | `pressure(level)`, `thermal(state)`, `background()` / `foreground()`, `network(profile)` to change the latency, failure rate or stall of a simulated network, `kill_and_relaunch()` to test persistence |
| **Record** | `observe(path)`: open the MCAP session tape with the budget's channels beside the camera and the frames (§16.5); `snapshot(path)`: frame plus budget plus GPU state in one bundle, for a failing test's artefacts |

**Uses**
- The headless real-GPU tests of §8 are written against this API.
- So are the symptom reproductions of the AS-IS document (§4.2), as scripts, and the device
  sessions, in real-time mode, with the same scripts as on the simulator.
- An agent can inspect a running viewer without screenshots of a window.

**Constraints**
- No new code path for rendering: headless only swaps the surface.
- The API is read-mostly and side-effect-free except for the explicit commands.
- It is off unless `--headless` or `--control` is passed.
- On iOS the socket API is not compiled in release builds.

## 5. Interfaces with GeometryStream and ContentPump

The sketch of 2026-10-17; §15.2 and §15.4 are the decided protocol and executor. In
particular `Credit` is not added, and the geometry server runs where its budget's inputs
are (D-A5): locally on interactive devices, in a `tuile-server` session for a consumer
that declares its budget.

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

Line numbers are those of `main` at 65b04cb (v0.8.3 plus #83–#87); the cut-over
PR re-reads them, since `main` keeps moving.

**tuile-core**
- `src/cache.rs` entirely:
  - `ResidentCache` 76;
  - `HIGH_WATER` 31, `LOW_WATER` 38;
  - `trim` 282, `release_beyond` 300 (`release_all_but` and `trim_except`
    are already gone from `main`).
- `src/traversal.rs`:
  - the `Config` fields `resident_budget_bytes` 297,
    `maximum_simultaneous_fetches` 299, `pinned_level` 324,
    `resident_tile_limit` 353 (moved to `BudgetConfig`);
  - the budget numbers in `interactive_globe` 394-404 and in the handheld
    preset 420-425;
  - `ResidencyView` 435 becomes a view exported by `Budget`.
  - `traverse`/`traverse_with` (544/571) keep taking `&[ViewState]`: the
    stereo union stays in the traversal (P6).
- `src/runtime.rs`:
  - fields `cache` 387, `in_flight` 389, `started` 393, `acked` 411,
    `filled` 414, `view_moved` 428;
  - the `Cancel`/`Trim` handling of `on_client` 781-811;
  - protection and trim in `retraverse` 1011-1070 (the `selected` protected
    set, the pinned chain, `cache.trim`);
  - the `camera_moved` cancellation 1144-1167;
  - the priming and spawn loops 1174-1206;
  - the scheduling half of `refresh` 1270-1300;
  - insert/evict in `on_load_done` 1457-1484;
  - the per-arrival `retraverse` after `drain_ready` (606; `drain_ready` 645): arrivals
    are applied at the next `decide`.
- `src/protocol.rs`:
  - `ClientMessage::Trim` 52;
  - the bare `Ack { tile }` 43;
  - the unbounded pair `in_process_pair` 294-307 (`InProcessStream` keeps
    the channels, bounded by the delivery credit).
- `src/offload.rs`: `run` 287-301 still runs a cancelled job to the end and
  drops its answer; jobs gain the cancel token and generation (§5).
- Stand-ins (D3, decided 2026-10-19; lines re-read by the cut-over PR):
  `ServerMessage::Fill` and `Retire`, the runtime's stand-in pass, `ClientMessage::Cancel`
  and the budget's `Class::StandIn`; downstream, `tuile-planetary`'s stand-in builder
  and the pump's fill handling (the `push_front` refill below).

**tuile-wgpu**
- `src/pump.rs`:
  - FIFO `pending` 28;
  - `UPLOADS_PER_FRAME` 89;
  - `pump`, `drain`, `pump_while`, `advance`, `advance_with`,
    `advance_while` 227-437;
  - the unpruned `ancestry` 56;
  - the per-upload `live_by_level` call 324;
  - the `push_front` refill 505;
  - the `retain/contains` of `Evict` 537-556.
- (Renderer skirt and stencil changes are **not** part of this branch.)

**tuile-planetary**
- `ImageryDetail` 298-318 and its use in the drape level choice 1170-1186
  (`level_for_texel_spacing` from the terrain error, raised by the global
  altitude target), replaced by the drape resource at the level the budget
  asks.

**tuile-viewer**
- `src/profile.rs`:
  - `UploadBudget` 91-104;
  - the `Profile` budget fields `resident_budget_bytes`,
    `resident_tile_limit`, `simultaneous_fetches` 109-113 and `uploads` 123;
  - `traversal()` 313-320;
  - `trim_on_warning` 340, `trim_in_background` 346,
    `trim_on_critical_pressure` 353.

  These are replaced by `Profile::budget() -> BudgetConfig`. The viewer's
  own `Pressure`, `Thermal` and `DeviceMemory` (profile.rs) stay: they are
  the platform side and feed `set_pressure`, `set_thermal` and `retarget`.
- `src/app/lifecycle.rs`: `Step::Trim` 55 and its producers (156, 207, 227,
  and the tests from 268) → `Step::Pressure`.
- `src/app/gestures.rs`: `Step::Trim` → `ClientMessage::Trim` 353-358 (and
  its test label 439, 475-482).
- `src/app/frame.rs`:
  - 111-123 (global texel target, `ImageryDetail::set_target_texel_spacing`);
  - 128-171 (the upload budget closure, `UploadBudget::allows`).
- `src/native/android_signals.rs` (new since the spec was written):
  `pressure_of_trim` 46 maps `onTrimMemory` levels to the viewer's
  `Pressure`; it stays and feeds `Budget::set_pressure`/`retarget`.
- `src/web/worker.rs` (the web worker moved here from `tuile-web`):
  `ToWorker::Detail { metres }` (control.rs 35, handled at worker.rs 148)
  goes with `ImageryDetail`; `ToWorker::Budget { reported }` (149) becomes
  `retarget`.

**Downstream changes in the same commit**
- `tuile-bake/src/session.rs`: the `ImageryDetail` field 569 and
  `set_imagery_detail` 671/1098-1117, `ClientMessage::Trim` between frames
  1272-1280, the `budget` read at 1041, `exact_traversal` 1570-1573; all on
  `BudgetConfig::batch()` (`frame_ready`, `release_beyond`).
- `examples/wasm-globe` no longer exists (removed on `main`); the web
  viewer is `crates/tuile-viewer` (`src/web`), with `examples/web-viewer`
  and `examples/web-viewer-vite` as its pages.
- `examples/orbit-probe`, `examples/globe-bulk`,
  `crates/tuile-wgpu/examples/snapshot.rs`.
- Tests:
  - `tuile-wgpu/tests/{eager_batches,fast_zoom_never_empties,headless,imagery_passes,no_black_ground,no_seams,no_two_surfaces,overlay,stand_ins}.rs`;
  - `tuile-planetary/tests/{selection_covers_the_ground,seams_are_closed}.rs`;
  - `tuile-viewer/src/app/stall.rs`.
- The downstream host (films): no API change beyond the re-recorded goldens. Its own film
  goldens move with ours.

**Not a seam of the budget**
- `TileStore::held` and `TileStore::get_unheld` in
  `tuile-tile-server/src/store.rs` (792, 806) are the tile *server's* store:
  async, and they return the payload. They are not a presence oracle. The
  client-side oracle (§4b) answers synchronously from an index (the foyer
  store's, the HTTP disk cache's) and never reads a payload.

## 8. Test strategy

The summary of 2026-10-17. The full matrix — pattern × level × profile, every test named,
with the invariant or obligation it proves — is `graphics-budget-test-plan.md` (Phase 2).

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
- Tiers:
  - no network request for a resource at `Stored`, `Decoded` or `Gpu` (I14);
  - a wrong `Stored` hint degrades to `Remote` exactly once;
  - local reads never wait behind network work (I15).

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

**Real GPU, headless**
- Written against the headless viewer and its control API (§4c), so they exercise the real
  app.
- Tests:
  - `relaunch_fills_from_the_store_without_network` (I14, I15);
  - `still_camera_cancels_nothing`;
- `zoom_back_redrapes_sharp`;
- `rotation_converges`;
- `coast_imagery_sharper_than_terrain`;
- `upload_plan_respects_bytes`;
- every existing test kept.

**Devices**
- The Mac replays and the iPhone touch scripts from AS-IS §4.2, with the budget's channels
  on the session tape (§16.5).
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
- `BudgetObserver` in the core, watch-only (the pattern of
  `tuile-film-native/src/observe.rs`), with its sinks: `/metrics`, the MCAP session tape,
  tracing, the control API, the overlay and the web worker's channel. §16 is the contract;
  it supersedes this section where they differ.
- A viewer debug overlay: bars per class against target and overflow; pending, in-flight
  and credit; counts of drawn exact, fallback and stand-in; the imagery level per tile as a
  colour.

## 10. Owner decisions

| | Status | Decision |
|---|---|---|
| **D1** | Proposed | Interactive load failure ⇒ retry with backoff; Batch ⇒ end session |
| **D2** | Recommended | The module lives in `tuile-core` |
| **D3** | Decided 2026-10-19 | Stand-ins go, and become the budget's `Role::Held`: `Fill`/`Retire`, `Class::StandIn` and the stand-in builders are deleted at the cut-over (§7), and a wanted tile that is not drawable is shown by the draw set's roles. Note for the review: by the definitions in `decisions.rs`, `Role::Held` is the finer descendants kept on a zoom-out, while the stand-ins' job today — coarser ground under a tile not yet drawable — is `Role::Fallback` under the fallback pass; both roles together replace them. `tuile-wgpu/tests/stand_ins.rs` and `no_two_surfaces.rs` must keep proving that the fallback pass never paints over drawn siblings |
| **D4** | Later | Imagery texture arrays or atlases (phase 2) |
| **D5** | Open | iOS budget fractions, credit size, imagery-over-terrain cap (3 proposed). Profile values found in Phase 3, accepted 2026-10-20 for the cut-over: the desktop reads its local store 32 at a time (`max_local_reads`; at 8 a warm start converged later than a cold one, P1.3); the ground pool's floors and ceilings (D-A3) |
| **Skirts** | Deferred | Skirts are deferred (see the header) |
| **D-A3** | Decided 2026-10-20 | The ledger is pooled: terrain and imagery share one pool, terrain between 25 % and 60 % of it, imagery between 40 % and 75 %, one eviction sweep over both (architecture §7). The fixed per-class split goes; the pooled policy is the only one |
| **D-A0…D-A8** | Decided 2026-10-19 | `graphics-budget-architecture.md` §6: one arbiter; I1 extended; imagery its own content; a pooled ledger simulated in Phase 3; per-layer drapes; the budget with `GeometryServer`, which runs where its budget's inputs are (locally on interactive devices, in a `tuile-server` session for a declared budget); side channels folded into the stream; `ViewState::from_fov` and the incremental decide for P6; the executor's `FrameDeadline` |
| **Cut-over** | Decided 2026-10-19 | One PR, reviewed in pieces, with no bridge: no flag, no shadow, no adapter between the old protocol and the new |
| **Deadband** | Decided 2026-10-19 | The refine deadband of #86 (`1ba0d77`, what the last pass *chose*) stays in the traversal; the budget does not re-decide the cut |
| **D-T1** | Open | The transport of a `tuile-server` session: WebSocket in v1 (docs/12); under discussion, QUIC (`quinn`) / WebTransport first (multiplexed streams: priorities and a cancel per content, no TCP head-of-line blocking behind a 22 MB drape, connection migration between Wi-Fi and cellular), shared-memory IPC on the same machine (zero-copy glb), WebSocket as the fallback; Workers keep the static mode (WebTransport on Workers to verify). The protocol stays transport-agnostic |
| **D-L1** | Proposed | Screen-space thresholds in logical pixels, so a scale change alone changes no need (§13.2, P5.5) |
| **D-P1** | Decided 2026-10-19, on evidence | The handheld convergence bound is set from device measurements: the L5 iPhone sessions, recorded in MCAP (the session tape, §16.6), which also calibrate the simulation's network model. Until then the 4 s test stays red and ignored (§11.7 measured 6.4 s against the model's floor of 6.3 s) |

## 11. Amendments from the implementation (Steps 1–3)

The module and its simulation (`feat/budget-core`) changed these points. Each was forced by
a test that failed, mostly a livelock the simulation found. Spec and code now agree.

### 11.1 The protected set
- **The drawn cut's ancestor chain is protected** (`Protection::Chain`). The traversal
  refines a tile only through resident ancestors, so evicting a refined parent made the cut
  flip back and forth at a tight budget, each step re-requesting what the last one evicted.
  The chain costs about a third of the frontier, as the reference engines keep it.
- **The drawable half of a swap in progress is protected** (`Protection::Swap`): a missing
  tile's drape waiting for its mesh, a held parent's resident children waiting for their
  siblings. Evicting either restarted the swap for ever.
- **Blockers are all the children a hold waits on, resident or not.** Listing only the
  missing ones made the resident siblings unwanted, hence evictable: the same livelock. The
  traversal must list them so (Step 4).

### 11.2 Hard limit and admission
- `hard_limit` per class, set by the profile from the device: the guard against a jetsam
  or out-of-memory kill. No delivery ever crosses it, whatever its urgency.
- `hard_reserve_percent` (15 %) is kept for swaps that shrink the set: a coarser parent
  replacing its drawn children may use it, nothing else. Urgent work stops at
  `hard_limit − reserve`, non-urgent work at the target, preload at the low watermark.
- **Admission preempts lower-value residents rather than refusing work** (checklist C15):
  to fit, a delivery evicts unprotected residents worth less (unneeded, or wanted at a lower
  urgency). Preload never displaces anything wanted, and is not fetched when the decoded
  tier could not keep it.

### 11.3 Requests
- `load_timeout` (4 s interactive, 120 s Batch) is the single owner of load timeouts: a load
  unanswered that long is cancelled and retried with backoff (Batch: the session ends).
  Transport-level timeouts are removed at the cut-over, or kept at least as long (§5).
- An urgent request (Floor, Hole) takes a slot from in-flight Ring, Ancestor or Refresh
  work of its class; nothing else is preempted.

### 11.4 Imagery
- The footprint is relaxed with `sse_scale`: under pressure imagery coarsens as terrain
  does. Without it the low clamp (160 MiB) never converged.
- When the level asked is held nowhere locally, a level held on the GPU, decoded or in a
  store, within one level of the window between the relaxed and the unrelaxed need, is
  taken instead: a relaxation never sends to the network what a local tier can show (I15).
  Batch takes the exact level, with no hysteresis, so a frame is a function of its inputs.
- A finer drape already worn is kept; under pressure the downgrade wins.
- The decoded tier releases copies of protected GPU residents first, then of evictable
  ones, then demoted ones, so an eviction demotes rather than drops.

### 11.5 Time and determinism
- Time is `Micros(u64)`; no float enters a decision. The imagery level uses only
  `+ − × ÷ √` and exact powers of two. The simulation's inputs avoid `libm` as well, so the
  golden decision digests hold on every platform, checked on the Linux CI.

### 11.6 The zoom-out-at-the-ceiling scenario
The review asked for a zoom-out at the hard ceiling where the needed parent is **not
resident**, asserting that the children stay drawn until it is. With the chain protected
(§11.1), that state cannot arise in the interactive traversal: every parent of the drawn cut
is resident by construction, which is the stronger guarantee. The simulation scenario
therefore asserts that guarantee at every step of a zoom-out on a small phone at its ceiling
(every drawn tile's parent drawable), that no class crosses its hard limit, that nothing is
lost, and that the budget settles, with nothing churning, within 10 s. The held-children
path itself (a parent missing, then wanted on a zoom-out) is covered by the unit tests
`on_zoom_out_the_children_stay_until_the_parent_is_drawable` and
`a_coarsening_parent_uses_the_reserve_at_the_ceiling`.

### 11.7 Measurements that bear on Step 6
- A cold phone converges 6.4 s after the camera stops. The simulated cellular network allows
  6.3 s at best (740 loads of about 0.24 s each, 8 + 8 in flight). 4 s is not reachable
  under that model: the target or the quotas must change.
- On a desktop back-and-forth the Drape class is full with the view alone (+3 / 2048²
  drapes, about 22 MB each): about 1 200 drapes come back from the store in 48 s, none from
  the network.
- Per step, `decide` costs about 0.6 ms (release, about 500 drawn tiles): O(view · log N),
  not yet the O(changes · log N) of I6. An incremental need is the follow-up.

## 12. Motion patterns (Phase 1, 2026-10-19)

The budget is proven pattern by pattern. A pattern is a family of camera scripts with
obligations beyond I1–I18, each stated as a predicate over the per-step decisions, the
ledger and the draw set, with a time bound and the profiles it runs under. The test plan
names the test that checks each one, per level and profile.

### 12.1 Notation

A run is a camera script, a profile π (§14) and a seed, stepped at k = 0, 1, … at times
t_k. The step is the decision step: 1/60 s on a desktop or a phone, the display period on a
headset.

| Symbol | Meaning |
|---|---|
| N_k | the `NeedSet` the traversal built from the step's views, every eye included |
| D_k, R_k | the step's `Decisions`, and its `DrawSet` `D_k.draw`; an item x has `mesh(x)`, `role(x)`, `drapes(x)`, `serves(x)` |
| G_k | the resources acknowledged drawable when step k decides (R_k is drawn from G_k only) |
| F, k_F | the floor: every mesh and every drape, every layer, at a level ≤ `pinned_level`; k_F is the first step with F ⊆ G_k |
| want(N) | the resources a need asks for: every listed tile's mesh, and the drape each draped tile needs, per layer |
| L_k[c], P_k[c] | `D_k.stats.gpu[c]` and `D_k.stats.protected[c]` |
| T[c], O[c], H[c] | the effective target, overflow and hard limit of class c, after pressure and `retarget` |
| V_k^e | the tiles inside eye e's frustum at step k |
| covered_k(t) | t or an ancestor of t is drawn in R_k, or every visible child of t is covered, three levels down at most (the simulation's `covered`) |
| dressed_k(x) | for every layer ℓ the needs of serves(x) ask for, drapes(x) holds a choice for ℓ whose resource is in G_k |
| lvl(x), img_ℓ(x) | the level of mesh(x); the imagery level of x's drape for layer ℓ |
| need_ℓ(t) | the imagery level t needs for layer ℓ, unrelaxed and clamped (§4) |
| still[k0, k1] | the views, the lenses, the pressure and the thermal state constant over the steps k0 to k1 |
| conv(k) | `D_k.stats.converged` and N_k.blockers empty (the simulation's `StepLog::converged`) |
| k_stop | the first step of the script's final still interval |
| T_conv(π) | the convergence bound of I3 for the profile (§14) |
| k + T | for a duration T, the step T later (T / dt steps) |

"Every step" means every step from k_F on, unless said otherwise.

### 12.2 P1 — Progressive loading

Scripts:
- **P1-cold**: the camera still at `home`, from an empty GPU, with no local store;
- **P1-warm**: the same after a relaunch, the local store holding what an earlier session
  loaded;
- **P1-low**: P1-cold at the handheld low clamp (160 MiB);
- **P1-approach**: the existing five-second `approach`, then still.

Profiles: desktop, handheld, handheld-low, web, headset, batch (a film's first frame).

| | Obligation | Predicate | Bound |
|---|---|---|---|
| P1.1 | The floor first; coarse ground before any refinement | (a) in every `D_k.start` and every `D_k.upload_plan.items`, each floor resource comes before every other resource; (b) for every k, from k = 0: if R_k holds an item with lvl(x) > `pinned_level`, then every t in N_k.drawn is covered_k and every item of R_k is dressed_k — no area refines while wanted ground anywhere is lost or bare | k_F ≤ T_F(π) = the larger, over the mesh and drape classes, of ⌈#F_c / max_in_flight[c]⌉ × the latency the slowest of the floor's #F_c loads of c stays under 95 % of the time (amended 2026-10-20: not one load's p95, which the slowest of 21 exceeds two times in three; for the simulation's exponential jitter, base + ln(#F_c / 0.05) × the mean jitter + the transfer); plus ⌈bytes(F) / upload_bytes_per_frame⌉ steps, plus 2 steps. With no host stalled; a failed floor load adds its backoff |
| P1.2 | Refinement is monotone while still | over any still[k0, k1] in which `sse_scale_milli` does not rise: for every x in R_{k−1} drawn `Exact`, every item covering x's ground at step k has lvl ≥ lvl(x) and, per layer, img_ℓ ≥ img_ℓ(x). No ground goes coarser, terrain or imagery | every step of the interval |
| P1.3 | Convergence within the target | conv(k) holds for some k ≤ max(k_stop, k_F) + T_conv(π) and from then to the end of the still interval. P1-warm also: no network start for a resource the store holds (I14), and it converges no later than P1-cold | T_conv(π) |
| P1.4 | Network work in priority order | (a) `D_k.start` is sorted by `PriorityKey`; (b) for each class c, a network job left unstarted at step k while in_flight(c) < max_in_flight[c] implies that no job of class c with a larger key started at k — nothing waits while a slot is free, apart from the preemption of §11.3; (c) local reads run under `max_local_reads` and never wait behind a network job (I15) | every step |

While the relaxation rises (P1-low), P1.2 is suspended: ground coarsens, but only through
I9's swaps. It applies again once `sse_scale_milli` has been constant for one second.

### 12.3 P2 — Zoom in

Scripts: cold (empty GPU) and warm (after a zoom-out and back); slow (height halved every
second) and fast (height divided by ten every second); from 200 km down to the deepest
level of the region (mountains: terrain level 15; plain: terrain 11, imagery 17).

Profiles: desktop, handheld, handheld-low, web, headset, batch (a film's dive).

| | Obligation | Predicate | Bound |
|---|---|---|---|
| P2.1 | A refining parent's children appear together, each dressed | for every k and every t in N_{k−1}.drawn that is not in N_k.drawn while some of its children are: every such child is drawn `Exact` and is dressed_k at step k. Until then t stays drawn (I9) | every step |
| P2.2 | No request for a level the view has left | (a) every `Start` of D_k is in want(N_k), or is the `Refresh` of a drawn resource; (b) a load whose resource has been out of want(N) since time t is cancelled by t + `cancel_grace` + one step (I5); (c) the wasted starts, whose resource left want(N) before it was delivered, are counted and goldened per script | (a) every step; (b) the grace |
| P2.3 | Imagery sharpens independently of terrain | (a) no `Start` of `Mesh(t)` while Mesh(t) is in G_k or decoded: a drape change never reloads its mesh; (b) once converged, for every exact t and layer ℓ, abs(img_ℓ(t) − need_ℓ(t)) ≤ 1 (I10), and over the plain some t has img_ℓ(t) > lvl(t) | (a) every step; (b) at convergence |
| P2.4 | Convergence at the bottom | conv within T_conv(π) of the camera stopping at the deepest level | T_conv(π) |

Measured and goldened, without a bound until the first green run sets one (Phase 3,
reviewed): the deepest fallback climb during the fast zoom, the largest lvl(t) − lvl(cover)
over the steps and the wanted tiles.

### 12.4 P3 — Zoom out

Scripts: to 200 km and back (the existing `zoom`); slow and fast (height multiplied by ten
every second); at the hard ceiling (handheld-low, its terrain class at 40 MiB, the existing
scenario); a session that starts low and climbs, cold.

Profiles: desktop, handheld, handheld-low, web, headset, batch.

| | Obligation | Predicate | Bound |
|---|---|---|---|
| P3.1 | Parents become drawable before children go | for every k: if `Mesh(u)` leaves R_k or is evicted at k, and u is a descendant of some t in N_k.drawn, then Mesh(t) is drawn `Exact` and dressed_k at step k; until then u stays drawn `Held` (I9, coarsening). And every drawn tile's parent is drawable: the protected chain (§11.6) | every step |
| P3.2 | Bytes come back under the target | there is a step k* ≤ k_stop + T_conv(π) such that L_k[c] ≤ T[c] for every class c and every k ≥ k*; before k*, L_k[c] ≤ max(T[c] + O[c], P_k[c]) (I2) | T_conv(π) after the stop |
| P3.3 | No deadlock at the ceiling | L_k[c] ≤ H[c] at every step; a coarsening parent is delivered from the reserve (§11.2); the budget settles within 10 s of the stop: one simulated second with nothing started, cancelled, delivered, uploaded or evicted and nothing in flight | 10 s |
| P3.4 | Back down, sharp again | after a zoom-out and back, conv within T_conv(π), with `underdraped == 0` (S3) | T_conv(π) |

### 12.5 P4 — Rotation

Scripts: yaw in place (180° in one second; a full turn at 180°/s; head rate, 300°/s); an
orbit around a ground target (5 km radius, one turn in 20 s, then in 10 s); a tilt (pitch
from 30° to 85° below the horizon, and up to 5° below it, at 30°/s); turn and back (180°, a
pause of 0.3 s, 2 s or 8 s, and back).

Profiles: desktop, handheld, web, headset (head rate).

| | Obligation | Predicate | Bound |
|---|---|---|---|
| P4.1 | Uncovered ground is filled from the ring, then the floor | (a) every t newly in N_k.drawn is covered_k and dressed in the step it appears; (b) while the yaw stays within the ring's angular margin of a pose held still for at least T_conv(π), newly visible ground is covered within 3 levels of its need (the ring's depth, SSE ≤ 8τ); beyond the margin, by the floor at worst; (c) the requests of newly visible tiles rank `Hole`, above `Drape`, `Ring`, `Ancestor` and `Refresh` | (a), (c) every step |
| P4.2 | Nothing still visible is evicted | for every k and r in D_k.evict: if r = Mesh(t), then t is in neither N_k.drawn nor N_k.blockers; if r is a drape, no item of R_k wears it and no t in N_k.drawn needs it | every step |
| P4.3 | No flapping when rotating back | turn and back: (a) I4 in every still interval; (b) a load whose tile comes back within `cancel_grace` is kept; (c) desktop: thrash = 0, no network fetch of a resource evicted less than 10 s before; handheld: the return's network bytes are at most a quarter of the first pass's (the bound of `back_and_forth_reuses_what_is_held`) | the run |
| P4.4 | An orbit keeps its target exact | from the orbit's first convergence on, the tile under the centre of the view stays drawn `Exact`, with abs(img_ℓ − need_ℓ) ≤ 1, at every step | every step after conv |
| P4.5 | Convergence after the stop | conv within T_conv(π) of any yaw, orbit or tilt stopping, head rate included | T_conv(π) |

### 12.6 P5 — Orientation change

Scripts: portrait to landscape and back on a handheld (1170 × 2532 to 2532 × 1170 physical
pixels, scale 3); a desktop window resized (1920 × 1080, then 960 × 1080, then 3840 × 2160);
a scale-factor change at constant logical size (2 to 1: a window moved between displays);
split screen (full width, then half, then a third); ten rotations in 20 s.

Profiles: desktop (resize, scale), handheld and handheld-low (rotation, split screen), web
(a browser window resized, a `devicePixelRatio` change).

k_r is the first step whose views carry the new lens.

| | Obligation | Predicate | Bound |
|---|---|---|---|
| P5.1 | No frame with a hole across the change | from k_r on: I1 against the new lens; and no x in R_{k_r−1} whose tile is still inside the new frustum is evicted at k_r: the old draw set stays until the new frustum is covered | every step from k_r |
| P5.2 | The new need is the new viewport's, from the same step | N_{k_r} is computed from the lens of step k_r. On a host, the `ViewerState` sent for the first frame presented at the new size carries the new viewport (the `Resized` path) | the step of the change |
| P5.3 | No burst beyond the new view's need | from k_r to k_r + T_conv(π): every start is in want(N_k) and held in no local tier (I14); no resource starts twice without a failure in between; the bytes started never exceed the new view's need not already held, ring included | T_conv(π) |
| P5.4 | Repeated rotations reuse what is held | once the first cycle has converged, cycles 2 to 10 start no network load for a resource either orientation needed, given a local store; with no store, thrash = 0 on the desktop and the handheld | the run |
| P5.5 | A scale change alone changes nothing the budget sees | at constant logical size, the `NeedSet`s before and after a scale change are equal, bit for bit (§13.2, D-L1) | the step of the change |
| P5.6 | Convergence after the last change | conv within T_conv(π); a split-screen shrink also meets P3.2 | T_conv(π) |

### 12.7 P6 — Stereo (VR)

Scripts: two eyes 63 mm apart, then hyperstereo (6.3 m, then 630 m); asymmetric frusta
(§13.1); head motion at 72, 90 and 120 Hz: still with sensor jitter (±1 mm, ±0.05°),
looking around (yaw ±30° at 0.5 Hz), flicks at 300°/s, walking (0.5 m/s, 2 m above the
ground).

Profiles: headset at 72, 90 and 120 Hz; desktop with two eyes (hyperstereo); batch (a stereo
film, `tuile-film`'s `Stereo::eyes`).

| | Obligation | Predicate | Bound |
|---|---|---|---|
| P6.1 | A pair costs its union (I16) | the run with views [L, R] and the run fed, step for step, the single need that is the union of the two eyes' needs (§13.3) give the same digest stream and the same ledger; each step starts only resources of want(N_L ∪ N_R), none twice | every step |
| P6.2 | Both eyes from one decision (I17) | every render frame, both eyes draw the same R_k (one step, one generation); for every eye e and every t in N_k.drawn inside V_k^e, covered_k(t), and its covering items are dressed_k | every frame |
| P6.3 | Uploads fit the display (I18) | every render frame, the bytes uploaded are at most `FrameDeadline::bytes` for the display rate, except a lone first item; deferrals are counted; I1 and P6.2 hold whatever the deferral pattern | every frame |
| P6.4 | Decisions stay fresh (D-A7) | `decide`'s work counter W_k (resources visited, a deterministic count, §16) is at most a · (#ΔN_k + #E_k) · log2(tracked + 2) + b, where ΔN_k is the tiles whose need entry changed since step k − 1 and E_k the events applied; a and b are set in Phase 3 and kept. On the device, `decide`'s p99 stays within the profile's share of the frame (§14) | every step |
| P6.5 | Convergence after the head stops | conv within T_conv(headset) | T_conv(headset) |

## 13. The lens: viewport, field of view, scale (Phase 1, 2026-10-19)

The lens replaces the fixed focal length — the simulation's `FOCAL`, the symmetric-only
`ViewState::perspective`, the circular cull cone of `world.rs`, which ignores pitch — in the
need and in the texel footprint.

### 13.1 Per eye

A lens is, per eye:
- the viewport in physical pixels, `width_px` × `height_px`;
- the scale factor `scale`, physical pixels per logical point (1, 2, 3; a browser's
  `devicePixelRatio`);
- the field of view as four angles in radians, OpenXR's convention: `left` and `down`
  negative, `right` and `up` positive. A symmetric lens is `Fov::symmetric(fovy, aspect)`;
  a headset's eyes are asymmetric, and the runtime gives them every frame.

A mono view is one eye, a stereo rig two eyes with their own poses, a multi-viewport host N.

Until a runtime gives real ones, the simulation's headset uses representative values, not
a device's: 2000 × 2000 px per eye at scale 1; the left eye `left −52°, right 44°, up 47°,
down −53°`, the right eye mirrored.

### 13.2 What the lens changes

- **Focal**, in logical pixels per unit of tangent, on the sharper axis so that neither axis
  is under-refined:
  `focal = max(width_px / (tan right − tan left), height_px / (tan up − tan down)) / scale`.
  For a symmetric lens with square pixels at scale 1 it is today's
  `height / (2 tan(fovy / 2))`, so a film's terrain selection is unchanged at scale 1.
- **SSE**: `geometric_error × focal / distance`, against τ in logical pixels.
- **Texel footprint**: `TexelFootprint::new(distance, cos_obliquity, focal, texel_l0,
  target)` takes this focal (it already takes a `focal_px`); the target is in logical pixels
  per texel.
- **Culling**: the four planes of the angles, widened by the cull margin (`CULL_MARGIN`
  today), with pitch and roll: a frustum, not a cone, so a tilt and a portrait screen select
  what they show.
- **Logical pixels (D-L1, proposed).** The thresholds are in logical pixels, so a scale
  change at constant logical size changes no need (P5.5), and a high-density display does not
  ask four times the bytes for the same picture. The alternative, physical pixels, makes the
  need follow the scale and the budget pay for it. To confirm at the review. The current
  viewer's convention is read at the cut-over; if it is physical, the change on the desktop is
  reported and reviewed as the film's imagery change is.

### 13.3 The union of the eyes

The traversal takes every eye (`traverse(&[ViewState])` is a union already) and gives one
`NeedSet`. A tile some eye wants is listed once, under its most urgent membership, with the
largest `sse_excess`, the largest `centrality`, the smallest `distance` and the finest
imagery footprint (the largest ratio) over the eyes that see it. The budget never sees an
eye. The executor draws the one `DrawSet` once per eye, culled by that eye's frustum (I17).

## 14. Platforms (Phase 1, 2026-10-19)

| Platform | Profile | Memory, pressure, thermal → budget | Display and deadline | Lens | `GeometryServer` and budget | Presence oracle |
|---|---|---|---|---|---|---|
| **Desktop** (Mac; Linux in CI) | `Profile::desktop`, Interactive | `retarget` from the GPU's working set (Metal's `recommendedMaxWorkingSetSize`, `VK_EXT_memory_budget`); no pressure; thermal nominal | 60–120 Hz, `FrameDeadline` from the display | one eye; the window; scale 1–2; resizes, scale changes | in-process, the server task | foyer index, HTTP disk cache |
| **Handheld** (iPhone; iOS simulator, normal and low memory) | `Profile::handheld`, Interactive; low memory is `handheld_low`'s 160 MiB clamp, by injected device memory (a tuning value) | `retarget` from `os_proc_available_memory` (`device.rs`); a memory warning → `Pressure::Warning`; the background → `Pressure::Background`; `thermalState` → `set_thermal` | 60 or 120 Hz | one eye; portrait and landscape; scale 3; split screen on iPad | in-process | foyer index |
| **Android** (emulator; phones) | `Profile::handheld`, Interactive | `retarget` from `/proc/meminfo` (`memory_from_meminfo`); `onTrimMemory` levels → `Pressure` (`pressure_of_trim`); thermal status (`thermal_of_status`) | 60–120 Hz | one eye; rotation; scale 2–3.5 | in-process | foyer index |
| **Web** (WebGPU, WebGL2) | `Profile::browser`, Interactive, within the worker's wasm32 heap | `navigator.deviceMemory` → `Retarget` (D-A6); no pressure signal: a hidden tab is the host's `Pause`; no thermal | `requestAnimationFrame` at the display rate; the deadline is the page's; a decision arrives one message late | one eye; the canvas; `devicePixelRatio` | the worker (`tuile-viewer/src/web/worker.rs`); the page executes | an in-memory index of the browser's store (an IndexedDB read is asynchronous, not a presence query) |
| **Headset** (Android-based, stereo) | `Profile::headset` (new): a handheld's memory, a desktop's network (Wi-Fi), Interactive | as Android | 72, 90, 120 Hz; one `FrameDeadline` per eye pair; `decide`'s p99 within a quarter of the frame (proposed) | two eyes, four asymmetric angles from the runtime | in-process | foyer index |
| **Batch** (films, bakes, Hydra) | `BudgetConfig::batch()` | none; `release_beyond` between frames | none: a frame is drawn at `frame_ready` | the film's cameras; `Stereo::eyes` for a stereo film | in-process in `tuile_bake::Session`; Hydra through the C ABI | the bake's store |
| **Declared budget, remote** (a USD/Cycles render node, a farm, a simulator cluster's image generators, a consumer that cannot link the core) | Batch or Interactive, from what the consumer declares | the declared memory (`Retarget`), the consumer's `Ack` bytes; no pressure signal | the consumer's; a batch or a LAN tolerates the round trip | the consumer's views | a `tuile-server` session (D-A5; transport D-T1) | none, or `Held` (open) |

T_conv(π), I3's bound per profile: desktop 2 s; handheld 4 s, red today (6.4 s measured
against a network floor of 6.3 s, §11.7), its bound to be set from the iPhone sessions
(D-P1); handheld-low 4 s (3.3 s measured,
architecture document §3.2); web, its device's bound plus one step; headset 2 s on the
desktop network model (proposed); batch, not timed: a frame waits for `frame_ready`.

## 15. The API decided on 2026-10-19 (target signatures)

From the architecture document's §5 as amended by D-A5. Where §3 or §5 differ, this section
wins. The cut-over lands it in one PR (§10).

### 15.1 Budget (`tuile_core::budget`)

```rust
pub struct LayerId(pub u16);

pub enum ResourceId {
    Mesh(TileId),
    Drape { tile: TileId, layer: LayerId, level: u32 },  // was Drape(TileId, u32) (D-A4)
}

pub enum Class { TerrainMesh, Tiles3dMesh, Drape, Overlay }   // StandIn goes with the stand-ins (D3)

pub struct LayerReach { pub layer: LayerId, pub max_level: u32 }
pub struct ImageryNeed { pub footprint: TexelFootprint, pub layers: Vec<LayerReach> }  // was max_level

pub enum Role { Exact, Held, Fallback }      // unchanged; together they replace the stand-ins (D3)
pub struct DrapeChoice { pub resource: ResourceId, pub layer: LayerId, pub level: u32, pub uv: UvTransform }
pub struct DrawItem {
    pub mesh: ResourceId,
    pub role: Role,
    pub drapes: Vec<DrapeChoice>,   // one per layer the need asks for, in stacking order (was drape: Option<_>)
    pub serves: Vec<TileId>,
}

impl Budget {
    pub fn new_epoch(&mut self, class: Class, layer: Option<LayerId>);  // per layer (was new_epoch(class))
    // unchanged: new, decide, on_loaded, on_failed, on_store_miss, on_drawable, on_discarded,
    // set_pressure, set_thermal, retarget, release_beyond, residency, resource, ledger, audit
}

/// The executor's upload cap per render frame (I18), derived from the display, outside `decide`.
pub struct FrameDeadline { pub bytes: Bytes, pub time: Micros }
impl FrameDeadline {
    pub fn from_refresh(hz: u32, upload_share_permille: u16, bytes_per_ms: u64) -> Self;
    /// The prefix of `queue` this frame uploads after `done` bytes: the first item always goes,
    /// and a swap group of `groups` goes whole when it fits, or waits whole (amended 2026-10-20:
    /// the plan's swap groups are an argument, since a group cannot go whole without them).
    pub fn take<'p>(&self, queue: &'p [ResourceId], groups: &[SwapGroup], done: Bytes,
        cost: impl Fn(ResourceId) -> Bytes) -> &'p [ResourceId];
}
```

- `BudgetConfig::interactive`, the pooled ground (D-A3, decided 2026-10-20): terrain and
  imagery share one `Pool` of 82 % of the device total (the 80 % they had, plus `StandIn`'s
  2 %) — its target, overflow and hard limit — each member between a floor (percent of the
  pool: terrain 25 %, imagery 40 %) and a ceiling (its own `target`, `overflow`,
  `hard_limit`: terrain 60 %, imagery 75 % of the pool's). A member's effective target is
  what the pool leaves once the other members' bytes are counted, between its floor and its
  ceiling; one eviction sweep runs over the members in the one eviction order, no member
  below its floor, none left above its ceiling; the relaxation reads the pool's protected
  bytes. 3D Tiles and overlays keep their own targets.
- `BudgetStats` gains the fields the observability contract needs (§16): `bare`, the
  per-class split of drawable and in-transit bytes, the work counter.
- `FrameDeadline` is in the core so that every executor — the wgpu pump, the web page, the
  USD scene index — applies one rule. `decide` never reads it.

### 15.2 Protocol (`tuile_core::protocol`)

```rust
pub enum ClientMessage {
    ViewerState { views: Vec<ViewState>, generation: u64 },
    Ack { resource: ResourceId, gen: Generation, bytes: u64 },
    Discarded { resource: ResourceId, gen: Generation },
    Pressure(Pressure),
    Thermal(Thermal),
    Retarget { total_bytes: u64 },
    ReleaseBeyond { bytes: u64 },                    // Batch, between frames
}
pub enum ServerMessage {
    Select { draw: DrawSet, ancestry: Vec<(TileId, Ancestry)>, stats: TraversalStats, generation: u64 },
    Content { resource: ResourceId, gen: Generation, ancestry: Ancestry, content: ResourceContent },
    Cancel { resources: Vec<(ResourceId, Generation)> },   // delivered, not yet uploaded: drop it
    Evict { resources: Vec<ResourceId> },                   // after the draw that stops using them
    Error { resource: Option<ResourceId>, message: String },
    Priming(Priming),
}
pub enum ResourceContent {
    Mesh(TileContent),      // DecodedTileContent without `imagery` and `withheld_drape`
    Drape(DrapeContent),    // one texture, its rect, its layer and level, keyed by raster::drape_identity
}
```

- **Removed:** `ClientMessage::Ack { tile }`; `ClientMessage::Cancel { tile }` (cancellation is
  the budget's, from the need); `ClientMessage::Trim` (`ReleaseBeyond`);
  `ServerMessage::Fill` and `Retire` (D3).
- **Open, not decided:** `ClientMessage::Held { resources }`, what a remote consumer already
  holds in local tiers. Needed only for a remote session whose consumer has local tiers (I14
  then needs the session to know them); optional for a declared-budget consumer (D-A5).
- **Never added:** §5's `Credit { bytes }`: the budget counts in-transit bytes itself until
  `Ack` or `Discarded`, whatever the transport.
- **Host messages (D-A6):** `ToWorker::Detail` is deleted; `ToWorker::Budget { reported }`
  becomes `ClientMessage::Retarget`; `ToWorker::Pause` and the layer switch,
  `ToWorker::Imagery`, stay host messages for now.
- **Where it runs (D-A5).** In-process or across a worker on an interactive device; across
  the network to a consumer that declares its budget, whose `GeometryServer` runs in a
  `tuile-server` session — its budget fed by the declared memory (`Retarget`) and by `Ack {
  resource, gen, bytes }`. glb on that wire, as docs/12 says.
- **The transport is open (D-T1, §10).** The protocol stays transport-agnostic.

### 15.3 Views (`tuile_core::traversal`)

```rust
pub struct Fov { pub left: f64, pub right: f64, pub up: f64, pub down: f64 }  // radians, OpenXR's signs
impl Fov { pub fn symmetric(fovy_rad: f64, aspect: f64) -> Fov; }
impl ViewState {
    pub fn perspective(position: DVec3, direction: DVec3, up: DVec3, viewport_px: DVec2, fovy_rad: f64) -> Self; // unchanged
    pub fn from_fov(position: DVec3, direction: DVec3, up: DVec3, viewport_px: DVec2, fov: Fov) -> Self;          // D-A7
    pub fn with_scale(self, scale: f64) -> Self;   // physical pixels per logical point; 1 by default
    pub fn focal_px(&self) -> f64;                 // §13.2
}
```

`ViewStateParams`, the serialised form of a view (the wire, the tape), carries a symmetric
perspective at scale 1 only; at the cut-over it gains the four angles and the scale, so an
asymmetric eye and a logical-pixel lens cross a wire and a tape (recorded 2026-10-20).

The architecture document writes `from_fov(position, orientation, …)`. The orientation is
the pair `direction`, `up` that `perspective` already takes; a host turns an OpenXR pose's
quaternion into it.

### 15.4 Executor (tuile-wgpu's pump, the web page, the USD scene index)

- One upload queue in the plan's priority order, keyed by `ResourceId` and generation,
  drained every render frame through `FrameDeadline::take`, never past a class's hard limit.
- `FrameDeadline::from_refresh(hz, share, bytes_per_ms)`: the frame period × the upload share
  × the measured upload throughput. The throughput is the executor's own measure and may
  vary; it never enters `decide` (I18).
- It draws the `DrawSet`: each item's mesh with one drape per layer through its UV
  transform. The wgpu path binds the layers as passes; the USD executor composes them once
  every layer of the item is drawable (D-A4).
- It acks with the actual bytes, sends `Discarded` for a failed upload, drops on `Evict`
  after the draw that stops using a resource, and on `Cancel` before the upload.
- It reports what it deferred, its upload bytes and times, and per-eye coverage to the
  observer (§16), never to the budget.

### 15.5 Decided alongside

- **Stand-ins (D3)** become the budget's roles: `Fill`, `Retire` and `Class::StandIn` go
  (§10, with the note on `Held` and `Fallback`).
- **The refine deadband stays in the traversal.** Since #86 (`1ba0d77`) it reads what the
  last pass chose at its own level, not what stood in, and moves only when the camera moves.
  The budget keeps only §4's imagery hysteresis and its relaxation; it does not re-decide
  the cut.
- **One cut-over PR, no bridge.** The new path replaces the old one in one PR, reviewed in
  pieces: no flag, no shadow, no adapter between the old protocol and the new.

## 16. Observability contract (Phase 1, 2026-10-19)

The budget is tested and watched through the same signals, so that it can change one piece
at a time: a change shows in production the way it showed in the test that proved it.

### 16.1 Principles

- **One vocabulary.** Every invariant and every obligation maps to at least one named signal
  (§16.3, §16.4). Tests assert through those signals wherever they can; dashboards and
  alerts read the same names. A test that needs a measure the signals lack adds a signal,
  not a private counter.
- **One observer, many sinks.** The budget computes nothing for a sink. A `BudgetObserver`
  in the core receives each step's record; the sinks — Prometheus, the MCAP session tape,
  tracing, the control API, the overlay, the web worker's channel — read what it
  accumulated. There is no sink-specific logic in the budget or in the observer.
- **Deterministic first.** Counts and digests come from the decisions and are the same on
  every platform. Wall-clock measures (`decide` seconds, upload seconds) are named as such and
  never feed a decision.
- **Bounded cardinality.** Labels take their values from closed sets (§16.5); no tile,
  resource or session id is ever a label. Per-tile detail goes to the MCAP tape.
- **No coordinates in the field.** As `journal.rs` already holds, field telemetry records the
  budget's signals, never a position or a credential (§16.9).

### 16.2 The observer (core: no I/O, no allocation per step beyond bounded maps, wasm32)

```rust
/// Watch-only, called by the host; takes nothing back (the pattern of tuile-film-native's `Observer`).
pub trait BudgetObserver {
    /// After each `decide`: from `GeometryServer`, the simulation, a Batch session.
    fn step(&mut self, _s: &StepRecord<'_>) {}
    /// After each render frame: from the executor (the pump, the page, the USD scene index).
    fn frame(&mut self, _f: &FrameRecord<'_>) {}
}
impl BudgetObserver for () {}
impl BudgetObserver for Vec<Box<dyn BudgetObserver + Send>> { /* each in turn */ }

pub struct StepRecord<'a> {
    pub step: u64,
    pub frame: u64,
    pub now: Micros,
    pub views_changed: bool,           // the host's: some eye's pose or lens moved since the last step
    pub eyes: u8,
    pub need: &'a NeedSet,
    pub events: &'a [BudgetEvent],     // what the host handed the budget since the last step, any order
    pub presence: &'a [(ResourceId, Presence)],  // the answers this step's oracle gave
    pub decisions: &'a Decisions,      // stats included
    pub ledger: Ledger,
    pub pressure: Pressure,
    pub thermal: Thermal,
    pub decide_wall: Option<Micros>,   // measured by the host around `decide`
}
pub struct FrameRecord<'a> {
    pub frame: u64,
    pub generation: u64,               // of the DrawSet drawn
    pub deadline: FrameDeadline,
    pub uploaded: &'a [(ResourceId, Bytes)],
    pub deferred: u32,
    pub upload_wall: Micros,
    pub eyes: &'a [EyeCoverage],       // per eye: wanted tiles in its frustum with no cover, bare items
}

/// The signals, accumulated: atomics in `tuile_core::metrics`, as the engine's other numbers.
pub struct BudgetMeter { /* bounded per-tile maps for flaps and convergence */ }
impl BudgetObserver for BudgetMeter {}
impl BudgetMeter {
    pub fn signals(&self) -> BudgetSignals;          // a snapshot
    pub fn publish(&mut self) -> BudgetSignals;      // the same, the peaks reset
}
/// A snapshot of every signal of §16.3, by the same names: what tests assert on.
pub struct BudgetSignals { /* … */ }

/// The checks one record allows, moved from the simulation's `Sim::check` into the core, so a
/// device counts the violations a test asserts on.
pub fn check(s: &StepRecord<'_>, cfg: &BudgetConfig, out: &mut Violations);
```

- **As built (Phase 3, accepted 2026-10-20).** The meter keeps plain integers and is read as a
  snapshot; the Prometheus sink copies the snapshot into `tuile_core::metrics`'s atomics when
  it publishes (4.2), rather than the meter writing atomics on every step. Durations are kept
  in integer microseconds and converted to seconds at publication; histograms are of
  integers, in power-of-two buckets, so no float decides a bucket.
- `BudgetEvent` is what the host passes to `on_loaded`, `on_failed`, `on_store_miss`,
  `on_drawable`, `on_discarded`, `set_pressure`, `set_thermal`, `retarget`, `new_epoch` and
  `release_beyond`, recorded by the host as it calls them. Their order does not matter: the
  budget applies them canonically (`arrivals_apply_in_a_canonical_order`).
- The presence answers come from a recording wrapper around the `PresenceOracle`.
- `check` covers what one record and the configuration allow: I1 (lost and bare once the
  floor is in), I2 (unprotected bytes over the target, a class over its hard limit), I7 (heap
  over its cap, duplicates in flight), I11 (an evicted resource that the draw set draws or
  wears, or a floor resource), I13 (an eviction or an inexact item in a ready Batch frame).
  What needs the world — coverage of the ground against each eye's frustum, the network's
  truth for I5 and I14 — stays in the simulation and the headless harness.
- `BudgetStats` gains what the signals need: `bare`, `floor_in`, the per-class split of
  drawable and in-transit bytes, evictions and cancels by reason, `wasted_starts`,
  `local_waits`, the work counter `work` (resources visited by the step).

### 16.3 Signals

Prefix `tuile_budget_`. Bytes in bytes, durations in seconds, ratios unitless.

| Signal | Type | Labels | From |
|---|---|---|---|
| `bytes` | gauge | class, state | ledger: drawable and in transit, per class |
| `decoded_bytes` | gauge | — | ledger |
| `protected_bytes` | gauge | class | stats |
| `target_bytes`, `overflow_bytes`, `hard_limit_bytes` | gauge | class | effective, after pressure and `retarget` |
| `over_target_seconds_total` | counter | class | time with bytes over the target (overflow in use) |
| `lost_tiles`; `lost_tile_steps_total` | gauge; counter | — | stats, once the floor is in |
| `bare_items`; `bare_item_steps_total` | gauge; counter | — | stats (D-A1) |
| `drawn_tiles` | gauge | role | stats: exact, held, fallback |
| `eye_lost_tiles`, `eye_bare_items` | gauge | eye | `FrameRecord` |
| `floor_in`; `floor_seconds` | gauge | — | whether the floor is in; from session start to the floor in |
| `fallback_depth_max` | gauge | — | deepest climb from a wanted tile to its cover since the last publish |
| `underdraped_tiles` | gauge | — | stats |
| `imagery_over_terrain` | gauge | gap | stats histogram |
| `converged` | gauge | — | stats |
| `converged_seconds` | histogram | — | from the views going still to convergence |
| `unconverged_seconds` | gauge | — | still and not converged, for how long |
| `converge_bound_seconds` | gauge | — | T_conv of the profile |
| `flaps_total` | counter | kind | a tile's second level change within 10 s while still (I4) |
| `sse_scale` | gauge | — | relaxation |
| `pressure_level`, `thermal_level` | gauge | — | 0–3 |
| `starts_total` | counter | class, source | starts |
| `wasted_starts_total` | counter | class | started, then out of the need before delivered |
| `local_waits_total` | counter | — | a stored resource left waiting while a network job of a lower key started (I15) |
| `cancels_total` | counter | reason | cancels: `grace`, `timeout`, and `preempted` (an urgent load took the slot; added 2026-10-20) |
| `completions_total`; `failures_total`; `stale_results_total` | counter | class; class, kind; — | arrivals |
| `load_seconds` | histogram | — | load latency, from `Micros`, all classes (no class label: the step's statistics count it for all classes; 2026-10-20) |
| `in_flight` | gauge | class, source | stats |
| `request_heap_entries`, `tracked_resources`, `duplicates_in_flight` | gauge | — | stats |
| `decide_seconds`; `decide_peak_seconds` | histogram; gauge | — | wall clock around `decide`; the worst since the last publish |
| `decide_share_seconds` | gauge | — | the profile's share of a frame for `decide` |
| `decide_work`; `decide_work_total` | gauge; counter | — | resources visited, deterministic |
| `evictions_total`; `releases_total` | counter | class, reason; class | GPU evictions; decoded copies released |
| `thrash_total`, `rereads_total` | counter | — | network refetches, and local re-reads, within 10 s of an eviction, all classes (no class label, 2026-10-20) |
| `network_bytes_avoided_total`, `store_misses_total` | counter | — | tiers |
| `upload_plan_bytes` | gauge | — | the step's plan |
| `uploads_total`; `upload_bytes_total` | counter | class | executor |
| `frame_upload_bytes`; `frame_deadline_bytes` | histogram; gauge | — | executor, per render frame |
| `deadline_deferrals_total` | counter | — | executor (I18) |
| `frame_upload_seconds` | histogram | — | executor, wall clock |
| `estimate_drift_exceeded_total` | counter | class | an ack more than 5 % off its estimate |
| `swaps_total`; `swap_gaps_total` | counter | kind; — | swaps completed; an old half gone before its new half was drawn (must stay 0) |
| `batch_steps_per_frame`; `batch_fatal_total` | histogram; counter | — | Batch |
| `invariant_violations_total` | counter | invariant | `check` |
| `rejected_inputs_total` | counter | — | non-finite need entries |

The decision digest and the stats digest are not metrics: they are the MCAP tape's and the
`det!` trace's (§16.6).

### 16.4 Every invariant and obligation has a signal

| | Signals |
|---|---|
| I1 | `lost_tile_steps_total`, `bare_item_steps_total`, `invariant_violations_total{invariant="I1"}` |
| I2 | `bytes`, `protected_bytes`, `target_bytes`, `overflow_bytes`, `hard_limit_bytes`, `over_target_seconds_total`, violations I2 |
| I3 | `converged`, `converged_seconds`, `unconverged_seconds` |
| I4 | `flaps_total` |
| I5 | `cancels_total{reason}`, `stale_results_total`, `wasted_starts_total` |
| I6 | `load_seconds`, `decide_seconds`, `decide_work`, `tracked_resources` (slopes) |
| I7 | `request_heap_entries`, `tracked_resources`, `duplicates_in_flight`, violations I7 |
| I8 | `pressure_level`, `thermal_level`, `target_bytes`, `sse_scale`, `lost_tile_steps_total` |
| I9 | `swaps_total{kind}`, `swap_gaps_total`, `drawn_tiles{role="held"}` |
| I10 | `underdraped_tiles`, `imagery_over_terrain{gap}` |
| I11 | violations I11, `evictions_total{reason}` |
| I12 | the per-step decision digest (`/budget/step`), replayed from `/budget/events` |
| I13 | `batch_steps_per_frame`, `batch_fatal_total`, violations I13 |
| I14 | `thrash_total`, `rereads_total`, `starts_total{source}`, `network_bytes_avoided_total` |
| I15 | `starts_total{source="local"}`, `in_flight{source="local"}`, `local_waits_total`, `store_misses_total` |
| I16 | `uploads_total`, `upload_bytes_total`, `bytes` (equal to the union run's), `duplicates_in_flight` |
| I17 | `eye_lost_tiles{eye}`, `eye_bare_items{eye}` |
| I18 | `frame_upload_bytes`, `frame_deadline_bytes`, `deadline_deferrals_total`, `frame_upload_seconds` |
| P1.1 | `floor_in`, `floor_seconds`, `lost_tile_steps_total`, `bare_item_steps_total` |
| P1.2 | `flaps_total`, `drawn_tiles{role}` over a still interval, the digest |
| P1.3, P2.4, P3.4, P4.5, P5.6, P6.5 | `converged_seconds`, `unconverged_seconds`, `converge_bound_seconds` |
| P1.4 | `starts_total{class,source}`, `local_waits_total`, `in_flight` |
| P2.1 | `swaps_total{kind="refine"}`, `swap_gaps_total` |
| P2.2, P5.3 | `wasted_starts_total`, `cancels_total{reason="grace"}`, `starts_total` |
| P2.3 | `underdraped_tiles`, `imagery_over_terrain` |
| P3.1 | `swaps_total{kind="coarsen"}`, `drawn_tiles{role="held"}`, `swap_gaps_total` |
| P3.2 | `bytes` against `target_bytes`, `over_target_seconds_total` |
| P3.3 | `bytes` against `hard_limit_bytes`, `sse_scale`, the step's idleness on the tape |
| P4.1 | `fallback_depth_max`, `lost_tile_steps_total` |
| P4.2 | violations I11, `evictions_total` |
| P4.3, P5.4 | `thrash_total`, `rereads_total`, `starts_total{source="network"}`, `flaps_total` |
| P4.4 | `drawn_tiles{role="exact"}`, `underdraped_tiles` |
| P5.1 | `lost_tile_steps_total` across the change, `evictions_total` at it |
| P5.2, P5.5 | `/views` on the tape: the lens of each step, and the need's digest |
| P6.1 | `uploads_total`, `bytes`, `duplicates_in_flight` |
| P6.2 | `eye_lost_tiles`, `eye_bare_items` |
| P6.3 | `frame_upload_bytes`, `deadline_deferrals_total` |
| P6.4 | `decide_work`, `decide_seconds`, `decide_peak_seconds` against `decide_share_seconds` |

### 16.5 Labels: closed sets

| Label | Values |
|---|---|
| class | `terrain_mesh`, `tiles3d_mesh`, `drape`, `overlay` |
| state | `drawable`, `in_transit` |
| source | `local`, `network` |
| role | `exact`, `held`, `fallback` |
| reason (cancels) | `grace`, `timeout`, `preempted` |
| reason (evictions) | `sweep`, `pressure`, `admission`, `release_beyond` |
| kind (failures; flaps; swaps) | `transient`, `permanent`; `terrain`, `imagery`; `refine`, `coarsen`, `mesh_drape`, `layer` |
| gap | `-8` to `8` |
| eye | `0` to `3` |
| invariant | `I1` to `I18` |

The host adds `profile` (desktop, handheld, handheld_low, browser, headset, batch) and
`platform` (macos, linux, ios, android, web) once, as process labels; the budget never does.
A `tuile-server` process aggregates its sessions' meters under its own labels: a session id
is never a label, and a session's detail goes to its tape.

### 16.6 Sinks

1. **Prometheus, through tuile-metrics.** The meter's numbers are atomics in
   `tuile_core::metrics`, published by `publish_every` (every 500 ms; loopback port 9464
   unless `TUILE_METRICS_ADDR` says otherwise) like the engine's other numbers. A gauge
   sampled every 500 ms misses a one-step event, so every one-step quantity has a counter twin
   (`lost_tile_steps_total`, `bare_item_steps_total`) or a peak reset at each publish
   (`decide_peak_seconds`, `fallback_depth_max`).
2. **The MCAP session tape** (it replaces the JSONL log the earlier sections named).
   tuile-tape writes the budget's channels into the same file as `/camera`, `/clock` and
   `/frame`, on the same timeline, so one session file holds the camera, the pictures and the
   decisions, and replays and diffs as one.
   - **Encoding**, as tuile-tape's channels: message encoding `json`, schema encoding
     `jsonschema`, schema names `tuile.*`. Floats in Rust's shortest round-trip form (exact,
     as `/camera`); 64-bit digests as 16-digit hex strings (a JSON number loses them past
     2^53, as `goldens.txt` writes them); byte counts as integers. Every message carries
     `version`, `step`, `frame` and `now_us`.
   - **Log time**: tuile-tape's frame stamp, so a step sits beside the camera and the picture
     of the frame that drew it.
   - **Topics**:

     | Topic | Schema | Written | Holds |
     |---|---|---|---|
     | `/budget/step` | `tuile.BudgetStep` | every step | the decision digest and the stats digest; counts of starts, cancels, deliveries, evictions, releases and uploads by class; `sse_scale_milli`, `frame_ready`, `converged`; the ledger by class and state; the stats |
     | `/budget/events` | `tuile.BudgetEvents` | every step | the events the host handed the budget since the last step, and the presence answers the step used: what `decide` read |
     | `/budget/frame` | `tuile.BudgetFrame` | every render frame | the executor's `FrameRecord` |
     | `/views` | `tuile.Views` | every step | every eye's pose and lens (viewport, scale, four angles) and the `ViewerState` generation; `/camera` stays the mono path a replay flies |
     | `/budget/decisions` | `tuile.BudgetDecisions` | every step, traces only | the full `Decisions`: starts, cancels, deliveries, evictions, the upload plan, the draw set |
     | `/budget/second` | `tuile.BudgetSecond` | every second | the `BudgetSignals` snapshot: the field log (§16.9) |

   - **Replay**: `/views`, `/budget/events` and the session's sources (named in the tape's
     metadata) are what `decide` read, so a device session's decisions replay offline, bit
     for bit, and their digests compare step by step with the tape's: I12 in the field.
     `tuile-trace-dump` prints the budget's channels; `tuile-scan-black` reads `/views` to
     scan each eye.
   - The file's own bytes are not yet deterministic (tuile-tape's note on `mcap`'s summary
     order); tapes are compared by their decoded messages.
3. **Tracing**, in the `det!` style: one `budget.decide` span per step (`step`, `frame`);
   `det!("budget.step", step, digest, starts, cancels, evictions, lost, bare)` on the
   `tuile_det` target, off unless asked, as the selection traces are; `warn` events only for
   what a person must see — ground lost or bare once the floor is in, a class within 10 % of
   its hard limit, an invariant violation — and never a line per tile.
4. **The control API** (§4c): `budget()` returns `BudgetSignals`, by the same names without
   the prefix, and the last step's summary; `observe(path)` opens the session tape;
   `wait_until` conditions are signals (`converged`, `lost_tiles == 0`,
   `bytes_below(class, n)`).
5. **The overlay** (the viewer's diagnostic views): bars per class against target, overflow
   and hard limit; lost, bare, held and fallback counts; `sse_scale`; deferrals; `decide`'s
   p99 against its share. It reads `BudgetSignals` and nothing else.
6. **The web worker's channel.** The worker owns the meter and posts
   `ToPage::Signals(BudgetSignals)` every 500 ms (a new host message); the page adds its
   executor's `FrameRecord`s under the same names and feeds its overlay. No listener runs in
   a browser: a test reads the page's signals through the page.

### 16.7 The dashboard

One dashboard, "Tuile — graphics budget", over any process that publishes tuile-metrics
(the desktop viewer, the headless viewer in a test run, the bake and film sessions on the
farm). Variables: `instance`, `profile`, `platform`.

| Row | Panels | Proves |
|---|---|---|
| Coverage | `lost_tiles` and `bare_items` (flat at zero); `drawn_tiles` by role, stacked; `eye_lost_tiles` by eye; `floor_seconds` | I1, I17, P1.1 |
| Memory | `bytes` by class and state, with `target_bytes`, `overflow_bytes` and `hard_limit_bytes` as lines; `protected_bytes`; the rate of `over_target_seconds_total`; `pressure_level` and `thermal_level` as annotations; `sse_scale` | I2, I8, P3.2, P3.3 |
| Convergence | `converged` as a state timeline; `converged_seconds` p50 and p95 against `converge_bound_seconds`; `unconverged_seconds`; `underdraped_tiles`; `imagery_over_terrain` as a heatmap; the rate of `flaps_total` | I3, I4, I10 |
| Work | starts by class and source; wasted starts; cancels by reason; failures; `load_seconds` p95 by class; `in_flight` against the quotas; heap, tracked, duplicates | I5–I7, I15, P1.4, P2.2 |
| Reuse | thrash, rereads, network bytes avoided, store misses | I14, P4.3, P5.4 |
| Uploads | `frame_upload_bytes` against `frame_deadline_bytes`; the rate of deferrals; `frame_upload_seconds` p99; `upload_plan_bytes`; estimate drift | I18, P6.3 |
| Decide | `decide_seconds` p50 and p99 and `decide_peak_seconds` against `decide_share_seconds`; `decide_work` | I6, P6.4 |
| Integrity | `invariant_violations_total` by invariant (flat at zero); rejected inputs; `swap_gaps_total` | all |
| Batch | `batch_steps_per_frame`; `batch_fatal_total` | I13 |

Alert rules worth having:

| Alert | Expression | Severity |
|---|---|---|
| Ground lost | `increase(tuile_budget_lost_tile_steps_total[1m]) > 0` | critical: black ground |
| Ground bare | `increase(tuile_budget_bare_item_steps_total[1m]) > 0` | critical |
| Invariant violated | `increase(tuile_budget_invariant_violations_total[5m]) > 0`, by `invariant` | critical |
| Duplicates in flight | `tuile_budget_duplicates_in_flight > 0` | critical (I7) |
| Approaching the hard limit | `sum by (instance, class) (tuile_budget_bytes) / on (instance, class) tuile_budget_hard_limit_bytes > 0.9` for 30 s | warning: the jetsam guard |
| `decide` over its share | `histogram_quantile(0.99, sum by (instance, le) (rate(tuile_budget_decide_seconds_bucket[5m]))) > on (instance) tuile_budget_decide_share_seconds` for 5 min | warning |
| Not converging | `tuile_budget_unconverged_seconds > 3 * tuile_budget_converge_bound_seconds` for 1 min | warning |
| Thrashing | `rate(tuile_budget_thrash_total[5m]) > 1` | warning |
| Deadline sustained | `rate(tuile_budget_deadline_deferrals_total[1m]) > 10` for 2 min | info |

The dashboard's JSON and the rule file are written from this section at the cut-over, where
the farm's dashboards live.

### 16.8 Tests read the signals

- The simulation's per-step check becomes the core's `check` plus what needs the world; a
  scenario asserts `BudgetSignals` (`lost_tile_steps_total == 0`, `converged_seconds ≤
  T_conv`) rather than private fields where a signal exists.
- The headless tests and the device runs read the same `BudgetSignals`: live through the
  control API, afterwards from the tape's `/budget/second` and `/budget/step`.
- Every signal has a test that it moves when its behaviour happens and stays still when it
  does not (the test plan's observability level).

### 16.9 Field telemetry (iOS, Android, headset)

Per session, on the device, bounded, with no coordinates and no credential:
- **The journal** (`journal.rs`, its start and end lines): the end line gains a budget
  summary — the peak bytes per class against the hard limit, lost and bare steps, invariant
  violations, `converged_seconds` p50 and p95, the worst `decide`, deferrals, the pressure and
  thermal events. One line, always written.
- **The field log**: an MCAP file with `/budget/second` and, from `/budget/events`, the
  platform's events only (pressure, thermal, retarget, background). No `/camera`, no `/views`,
  no frames. About a kilobyte a second; capped at 32 MiB, the oldest session dropped first;
  in the app's own container (`Library/Logs/Tuile/` on iOS, the app's files directory on
  Android and on the headset).
- **A trace**, when a person or a test asks for one (`TUILE_TRACE`, as today): the full
  session tape of §16.6, camera and frames included.

Pulled in tests:
- iOS simulator: the container from `xcrun simctl get_app_container booted <bundle> data`;
- iPhone: `xcrun devicectl device copy from --device <id> --domain-type appDataContainer
  --domain-identifier <bundle> --source Library/Logs/Tuile --destination <dir>`;
- Android emulator, phone, headset: `adb exec-out run-as <package> tar c files/tuile-logs`
  (a debuggable build);
- live, during a run: the control socket's `budget()`, through `adb forward` or the
  simulator's loopback.

The harness reads the pulled files with the reader the headless tests use, so a device run
is judged by the same assertions.
