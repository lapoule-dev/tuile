<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Progressive coarse and neighbour preload: one score for loading and releasing

Status: **implemented, 2026-11-23**, branch `feat/budget-progressive-coarse-preload`, for
Laurent's review. Asked by Laurent ("ça doit être corrigé asap"): holes and missing coarse
ground on a zoom-out or a rotation (`videos/holes-study/`).

## 1. What was wrong

On `main` (2719c2f3) a coarse tile got no treatment of its own:

- the coarse band was empty (`coarse_through = 0` in every specification) and the floor
  stopped at level 2;
- a preloaded ancestor was the chain of the drawn tiles only, worth a flat 25 % of a drawn
  tile, and nothing loaded the tiles *around* the view at any level (no ring without views
  ahead, no siblings);
- the release order of everything that was wanted but not drawn was recency then
  restore cost per byte, the same for a level-12 tile next to the eye and a level-6 tile
  three hundred kilometres away.

So a zoom-out or a turn found nothing resident beside the chain under the eye, and the
ground fell back to the floor (or showed a hole while the floor was still on its way).

## 2. The score

For a tile `t` of level `ℓ`, the view needing level `n` at its nearest point `F` (the centre
of the finest drawn ground tile nearest the eye), and `d` the distance from `F` to `t`
measured **in tiles of level `ℓ`** (`d = dist(F, volume(t)) / s_ℓ`, `s_ℓ` the size of the
level-`ℓ` tile under `F`):

```
score(t) = P(ℓ, n, d) × R(tier) / bytes(t)          (release: lowest first)
gain(t)  = P(ℓ, n, d) × S(t) × (H + R(tier))         (preload: highest first)

P(ℓ, n, d) = A(ℓ, n) × D(d)                          the probability of use

A(ℓ, n) = 1                       ℓ = n   (the present need)
        = a0 × 2^−(n−1−ℓ)         ℓ < n   (n−1: a0, n−2: a0/2, then halving per level up)
        = a↓ × 2^−(ℓ−n−1)         ℓ > n   (n+1: a↓; only n+1 is listed)
  and at least  c × 2^−(ℓ−ℓc)     ℓ ≥ ℓc  (the graded coarse levels: ℓc = 3, c = 0.95)

D(d)    = ρ² / (ρ + d)²                                (1 at F, ¼ at d = ρ)
```

- `R(tier)` is the restore cost of the tier a release sends the copy back to — a GPU copy
  whose decoded copy is still held comes back by an upload (2 ms), any other by the network
  (150 ms) — the specification's `RestoreCosts`, replaced by measured ones as they come
  (spec §4.2). Dear to restore ⇒ kept longer.
- `bytes(t)` is the GPU copy's size, floored at `ref_bytes / 16` (16 KiB).
- `S(t)` is the screen weight (½ at the edge of the view, 1 at its centre) and `H + R` the
  lead a preload needs: a tier slow to come back is started sooner. A load does not know its
  bytes yet. **The two sides share `P` and `R`**: that is the one cost function — what is
  preloaded first is what is released last. (The present need's own loads keep
  `S × (H − R)⁺`, in a band of their own before every preload: §3.)
- `R` is **measured**: the geometry server times every load from the step that started it to
  the step it landed, by the tier it read (network, local store), and hands the budget each
  second the interval's median (`Measured`, recorded with the step as any input, so a replay
  decides the same). The budget smooths it (EMA, τ = 8 s) and uses it for a resource of the
  reference size, quantised to three significant bits (a change under ~12 % moves nothing),
  the specification's constant until a tier has samples (`Budget::restore_costs`; the
  window's `R(tier)` line, `tuile_budget_restore_cost_seconds{tier}`).
- `n` and `d` follow the pose. An ancestor of what is drawn lies on the view's footprint
  (`d` = 0) and is scored against the finest level drawn under it — **the need at that
  place**: a grazing view's far ground is drawn coarse, and its ancestors score `a0` against
  that coarse need, above a refinement under the eye (`a↓`). The neighbourhood's tiles are
  placed from `F`. The ring is isotropic (any direction is possible); where the view is going
  is the predictions' to say (the views ahead, step C), never an extrapolation here.
- A tile that leaves the neighbourhood keeps its last `P`, and its release key ages as
  GreedyDual-Size's does (`L + P·R/bytes`, `L` the time it was last needed): nothing drops
  from one score to zero.

Everything is in integers: `P` in 1/32 000ths (`value::CERTAIN`, a multiple of 1 000 so a
probability in ‰ is exact; fifteen halvings reach one unit), `d` in sixteenths of a tile,
times in µs. No float reaches a decision; the same on wasm. Sixteenths, not quarters: one
step of `D` near `F` is then `(1 + 1/(16ρ))²`, 1.06 at ρ = 2, under the margin's 1.25, so
two tiles of one level cannot trade places for an infinitesimal move of `F` (the proof
notebook's finding: with quarters, `(1 + 1/8)²` = 1.27 at ρ = 2 exceeded it).

### Constants as implemented

| Constant | Value | Where |
|---|---|---|
| pinned floor `F` | levels `0..=2`, mesh and drape, loaded first, never released (absolute law) | `Profile::pinned_level` (`TUILE_PIN_LEVEL` overrides) |
| graded coarse | from `ℓc` = 3, `A` ≥ 0.95 × 2^−(ℓ−3): level 3 0.95, level 4 0.475, level 5 0.24…: under pressure the fine go first, then the intermediate levels, level 4, level 3 last | `Params::progressive.coarse_from`, `coarse_boost_permille` (`coarse_from=`, `coarse_boost=`) |
| `a0` | 800 ‰ | `Params::progressive.a0_permille` (`a0=` in a query) |
| `a↓` | 200 ‰ | `Params::progressive.descend_permille` (`a_desc=`) |
| `ρ` | desktop and desktop page 4, phone and phone page 2, film 0 | `BudgetConfig::reach_tiles` |
| `K(ρ)` | `⌊π(ρ+1)²⌋` tiles per level: 78 at ρ = 4, 28 at ρ = 2 | `tuile-core` runtime |
| `m` | 25 % (interactive, known path), 15 % (FPV), 0 (film) | `Params::margin_permille` |
| `R` | decoded 2 ms, stored 6 ms, browser cache 15 ms, network 150 ms | `Params::restore` |
| `H` | 1 s interactive, 8 s known path, 2 s FPV | `Params::horizon` |
| preload slots | ≤ 75 % of each class's network slots (≥ 1): a quarter always free for the present need, and a hole preempts a preload in flight | `Params::speculation.slots_permille` |
| preload memory | delivered only below the target's low watermark, never displacing the present need | `deliver_one` |

### Where the tiles come from

The geometry server lists them (`crates/tuile-core/src/neighbourhood.rs`): the chain of every
drawn tile stays the **ancestors** list (the fallback of what is drawn, protected as it was),
each placed against the need; every other tile of the neighbourhood goes in the **ring** list
(urgency `Ring`): wanted, scored, released by the score, never protected. A preload's drapes
take the preload's own urgency and score, never the present need's.

### The views ahead (#282's `Need.ahead`)

A ring tile a prediction names carries `AheadUse { next_use_in, probability }`, part of the
need and so recorded in every `StepRecord`: the budget reads it as an input, never computes
it. It enters the one score through `P` alone, read in time with the same terms:

```
P_ahead = p × D_t(slack)       D_t = H² / (H + slack)²,  slack = (next_use_in − R(tier))⁺
P       = max(P_place, P_ahead)   a placed ring tile (the prediction only raises it)
        = P_ahead                 a ring tile the traversal did not place (the flat
                                  anticipation stood in for a probability it did not have)
```

`p` is the prediction's probability (‰ → `CERTAIN`), `H` the specification's horizon, `R` the
restore cost of the tier the copy would come from, measured (`Budget::restore_costs`): a use
due within its load time is as urgent as the present place (`D_t` = 1), one a horizon past it
weighs a quarter, as `D` at `d = ρ`. The load side (`P·S·(H+R)`) and the release side
(`P·R/bytes`) read the same `P`, so a tile predicted soon and likely is loaded first and
released last. In integers (`Quantised::{ahead_ms, ahead_permille}`, sixteen bytes with the
narrowed floats beside them).

A predicted tile is preloaded whole: its drapes are decided and loaded as the views ahead will
wear them (preloads of its own `P`, never counted as the present need's deficit), and its mesh
is delivered to the GPU only once its drapes are loaded, so the view it was predicted for draws
it at its need rather than its geometry under a coarser drape.

The absolute laws stay outside any prediction: what is drawn, a swap, the chain and the
floor are never released on one, and a wrong prediction costs a reload, never a hole. **A
prediction never delays a current need**: a drawn tile's drape or a swap's blocker facing full
slots preempts a predicted preload in flight (a hole preempts any preload, as before). Without
views ahead nothing changes, to the work counted: the goldens, the pattern goldens and the
decision digests do not move.

## 3. Above the score: the absolute laws

The score orders the value band only. Before it, in this order and whatever the score:

1. **the floor** (levels ≤ `F`): loaded first, never released;
2. **coverage**: a wanted tile with nothing drawable (a hole) loads before any value;
3. **the swap**: what is drawn, the children a drawn parent waits on, and the chain of
   ancestors of what is drawn (its fallback), are never released;
4. **the present need before anticipation**: a drawn tile's other loads (its drape) rank
   before every preload, whatever the values (`Precedence::Value` before
   `Precedence::Anticipation`);
5. **the hard limit**.

**Release keeps a headroom, and lets go of what is no longer worth holding.**

- **Headroom.** Speculation (the ring, a refresh) is delivered only under
  `target × (1 − headroom)` (`headroom_permille`, default 0), never by evicting to make that
  room; the chain of ancestors of what is drawn is coverage and takes the room the present
  need takes. The dynamic headroom — the measured upload rate (bytes delivered a second,
  smoothed over 0.5 s) over the horizon, plus the bytes in transit — is measured and reported
  (`ScoreStats::headroom_bytes`: 0 at rest, 84 MB on the test's pan at 512 MiB): gating the
  ring on it delayed the views ahead in the simulation (a slow zoom-in missed its 2 s bound),
  so it informs and does not gate yet. A GPU class (or the pool) over its target is swept down
  to its low watermark, lowest scores first, not bounded per step, so need never waits while
  low scores hold the pools; the decoded tier over its target is released down to it (a turn
  back finds its demoted copies there).
- **The sweep takes only what will not be needed soon** (Laurent, 2026-11-24;
  `tuile_budget::sweep`). A class (or the pool) over its high watermark — its target — is
  swept in the one order, in two parts. Under the target, down to the low watermark, the
  sweep is proactive: it runs ahead of any delivery, and it never takes a copy the need wants
  this step, at any urgency — the ring is the need's own prediction of what comes next — nor
  one it wanted within the horizon, its last camera poses (`sweep_keep_poses`, 300: a pose is a
  step of the decider, one camera handed to it — the law counts steps and reads no clock;
  300 poses are the 5 s it was in time at the viewers' 60 steps a second). What the law keeps stays; a
  delivery makes its room by admission (worth less than the work), and the class may sit
  between its watermarks meanwhile. That part runs first; then whatever is still over the
  target goes back within it, the law's keepers included (reason `fit_target`, as I2's pass,
  counted with `make_room`): the budget holds its size. Since grouped copies (needed at the
  step's time or the one before) are within any horizon of a step, the proactive part walks
  the filed copies alone and passes over at most `release_visits` kept copies a step: a
  still view's step does not grow with what it holds. A memory signal is not bound by the
  law (reason `pressure`). Before the law, a view swinging east–west every second under a
  budget its need and ring overfill had 335 copies swept as headroom, every one of a tile
  listed that very step, every one loaded again; with it, none as headroom, and 192 still go
  back within the target — that need does not fit 24 MiB. Ten seconds of horizon kept so much
  of a settled headset's recent past that its ring could not load (a 20° flick found ground
  eight levels up, P4.1); two and five seconds keep it within three.
- **What left the need goes.** A copy that leaves the need keeps a 64th of its probability
  (a copy drawn without being wanted — a stand-in for another tile — counts as certain while
  drawn). Once out of the need for the thrash window (10 s: a turn and back finds it still),
  it goes, room or not, from both tiers (its GPU copy, then its decoded copy) when its
  probability is under the **floor value** (1 %, `floor_value_permille`: every preload left
  behind) **or its level is more than one finer than the need's** (nothing selects it soon,
  whatever it was). A copy drawn a moment ago at the need's level stays as the cache a revisit
  finds (1.6 %): the simulation's revisits (soak I6, turn and back, newly visible ground)
  need it — releasing every left copy after 10 s was tried and broke those four invariants.
  At rest the pools hold the need, its ring, the coarse stack and the recent cache, not old
  fine levels. Laurent's Bordeaux capture scripted (a
  dive to level 19, out to 12, still): levels 14–19 held 406 tiles → 0 after 11 s, decoded
  124 MB → 76 MB (`at_rest_the_fine_levels_left_behind_are_released_in_both_tiers`). The pass
  looks at `release_visits` (16) copies a step per tier from the bottom of the release
  order: a frame's releases stay small.
- **Relaxed, between the watermarks.** A relaxed picture comes back to full detail only once
  every class is under its low watermark. Laurent's stall (the drapes at 97–99 % of target,
  relaxed, nothing loading) is a class held between them by copies nobody draws: the floor
  value's pass takes them (far finer than the need, or under the floor value) and the class
  falls under its low watermark. An extra sweep when relaxed was tried: run whenever relaxed
  it took the soak's cache (183–217 ms instead of 33 ms at p95); run only when stuck and on
  junk only, its walk made a still step's work grow with what is held (2 000 visits a step).
- Measured on the simulation and not kept: a fixed 12.5 % headroom swept under the target
  every step (the soak's revisits waited 167–267 ms at p95 instead of 33 ms; a slow zoom-in
  missed its bound), and a 75 % cap on the preloads' share of the slots (it starved the views
  ahead and coarsened a still view).
- Never released: what is drawn, a swap, the pinned floor, the chain of what is drawn, what
  is in transit; never released by the sweep under the target: what is wanted this step or
  was within the last `sweep_keep_poses` poses. The window and the metrics count the releases per step
  by reason — `headroom`, `below_floor_value`, `make_room` — by level × distance
  (`released_by_level_distance{level,distance,reason}`, `released_total{reason}`), GPU and
  decoded (`side` evict | decoded), and the dynamic headroom (`ScoreStats::headroom_bytes`).

Every number here is the specification's (`Params`, bounded, in the digest, recorded with
the step, re-specifiable at run time by `tuile://budget?…` and the config file): `a0`,
`a_desc`, `rho` (0: the profile's), `margin`, `headroom`, `floor_value`, `release_visits`,
`finer_gap` (#277's over-need weight), `restore_*_ms` (until measured), `horizon_ms`. The
floor level stays the profile's (`BudgetConfig::pinned_level`, `TUILE_PIN_LEVEL`): the
geometry server lists the floor from it.

A preload never displaces anything the present need wants; it displaces another preload only
when `P(new) > P(old) × (1 + m)` (hysteresis: two tiles of nearly equal score do not trade
places at every step).

## 4. The bound

The runtime lists, per level `ℓ` from `F + 1` to `n + 1`, the `K(ρ) = ⌊π(ρ+1)²⌋` tiles of
level `ℓ` nearest `F` among those with `d ≤ ρ` (nearest first, ties by tile id). So

```
preloaded tiles ≤ |floor| + (n + 1 − F) × ⌊π(ρ+1)²⌋
```

by construction, whatever the tree. With `n = 13`, `F = 3`: 11 levels; at ρ = 4, 858 tiles;
at ρ = 2, 308. A level-3 floor is 85 tiles on a 4ˡ pyramid. At ≈ 340 KiB a tile (drape and
its mipmaps, as `TUILE_PIN_LEVEL`'s note measures it) plus its mesh: about 300 MiB on a
desktop (target ≥ 1 GiB) and 105 MiB on a phone (target 160–768 MiB), the floor 28 MiB more.
Beyond what fits under the low watermark the lowest scores are simply not delivered: the
bound is on what is *asked*, the target on what is *held*.

## 5. Coverage

On a locally square grid of side `s_ℓ`, the tiles meeting a disc of radius `r` around `F` lie
inside the disc of radius `r + √2` (a tile's diagonal is `√2`), so there are at most
`π(r + √2)²` of them. They are also the nearest to `F`: any tile not meeting the disc is
farther. Keeping the `K = ⌊π(ρ+1)²⌋` nearest therefore keeps every tile meeting the disc of

```
r_ℓ = (ρ + 1 − √2) × s_ℓ          ≈ 3.59 s_ℓ at ρ = 4, 1.59 s_ℓ at ρ = 2
```

and `s_ℓ` doubles per level up. Once those tiles are resident, **any zoom-out or rotation
whose new view stays within `r_ℓ` of `F` finds level-`ℓ` ground under it**, for every `ℓ`
between the floor and `n`; beyond the coarsest listed level's disc, the floor covers the
globe. With `s_13` ≈ 4.9 km (a 4ˡ pyramid on the equator), the level-12 disc reaches 35 km,
level 10 140 km, level 7 1 100 km, level 4 9 000 km — a zoom-out from level 13 to the whole
globe crosses only resident ground. On a geographic grid near a pole a tile is narrower than
it is tall: the count `K` stays the bound, and the disc shrinks by `cos φ` in longitude.

Order of arrival: at equal distance a level `n−1` tile scores `a0 = 0.8` of a drawn one,
`n−2` 0.4, and the intermediate levels halve again each step up — they are loaded last and
released first, after every tile nearer the present need. Neighbours of the present level
come before the far coarse ones by `D`: at `d = ρ` a level-`n` tile scores ¼.

## 6. The work

- The neighbourhood is walked again only when `n`, the level-`n` tile under `F`, or `ρ`
  change: a still or slowly moving camera costs nothing. The walk visits at most
  `4 K` tiles per level (the children of the tiles within `ρ/2`).
- The budget reads it as part of the need's delta, tile by tile: a tile's `P` changes only
  when its `d` or `n` did, for the wants the need named (`ScoreStats::places`). A wanted
  resource is in no persistent index (the step's room sorts the wanted ones under pressure,
  as before); a held resource no longer wanted keeps its last `P` and ages by `L`. So a
  re-key of the persistent release index (`ScoreStats::rekeys`, O(log N) each) happens only
  for a filed resource whose probability moved — bounded by what the step named, never by
  what is held (`a_turn_re_keys_what_it_names_not_what_is_held`: the same with 4 000 and
  40 000 copies held).
- `d` is quantised in sixteenths of a tile; `F` is a tile's centre, so the distances move
  only when the eye crosses a tile of its level.
- The decision stays pure CPU and never waits on I/O; preloads are started with free slots
  only, at most three quarters of them (the last quarter stays free for the present need), and a hole preempts a preload in flight.

## 7. Instruments: one source, the window and Grafana

The budget fills the numbers while it decides; the window, the metrics and the log records
read them and never compute their own (`crates/tuile-budget/src/score.rs`).

- **Per decision** (`Decisions::scores`): every preload started and every GPU copy evicted,
  with its terms `A`, `D`, `P`, `R`, `bytes`, `score`, its level, distance band, side and
  reason (`preload`, `admission`, `swap`, `sweep`, `pressure`, `recycled`).
- **Per step** (`BudgetStats::score`): preloads in flight and evictions by level × distance
  band, and the parameters in force.
- **The held set** (`Budget::held_report`, taken by the geometry server at each publish,
  every 0.5 s, never in a step): per level × band the count, sum, min and max of each term,
  and the 32 lowest and 32 highest scored GPU copies outside the floor, with their state
  (`kept`: drawn or in a swap; `wanted`; `left`).

Units, the same everywhere: `A`, `D`, `P` in 1/32 000ths (`CERTAIN`; the window prints them
as fractions), `R` in µs, bytes in bytes (the window in MiB), the **score in µflouz per
reference byte** — `P × R × ref_bytes / bytes`, µs at certainty for a 256 KiB resource: the
unit of the release key's credit.

Metrics (`tuile_budget_` prefix; labels `level`, `distance` ∈ {0, 1, 2, 3, 4+}, `side` ∈
{preload, evict, held}, `term` ∈ {score, a, d, r_us, bytes}, `name`; never a tile):

| Series | Kind | Meaning |
|---|---|---|
| `param{name}` | gauge | `a0_permille`, `a_desc_permille`, `rho_tiles`, `margin_permille`, `floor_level`, `preload_slots_permille`, `need_level` |
| `need_level` | gauge | `n` |
| `preload_by_level_distance{level,distance}` | gauge | preloads in flight |
| `evictions_by_level_distance{level,distance}` | gauge | the last step's evictions |
| `score_decisions_total{level,distance,side}` | counter | preloads started, copies evicted |
| `score_sum_total{level,distance,side,term}` | counter | each term summed over them (mean = sum / decisions) |
| `score_min`, `score_max{level,distance,side,term}` | gauge | over the last publish period |
| `score_held_tiles{level,distance,side="held"}` | gauge | GPU copies held |
| `score_held_sum`, `score_held_min`, `score_held_max{level,distance,side,term}` | gauge | their terms |
| `score_events_dropped_total` | counter | decisions beyond the log records' rate limit |
| `exchange_threshold` | gauge | λ: the highest score released to make room or keep the target since the last publish — held under it would go, a candidate over it would load |
| `score_quantile{side,q,level}` | gauge | the 10th, 50th, 90th percentiles of the scores of the preloads started and the copies evicted since the last publish, and of the copies held (`side` preload, evict, held; `q` 0.1, 0.5, 0.9), from the cells' doubling histograms: no sort |

Log records (OTLP `/v1/logs` → Loki), one per decision (`body` `tuile.budget.decision`) and
one per listed held copy (`tuile.budget.held`, `end` lowest | highest, `rank`), attributes
`tile` (`level/x/y`), `resource`, `class`, `level`, `distance`, `side`, `reason` or `state`,
`a`, `d`, `p`, `r_us`, `bytes`, `score`; at most 2 000 decisions a push, the rest counted.
**A tile's address leaves the process here and only here**, as a log attribute, at
Laurent's request (2026-11-23); it is never a metric label.

The board's box plots (`tuile-ui` `level_scores`) draw those quantiles per level with λ
marked, and its waste (`level_waste`) is, once scores are published, the held bytes under λ
(the level's held histogram's share). The window prints the formula with its parameters in its header, the level × band table
(preloads in flight / evicted, or started / evicted since the cumulative view's start), the
terms per level and side, and the two held lists.

### Why a tile waits (`tuile_budget::waits`)

A wait is a tile the present need wants drawn that is not drawn as itself (shown by an
ancestor, or not at all), from the step it is first wanted to the step it is drawn as
itself; a tile the need stops wanting ends no wait. The **meter** follows them
(`BudgetMeter`, `waits::WaitTracker`) from the step records — `decide` does not, and pays
nothing for them: its allocation ratchets do not move. Each wait is classified when it
opens: a **first load**, or a **reload** of a mesh evicted (`Decisions::evict`) less than
5 s before (`reload_5s`) or 5 to 30 s before (`reload_30s`); an eviction is remembered 30 s
(`RELOAD_MEMORY`), past that a reload counts as a first load. A reload carries the
eviction's reason from the step's `Decisions::scores`: headroom sweep, below the floor
value, room made, or the rest (an epoch's recycling).

**What the soak's reloads are** (measured 2026-11-24 on #289). In the last window (the
tour's orbit again: a full turn a minute at full detail) 812 of 1 753 mesh waits are
reloads, about half after the sweep, half after admission. Every mesh the sweep took there
had last been wanted — by any list of the need, ring and ancestors included — 30 to 120 s
before; none was wanted that step or within the horizon (an in-engine trace: 656 of 656).
The orbit's own working set (290 MB of meshes and at least 650 MB of drapes drawn over the turn) does not fit
beside what the view holds now in the pool's 1.27 GB: the oldest copies are the next to come
back, the textbook worst case of a least-recently-needed order on a loop longer than the
cache. Nothing in the need predicts their return — the ring is around the footprint, not
along the turn — so no law on the present need keeps them; the sweep's law leaves the
soak's numbers unchanged (identical waits at a horizon of 0, 2 and 5 s). What moves them,
measured on a7a4f720 and not kept: the low watermark at 100 % instead of 90 % (no proactive band) —
reloads 812 → 568; sweeping only down to the dynamic headroom — 812 → 861. The step that
should keep them is a prediction of the views ahead along the turn (#282's `Need.ahead`),
whose tiles the need would then want, and the law would keep.

`BudgetSignals::waits` totals them by class, level and kind, their durations by kind on the
load latencies' power-of-two ms buckets, and the reloads by reason. Series:
`tuile_budget_waits_total{kind,class,level}`, `tuile_budget_wait_p50_ms{kind}`,
`tuile_budget_wait_p95_ms{kind}`, `tuile_budget_reloads_total{reason}`. The board's
**Waits** section prints, per kind, the rate since the last publish, p50 and p95 since the
start, and the count, with a hover legend; the reloads' legend splits them by the
eviction's reason. The 5-minute soak prints the same split for its first and last windows,
told the same way from its mesh waits and the decisions' evictions — comparable with a
budget that predates the classification.

### Why a tile under its need is blocked (`tuile_budget::blocked`)

A level's red triangle on the board says that tiles are under their need there with nothing
in flight or queued at that level for a second; it did not say why. Every tile the per-level
`under_need` counts — a wanted mesh drawn by an ancestor, at the wanted level; an exactly drawn
tile wearing a drape under the level decided for it, at the tile's level — now carries one
reason, read after the step from the state of the resource it waits on (its mesh; the drape
of its first short layer at the decided level): `not_requested` (not among the wants),
`deferred` (worn one level coarse: its own level is not wanted until the deficit's dwell is
paid, §5.3), `queued`, `origin_down`, `backoff` (a failed load waiting out its retry),
`fetching_store`, `fetching_network`, `decoded` (waiting for a delivery), `uploading` (handed to
the consumer and not acknowledged: the upload or the GPU encode, deferred by the frame's lanes
or under way), `swap` (drawable, its swap group not complete). A level's reasons sum to its
`under_need`. A tile is dated by the step it fell under its need, whatever its reason since;
the step's own time, never a clock, and none of it in `decide`: the walk that counts
`under_need` classifies.

`LevelStats::blocked` and `blocked_oldest_ms` carry them; series
`tuile_budget_blocked_tiles{class,level,reason}` and
`tuile_budget_blocked_oldest_seconds{class,reason}`, each exposed from its first tile. On the
board, the hover of a level's mark (its residency region) and of its under-need cell lists the
reasons, the most tiles first, each with the oldest one's age.

### Grafana panels (proposal, to port into the STL dashboards)

```json
{
  "title": "Budget — progressive score",
  "panels": [
    {"type": "stat", "title": "Parameters",
     "targets": [{"expr": "max by (name) (tuile_budget_param)", "legendFormat": "{{name}}"}]},
    {"type": "heatmap", "title": "Preloads in flight, level × distance",
     "targets": [{"expr": "sum by (level, distance) (tuile_budget_preload_by_level_distance)", "format": "table"}]},
    {"type": "heatmap", "title": "Evictions /s, level × distance",
     "targets": [{"expr": "sum by (level, distance) (rate(tuile_budget_score_decisions_total{side=\"evict\"}[1m]))", "format": "table"}]},
    {"type": "timeseries", "title": "Mean score by level and side",
     "targets": [{"expr": "sum by (level, side) (rate(tuile_budget_score_sum_total{term=\"score\"}[5m])) / sum by (level, side) (rate(tuile_budget_score_decisions_total[5m]))", "legendFormat": "{{side}} z{{level}}"}]},
    {"type": "timeseries", "title": "Held score range by level",
     "targets": [{"expr": "min by (level) (tuile_budget_score_held_min{term=\"score\"})", "legendFormat": "min z{{level}}"},
                 {"expr": "max by (level) (tuile_budget_score_held_max{term=\"score\"})", "legendFormat": "max z{{level}}"}]},
    {"type": "timeseries", "title": "Mean A and D of evictions",
     "targets": [{"expr": "sum by (term) (rate(tuile_budget_score_sum_total{side=\"evict\",term=~\"a|d\"}[5m])) / ignoring(term) group_left sum(rate(tuile_budget_score_decisions_total{side=\"evict\"}[5m])) / 32000", "legendFormat": "{{term}}"}]},
    {"type": "table", "title": "Next released (Loki)", "datasource": "Loki",
     "targets": [{"expr": "{service_name=\"tuile-viewer\"} | json | body=\"tuile.budget.held\" | end=\"lowest\" | line_format \"{{.tile}} z{{.level}} d{{.distance}} score {{.score}}\""}]},
    {"type": "logs", "title": "Evictions by tile (Loki)", "datasource": "Loki",
     "targets": [{"expr": "{service_name=\"tuile-viewer\"} | json | side=\"evict\""}]}
  ]
}
```

## 8. Tests

`crates/tuile-budget/tests/budget_progressive.rs`, the need built as the geometry server
lists it (each red with the neighbourhood left out of the need, or with the probability
taken out of the release key):

1. `the_floor_through_level_three_stays_under_pressure` — a budget cut to a twentieth evicts
   preloads, never a tile of levels 0–3;
2. `an_aggressive_zoom_out_from_level_13_finds_its_ground_resident` — levels 11 and 12 around
   the footprint resident before it starts; from 13 to 5, one level every two steps, no load
   completing: no hole, no tile drawn by a coarser ancestor;
3. `a_rotation_near_the_ground_finds_its_neighbours_resident` — the view turns from east to
   west at level 13 and draws exactly at once;
4. `the_release_order_follows_the_score` — property: under a random cut, every preload
   evicted scores at most the least preload kept;
5. `the_preloads_are_bounded_per_profile` — at most `⌊π(ρ+1)²⌋` a level, and `A`, `D` as
   written in §2;
6. `preloads_continue_when_idle_and_never_delay_the_present_need` — a still view keeps
   starting preloads until its neighbourhood is in; a hole opening then starts on the step it
   opens;
7. `a_preload_worth_more_by_the_margin_takes_a_weaker_ones_place` — a zoom-in with the room
   full: the levels above the new need replace the coarse ones by swaps.

Beside them: `tuile-core`'s `neighbourhood::tests` (on a flat quadtree: the per-level bound,
the levels listed, every tile meeting the disc of radius `(ρ + 1 − √2) s_ℓ` listed, no walk
while nothing moved), `budget_signals::the_score_series_are_the_decisions_terms` (the series
are the decisions' terms, one source), the window's `the_score_rows_are_the_series_numbers`
and the log records' `a_decision_record_carries_its_terms`.
