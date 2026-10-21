# Graphics budget: roadmap after the plan (A → B → C)

Kept from the plan approved with Laurent on 2026-10-19/20. Status on 2026-10-21: A0 done (#99, v0.10.0), A1 done (#100, #109), A3 done (#108), A5 first half done (#105, v0.12.0); A6 in progress (texcomp, #107); A2, A4, B and C not started.


Order is strict: A before B, B before C. Every step of A, B and C goes through the same battery before
the next starts — tests seen red, mutants killed, the reference model extended, Kani proofs of its
invariants where they apply, protocol model check if it changes messages, fuzzing, a runtime audit
signal, the MCAP regression corpus — and is measured before/after (simulation P1–P6 × profiles,
headless runner on the real network, criterion benches, Pyroscope/Prometheus on the farm).
Each experiment is kept only if it wins (network, blur, thrash, decide cost, invariants); the loser
is deleted (no switch).

### A — Algorithmic corrections (first)
A0. The cut-over, EFFECTIVE everywhere: native (Mac first, then iOS/Android), web (page + worker over
    postMessage, WebGPU and WebGL2, verified in a real browser) and bake (tuile-bake Session on
    BudgetConfig::batch(), terrain digests identical) — the PR is not done until all run on the budget.
    In progress (feat/budget-cutover).
A1. Correctness defects found by the measurements:
    - the warm relaunch that refetches 1 537 items after a flushed store (lying index or lost writes)
      — the store ↔ ledger contract must be right before any cost-aware eviction;
    - the pooled ledger's phone slow dive over mountains (512 MB) that no longer converges in 8 s;
    - the ignored tests' list reviewed (each one fixed or its reason still true).
A2. Ledger hardening: cargo-mutants on crates/tuile-core/src/budget/ (1 141 mutants; every missed one
    killed or recorded as a spec hole); the proptest-state-machine reference model; the continuous
    camera-path generator and the L2 pattern properties; Kani bounded proofs (ledger = sum of states,
    the protected set never evicted, no stale delivery); the budget ↔ executor protocol model-checked
    with Stateright (acks a round trip late, Discarded, cancels, swaps, pressure — the web worker and
    remote sessions); cargo-fuzz on decide; runtime audit (full in debug/test, sampled in release →
    tuile_budget_audit_failures + alert); the MCAP corpus of real sessions replayed in the simulation;
    the 1 h soak with resizes and stereo.
A3. Budget code audit fixes — BEFORE the algorithm work (Laurent, 2026-10-20). Read-only audit of origin/main
    7b0ac83 (report: scratchpad/budget-antipatterns.md, 64 findings; clippy perf lints find nothing — the
    cost is structural): ~8–10k allocations per decide at 500 drawn tiles (estimate, to measure).
    Fixes, keeping Decisions::digest bit-identical (goldens.txt, pattern_goldens.txt):
    a. select() linear and allocation-free: an ancestor→held index built once per step instead of scanning
       held_last per missing tile (engine.rs:733 — O(missing × held × depth), the likely p99 in motion);
       a lazy ancestor iterator and a has_drape that stops at the first found (engine.rs:619–686), memoised
       within the step; no per-item BTreeSet/BTreeMap (engine.rs:674, 142, 713–722, 790, 849).
    b. A scratch of reused sorted Vecs owned by the Budget instead of per-step BTree rebuilds and clones:
       borrow Need instead of cloning it (engine.rs:459), drawn_res/drapes/*_last without clones
       (1030–1066, 1261–1271), maps used only for lookups replaced by merge joins (450, 509, 862, 1093,
       1099, 1177–1194), protected_bytes without a materialised union (1307–1331), sort_unstable on total
       keys (305, 1140, 1588, 1800, 1891, 1939, 2006, 2189), buffers keeping their capacity (events 304,
       ranked_last 995/1145); keep the tie rules (read_need/wants keep the first, self.tiles the last).
    c. Stop scanning every tracked entry: plan() and pre-emption over `active` only (1786–1847, 1443–1469),
       one eviction order per class per step consumed by a cursor (1874–2006), make_room without a full
       sort per delivery (1567–1588), prune spread out (2121–2160).
    d. The observer honours its zero-cost contract (observe.rs:252–263, 891–998).
    e. Robustness: the 8 `u32 -= 1` counters checked (saturating with an audit failure, never a wrap);
       a compile-time assertion on size_of::<Entry>() (~256 B today); two Vecs per DrawItem reviewed.
    f. Lints on the budget module: iter_over_hash_type (deny, determinism guard), float_arithmetic (deny on
       engine.rs/state.rs), cast_possible_truncation with a saturating helper (wasm32 usize), optionally
       too_many_lines (step() is 286 lines).
    g. Allocation made measurable: a counting global allocator in the budget_sim binary (or dhat), counted
       around decide; two ratchets frozen like goldens — internal allocations per step on a converged
       fixed view (target ~0 after warm-up; output only), and bytes per tracked resource (~450–550 today).
    Measured before/after with a criterion bench (mean and p99 of decide).
A4. Algorithms and structures of the budget: the incremental decide — dirty sets, need deltas from
    the traversal, audit() == recomputation (O(changes · log N), required by P6.4); a dense arena
    (ResourceId → u32 slot, slotmap; state in Vec/SoA); an indexed heap with change-priority or a bucket
    queue (ties by id); bitsets over dense indices (protection, per-view masks); smallvec per tile;
    Decisions buffers reused. Rule kept: never iterate a HashMap to decide (wasm32 vs 64-bit order);
    hash maps only for lookups with a fixed-seed hasher. Measured on a criterion bench (500 drawn tiles).
A5. The farm bake: the three-stage pipeline — (1) serial selection per frame, hysteresis chain kept,
    upcoming frames' tiles prefetched (known future, batch); (2) parallel ground + placement + audit
    on rayon from frozen selections; (3) in-order pack writing through a reorder buffer — proven by
    bit-identical packs; the survey's double settles removed, its placements reused; the ground index
    built cheaper (no per-vertex trig, no rebuild when a tile's stitch plan is unchanged); phase labels
    (survey_lift / survey_hud / pose) + stl_bake_pose_seconds. In progress (perf/bake-three-stage).
    Then one tokio runtime and one HTTP connection pool per process shared by all bakes (HTTP/2 to the
    object store and stl-tiles). The branches are reconciled with the cut-over afterwards.
A6. Loading and decoding: GPU-compressed textures (BC7/ASTC/ETC2 via KTX2 + Basis Universal: 4–8× less
    GPU memory and upload per drape, no client JPEG decode; storage format, transcoding, per-backend
    support wgpu/WebGL2); PNG through zune-png; io_uring for the farm's PMTiles reads only if syscalls
    still weigh after the shared runtime (Linux-only, needs a measurement). hyper instead of reqwest:
    not planned (marginal).

### B — The GC: the ledger's eviction side (after A)
Inspiration: OS virtual memory (page-frame database = ledger, working set = need, mlock = protected,
Clock/LRU = eviction, swap = demotion to the stored tier), caches (ARC, 2Q, GreedyDual), GPU residency
managers (D3D12 residency, QueryVideoMemoryInfo).
B1. Cost-aware eviction (GreedyDual-Size): H = L + restore_cost(tier, class)/size, BTreeSet<(H, id)>,
    L = H of the last evicted; restore costs as config constants per tier (or re-estimated per epoch and
    recorded in the MCAP); only among evictable resources (not protected, not needed); the pooled
    ledger's single sweep follows H within its floors/ceilings. The store notifies the ledger when it
    evicts (Stored → Remote; check foyer's eviction listener), with a test; fallback: query the presence
    oracle for eviction candidates when computing H.
B2. Scan resistance (2Q / ARC): a probation list (loaded, not yet drawn on K frames) evicted first, a main
    list for the confirmed working set; ARC ghost lists (ids only, bounded) adapt the split. Target: the
    38 489 evictions for 43 679 uploads seen on the Mac before the cut-over.
B3. Next-use eviction (Belady): batch — a next-use index (ResourceId → next frame needing it) from the
    three-stage bake's stage 1; evict the farthest next use; release_beyond keeps what is needed soon;
    property: reloads equal the brute-force optimum on small traces. Interactive — next use from the
    current motion only (the predicted view comes in C). The ledger hints the store what is likely to
    come back (double caching coordination).
B4. Bounded eviction per frame: an eviction work quota inside the FrameDeadline; over-target tolerated a
    bounded time (I2, I8); CPU-copy releases deferred to quiet frames.
B5. GPU pools: executor slabs / texture arrays per size class; the ledger counts slots, acks report slot
    + occupancy → fragmentation measured; per-backend limits (layer counts, WebGL2).
B6. Adaptive tuning: a slow controller with hysteresis moves the pool's per-class floors/ceilings within
    bounds from thrash/blur/network signals; every adjustment recorded in the MCAP.
B7. Per-view reference counts: a view bitmask per needed resource (≤ 8 eyes/channels); a view's
    protection drops with its bit; one charge per resource (I16); a closed channel releases cleanly.
B8. The ignored-tests review becomes a non-blocking CI job (cargo test -- --ignored, reported).

### C — Predictive loading, INSIDE the ledger (after B)
The prediction is not a side channel: it is a ledger concept. A predicted resource is a ledger entry like
any other — its state, tier, bytes and class accounted, its own priority band strictly below the current
need, never protected, entering B2's probation list; prefetch quotas (in-flight, bandwidth, memory
headroom, e.g. ≤ 20 %) are ledger budgets; predictions are recorded inputs of decide (replayable,
deterministic); cancelled by generation when the prediction changes; next-use (B3) and prediction are the
same information (one drives eviction, the other loading).
C1. Known future: films/bakes (beyond A4's batch prefetch: next-use-aware release_beyond), tape replay,
    Goto/Glide destination and path.
C2. Gesture inertia (fling, pinch momentum — the viewer's own model gives the exact path) and the
    headset's predicted pose (OpenXR predicted display time) + guard views.
C3. Tiered prefetch: far ahead network → stored (disk), medium decoded (RAM), GPU just in time inside the
    FrameDeadline; an adaptive lookahead window (Linux readahead): grows on hits (used within T), shrinks
    on misses.
C4. Level skipping along the predicted path from ancestors' availability metadata (quantized-mesh every 10
    levels), breaking the parent→child chain in cold zones.
C5. Motion extrapolation (Kalman, established crate) and domain priors (live athlete position, track
    start/finish), last, tuned on field data.
Proofs: batch prefetch never changes a selection (bit-identical packs); never evicts a current need, never
delays one (strict priority), quotas hold; signals tuile_budget_prefetch_{started,used,wasted_bytes,
window_seconds}, hit rate per source.
