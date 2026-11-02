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

### O1 — the budget's algorithms, before CB (status: O1.5 part 4, 2026-11-02)

The budget is its own crate, `tuile-budget` (#163). O1 optimises its `decide` before the compression budget,
decisions bit-identical throughout. The guard comes first (O1.0): today's engine frozen at 3d1a028e in
`crates/tuile-budget/tests/reference/` (engine, state, sorted and the `Budget` methods, a mechanical copy) and
driven step by step beside the real one (`Lockstep`) in every `budget_sim` run and in the properties' random
scripts — `Decisions` (digest and content; statistics but the `work` counter), ledger, audit, residency and
resource views compared every step. O1.1–O1.5 change the real engine and keep the lockstep green. The debt pass after O1.3
extended the lockstep to the whole internal state, then retired the frozen copy: before it went (at af19a3e9) it
recorded, per step, digests of its decisions, ledger, views and internal state on every `budget_sim` run and on a
fixed corpus of 128 property scripts (`tests/reference/recorded.txt`, 195 KB); the real engine is now checked
against that file, every 256 steps and at each run's end. Random property scripts are no longer compared with a
reference.

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
`BTreeSet<(EvictKey, slot)>` per class and per copies held (both, GPU only, decoded only) — a hand-written
intrusive treap at first (#168), replaced by std's ordered set in the debt pass (#170) — and its GPU bytes counted as its
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

The pressure scenario's I7 break, explained (left as found through O1, where behaviour was frozen; fixed after
O1.4 — see *The decoded tier retargeted* below): at t ≈ 105 s of the unlooped tour under `pressure_profile`,
1 336 of the 1 929 tracked resources held only a decoded copy (no GPU copy, nothing in transit or loading; 984
of them not wanted), and the decoded tier sat at 534 MB, just under its 512 MiB target. That target is
`BudgetConfig::interactive`'s quarter of the configured GPU total (2 GiB), and `retarget` did not scale it:
`eff()` scaled the decoded target by pressure only, not by the retarget factor. So on a 384 MiB budget the
decoded tier still kept 512 MiB of copies demoted by evictions, and every one of them is a tracked resource that
the simulation's I7 bound (GPU-drawable + pending + need) does not count.

**O1.3 — need deltas** (`perf/budget-o1-need-deltas`). The need no longer arrives whole each step to be sorted,
deduplicated and reinserted (O(N log N) for a still view). `NeedDelta` (in `tuile-budget`) says what changed
since the step before, tile by tile: the tiles that entered and those whose entries changed, each with all its
entries now (membership and need, in the need's order), and the tiles that left; a change is any entry more or
less, another membership, or any field of a need, floats compared bit for bit. `NeedDelta::diff(before, after)`
builds one from two whole needs, and `apply_to` gives the whole need back. `FrameInput::from_delta` passes one
(`FrameInput::new` still passes a whole need: the first step, a resynchronisation, a caller with whole needs
only), and the geometry server (`runtime.rs`) sends the whole need on its first step and deltas after, diffed
against the need it kept. The budget keeps the need it last read (`need_state::NeedState`): each wanted tile's
best entry in a sorted vector, which the step reads as it is — no per-step copy. A delta resolves only the tiles
it names (best entry first, the first of equals; tree position from the last entry, as before), merged in one
pass; a whole need is applied the same way, every tile named. The tree positions (`tiles`) become a sorted vector
of packed 48-byte entries, updated only for the tiles named. Same decisions: lockstep green in every `budget_sim`
run (the simulation sends deltas, the frozen engine the whole need) and the properties (2 048 cases, one step in
four a whole need, the rest deltas), goldens and pattern goldens untouched. In debug builds the kept need is
checked every step against a recomputation from the whole need (kept beside, rebuilt by `apply_to`), and every
wanted tile's tree position too. Red seen: a delta whose changed tiles are not applied diverges at step 3 in 60 of
61 `budget_sim` runs, fails the property, and fails `the_need_kept_through_deltas_is_the_whole_needs` at its
first step.

| Measure | O1.2 | O1.3 |
|---|---|---|
| `decide` still (484 drawn), told what changed (nothing) | mean 809 µs, p50 798, p99 884 | mean 771 µs, p50 761, p99 851 (−5 %) |
| `decide` still, told the whole need (`still-full`) | (the only path) | mean 807 µs, p50 797, p99 889 (no change) |
| `decide` motion | mean 680 µs, p50 605, p99 1 618 | mean 698 µs, p50 626, p99 1 618 (+3 %) |
| `decide` pressure | mean 464 µs, p50 454, p99 716 | mean 467 µs, p50 458, p99 709 (no change) |
| allocations per step, still (internal / output) | 13 / 649 | 13 / 649 |
| allocations per step, motion p99 (internal) | 38 | 38 |
| allocations per step, pressure p50 / p99 (internal) | 12 / 74 | 12 / 74 |
| step buffers after the tour | 381 056 B | 349 824 B |
| bytes per tracked resource | 445 (2 927 tracked) | 432 (2 927 tracked) |
| P6.4 `work`, still view (nothing changing) | ≈ 1 470 a step, worst 4 646 | ≈ 910 a step, worst 4 080 |

Criterion: `decide/still` −5.1 % (p < 0.05). The gain is small, as expected: everything after `read_need`
(imagery levels, draw set, wants, ranking, prune) still walks the whole need every step until O1.4's dirty sets.
In motion every wanted tile's SSE and distance change every step, so a delta names nearly the whole need and the
budget resolves it all as before; the step is about 3 % slower there (the larger kept record, 96 bytes against
the 40 of the old borrowed one, and the sorted vectors' merges when tiles enter), and the diff the host now
computes (outside `decide`) is not in these numbers. `work` counts what the delta names instead of every entry:
on the still view, about 560 less a step. P6.4 stays ignored (red on 1 488 of 5 100 steps, all still, against
1 492 before) until O1.4. Memory: the kept need adds about 65 KB on the tour (607 tiles), paid for by the tree
positions' sorted vector of packed entries (1 841 tiles, 214 KB as a `BTreeMap` → about 103 KB) and the step's
borrowed copy of the need gone.

**The debt pass after O1.3** (`perf/budget-o1-debt`, on top of the stack). One commit per debt, decisions
bit-identical (lockstep green, goldens and pattern goldens untouched, ratchets only down):

- `SessionError::resource()` matched exhaustively in the crate (#163's wildcard in the runtime gone);
- the lockstep compares the whole internal state each step (`tuile_budget::snapshot`), not only outputs (#165):
  seen red with a failure count not reset on success, which the old lockstep passed on all 61 `budget_sim` runs and
  the new one fails on 54;
- the eviction index on `std::collections::BTreeSet` instead of the hand-written treap (#168), Laurent's decision:
  decisions identical, `decide` within noise (still +4.9 % by criterion, motion and pressure unchanged), bytes per
  tracked resource 432 → 470 and motion allocations p99 38 → 39. About 76 KB and one allocation a step for the
  view's ~2,000 tracked resources, against an established structure in place of a hand-written balanced tree and
  its audit: the two ratchets were raised deliberately, once, in that change;
- the host's diff (`NeedDelta::diff`, what the runtime computes before `decide`) measured (#169): about 22 µs
  still, 33 µs in motion, and about 640 heap blocks a step on the tour (each changed tile's `Need` cloned);
- motion back under O1.2 (#169): the kept tiles and the tree positions as key/value columns (`SortedCols`, lookups
  in a dense key column), entering tiles merged in place (no buffer), the kept record 88 → 80 bytes;
- the need state's memory rules each kept with the number that justifies it (#169).
- the frozen copy of the old engine retired (debt of the whole guard): its per-step digests recorded first, then
  the engine and its `FrameInput` twin deleted; the file-based check seen red on an internal mutation (54 of 61
  `budget_sim` runs and the corpus, part `state`) and on an output one (60 of 61, parts `decisions`, `ledger`,
  `views`, `state`).

`budget_decide` per layer, the same binaries run back to back five times on the same (shared, loaded) Apple M2,
best of five (noise only adds time); `decide+diff` is a delta step's whole cost to the host:

| Layer | still | still, whole need | motion | pressure | decide + diff, still | decide + diff, motion |
|---|---|---|---|---|---|---|
| O1.0 | 1 097 / 1 195 | (the only path) | 904 / 2 174 | 913 / 1 654 | — | — |
| O1.1 | 814 / 893 | (the only path) | 695 / 1 730 | 652 / 1 125 | — | — |
| O1.2 | 812 / 902 | (the only path) | 680 / 1 619 | 466 / 723 | — | — |
| O1.3 | 774 / 867 | 809 / 892 | 699 / 1 620 | 471 / 720 | not measured then | not measured then |
| debt pass | 721 / 789 | 749 / 815 | 654 / 1 518 | 429 / 658 | 742 / 822 | 686 / 1 584 |

(mean / p99, µs.) Allocations (release `budget_sim`, counting allocator):

| Layer | still internal / output | motion p99 | pressure p50 / p99 | step buffers | bytes / tracked resource |
|---|---|---|---|---|---|
| O1.0 | 13 / 649 | 43 | 21 / 76 | 381 056 B | 534 |
| O1.1 | 13 / 649 | 42 | 21 / 73 | 381 056 B | 452 |
| O1.2 | 13 / 649 | 38 | 12 / 74 | 381 056 B | 445 |
| O1.3 | 13 / 649 | 38 | 12 / 73 | 349 824 B | 432 |
| debt pass | 13 / 649 | 38 | 12 / 74 | 340 096 B | 432 |

**M-run** (native headless runner, `p2-slow`, Paris 160 km → 5 km, desktop 1280×800 @2, imagery `Off`, Metal,
`TUILE_REQUIRE_GPU=1`; the six builds interleaved, three runs each, median). Measured on a machine shared with
another heavy job (load average 15–50 during the runs: frame times are inflated for every build alike), so the
rows compare builds, not absolute frame budgets. "Every upload landed" is the runner's settle time plus its idle
wait. `main` is 0.15.1; the stack is based on 0.15.0 (`#163`'s base), so `main` differs from the stack by more
than the budget.

| Build | imagery on the GPU at its peak | every upload landed | frame p99 | frame p50 | obligations |
|---|---|---|---|---|---|
| `main` (2f9d240b) | 161.1 MB | 1.2 s | 78 ms | 39 ms | kept ×3 |
| #163 (crate move; O1.0 is the same binary) | 171.3 MB | 1.4 s | 73 ms | 32 ms | kept ×3 |
| O1.1 | 172.0 MB | 0.7 s | 72 ms | 32 ms | kept ×3 |
| O1.2 | 172.3 MB | 0.8 s | 70 ms | 39 ms | kept ×3 |
| O1.3 | 173.0 MB | 1.0 s | 67 ms | 40 ms | kept ×3 |
| debt pass | 173.0 MB | 1.3 s | 62 ms | 33 ms | kept ×3 |

No black pixel in any of the 18 settled pictures. Within the noise, the layers do not move the runner's numbers:
the budget's `decide` is a fraction of a millisecond in frames of tens.

**M-web** (`scripts/web-bench.py`, headless Chrome, 1280×800 @2, the ten patterns), `main` and the debt pass:

| Build | Configuration | Patterns kept | Convergence median / max | Holes (moving frames, black pixels) | Violations |
|---|---|---|---|---|---|
| `main` | WebGPU auto | 10/10 | 8.0 / 15.0 s | 0, 0 | none |
| `main` | WebGL2 auto | 9/10 | 10.5 / 15.0 s | 0, 0 | hand: one duplicate concurrent request |
| `main` | off | 10/10 | 6.0 / 15.0 s | 0, 0 | none |
| debt pass | WebGPU auto | 8/10 | 10.1 / 28.1 s | 0, 0 | yaw, tilt: convergence over bound |
| debt pass | WebGL2 auto | 10/10 | 9.5 / 17.0 s | 0, 0 | none |
| debt pass | off | 10/10 | 7.5 / 16.0 s | 0, 0 | none |

The convergence violations follow the machine's load, not the build: `main`'s first WebGPU auto run, under a load
average near 100, missed the same two bounds (yaw 39.5 s, tilt 22.2 s) and the run here was taken at load 13; a
duplicate concurrent request appeared once on each build. Neither build left a hole or a black patch. Per-layer
M-web was not run (a wasm build and a 25-minute run per layer, on a loaded machine).


**O1.4 — dirty sets** (`perf/budget-o1-dirty-sets`). The passes after the need — drape levels, the floor,
the selection, the draw set, the wants and refreshes, the marks, the work — keep their results from one step to
the next (`downstream.rs`). A unit (a tile's levels, a drawn tile's selection, a mesh's draw item, a tile's wants,
a wanted resource's work) is read again only when what it read may have changed: its tile was named by the need's
change (for its shape or place: membership, level, class, layers, parent, footprint; a tile named for its floats
alone has only its levels and wants read again); its tile's resources changed (an event, an eviction, a delivery,
a prune: the arena notes every slot's changes); it read another tile and that changed — each unit's reach is kept
and indexed by tile, which finds the descendants falling back on a tile and the parent a finer tile holds; a time
it waits on fell due, through queues ordered by time instead of scans (a retry or a delivery's backoff, a load's
timeout or an unneeded resource's grace, the 30 s an empty slot backing off is kept); or something every unit
reads moved (the LOD scale, the pressure, an epoch, Batch). Kept outputs are patched — a unit's old contribution
out, its new one in: statistics as sums, drapes worn as counts, the drawn, wanted, swap and chain sets as marks
on the arena's slots. A whole need is read as a delta (it names the tiles it lists and those it dropped). The
prune keeps its eighth-step cadence but walks only the slots emptied since or whose time fell due, the recent
evictions in time order, and the tiles its counts of what keeps each let go (a cycle among the parents falls back
on the whole walk). In a debug build every pass is recomputed from scratch each step and compared, as are the
work sets and their waits, the marks, the held ancestry, the prune's counts and the slots it frees; a release
build with debug assertions runs these checks on every `budget_sim` scenario. Decisions and the whole internal
state unchanged: the recorded digests match on every `budget_sim` run and the corpus, goldens and pattern goldens
untouched. P6.4 (`decide_work_grows_with_changes_not_with_the_view`) is no longer ignored: a 16, b 256 hold on all
5 100 steps (red before on 1 499, worst W 19 329 against 256).

Red seen, each propagation rule removed in turn: the parent's (held ancestry changed at a step's end), the
descendants' (an ancestor's mesh), the drapes' (an ancestor's drape) and the place's (an ancestor's footprint)
each fail a scenario of their own in `budget_core` (`dirty_sets::*`, on the decision in release, on the full
recomputation's check in debug); the drapes' also diverges from the recorded digests on 34 of 60 `budget_sim`
runs. Resource events: 20 `budget_core` tests and 54 of 60 runs. The deadline queues: a load's timeout (one
`budget_core` test, 6 runs), the retry/backoff wake (3 tests, 42 runs), the prune's re-look when a kept slot's
want goes (48 runs). The worn rule (units that read a tile's last item are read again when it changed) has its
own scenario since the debt pass: a tile's selection reads the drapes an ancestor's mesh wore last step.

First measured at 1265049b, before the debt pass below:

| Measure (Apple M2, shared; best of 5 interleaved rounds, mean / p99 µs) | main (#170) | O1.4 |
|---|---|---|
| `decide` still (484 drawn) | 911 / 1 002 | 45 / 47 |
| `decide` still, told the whole need | 952 / 1 032 | 375 / 412 |
| `decide` motion | 815 / 1 925 | 452 / 1 212 |
| `decide` pressure | 539 / 828 | 402 / 941 |
| `decide` + host diff, still | 974 / 1 051 | 71 / 72 |
| `decide` + host diff, motion | 856 / 2 012 | 486 / 1 283 |
| host diff alone, still / motion (mean) | 27 / 40 | 25 / 33 |
| allocations per step, still (internal / output) | 13 / 649 | 0 / 649 |
| allocations per step, motion p99 (internal) | 40 | 31 |
| allocations per step, pressure p50 / p99 (internal) | 13 / 83 | 2 / 82 |
| step buffers after the tour | 340 096 B | 244 216 B |
| bytes per tracked resource | 470 | 467 |
| P6.4 `work`, still view (mean / worst) | 16 524 / 19 329 | 9 / 5 409 |
| P6.4 `work`, tour (mean / worst) | 18 453 / 25 194 | 5 297 / 12 990 |

Load average 9–39 during the rounds; the pressure p99 moves within the rounds' spread.

**O1.4's debt pass** (PR #175, 2026-11-01). The worn rule got its red scenario; the session's presence oracle
tells a generation (answers kept per one-second window or loader epoch, so a still view reads no level again
within a window); the prune's cycle path has a test; `check_marks` reads the pass's own swap and chain rule. The
sorted vectors' upkeep is now counted in `work`, and two changes keep P6.4 green with it: the held ancestry is the
held set itself — geometric walks filed by a Morton key whose quadtree descendants are one interval, the others
apart and walked — instead of its closure (~3 000 pairs, ~18 rewritten per held change); and the kept maps are
sorted blocks of 32 entries (keys and values as two columns, an index of first keys, a one-block fast path, a
cached last block, one spare block), each counting what it shifts. P6.4's #E_k counts the decisions of the step
before as well (their effect is read one step later, as an event's). Cost on x86 (16 reserved EPYC-Milan CPUs,
against the held-set commit): decide means +3–5 %, +21 % on the whole-need resync (still-full 367 → 443 µs), p99s
6–9 % lower; arm64 at parity but for the resync (+4–7 %). Ratchets once: bytes per tracked resource 467 → 455
(470 on `main`), step buffers after the tour 244 216 → 236 688 B (340 096 on `main`), allocations p99 31 → 30.

M-run (`p2-slow`, Off, Mac, load 4–8, five interleaved runs, at a920e258): imagery peak 170.2 MB (`main`) against
171.3, converged 0.57 against 0.60 s, slowest frame 63 ms (59–65) against 63 (55–67), median frame 35 against
35 ms, no black pixel and no hole. M-web (WebGPU auto, WebGL2 auto, off; load 5–8, at c9572f88, before the
held-set and block commits, whose decisions are digest-identical — convergence and holes follow the decisions,
the page's timings are not re-measured): patterns kept 6/8/10 on `main` against 10/9/10, no hole or black patch
on either; the one O1.4 violation is WebGL2 yaw converging in 22.1 s against 20 s.

**Arena capacity returned** (stacked on #175). The arena's columns and index grew to the session's high-water
mark. Now, when the tracked resources stay under a quarter of the room for eight prunes (64 steps), the arena
renumbers its live slots in order and cuts its columns to twice what they use; every set keyed by slot follows.
Decisions and the whole state are unchanged (digests, goldens). The flight that grows the tracked set to 4 889
and falls back to 995 is a recorded scenario of its own
(`the_arena_gives_its_room_back_on_the_flight_that_shrinks_the_tracked_set`: the room, read through
`Budget::tracked_room`, stays over four times the set for 56 steps, then falls once, at t = 41.05 s, from 5 378
to 1 178 slots for 589 tracked — twice the set — and the budget ends at 995 tracked in that room; its decisions
and whole-state digests are in the recorded reference, and the checked simulation runs the renumbering's
assertions on it every step). After the flight the budget holds 870 B per tracked resource instead of 1 664 (new
ratchet, bounded by twice the steady ratchet read from the ratchets file). The hysteresis has its own test: a
set hovering around the quarter never compacts, a compacted arena is filled and grown without one, and the next
compaction waits for eight sparse prunes again (`the_compaction_waits_for_a_set_that_stays_small_and_does_not_thrash`);
the arena's compaction test carries transits, loads and ranks through the renumbering; `SortedMap::rekey` has a
property against the standard map renumbered. A compaction's entries moved count in `work`. P6.4 run once on the
flight: the compaction step is within its bound (W 13 370, carried by the background's evictions the step
before); 7 of 3 600 steps are over it, none of them the compaction — five still steps after the descent
(W 308–414 against 256, nothing changing) and the two pressure switches (W 4 416 and 2 072: a pressure change
reads every unit again, which P6.4's #E does not count) — a finding outside P6.4's two runs, left as is. The soak,
the tour, the pressure scenario and P6.4's run never compact.

**The decoded tier retargeted** (stacked on the arena's room; the stack's one deliberate behaviour change). `eff()`
scaled the decoded tier's target by the pressure percentage only, not by the `retarget` factor applied to every
GPU target, overflow and hard limit: a budget configured for a 2 GiB device and retargeted to 384 MiB kept the
configured 512 MiB of decoded copies — the copies an eviction demotes to, every one a tracked resource — and the
simulation's I7 bound on tracked state broke on the tour under `pressure_profile` (above). The tier is now a
target like the GPU's, `scale(decoded_target, pct)`: retargeted and scaled by pressure, `Bytes::MAX` left alone.
Red first: `the_decoded_tier_is_retargeted_with_the_gpu_limits` (six meshes decoded on a tier of eight, the
device retargeted to half: 614 400 B kept against 409 600) and
`the_tour_under_pressure_keeps_its_tracked_state_bounded` (the whole tour, 180 s, on 384 MiB of 2 GiB, every
invariant checked, the tier bounded by its retargeted target plus the copies in transit every step: red on the
tier at t = 0.3 s, and on the simulation's own I7 at t = 105.55 s with that assertion out). On the pressure tour:
the decoded tier 534 MB → at most 100.7 MB (96 MiB plus copies in transit), tracked at most 1 339 (1 968 before,
at t = 105 s), evictions over the first 105 s 5 078 → 5 997, decoded releases 8 129 → 83 091, 3 049 re-reads from
the store within 10 s of an eviction over the run, no network re-fetch. The looped pressure scenario: over target
on 2 883 of 3 000 steps (2 879), evicting on 688 (551), 537 tracked (1 178). Recorded digests re-blessed for the
one run that retargets (`the_pressure_scenario_stays_over_target`) and the new run added; every other run, the
goldens and the pattern goldens byte-identical; ratchets untouched.

**Failures bounded** (stacked on the decoded tier; decisions byte-identical). After a failed load a resource backs
off (500 ms doubling to 30 s) and the prune keeps its empty slot while it backs off, for 30 s at most once its
want went, so that a server that is down is not asked again the moment a tile comes back into view; only that
window bounded those slots. They are now capped by count, and so by the bytes of their metadata:
`BACKOFF_KEEP_MAX` = 128 KiB / `BACKOFF_KEPT_BYTES` (a slot across the arena's columns, its index entry, the kept
map's entry, its queue entry, and the entry of its tile's place, which a kept slot keeps alive: 228 B) = 574 slots,
under a fifth of a steady desktop view's tracked state; left out, stated as bounded multiples of the same count,
the index's spare room (hashbrown's 7/8 load at a power of two) and the queue's stale entries (at most twice the
live ones plus 64). Past the cap the prune lets go the slots whose want went longest ago — ties broken by the
resource, the crate's final tie-break, never by the slot number, which follows the free list and the compactions
(a whole place leaves on one step, so ties are the normal case) — into a step buffer whose room persists like
every step list's (no index kept for it), made only when the kept slots together, wanted ones included, outnumber
the cap; their failure history goes with them — a want that comes back retries at once; the wanted ones are the
view's and never let go for the cap. Red first: `the_slots_kept_for_their_failures_are_capped_the_longest_left_first`
(twice the cap of failed, backing-off, unneeded slots through one prune: 1 148 kept without the cap, the debug
assertion; 574 with it, the most recently left) and `the_cap_keeps_the_same_resources_whatever_the_slot_numbers`
(the same failures through a fresh arena, one whose free list reversed the slot numbers and one compacted first:
the same resources kept; keyed by the slot, the churned arena kept the other half of the place the cap cuts). The
simulated network has an outage (every load fails at once for a window) and a recorded scenario,
`a_network_outage_leaves_the_tracked_state_bounded_and_recovers`: half a minute of outage while the camera jumps
to a new place every two seconds, then the network back and the camera still; after every prune, through the
outage and the recovery, the kept slots are checked against the cap and the tracked resources against what
accounts for them (wanted, holding a copy, loading or in transit, kept within the cap), and the view converges
once the network is back. No simulated outage reaches the cap — while nothing arrives the traversal wants a
place's coarse tiles and fails them over and over, so the kept slots follow the places visited within their
backoff: 384 at most on the two-second jumps (3 949 failures; cap 574), 200 on the grow-then-shrink flight, 26 on
eight-second jumps — so the scenario holds the cap without a red of its own; the cap's red is the unit test's, and the number is not lowered to make a scenario trip it.

**O1.5 — the step's outputs allocate nothing once grown** (`perf/budget-o1-outputs`, part 1 of O1.5; decisions and
the whole state byte-identical). Profiled first (samply on the bench, `Budget::step` in motion): the passes after
the need 42 % of the step, the wants map's lookups 27 % (`of_tile` twice per named tile, a tile's mesh and drapes in
two regions of a map ordered by resource, which defeats the block hint), `apply_delta` 8.5 %, the draw set's
construction 6.6 %, malloc 1.2 %; the host's diff ~640 blocks a step. O1.5 is split by that weight, one stacked PR
each: (1) the outputs, this part; (2) the wants and refreshes keyed tile-first, `imagery_prev`, `active` and
`recent` on the sorted blocks, the whole-need resync by a cursor merge over the blocks, the ranking's bucket queue
decided on the measurement (the ranking is under 2 % of the step); (3) the runtime's presence answers kept until a
store event — a network load completed, a store miss, the loader's epoch — instead of the 1 s window; (4) P6.4 on
the flight: the pressure switches (only the wants that read the pressure read again) and the five still steps after
the descent, and the prune's scan of the loads in flight replaced by the arena's notes.

Part 1: `Budget::decide_into(input, &mut Decisions)` writes a step into the step before's decisions — emptied
first, every list refilled in place, the draw set's items overwritten one by one (their drapes and tiles served in
their own room), the plan's groups overwritten and cut to those used — and `decide` is `decide_into` on a fresh
`Decisions`; the geometry server keeps one `Decisions` and hands it to every step, the simulation and the bench
too. `DrawItem` holds its drapes (one or two) and the tiles it serves (up to four) in place (`smallvec`, a public
type change followed in tuile-core, tuile-wgpu, tuile-web and tuile-bake): a fresh copy of a still step's decisions
takes 1 block instead of 649. The plan no longer builds a map of groups and a vector per group: the transits with
their key, bytes and group in a step buffer sorted by key, their places by group in a buffer lent for the step's
end, a group taken whole at its first member and its members flagged. The jobs buffer loses the resource its key
already holds (80 → 64 B an entry). `NeedDelta::diff` writes over its earlier entries (`Need::clone_from` keeps
the layers' buffer; the lists keep their length apart), so the host's diff allocates nothing. Red first:
`a_step_into_the_last_decisions_is_the_step_into_fresh_ones` (two rigs, fresh against reused decisions, every
`Decisions` compared whole over a view that grows and shrinks; red with the buffers not emptied),
`a_delta_built_again_says_only_what_changed_since` (red with the earlier entries still listed), and the still
ratchet at 0 (red with `decide_into` deciding into a fresh buffer and moving it). The ratchets are renamed for what
they count — every block of the call, its output written into the host's buffers — and every one is down or equal:
still 0 (the output's 649 gone), motion p99 30 → 23, the host's diff p99 (new) 0, step buffers after the tour
236 688 → 231 696 B, bytes per tracked resource 451, after shrinking 870; the pressure scenario's blocks per step
p50 33 → 28, p99 115 → 81. M-bench on x86 (16 reserved EPYC-Milan CPUs, 5 interleaved rounds, medians, against the stack's tip):
decide/still 65.0 → 41.5 µs (−36 %: the output's 649 allocations and the draw set's rebuild), still-full 441 → 413
(−6 %), motion 711 → 705 and pressure 511 → 519 (within the builder's band: a motion step's cost is the passes,
part 2), decide + the host's diff in motion 589 / 1 341 → 562 / 1 287 (mean / p99), the diff alone 60.3 → 36.4 µs
(−40 %). M-run (`p2-slow`, Off, Mac at load 3, five interleaved runs): imagery peak 172.3 → 172.0 MB, every upload
landed 0.8 → 0.7 s, slowest frame 64 → 64 ms, median 36 → 35, no black pixel and no hole in any run.
M-web (the ten patterns, headless Chrome, the display held awake — asleep, it hides the page and the viewer holds
its loads): patterns kept 6/9/10 on the stack's tip (WebGPU auto / WebGL2 auto / off) against 9/10/10, no hole or
black patch on either; the one violation left is WebGPU's warm relaunch converging in 21 s against 20.

Part 2 (`perf/budget-o1-wants-by-tile`): the maps a step reads for every tile, read in one pass. The wants and
the refreshes are keyed tile-first (`ByTile`: the tile, then the resource): a tile's mesh and drapes are one range
of the map, read in one pass, and consecutive tiles read consecutive ranges, so the sorted blocks' hint holds
through the wants pass and the mark (keyed by resource, every mesh came before every drape: two regions, two
lookups per tile, the hint defeated at each). The loads and transits in flight are a list of slots (pushed when a
load or a delivery starts, the slot's `Active` mark saying whether it is listed; retained at the prune; renumbered
with the compaction; its room given back when it empties), where a `BTreeSet` of resources was looked up and
allocated nodes. The whole-need resync walks the kept need and the tree positions with a cursor each
(`sorted::Cursor`): the tiles come in order, so one comparison an entry passed finds each, the tiles passed between
two named ones being exactly those that left — the +21 % the sorted blocks had cost the resync (#175) given back
and more. Tried and not kept, numbers at the declarations: `imagery_prev` on the blocks (its upkeep, counted, put
P6.4 over on the big zoom, 115 978 against 113 728, and on a still step by one unit) and `recent` on the blocks
(one block less at motion's p99, 19 B more per tracked resource after the shrink flight: entries expiring scattered
by key leave the blocks half full). The ranking's bucket queue is not taken: every job's exact key is needed each
step (the ranks map, the views digest, the preemption's victim), so a bucket queue would replace only the final
sort, under 1 % of a motion step. Red first: `a_tiles_resources_are_one_range_of_the_wants_map`; the cursor's
property against the standard map (which, with the resync test, caught the cursor left on a found key while it was
written). M-bench on x86 (5 interleaved rounds, against #183): decide/still 45.0 → 38.6 µs (−14 %), still-full
413 → 324 (−21 %), motion 710 → 642 (−10 %), pressure 515 → 477 (−7 %), motion p99 1 211 → 1 120, pressure p99
946 → 887. Ratchets: step buffers after the tour 231 696 → 229 648 B, the rest equal, but the bytes per tracked
resource after the shrink flight 870 → 872: one emptied block the wants map keeps for its next split (the pool,
one per map by design) that the flight's history under the tile-first key leaves behind where the resource order
did not — raised deliberately, Laurent's decision.

Left for the next parts of O1.5: the whole-need path groups the need into a fresh buffer each time (one block:
keeping it would add the first step's grouping to every delta host's step buffers); P6.4
on the flight and the prune's scan of the loads in flight.

Part 3 (`perf/budget-o1-presence-events`): the presence answers kept until a store event, not for a 1 s window.
The session dropped every answer once a second and asked every drape again — 156 asks a second on a still view,
an answer up to a second stale. `TileLoader::store_events` (a count that moves with every change to what `holds`
reads) and `store_changes` (which of the answers kept the events since a count may move, given each answer) let
the session's `PresenceCache` ask again, at the first step after an event and before the budget asks anything,
exactly the answers an event names; the oracle's generation moves when one changed (or the epoch dropped them
all), so a still view asks the loader nothing and reads no level. The planetary loader notes its changes as it
makes them — a decoded mesh or imagery tile kept or let go, a store's index changed by a fetch through it — with
the way each went (a tile kept moves only absent answers, one let go only held ones), and polls what changes
without it: the imagery switched, and the availability's ranges folded in (`Availability::added_since`, a log in
tuile-terrain). A mesh is touched by a change on its climb (up to the first tile decoded or the source's), or by
availability revealed at or above it; a drape by a decoded tile over it, or a stored one at its floor level or its
mosaic's. Not seen, and so the bound that remains: an entry a store drops on its own (foyer's disk-tier eviction,
a store suspended, another writer) — a held answer believed until the resource is loaded (its read then goes to
the network under the local quota, once, and the load puts it back) or a change near it is noted. M-run
(`p2-slow`, three interleaved runs per mode, 10 s held still after settling): still view 156 → 0 asks a second,
decide in the still phase 94 → 81 µs (off); over the run, the loading burst asks more (off 6 818 → 10 460 asks
in a run, 406 → 640 a second: every answer an event touches, the moment it is touched, where the window refreshed each once a
second, late) for the same decide time, imagery peak, frames and convergence.

Part 4 (`perf/budget-o1-flight`): P6.4 on the flight. P6.4 run on the flight that grows the tracked set and falls
back (`decide_work_grows_with_changes_not_with_the_view_on_the_flight`) was over its bound on 7 of 3 600 steps, and
across changes of pressure on a still view (`a_change_of_pressure_reads_again_only_the_wants_that_read_it`) on 9
of 720. Three causes, each fixed with decisions and the whole state byte-identical (two runs added to the recorded
reference, every other line unchanged): (1) a change of pressure read every tile's wants again (W 1 467 to 4 454
against 256 with nothing else changing); the wants keep the tiles that read the pressure — a drape worn one level
finer than its need — and a change to or from `Normal` reads only those (a change between two pressures under
`Normal`, none); (2) the thrash window's expiries all landed on the prune ten seconds after a burst of evictions
(the five still steps after the descent, W 308–414, and 2 072 ten seconds after the background's 2 804
evictions): the prune now moves a horizon and the entries behind it leave `recent` 64 a step (`RECENT_DRAIN`),
the snapshot and the thrash count reading only what is past the horizon; (3) the prune scanned every load and
transit in flight every eighth step: the arena keeps the active slots in two runs (busy, ended), each slot knowing
its place in `Cold`'s spare room (still 48 B), moved the moment its last load or transit ends, and the prune drops
the second run whole. P6.4's #E now counts, deliberately and each with its reason in the test, a release as a
decision (as an eviction), the evictions and releases a change of pressure forces on its own step (one input,
whose effect is a set: no algorithm takes them in less than a unit each), and on a prune step the events and
decisions since the last prune plus the slots it frees, read the next step. M-bench (x86, 5 interleaved rounds,
against `main`): at parity — still 39.1 → 39.0 µs, still-full 329 → 330, motion 642 → 648, pressure 478 → 479,
motion p99 1 121 → 1 110, pressure p99 888 → 900: the work removed was the flight's spikes, which the bench's
scenarios do not hold. Ratchets unchanged (the readers' vector gives back its room past four times its use: one
reader in a room of 64 at the tour's end was the one byte per tracked resource over the ratchet). Not reached:
motion's 23 blocks a step at p99, untouched by this part.

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
