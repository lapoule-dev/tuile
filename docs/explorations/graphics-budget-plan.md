<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — implementation plan

Implements `graphics-budget-to-be-and-spec.md`. One branch, `feat/budget-core`, in its own
worktree. It is buildable and green at every commit. The old budget management is deleted in
the cut-over commit, with no flag and no shadow. Skirts are out of scope.

## Context

The viewers (desktop and iOS) have no GPU budget: one CPU-side cache counted in decoded
bytes and tile count, four unbounded queues in four orders, cancellation at frame rate,
non-cancellable decode, O(N) scans per upload, and an imagery level frozen at load time from
a global altitude. That gives the reported symptoms: loading slows down over a session, and
tiles stay coarse on rotation and zoom-out. Films go through the same traversal and must
keep their exactness.

The goal is one pure, deterministic budget module that drives viewers and films alike, with
imagery chosen independently of terrain, proven by simulation before it touches a pixel.

## Prerequisites
- **PR #62 (iOS app) merged.** The branch touches `tuile-viewer/src/profile.rs`,
  `app/lifecycle.rs` and `app/frame.rs`, all of which #62 rewrites.
- **The local machine stays usable:** one heavy build at a time, `CARGO_BUILD_JOBS=4`, and
  film renders on the farm only.

## Steps

### Step 0 — Baseline (S)
1. Worktree `../tuile-budget` on `feat/budget-core` from `main`, after #62.
2. **Observer first:**
   - `BudgetObserver` (watch-only, the pattern of `tuile-film-native/src/observe.rs:146`),
     called once per frame by the pump;
   - JSONL sink (`TUILE_BUDGET_LOG`) and `/metrics`;
   - the fields of AS-IS §4.1, plus `underdraped` and the imagery-over-terrain histogram.
3. **Symptom tests on today's code, written against the headless control API (Step 0b), committed red** (documented as expected failures, so CI
   stays green: `#[ignore = "red until budget-core"]`), each seen failing:
   - `still_camera_cancels_nothing` (S2, M2.1);
   - `zoom_back_redrapes_sharp` (S3, M3.1);
   - `rotation_converges` (S3, M3.2/M3.3), with a time bound;
   - `coast_imagery_sharper_than_terrain` (independence).
4. **Film baseline:**
   - record the `det!` selection and request digests of the golden scenes on `main`;
   - render the reference films on the farm (one-off job), open them, and record their
     parameters in `videos/README.md`.
5. **Device baseline:** the Mac replay and the iPhone touch script from AS-IS §4.2, run for
   30 min with the JSONL log. Save the logs. They are the "before" curves.

### Step 0b — The headless viewer and its control API (M)
Built first, on today's code, because every later test and reproduction drives the viewer
through it (spec §4c).
- **`tuile-viewer --headless`:**
  - an offscreen surface of a given size and scale;
  - the desktop or handheld profile, selectable;
  - a stepped clock, plus a real-time mode;
  - a pluggable network: real fetchers, a recorded trace, or `SimNet`, through the loader
    trait.
- **The control API:**
  - an in-process Rust API, and the same commands over a localhost socket (JSON, versioned,
    with a schema doc);
  - read: budget, resource, need, GPU, frame (PNG or RGBA plus diagnostic layers), coverage;
  - move: `set_view`, `fly_to`, `orbit`, `zoom`, `look`, gestures;
  - series: `run(script | tape)`, `step`, `advance`, `wait_until(converged | idle |
    no_holes | bytes_below)`;
  - inject: `pressure`, `thermal`, `background`/`foreground`, `network`,
    `kill_and_relaunch`;
  - record: `observe`, `snapshot`.
- Until Step 4, `budget()` and `resource()` read the current cache and pump. After the
  cut-over they read `Budget`. The API shape stays the same.
- **Tests:**
  - the API itself: the commands are deterministic under the stepped clock, the frame
    readback matches a known scene, and `kill_and_relaunch` keeps the store;
  - it never compiles into iOS release builds.
- **Docs:** a short page on driving the viewer from a script or an agent.

### Step 1 — The pure module (M)
Create `tuile-core/src/budget/`, unused by the runtime, so the build stays green:
- `types.rs`: `Class`, `ResourceId`, `Bytes`, `Generation`, `Policy`, `BudgetConfig`,
  `Pressure`, `Thermal`;
- `ledger.rs`: per-class bytes, overflow, high/low watermarks;
- `state.rs`: the resource state machine with generations; arrivals applied at the step
  boundary;
- `priority.rs`: tiers, the integer score, the id tie-break, and dedup;
- `requests.rs`: the bounded heap, per-class quotas, the grace time;
- `protect.rs`: P = drawn ∪ fallback chains (meshes **and** drapes) ∪ in transit ∪ floor;
- `evict.rs`: ordering and sweep;
- `relax.rs`: time-based LOD relaxation with hysteresis;
- `upload.rs`: the byte-capped plan and the swap groups (both directions);
- `batch.rs`: the Batch rules (unbounded, complete-or-wait, `release_beyond`);
- `tier.rs`: `Tier` (Gpu, Decoded, Stored, Remote), the `Presence` hint, the separate local
  read quota, and tier-aware cost and priority (spec §4b).

Tests:
- unit tests plus `proptest` for every property in spec §8;
- a wasm32 check of `tuile-core`.

**Exit:** `cargo test -p tuile-core budget::` green; proptest at 10 000 cases per property in
CI and 1 000 000 nightly.

### Step 2 — The imagery need (M)
- `tuile-core/src/budget/imagery.rs`: the texel footprint at a tile's closest visible point
  (distance and obliquity), the target pixels per texel, ±0.5-level hysteresis, the clamp to
  `[floor, terrain + cap]` and to the source maximum.
- Selection of the best drawable fallback drape (own coarser level or an ancestor), with its
  UV transform.
- **Tests:**
  - monotonicity in distance and obliquity;
  - hysteresis, so nothing flaps;
  - the cap;
  - a plain at shallow terrain level gets imagery deeper than its terrain level;
  - the UV transform for an ancestor drape, checked against `tuile-planetary`'s existing
    mosaic geometry.

### Step 3 — The simulation harness and soak (M)
`tuile-core/tests/budget_sim/`:
- `SimWorld`: synthetic terrain and imagery pyramids, with plains, mountains and coast.
- `SimNet`: seeded latency, jitter, failures, stalls, bandwidth, per-class servers.
- `SimGpu`: per-class byte counters.
- `SimServer`: a minimal stand-in for the runtime that drives `Budget` exactly as the real
  one will: traverse → need → decide → load → deliver → ack.
- **Scenarios:**
  - zoom out to 200 km and back;
  - 180° rotation in 1 s;
  - a coastal pan;
  - back-and-forth;
  - still for 10 min;
  - a memory warning mid-flight;
  - thermal serious;
  - a 300-frame film under Batch.
- **Assertions:** every invariant I1–I13, at every step.
- Golden decision digests per scenario, stored.
- **Soak:** one simulated hour under Interactive, nightly in CI, with a 5-minute reduced
  version on every PR. Slopes are asserted.

**Exit:** all scenarios green; soak slopes flat; the Batch film exact.

### Step 4 — The cut-over commit (L)
In one commit, which is reviewed in pieces on the branch beforehand:
- **Protocol:**
  - `Ack { resource, gen, bytes }`;
  - `Credit`, `Discarded`, `Pressure`, `Thermal`, `ReleaseBeyond`;
  - `Content` keyed by `ResourceId` and carrying a generation;
  - drapes as their own content;
  - `Select` carrying the `DrawSet` and the swap groups;
  - the `InProcessStream` credit and the coalesced `Select`.
- **Runtime:**
  - `GeometryServer` owns `Budget`; `decide()` runs once per coalesced client step, or per
    frame in Batch;
  - no retraversal per arrival;
  - `camera_moved` cancellation removed.
- **Offload:** cancel tokens and generations on jobs.
- **Stores:** `holds(key) -> Presence` on the tile store (foyer index) and on the HTTP disk
  cache, batched per step. Local reads get their own quota.
- **Planetary:** drapes built at the level asked; `ImageryDetail` and the load-time choice
  removed.
- **Pump:** `advance(plan, …)`, the priority-keyed pending queue, the pruned ancestry,
  O(1) counters, drawing from the `DrawSet` with the ancestor UV transform.
- **Viewer:** `Profile::budget()`, `Step::Pressure`, thermal input, and the observer wired
  in.
- **Bake, film, web, examples:**
  - `BudgetConfig::batch()` in bake;
  - `tuile-web/src/worker.rs` fixed;
  - examples and existing tests adapted.
- **Deleted:** everything in spec §7.
- The Step 0 symptom tests lose their `#[ignore]` and are green.
- The terrain `det!` digests are identical to the Step 0 recordings.
- The film goldens are re-recorded in the **next** commit, alone, so the diff of expected
  changes is isolated.

**Exit:** the workspace builds; clippy `-D warnings`; the wasm guardrail (core, and every
crate that claims wasm); all tests green; the iOS sim and device targets build.

### Step 5 — Film goldens and A/B (S)
- Re-record the film goldens in a dedicated commit, with a message stating why (imagery need
  per tile).
- A/B: render the reference films on the farm from the branch with an unbounded budget,
  through the one-off film job. Open them next to the Step 0 films, and record both in
  `videos/README.md`.
- Check the terrain digests identical and no frame with a hole.
- The owner accepts the imagery difference by eye.

### Step 6 — Tuning per profile (S)
- **Desktop and iOS fractions** from simulated pressure:
  - target, overflow, credit;
  - per-class quotas;
  - `upload_bytes_per_frame`;
  - relaxation rates;
  - the cap of 3.
- **Thermal steps** on iOS.
- Each value is justified by a simulation curve and written next to the constant.

### Step 7 — Device sessions (S)
- **Mac:** the Step 0 replays with the JSONL log, compared with the "before" curves.
  Convergence time after rotation and zoom-out, latency slope over 30 min, underdraped = 0
  at rest.
- **iPhone:** the touch-script tour for 30 min, plus a memory warning from Xcode, plus
  background/foreground. No hole, no crash, no watchdog kill.
- Screen recordings saved to `videos/` and copied to `~/Documents/stl-videos`.

### Step 8 — Merge gate (all required)
- The unit, property, simulation and soak suites green; the headless real-GPU suite green.
- The four symptom tests: red at Step 0, green now.
- Terrain `det!` digests identical. The film imagery change accepted on the A/B.
- The Mac and iPhone sessions checked by eye, with before/after curves attached to the PR.
- CI green on Ubuntu and macOS, with the wasm guardrail extended.
- The PR description lists the deleted items (spec §7) and the downstream impact.

## Sizes and order

```
0b Headless viewer + control API (M) ─► 0 Baseline (S) ─► 1 Module (M) ─► 2 Imagery need (M) ─► 3 Simulation (M)
                                                              │
                                   4 Cut-over (L) ◄───────────┘
                                         │
                       5 Film A/B (S) ─► 6 Tuning (S) ─► 7 Devices (S) ─► 8 Gate
```

Steps 1 and 2 can be written in parallel by two agents: they touch disjoint files.
Everything else is sequential.

## Risks
- **The cut-over is large.** Mitigation: Steps 1–3 prove the module in isolation first. The
  cut-over is then mostly wiring, and is reviewed piece by piece on the branch.
- **Drawable residency changes interactive selection timing.** Mitigation: the simulation
  scenarios plus the headless tests, with time-bounded convergence.
- **Drapes as their own content may raise the message count.** Mitigation: credit plus
  coalescing; the stats are watched.
- **Films change on purpose (imagery).** Mitigation: isolated golden commit, A/B on the farm,
  owner sign-off.
- **The downstream host's film goldens move with ours.** Mitigation: coordinate its repin of
  tuile after release.

## Decisions needed before Step 4
- **D1:** retry-with-backoff for interactive failures (proposed).
- **D2:** module in `tuile-core` (recommended).
- **D3:** keep stand-ins as a cheap class at first; decide on the simulation evidence.
- **D5:** iOS fractions and the imagery-over-terrain cap of 3. Defaults are proposed in
  Step 6.
