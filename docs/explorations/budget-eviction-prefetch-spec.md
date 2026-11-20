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

## 2. The laws

Thirteen laws, each with the invariant that tests it. **Absolute laws** stand outside any
specification: no specification can relax them, so a bad specification costs reloads, never
a hole. **Choice laws** fix the shape of the decision; their numbers come from the
specification. **Specification laws** say what a specification is.

### 2.1 Absolute laws (outside any specification)

| # | Law | Testable invariant |
|---|---|---|
| 1 | **Never a hole.** What is drawn, the fallback chain of every needed-but-absent place, and both halves of a swap group are never evicted; a replacement is active before what it replaces is released. | Every sim step: `lost == 0` and `bare == 0` once the floor is in (I1); no `evict`/`release` entry is in the protected set P (I11); for every swap, the step that stops drawing the old drew the new (I9). Proptest over random needs, arrivals, pressures **and random specifications** (§3.6): the same three hold. |
| 2 | **Coarse first.** A coarser resource is loaded before a finer one of the same place and evicted after it; the floor (`pinned_level`) is always held. Lexicographic, never a weight. | Within a step: no `start` of a level-`l+1` resource of a place before its level-`l` ancestor is held or started (for wanted chains); no eviction of a coarse-band resource while a fine-band one of the same class is evictable; the floor is never in `evict`. Proptest: for any specification, the eviction order restricted to one class is sorted by band first. |
| 3 | **The hard limit is never crossed.** A load starts (a delivery is made) only if room exists, or can be made by releasing something worth less. | Every step, every class and the pool: `gpu ≤ hard_limit`, `decoded ≤ decoded_hard` (the existing I2/§11.2 check); a `start` whose room needs an eviction names a victim of lower value than itself (recorded reason §7.2). |
| 4 | **The present need before anticipation.** A prefetch never delays or displaces a load the picture needs now. | For every step: no speculative `start` (band Ring/Ancestor/Predicted) while a present-need want (Floor/Hole/Blocker/Drape) of the same quota is waiting and admissible; no eviction of a present-need resource to make room for a speculative one; a selection is the same with and without predictions (proptest: `draw` identical when the predictions are dropped, for the first step they differ). |
| 5 | **Reproducible.** Same inputs, same decisions. Predictions, specifications and measured inputs are recorded step inputs; replay gives identical decisions. Executor-measured numbers enter `decide` only as recorded inputs. | The decision digest (`det!`, goldens) is a function of the recorded inputs only: a step stream recorded to MCAP (a catalogue run, or a viewer session tape) replayed gives the same digest, bit for bit; nothing in `tuile-budget` reads a clock, a hash order or a float that was not quantised once at the boundary. |

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

/// A value term, in micro-seconds of "worth keeping" (one unit across terms).
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
- **Values are filed, not recomputed**: a held resource's value is computed when it is
  (re)filed in the persistent eviction index (`state::EvictIndex`, on every change of its
  copies, its last-needed time or its prediction). A resource filed under the old blend keeps
  the value it was given; GreedyDual's aging (the recency term grows with time for the
  others) retires it naturally. No step re-files the whole index because of a switch.
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
  read and set as a **deadline** (`downstream`'s deadline heap, already used for the cancel
  grace), so the incremental passes re-read exactly the places whose crossing came, and the
  full recomputation (`check_wants`) at the same `now` agrees.

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

### 7.1 Metrics (`tuile_budget_*`, through `BudgetSignals` and the existing exporters)

| Name | Type | Labels | What |
|---|---|---|---|
| `deficit_levels` | gauge | `kind` = `mesh` \| `imagery` | `D(k)`, unweighted levels |
| `deficit_weighted` | gauge | `kind` | `D(k)`, screen-weighted (‰) |
| `under_need_tiles` | gauge | `kind` | drawn places with `worn < need` (#251's count) |
| `over_need_tiles` | gauge | `kind` | `worn > need` |
| `deficit_seconds_total` | counter | `kind` | `Σ D(k)·dt`, the objective's integral |
| `rest_deficit_seconds` | histogram | | time from the views going still to `D = 0` |
| `headroom_bytes` | gauge | `class` | `H` |
| `headroom_target_bytes` | gauge | `class` | `H*` |
| `release_rate_bytes` | gauge | | the controller's `r` |
| `decisions_total` | counter | `list` = `start` \| `evict` \| `release`, `reason` | per decision reason (§7.2) |
| `spec_active` | gauge | `spec` = `interactive` \| `known_path` \| `film` \| `fpv` \| `fpv_film` \| `custom` | 1 for the one in force |
| `spec_blend` | gauge | | 0–1 progress of a re-specification |
| `prefetch_used_total`, `prefetch_wasted_bytes_total` | counter | `class` | speculative loads used before release / released unused (SARC's waste, §10b) |
| `bucket_refused_total` | counter | `flow` = `start` \| `speculative` \| `upload` \| `release` | congestion: tokens refused |
| `queue_depth`, `wait_seconds` | gauge, histogram | `class` | congestion |
| `measured_cost_seconds` | gauge | `tier` | the smoothed restore cost per tier |

The labels `kind`, `list`, `flow` and `spec` are closed sets, added to
`tuile_telemetry::resource::ALLOWED_LABELS` and to `observe::labels` (the existing
`reason`, `class`, `tier` are reused). No tile, place or path in any label.

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
| 5 | **Viewer wiring** (desktop and web, same code). The context selects the spec (ordinary navigation: `interactive`; the FPV camera mode, once it exists: `fpv`; flights and tapes: `known_path`); a **diagnostic override** in the menu and `tuile://budget?spec=…&margin=…` (`spec=auto` clears it), labelled as such; `spec_active`, deficit and reasons through tuile-telemetry (labels allow-listed); the HUD line. | steering parse tests; telemetry label test | — |
| 6 | **The simulator crate, the scenario catalogue and the search** (§9.3). `tuile-budget-sim` library (the `budget_sim` harness moved), simulated measured inputs, the objective, `tuile-budget-tune` with Parquet out. | the objective of a known run; determinism of the search | unchanged (moved, not changed) |
| 7 | **The controller** (§6): headroom set-point, proactive release, token buckets, the LOD actuator replacing `relax()`; the equilibrium scenarios of §9.2 with their bounds. | `pressure-ramp` (no 16× haste, monotone recovery), "a load never waits for an emergency eviction" | re-recorded, reason: the sweep and `relax()` are gone |
| 8 | **Speculation and predictions**: `Urgency::Predicted`, quotas, the film schedule into `release_beyond`, the viewer's known paths (Glide, tape) into predictions. | law 4 and 8 invariants; Bélády optimum on small traces (B3) | re-recorded where predictions are given |

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
