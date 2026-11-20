<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Extensible resource classes in the budget: clouds, units, procedural terrain, waves

Status: **study** (2026-11-21), no code. For Laurent's review.

The question (Laurent, 2026-11-21): the budget must be built so that **new kinds of
objects** can join it: volumetric clouds (cumulonimbus, stratus), animated units
(moving models, characters, vehicles), procedural terrain and procedural waves. What
must a kind of object declare to join, how does each of these families map onto the
budget's single currency, does the budget need a second dimension (time) beside
memory, and what does this change in the decider being designed now and in today's
code?

Inputs read: `docs/00-vision.md`, `01-architecture.md`, `10-crate-core.md`; the crate
`crates/tuile-budget` on `origin/main` (30e5ea7f): `types.rs` (`Class`, `PerClass`,
`ResourceId`), `config.rs` (`BudgetConfig`, `Pool`), `need.rs`, `cost.rs`, `tier.rs`,
`state.rs` (`Arena`, `EvictIndex`), `decisions.rs` (`digest`), `observe.rs`; the
explorations `graphics-budget-roadmap.md`, `budget-atmosphere-voxels-procedural.md`
(#162, cited "#162" below), `3d-tiles-in-the-budget.md` (#174),
`animated-instanced-3d-tiles.md`, `volumetric-clouds.md`,
`procedural-unbounded-terrain.md`; and the decider in review: PR #254
(`budget-eviction-prefetch-spec.md`, "the decider spec" below, cited by section) with
its first code in #255 and #256 (`spec.rs`: `Facts`, `Predicate`, `Term`). Nothing was
built or run.

---

## 0. Summary

1. **A class joins the budget by declaring data, not code.** The decider stays one
   pure, replayable function (decider spec, law 5). So a class is split in two:
   - a **`ClassDescriptor`**: a small integer value, validated once, recorded in the
     session header and hashed into the digest. It says what the class's resources
     cost, where they are restored from (fetched, regenerated, derived), how long they
     live, what a hole is for this class, what its floor is, and which hierarchy its
     fallback chain follows;
   - a **producer**, outside `decide`: host code (a trait in `tuile-core`, implemented
     by the class's crate or by the host) that turns the class's own LOD metric into
     the budget's common inputs (a quality excess in levels, a screen weight, a
     parent, predictions). What it outputs is a recorded step input, like the need is
     today.

   `decide` never calls class code. A new class needs no edit to `tuile-budget`.
2. **One currency, several constraints.** The value of a resource stays the decider's
   one number (decider spec law 6: microseconds of worth keeping, from the quality it
   brings, its next use, its restore cost and its size). Restoring by regeneration
   fits it as it is: a regenerated resource's restore cost is its compute time, in the
   same unit. "Keep or regenerate" is the decision the currency already makes.
   What grows is the **set of limits**. Beside GPU and decoded bytes come per-frame
   time lanes (CPU generation, GPU generation, upload) and a drawn-work estimate. Each
   limit has a **shadow price**: the controller's virtual queue for that dimension
   (decider spec §6.4, drift-plus-penalty). A resource's cost is the sum of its sizes
   in every dimension, each weighted by its price. The currency stays single, the
   knapsack becomes multi-dimensional, and the controller already computes the
   weights.
3. **The laws carry over unchanged once "hole" and "floor" are defined per class.** A
   class declares its **floor representation**: always held, bounded in bytes and in
   per-frame work. Examples: the coarse ground levels; a cloud field's 2.5D slab from
   the weather map; a unit's impostor or coarsest LOD; the ocean's coarsest cascade.
   A class also declares its **coverage kind**:
   - `Hole`: absence is a hole (ground, units in view);
   - `Continuity`: what the last frame drew may not vanish (clouds, ocean);
   - `Additive`: absence is less detail (ADD-refined content, decorative detail).

   Law 1 (never a hole) then applies to every class without a special path, and so
   does the order hard limit > no hole > coarse coverage > present need > anticipation.
4. **No fixed reservation per class.** Every class is a member of one shared pool. A
   class's floor in the pool is the measured bytes of its floor representation, not a
   percent of the device. A context weights classes only through the specification's
   value terms (a weather app weights clouds, FPV weights near terrain), and only
   **inside** the protection and band order, so no context can starve another class's
   floor.
5. **The open set is cheap.** `Class` becomes a `ClassId(u8)` index into a registry
   built by the host. The four classes of today are pre-registered at indices 0 to 3,
   so the decision digests of every existing scenario stay the same bit for bit (the
   digest hashes `class.index()` and the `ResourceId` discriminant; §5.3). `PerClass`
   becomes a fixed-capacity array (16 classes) with a length: no allocation, `Copy`,
   wasm-clean. `ResourceId` gains one generic variant, appended last.
6. **Per family:**
   - **Procedural terrain** is the smallest step: the classes it produces exist
     already, and it needs only a `Generated` tier and its restore-cost row.
   - **Clouds** need versioned resources (forecast steps) and derived resources
     (cascades derived from weather tiles).
   - **Units** need a need that moves with the unit rather than with the camera,
     aggregated per (model, LOD) and not per instance, plus shared resources held by
     reference.
   - **Waves** are mostly not cache. They are **standing** resources regenerated every
     frame, and their cost is recurring time, so they enter through the time dimension
     and the quality controller, not through eviction.
7. **What to do now:** nothing jumps the budget plan's order. Ask the decider stack
   (#254 to #256 and its PRs 4 to 8) for five small things that keep it from baking
   in four classes (§5.1). Implement the registry after decider PR 4, with byte-
   identical goldens. Implement the first real new class (procedural terrain) when
   `tuile-procedural` reaches host integration.

---

## 1. What the budget assumes today

| Assumption | Where | What it blocks |
|---|---|---|
| `Class` is an exhaustive enum of four (`TerrainMesh`, `Tiles3dMesh`, `Drape`, `Overlay`), "deliberately exhaustive: `PerClass` is a fixed array indexed by it" | `types.rs:21-64` | any class the crate does not name |
| `PerClass<T>([T; 4])` in every per-class number: targets, overflows, hard limits, in-flight quotas, the pool's members and floors, the ledger | `types.rs:74`, `config.rs`, `lib.rs` (`Ledger`), `state.rs` (`EvictIndex.filed`, `gpu_by_level`, `protected`) | the same |
| `ResourceId` is `Mesh(TileId)` or `Drape { tile, layer, level }`; its derived order is the final tie-break of every decision | `types.rs:201` | a resource that is not a tile's mesh or drape (a shared model, a cascade, a brick that is not a tile) |
| The need is tile-shaped: `NeedSet { drawn, blockers, ancestors, ring }` of `(TileId, Need)`; `Need` carries `sse_excess`, `centrality`, `distance`, `parent`, and the drape-specific `imagery` and `rect` | `need.rs:22`, `need.rs:524` | a need that comes from a simulation (units) or from a field (clouds), not from a tile traversal |
| `Cost { gpu, staging }`: bytes only | `cost.rs:33` | time costs: generation, skinning, march |
| `Tier`: `Gpu`, `Decoded`, `Stored`, `Remote`, all of them places a copy sits | `tier.rs:14` | "nowhere, but computable here" (procedural), "derivable from what is held" (cascades) |
| `Class::is_ground()`: the floor and P1.1's "coarse ground first while ground is lost" belong to the ground classes | `types.rs:51` | a per-class floor for content that is not ground |
| The draw set is a list of tile meshes, each with its drapes (`DrawItem { mesh, role, drapes, serves }`) | `decisions.rs` | drawing a resource that is not a tile mesh |
| Telemetry labels a class with a `match` (`observe::labels::class`) | `observe.rs:442` | a label for a class the crate does not name |
| Downstream code that names classes: `tuile-core` (`source`, `tileset`, `tiles3d`, `runtime`, `protocol`, `cover`), `tuile-wgpu` (`pump`), `tuile-web` (`wire`), `tuile-viewer` (`web/control`) | grep of `PerClass` / `Class::` | the migration's reach (§5.3) |

Two properties the study keeps:

- **The budget's unit is a resource, and the decider keys everything by it.** Requests,
  ledger, protection, eviction order and the digest are all per `ResourceId`. A new kind
  of object joins by having resources, not by having a new loop.
- **Everything that enters `decide` is a recorded input.** The decider spec amends #162's
  rule "measured times never enter `decide`" into law 5: measured numbers do enter, as
  fields of the step's input, recorded and replayed. This study relies on that
  amendment for the time dimension (§3).

---

## 2. What a resource class declares

### 2.1 The split: descriptor (data, inside) and producer (code, outside)

A trait object called from inside `decide` would put code there that the replay does not
record. The digest would then be a function of the host's code version, which is unsafe
on a farm, and the determinism lints (no hash iteration, no float arithmetic, no
narrowing `as`) could not be enforced on host code. So the class's **behaviour that the
decider needs is data**, and the class's **intelligence is a producer outside**.

```
                host side (tuile-core, the class's crate, or the host)            tuile-budget (pure)
 ┌────────────────────────────────────────────────────┐            ┌──────────────────────────────┐
 │ ClassProducer (trait): from the views and the      │  recorded  │ ClassRegistry: descriptors   │
 │ class's own state (weather, simulation, spectrum), │ ─────────▶ │ (data, hashed)               │
 │ emits wants in the common currency, predictions,   │   inputs   │ decide(): one value, laws,   │
 │ cost estimates                                     │            │ shared pool, limits          │
 └────────────────────────────────────────────────────┘            └──────────────┬───────────────┘
 ┌────────────────────────────────────────────────────┐   Decisions               │
 │ Restorer (executor): fetch / generate / derive,    │ ◀────────────────────────┘
 │ upload, draw; reports arrivals with actual bytes   │
 │ and, where measured, actual time                   │
 └────────────────────────────────────────────────────┘
```

This is the shape the budget already has for tiles: the traversal (producer) computes the
SSE and emits `Need`s, and the executor (`pump`, the web loader) restores and draws. The
study generalises it. It does not invent it.

### 2.2 The descriptor

A sketch, integers only, `Eq + Hash`, validated by `ClassRegistry::register`:

```rust
/// What the decider knows of a class. Data, recorded, hashed into the digest.
pub struct ClassDescriptor {
    /// Telemetry label: `[a-z][a-z0-9_]{0,23}`, unique in the registry (§4.4).
    pub label: &'static str,
    /// Capability tags a specification can select without naming the class (§4.3).
    pub tags: ClassTags,                 // bit set: GROUND | VOLUMETRIC | ANIMATED | GENERATED | SHARED …
    /// What its fallback chain follows (§2.6).
    pub hierarchy: Hierarchy,
    /// What absence means, and what is always held (§2.5).
    pub coverage: Coverage,
    pub floor: FloorRule,
    /// Where a released copy comes back from, cheapest first (§2.4).
    pub restore: RestorePaths,
    /// How the content and the need change with time (§2.3).
    pub lifetime: Lifetime,
    /// The dimensions its resources are charged in (§3), and its estimator's tolerance.
    pub charges: Dimensions,             // bit set: GPU_BYTES | DECODED_BYTES | UPLOAD | GEN_CPU | GEN_GPU | DRAWN_WORK
    pub drift_permille: u16,             // like cost::within_drift's 5 %
    /// Calibration of its quality levels against the other classes' (§2.7), ‰.
    pub quality_scale: u16,
    /// Network / local / generation quotas, as today's `max_in_flight`.
    pub quotas: ClassQuotas,
}
```

Each field is described below. None of them is a fixed share of memory (§4.2).

### 2.3 Lifetime: static, versioned, moving, standing

| `Lifetime` | Content | Need | Example | What the decider does differently |
|---|---|---|---|---|
| `Static` | a function of the resource id only | moves with the camera | terrain, imagery, 3D Tiles, procedural terrain (content = f(generator version, seed, level, x, y)) | nothing: today's model |
| `Versioned` | a function of (id, epoch); epochs follow a known clock | moves with the camera and the epoch | cloud weather tiles per forecast step; a time-dynamic 3D Tiles content | an epoch change is a `Refresh` (it exists: `gpu_epochs`, `Reason::Refresh`); interpolation between two epochs is a **temporal swap group** (both held until the blend ends) |
| `Moving` | static | moves with the **object**, not the camera | a unit's model LODs: the content is fixed, but which LOD is needed follows the unit's distance | the producer recomputes the need from the simulation every step; the need is aggregated per (model, LOD) (§3.2 of the units family, §2.8) |
| `Standing` | regenerated every frame (or every few) from a closed form or a simulation | the class's presence and its quality vector | ocean FFT cascades; cloud targets; a crowd's per-frame instance buffer | not evictable by value: held while the class is active, sized by the quality vector, resized only with the swap order (new allocation active before the old one is released); its cost is mostly recurring time (§3) |

`Standing` is how "no fixed GPU reservation" and "per-frame state" are reconciled. A
standing resource is in the pool, so its bytes count with everyone's and the split
follows the view. It is never a slice of the device reserved apart. When the class is
inactive (no ocean in view), the bytes go back to the pool.

### 2.4 Restore paths: fetched, regenerated, derived

The decider spec's value already has a restore cost per tier, in microseconds
(`Term::RestoreCost`, `RestoreCosts`). A class declares which paths it has, and the tier
list grows by two entries that are not places but **ways back**:

| Path | Tier (decider) | Restore cost, µs | Who measures it |
|---|---|---|---|
| on GPU | `Gpu` | 0 | — |
| decoded in RAM | `Decoded` | upload estimate | the upload lane |
| local store, HTTP cache | `Stored`, `HttpCache` (decider spec §4.2) | latency + bytes / throughput | the host, per tier (decider spec §4.1) |
| network | `Remote` | the same, × 1 / (1 − failures) | the host |
| **regenerated** | `Generated { lane }`, lane = `Cpu` or `Gpu` | units × measured µs per unit on that lane (65×65 mesh on CPU ≈ 0.5 to 1.5 ms est., 256² imagery on GPU < 0.2 ms est., `procedural-unbounded-terrain.md` §3.7) | the executor, per lane, recorded |
| **derived** | `Derived` | derive µs + Σ restore cost of its inputs not held (one level deep: an input is itself fetched or generated, never derived again) | the executor, recorded |

Consequences, each without a special case:

- **Keep or regenerate is the currency's own decision.** A generated tile held on the GPU
  is worth `RestoreCost(Generated) × ref_bytes / size`, plus its next use and deficit
  terms. A tile that is cheap to regenerate, large and not needed soon sinks below a
  network tile that is costly to restore, and it is released first. This is
  GreedyDual-Size's credit with compute time as the credit (#162 §4.4 said so; the
  decider spec gives it its unit).
- **Persist or not.** When CPU generation costs more than a local read (procedural
  imagery: 10 to 40 ms est. against about 1 ms), writing the generated tile to the store
  makes `Stored` the cheaper path next time. That is a new decision output, `persist`.
  The same currency decides it: persist when `(cost(Generated) − cost(Stored)) × p(reuse
  within the store's horizon) > write cost`. It is under a write quota, and it is
  rate-shaped like every flow (decider spec §6.3). Open question Q9.
- **Derived resources and their inputs.** A cloud cascade is derived from decoded weather
  tiles, and a drape mosaic is (in spirit) derived from imagery tiles. Holding a derived
  resource does not oblige the decider to hold its inputs. Its restore cost includes
  re-fetching them when they are gone, so the decider keeps the inputs exactly when they
  are worth it. The dependency is declared per resource by the producer
  (`derives_from: [ResourceId; ≤ 4]`) and bounded.
- **Shared resources by reference.** A shared model (`animated-instanced-3d-tiles.md`
  §4.3), a pose texture or a noise volume used by many referrers is protected while any
  referrer is drawn. Law 1 extends along the reference: a drawn instance tile protects
  its model, as a drawn tile protects its fallback chain today. The referrer counts are
  the producer's, given per step, bounded (B7's per-view reference counts).

### 2.5 Coverage, floor, and what a hole is

The decider spec's law 1 is "never a hole", with a hole defined for the ground (`lost`,
`bare`). Each class has to say what a hole is for it, and what is always held so that
none can happen:

| `Coverage` | What absence means | Law 1 for the class | Floor (`FloorRule`) |
|---|---|---|---|
| `Hole` | a place or an object the frame shows, drawn by nothing: indistinguishable from a fault | `lost == 0` for the class, every step, once its floor is in | the levels `0..=pinned`, or "the coarsest representation of every object in the scene" |
| `Continuity` | the previous frame showed it and this one does not: a cloud that vanishes, an ocean that goes flat | a resource drawn at `k` is drawn at `k+1` or replaced by one active at `k+1` (the swap order, I9) | the class's analytic or coarsest form: the weather-map slab for clouds, the coarsest cascade for the ocean |
| `Additive` | less detail, never a gap: ADD-refined children, decorative detail bricks, extra wave cascades | the swap order still applies to replacements; absence is not counted as a hole | none required |

`FloorRule` is one of:

- `Levels(n)`: the hierarchy's levels `0..=n` (today's `pinned_level`);
- `Coarsest`: every object's last link of its LOD chain, held while the object exists;
- `Standing`: the class's standing resources, at their minimum quality;
- `None`: `Additive` only.

The sum of every registered class's floor, in **bytes and in per-frame work**, must fit
the device's floor budget. `ClassRegistry::register` refuses a class whose floor would
not fit (proposed invariant I24, §5.2). A floor that cannot be held is a hole by
construction, so the refusal comes at registration, never at frame 17 501.

**Units: popping versus missing.** A unit that exists in the simulation and is in view
must be drawn: missing is a hole (`Hole` coverage, `Coarsest` floor = its impostor or
lowest LOD, held while the unit exists). A finer LOD that is not yet resident is
**popping**, not a hole. It costs quality (deficit levels × screen weight) and is
handled like a drape a level under its need. Its replacement is a swap group that lasts
through the renderer's cross-fade: both LODs are held for the fade's duration (a
`blend_us` per class, a recorded number), so the fade is continuous and never steps
(the continuous-rendering rule).

### 2.6 Hierarchy

The budget needs one thing from a hierarchy: **the fallback chain** (`Need.parent`), so
that a missing resource is shown by its ancestor and the coarse band loads first
(law 2). The kind of hierarchy matters only to the producer and to bands:

| `Hierarchy` | Parent of a resource | Band (law 2) from | Examples |
|---|---|---|---|
| `Quadtree` | the tile's parent | level | terrain, imagery, procedural terrain, 3D Tiles (implicit or explicit) |
| `Octree` | the brick's parent | level | voxel bricks, cumulonimbus volumes, caves |
| `LodChain` | the next coarser LOD of the same object | LOD index from the coarsest | unit models, skeletal LODs, impostors |
| `Nested` | the next coarser ring or cascade, centred on the camera | ring index | weather cascades, ocean cascades, clipmaps |
| `Flat` | none | the class's floor only | a single shared noise volume, a shared model with no LOD |

`TileId` is already an opaque `u64` (`tile_id.rs`). For tree classes the producer encodes
its own key in it, and `ResourceId::Item` (§5.3) carries a class and a key for the rest.

### 2.7 Need and screen weight: the class's metric mapped onto the currency

The decider's value reads two numbers per wanted place: the **deficit in levels**
(`need − worn`, decider spec §1) and the **screen weight** `w` (‰, to become
`Need::screen_px`, decider spec §11). Every class maps its own metric onto them:

| Class | Its own metric | Quality excess (levels) | Screen weight |
|---|---|---|---|
| terrain, imagery, 3D Tiles, procedural terrain | SSE in pixels | `log2(sse / τ)` (today's `sse_excess`) | projected area of the tile (centrality until then) |
| cloud bricks | voxel footprint against pixel footprint along the view rays | `log2(voxel_px / τ_vox)`, the brick's projected voxel size over the target | projected area × **mean opacity seen through it** (last frame's transmittance, recorded): a brick behind a cumulonimbus wall is worth little |
| unit models | screen size in pixels against the LOD's design size | `log2(px / px_lod)` for the nearest visible instance of the model | Σ over the model's visible instances of their projected area |
| ocean cascades | cascade texel footprint against the wavelength band it carries | `log2(texel_px / τ)` per cascade | the ocean's share of the screen |

**Are levels comparable between classes?** Only approximately, and that is the main
open point of the currency (Q5). One level of deficit on a building's 3D Tiles and one
level on a stratus layer do not cost the picture the same. The descriptor carries a
`quality_scale` (‰) that maps a class's level onto the common level. It is a constant
tuned in the simulator against an image metric (the image lab's methods), and a
specification can still re-weight it per context (§4.3). The scale is a calibration,
not a preference, so it lives in the descriptor. The preference lives in the
specification.

### 2.8 Prediction inputs

The decider's predictions are `(resource, next_use_in, probability)` (decider spec §5.1,
today keyed by `TileId`, to be keyed by `ResourceId`). Each class's producer predicts from
what it knows:

| Class | Prediction source | Exactness |
|---|---|---|
| ground, 3D Tiles, procedural terrain | camera paths: film schedule, Glide/tape, gesture extrapolation (decider spec §5.2) | exact to probabilistic |
| clouds | the forecast clock: the next epoch's weather tiles are needed at a **known time**; the wind advects the field predictably; plus the camera | the epoch is exact, the advection near-exact |
| units | the simulation: waypoints, scripted paths, animation timelines, velocity extrapolation; a unit about to enter the view predicts its model's LODs | exact in a film or a scripted scene, probabilistic in play |
| ocean | none needed: standing resources, regenerated in place | — |

A prediction that a unit will come near raises the value of its finer LOD exactly as a
predicted tile's value rises today. No class-specific rule is needed.

### 2.9 The producer trait (host side)

A sketch in `tuile-core` (not in `tuile-budget`), so that the budget stays at the bottom:

```rust
/// A kind of content the budget manages, as the host implements it.
pub trait ClassProducer {
    fn descriptor(&self) -> ClassDescriptor;
    /// This step's wants of the class, in the common currency, from the views and the
    /// class's own state (weather, simulation, spectrum). Deterministic in its inputs.
    fn need(&mut self, views: &Views, out: &mut ClassNeed);
    /// Next uses within `horizon`.
    fn predict(&mut self, horizon: Micros, out: &mut Vec<Prediction>);
    /// The deterministic cost estimate of one resource, in every dimension it charges.
    fn cost(&self, r: ResourceId) -> Cost;
}
```

The traversal over tilesets and terrain is itself a producer of the built-in classes
(§5.3, stage 3). The executor's `Restorer` (fetch, generate, derive, upload, draw) is the
other half and stays in each backend.

---

## 3. A second dimension: time

### 3.1 What costs time, and how

| Work | Kind | Who controls it | Deterministic estimate |
|---|---|---|---|
| network or store fetch | latency (waiting), not frame time | the start buckets (decider spec §6.3) | bytes, quotas |
| upload | per-frame lane | `FrameDeadline`, `upload_bytes_per_frame` | bytes |
| GPU encode | per-frame lane (CB5) | the encode lane | blocks |
| **regeneration** (procedural tiles, bricks) | per-frame lane, CPU workers or GPU | a generation lane per backend | texels, vertices, voxels |
| **derivation** (cascade refill) | per-frame lane | the same lanes | texels |
| **drawing** what is resident: triangles × instances, skinned vertices, overdraw | recurring, follows the selection | the selection (what is refined) | `work` units per resource (`animated-instanced-3d-tiles.md` §4.2) |
| **recurring passes**: cloud march, ocean FFT, atmosphere, frame light | recurring, does not follow residency | quality knobs (#162 §5.4) | none per resource: measured per pass |

The split is #162's (admitted work can wait a frame, recurring work can only get
cheaper), extended by one row that #162 did not separate: **drawn work**, which is
recurring but follows the selection, and so belongs to the decider.

### 3.2 One currency, several limits: shadow prices

The decider spec already measures value in microseconds of worth keeping, so time needs
no second currency. What changes is the problem's constraints. Holding a set of
resources is a knapsack (Funkhouser & Séquin, decider spec §10b). With time it becomes a
**multi-dimensional** knapsack: GPU bytes, decoded bytes, upload per frame, generation
time per lane per frame, and drawn work per frame. The greedy by value per byte
generalises to value per **effective size**:

```
size_eff(r) = Σ_d  π_d × size_d(r) / capacity_d        over the dimensions d the class charges
order       = value(r) / size_eff(r)                    (the decider's release / admit key, within band)
```

`π_d` is the **shadow price** of dimension `d`: how scarce it is right now. The decider
spec's controller (§6.4, drift-plus-penalty) already keeps a virtual queue per limit
(GPU headroom, RAM headroom, each tier's flow). In Neely's framework the queue lengths
are exactly these prices (decider spec §10b, level R). So:

- the controller's state gives `π_d` for every dimension, in integers, every step;
- a class that charges only bytes (terrain) has `size_eff` as today when the time prices
  are zero. **When nothing is short of time, the order is today's**;
- when the GPU is short of generation time, regenerable content becomes "more expensive
  to hold off": its restore cost rises with `π_gen`, so it is kept rather than
  regenerated. The opposite order follows when memory is short and time is free. This
  is the keep-versus-regenerate trade-off of §2.4, made continuous by the price.

### 3.3 How the laws apply to each dimension

| Dimension | Hard or soft | Law that applies |
|---|---|---|
| GPU bytes, decoded bytes | **hard** (a jetsam or out-of-memory kill) | law 3: never crossed; the bypass releases what it must |
| upload, generation, encode lanes | **flow** (a token bucket per frame) | decider spec §6.3: shaped, no bursts; the first item of a frame always goes (I18) |
| drawn work | **soft** (a missed frame, not a crash) | law 10: degradation is continuous; when drawn work exceeds its target, the class's refinement stops (its LOD scale rises), and **coverage is never dropped**: law 1 ranks above it |
| recurring passes | **soft**, not per resource | #162 E3: a quality controller outside the ledger, whose output (a quality vector) is a recorded input; continuous knobs only |

Two rules make the precedence hold across dimensions:

1. **No dimension can buy a hole.** Under time pressure the decider refines less and the
   quality controller lowers knobs. It never releases a floor, and never drops a swap
   group's old half before the new one is active. This is why the floor is declared in
   per-frame work too (§2.5): the floors' recurring cost is admitted before anything
   else, and the registry refuses a set of classes whose floors would not fit in a
   frame.
2. **Time prices weight the value; they never reorder bands.** The decider spec's order
   `(protected?, band, value, …)` is unchanged: a price is inside `value / size_eff`, so
   the coarse band of any class still loads before the fine band of any class.

### 3.4 How #162's GPU-time budget fits

#162 recommended an execution budget in four stages (E1 measure, E2 a generic GPU-work
lane in CB5, E3 a quality controller with a recorded output, E4 more lanes). They all
stay, and this study places them:

- **E1** (per-pass GPU time in `FrameRecord`) is what measures the time prices' inputs.
- **E2's** `GpuWork { encode_blocks, generated_texels, brick_voxels }` is the set of
  lane dimensions of §3.2. Each unit is converted to time by the executor's measured
  rate. With the decider spec's law 5 that rate is a recorded input of `decide` (the
  restore cost of `Generated`), no longer an executor-only number. This supersedes
  #162's "measured times never enter `decide`" with the letter of law 5; the intent (no
  hidden input) is kept.
- **E3's** quality controller is the price of the recurring dimension. The decider's
  controller and E3's should be **one controller with several queues**, so that the
  LOD scale (the decider's actuator of last resort) and the march steps (E3's knobs) do
  not fight. The decider spec folds the LOD scale into its controller (§6.4); E3's knobs
  join it as further actuators, each with its own rate limit (I21). Q4.
- **#162's rejection** of "a per-resource GPU-time charge in the ledger" stands for
  **recurring passes** (a cloud march does not belong to a resource). It is lifted for
  **drawn work**, which does follow the selection (the forest tile of 20 million
  triangles in 1.8 MB, `animated-instanced-3d-tiles.md` §4.2): that is a deterministic
  per-resource estimate, not a measurement.

---

## 4. Extensibility mechanics

### 4.1 The registry: an open set, no enum to edit

- `ClassId(u8)`: a dense index, `MAX_CLASSES = 16`.
- `ClassRegistry`: built by the host and given to `Budget::new` within `BudgetConfig`.
  The four classes of today are **pre-registered** at indices 0 to 3, in today's order
  (`TERRAIN_MESH`, `TILES3D_MESH`, `DRAPE`, `OVERLAY`, as associated constants of
  `ClassId`). A host's classes follow, in its registration order. That order is
  deterministic, recorded, and part of the digest.
- **Adding a class at runtime** (a weather layer switched on) is a recorded input of the
  next step (`FrameInput::with_registration`). The registry is append-only: ids are never
  reused. **Removing a class** deactivates it: its resources are released through the
  shaped release flow, and its id stays. Replay sees both.
- `PerClass<T>` becomes `[T; MAX_CLASSES]` plus the registry's length: `Copy`, no heap,
  loops over the registered length only. Today's loops over `Class::ALL` (4) become loops
  over `registry.ids()` (4 when the host registers nothing).
- A per-class number (in-flight quota, ceiling) is the descriptor's or the
  specification's, never a field of `BudgetConfig` that someone edits per class.

### 4.2 No fixed reservation: shared pool, share follows the view

Every registered class is a member of the one pool. Nothing reserves a size for a class
(the memory rule of 2026-10-31). What changes from today's `Pool`:

| Today (`config.rs`) | Proposed |
|---|---|
| `members: PerClass<bool>`; `Overlay` outside the pool with its own 3 % | every class a member; `Overlay` too (Q3) |
| `floor_percent`: terrain 25 %, imagery 40 % of the pool | a class's floor is the **bytes of its floor representation** (§2.5), computed, not a percent: it is what the class needs to have no hole, no more |
| per-class `hard_limit` as the member's ceiling (60 %, 35 %, 75 %) | ceilings become **optional and adaptive**, a specification's parameter (e.g. speculation's share), never a reservation |

The split of the pool between classes is then entirely the value order: whichever class's
resources are worth more per effective byte holds the bytes. A view without clouds gives
clouds nothing. A view inside a cumulonimbus gives them most of the pool, down to the
ground's floor and no further.

Changing the shares moves the recorded digests. The memory rule says the change is
re-blessed deliberately, with the reason written. Stage 1 of the migration (§5.3) keeps
the old numbers, as a pool configuration equivalent to today's, so that the move is its
own PR.

### 4.3 Specifications weight classes per context

The decider spec's predicates already include `Class(Class)`. With the registry:

- `Predicate::Class(ClassId)`: a host's own specification names its own class;
- `Predicate::Tag(ClassTags)`: a specification written once (in tuile) selects classes it
  does not know by name: `Tag(VOLUMETRIC)`, `Tag(ANIMATED)`, `Tag(GROUND)`. A weather
  application's specification raises `ScreenSize` and `NextUse` on `Tag(VOLUMETRIC)`.
  FPV's specification (decider spec §3.7) raises `Deficit` on `Tag(GROUND)` near the
  camera, and stays valid when a host adds units;
- a specification that names an unregistered class is **valid and inert** for that rule:
  a specification travels between hosts. Validation warns and never fails, so a URL
  override never breaks a session;
- `Facts` (#256's `spec.rs`) gains `tags` beside `class`.

Examples, in the decider spec's notation:

| Context | Rule |
|---|---|
| FPV (exists) | `{ when: All[Tag(GROUND), LevelAtLeast(near)], term: Deficit, ×2000 }` |
| weather app | `{ when: Tag(VOLUMETRIC), term: ScreenSize, ×2500 }`, `{ when: Tag(VOLUMETRIC), term: NextUse, ×2000 }` (the forecast clock) |
| game, third person | `{ when: Tag(ANIMATED), term: Deficit, ×2000 }`; terrain left at ×1000 |
| film with units | `{ when: Always, term: NextUse, ×4000 }` with the units' scripted paths as exact predictions |

No context reaches above the bands: a weather app can make the clouds' fine bricks worth
more than the ground's fine tiles. It cannot make them worth more than the ground's
floor.

### 4.4 Telemetry: bounded labels per class

- The label value is the descriptor's `label`, validated by `register`
  (`[a-z][a-z0-9_]{0,23}`, unique). The set is bounded by `MAX_CLASSES`, so a series has
  at most 16 `class` values. The label key `class` is already allowed
  (`tuile_telemetry::resource::ALLOWED_LABELS`); its values become the registry's.
- `observe::labels::class(c)` becomes a registry lookup. The built-in labels
  (`terrain_mesh`, `tiles3d_mesh`, `drape`, `overlay`) are unchanged, so dashboards keep
  their series.
- Never a model name, a unit id or a layer URL in a label (the existing rule). A host
  that wants per-model numbers uses its own gauges.

### 4.5 Determinism, replay, wasm

| Concern | How it is kept |
|---|---|
| same inputs, same decisions | descriptors are data, recorded in the session header (`/budget/registry` in the MCAP tape) and hashed into the decision digest; producers' outputs are step inputs, recorded |
| no host code in `decide` | the producer runs before `decide` and the restorer after; `decide` reads only data |
| integer arithmetic | descriptor fields are integers; the effective size uses the existing saturating `scaled`; prices are the controller's integers |
| order | `ClassId` is the registration order; `ResourceId::Item` orders after the built-in variants, so the final tie-break of existing scenarios is unchanged |
| wasm | no new dependency; fixed arrays; `&'static str` labels |
| films | Batch: unbounded bytes, so prices are zero for memory; time lanes still shape the work; a film never regenerates on the GPU at render time unless its digest is per adapter by contract (#162 Q7: bake into the pack) |

---

## 5. Impact on the decider and on today's code

### 5.1 Asks of the decider stack now (#254 to #256, PRs 4 to 8)

Small, and each saves a breaking change later. None of them changes a decision:

1. **`Prediction` keyed by `ResourceId`**, not `TileId` (decider spec §5.1). A cloud epoch
   or a unit's model is not a tile.
2. **`RestoreCosts` as a table by restore path** with room for `Generated { lane }` and
   `Derived` rows (decider spec §4.2), even if empty. #162 asked the same of B1.
3. **`Facts.class` read through `ClassId::index()`**, and no `match` on `Class` in the
   value code, the same rule as the grep test for the specification's kind (law 11).
4. **The controller's virtual queues as a list of dimensions**, not two named fields
   (§3.2), so that time lanes join as entries.
5. **`Need::screen_px`** (decider spec §11), the screen weight every class maps onto
   (§2.7).

### 5.2 New invariants (to add to the spec when each lands)

| ID | Invariant |
|---|---|
| I24 | **Floors fit by construction.** Σ over registered classes of floor bytes ≤ the pool's hard limit minus the hard reserve, and Σ floor per-frame work ≤ the frame's floor share; `register` refuses otherwise. |
| I25 | **No class can buy a hole.** For any registry and any specification (proptest with random descriptors as well as random specifications, decider spec §3.6), laws 1 to 4 hold per class, with "hole" as the class's `Coverage` defines it. |
| I26 | **Unregistered is inert.** A specification rule naming an unregistered class changes no decision (digest identical with and without the rule). |
| I27 | **Built-ins unchanged.** With no host class registered, every golden (`goldens.txt`, `pattern_goldens.txt`, `tests/reference/recorded.txt`) is byte-identical to before the registry. |
| I28 | **Standing resources follow the swap order.** A standing resource's resize allocates the new size, activates it and only then releases the old one; the transient double counts against the hard limit. |
| I29 | **References protect.** A shared resource referenced by a drawn resource is protected (I11 extended along references). |

### 5.3 What breaks, and the migration

| Change | Breaking? | Digest impact |
|---|---|---|
| `Class` enum → `ClassId(u8)` with associated constants | **API**: every downstream `match` on `Class` (tuile-core `source`, `tileset`, `tiles3d`, `runtime`, `protocol`, `cover`; tuile-wgpu `pump`; tuile-web `wire`; tuile-viewer `web/control`) gains a `_` arm or becomes comparisons; constants are usable as patterns | none: `digest` hashes `class.index()` (`decisions.rs:220`), and the indices stay 0 to 3 |
| `PerClass<T>([T; 4])` → `[T; 16]` + length | **API** for `BudgetConfig` fields and `Ledger.gpu` | none if loops visit registered classes in index order |
| `ResourceId::Item { class, key: u64, sub: u32 }` appended | **API** for exhaustive matches on `ResourceId` | none: the hasher writes discriminant 0 / 1 for the existing variants (`decisions.rs:283`); `Item` takes 2 |
| `Need` gains `screen_px`; `sse_excess` documented as the class's quality excess (renaming it is optional, Q10) | additive | none until a decision reads it |
| `Cost` gains dimensions (`work`, lane units); CB2's `transfer`, `transient`, `encode_blocks` are the first | additive | none at zero |
| `Tier` gains `Generated { lane }`, `Derived` (and the decider's `HttpCache`) | **API** for exhaustive matches | none until presence answers them |
| `NeedSet` gains a generic list `items: Vec<(ResourceId, ItemNeed)>` beside the tile lists | additive | none when empty |
| `DrawSet` gains `items` for generic resources (the tile list keeps its drapes) | additive | hashed only when non-empty |
| the wire (`tuile-web/src/wire.rs`, `tuile-core/src/protocol.rs`) carries class indices | protocol: a remote session's registry must be sent once with the session | — |

Migration, in stages that each stand alone and re-record nothing until stage 4:

| Stage | Content | When (plan order kept) | Goldens |
|---|---|---|---|
| 0 | this study | now | — |
| 1 | `ClassId`, the registry with the four built-ins, `PerClass` as a fixed array with a length, labels from the registry, `Predicate::Tag`; I27 proven by running the goldens | after decider PR 4 (the value), so that the eviction key is not changed twice | **byte-identical** (I27) |
| 2 | `ResourceId::Item`, generic `NeedSet.items` and `DrawSet.items`, `Cost` dimensions, `Coverage` / `FloorRule` / `Lifetime` read by the decider; a **synthetic class in `tuile-budget-sim`** (moving objects with an LOD chain and a `Coarsest` floor) to prove I24 to I26 and I29 on the scenario catalogue | after decider PR 6 (the simulator crate) | byte-identical for built-in scenarios; new goldens for the synthetic class |
| 3 | `Generated` restore path, `persist`, the generation lanes; the terrain and tileset traversal wrapped as the producer of the built-in classes | with `tuile-procedural`'s host integration (its PR 3, #162 §6.1) | byte-identical (no generated class in old scenarios) |
| 4 | the dimensions' shadow prices in the controller; drawn work as a soft dimension; E3's knobs as actuators | with decider PR 7 (the controller) | re-recorded deliberately, with the reason |
| 5 | pool floors from floor representations instead of `floor_percent` | its own PR, after stage 4 | re-recorded deliberately (the memory rule) |
| 6+ | one family at a time, each with its producer crate and its scenarios: procedural terrain, then clouds, units, ocean | when each consumer exists | new goldens per family |

This order respects the budget plan: no stage jumps a decider step, and 3D Tiles stay
parked as decided. 3D Tiles are a built-in class here, and this study changes nothing
about when they come back into the ledger.

---

## 6. Per family, worked through

Each family is given as its descriptor and the four questions that matter: what is
cached, what restores it, what a hole is, and what predicts it. The two reference
classes come first.

### 6.1 Reference: terrain and imagery (built-in)

| | Terrain mesh | Drape |
|---|---|---|
| Tags | `GROUND` | `GROUND` |
| Hierarchy | `Quadtree` | `Quadtree` (imagery levels, chosen apart from the tile's level) |
| Coverage / floor | `Hole` / `Levels(pinned_level)` | `Hole` (`bare`) / `Levels(pinned_level)` |
| Restore | `Remote`, `Stored`, `HttpCache`, `Decoded` | the same; a drape mosaic is composed from imagery tiles (`Derived` in spirit, kept as today) |
| Lifetime | `Static` (epochs for a content refresh) | `Static` |
| Charges | GPU bytes (+ CPU copy for stitching), decoded, upload | the same, encode blocks when compressed |
| Need | SSE excess, screen area | footprint level, screen area |
| Predictions | camera paths | camera paths |

### 6.2 Reference: 3D Tiles (built-in)

Tags none (not ground: `is_ground() == false` today). Hierarchy `Quadtree`/`Octree` as
the tileset's tree. Coverage `Hole` for REPLACE (blockers make a swap group), `Additive`
for ADD children. Floor: none of its own. #174's finding, "a tileset's coarsest tiles are
no floor", becomes a descriptor fact rather than an `is_ground` branch. Restore `Remote`,
`Stored`. Charges GPU bytes (without the CPU copy, #174 §1), decoded, upload, and drawn
work for instanced tiles. With the registry, the "ground under a tileset" rule (#174 §4)
stays traversal-side, unchanged.

### 6.3 Volumetric clouds

What exists to budget (`volumetric-clouds.md` §3.8, #162 §3):

| Resource | Size, desktop / mobile (est.) | Lifetime | Restore | Hierarchy |
|---|---|---|---|---|
| shape + detail noise, curl, profile LUT | ≈ 8.2 / 1.2 MiB | `Standing` (seeded, once) | `Generated { Gpu }` | `Flat` |
| weather tiles (coverage, type, density per layer) | 12–48 MiB decoded per regional flight | `Versioned` (forecast steps) | `Remote`, `Stored` | `Quadtree` |
| weather cascades 3 × 512² around the camera | ≈ 18 / 4.5 MiB | `Standing`, refilled on motion | `Derived` from weather tiles | `Nested` |
| voxel bricks for tall clouds (cumulonimbus), octree | 10²–10³ MiB per region | `Versioned` | `Remote` or `Generated` from weather + noise | `Octree` |
| shadow map, half-res targets + history | ≈ 20 / 8 MiB at 1080p | `Standing`, resolution-dependent | `Generated { Gpu }` | `Flat` |
| the march | 1–2 ms desktop, 3–5 ms mobile | recurring | — | — |

**Cumulonimbus against stratus.** They are the same class with different costs, not two
classes:

- a stratus deck is wide and thin. The 2.5D path (weather map × height profile) carries
  it almost entirely. Its bricks are few and coarse, and its cost is march time over a
  large screen area;
- a cumulonimbus is tall (to 12 km and more) and dense. It needs octree bricks through
  its height. Its bytes grow with its vertical extent, and its screen weight is large
  while its interior is mostly hidden by its own opacity.

The producer emits per brick what the weather says, and the currency does the rest. A
separate class per cloud genus would be a classification the weather data already makes.
Layers (low, middle, high, cirrus) are a `sub` of the resource key, as imagery's
`LayerId` is.

**Hole.** `Continuity`, floor `Standing` (the noise and the coarsest cascade) plus the
2.5D slab: clouds never vanish from one frame to the next. Fine bricks are refinement:
absent, the cloud is softer, never missing. **A forecast step is a temporal swap group**:
the weather tiles of `t₀` and `t₁` are both held while the renderer interpolates between
them, and `t₀` is released only once the blend has passed it.

**Need.** Brick quality excess from the voxel's projected footprint (§2.7). The screen
weight is weighted by the transmittance seen through the brick on the previous frame (a
recorded input). That keeps the budget from spending a cumulonimbus's interior on bricks
nobody sees, the volumetric analogue of the ground-under-a-tileset rule.

**Predictions.** The forecast clock gives the next epoch's tiles an exact `next_use_in`.
Wind advection moves the field along a known vector, so bricks upwind of the view are
predicted. The camera's predictions apply as for tiles.

**Time.** The march is recurring (E3's knobs: steps, resolution, amortisation, all
continuous, #162 §5.4). Cascade refill and brick generation go in the GPU generation
lane. A weather-app specification raises the clouds' value; it cannot raise the march
past the recurring dimension's price, which is the controller's.

### 6.4 Animated units (moving models, characters, vehicles, crowds)

What exists to budget (`animated-instanced-3d-tiles.md` §4):

| Resource | Size (est.) | Lifetime | Restore | Hierarchy |
|---|---|---|---|---|
| a model's LODs (mesh, textures), shared by every unit of the model | 0.1–3 MB per LOD | `Moving` | `Remote`, `Stored` | `LodChain` |
| the model's impostor atlas (the floor) | 0.1–1 MB | `Moving` | `Remote` or `Generated { Gpu }` (baked from the coarsest LOD) | last link of the chain |
| pose / animation textures per clip | ≈ 0.4 MB per model (2 clips × 2 s × 64 joints) | `Static`, shared | `Remote`, `Generated { Cpu }` (baked at decode) | `Flat` |
| per-instance buffers (transforms, animation phase) | N × 64 B | `Standing`, updated every frame | the simulation | — |
| skinning, drawing | ∝ visible instances × LOD vertices × joints | recurring, **follows the selection** | — | — |

**The unit of the budget is the resource, not the unit.** Ten thousand walkers of
twelve models are at most twelve × (LODs) resources. The producer (the host's
simulation, or `tuile-bevy` on its behalf) **aggregates** each frame:

- the need of a model's LOD is the finest LOD any visible instance needs;
- its screen weight is the sum over the instances that need it.

The need traffic stays bounded by models × LODs, whatever the crowd's size, and a
`NeedDelta` changes only when an aggregate changes. A per-unit need would make the
budget's work grow with the crowd (the P6.4 bound: work grows with change, not with the
view).

**Visibility moves with the unit.** The producer computes distance and screen size from
the simulation's positions and the views, not from a tile traversal. A parked camera
watching cars go by has a moving need. That is `Lifetime::Moving`; the decider sees only
needs that change.

**Hole.** `Hole`, floor `Coarsest`: the impostor (or coarsest LOD) of every model with an
instance in the scene is held while the instance exists. A unit in view is never
missing. A finer LOD not yet resident is popping: deficit levels × weight, and a swap
group through the cross-fade (§2.5).

**Drawn work, the second dimension.** Crowd LOD is exactly the trade between the
deficit term and the drawn-work price. When skinned vertices cost frame time, `π_work`
rises and the finer LODs' effective size grows. Fewer instances get the near LOD, and
the boundary between skinned meshes and impostors moves **continuously** with the price,
without a fixed distance threshold (the continuous-rendering rule).

**Predictions.** Scripted paths, waypoints and timelines give exact next uses: in a film,
or a scene whose units follow known paths, a vehicle that will come near has its fine
LOD loaded ahead with `p = 1000`. In play, velocity extrapolation gives a probability,
under the speculation quota.

**Scope.** Units' content (models, simulation) is the host's, not tuile's. Tuile offers
the class mechanics, and the producer lives in the host or in `tuile-bevy`. Q7.

### 6.5 Procedural terrain

What exists (`procedural-unbounded-terrain.md`, #162 §4): generated terrain meshes and
drapes, the **same classes** as streamed ground (`TerrainMesh`, `Drape`), with one more
restore path. That makes it the smallest step and the best first new class.

| | |
|---|---|
| Tags | `GROUND`, `GENERATED` |
| Hierarchy, coverage, floor | the ground's: `Quadtree`, `Hole`, `Levels(pinned)`, the coarse levels generated first and never released |
| Restore | `Generated { Gpu }` (< 0.2 ms per 256² est.) or `Generated { Cpu }` (0.5–40 ms est.), and `Stored` once persisted |
| Lifetime | `Static`: content = f(generator version, seed, level, x, y). **Quality never changes content**; it changes only which levels are selected (#162 §4.3) |
| Charges | GPU bytes, upload (CPU path), the generation lane |
| Need, predictions | the ground's: SSE, camera paths |

**Cache or recompute** is §2.4's currency decision: a generated tile is kept while its
value per effective byte beats the others'. Its restore cost is its generation time,
priced by the generation lane's shadow price (§3.2). When the GPU has spare time, cheap
GPU-generated tiles are released early and regenerated on return. When the GPU is busy
(a heavy cloud frame), they are kept. `persist` writes CPU-expensive imagery to the
store.

**Mixed worlds.** Procedural detail under real data (a host grid of real terrain with
generated octaves beneath, `procedural-unbounded-terrain.md` §3.5) is a `Derived`
resource: its inputs are the real tile (fetched) and the generator. Its restore cost
counts the real tile's fetch when the real tile is gone.

**The physical level** stays outside the budget (#162 §4.3). A host-declared local floor
around points of interest (#162 Q6) is a protected need, the same mechanism as a class
floor.

### 6.6 Procedural waves and ocean

What exists to budget, following the usual FFT ocean (Tessendorf's spectrum, several
cascades; sizes are estimates for this study, not measurements):

| Resource | Size (est.) | Lifetime | Restore | Hierarchy |
|---|---|---|---|---|
| initial spectra `h₀(k)` per cascade, seeded | 3–4 cascades × 256² or 512² × RGBA32F: 3–16 MiB | `Standing`, regenerated when wind or fetch changes | `Generated { Gpu }` | `Nested` |
| displacement, derivatives, foam per cascade | 3–4 × 256²–512² × (RGBA16F × 2–3), with mips: 6–40 MiB | `Standing`, **regenerated every frame** (or every few frames for the long cascades) | `Generated { Gpu }`: the FFT | `Nested` |
| coastal data: bathymetry, shoreline distance | streamed tiles | `Static` | `Remote`, `Stored` | `Quadtree` |
| wake or interaction simulation | a few MiB around the camera | `Standing`, **owned**: state, not cache | none: lost if released | `Flat` |
| the FFTs, the surface draw | 0.3–1.5 ms est. per frame, by cascade count and size | recurring | — | — |

**The ocean is mostly not cache.** Its large resources are regenerated every frame from
a closed form: `h(k, t)` is a function of time and the seeded `h₀`, with no history. There
is nothing to keep or evict by value, and no fetch. So it enters the budget:

- as **standing** resources in the pool, sized by the quality vector, resized only with
  the swap order (I28). Its bytes count with everyone's, and an ocean off screen
  releases them;
- through the **time dimension**: the FFTs are recurring work, controlled by E3's
  continuous knobs. Cascade *sizes* are not knobs, since resizing reallocates and steps
  (#162 §5.4). The continuous knobs are each cascade's **update interval** and the
  spectrum's cut-off:
  - the update interval is continuous and phase-correct, because `h(k, t)` is evaluated
    at the frame's time, so a long-wavelength cascade updated every third frame is
    exact at the frames it is computed and moves slowly on screen between them;
  - the cut-off fades the shortest waves into the normal map by `smoothstep`;
- the **coastal tiles** are an ordinary fetched quadtree class.

**Hole.** `Continuity`, floor `Standing`: the coarsest cascade at minimum resolution. The
sea is never flat for a frame, and never missing. Finer cascades are refinement.

**Owned state.** The wake simulation is the first `Standing` resource whose loss is not
a quality loss but a lie: it cannot be regenerated. #162's "owned (dirty) resources"
applies: never released while the class is active, and written back (or deliberately
reset) on deactivation. Q6.

**Films.** The decisions never depend on the wave field, only on declared costs, so a
film's decision digest is independent of the GPU's FFT bits. The **pixels** are not. For
frame-exact films across adapters, the spectrum is evaluated on the CPU or the film
accepts per-adapter image digests, the same choice as procedural GPU content (#162 Q7).

### 6.7 The families side by side

| | Terrain / imagery | 3D Tiles | Clouds | Units | Procedural terrain | Ocean |
|---|---|---|---|---|---|---|
| Hierarchy | quadtree | tree | octree + nested | LOD chain | quadtree | nested |
| Coverage | hole | hole / additive | continuity | hole (missing) / popping (LOD) | hole | continuity |
| Floor | pinned levels | none | slab + coarsest cascade | impostor per model in use | pinned levels | coarsest cascade |
| Restore | fetch | fetch | fetch, derive, generate | fetch, bake | generate, persist | generate |
| Lifetime | static | static | versioned | moving | static | standing |
| Main dimension | bytes | bytes, drawn work | bytes, recurring time | drawn work, bytes | generation time, bytes | recurring time |
| Predicted by | camera | camera | forecast clock, wind, camera | simulation, scripts | camera | — |

---

## 7. Risks

- **Over-generalisation.** A descriptor designed for six families before the second
  exists is a guess. Mitigation: stage 1 only opens the set (ids, registry, labels),
  behaviour-free and byte-identical; each descriptor field lands with the first family
  that needs it and a synthetic sim class that tests it (stage 2).
- **Cross-class comparability.** If one level of a cloud brick and one level of a
  terrain tile are not worth the same on screen, the shared pool misallocates. That
  would not make a hole, but it would put quality in the wrong place. Mitigation:
  `quality_scale` tuned in the simulator against an image metric, and the deficit
  reported per class so that a bad scale shows. Q5.
- **Need traffic.** Moving needs (units) and versioned needs (clouds) change every step.
  Without aggregation, a crowd would push the decider's work past P6.4's bound.
  Mitigation: aggregation per resource is part of the producer's contract, and the sim's
  allocation ratchets and `budget_decide` bench run with the synthetic class.
- **Controller coupling.** Several prices in one controller can oscillate against each
  other: memory releases regenerable content while time pressure keeps it. Mitigation:
  one controller, critically damped per queue, with the stability bounds of decider spec
  §6.5 extended to every dimension, and the scenario catalogue gaining "generation-bound"
  and "draw-bound" profiles.
- **Golden churn.** Every stage claims byte-identical goldens until stage 4. That claim
  is proven by running the goldens, never assumed (I27). Any stage that moves a digest
  says so in its PR.
- **API churn downstream.** `ClassId` and the fixed array touch eight downstream files in
  four crates (§5.3). It is one mechanical PR, but it collides with any open PR that
  names `Class::` (the 3D Tiles stack). Mitigation: land stage 1 between stacks, and
  rebase the parked 3D Tiles stack onto it.
- **Remote sessions.** A `tuile-server` session's budget (`01-architecture.md`, D-A5)
  would need the consumer's registry and producers. Host classes such as units are
  local by nature (the simulation is the host's). Leaning: host classes live only in
  local geometry servers, for now.

---

## 8. Open questions for Laurent

| # | Question | Leaning |
|---|---|---|
| Q1 | A class is **data (descriptor) + a producer outside `decide`**, never a trait object called inside it? | yes: it keeps law 5 and the determinism lints whole |
| Q2 | The registry fixed at `Budget::new`, or **append-only at runtime** as a recorded input? | append-only, recorded; removal deactivates, ids never reused |
| Q3 | Pool floors computed from each class's **floor representation**, replacing `floor_percent`, and `Overlay` brought into the pool? | yes, as its own PR (stage 5), digests re-blessed with the reason |
| Q4 | **Several limits with shadow prices** in the decider's controller (one currency, multi-dimensional knapsack), and E3's knobs as further actuators of that one controller rather than a second controller? | yes |
| Q5 | Cross-class calibration: a constant `quality_scale` per class tuned in the simulator against an image metric, or a measured perceptual weight per frame? | the constant first; a measured weight only if the per-class deficit shows misallocation |
| Q6 | **Owned state** (ocean wakes, edited voxels): the budget tracks it (never released while active, write-back on deactivation), or the class keeps it outside the ledger as its own allocation? | the budget tracks it (#162 Q9), with a write-back quota |
| Q7 | Units: tuile offers the class mechanics and a synthetic sim class only, with the units' producer in the host or `tuile-bevy`? | yes: units' content is the host's, as media are STL's |
| Q8 | The first real new class: **procedural terrain** (reuses the ground classes, adds only `Generated`), before clouds? | yes |
| Q9 | `persist` (writing generated content to the store) as a decider output, under a write quota? | yes, at stage 3 |
| Q10 | Rename `Need::sse_excess` to `quality_excess` (breaking, cosmetic) or keep the name and document it as the class's excess? | keep it, document it; rename only if a second need type appears |

---

## 9. Literature pointers

Verification levels as in the decider spec: **V** full text read, **R** abstract or
extracts only, **C** prior knowledge not re-checked for this study. Everything below is
**C** unless marked; each is a pointer to read before the family it concerns is
designed, not a source of a figure used above.

| Topic | Pointer | Level | Relevance |
|---|---|---|---|
| one budget over heterogeneous content | Funkhouser & Séquin, *Adaptive display algorithm for interactive frame rates during visualization of complex virtual environments*, SIGGRAPH 1993 | V (via the decider spec §10b) | LOD selection as a knapsack by benefit / cost; the base of §3.2 |
| cost-benefit currency for prefetch and retention | Patterson et al., *Informed prefetching and caching* (TIP), SOSP 1995 | V (via the decider spec) | the single currency |
| shadow prices from queues | Neely, *Stochastic Network Optimization with Application to Communication and Queueing Systems*, 2010 | R (via the decider spec) | queue lengths as Lagrange multipliers (§3.2) |
| cache with restore cost | Cao & Irani, *Cost-aware WWW proxy caching algorithms* (GreedyDual-Size), USITS 1997 | C | regeneration time as restore cost |
| volumetric clouds, real-time budget | Schneider & Vos, *The real-time volumetric cloudscapes of Horizon Zero Dawn*, SIGGRAPH 2015 Advances; Schneider, *Nubis* (2017) and *Nubis³* (SIGGRAPH 2023 Advances) | C (cited in `volumetric-clouds.md`) | per-frame march budget, amortisation, SDF stepping |
| clouds with a physical sky | Hillaire, *Physically based sky, atmosphere and cloud rendering in Frostbite*, SIGGRAPH 2016 course | C | march cost, temporal reprojection, froxels |
| sparse voxel residency | Crassin et al., *GigaVoxels: ray-guided streaming for efficient and detailed voxel rendering*, I3D 2009 | C | ray-guided brick requests: the GPU reports which bricks the rays needed; a candidate producer for cloud bricks (§6.3) |
| crowd LOD | Dobbyn et al., *Geopostors: a real-time geometry/impostor crowd rendering system*, I3D 2005; Kavan et al., *Polypostors*, I3D 2008; Beacco et al., *A survey of real-time crowd rendering*, CGF 2016 | C | skinned mesh / impostor trade-off driven by a budget, the drawn-work dimension (§6.4) |
| ocean FFT | Tessendorf, *Simulating ocean water*, SIGGRAPH course notes 2001 | C | closed-form `h(k, t)`: no history, phase-correct amortisation (§6.6) |
| ocean in a production engine | Gonzalez-Ochoa, *Wakes, explosions and lighting: interactive water simulation in Atlas*, GDC 2019 | C | cascade budgets, wake state as owned resource |
| procedural terrain caching | Losasso & Hoppe, *Geometry clipmaps*, SIGGRAPH 2004; Dachsbacher & Stamminger, *Rendering procedural terrain by geometry image warping*, EGSR 2004 | C | nested regeneration around the camera; generate-on-demand against cache |
| virtual texturing priority | van Waveren, *Software virtual textures* (id Tech 5), 2012 | V (via the decider spec) | (wanted − resident) × screen samples: the deficit × screen weight of §2.7 |

Patents: none were searched for this study. Before a family's design borrows a specific
method (crowd LOD selection, ocean cascade scheduling), the decider spec's rule applies:
the claims are read first.
