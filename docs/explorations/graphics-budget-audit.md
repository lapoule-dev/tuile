# Budget module: Rust anti-patterns and avoidable cost

Target: `crates/tuile-core/src/budget/` at `origin/main` 7b0ac83 (PR #71), 16 files, about 5,900 lines. All `file:line` references are to that commit.

Method: I read every file of the module plus the budget tests (`budget_sim/soak.rs`, `server.rs`, the goldens) through `git show`. The main checkout was not touched. Clippy ran twice on a copy of `tuile-core` and `tuile-b3dm` extracted with `git archive` into the scratchpad, with its own target directory and `-j 2`, after checking that no other cargo or rustc process was running. Nothing was built or run in the repository.

**No runtime measurement was taken.** Allocation counts, lookup counts and impact figures below are estimates from reading the code under a stated workload model. The harness in section 8 turns them into measurements, and it should be built before anything is prioritised on the strength of these numbers.

Workload model for the estimates: about 500 drawn tiles, 1 imagery layer, about 800 need entries (E), about 1,100 wants (W), tree depth about 12, about 4,000 tracked entries (T), about 60 active loads and transits (A). Motion adds about 100–200 drawn tiles not yet drawable (M).

Impact classes:
- **step**: once per `decide`.
- **item**: once per drawn tile or need entry, every step.
- **delivery**: once per delivery or load inside a step.
- **periodic**: one step in eight.
- **resource**: memory held per tracked resource.
- **cold**: once per scrape, publish or test.

---

## 1. Summary

Clippy confirms the main point. The `perf` group, `redundant_clone`, `needless_collect`, `large_types_passed_by_value`, `trivially_copy_pass_by_ref`, `needless_pass_by_value`, `implicit_clone`, `unnecessary_to_owned` and `clone_on_ref_ptr` all report **zero hits** on the module. The cost is not in local idioms. It sits in three structural patterns that no lint can see:

1. **Two quadratic walks in `select()`.** For every wanted tile that is not shown, the code scans all of last step's held tiles. Separately, `dressable()` checks whether a drape exists by running the full `best_drape_for` ancestor walk, with 3–6 allocations, for every drawn tile, every layer and every step. When it is called on ancestors this becomes O(depth²). These are the most likely sources of both the mean cost and the 6.9 ms p99 during motion.
2. **Every derived structure is rebuilt from scratch each step.** About 15 `BTreeMap`/`BTreeSet` values are built per step, several of them cloned. Most are only probed by key afterwards. Each drawn item also gets 3–4 small heap containers. Estimated total: **8,000–10,000 heap allocations per `decide`** in steady state at 500 drawn tiles. Roughly a third of them come from point 1.
3. **Full scans over all tracked entries on hot paths.** `plan()` scans everything every step. `make_room()` scans and sorts once per delivery when a class is at its limit. Preemption scans once per preempting load. The eviction sweeps scan once per class. `prune()` scans everything one step in eight. Each of these could use an index that already exists (`active`) or a single per-step ordering.

Unchanged:
- No `unwrap`/`expect` in non-test code.
- No `format!` or `String` anywhere in the module.
- No `Arc`/`Rc`.
- No `HashMap` (`clippy::iter_over_hash_type`: 0 hits).
- No float in any ordering decision in `engine.rs` (`clippy::float_arithmetic`: 0 hits there).

The determinism design holds. Every optimisation proposed below keeps it, and section 6 lists the traps to watch.

---

## 2. Top 15, prioritised

| # | Where | What | Why it costs | Fix | Impact |
|---|---|---|---|---|---|
| 1 | engine.rs:733-738 (+585-603) | `held_last.iter().filter(is_descendant && shown)` for every drawn tile that is not shown | O(M × \|held_last\| × depth) BTree lookups. `held_last` holds every Exact and Held mesh from last step (about N), so it is quadratic during motion. Estimated 100k–500k lookups at M=200. | Once per step, walk each held tile up its parents and push `(ancestor, held)` into a reusable sorted Vec. Then each tile does one range lookup. Hoist `tiles.get(&of)` out of `is_descendant`. | step, p99 (motion) |
| 2 | engine.rs:686 → 619-671 (+745, 606-617) | `dressable()` checks existence with `best_drape_for(tile, l, 0)` | Asking for level 0 means the own-drape fast path almost never hits. Each call walks the whole chain: `vec![tile]` + `ancestors()` Vec + `cands` Vec, one `entries.range` per ancestor and a `UvTransform::between`. `shown()` then calls it again on each ancestor tried, so finding a fallback is O(depth²). | Add `has_drape(tile, layer) -> bool`: a lazy ancestor iterator (`successors`, capped at 64) with `any()` and an early exit, own tile first, one range probe, no allocation. Memoise per (tile, layer) within the step, since siblings test the same ancestors. Make `ancestors()` an iterator. | item (mean), step (motion) |
| 3 | engine.rs:1567-1588 | `make_room()` collects all GPU residents of the class, then runs a stable sort, once per delivery | O(deliveries × T log T) whenever a class sits at its limit. That is exactly the steady state under pressure. The stable sort also allocates scratch. | Compute the class's eviction order at most once per step, lazily, and consume it with a cursor; apply the `keep`/`wants`/urgency filter while consuming. End state: a persistent cold-eviction index (§7). | delivery → step, p99 |
| 4 | engine.rs:2121-2160, 2137-2146 | `prune()` one step in eight: `entries.retain`, `active.retain`, `tiles.retain`, a `keep` BTreeSet over all tiles, a `seeds` Vec copy, plus the `recent` cap loop | A periodic O(T + tiles) spike, which is a structural p99. The cap loop runs `min_by_key` over the whole map once per removal: O(excess × 4096). | Spread the work: prune k entries per step with a cursor (`range(cursor..)`), refcount tiles from entries and need, and order `recent` by `(Micros, ResourceId)` or sort once and drop the excess. | periodic, p99 |
| 5 | engine.rs:1786-1847 | `plan()` scans every entry for transits. It clones each group Vec per item (1813), clones `members` to sort it (1843), runs `transit.iter().find` per member (1814-1829, O(n²)), and keeps a `taken` BTreeSet. `drapes` is unused (1801). | O(T) every step. The comment at 1209 acknowledges it. Every transit is already in `active` (`deliver_one` inserts it at 1751). | Iterate `self.active`. Store bytes inside each group. Use `groups.remove(&g)` instead of `.cloned()`, which also replaces `taken`. Sort `members` in place. Drop the parameter. Output stays identical, because the result is sorted by `(key, r, b)`. | step |
| 6 | engine.rs:1443-1469 | Preemption victim search scans all entries once per Hole load while the class quota is full. The predicate is duplicated (`A && B && C \|\| A && B && D`). | O(preempting loads × T). Every loading entry is in `active` (inserted at 1511). | Scan `active`, or keep a per-class in-flight set ordered by key. Factor the predicate to `A && B && (C \|\| D)`. | delivery |
| 7 | engine.rs:459 (+462-467) | `need: n.clone()` for every need entry, including candidates that lose. `get` then `insert`, and `sort_key` computed twice. | One heap allocation per tile with imagery (`ImageryNeed.layers: Vec`), about E per step. Two lookups per entry. | `TileNeed<'a> { need: &'a Need }`, borrowed from `FrameInput`. Use the entry API. Compute `sort_key` once. | item |
| 8 | engine.rs:1030-1066, 1261-1271 | `drawn_res` cloned twice (`drawn_last` at 1035, `keep` at 1057). `worn_last` rebuilt with `i.drapes.clone()` per item (1040). `chain_last`, `swap_last`, `held_last` and `wanted_last` each rebuilt with `collect`. `touched` is a materialised union. | Each `BTreeSet: FromIterator` collects a temporary Vec, stable-sorts it (scratch allocation) and allocates nodes. Inputs are already sorted. Adds about N + 1,000 allocations per step. | Use reusable sorted `Vec<ResourceId>` buffers with `clear`/`extend`/`dedup` and `binary_search`. Use `mem::swap` instead of clone. Make `keep` a predicate (`drawn ∪ swap`), not a set. Store `worn_last` as a flat sorted `Vec<(ResourceId, DrapeChoice)>`. Make `touched` a merge-join (§3, A8). | step |
| 9 | engine.rs:674-676, 142 & 713-722, 790-796, 849 | Per-item containers in `select`/`build_draw`: one `BTreeSet<LayerId>` per drawn tile (`layers_of`), one `BTreeSet<TileId>` per selected mesh (then converted to a Vec at 849), one `BTreeMap<LayerId,u32>` per draw item | About 3N small allocations per step for sets of 1–2 elements. | Layers: pass the `drapes.range(tile_layers(t))` iterator or a slice. `serves`: push into a Vec, then `sort_unstable` + `dedup`. `needs`: a small sorted Vec (or `[(LayerId,u32); 2]` + spill). | item |
| 10 | engine.rs:450, 509, 862, 1093, 1099, 1177-1194 | Per-step `BTreeMap`s that are only probed after they are built: `tiles`, `drapes`, `wants`, `fallback_cover`, `keys`, and `floor_groups` (which clones the group Vec per member at 1192) | Node allocations of W/8–E/8 each, plus building cost, every step. | Sorted Vecs in a Budget-owned `Scratch` (cleared, never freed). `fallback_cover` becomes `draw.get(r)` (items are already sorted by mesh). `floor_groups` is computed on demand from `tiles`. | step |
| 11 | engine.rs:1099-1139 | A `PriorityKey`, `score` and `tier()` (which can call `presence.holds`) for **every** want, inserted into `keys`. Only jobs, loading victims and transits read keys. | In steady state most wants are resident and have no job, yet each pays for a key and a map insert. | Compute keys for jobs and for wants with an entry in `active`. Store them in a sorted Vec (wants are iterated in order). Missing keys already fall back to defaults in `plan()`. | item |
| 12 | engine.rs:1307-1331 (+948-966) | `protected_bytes()` builds a union `BTreeSet` of four sets, then `entries.get(&r)` and `protection(r)`, which looks up `entries` again plus three set lookups | A temporary set and about 5 lookups per candidate, every step. | K-way merge of the four sorted iterators with dedup. Use `protection_of(r, &Entry)`. End state: protected counters maintained incrementally. | step |
| 13 | engine.rs:305, 1140, 1588, 1800, 1891, 1939, 2006, 2189 | Stable `sort()` / `sort_by_key()` | Stable sort allocates a scratch buffer (up to T×24 B for the eviction sorts). Every key ends in the unique `ResourceId`, or the whole element is compared, so the order is total and `sort_unstable` gives bit-identical output with no allocation. | Switch to `sort_unstable*`. Keep the "key ends in the unique id" invariant (§6). | step |
| 14 | observe.rs:252-263, 773-775, 891-998, 1000-1006 | `BudgetMeter::step`, which the host calls after each `decide`: one `BTreeSet<LayerId>` per drawn item (`worn_layers`, 973-978); the `levels`, `parents` and `floor_levels` maps from the same need; `wanted`, `drawn` and `served` maps; `refined`/`coarsened` sets used only for `len()`; `check()` builds `worn` and `floor_levels` even when `d.evict` is empty; a full `BudgetConfig` rebuilt per step | Adds about N + 7 map builds per step when a meter is attached. It contradicts the module's own contract: "no allocation per step beyond bounded maps" (observe.rs:9-10). | Build `check()`'s sets lazily, only when `!d.evict.is_empty()`. One pass over the need into a sorted Vec of `(tile, level, parent)`. Compare layer slices instead of sets. Reuse buffers on the meter. Store the reduced config once in `new()`. | step (meter) |
| 15 | engine.rs:1874-1891, 1912-1939, 1987-2006 | `evict` runs one full scan per class over its high watermark (up to 4), then decoded release, then pooled eviction. Each builds a Vec and runs a stable sort. The same eviction key is written four times (1582, 1886, 1999, 2184). | Up to 6 × O(T log T) per step under sustained pressure. | Do one pass that buckets by class and use `sort_unstable`. Share an `evict_key(e, r)`. End state: the same cold index as #3. | step (under pressure) |

Next in line: buffers whose capacity is dropped every step (`events` at 304, `ranked_last` at 995/1145), `Entry` at about 256 B (state.rs:58-81), the output `DrawItem` Vecs (decisions.rs:63-65), and the `u32 -= 1` counters (§3).

---

## 3. Full catalogue

### A. Clones (9)

| ID | Where | Finding | Fix | Impact |
|---|---|---|---|---|
| C1 | engine.rs:459 | `Need` cloned per entry; the `Vec<LayerReach>` goes to the heap | Borrow `&'a Need` | item |
| C2 | engine.rs:1035 | `self.drawn_last = drawn_res.clone()` | Fill `drawn_last` in place (sorted Vec) or `mem::swap` at the end | step |
| C3 | engine.rs:1057 | `keep = drawn_res.clone()` + extend | Predicate `drawn.contains(r) \|\| swap_last.contains(r)` | step |
| C4 | engine.rs:1040 | `i.drapes.clone()` for every item with drapes → `worn_last` | Flat sorted `Vec<(ResourceId, DrapeChoice)>` reused across steps | item |
| C5 | engine.rs:1185-1192 | `group` Vec per floor tile, cloned per member | Compute on demand from `tiles` | step |
| C6 | engine.rs:1813 | `groups.get(&g).cloned()` per transit item | `groups.remove(&g)` (each group is visited once) | delivery |
| C7 | engine.rs:1843 | `members.clone()` only to sort | Sort the owned Vec after pushing the items | delivery |
| C8 | engine.rs:1339 | `active` copied to a Vec to satisfy the borrow checker | Split borrows: `let Budget { active, entries, ledger, in_flight, local_reads, drawn_last, .. } = self;` | step |
| C9 | observe.rs:754-757 | `BudgetSignals` (about 3 KB, 4 histograms) cloned per `signals()` | Fine at publish rate; cold | cold |

`self.fatal.clone()` (engine.rs:1253) is trivial (`SessionError` holds one `ResourceId`). It could derive `Copy`, but that is not a cost.

### B. Over-allocation: temporary or oversized containers (27)

| ID | Where | Finding | Fix | Impact |
|---|---|---|---|---|
| O1 | engine.rs:606-617 | `ancestors()` returns a Vec (grows 4→8→16) and its callers stop at the first hit (745) | `fn ancestors(&self, t) -> impl Iterator` (`successors`, `take(64)`) | item |
| O2 | engine.rs:632-634, 661-670 | `chain` Vec, `extend` realloc, `cands` Vec, then a second pass to find the winner | Iterate the chain lazily and keep the running best (`best_drape` already folds) | item |
| O3 | engine.rs:674-676 | `layers_of` → one `BTreeSet<LayerId>` per drawn tile, per `select` call | Pass the range iterator or a slice | item |
| O4 | engine.rs:142, 713-722, 849 | `BTreeSet<TileId>` per selected mesh, converted to a Vec | Vec + `sort_unstable` + `dedup` | item |
| O5 | engine.rs:733-738 | `held: Vec` per missing tile | Range into the per-step ancestor index (#1) | item (motion) |
| O6 | engine.rs:790-796 | `BTreeMap<LayerId,u32>` per draw item | Small sorted Vec | item |
| O7 | engine.rs:784, 804, 1100 | `out`, `chosen` and `jobs` without `with_capacity` (sizes known: items, `needs.len()`, `wants.len()`) | `with_capacity` / reuse | step |
| O8 | engine.rs:450-451, 509, 862 | `tiles`, `drapes` and `wants` rebuilt as BTreeMaps per step | Sorted Vec scratch now; incremental later (§7) | step |
| O9 | engine.rs:1093-1099 | `fallback_cover` and `keys` maps | `draw.get(r)`; keys as a sorted Vec restricted to consumers (#11) | step |
| O10 | engine.rs:1177-1194 | `floor_groups: BTreeMap<ResourceId, Vec<ResourceId>>` | On-demand derivation | step |
| O11 | engine.rs:1042-1056, 1261-1271 | `chain_last`, `swap_last`, `held_last` and `wanted_last` rebuilt by `collect` (temporary Vec + stable sort + nodes) | Reusable sorted Vecs (inputs are already sorted) | step |
| O12 | engine.rs:1061-1066 | `touched` union set | Three-way merge-join (A8) | step |
| O13 | engine.rs:1309-1316 | `protected_bytes` candidate union set | K-way merge | step |
| O14 | engine.rs:1153-1158, 1172-1176 | `loads` / `delivers` collected from `jobs` | Iterate `jobs` with a filter, or partition indices into reused buffers | step |
| O15 | engine.rs:304, 995 & 1145 | `mem::take(&mut self.events)` / `mem::take(&mut self.ranked_last)` drop the buffer; `ranked_last` is re-collected | Take, use, `clear()`, put back | step |
| O16 | engine.rs:1786-1807 | `plan`: `transit` Vec, `taken` BTreeSet, `groups` BTreeMap of Vecs | See #5; one Vec sorted by `(group, key)` | step |
| O17 | engine.rs:1567-1587, 1874-1890, 1912-1938, 1987-2005, 2173-2188 | Victim/candidate Vecs of every resident of a class + stable-sort scratch | One ordering per step (cursor), `sort_unstable` | delivery / step |
| O18 | engine.rs:2147-2150 | `prune`: `keep` BTreeSet of all tiles + `seeds` copy of it | Refcount, or a reusable Vec + `sort_unstable` + `dedup` | periodic |
| O19 | decisions.rs:63-65 | Output API: two Vecs per `DrawItem` (`drapes`, `serves`; `serves` holds a single element for Exact items) | Add `decide_into(&mut Decisions)` that reuses the previous step's buffers (non-breaking). Later, a flat `DrawSet { items, drapes, serves }` with ranges (breaking). | item |
| O20 | observe.rs:252-263 | `check()` builds `worn` (all drawn resources) and `floor_levels` every step, but only uses them when `d.evict` is non-empty | Build lazily | step (meter) |
| O21 | observe.rs:773-775 & 891, 925, 929-945 | `levels`, `wanted`, `drawn`, `served` and `parents` maps per step; three of them come from the same `need.entries()` | One pass into a sorted Vec; reuse | step (meter) |
| O22 | observe.rs:973-978 | `worn_layers: BTreeMap<TileId, BTreeSet<LayerId>>`, one set per drawn item | Store `Vec<(TileId, SmallLayers)>` and compare slices (drapes are sorted by layer) | item (meter) |
| O23 | observe.rs:946, 959 | `refined` / `coarsened` sets used only for `.len()` | Vec + `sort_unstable` + `dedup`, or count distinct parents | step (meter) |
| O24 | observe.rs:1000-1006 | A `BudgetConfig` built per step to pass 3 fields to `check` | Store it once in `new()` | step (meter, no alloc) |
| O25 | observe.rs:469-697 | `series()`: one `Vec` of labels per series (about 250) | `&'static [(&str,&str)]` or `[(&str,&str); 2]` + len | cold |
| O26 | observe.rs:1013-1014 | `frame()`: two BTreeSets per render frame to count an intersection | Sort the small `uploaded` slice once, or merge | render frame (small) |
| O27 | state.rs:58-81 | `Entry` ≈ 256 B (computed from field sizes, not measured). Contents: `Option<Micros>` ×2 (16 B each), `priority: Option<PriorityKey>` (32 B, which repeats the 16 B map key), `Cost` duplicated in `cpu` and `transit`, a `u64` epoch ×4. `Clone` but not `Copy`. | Use a `Micros::NEVER` sentinel; store `(urgency, score, tier)` without the id; derive `Copy`. Target is about 176 B: −30 % per tracked resource, and full scans touch less memory. Add `const _: () = assert!(size_of::<Entry>() <= 256);`. | resource |

### C. Algorithms, scans and repeated lookups (22)

| ID | Where | Finding | Fix | Impact |
|---|---|---|---|---|
| A1 | engine.rs:733-738, 585-603 | O(M × \|held\| × depth) held-descendant scan | Per-step ancestor → held index | step, p99 |
| A2 | engine.rs:686, 619-671, 745 | Existence check via a full best-drape walk; O(depth²) on fallback search | `has_drape` early exit + per-step memo | item / step |
| A3 | engine.rs:1563-1612 | `make_room` full scan + sort per delivery at the limit | Per-step ordering with a cursor → cold index | delivery, p99 |
| A4 | engine.rs:1443-1469 | Preemption full scan per load | Scan `active` | delivery |
| A5 | engine.rs:1786-1829 | `plan` scans all entries; O(n²) `find` for bytes | Scan `active`; carry bytes | step |
| A6 | engine.rs:2121-2146 | Prune spike one step in eight; `recent` cap O(excess × n) | Cursor-based amortisation; ordered secondary index | periodic, p99 |
| A7 | engine.rs:768-776 | `select` runs twice while the floor is not in and tiles are lost | Pass the cap as a filter on the first run's result, or memoise `has_drape`/`shown` across both runs | step (start-up) |
| A8 | engine.rs:1067-1086 | Marking loop: `wants.get` + `drawn_res.contains` + `tiles.get` + `entries.get_mut` per touched resource; dead `let _ = w` (1078) | Merge-join over three sorted sources (wants, drawn, wanted_last) with peekable iterators; use `contains_key` | step |
| A9 | engine.rs:1317-1321, 948-966 | `entries.get` twice per protected candidate | `protection_of(r, e)` | step |
| A10 | engine.rs:462-467, 864-869, 1014-1015 | `get` then `insert` (two lookups); `sort_key` computed twice | Entry API | item |
| A11 | engine.rs:468-475 | `self.tiles.insert` for every need entry (a tile in 3 lists is written 3 times; the last list wins) | Write once per distinct tile after the merge, keeping last-wins (§6) | item |
| A12 | engine.rs:531 & 554-556, 2106-2110 | `imagery_prev` read, then rewritten in a second pass; `retain` runs every step | Update in the main loop (`entry`); amortise `retain` with prune | step |
| A13 | engine.rs:837, 748-749 | `tiles.get(&tile)` inside the per-drape loop; the `level` closure looks up twice | Hoist | item |
| A14 | engine.rs:1863-1909 | One full scan per class over its watermark | One bucketed pass | step (pressure) |
| A15 | engine.rs:305, 1140, 1588, 1800, 1891, 1939, 2006, 2189 | Stable sorts on total keys | `sort_unstable` | step |
| A16 | engine.rs:1230-1236 | `pending_view`: O(W) `entries.get` every step, for a statistic | Fold it into the jobs loop, where `e` is already fetched | step |
| A17 | engine.rs:1005 | `input.need.entries().count()` iterates every entry | Sum of the four `len()` | step |
| A18 | engine.rs:212-216 | `epoch()` looks up `layer_epochs` for the same resource in several phases (tier, job, refresh, deliver, start) | A per-step epoch array (layers are few), or pass it along | item |
| A19 | engine.rs:1011-1027 | Refresh loop: `entries.get` + `contains_key` + `insert` per drawn resource | Entry API; fold into the marking merge | item |
| A20 | engine.rs:1099-1139 | Keys and tiers computed for all wants | Restrict to consumers (#11) | item |
| A21 | deadline.rs:68-77 | `FrameDeadline::take`: `groups.iter().find(contains && all(contains))` at each queue position: O(Q·G·M²) per render frame | Build a member → group index once per plan (sizes are small today) | render frame |
| A22 | decisions.rs:155-238 | `digest()` runs FNV-1a one byte at a time over the whole draw set | Leave it: cold (tests, tape), and changing it re-freezes every golden | cold |

### D. Other anti-patterns (6)

| ID | Where | Finding | Fix |
|---|---|---|---|
| X1 | engine.rs:322, 324, 354, 355, 380, 1356, 1357, 1474 | `u32 -= 1` on `in_flight` / `local_reads`: panics in debug and wraps to `u32::MAX` in release if the invariant ever breaks. A wrap would freeze the quota until `audit` runs. | `debug_assert!(x > 0)` + `saturating_sub`, or `checked_sub` routed to the audit error |
| X2 | engine.rs:1078, 1801; observe.rs:851 | Dead bindings: `let _ = w;`, the `drapes` parameter of `plan` (`let _ = drapes;`), `let _ = (CancelReason::ALL, EvictReason::ALL);` | Remove |
| X3 | engine.rs:203 | `class_of(&self, …)` does not use `self` (`clippy::unused_self`) | Free function or `ResourceId` method |
| X4 | engine.rs:990-1305 (286 lines), 303-443 (137), 1396-1543 (128) | `step()` is a single function, so a profiler cannot attribute cost by phase | One function per numbered phase (no behaviour change) |
| X5 | state.rs:58, stats.rs:59, error.rs:53 | `Entry`, `BudgetStats` (about 650 B) and `SessionError` are `Clone` but not `Copy`, although every field is `Copy` | Derive `Copy` (on `SessionError` only if `#[non_exhaustive]` growth allows) |
| X6 | mod.rs:71 | `&dyn PresenceOracle`: one indirect call per non-resident want | Keep it. The cost is negligible next to the lookups, and generics would spread through `FrameInput`. |

No issues found in these areas:
- No `unwrap`/`expect` outside tests.
- No `Arc`/`Rc`.
- No `Box<dyn>` on the hot path; the only one is the observer list, called once per step.
- No large enum variants (`Event` is 48 B, `Load` 32 B).
- No `format!`/`to_string`/`to_vec` anywhere.
- `ResourceId` is 16 B, which is good.

### Totals by category

| Category | Findings | Hot (step/item/delivery/render frame) | Periodic | Resource | Cold | No measurable cost (robustness, readability) |
|---|---|---|---|---|---|---|
| A. Clones | 9 | 8 | 0 | 0 | 1 | 0 |
| B. Over-allocation | 27 | 24 (6 of them in `observe.rs`, active only when a meter is attached) | 1 | 1 | 1 | 0 |
| C. Algorithms / scans / lookups | 22 | 20 | 1 | 0 | 1 | 0 |
| D. Other anti-patterns | 6 | 0 | 0 | 0 | 0 | 6 |
| **Total** | **64** | **52** | **2** | **1** | **3** | **6** |

---

## 4. Estimated allocations per `decide` (steady state, still camera, all Exact)

These are estimates from reading the code under the workload model above. Section 8 measures them.

| Source | Where | Est. allocations / step |
|---|---|---|
| `dressable` → `best_drape_for(…, 0)`: chain + ancestors growth + cands | 686, 606-671 | ~3,000 (6 × N) |
| Output: `DrawItem.drapes` + `serves` + items Vec | 845-850 | ~1,000 |
| `Need` clones (`Vec<LayerReach>`) | 459 | ~800 |
| `layers_of` BTreeSet per drawn tile | 675 | ~500 |
| `Selection` BTreeSet per mesh + `items` map nodes | 713-722 | ~560 |
| `build_draw`: `needs` map + `chosen` Vec per item | 790, 804 | ~1,000 |
| `worn_last` drape clones + map nodes | 1036-1041 | ~550 |
| Per-step maps (`tiles`, `drapes`, `wants`, `keys`) | 451, 509, 862, 1099 | ~400 nodes |
| Snapshot sets + clones (`drawn_res` ×3, `chain`/`swap`/`held`/`wanted`/`touched`, protected union), each with a temporary Vec + sort scratch | 1030-1066, 1261-1271, 1309 | ~900 |
| Smaller (jobs, loads, delivers, ranked_last, events, plan, sorts) | various | ~50 |
| **Total** | | **~8,000–10,000** |

At 20–40 ns per malloc+free pair (native; `dlmalloc` on wasm32 is slower), that is 0.15–0.4 ms per step: an estimated 15–40 % of the 0.6–1.1 ms mean, before counting the cost of building the BTree nodes and the cache misses. During motion, add A1 and the O(depth²) part of A2: an estimated 10⁵ BTree lookups or more per step, which fits the multi-millisecond p99.

`BudgetMeter::step` adds an estimated ~600 allocations and 7 map builds per step when attached.

## 5. Estimated bytes per tracked resource

From field sizes; not measured.

- `Entry` ≈ 256 B (O27). In `BTreeMap<ResourceId, Entry>` a leaf is ≈ 3 KB for 11 entries; at about 70 % fill plus internal nodes, that is **≈ 400–420 B per entry**.
- Index memberships (`active`, `floor_set`, `drawn_last`, `swap_last`, `chain_last`, `wanted_last`): ≈ 20–25 B each. `recent`: ≈ 35 B.
- Per tile: `tiles` entry ≈ 100 B (`TileInfo` 64 B); `imagery_prev` ≈ 30 B per (tile, layer).
- **Total ≈ 450–550 B per tracked resource.** A mesh + drape pair costs ≈ 1 KB with its tile.

On wasm32 the figures are about the same: `Entry` holds no `usize`; only node pointers shrink.

---

## 6. Determinism traps to keep while optimising

1. **Never iterate a hash container to decide.** A `HashMap`/`HashSet` brought in only for O(1) lookup must never be iterated, and its hasher must be fixed (std `RandomState` is seeded per process). The preferred alternative is a sorted `Vec` + `binary_search`: deterministic, one allocation, cache-friendly. Guard it with `#![deny(clippy::iter_over_hash_type)]` in `budget/mod.rs` (0 hits today).
2. **`sort_unstable` is safe only on total keys.** Every sort key today ends in the unique `ResourceId`, or the whole element is compared. Keep that invariant. On equal keys, the order of an unstable sort is an artefact of the std implementation, and a toolchain upgrade would move the digests.
3. **"Best wins" merges are first-wins on ties.** `read_need` keeps `cur` when `sort_key(cur) >= sort_key(cand)`, and `wants`' `put` keeps `cur` on `(urgency, Reverse(q))` ties, while `Want.class`/`level` and `Need.rect` are outside the compared key. Replacing those maps with a sort-then-dedup must carry the original sequence index as the final tie-break, keeping the first. `self.tiles.insert` (468) is **last**-wins across lists; keep that too.
4. **`usize` width.**
   - Never `u64 as usize`: it truncates on wasm32.
   - Never feed a `usize` into a digest or a sort key without widening it to `u64` (the digest already uses `len() as u64`).
   - `len() as u32` (17 sites) is benign but should go through a saturating helper.
   - `RECENT_CAP` and the indices are fine.
5. **Float bit-exactness.** `Decisions::digest` hashes `UvTransform` bits.
   - Do not replace `/ w` with `* (1.0 / w)` in `UvTransform::between` or `TexelFootprint::relaxed`.
   - Do not reorder those operations.
   - Never introduce `mul_add`; FMA availability differs between targets.
   - Floats belong in `imagery.rs` and `priority.rs` (quantisation) only. Enforce it with `clippy::float_arithmetic`, which has 0 hits in `engine.rs` today.
6. **Goldens are the oracle.** Every change in this report is meant to preserve decisions. `budget_sim/goldens.txt` and `pattern_goldens.txt` hash `Decisions::digest` per step, statistics excluded, and must stay bit-identical. `BudgetStats.work` will change (A5, A6) and moves only the stats digest and the `decide_work` signal, which needs documenting.
7. **Incremental equals from-scratch.** The incremental `decide` should keep a test mode that runs both the incremental and the full path and compares digests every step, as `audit()` does for the ledger.

## 7. Where incremental `decide` is the real fix

- **Local fixes are enough** for: A1, A2, A4, A5, C1–C8, O1–O7, O14–O16, A10–A19, X1–X5. These are pure refactors.
- **Per-step sorted scratch now, incremental later** for: `tiles`/`wants`/`drapes`/`draw` (O8), the `*_last` snapshots (O11), and protected bytes (O13).
  - With a dirty set of tiles whose need changed, plus resources whose state changed (events, starts, deliveries, evictions), a steady-state step costs O(changes · log n) instead of O(N log N).
  - Prerequisite: diff the `NeedSet` against the previous one, or make the traversal hand over a diff (API).
- **Cold-eviction index** (A3, A14, O17, `release_beyond`): a persistent `BTreeSet<(last_needed, Reverse(level), Reverse(distance), ResourceId)>` per class, over **unneeded** GPU residents only.
  - Needed resources have `last_needed = now` and sort last anyway. They would be added on demand only when the cold tier is exhausted (strain).
  - Updates happen on `unneeded_since` transitions, eviction and upload, so the index never sees the per-step churn of `last_needed`.
- **Priority keys** (A20): the age bonus changes every step for waiting work, so keys of waiting resources stay per-step. They are bounded by work, not by wants.
- **Prune** (A6): refcount tiles from entries and need; delete emptied entries when they become empty, not on a sweep.

---

## 8. Clippy

Two runs: pedantic + nursery + perf, then restriction. Both on the budget module only.

| Lint | Group | Hits | Worth enabling on `budget`? |
|---|---|---|---|
| `iter_over_hash_type` | restriction | 0 | **Yes, deny**: the determinism guard |
| `float_arithmetic` | restriction | 15 (imagery 10, priority 2, decisions 1, observe 2; **engine 0**) | **Yes, deny** on `engine.rs`/`state.rs`; allow on the documented float boundary |
| `cast_possible_truncation` | pedantic | 23 (17 `len() as u32`, quantisation, `scale`) | **Yes**, with a `sat_u32(usize)` helper; it guards against wasm32 width bugs |
| `cast_sign_loss` / `cast_possible_wrap` / `cast_lossless` | pedantic | 2 / 1 / 2 | Yes, together with the above |
| `too_many_lines` | pedantic | 5 (`step` 286, `series` 227, `BudgetMeter::step` 213, `apply_events` 137, `start` 128) | Yes: per-phase functions make profiling attributable |
| `map_unwrap_or` | pedantic | 13 | Optional (readability, `map_or`) |
| `or_fun_call` | nursery | 2 (engine.rs:2222-2223) | Optional (cheap here) |
| `unused_self` | pedantic | 1 (engine.rs:203) | Yes |
| `shadow_unrelated` | restriction | 4 | Optional |
| `option_if_let_else` | nursery | 2 | No |
| `arithmetic_side_effects` | restriction | 140 | No: fix the 8 `-= 1` sites (X1) by hand instead |
| `as_conversions` / `indexing_slicing` / `integer_division` | restriction | 92 / 39 / 13 | No: all bounded by construction |
| `use_self`, `must_use_candidate`, `missing_const_for_fn`, `return_self_not_must_use`, `default_trait_access`, doc lints | pedantic/nursery | 77, 62, 22, 13, 13, 14 | No (style only) |
| `perf` group, `redundant_clone`, `needless_collect`, `large_types_passed_by_value`, `trivially_copy_pass_by_ref`, `needless_pass_by_value`, `implicit_clone`, `unnecessary_to_owned`, `clone_on_ref_ptr`, `set_contains_or_insert`, `large_stack_frames` | various | **0** | Enable them, but they will not find what this report lists |

`needless_collect` misses O1 because the collect and the iteration sit on opposite sides of a function boundary (`ancestors()` returns a `Vec`; the caller runs `.into_iter().find()`).

Suggested header for `budget/mod.rs`:

```rust
#![deny(clippy::iter_over_hash_type)]
#![warn(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap,
        clippy::cast_lossless, clippy::too_many_lines, clippy::unused_self)]
// engine.rs / state.rs: #![deny(clippy::float_arithmetic)]
```

---

## 9. Making allocation measurable: the harness

Goal: one test binary that asserts **allocations per `decide` in steady state** and **bytes per tracked resource** as ratchets. They are frozen like the goldens and only ever lowered. Each fix above must make them drop. Then revert the fix and confirm the ratchet fails.

### 9.1 Counting allocator (one per binary)

Put it in `budget_sim/main.rs`, because `#[global_allocator]` is per binary and the simulation lives there. The workspace denies `unsafe_code`, so this binary root needs `#![allow(unsafe_code)]` with a justification comment.

If the team prefers no `unsafe`, `dhat` (a native-only dev-dependency) offers `dhat::Alloc` + `Profiler::builder().testing()` + `HeapStats::get()`. Its counters are process-wide, though, so the test must run alone. Its per-call-site output (`dhat-heap.json`) is the right one-off tool to attribute allocations to the IDs above.

```rust
// budget_sim/alloc.rs
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub struct Counting;

thread_local! {
    // const-initialised, no destructor: no lazy registration, no allocation
    // inside the allocator.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn on_alloc(n: usize) {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            ALLOCS.with(|c| c.set(c.get() + 1));
            BYTES.with(|c| c.set(c.get() + n as u64));
            LIVE.with(|l| {
                let v = l.get() + n as i64;
                l.set(v);
                PEAK.with(|p| p.set(p.get().max(v)));
            });
        }
    });
}
fn on_free(n: usize) {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            LIVE.with(|l| l.set(l.get() - n as i64));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { on_alloc(l.size()); unsafe { System.alloc(l) } }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 { on_alloc(l.size()); unsafe { System.alloc_zeroed(l) } }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { on_free(l.size()); unsafe { System.dealloc(p, l) } }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        on_free(l.size()); on_alloc(new); unsafe { System.realloc(p, l, new) }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tally { pub allocs: u64, pub bytes: u64, pub live: i64, pub peak: i64 }

/// Runs `f` with counting armed on this thread only (parallel tests do not interfere).
pub fn counted<R>(f: impl FnOnce() -> R) -> (R, Tally) {
    for c in [&ALLOCS, &BYTES] { c.with(|x| x.set(0)); }
    LIVE.with(|x| x.set(0)); PEAK.with(|x| x.set(0));
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    let t = Tally {
        allocs: ALLOCS.with(Cell::get), bytes: BYTES.with(Cell::get),
        live: LIVE.with(Cell::get), peak: PEAK.with(Cell::get),
    };
    (r, t)
}

// budget_sim/main.rs
// #![allow(unsafe_code)] // the counting allocator, test-only: see alloc.rs
// mod alloc;
// #[global_allocator] static A: alloc::Counting = alloc::Counting;
```

### 9.2 Hook in the simulation

`server.rs:351-353` already times `decide`. Count next to it:

```rust
let clock = std::time::Instant::now();
let (d, tally) = super::alloc::counted(|| self.budget.decide(input));
self.decide_us.push((self.now, clock.elapsed().as_secs_f64() * 1e6));
self.decide_allocs.push((self.now, tally, d.draw.items.len() as u32, self.budget.tracked() as u32));
```

### 9.3 Assertions

```rust
/// Steady state: a still view, converged, decides with no allocation beyond
/// its output (2 per drawn item for `drapes`/`serves`, plus the lists).
#[test]
fn a_converged_still_view_allocates_only_its_output() {
    let mut sim = Sim::new(Profile::desktop(), 7);
    let cam = still_camera();
    sim.run_until_quiet(cam, 30, 3_000);          // converged and idle for 30 steps
    let from = sim.decide_allocs.len();
    for _ in 0..200 { sim.step(cam); }
    let w = &sim.decide_allocs[from..];
    let worst = w.iter().map(|(_, t, drawn, _)| t.allocs.saturating_sub(2 * u64::from(*drawn) + 8)).max().unwrap();
    eprintln!("internal allocs/step: max {worst}");
    assert!(worst <= RATCHET_STILL, "decide allocated {worst} blocks beyond its output");
}

/// Motion: allocations per step scale with what is drawn, not with what is tracked.
#[test]
fn allocations_per_step_follow_the_view_not_the_store() {
    // the soak's tour, 2 min; then the same tour after a 10 min warm-up (more tracked)
    // assert p99(allocs / drawn) is within 10 % between the two windows.
}

/// Bytes per tracked resource: what dropping the budget frees.
#[test]
fn a_tracked_resource_costs_bounded_bytes() {
    let mut sim = Sim::new(Profile::desktop(), 11);
    sim.run_tour(Micros::from_secs(60));
    let tracked = sim.budget.tracked() as i64;
    let cfg = sim.budget.config().clone();
    let ((), t) = super::alloc::counted(|| drop(std::mem::replace(&mut sim.budget, Budget::new(cfg).unwrap())));
    // `Budget::new` allocates nothing (empty BTreeMaps and Vecs), so -live is the old budget's heap.
    let per = -t.live / tracked.max(1);
    eprintln!("{per} B per tracked resource ({tracked} tracked)");
    assert!(per <= RATCHET_BYTES_PER_TRACKED, "{per} B per tracked resource");
}
```

Notes on the harness:
- `Sim.budget` must be reachable from the test module: `pub(crate)` or an accessor.
- `run_until_quiet` and `run_tour` are small helpers over the existing `Sim::step` and its idle detection.
- Freeze the current values of `RATCHET_STILL` and `RATCHET_BYTES_PER_TRACKED` on the first run (as with `goldens.txt`) and lower them with each fix. Expected targets once the top 3 below land: about 0–20 internal allocations per still step (then 0 with `decide_into`), and ≤ 350 B per tracked resource after O27.
- Add a compile-time guard in `state.rs`: `const _: () = assert!(core::mem::size_of::<Entry>() <= 256);`. It holds on wasm32 too, since `Entry` has no `usize`.
- Phase attribution: run `dhat` once on the motion scenario to map allocations to the IDs above, and split `step()` into per-phase functions (X4) so a sampling profiler shows where the 6.9 ms p99 goes. A deterministic counterpart already exists: `BudgetStats.work`, extended per phase.
- What the harness does not cover: wasm32 `dlmalloc` cost per allocation (counts are the same, time is not), and the host's own allocations around `decide` (`NeedSet` construction, which itself allocates one `Vec<LayerReach>` per tile in `ImageryNeed`).

---

## 10. The three changes with the most impact

1. **Make `select()` linear and allocation-free.** Covers #1, #2 and part of #9.
   - Build an ancestor → held index once per step.
   - Make `ancestors()` a lazy iterator.
   - Add `has_drape(tile, layer)` with an early exit, own tile first, memoised per step.
   - Have `best_drape_for` fold lazily instead of collecting candidates.
   - Expected (estimate): removes the quadratic motion path, the likely main contributor to the 6.9 ms p99, plus about 3,000 allocations and about 20,000 BTree lookups per steady step: about a third of all allocations.
2. **A Budget-owned scratch of sorted Vecs replaces every per-step BTree build, clone and per-item container.** Covers #7–#10, #12, #13, O15.
   - Borrow `Need` instead of cloning it.
   - Use `sort_unstable` on the total keys.
   - Use merge-joins instead of repeated lookups.
   - Keep first-wins and last-wins tie semantics via a sequence index.
   - Expected (estimate): allocations per `decide` go from about 8–10k to about the output alone (2 per drawn item), and to 0 with a `decide_into`. A plausible −30 to −50 % on the mean. To be confirmed by §9.
3. **Stop scanning all tracked entries on the hot path.** Covers #3–#6 and #15.
   - `plan()` and preemption iterate `active`.
   - One eviction ordering per class per step, consumed by a cursor and shared by `make_room`, `evict`, `evict_pooled` and `release_beyond`.
   - Prune amortised by a cursor; fix the `recent` cap.
   - End state: the persistent cold-eviction index of the incremental `decide`.
   - Expected: removes the O(deliveries × T log T) term under pressure and the spike one step in eight, bringing p99 toward the mean. Decisions are unchanged; only `stats.work` moves.

All three keep `Decisions::digest` bit-identical, which is checked by the simulation goldens and pattern goldens. None touches a public type, except the optional `decide_into`.
