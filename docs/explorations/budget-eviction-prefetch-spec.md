<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# One decider for eviction and prefetch: laws, specification, control

Status: **proposed, 2026-11-20**, for Laurent's review. The laws of §2 were shown to and
agreed by Laurent ("on y va", 2026-11-20); §3–§10 turn them into types, a control model,
tests and a plan of PRs. The four questions of §11 are still open: each has a proposed
default.

This is steps B and C of the budget plan (`graphics-budget-roadmap.md` §B and §C, plan
"REVISED ORDER" of 2026-11-02), built as **one module**: one decider proactively chooses
both what to release and what to load ahead, from one value per resource. Film, known path
and interactive use are three **specifications** of that decider, not three modules
(Specification pattern, re-specifiable while running).

What it replaces is listed in §8; nothing old is kept behind a switch.

## 0. Why now

PR #251 (Cédric) measured a real defect: a viewer that moved then stopped converged on a
patchwork — on a recorded session (a tilted camera, 8 s, replayed A/B), 42 and 31 tiles of
1 843 stayed **a level under their need, at rest, for good**, while the budget called the
view converged. Two rules kept them there (`engine.rs`, `tile_wants` and `drape_levels`):

- a drape worn one level under its need, under `Pressure::Normal`, is never asked for: the
  ±1 tolerance of `underdraped` hides it;
- a needed drape held nowhere locally takes a held level within one, even at scale 1, and
  that stand-in is remembered as the tile's level.

#251 fixed it with a still-view regime (a 250 ms stillness detector, a catch-up cursor) and
a 16× relaxation descent when a pressure lifts. Laurent judged both stopgaps: they are modes
(a "still" switch, a "lifted" switch) on top of the model, where the plan wants one
continuous model driven by an estimated next use per resource. What is kept from #251:

- its **measurement** — tiles under their need at rest — becomes one of the quality
  objective's measured quantities (§1) and part of every scenario's acceptance (§9.1: 0
  tiles under their need within 2 s of rest). The recorded session itself is **not** a test
  input (Laurent, 2026-11-20): the module is judged on a catalogue of predefined, scripted
  scenarios (§9.3);
- the rule "a local stand-in is taken only under a relaxation", extracted by another agent
  into its own small PR (`fix/budget-standin-under-relaxation`); this stack builds on it
  and rebases onto it when it lands.

#252's diagnostic views (levels, lag, cap) are the precursor of this module's
decision-reason view (§7.3).

## 1. The objective

**Maximise the quality of the scene**: minimise the expected **screen-weighted deficit**
between the level each visible place wears and the level it needs, integrated over a short
horizon, where **a hole has infinite cost** — under memory, bandwidth and compute limits.

Formally, per step `k` at time `t_k`, over the drawn items `i` and each content `c` they wear
(the mesh, one drape per imagery layer):

```
D(k)   = Σ_i Σ_c  w_i · max(0, need_c(i) − worn_c(i))          (levels × weight)
D⁺(k)  = Σ_i Σ_c  w_i · max(0, worn_c(i) − need_c(i))          (over-detail, reported apart)
J      = Σ_k D(k) · (t_{k+1} − t_k)  +  λ_q · Q  +  ∞ · holes
```

- As implemented (PR 2), `D(k)` adds two measured parts: the levels a drawn place wears
  under the level the step decided for it, and the levels that decided level is coarser than
  the unrelaxed need (a stand-in, a relaxation: `imagery_short`).
- `need_c(i)` is the level the picture needs **at scale 1** — the unrelaxed footprint level
  with its ±½-level hysteresis (`imagery::needed_level` on the unrelaxed footprint) for a
  drape, the wanted tile's level for a mesh. It is deliberately *not* the relaxed level or
  the level remembered in `imagery_prev`: a stand-in remembered as a level was exactly how
  #251's deficit hid. Under a relaxation (law 10) the deficit is non-zero by design and
  measured as such.
- `worn_c(i)` is the level of the resource actually drawn: the item's own mesh, or the
  ancestor drawn in its place (`Role::Fallback`); the drape worn, own or ancestor's.
- `w_i` is the screen weight, an integer: `500 + centrality/2` (‰, the weight `priority::score`
  already uses) until the need carries a screen area (§11, follow-up: `Need::screen_px`,
  which the traversal has).
- A **hole** — a wanted place drawn by nothing (`lost > 0` once the floor is in), or a drawn
  item without a drawable drape for a layer it needs (`bare > 0`) — makes `J` infinite. The
  absolute laws (§2.1) make it impossible by construction; the simulation checks it every
  step.
- `Q` is the congestion penalty (§6.4): queue depth, waits, deferrals.

Eviction and prefetch are **two faces of one choice**: which set of resources to hold.
Loading a resource and keeping one are both "hold it"; releasing and not loading are both
"don't". The decider makes that one choice from one value per resource.

## 1b. The core: one currency (Laurent, 2026-11-20)

After Patterson et al., *Informed Prefetching and Caching* (TIP, SOSP 1995): **one manager
arbitrates every byte** between competing uses — prefetching announced or predicted
resources, retaining held resources likely to be reused, and the rest — by estimating, in
**one common unit**, the benefit of giving a buffer to a use and the cost of taking it from
another, and moving buffers while the exchange is profitable.

**The unit is expected screen quality gained per byte over a short horizon.** The
currency has a dimension: **pixel · level · step** — px·lvl·s in interactive use, px·lvl·frame
in a film (the step axis is abstract, §5.4). Its name is Laurent's to choose (proposal:
*tessera*, plural *tesserae*); the code uses one newtype for it (`value::Tessera`) and
renames it once chosen. A value in µs does not compile.

```
benefit(r) = Σᵢ P(r visible at tᵢ) · Δq(r) · w(r) · δ(tᵢ) · Δtᵢ          [px·lvl·step]
```

with `Δq` in levels, `w` in pixels of screen covered, `P` and `δ` dimensionless, `Δt` in
steps. The two sides of the exchange, each in currency, then per byte:

- **loss of releasing a held `r`** = `P(r needed again within H) · Δq · w · T_restore(tier)`
  — the quality missing while it comes back: the **restore cost is in the numerator**, so a
  resource dear to restore (network) is worth more held than a cheap one (decoded RAM), as
  GreedyDual / TIP and law 6 say;
- **gain of loading a missing `r`** = its benefit counted only **from its arrival**
  (`now + T_fetch(tier)`) onward — slow fetches earn less, and urgency follows from it;
- **exchange**: load `A` releasing `B` iff `gain(A)/size(A) > loss(B)/size(B) + margin`, both
  in currency per byte;
- **each limit `k`** (GPU bytes, RAM bytes, upload bytes, GPU ms, …) has a **price `λₖ`** in
  currency per unit of that resource; the objective `Σ gains − Σₖ λₖ · useₖ` is homogeneous
  (the regulator of §6 sets the prices).

*(Correction, 2026-11-20: an earlier version of this section divided the benefit by
`size × c(tier)` with `c` in µs — dimensionally wrong, and inverted for eviction: it would
have released the network-tier resource first. The implementation of PR 4 keeps the restore
cost in the numerator — see its test `a_dear_restore_is_worth_more_held`.)*

| Term | Unit | Meaning | Set by |
|---|---|---|---|
| `P(r visible at t)` | dimensionless (‰ in code) | probability that `r` is drawn at `t` | the need (P = 1 for the present need), the predictions (§5): film / FPV film P = 1 on the known path; known path decaying where the user may interrupt; interactive decaying fast with time |
| `Δq(r)` | levels (level-equivalents) | the deficit reduction holding `r` brings: `need − worn` for the place it serves (§1) | the step; every resource class's producer states it through its dimensionless `quality_scale` (below) |
| `w(r)` | pixels | the screen area the place covers | the need; `Term::ScreenSize`'s weight |
| `δ(t)` | dimensionless | discount with delay: 1 at 0, 0 at `H` | the specification's horizon and discount shape (FPV: `H` grows with speed) |
| `Δtᵢ` | steps (s or frames) | the sampling interval of the discrete horizon (§1c) | the specification |
| `size(r)` | bytes | what holding it takes | the cost estimator |
| `T_restore(tier)`, `T_fetch(tier)` | steps | time to bring it back from the tier it would come from: decoded, RAM, local store or browser cache, network | measured (§4.2), the specification's defaults until then — GreedyDual-Size / Landlord's credit and rent (Young; Cao & Irani) |
| `λₖ` | currency per unit of resource `k` | the price of a limit | the regulator (§6) |

**Contract for every resource class** (terrain, drapes, 3D Tiles, and the classes of the
atmosphere / voxels / procedural study, #260): its producer gives `Δq` in **level-equivalents**
— through a dimensionless `quality_scale` that maps its own refinement steps onto levels —
and `w` in **screen pixels** (a cloud's weighted by its transmittance). With that, every class
is valued in the same currency and shares the one pool.

Consequences, by construction rather than by rule:

- **The present need ranks highest**: P = 1000 and no discount. No separate "present first"
  tier is needed for ordering; law 4 stays as the guarantee and the anticipation quota as
  its bound.
- **Anticipation is discounted** by uncertainty (P) and time (δ); a near-certain known
  future (film, FPV film) can outrank a marginal present refinement.
- **Urgency rises continuously** as a deadline approaches: the slack `deadline − estimated
  fetch time` (the measured tier costs) enters δ — no thresholds.
- **Prefetch and eviction read the same order**: load the top missing, release the bottom
  held; a swap is made only if the gain exceeds the loss by the margin (law 7; Cao, Felten,
  Karlin & Li's *Do No Harm*: never release A to fetch B if A is used before B).
- **Outside the currency**: the absolute laws (coverage floor first, no hole by the
  decider's actions, the hard limits) and the anticipation quota.

**It replaces today's two disjoint orders**: the load `PriorityKey` (lexicographic urgency
tiers Floor / Hole / Blocker / Drape / Ring / Ancestor / Refresh, then `sse_excess × (0.5 +
0.5 × centrality) + age + cover`, then the residency tier) and the `EvictKey` (LRU last
needed, then finer, then farther). Kept from them: integer and fixed-point arithmetic, a
total order with stable tie-breaks, the protected sets, the incremental dirty-set
recomputation, and sampled candidates on the release side to bound `decide`'s time.

Where it lands: PR 4 puts the release side on the value (restore cost per tier, size,
recency standing for P until predictions exist) and the present-need deficit term; **PR 4b**
moves the load side's `PriorityKey` onto the same value — the urgency tiers reduced to the
absolute laws' bands (floor and coverage, blockers of a swap) with the value ordering
everything else (§10).

### 1c. The mechanics: TIP's principle, per-frame mechanics (Laurent, 2026-11-20)

TIP's **principle** is kept (one currency; exchange a buffer while the gain exceeds the
loss); its **mechanics** are adapted to per-frame decisions over thousands of resources, in
a fraction of a millisecond, native and wasm:

1. **Two ordered structures, no global recomputation**: the missing resources by value per
   byte descending, the held ones by value per byte ascending (the persistent eviction
   index of O1.2 is the second). Each step **exchanges while best-missing > worst-held × (1 +
   margin)**: O(k log n), `k` bounded by the rate shaping (8).
2. **Values quantised into integer buckets** (a bucket queue, plan item O1.5) for near-O(1)
   insert and extract; ties inside a bucket by the stable tie-breaks.
3. **Incremental recomputation** through the existing dirty sets: only resources whose
   inputs changed are re-valued (the deficit's re-read deadline, §5.3, is one such input:
   time).
4. **Precision near the frontier only** (LRB's insight): resources far above or below the
   exchange frontier keep an approximate value refreshed less often; the order is exact
   where a decision is taken.
5. **Sampled candidates on the held side** (hyperbolic caching; Im et al.): the first `k`
   of the index and a deterministic sample, not a perfect order over the whole held set.
6. **A discrete horizon**: the value summed at a few instants — now, +0.5 s, +1 s, +2 s,
   specification parameters — with `P(visible)` from cheap geometry (the union of predicted
   frusta), no ray casting.
7. **Costs from a small table**: the per-tier restore cost (smoothed from measurements,
   §4.2) × the known size; no disk model à la TIP.
8. **A bounded number of exchanges per step** (a token bucket), so per-frame work is
   bounded — P6.4: work grows with changes, not with the scene.

**Validation — the exact reference as judge.** The simulator keeps an **exact TIP**
implementation (the whole value recomputed for every resource every step, every exchange
evaluated exactly; slow, small scenarios only), as Albers' offline optimum judges the online
policy. The fast decider is run beside it on the catalogue's small variants and compared on
the objective `J` (time-integrated screen-weighted deficit, §1). **Acceptance**: the fast
version's `J` within 2 % of the exact one on every small scenario (a first proposal: the gap
must be negligible), and never a law the exact one keeps broken. This is PR 6's
`exact_tip_reference` test; the approximations 2, 4 and 5 are only accepted with it green.

## 2. The laws

Thirteen laws, each with the invariant that tests it. **Absolute laws** stand outside any
specification: no specification can relax them, so a bad specification costs reloads, never
a hole. **Choice laws** fix the shape of the decision; their numbers come from the
specification. **Specification laws** say what a specification is.

### 2.1 Absolute laws (outside any specification)

Refined after Laurent's review (2026-11-20): the laws constrain the decider's **actions**,
not outcomes it cannot control.

| # | Law | Testable invariant |
|---|---|---|
| 1 | **The decider never creates a hole.** No eviction, release or swap of the decider leaves a place uncovered: what is drawn, the fallback chain of every needed-but-absent place, and both halves of a swap group are never released, and a replacement is active before what it replaces is released. Coverage has priority over every other want. It cannot promise what the network does not deliver — a first load, a floor tile that never arrives — so those are **measured** (time to first complete view, §7.1), not violations. | Every sim step: no `evict`/`release` entry is in the protected set P (I11); for every swap, the step that stops drawing the old drew the new (I9); a place covered at step k−1 is covered at step k unless no copy of it or an ancestor was ever drawable (`swap_gaps_total == 0`). `lost` and `bare` are reported (time-to-coverage bounds per scenario), and must be 0 once the floor is in. Proptest over random needs, arrivals, pressures **and random specifications** (§3.6). |
| 2 | **Coverage first.** The coarse floor covering the view is loaded before any detail, and a fine level is never drawn without an ancestor drawable behind it. It does **not** mean loading every intermediate level in order: skipping intermediate levels is allowed and desirable (a fast dive; skip-LOD saves 30–50 % of loads in the reference engines, to measure on the catalogue). The floor is always held. | Within a step: no fine-band `start` while a floor tile of the view is wanted and admissible and not started; every drawn `Exact`/`Held` mesh has a drawable ancestor chain to the floor (or is the floor); the floor is never in `evict`. A dive scenario counts the intermediate levels skipped (`levels_skipped_total`) — more is better, never a hole. |
| 3 | **The hard limits are never crossed.** The limits are the GPU bytes of each class and of the pool (`hard_limit`, refined by the measured device limit, §4) and the decoded RAM (`decoded_target` and its hard bound from the measured available RAM). They account for the **transient peak**: uploads in flight and the overlap that "new before old" costs during a swap (`hard_reserve_percent` keeps that overlap). When the floor and the drawn set cannot fit, **quality yields** (the LOD scale relaxes the screen-space error), never the limit. | Every step, every class and the pool: `gpu + in_transit ≤ hard_limit`, `decoded ≤ decoded hard`; a step whose protected set exceeds the target raises the LOD scale rather than delivering past the limit (sim: pressure ramp, memory squeeze). |
| 4 | **Present need before anticipation — a priority, not starvation.** A prefetch never displaces a load the picture needs now, but anticipation keeps a **reserved, bounded share** of bandwidth and memory (the speculation quota) that varies **continuously** with speed and with the recent prediction accuracy (a Flare-like margin, §10b), so prefetch is never starved in fast motion (FPV). | Every step: no eviction of a present-need resource for a speculative one; speculative in-flight ≥ min(reserved share, speculative wants) whenever speculative wants exist (no starvation) and ≤ the share's ceiling; the share is a continuous function of the recorded speed and accuracy (unit test: no step of the share larger than its rate bound). |
| 5 | **Reproducible.** Same inputs, same decisions. Predictions, specifications and measured inputs are recorded step inputs; integer arithmetic; stable tie-breaks; replay gives identical decisions. Executor-measured numbers enter `decide` only as recorded inputs. | The decision digest (`det!`, goldens) is a function of the recorded inputs only: a step stream recorded to MCAP (a catalogue run, or a viewer session tape) replayed gives the same digest, bit for bit; nothing in `tuile-budget` reads a clock, a hash order or a float that was not quantised once at the boundary. |

**Hierarchy when laws conflict**, strongest first: **hard limit > no hole created by the
decider > coverage first > present need > anticipation under its quota**. When one cannot
hold, **quality yields** (the LOD scale, the deficit), never a higher law.

**Near-absolute requirements**, with their invariants: **stability** (no oscillation, bounded
settling — §6.5's SASO checks, I3, I4) and a **bounded decide compute time** (≤ 10 % of a
frame at p99, the P6.4 work bound — §11 Q2).

### 2.1b First load: the same decider from an empty state

Nothing is special-cased; from an empty state the laws and the value give, in order:

1. the **minimal coarse coverage** of the camera's view — terrain coarse **and** its drape
   together (one without the other is not a usable view: the floor's mesh and drape are one
   swap group, `Urgency::Floor`) — as fast as possible;
2. at equal value, **cheapest first** — the local store or the browser cache before the
   network (`PriorityKey`'s tier tie-break; local reads on their own quota), so a warm start
   is near-instant;
3. then refinement by value (deficit × screen weight; centre first), skipping useless
   intermediate levels (law 2);
4. anticipation only once coverage is reached, under its quota (law 4).

Metrics: `time_to_first_complete_view_seconds` (first step with the floor in and `lost ==
bare == 0`; today's `floor_seconds` is its first part) and `time_to_settled_view_seconds`
(first step with the deficit under its threshold, after `converged_seconds`). Catalogue
scenarios: `cold-start` (empty cache, `slow` network) and `warm-start` (full cache), each ×
device budget, with bounds: cold desktop first complete view ≤ 3 s, settled ≤ 8 s; warm ≤ 0.5 s
and ≤ 2 s (first proposals, to measure).

Law 5 amends the plan's rule "executor-measured numbers never enter `decide`" (Laurent,
2026-11-20): measured numbers (§4) **do** enter, but only as fields of the step's input, so
the step record carries them and a replay feeds the same numbers back. The rule's intent —
no hidden input — is kept; its letter changes.

### 2.2 Choice laws (parameters set by specifications)

| # | Law | Testable invariant |
|---|---|---|
| 6 | **One value per resource**, from its probability and time of next use, its restore cost by tier (decoded / local store / browser HTTP cache / network) and its size. Load what is missing and worth most; release what is held and worth least. | Unit: `value` is monotone — non-increasing in the delay to next use, non-decreasing in probability and in restore cost, non-increasing in size per byte. Sim: every eviction's victim has a value ≤ every held evictable resource of its class not evicted that step (eviction order = value order within band). |
| 7 | **The same rule both ways, with a margin.** No load that would be evicted at once; no eviction that would be reloaded at once; a swap is made only if its gain exceeds its cost. | Sim: `thrash` (network refetch of a resource evicted < 10 s before) and `rereads` bounded per scenario (ratchet); no resource is started and evicted within `min_dwell` (§11 Q1) unless the hard limit forced it (reason `hard`). Unit: a load whose value is below the lowest held evictable value × (1 + margin) is not admitted when it needs room. |
| 8 | **When known, don't guess.** Film or known path: a fixed schedule, no speculation. Interactive: probabilistic, and speculation has a bandwidth quota and a memory share it never exceeds. | Film spec: every `start` is of a resource the schedule needs, `wasted_starts == 0`, nothing in the Predicted band. Interactive: speculative in-flight ≤ `speculation.slots` per class, speculative bytes held ≤ `speculation.memory` of the target, every step. |
| 9 | **Calm when nothing changes.** No decision moves without an input change; at rest every place reaches its need in bounded time; no oscillation. | Still camera, steady measured inputs: after settling the decision digest is constant (I3); every drawn place reaches `worn == need` within `T_rest` (desktop 2 s, handheld 4 s — §9.1); no place changes level twice in 10 s (I4); the controller's state (§6) reaches a fixed point (no limit cycle: the state sequence becomes constant). |
| 10 | **Graceful degradation.** Quality falls and recovers as continuous functions of pressure: no steps. | Pressure ramp scenario: the LOD scale and `D(k)` change by at most `Δ_max` per second (no step larger than one controller increment); recovery after the pressure lifts follows the same controller (no special haste), settling within `T_recover`, overshoot ≤ 10 % (§6.5). |

### 2.3 Specification laws

| # | Law | Testable invariant |
|---|---|---|
| 11 | **A policy is data.** Composable rules (and/or/not, weights, thresholds) — restore cost per tier, known next use, gesture-inertia probability, speculation quota. Film, known path and interactive are three specifications of the same decider, not three modules. | There is one `decide`; the three default specifications are values (`Specification::interactive()`, `::known_path()`, `::film()`), each round-trips through its serial form (`to_query`/`from_query`, the `tuile://budget` grammar) and through the step record. A grep test: no `match` on the specification's kind inside the decision code — the kind is a label for telemetry only. |
| 12 | **Re-specifiable while running.** A change is a recorded input of the next step; the switch is smooth (blended); absolute laws stay outside, so a bad specification costs reloads, never a hole. | Proptest: random specification switches mid-script keep laws 1–4; the step after a switch decides with the blend (§3.5) — no step evicts more than the shaped release rate (§6.3) because of the switch; replay of a session with switches gives the same digest. |
| 13 | **Every decision says why.** The rule that caused each load and each release is recorded, visible in telemetry and in the debug view. | Every `start`, `evict` and `release` entry of `Decisions` has a `Reason` (§7.2); the counts per reason sum to the counts per list; the sim checks the sums every step. |

## 3. The specification model

### 3.1 Shape

A specification is a value, built from composable parts. It lives in `tuile-budget`
(`spec` module), integers only, `Eq + Hash`, validated once.

```rust
/// What a specification is for: a label for telemetry and the menu, never
/// branched on by the decision code (law 11).
pub enum SpecKind { Interactive, KnownPath, Film, Fpv, FpvFilm, Custom }

pub struct Specification {
    pub kind: SpecKind,
    /// Terms that make a resource's value (law 6), each under a condition.
    pub rules: Vec<Rule>,
    /// The numbers of laws 7–10 and of the controller (§6).
    pub params: Params,
}

/// One term of the value, applied to the resources its predicate selects.
pub struct Rule {
    pub when: Predicate,
    pub term: Term,
    /// ‰ multiplier of the term (1000 = as is).
    pub weight: u32,
}

/// The Specification pattern proper: a predicate over a resource's facts,
/// composable.
pub enum Predicate {
    Always,
    Class(Class),
    LevelAtMost(u32),
    LevelAtLeast(u32),
    Tier(Tier),               // where it would be restored from
    Band(Band),               // Floor | Coarse | Fine
    Membership(Membership),   // drawn, blocker, ancestor, ring
    Predicted,                // a prediction names it this step
    Not(Box<Predicate>),
    All(Vec<Predicate>),      // and
    Any(Vec<Predicate>),      // or
}

/// A value term, in the currency (px·lvl·step; `value::Tessera`).
pub enum Term {
    /// Recency: the time it was last needed (LRU's key; GreedyDual's L).
    Recency,
    /// Restore cost by tier (GreedyDual-Size credit): `cost_us[tier] × ref_bytes / size`.
    RestoreCost,
    /// Known or predicted next use: `p × (horizon − delay)⁺`.
    NextUse,
    /// The quality gained by holding it: `w_i × |need − worn| × dwell`
    /// (the load side of the deficit, §5.2).
    Deficit,
    /// The place's screen weight `w_i` (§1): large on screen first (§3.7).
    ScreenSize,
}

pub struct Params {
    pub coarse_through: u32,        // law 2: levels ≤ this are the coarse band
    pub margin_permille: u32,       // law 7
    pub min_dwell: Micros,          // law 7, §11 Q1
    pub horizon: Micros,            // the prediction horizon
    pub restore: RestoreCosts,      // per tier, shared by every spec (§4.2)
    pub speculation: Speculation,   // law 8: slots ‰ per class, bytes ‰ of target
    pub headroom: Headroom,         // §6.2: the band below the limits kept free
    pub flow: Flow,                 // §6.3: token buckets for starts, uploads, releases
    pub controller: Gains,          // §6.4: the damped controller's gains
    pub blend: Micros,              // law 12: how long a re-specification blends
}
```

Value of resource `r` under specification `S` at step `k`:

```
v_S(r, k) = Σ_{rule ∈ S.rules, rule.when(r)}  rule.weight × rule.term(r, k) / 1000
```

The order the decider uses (ascending = released first, descending = loaded first) is
lexicographic: `(protected?, band, value, Reverse(level), Reverse(distance), id)`.
Protection (law 1) and the band (law 2) come **before** the value: no specification reaches
them. The value is a `u64`; every term is saturating integer arithmetic.

### 3.2 The three default specifications

| | Interactive (viewer, game) | Known path (flight, tape) | Film (batch) |
|---|---|---|---|
| Recency | ×1000 | ×1000 | ×0 |
| RestoreCost | ×1000, all tiers | ×1000 | ×1000 (ties) |
| NextUse | ×1000, gesture/extrapolation `p`, horizon 1 s | ×2000, path `p` decaying where the user may interrupt, horizon 8 s | ×4000, exact `p = 1`, horizon = the schedule's window |
| Deficit | ×1000 | ×1000 | — (Batch draws only complete frames) |
| Speculation | 25 % of slots, 20 % of the target | 50 % of slots, 30 % | 0 (nothing speculative: the schedule is the need) |
| Margin | 25 % | 25 % | 0 |
| min_dwell | 1 s | 1 s | 0 |
| Headroom band | 8–12 % below the pool's target | same | none (unbounded) |

Film is Bélády weighted by restore cost ("weighted offline caching", plan correction of
2026-11-02): with tiers the right key combines next use with restore cost.

The numbers are first proposals, to be tuned by the simulator's parameter search (§9.3).

### 3.3 How a specification is supplied

**The specification follows the context** (Laurent, 2026-11-20: "in the viewer it will be
the viewer's budget… in FPV it will be different"). Nobody picks a budget from a list in
ordinary use:

| Who selects | From what | Example |
|---|---|---|
| The **host**, from its use context | what the camera is doing | the viewer's ordinary navigation runs `interactive`; entering the viewer's FPV camera/navigation mode (or a host app's FPV flight) runs `fpv`; leaving it returns to `interactive`; a Glide flight or a tape replay runs `known_path` while it plays |
| The **film**, from its plan | the plan's camera | a film runs `film`; an FPV film runs `fpv-film` with its schedule |
| An **override**, for tests and diagnosis only | the menu's *Budget (diagnostic)* entry, `tuile://budget?spec=…`, the control API | comparing `fpv` against `interactive` on the same flight; clearly labelled as a diagnostic, cleared by `tuile://budget?spec=auto` (back to the context's) |

A context's specification carries its own numbers where they differ — memory share of
speculation, horizon, quotas. Entering or leaving a context is a re-specification like any
other: recorded with the step, blended (§3.5), absolute laws untouched.

The mechanics:


- The host gives it with the step: `FrameInput::with_spec(&Specification)`. A step given a
  specification whose digest differs from the one in force starts a re-specification (§3.5).
  A step given none keeps the one in force. `Budget::new` starts on
  `Specification::for_policy(cfg.policy)` (Interactive → interactive, Batch → film).
- `Specification::validate()` bounds every number (weights ≤ 16 000 ‰, horizon ≤ 60 s,
  quotas ≤ 100 %, margin ≤ 400 %, predicates nested ≤ 8 deep and ≤ 64 rules). An invalid one
  is refused by the host API with an error; it never reaches `decide`.
- Serial form: a query string, the same in `tuile://budget?…` and in the control API:
  `spec=interactive&margin=250&horizon_ms=1000&speculation_slots=250&…` — a named default
  with overrides. A free composition of predicates is the library API's (no parser for
  arbitrary trees in a URL; §11 Q3).

### 3.4 How it is recorded and replayed

- `StepRecord` gains `spec: Option<&Specification>` (present on the step that changed it,
  and on the first), `measured: &Measured` (§4) and `predictions: &[Prediction]` (§5).
- The MCAP session tape (`/budget/step`) carries them; the replayer feeds them back. A
  specification is small (≤ 64 rules): recorded whole, not by digest.
- The decision digest hashes the specification's digest on the steps that carry one, so a
  goldens run with another specification cannot pass for the default one.

### 3.5 Blending on re-specification (law 12)

A specification change is smooth by construction, at no refile storm:

- **Weights blend linearly** over `blend` (default 2 s): at step time `t` after the switch at
  `t₀`, every rule's effective weight is `w_old + (w_new − w_old) × min(1, (t − t₀)/blend)`,
  in integers; rules present in one spec only blend from or to 0. Parameters with a natural
  rate (margin, quotas, headroom band) blend the same way.
- **Values are tabulated, then filed**: the specification's terms are evaluated once into
  a table per class, level and copies held (`state::Valuation`), which the persistent
  eviction index's key reads. A switch replaces the table and files every filed resource
  again under its new key, once — O(N log N), counted in `work` (the first implementation,
  PR 4). With the blend (PR 7) the table is re-made at a bounded number of blend points
  (e.g. 4 over `blend`), not every step, and the release bucket bounds what any re-filing can
  evict per step.
- **Absolute laws stay outside**: protection and bands are not terms. The shaped release
  rate (§6.3) bounds what a switch can evict per step. A specification that values
  everything at zero degrades to "evict the oldest finest first" — reloads, never a hole.

### 3.6 Random specifications in the property tests

`proptest` strategies generate arbitrary valid specifications (rules, predicates, weights,
params). Every absolute law (§2.1) is asserted under them, on the same scripts as the
existing `budget_properties` suite. This is the proof that "a bad specification costs
reloads, never a hole".

### 3.7 Worked example: the FPV specification

First-person-view flight (Laurent, 2026-11-20): a drone-like camera, low over the terrain,
fast, banking and yawing, always moving forward — in films and in interactive use. It is a
**specification composed from the generic rules**, not a mode: no line of the decider knows
it exists.

What FPV needs, and the rule that gives it:

| Need | How the generic model expresses it |
|---|---|
| What lies ahead is used next | **NextUse** on predictions along the predicted forward path: exact from the plan in a film (the film's schedule), from velocity and attitude extrapolation in interactive use (§5.2) |
| The look-ahead grows with speed | the horizon is `horizon = clamp(k_h × speed, h_min, h_max)`: a **corridor** (a cone ahead of the camera) whose length is time, not distance; the producer of the predictions applies it (it knows the speed), and records it with them |
| Soon-visible and large on screen first | NextUse's `p × (horizon − delay)⁺` already favours time-to-visibility; a **ScreenSize** term (`w_i`, §1) weighted ×2000 for FPV |
| The near field matters most | `Rule { when: All[Membership(Drawn), LevelAtLeast(near_level)], term: Deficit, weight: 2000 }` with `near_level` = the deepest level drawn (the near terrain and its drapes), and the same for `Class(Drape)` |
| Behind the camera, or out of the corridor, loses value fast | `Rule { when: Not(Predicted), term: Recency, weight: 250 }` — recency counts a quarter as much as in the interactive spec, so what left the corridor sinks below everything predicted within a fraction of a second and is released early, **proactively and rate-shaped** (the release bucket of §6.3), never in a burst |
| Speculation sized for speed | `speculation.slots = clamp(k_s × speed, 25 %, 60 %)`, `speculation.memory = 35 %` — the quotas are parameters, and the interactive FPV variant's producer reports the speed so the blend (§3.5) follows it smoothly |
| Coarse first still holds | unchanged: the corridor's coarse band is loaded before its fine band (law 2); absolute laws untouched |

The two variants:

| | `fpv` (interactive) | `fpv-film` |
|---|---|---|
| Predictions | probabilistic: velocity/attitude extrapolation (first a constant-turn-rate model on the camera's last 250 ms, later C5's Kalman), `p` decaying with delay | exact: the film's schedule along the plan, `p = 1000` |
| Speculation | the speed-sized quota | 0 (nothing speculative: the schedule is the need) |
| Recency | ×250 | ×0 |
| NextUse | ×3000 | ×4000 |
| Deficit (near field) | ×2000 | — (Batch) |
| ScreenSize | ×2000 | ×1000 (ties) |
| Margin, min dwell | 15 %, 0.5 s (a fast camera changes its need faster) | 0, 0 |
| Parameters exposed | `k_h` (s of look-ahead per m/s, default 0.04), `h_min` 0.5 s, `h_max` 4 s, `k_s`, `near_level` offset, the weights | the same, from the plan |

`Term::ScreenSize` is the one term FPV adds to the closed set of §3.1; it reads the place's
screen weight (`w_i`), so every specification may use it.

In the viewer `fpv` is the specification of the FPV camera mode (§3.3: the context selects
it); the diagnostic override (`tuile://budget?spec=fpv`) forces it on ordinary navigation to
compare against `interactive` on the same flight. In a film, `fpv-film` is the specification
the film host passes with the schedule.

## 4. Measured inputs (recorded)

### 4.1 What the host measures

```rust
pub struct Measured {
    /// GPU bytes the device reports allocated (all of the process), and its limit.
    pub gpu_allocated: Bytes,
    pub gpu_limit: Bytes,
    /// System RAM available to the process.
    pub ram_available: Bytes,
    /// Per tier: the latest samples of access cost.
    pub store: TierSample,       // local store (foyer memory/disk); native only
    pub http_cache: TierSample,  // browser HTTP cache (Resource Timing, transferSize == 0)
    pub network: TierSample,     // upstream
}
pub struct TierSample {
    pub latency_us: u32,         // median of the interval's fetches, µs (0: no sample)
    pub bytes_per_ms: u32,       // throughput over the interval
    pub failures_permille: u16,  // failed / attempted over the interval
    pub samples: u16,            // how many fetches the interval saw
}
```

| Input | Native source | Web source | Unit, sampling |
|---|---|---|---|
| `gpu_allocated`, `gpu_limit` | wgpu allocator report / `VK_EXT_memory_budget` / Metal `currentAllocatedSize`, `recommendedMaxWorkingSetSize` | the budget's own ledger + `adapter.limits` (no allocator report in browsers) | bytes, once a second |
| `ram_available` | `os_proc_available_memory` (iOS), `sysinfo` available (desktop), Android `MemoryInfo.availMem` | `navigator.deviceMemory` × fraction (coarse) | bytes, once a second |
| `store` | `tuile_tiles_fetch_seconds{source=cache}` per interval | — | µs, B/ms, ‰; per second |
| `http_cache` | — | Resource Timing, `transferSize == 0` | same |
| `network` | `tuile_tiles_fetch_seconds{source=upstream}` | Resource Timing, `transferSize > 0` | same |

The host samples once per **measurement interval** (1 s) and passes the latest sample with
every step (the same value until the next interval). Nothing is measured inside the budget.

### 4.2 Smoothing, in integers, inside the decider

The raw samples are recorded; the smoothing runs **inside** `decide`, deterministic over the
recorded samples, so replay is exact (law 5). Each input has a fixed-point exponential
moving average with a time constant, the same filter `observe` uses (Q16.16, integer
`α = dt / (τ + dt)`, applied when the sample's interval changes, not per step):

| Input | τ | Why |
|---|---|---|
| GPU allocated / limit, RAM | 4 s | memory moves with uploads; a second's noise must not move the target |
| Tier latency, throughput | 8 s | fetch times are heavy-tailed; the restore cost changes the order of many resources |
| Failures | 2 s up, 16 s down (asymmetric) | a network that drops is believed at once, one that recovers slowly |

The **restore cost per tier** (`RestoreCosts`, in µs per reference resource) is then:

```
cost_us[Decoded] = upload estimate                     (constant: bytes / upload rate)
cost_us[Stored]  = ema(store.latency) + ref_bytes / ema(store.throughput)
cost_us[HttpCache] = same over http_cache
cost_us[Remote]  = (ema(network.latency) + ref_bytes / ema(network.throughput))
                   × 1000 / (1000 − ema(failures))      (a failed fetch costs a retry)
```

Until a tier has samples its cost is the specification's default (`Params::restore`:
upload 2 ms, store 6 ms, HTTP cache 15 ms, network 150 ms — the order of the O-B
measurements, to be replaced by them). A new `Tier::HttpCache` sits between `Stored` and
`Remote` (plan "Local store vs browser cache"), fed on the web by a presence hint for tiles
fetched recently (a bounded LRU of ids in the web loader, no I/O).

The GPU and RAM inputs set the **limits the controller works under** (§6.2): the effective
hard limit becomes `min(configured hard limit, measured gpu_limit − (gpu_allocated − own
ledger))` — the device's room left by everything that is not the budget's — and the decoded
tier's target follows `ram_available`. This is `Budget::retarget` made continuous and
recorded, instead of a host call at platform events only.

## 5. Predictions (recorded)

### 5.1 What a prediction is

```rust
pub struct Prediction {
    pub tile: TileId,          // listed in the need (ring/ancestors carry its Need data)
    pub next_use_in: Micros,   // expected delay to its next use
    pub probability: u16,      // ‰
}
```

A prediction says **when and how likely**; the need says **what** (level, parent, class,
imagery). A predicted tile that is not held must be in the need's `ring` (or `ancestors`) to
be loadable — the traversal lists the tiles along the predicted path there, as it does for
the ring today. A prediction naming a tile not in the need only raises the value of what is
held (eviction side).

### 5.2 Who produces them, and how they enter

| Source | Producer | Exactness |
|---|---|---|
| Film | the film's schedule: frame → tiles, from the known trajectory and the convergence's per-frame selection (`tuile-film`); each resource's next frame | exact, `p = 1000`, `next_use_in` = (frame − current) × frame time |
| Known path | the viewer's Glide flight (`app/flight.rs`, a pure function of time) and the tape replay (`app/tape.rs`): the views of the next `horizon`, through the runtime's `ClientMessage::Ahead` | near-exact; `p` decays from 1000 at `t` to 1000 × (1 − interrupt hazard) along the path |
| Interactive | gesture inertia (`tuile-camera`'s eased target and, with C2, a fling model), later motion extrapolation (C5, an established Kalman crate) | probabilistic, short horizon (1 s) |

The runtime (`tuile-core`) traverses the predicted views (it does today every
`AHEAD_EVERY = 4` steps), merges their tiles into the ring, and emits the predictions with
the step. In `decide`, predictions are an input like the need: recorded, replayed.

The **load side** reads the same value: a want's urgency band (Floor, Hole, Blocker, Drape
= present need; Ring, Ancestor, **Predicted** = anticipation) orders first (law 4), then the
value orders within the band. The `Predicted` band sits **below Ancestor** (roadmap C), is
never protected, and is under the speculation quota (law 8).

### 5.3 The deficit term on the load side (what fixes #251)

For a drawn place wearing level `worn` of a content whose need is `n ≠ worn`, the want of
the need's own resource has value

```
v = w_i × |n − worn| × dwell(t)           dwell(t) = min(t − t_n, horizon)
```

where `t_n` is when the place's need came to be `n` (kept per place and layer beside
`imagery_prev`). The want is **made** once `v ≥ cost_us[tier(own)] × (1000 + margin)/1000` —
the same rule both ways (law 7): worth fetching once the quality it brings, over the time it
is expected to serve, pays for its restore cost.

- In motion the need moves a level at a time and `t − t_n` stays short: a place approached
  level by level is not fetched at every level (the reason for today's ±1 tolerance).
- At rest `t − t_n` grows continuously: every place's own level is wanted within
  `T = cost × (1 + margin) / (w × |n − worn|)` — bounded (law 9). With the defaults (network
  150 ms, margin 25 %, weight ≥ 500 ‰, one level), `T ≤ 375 ms` after the need settled; from
  the store, 15 ms.
- No regime, no stillness detector, no cursor: the crossing time is computed when the want is
  read and kept as a **re-read deadline** (`downstream`'s `rewant` heap, one live entry per
  place), so the incremental passes re-read exactly the places whose crossing came, and the
  full recomputation (`check_wants`, debug builds) at the same `now` agrees. Until a tier is
  measured (§4.2), the cost is the network's — the crossing time is an upper bound.

### 5.4 Films: the schedule is a file, and time is frames (Laurent, 2026-11-20)

**A film predicts nothing.** The list of tiles each frame needs is produced upstream by the
bake and the convergence and **is available as a file**: the **tuile pack**
(`crates/tuile-pack`, FlatBuffers, schema `crates/tuile-pack/schema/tuile_pack.fbs`, magic
`TUILEPK\0`, version 3), written as `Content::References` ("the plan alone"):

| Pack item | Fields | Decider input |
|---|---|---|
| `Frame` (fbs:141) | `frame: u32`, `tiles: [u32]` (indices into `Pack.tiles`), the `View` | the frame index is the step; `tiles` is the frame's exact need |
| `Tile` | `id: u64`, `drape: u64`, `TerrainRef{x, y, level, digest}` (possibly an ancestor), `imagery: [ImageryRef{x, y, level, …}]` bottom layer first | `ResourceId::Mesh(TileId)` and `ResourceId::Drape(TileId, layer, level)` per imagery reference |
| `Pack` | `first_frame`, `last_frame`, `terrain_layer`, `imagery_layer`, `night_layer` | the layers, once per film |

Reader: `Pack::open`, `Pack::frame(n)` (`crates/tuile-pack/src/lib.rs:1152`), `refs_of(tile)`;
the film's `tuile_film::Cursor` walks it frame by frame into `FrameDiff { enter, leave }`.
**Today the film path does not feed the budget at all**: `tuile-film-native`'s render loop
uploads from `enter` and drops on `leave`, and fetches every referenced tile once, ahead
(`render.rs`, `serve.rs`); `ClientMessage::Ahead` / `release_beyond` / `Policy::Batch` are
used by `tuile-bake`'s session only. The film's memory is therefore whatever the pack's
window needs — unbounded by any budget.

The decider reads the pack **as an injected input** — a `KnownSchedule` source (dependency
injection; the budget does not open files):

```rust
/// The exact future of a film: for each resource, the frames it is used at.
pub trait KnownSchedule {
    /// The next frame at or after `frame` that uses `r`, if any.
    fn next_use(&self, r: ResourceId, frame: u64) -> Option<u64>;
    /// The resources frame `frame` needs (its exact need).
    fn frame(&self, frame: u64) -> &[ResourceId];
}
```

built by the film host from the pack (one pass: a next-use index per resource, the pack's
frames in order). `fpv-film` uses the same file. The film specification orders by **next
use** (Bélády; Cao et al.'s known-future rules), the restore cost per tier breaking ties
(weighted caching); loads follow the schedule's order (first missing, Cao's *First
Opportunity*); a release never takes a resource needed before the one loaded (*Do No Harm*).
Interactive modes keep the probabilistic predictions of §5.2.

**Time is an abstract axis.** The core's clock is a monotonic **step index**: the
interactive host maps it to wall-clock microseconds (measured, recorded, as `FrameInput::now`
today), a film to **frame numbers**. In the film path there are **no seconds**: the horizon
is N frames, a next use is a frame index, urgency is "frames until needed" (minus the load
cost expressed in frames of work, or simply the next-use order), and a step is one rendered
frame, whatever its wall-clock length (the film renders offline). The specification's
horizon, dwell and blend are given in the axis' unit (`Params` gains `axis: Axis::{Wall,
Frames}`; a film specification's numbers are frames).

**Catalogue scenario** `film-pack`: a film driven by a pack file (the fixture pack of
`tuile-pack`'s tests, and a pack baked from `long-pan`), on a bounded budget. Acceptance:
**no load later than its frame's need** (every frame ready when rendered), **no eviction of
a resource needed before the one loaded** (Do No Harm), reloads within the offline bound
(LP / 4-approximation, §10b), and **the same decisions whatever the simulated render
speed** (the frame clock run at 1×, 0.1× and 10× wall time gives identical digests).

## 6. The control model: an equilibrium regime

The budget is a **dynamical system** that must settle into a steady state working at the
best quality the limits allow, stay there under steady inputs, and return smoothly after a
disturbance (zoom, pressure, network drop). §6 writes the model explicitly; §9.2 tests it.

### 6.1 State, inputs, outputs

| | Symbol | What |
|---|---|---|
| State | `H` | headroom: effective hard limit − held bytes, per pool/class |
| | `F` | flows: starts, uploads (bytes), releases per second (token buckets' fill) |
| | `D` | quality deficit `D(k)` (§1) |
| | `s` | LOD scale (`sse_scale`), thousandths |
| | `x_ctrl` | the controller's integrator, per pool |
| Inputs | `u` | need (and its deltas), predictions, measured (§4), pressure, thermal, specification |
| Outputs | | starts, cancels, deliveries, upload plan, evictions, releases, draw set, `s` |

### 6.2 Set-points

- **Headroom band** `[H_lo, H_hi]` below the effective hard limit: `H* = headroom ×
  effective target` (default 10 %, band ±2 %), **plus the predicted inflow** — the bytes of
  the predicted, not-held resources within the horizon, capped by the speculation share. The
  band is the room kept free so that **a load never waits for an emergency eviction**
  (Addition 2: no last-minute eviction).
- **Flow set-points**: the measured throughput of each tier (§4.2) bounds the start rate the
  network can serve; the upload bytes per frame (`FrameDeadline`) bounds uploads; releases
  follow §6.3.
- **Quality set-point**: `D* = 0` at scale 1. `s` rises only when the headroom cannot be held
  with `D` at 0.

### 6.3 Shaped flows: no peaks

Every rate is a **token bucket in integers**, refilled with time (never with frames), capped
at one burst:

| Flow | Refill | Burst | Bypass |
|---|---|---|---|
| Starts (network) | `min(max_in_flight, measured throughput × horizon / ref_bytes)` per second | 1/4 s of refill | none: the present need waits in the queue, in order |
| Starts (speculative) | `speculation.slots` of the above | 1/8 s | none |
| Uploads | `FrameDeadline` (bytes per frame, existing) | one frame | the first item of a frame (existing) |
| Releases (proactive, §6.4) | `release_rate` = controller output, bytes per second | 1/4 s | **the hard limit** (law 3 is absolute: a step at the hard limit releases what it must) |

A release that the bucket refuses waits for the next step; since releases are proactive
(started at `H < H_lo`, long before the hard limit), the bypass is the exception, counted
(`budget_releases_total{reason=hard}`), and its rate is a congestion metric with a target of
0 in steady state.

### 6.4 The controller

Derived from Lyapunov drift-plus-penalty (Neely; BOLA as the worked transposition, §10b):
the headroom and each tier's flow are virtual queues, the deficit is the penalty, one knob
`V` trades quality against headroom. In its linearised form it is one damped controller per pool (and per class outside it), in integers, evaluated every step
with the step's `dt` (time-based, so 30 Hz and 120 Hz settle alike):

```
e      = H* − H                                   (bytes: > 0 when too little headroom)
x_ctrl = clamp(x_ctrl + e·dt, −X, X)              (integrator, anti-windup)
r      = clamp(Kp·e + Ki·x_ctrl, 0, R_max)        (release rate, bytes/s)  → release bucket
p      = max(0, e − E_s)                          (what releases cannot absorb)
ds/dt  = Ks·p − Kd·(s − 1)·[p == 0]               (LOD scale: rises with p, decays to 1)
```

- Releases take the **lowest-valued** held evictable resources (law 6), out of the
  persistent index, within the release bucket; loads take the highest-valued wants, within
  the start buckets and the headroom.
- The LOD scale (plan B6) is **folded into the controller**: it is the actuator of last resort
  when releases cannot hold the headroom — the protected set alone over the target, or the
  release bucket saturated. This replaces `relax()`'s two fixed rates, its ×4 at the hard
  limit and #251's ×16 haste (§8): rise and decay are one continuous function of the
  headroom error, the same in both directions.
- Gains default to a **critically damped** response with time constant `τ_H = 1 s`
  (`Kp = 2/τ_H`, `Ki = 1/τ_H²` in normalised units), chosen so the step response has no
  overshoot beyond the band; tuned by the simulator (§9.3).

### 6.5 Stability, made testable

| Property | Bound (desktop / handheld) | Measured on |
|---|---|---|
| Settling time of `D` after the camera stops | ≤ 2 s / 4 s to `D = 0` (law 9, §9.1) | every catalogue scenario that ends at rest (§9.3) |
| Settling time of `H` into its band after a disturbance | ≤ 3 τ_H | zoom, pressure step, network drop |
| Overshoot of `H` below `H_lo` | ≤ 10 % of the band, never below 0 (law 3) | same |
| Overshoot of `s` after a pressure lifts | none: `s` decays monotonically to 1 | pressure ramp |
| No limit cycle | under steady inputs the state sequence `(H, F, D, s, x_ctrl)` becomes constant within 10 s, and the decision digest with it | still, orbit at constant rate (periodic need: the state becomes periodic with the need's period, no sub-harmonic) |
| No oscillation per place | no place changes level twice in 10 s (I4) | every scenario |
| Bounded congestion | queue depth ≤ `request_heap`, wait p95 ≤ 2 × the tier's latency, upload deferrals ≤ 5 % of frames in steady state | every scenario |

## 7. Telemetry and the reason view

### 7.1 Metrics (`tuile_budget_*`, OpenTelemetry through tuile-metrics → tuile-telemetry)

Everything the decider does is trackable (Laurent, 2026-11-20). Every series below is a
field of `BudgetSignals`, exported by `BudgetSignals::series` (the one list both the
Prometheus endpoint and the OTLP push read), the same natively and on the web (the worker
publishes its signals to the page, which pushes them). **Cardinality is bounded by
construction**: every label is a closed set listed in `observe::labels` and in
`tuile_telemetry::resource::ALLOWED_LABELS`; no tile, place, URL or path ever is a label.
Histograms have fixed buckets. Units are in the name (`_seconds`, `_bytes`, `_total`).

| Name | Type | Labels | Unit | What |
|---|---|---|---|---|
| **Quality** | | | | |
| `under_need_tiles` | gauge | `kind` = `mesh`\|`imagery` | tiles | drawn places under their need (#251's count) |
| `over_need_tiles` | gauge | `kind` | tiles | over their need |
| `deficit_levels` | gauge | `kind` | levels | `D(k)` unweighted |
| `deficit_weighted` | gauge | `kind` | level·‰ | `D(k)` screen-weighted |
| `deficit_level_seconds_total` | counter | `kind` | level·s | the objective's integral |
| `deficit_by_level` | gauge | `kind`, `level` | levels | `D(k)` by the need's level (0–24) |
| `under_need_still_seconds` | gauge | | s | views still and some place under its need, for how long |
| **Decisions and their score** | | | | |
| `decisions_total` | counter | `list` = `start`\|`evict`\|`release`, `reason`, `class` | decisions | per decision reason (§7.2) |
| `decision_value` | histogram | `list`, `class`, `reason` | currency per byte (px·lvl·step / B; buckets doubling) | the value at decision time: what was loaded / released was worth this |
| `decision_leaf_total` | counter | `list`, `term` = `recency`\|`restore`\|`next_use`\|`deficit`\|`screen` | decisions | the specification's term that dominated the value (provenance, §10b) |
| `mechanism_corrections_total` | counter | `law` = `1`…`5` | decisions | a proposal the mechanism refused (absolute laws) |
| **Prefetch outcome** | | | | |
| `prefetch_started_total` | counter | `class`, `band` = `ring`\|`ancestor`\|`predicted` | loads | speculative starts |
| `prefetch_used_total` | counter | `class` | loads | drawn before released |
| `prefetch_wasted_bytes_total` | counter | `class` | bytes | released unused (waste) |
| `prefetch_pollution_total` | counter | `class` | evictions | a resource released for a speculative load and needed again before it (Do No Harm broken: must stay 0) |
| **Loads** | | | | |
| `starts_total` (exists) | counter | `class`, `source` = `local`\|`network` | loads | |
| `loads_by_tier_total` | counter | `class`, `tier` = `decoded`\|`stored`\|`http_cache`\|`remote` | loads | by the tier they came from |
| `loads_by_layer_total` | counter | `layer` = `0`…`7` (the session's imagery layer slots) | loads | drape loads per layer — the label scheme shared with the layer-substitution work (one `layer` slot index, never a layer's name or URL) |
| **Headroom, flows, congestion** | | | | |
| `headroom_bytes`, `headroom_target_bytes` | gauge | `class` | bytes | `H`, `H*` |
| `release_rate_bytes_per_second` | gauge | | B/s | the controller's `r` |
| `evictions_total` (exists) | counter | `class`, `reason` | evictions | reasons extended: `headroom`\|`pressure`\|`room`\|`hard`\|`respecified`\|`replaced` |
| `bucket_refused_total` | counter | `flow` = `start`\|`speculative`\|`upload`\|`release` | tokens | congestion |
| `queue_depth` | gauge | `class` | wants | ranked, not started |
| `in_flight` (exists) | gauge | `class`, `source` | loads | |
| `wait_seconds` | histogram | `class` | s | want → start |
| `deadline_deferrals_total` (exists) | counter | | uploads | upload deferrals |
| `measured_cost_seconds` | gauge | `tier` | s | the smoothed restore cost per tier (§4.2) |
| **Specification** | | | | |
| `spec_info` | gauge (= 1) | `spec` = `interactive`\|`known_path`\|`film`\|`fpv`\|`fpv_film`\|`custom`, `source` = `context`\|`override` | | the specification in force, and whether a diagnostic override set it |
| `spec_changes_total` | counter | `spec`, `source` | changes | re-specifications |
| `spec_blend` | gauge | | 0–1 | a re-specification's progress |
| **Regulator** | | | | |
| `sse_scale` (exists) | gauge | | ratio | the LOD actuator |
| `controller_error_bytes`, `controller_integral` | gauge | `class` | bytes, B·s | the controller's state (§6.4) |
| **Cost of deciding** | | | | |
| `decide_seconds`, `decide_work` (exist) | histogram, gauge | | s, units | |

The labels `kind`, `list`, `term`, `law`, `band`, `layer`, `flow`, `spec` are added to the
allow-list with this module (`tier`, `class`, `reason`, `level`, `source` are already on it).
An export test drives a budget through a fake OTLP receiver (`tuile-telemetry`'s test
collector) and asserts every name above arrives, with only allow-listed labels.

The first PR that wires the viewer (PR 5) exports the quality, specification and existing
series; the score histograms, prefetch outcome and flow series come with the PRs that
create what they measure (4, 7, 8), each with its export test.

### 7.2 Decision reasons (law 13)

`Decisions` gains a parallel `reasons` list per decision list (or a reason in `Start`), a
closed enum:

| Reason | Lists | Rule |
|---|---|---|
| `Floor` | start | law 2, the floor |
| `PresentNeed` | start | a hole, blocker or drape the picture needs now (law 4) |
| `Deficit` | start | §5.3: the need's own level now worth its cost |
| `Predicted` | start | a prediction's value (law 8) |
| `Ring`, `Ancestor` | start | anticipation bands |
| `Refresh` | start | an epoch change |
| `Headroom` | evict, release | the controller's proactive release (§6.4), lowest value |
| `Room` | evict | made room for a higher-valued load (law 3 / 7) |
| `Hard` | evict, release | the hard limit's bypass (§6.3) |
| `Pressure` | evict, release | a pressure level's target |
| `Respecified` | evict | released during a blend because its value fell (law 12) |
| `Decoded` | release | the decoded tier over its target |

`EvictReason` (today `Sweep`, `Pressure`, `Admission`) is replaced by it.

### 7.3 In the viewer

- A **HUD line** (desktop and web, same code): `spec interactive · D 0 (0 under) · headroom
  11 % · rel 4 MB/s · why: deficit 12 room 0 headroom 3`.
- The `D` diagnostic cycle of #252 gains a **reason** view: every drawn tile coloured by the
  reason of its last load (deficit, present need, predicted, floor), and tiles under their
  need edged red.

## 8. What it replaces in `tuile-budget` (no old path kept)

| Today | Replaced by | Removed |
|---|---|---|
| `state::EvictKey = (last_needed, Reverse(level), Reverse(distance), id)` (LRU, deepest first) | `(band, value, Reverse(level), Reverse(distance), id)`; the persistent index is kept, its key changes | the LRU key |
| `evict` / `evict_pooled`'s sweep from the high to the low watermark | the controller's proactive release (§6.4) within the release bucket, down the same index; the watermarks become the headroom band | `Watermarks` (its numbers move into `Headroom`) |
| `make_room`'s urgency comparison | value comparison with the margin (law 7), urgency bands kept first (law 4) | |
| `tile_wants`' ±1 tolerance and the pressure rule for a drape worn a level off | §5.3's deficit term and its deadline | the tolerance, `reads_pressure`'s special case |
| `drape_levels`' stand-in (after `fix/budget-standin-under-relaxation`: only under a relaxation) | kept as the PR leaves it; under a relaxation the stand-in is the relaxed need, measured by `D` | |
| `relax()`: `up_per_second`, `down_per_second`, ×4 at the hard limit | the controller's LOD actuator (§6.4) | `Relaxation`'s two rates (its `max_scale` stays as the actuator's bound) |
| `release_decoded`'s fixed order | the same value order over decoded copies | |
| `release_beyond` (Batch) in LRU order | the film specification's next-use value (Bélády × restore cost) | |
| `Urgency::Ring`, `Ancestor` unquoted | the speculation quota; `Urgency::Predicted` added below Ancestor | |
| `EvictReason` | `Reason` (§7.2) | |
| `Budget::retarget` called by the host on platform events | still there; the measured inputs (§4) refine it each interval | |

Goldens (`goldens.txt`, `pattern_goldens.txt`), the allocation ratchets and the recorded
reference (`tests/reference/recorded.txt`) **move** with this change of behaviour. Each PR that
moves them re-records them deliberately, with the reason in the PR and the before/after
M-sim table. The film golden is expected to stay (Batch with no prediction: the film
specification's order reduces to restore cost then the old tie-breaks — to verify, and if
it moves, said so).

## 8b. Module map (Laurent, 2026-11-20: no god-module, no god-crate)

Split by responsibility; each module has a small public surface, its own tests, and states
the law it serves. Dependencies point one way (no cycles). A separate crate only where the
boundary is real.

| Module (crate `tuile-budget` unless said) | Responsibility | Laws | Depends on |
|---|---|---|---|
| `spec` | the Specification: AST of predicates and value terms, composition, validation, digest, query form; the named specifications | 11, 12 | `types`, `tier`, `need` |
| `inputs` | measured inputs (`Measured`, `TierSample`) and their integer filters (`Smoothed`); predictions (`Prediction`) — injected by the host, never read from a global | 5 | `types` |
| `value` | the one currency: a resource's value from the specification's tabulated terms, the restore-cost credit, the deficit term and its due time (`Valuation`, `deficit_due`) | 6, 7, 9 | `spec`, `inputs`, `types`, `tier` |
| `state` (exists) | the arena and the ledger; the persistent held-side index keyed by `value` | 3, 5 | `value` |
| `exchange` (PR 4b) | the two ordered structures (missing by value per byte, held by value per byte), the bounded exchange loop with the margin, sampled held candidates | 6, 7 | `value`, `state` |
| `mechanism` (PR 4b, from today's protection code in `engine`/`downstream`) | the absolute laws: protected sets, coverage first, hard limits incl. the transient peak, the safe fallback; validates every proposal, counts corrections | 1–4 | `state` |
| `regulator` (PR 7) | headroom set-points, token buckets, the damped controller and the LOD actuator (replaces `relax`) | 9, 10 | `inputs`, `state` |
| `observe` (exists) + `provenance` (PR 4b) | signals, series, closed label sets; the reason and the dominating term of each decision | 13 | everything above, read-only |
| `engine`, `downstream` (exist) | the step's order of passes, calling the modules above; to shrink as `exchange` and `mechanism` take their parts | — | all |
| crate `tuile-budget-sim` (PR 6) | the simulator, the scenario catalogue, the exact-TIP reference, the objective | — | `tuile-budget` |
| crate `tuile-budget-tune` binary in `tuile-budget-sim` (PR 6) | the parameter search, Parquet out | — | `tuile-budget-sim` |

Policy proposes, mechanism validates (Simplex): `value`/`exchange` propose loads and
releases; `mechanism` accepts or corrects them; a specification never reaches `mechanism`.
Dependency injection: the host gives measured inputs, predictions and the presence oracle
with the step; no module reads a global or reaches into another's state.

Today `engine.rs` (≈3 000 lines) and `downstream.rs` (≈3 500) predate this map; each PR of
the stack moves out what it touches rather than adding to them (PR 4 adds `value`).

**Every PR that changes runtime behaviour builds and runs in the viewer, desktop and web**,
and says in its description how to observe the change live (what to look at, which series).

## 9. Acceptance

### 9.1 Quality at rest (#251's measurement)

- **Sim scenario `rest-after-approach`** (new, in `budget_sim/patterns.rs`): a camera that
  approaches the ground level by level for 8 s (the need of each place moves a level at a
  time, the pattern #251 observed) then stops; network latency from the `p2-slow` profile.
  **Pass: `under_need_tiles == 0` (mesh and imagery) within 2 s of the stop (desktop) /
  4 s (handheld), and it stays 0.** Red on `main` (tiles stay a level under).
- The same criterion is part of every scenario of the catalogue (§9.3) that ends at rest.
  No recorded session is a test input; #251's figures (42 / 31 of 1 843 before) are cited
  for context only.
- Existing I3 (convergence), I4 (no oscillation), I10 (imagery matches need, tightened from
  ±1 to exact at rest) green.

### 9.2 Equilibrium (§6.5)

Sim scenarios, each asserting the bounds of §6.5 on the signals: `still`, `orbit-steady`,
`zoom-then-rest`, `pressure-ramp` (Normal → Critical → Normal over 20 s), `network-drop`
(latency ×10 and 30 % failures for 10 s), `memory-squeeze` (measured `gpu_limit` halved for
10 s), `flaky-network` (seeded failures 5 %).

### 9.3 The scenario catalogue, the simulator and the parameter search

**The catalogue** (Laurent, 2026-11-20): a named, **versioned** set of predefined scenarios
that the simulator always runs whole, and that the optimiser scores **across** — never one
trajectory. A scenario is a scripted, deterministic camera path (code in
`tuile-budget-sim`, exportable to MCAP for the viewer's replay) × an input profile (§4,
simulated), with its acceptance bounds. Adding, removing or changing a scenario bumps the
catalogue's version (`CATALOGUE_VERSION`), recorded in every result table, so two results
compare only on the same version.

Camera paths (each also exists in its 3D Tiles variant where marked ●):

| Path | Script | Ends at rest |
|---|---|---|
| `still-low`, `still-mid`, `still-high` | still view at 300 m, 3 km, 300 km | yes |
| `slow-dive` | 400 km → 500 m over 40 s, eased | yes |
| `fast-dive` | 400 km → 500 m over 4 s | yes |
| `climb-to-orbit` | 500 m → 20 000 km over 10 s | yes |
| `yaw-sweep` | 360° yaw at 3 km in 6 s | yes |
| `tilt-to-horizon` | pitch −90° → −5° at 1 km in 5 s | yes |
| `long-pan` ● | a 60 s flight along a 40 km path at 800 m, tilted | yes |
| `city-fly-through` ● | a 30 s flight at 150 m between the towers of a 3D Tiles city tileset | yes |
| `zoom-thrash` | zoom in ×8 / out ×8, ten times in 20 s (thrash bait) | yes |
| `headset-jitter` | two eyes at 90 Hz, ±0.5° head jitter at 4 Hz, slow translation | no |
| `known-path` | the `long-pan` path given as an exact schedule (film-like specification) | yes |
| `fpv-valley` | FPV flight at 40–80 m above a valley floor, 35 m/s, banking turns of 30° at 0.5 rad/s, 45 s; interactive (scripted stick inputs, predictions from extrapolation, `fpv` spec) | yes |
| `fpv-ridge` | FPV along a ridge line at 20 m, 45 m/s, yaw reversals every 4 s, a dive off the ridge at 30 s; interactive | yes |
| `fpv-valley-film`, `fpv-ridge-film` | the same paths as films: the plan's exact schedule, `fpv-film` spec | yes |

Input profiles, crossed with every path (the full cross product is the catalogue; the CI
job runs it in release):

| Axis | Values |
|---|---|
| Device budget | `desktop` (1 GiB GPU, 8 GiB RAM), `phone` (384 MiB GPU, 1.5 GiB RAM) |
| Memory squeeze | none; `gpu-squeeze` (measured limit halved from 30 % to 60 % of the run); `ram-squeeze` (available RAM to 25 % for the same window) |
| Network | `fast` (20 ms, 50 MB/s), `slow` (`p2-slow`: 150 ms, 2 MB/s), `flaky` (`slow` + 10 % failures + a 5 s drop at mid-run) |
| Disk cache | `cold` (nothing stored), `warm` (the path's tiles stored, store at 4 ms) |

Acceptance bounds per scenario (asserted, and reported as columns):

| Quantity | Bound |
|---|---|
| holes (`lost`, `bare` once the floor is in) | 0, every step (law 1) |
| hard limit | never crossed (law 3) |
| tiles under their need at rest (#251's quantity) | 0 within 2 s (desktop) / 4 s (phone) of the end of motion, for paths that end at rest, `fast`/`slow` networks; within 4 s / 8 s under `flaky` |
| settling of `H` after a disturbance | ≤ 3 τ_H, overshoot ≤ 10 % of the band (§6.5) |
| congestion | queue depth ≤ `request_heap`; wait p95 ≤ 2 × the tier's latency; upload deferrals ≤ 5 % of frames outside the disturbance windows |
| thrash | `zoom-thrash`: network refetches of evicted resources ≤ a recorded ratchet |
| speculation | `known-path`, `fpv-*-film`: `wasted_starts == 0`; interactive: speculative bytes ≤ the share |
| FPV forward view | `fpv-*`: the deficit **in the forward view** (places within the forward 60° cone and the first second of the corridor) ≤ 1 level on average over the flight and 0 holes; `fpv-*-film`: 0 under need on every frame (the film waits for complete frames, law 8); `fpv` beats `interactive` on the forward-view deficit integral on both paths (the reason FPV exists) |

The existing `budget_sim` scenarios and patterns (P1–P6, goldens) stay; the catalogue is
added beside them and reuses their world, network and GPU models.

- **Simulate**: run the decider headless over each scenario, with the **simulated measured
  inputs** of its profile fed as recorded inputs (§4), and compute the objective `J` (§1):
  time-integrated screen-weighted deficit, congestion penalties, holes = infinite.
- **Parameter search** over a specification's parameters (margin, horizon, weights, headroom
  band, gains, speculation): deterministic and reproducible (fixed seeds, the sim's
  determinism), by coordinate search over a grid, then refined (an established optimiser
  crate — `argmin`'s Nelder–Mead — if the grid is not enough). Objective: the mean of `J`
  over the **whole catalogue**, each scenario normalised by the default specification's `J`
  on it (so no scenario dominates by scale); constraints: every acceptance bound holds on
  every scenario (a violation is `J = ∞`).
- **Outputs**: Parquet tables (`runs.parquet`: one row per (scenario, parameter set) with `J`,
  `D` integral, settling times, overshoot, congestion, thrash; `steps.parquet`: per step
  signals for the best and the default) — never CSV — and a short markdown report.
- Where: a new **library crate `tuile-budget-sim`** (`publish = false`, native only; not a
  dependency of `tuile-budget`) that the `budget_sim` test binary moves into (world, network,
  GPU, server, scenarios), and a binary `tuile-budget-tune` (in that crate, `parquet` and
  `arrow` dependencies there only). The `budget_sim` tests become thin callers of the library.

### 9.4 Measurements per PR

Each implementation PR carries the plan's per-PR table where it applies: **M-sim** (`budget_sim`
release, P1–P6 × profiles, plus the new scenarios), **M-bench** (`budget_decide` mean/p99,
still and in motion), **allocations per step** (ratchets), and the objective `J` on the
scenario set, before/after.

## 10. Plan of implementation PRs (a stack on `main`)

| PR | Content | Red first | Goldens |
|---|---|---|---|
| 1 | **This document.** | — | — |
| 2 | **The quality metric.** `D(k)`, `under_need_tiles`, `over_need_tiles` in `BudgetStats` (computed in `downstream` from the items, incrementally); `BudgetSignals` and the exporters; the objective `J` integrated by the sim; the scenario `rest-after-approach` written and shown **red on main** (kept `#[ignore]` with its reason until PR 4 turns it green — the only ignore of the stack). | the metric's unit tests (a drape a level under counts 1, an ancestor fallback counts its gap) | unchanged (stats only; the stats are not in the digest) |
| 3 | **The specification and the recorded inputs.** `spec` module (§3), `Measured` (§4.1), `Prediction` (§5.1), `FrameInput::with_spec/with_measured/with_predictions`, validation, digest, serial form (`to_query`/`from_query`), recorded in `StepRecord`; random-spec proptest strategy. No decision reads them yet. | round-trip and validation tests | byte-identical |
| 4 | **The value.** The eviction key (§8 row 1) and the deficit want (§5.3, deadlines), restore cost per tier (constants then smoothed measured), margin, `min_dwell`, reasons (§7.2). `rest-after-approach` green. Law invariants 1, 2, 6, 7 in the sim and proptest under random specs. | `rest-after-approach`; "eviction follows value order"; "a load below the margin does not displace" | re-recorded, reason: the LRU key and the ±1 tolerance are gone |
| 4b | **The load side on the same value** (§1b): `PriorityKey` reduced to the absolute laws' bands (floor and coverage, swap blockers), the value ordering the rest; the release side's sampled candidates. | "a load and a release read the same value"; Do No Harm on small traces | re-recorded, reason: the urgency tiers below coverage are gone |
| 5 | **Viewer wiring** (desktop and web, same code). The context selects the spec (ordinary navigation: `interactive`; the FPV camera mode, once it exists: `fpv`; flights and tapes: `known_path`); a **diagnostic override** in the menu and `tuile://budget?spec=…&margin=…` (`spec=auto` clears it), labelled as such; `spec_active`, deficit and reasons through tuile-telemetry (labels allow-listed); the HUD line. | steering parse tests; telemetry label test | — |
| 6 | **The simulator crate, the scenario catalogue and the search** (§9.3), with the **exact TIP reference** (§1c) and its acceptance test. `tuile-budget-sim` library (the `budget_sim` harness moved), simulated measured inputs, the objective, `tuile-budget-tune` with Parquet out. | the objective of a known run; determinism of the search | unchanged (moved, not changed) |
| 7 | **The controller** (§6): headroom set-point, proactive release, token buckets, the LOD actuator replacing `relax()`; the equilibrium scenarios of §9.2 with their bounds. | `pressure-ramp` (no 16× haste, monotone recovery), "a load never waits for an emergency eviction" | re-recorded, reason: the sweep and `relax()` are gone |
| 8 | **Speculation and predictions**: `Urgency::Predicted`, quotas, the viewer's known paths (Glide, tape) into predictions; **the film's `KnownSchedule` from the pack** (§5.4), the frame axis, and the film host (`tuile-film-native`) driving its residency through the budget. | law 4 and 8 invariants; Bélády optimum on small traces (B3); `film-pack`: no late load, Do No Harm, same decisions at any render speed | re-recorded where predictions are given |

Order note: PR 5 comes before the simulator so that Laurent can try the value rule live as
soon as it exists; the simulator (6) is what proves the equilibrium of 7 and tunes 7–8. If
Laurent prefers the simulator first, 5 and 6 swap without other change.

## 10b. Prior art: what is taken, adapted to the laws

From the literature review (`reports/Bibliographie éviction et préchargement.md` in the
working tree, 2026-11-20). Evidence levels are the review's: **V** full text read, **R**
abstract or extracts only, **C** prior knowledge not re-checked. What is R or C is a
**hypothesis to confirm on the simulator**, not a fact. The review's own caveat holds: the
pieces are published, their assembly here is not.

| Source (level) | What it gives | How it is taken here |
|---|---|---|
| Funkhouser & Séquin 1993 (V) | LOD selection as a multiple-choice knapsack, greedy by benefit/cost, solved incrementally from the last choices | §1's objective is that knapsack **plus** a tree constraint (a held tile has its coarser ancestors), a pinned floor and two budgets (resident bytes, bandwidth); a hole is a **hard constraint** kept by the mechanism, never a weight. The incremental solve is O1's dirty sets |
| van Waveren 2012, id Tech 5 virtual texturing (V) | priority = (wanted − resident level) × screen samples; evict finest first; coarsest pinned; high/low marks with a dead band | the priority is §1's screen-weighted deficit and §5.3's Deficit term; finest first is law 2's band; the dead band is the headroom band (§6.2). **Its swap order is not taken**: it unmaps the old page first; ours is new active before old released (law 1) |
| TIP, Patterson et al. 1995 (V) | one cost/benefit currency between announced prefetch, retention and the rest | the one value per resource (law 6): the same score serves both ends — load the top missing, release the bottom held. Value per byte: `v / size` orders the release side |
| GreedyDual / Landlord (Young; Cao & Irani GreedyDual-Size) (C for Cao & Irani) | credit = restore cost, rent ∝ size | `Term::RestoreCost` with `ref_bytes / size`; the tiers grouped into ≤ 4 cost classes (GPU-decoded, store, HTTP cache, network) so the weighted-paging guarantee of Bansal et al. applies |
| Cao, Felten, Karlin & Li 1995 (V) | fetch the next missing; evict the furthest next use; **Do No Harm** (never evict A to fetch B if A is used before B); **First Opportunity** | Do No Harm is an invariant of law 4/7 in the known-path and film specifications (sim check: no eviction whose next use precedes the fetched one's); First Opportunity is the start rule under the start bucket. Conservative ↔ aggressive is a **continuous** parameter (the speculation quota and the margin), not a switch |
| Kimbrel & Karlin (R) | reverse aggressive for parallel channels | a hypothesis for the film's several load channels (network and store quotas), to try in PR 8 |
| Albers, Garg & Leonardi (R); Bar-Noy et al. (C) | offline optimum by LP; with sizes it is NP-hard, 4-approximation | the film oracle in the simulator is an **LP bound or the 4-approximation, not exact Bélády** (sizes vary); the online decider is scored against it on the catalogue |
| Bansal et al. SODA 2022, Lykouris & Vassilvitskii, Rohatgi (R) | prediction-augmented weighted paging, robust fallback (≤ O(log) worse when predictions are wrong) | the interactive predictions are used under a **Marker-style guard**: nothing selected in the current phase (drawn within `min_dwell`) is released on a prediction — wrong predictions cost bounded reloads |
| Im et al. 2022 (R) | evaluate the predictor on a sample of candidates | the release side asks the value of the first k candidates of the persistent index only (k = 64), never the whole set — keeps P6.4's bound |
| TRACK, Qian et al., Gül et al. (R) | linear/Kalman extrapolation beats deep models below ~1–2 s; "still camera" is the mandatory baseline | the interactive and `fpv` predictors are a constant-turn-rate extrapolation first, Kalman (an established crate) next; every predictor is scored against "still camera" in the simulator |
| Teler & Lischinski 2001 (R); Ng et al. 2005 (V); Funkhouser 1996 (R) | expected benefit along the predicted path under a bandwidth limit; conservative frustum = union of future frusta; one manager for prefetch, LOD and replacement | the runtime's predicted views (§5.2) are the union of future frusta over the horizon; the one decider is Funkhouser's single manager |
| Flare, Duanmu (V for Flare) | two-tier 360° streaming: coarse base everywhere with a long horizon, fine on the predicted view with a short one; out-of-view margin `k = c₀ + ⌈ξ(1−S)(n−c₀)⌉` growing as recent accuracy `S` drops | coarse band with a long horizon, fine band with a short one (horizon per band in `Params`, PR 8); the corridor's margin grows **continuously** with `1 − S`, `S` the measured prediction accuracy over the last seconds (recorded) |
| 2Q / ARC idea; SARC/AMP (V for SARC) | seen-once apart from reused; prefetched data in its own list sized by marginal utility; pollution and waste counters | a resource loaded on a prediction and not used yet counts as speculative bytes (the quota of law 8); metrics `prefetch_used_total`, `prefetch_wasted_bytes_total` (prefetched then released unused) |
| Neely, drift-plus-penalty; BOLA (R for Neely's book; V for BOLA) | per-step regulator on virtual queues with a penalty; utility within O(1/V) of optimal, queues O(V) | §6.4's controller is **derived from drift-plus-penalty**: virtual queues for GPU headroom, RAM headroom and each tier's flow; the deficit is the penalty; `V` is the one knob between quality and headroom; the LOD scale (B6) is its actuator of last resort. The only formal stability guarantee available — and it bounds averages, not oscillation, so §6.5's SASO checks stay |
| Linux kswapd watermarks (V); Kalyvianaki (R); Lu & Abdelzaher (R) | background reclaim between marks, synchronous fallback; target = filtered working set + safety margin; feedback split of one pool between classes | the headroom band (§6.2) with the hard-limit bypass as the synchronous fallback; `H*` = filtered demand + margin; the pool split stays the shared pool (no fixed reservation) |
| Token bucket (V); TCP Vegas (V); BBR (V, draft); RFC 6298 (V) | hard burst bound; dead band on bytes in flight; per-source windowed max bandwidth and min latency with pacing; integer smoothing of mean and deviation | §6.3's buckets; a dead band ≥ the largest tile on in-flight bytes; per-tier windowed max throughput / min latency next to §4.2's EMA; RFC 6298's integer mean/deviation for latency (cheaper and as deterministic as the EMA — to compare on the simulator) |
| PANDA, BOLA-O (R / V) | climb slowly, drop fast — as continuous gains; cap a rise on the previous measurement | the controller's asymmetric gains (continuous, not a mode); a rise of the LOD scale capped per step |
| Evans & Fowler, Specification (V) | composable selection/validation predicates | §3's predicates, extended with **score combinators** (terms × weights) as a serialisable AST — the review finds no "specification with scores" in the DDD literature: ours is an extension |
| Hydra 1975, Simplex 2001, shielding (R / C) | policy proposes, mechanism validates, a simple safe fallback | the absolute laws live in the mechanism (protection, bands, hard limit); a specification only scores; the safe fallback is "evict the oldest finest first" (§3.5) |
| Bumpless transfer; Dragan & Srinivasa blending (C) | a switch of controller ramped by α | §3.5's linear blend of weights over `blend`, under the bounded release rate |

**Patents** (review's warning, claims not read): MPC-DASH, US 10271112 (Yin et al.'s MPC),
and map-tile prefetch US 8204966 / 8805959 / 10304238. The film's receding-horizon schedule
(§5.2, PR 8) is planned from the exact known future and **not** a reproduction of Yin et
al.'s method; before anything resembling MPC-DASH lands, the claims are read.

**Gaps the review found** (open problems, so measured, not assumed): no published result
combines prediction, prefetch under a bandwidth limit and heterogeneous costs and sizes; no
stability analysis exists for a residency/LOD controller; no primary source on eviction
bursts. §6.5's SASO criteria on the catalogue (overshoot, settling time, the spectral peak
of the eviction rate, the upload peak per frame) are therefore this project's own evidence.

## 11. Open questions, with proposed defaults

| # | Question | Proposed default |
|---|---|---|
| Q1 | Is law 7 (margin) enough, or add a per-resource maximum change rate? | **Add a minimum dwell** (`min_dwell`, default 1 s interactive, 0 film): a resource loaded is not released before it, unless the hard limit forces it (reason `Hard`). The margin prevents thrash between two resources of near-equal value; the dwell prevents it for one resource whose value oscillates with the input (a place at the edge of the view). Cheaper and easier to test than a rate per resource. |
| Q2 | A compute budget for the decider (a fraction of a frame)? | **Yes, ≤ 10 % of a frame at the profile's rate** (1.6 ms at 60 Hz, 1.1 ms at 90 Hz) at p99, measured by `budget_decide` (M-bench) and by the viewer's `decide` time metric — never enforced by a clock inside `decide` (law 5). The deterministic proxy is the work bound per step (P6.4: work grows with changes, not with the view). |
| Q3 | Who may re-specify: the host only, or also `tuile://` / the control API? | **The host, from its context** (§3.3) — that is how specifications are chosen. The viewer's steering (`tuile://budget?…`) and control API may **override** it for tests and diagnosis only, labelled so; a remote client of a `tuile-server` session may not (its budget is the session's). URLs and the control API take a named specification with numeric overrides only, validated and clamped — no free predicate trees. Absolute laws are not reachable from any of them. |
| Q4 | Measured inputs smoothed where? | **Inside `decide`**, over the recorded raw samples (§4.2), so the filter is part of the replay and its constants part of the specification. |
