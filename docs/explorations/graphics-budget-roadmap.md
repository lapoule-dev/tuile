# Graphics budget: roadmap after the plan (A → B → C)

Kept from the plan approved with Laurent on 2026-10-19/20.

## Status on 2026-10-25 (v0.14.0)

| Step | Status | Where |
|---|---|---|
| A0 cut-over | done | #99 (v0.10.0) |
| A1 correctness defects | done | #100, #109 |
| A3 audit fixes | done — decide allocations 6,492 → 13 per still step, digests bit-identical | #108 |
| A5 bake | three-stage pipeline done (#105, v0.12.0); the shared tokio runtime and HTTP pool per process not started | #105 |
| A6 loading and decoding | GPU-compressed imagery shipped as the client's choice, off by default: the format seam (#114), stored KTX2/UASTC transcode-only (#121), GPU encode to BC7/ASTC in tuile-wgpu (#122), the web paths — GPU encode on WebGPU, CPU encode in the decode workers on WebGL2 (#127); v0.13.0 and v0.14.0. zune-png dropped after the decode bench (#110, open). io_uring not started | #114, #121, #122, #127 |
| A2 ledger hardening | not started | |
| A4 incremental decide, dense structures | not started | |
| B, C | not started | |

**Next: the compression budget.** With compression on, the budget still under-counts it: `plan` caps on resident
bytes (87,408 per BC7 tile) while 349,524 RGBA8 bytes cross; the encode's transient memory and the encoder scratch
are never charged; the encode pacing (`EncodePace`, 4 ms a frame) sits outside `FrameDeadline` and assumes 5 ms
per BC7 tile while it costs 9–10 ms, which explains the settle regression from 5.0 s to 10.2 s. A plan of eight
PRs — numbered B1–B8 in that plan, unrelated to section B below — fixes it in the core first: a cost per path
(transfer / resident / transient), the transient peak and fixed reservations in the ledger, an encode lane inside
`FrameDeadline`, simulation profiles with goldens, then the wgpu pump and the web CPU path on the core's deadline,
signals, and a measured acceptance (imagery peak ≤ 15.5 MB, BC7 settle ≤ 6.0 s, Off goldens unchanged). It comes
before A2 and A4.

### O1 — the budget's algorithms, before CB (status: O1.2 eviction index, 2026-10-31)

The budget is its own crate, `tuile-budget` (#163). O1 optimises its `decide` before the compression budget,
decisions bit-identical throughout. The guard comes first (O1.0): today's engine frozen at 3d1a028e in
`crates/tuile-budget/tests/reference/` (engine, state, sorted and the `Budget` methods, a mechanical copy) and
driven step by step beside the real one (`Lockstep`) in every `budget_sim` run and in the properties' random
scripts — `Decisions` (digest and content; statistics but the `work` counter), ledger, audit, residency and
resource views compared every step. O1.1–O1.5 change the real engine and keep the lockstep green.

The baseline on an Apple M2 (16 GB), `cargo bench -p tuile-budget --bench budget_decide` and the release
`budget_sim` ratchets:

| Measure | Value |
|---|---|
| `decide` still view (484 drawn) | mean 1 098 µs, p50 1 086 µs, p99 1 188 µs |
| `decide` motion (soak tour, ≤ 1 022 drawn) | mean 902 µs, p50 797 µs, p99 2 174 µs |
| `decide` pressure (desktop on 384 MiB, LOD relaxation capped, the tour's orbit looped; ≤ 293 drawn) | mean 911 µs, p50 879 µs, p99 1 650 µs |
| allocations per step, still (internal / output) | 13 / 649 |
| allocations per step, motion p99 (internal) | 43 |
| allocations per step, pressure p50 / p99 (internal) | 21 / 76 |
| step buffers after the tour | 381 056 B |
| bytes per tracked resource | 534 (2 927 tracked) |

The pressure scenario (`soak::pressure_profile`) is over its target on 2 879 of 3 000 steps and evicts on 551:
the eviction order is built and sorted nearly every step, for a view a third the size of the motion one at the
same cost. P6.4 (`decide_work_grows_with_changes_not_with_the_view`) is written and ignored: on a converged
still view, with nothing changing, W_k is about 3 550 (up to 4 646) against a bound of 256 — `work` is
O(need + tracked) until the incremental decide (O1.4).

**O1.1 — dense arena** (`perf/budget-o1-arena`). `entries: BTreeMap<ResourceId, Entry>` is gone: each tracked
resource has a `u32` slot in `state::Arena`, found through a fixed-seed hash index (hashbrown's `HashTable` of
slots, foldhash's `FixedState`) that is only probed, never iterated; freed slots are reused last-freed-first. The
state is columns by slot: id, class, a flags byte (live, decoded, delivered, in transit, on the GPU, loading,
local), the eviction key (last needed, level, distance), each copy's payload without its `Option`, the step's
rank, and the cold rest. 211 B a slot (`SLOT_BYTES`, was `ENTRY_BYTES` 240 plus a 16 B key in a map node). The
full scans (eviction order, decoded release, pooled sweep, `release_beyond`, prune, audit) walk the slots and
sort by their total keys as before; the one range query (a tile's drawable drapes of a layer, by level) reads an
ordered index of the GPU drapes kept by `put_gpu`/`take_gpu`. The eviction orders carry their slots, so a step's
evictions look nothing up. Lockstep with the frozen reference green (budget_sim release, properties at 2 048
cases), goldens untouched; a broken drape index is caught at step 8 by every lockstep run.

| Measure | O1.0 | O1.1 |
|---|---|---|
| `decide` still (484 drawn) | mean 1 098 µs, p50 1 086, p99 1 199 | mean 818 µs, p50 808, p99 889 (−26 %) |
| `decide` motion | mean 900 µs, p50 796, p99 2 176 | mean 696 µs, p50 609, p99 1 729 (−23 %) |
| `decide` pressure | mean 908 µs, p50 877, p99 1 637 | mean 652 µs, p50 637, p99 1 124 (−29 %) |
| allocations per step, still (internal / output) | 13 / 649 | 13 / 649 |
| allocations per step, motion p99 (internal) | 43 | 42 |
| allocations per step, pressure p50 / p99 (internal) | 21 / 76 | 21 / 73 |
| step buffers after the tour | 381 056 B | 381 056 B |
| bytes per tracked resource | 534 (2 927 tracked) | 452 (2 927 tracked) |

The criterion means (`decide/still` −26.1 %, `motion` −23.1 %, `pressure` −29.4 %, p < 0.05) agree. The
allocation counts barely move: the arena allocates only when its columns grow, and the per-step allocations
left are the step's own buffers (O1.5).

**O1.2 — persistent eviction index** (`perf/budget-o1-evict-index`). The eviction order is no longer rebuilt by a
full scan and sort in `evict_order`, the pooled sweep, the decoded release and `release_beyond`, and `make_room`
no longer folds the whole order on every delivery to learn whether room can be made. The arena keeps the order up
to date. An eviction key moves whenever the view touches a resource — every step for everything drawn — so the
arena files only what the view left alone: the resources needed at the two most recent times are *grouped*
(listed by that time, outside the index); every other resource holding a copy outside transit is *filed* in a
treap per class and per copies held (both, GPU only, decoded only), keyed by `(EvictKey, slot)`, its links a
column of the arena (8 B a slot, nothing allocated as resources come and go), and its GPU bytes counted as its
class's *idle bytes* unless it is of the floor. A third time files the older group's resources not needed since;
a resource needed again leaves the index. A step that draws what it drew moves nothing; one that turns files what
it left. Walks merge the index with the grouped resources, sorted once a step when a walk needs them; nothing
filed is wanted or kept, so `make_room` knows what it can free from the idle bytes plus the step's grouped
candidates by want urgency (`Room`), in O(1) per delivery, and evicts by walking the order (O(log N) per victim).
Same victims in the same order: lockstep green in every `budget_sim` run and the properties (2 048 cases),
goldens and pattern goldens untouched. `audit` checks the index (order, heap, membership, idle bytes, groups)
against a recomputation in debug builds, and `make_room` checks its count against a scan. Red seen: a slot left
out of the index when its transit ends diverges from the frozen reference in 13 of 61 `budget_sim` runs (first
at step 140 of the back-and-forth, on the decoded releases).

The index's links and the group lists cost about 17 B per tracked resource; the arena's columns now grow by an
eighth instead of doubling (slots are never given back, so a doubling left up to half of every column unused for
the session), which more than pays for them.

| Measure | O1.1 | O1.2 |
|---|---|---|
| `decide` still (484 drawn) | mean 816 µs, p50 806, p99 905 | mean 813 µs, p50 802, p99 887 (no change) |
| `decide` motion | mean 696 µs, p50 608, p99 1 726 | mean 680 µs, p50 606, p99 1 627 (−2 %, p99 −6 %) |
| `decide` pressure | mean 652 µs, p50 637, p99 1 117 | mean 466 µs, p50 456, p99 720 (−29 %, p99 −36 %) |
| allocations per step, still (internal / output) | 13 / 649 | 13 / 649 |
| allocations per step, motion p99 (internal) | 42 | 38 |
| allocations per step, pressure p50 / p99 (internal) | 21 / 73 | 12 / 74 |
| step buffers after the tour | 381 056 B | 381 056 B |
| bytes per tracked resource | 452 (2 927 tracked) | 445 (2 927 tracked) |

The criterion means: `decide/pressure` −30.1 % (p < 0.05); `still` −0.2 % and `motion` −1.2 %, neither
significant. The still view never evicts, so it gains nothing (and pays nothing measurable: the group lists turn
over in place). The motion tour evicts on few steps. Under pressure, timed with temporary probes, `make_room`
took about 180 µs a step (some 270 calls a step past its fast path, each folding the order, plus the order's
build and sort) and the sweeps about 56 µs (the pooled one 44); after, about 22 µs (mostly the probes' own
clocks) and 25 µs, of which the step's grouped candidates, sorted, about 16. What is left of the step is the
need, the draw set and the ranking (O1.3–O1.5). Allocations under pressure: the median step halves; the p99 is
one block higher (73 → 74), the decoded release now collecting the kept copies and the grouped holders apart.

The pressure scenario's I7 break, explained (not fixed: behaviour is frozen through O1): at t ≈ 105 s of the
unlooped tour under `pressure_profile`, 1 336 of the 1 929 tracked resources hold only a decoded copy (no GPU
copy, nothing in transit or loading; 984 of them not wanted), and the decoded tier sits at 534 MB, just under its
512 MiB target. That target is `BudgetConfig::interactive`'s quarter of the configured GPU total (2 GiB), and
`retarget` does not scale it: `eff()` scales the decoded target by pressure only, not by the retarget factor.
So on a 384 MiB budget the decoded tier still keeps 512 MiB of copies demoted by evictions, and every one of
them is a tracked resource that the simulation's I7 bound (GPU-drawable + pending + need) does not count.


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
    Done (#99).
A1. Correctness defects found by the measurements (done, #100 and #109):
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
A3. Budget code audit fixes — BEFORE the algorithm work (Laurent, 2026-10-20). Done (#108). Read-only audit of origin/main
    7b0ac83 (report: docs/explorations/graphics-budget-audit.md, 64 findings; clippy perf lints find nothing — the
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
    (survey_lift / survey_hud / pose) + stl_bake_pose_seconds. Done (#105).
    Then one tokio runtime and one HTTP connection pool per process shared by all bakes (HTTP/2 to the
    object store and stl-tiles). The branches are reconciled with the cut-over afterwards.
A6. Loading and decoding: GPU-compressed textures (BC7/ASTC/ETC2 via KTX2 + Basis Universal: 4–8× less
    GPU memory and upload per drape, no client JPEG decode; storage format, transcoding, per-backend
    support wgpu/WebGL2); PNG through zune-png; io_uring for the farm's PMTiles reads only if syscalls
    still weigh after the shared runtime (Linux-only, needs a measurement). hyper instead of reqwest:
    not planned (marginal).
    As shipped (v0.13.0–v0.14.0): BC7 and ASTC 4×4 only, chosen by tuile's client and off by default
    (no compression in the bake or the film); stored imagery is read as KTX2/UASTC and transcoded, never
    re-encoded; ETC2 and a client JPEG-free path were not pursued; zune-png was measured and dropped (decode bench, #110).
    The budget accounting of compression is the next step (see the status above).

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
