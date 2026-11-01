<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The graphics budget beyond streamed tiles: atmosphere, voxels, procedural — and GPU time

Status: **study** (2026-10-31), no code. Question from Laurent: the budget
(`tuile_core::budget`, about to become `tuile-budget`) accounts for streamed
meshes and drapes; the atmosphere, future voxels and the planned
`tuile-procedural` will need budget notions too. Which ones, and how do they
fit the existing model without a special path per mode? Addendum, same day:
should tuile have an **execution budget** (GPU time per frame) beside memory
and upload bytes?

Inputs read: `graphics-budget-architecture.md`, `graphics-budget-to-be-and-spec.md`
(I1–I18), `graphics-budget-roadmap.md`, `gpu-compressed-textures.md` §6/§9,
the approved order (tuile-budget → O1 → O-CB → CB1–CB10 → A2 → O-B → B1–B8 →
O-C → C1–C5), the code of `crates/tuile-core/src/budget/`, `crates/tuile-atmosphere`,
`crates/tuile-wgpu/src/{sky.rs,compress.rs}`, and the explorations
`atmosphere-generalisation.md`, `volumetric-clouds.md`,
`procedural-unbounded-terrain.md`. Line numbers are on `origin/main` after
v0.15.0.

---

## 0. Summary

- **The model holds.** Every new subject decomposes into five notions, four
  of which the budget already has or has planned: *evictable resources* in
  the ledger (classes, tiers, pool floors/ceilings), *fixed reservations*
  (CB4), *admitted work per frame* (`FrameDeadline` lanes: bytes today,
  encode in CB5), *quality as a continuous dimension* (`sse_scale` relaxation
  exists, `engine.rs:2273`), and *prediction* (C). The missing one is a
  **recurring GPU-time** account — what every frame costs whatever is
  admitted (draw, atmosphere, cloud march) — which belongs to a host-side
  controller, not to `decide`.
- **Atmosphere** is small in memory (≈ 1.4 MB of fixed LUTs) and recurring in
  time (sky-view, frame light, per-fragment aerial march). It needs a
  **fixed reservation** and a place in the **recurring GPU time**; it needs
  no ledger class. Its metering/exposure must never be scaled.
- **Voxels** are tiles in three dimensions: a **new class** (bricks) with its
  own pool floor/ceiling, the same fallback chain (octree parent), upload and
  build **lanes**, fixed reservations (noise, brick atlas, half-res targets),
  and one genuinely new notion for destructible content: **owned (dirty)
  resources** whose eviction requires a write-back.
- **Procedural** content reuses the classes it produces (terrain mesh,
  drape). What is new is a **`Generated` tier/source**: restore cost =
  regeneration time instead of a fetch, its own generation quota (CPU
  workers) and a **GPU generation lane**. It fits B1's GreedyDual-Size
  restore cost directly and C's prediction as "generate in spare lane time".
  The physical level is outside the budget entirely; the visual level is
  budgeted like any tile.
- **Execution budget: yes, in stages, not now as a decide input.** Measure
  first (extend CB1 to per-pass GPU time), make CB5's encode lane the first
  instance of a *generic* GPU-work lane, then — after A2.7 gives MCAP replay
  — add a slow quality controller beside B6 whose *outputs* (continuous
  quality numbers) are recorded inputs of `decide`. Measured times never
  enter `decide` (D-A8, I12).
- **Minimal first step:** in CB1, record GPU time per pass (ground, sky,
  frame light, encode, overlays) and the presentation interval in
  `FrameRecord`, metrics and MCAP; run
  `crates/tuile-atmosphere/examples/frame_light_cost.rs` on the Mac, an
  iPhone and WebGL2 to get the atmosphere's real numbers. No behaviour change.

---

## 1. The model today, as the new subjects see it

| Notion | Where | Unit | Who decides | Deterministic |
|---|---|---|---|---|
| Classes with target / overflow / hard limit | `types.rs:21` `Class` (4, exhaustive), `config.rs:106` | bytes | `decide` | yes |
| Pooled ledger (ground) with per-class floors/ceilings | `config.rs:71` `Pool`; `interactive()` 82 % of the device, terrain 25–60 %, imagery 40–75 % | bytes | `decide` | yes |
| Tiers GPU / Decoded / Stored / Remote | `tier.rs:13` | — | `decide` (presence oracle) | yes |
| Per-step upload cap | `config.rs:131` `upload_bytes_per_frame` | bytes | `decide` | yes |
| Per-frame deadline | `deadline.rs:23` `FrameDeadline { bytes, time }` | bytes | executor | executor-only, deviations reported (I18) |
| Encode pacing | `tuile-wgpu/src/compress.rs:195` `EncodePace` (4 ms share, outside the budget) | µs → blocks | executor | no (removed in CB6) |
| Quality relaxation | `engine.rs:2273` `relax`: `sse_scale` rises/decays per second on memory overflow | thousandths | `decide` | yes (time is an input) |
| Thermal | `engine.rs:312–318` quota and upload ÷1/2/4; `engine.rs:1443–1449` SSE floor 1.5/2.0 | levels | `decide` | yes, **stepped** |
| Reservations | CB4 (planned): `reserved: PerClass<Bytes>`, `decoded_reserved` | bytes | config | yes |
| Prediction | C (planned): a ledger entry in a band below Ancestor | — | `decide` | yes (predictions are recorded inputs) |

Two properties matter for everything below:

1. **The ledger counts resources that come and go** (keyed by `ResourceId`,
   evictable unless protected, I11). Anything allocated once and held for
   the session is not a ledger entry: it is a *reservation* subtracted from
   the device before the split.
2. **`decide` sees only recorded inputs.** Pressure, thermal and `retarget`
   are host signals, set between steps and replayable. A measured duration is
   not; it may only reach `decide` through a host decision that is itself an
   input (D-A8).

---

## 2. Atmosphere

### 2.1 Resources

| Resource | Size | Lifetime | Where |
|---|---|---|---|
| Transmittance LUT 256×64, `Rgba32Float` | 262,144 B | per medium (CPU f64, uploaded) | `hillaire.rs:59`, `gpu.rs:456`, `LUT_FORMAT` `gpu.rs:356` |
| Multiple scattering 32×32 | 16,384 B | per medium | `hillaire.rs:61` |
| Level irradiance 256×16 | 65,536 B | per medium | `hillaire.rs:66` |
| Sky-view + Moon-view 2 × 192×108 | 663,552 B | **every frame** (GPU) | `hillaire.rs:63`, `gpu.rs:463–464` |
| Star table 256 × n | ≈ 0.37 MB (from `stars.table`) | once | `gpu.rs:465`, `stars.rs:82` |
| Frame-light textures (meter rays 336×2, partials) | tens of KB | every frame | `frame_light.rs:600–620` |
| Uniform | < 1 KB | every frame | `gpu.rs:44` |
| HDR target (if adopted) | `w × h × 8 B` (Rgba16Float): 32.8 MB at 2560×1600 | per resize | not yet |
| N lights (generalisation) | + 330 KB per sky-view layer | per config | `atmosphere-generalisation.md` §3.3 |

CPU: `LutAtmosphere::new` is f64 and takes "seconds even optimised"
(`atmosphere-generalisation.md` §1); today's clear day is baked
(`today.tables`, 252 KB) and a changed medium recomputes on the calling
thread (`gpu.rs:670–700`). The reference model is kept twice in memory
(`model`, `reference`, `gpu.rs:318–319`).

### 2.2 Time per frame

| Pass | Work | Scales with | Measured |
|---|---|---|---|
| Frame light (two passes: shares, then slots) | 1024 directions × 2 lights + 336 meter rays × 16 steps | fixed | not yet — `examples/frame_light_cost.rs` exists (timestamps where available) |
| Sky-view | 2 × 20,736 texels × 30 steps (`SKY_STEPS`, `hillaire.rs:82`), compute or render pass (WebGL2) | table size, steps | est. 0.05–0.1 ms desktop, 0.3–0.5 ms mobile (`atmosphere-generalisation.md` §5.5) |
| Aerial perspective + ground light, per ground fragment | march 12 steps under 10 km, 24 above (`tuning.rs:350` `DEFAULT_MARCH_STEPS`) × K lights | **pixels × steps** | "the dominant cost" on mobile (same §5.5), unmeasured |
| Sky pixels: discs, stars, glare | per sky fragment | pixels | unmeasured |

The light and the sky-view are made **every frame** since 32b22d3f (the
stepped recompute was visible: luma jumps of 13 levels). That rule is a
constraint on any budget: quality may not be traded by recomputing less
often.

### 2.3 Must never degrade

- **The sky is never black or missing.** The tables are always bound (the
  "off" uniform until a look asks, `gpu.rs:864`); a new medium's tables are
  computed off the render thread and swapped in only when complete — the
  activate-then-deactivate order of the ground, applied to LUTs.
- **The exposure is never scaled.** Metering, exposure, white balance and the
  sky harmonic come from the frame light; any quality change there moves the
  whole frame's brightness. Its cost is fixed and small: it is outside every
  quality knob.
- **No step in the picture.** Changing the march from 24 to 20 steps changes
  the haze's bias; done at once it is a visible jump. Any quality change of
  the march must be continuous (§5.4).
- **Films** keep their fixed quality (Batch): the atmosphere-on film digests
  do not move.

### 2.4 Budget notions it needs

| Notion | How |
|---|---|
| **Fixed reservation** (GPU), not a ledger class | ≈ 1.4 MB today, + 330 KB per extra light, + the HDR target. A session-wide reservation subtracted before the split (§6.2). The HDR target is resolution-dependent: a reservation the host updates on resize, as an input. |
| **Recurring GPU time** | sky-view + frame light + per-fragment march: part of the frame's recurring cost (§5), traded through continuous quality knobs (march steps, sky-view steps, K lights), never the frame light. |
| **Generated resource with a long restore** (the per-medium LUTs) | not in the ledger (never evicted, one copy), but its rebuild is a *job* on a worker with the swap order above; on the web, baked presets only (CPU would block for seconds). |
| Nothing else | the atmosphere has no streaming, no eviction, no prediction. |

---

## 3. Voxels (clouds first, then volumes)

### 3.1 Resources

From `volumetric-clouds.md` §3.2, §3.8, §5.4, §6.6 and the general case of a
sparse voxel structure:

| Resource | Size (desktop / mobile) | Kind |
|---|---|---|
| Shape + detail noise, curl, profile LUT | ≈ 8.2 MiB / 1.2 MiB | fixed, generated once from a seed |
| Weather cascades 3 × 512² (camera-centred) | ≈ 18 MiB / 4.5 MiB | fixed per tier, refilled as the camera moves (swap order) |
| Cloud shadow map | 8 MiB / 2 MiB | fixed per tier |
| Half/third-res cloud targets + history | ≈ 12 MiB at 1080p / 6 MiB | resolution-dependent |
| Cloud tiles (decoded, CPU) | 12–48 MiB per regional flight | streamed: "counted like imagery" (§6.6) |
| Voxel bricks (e.g. 8³ + apron) in a 3D atlas, with an indirection table / octree nodes | 10²–10³ MiB per region (§3.2) | streamed, evictable, LOD by octree level |
| Edited bricks (destructible terrain, caves) | per edit | **owned**: no remote copy |

### 3.2 Time per frame

| Work | Scales with | Order of magnitude |
|---|---|---|
| Cloud march (recurring) | pixels × resolution scale × steps × amortisation (1, 1/4, 1/16 pixels per frame) | 1–2 ms desktop, 3–5 ms mobile (§3.8) |
| Shadow map (amortised) | resolution, steps, how often | 0.1–0.3 ms |
| Cascade refill (admitted) | texels per refill | bursty, on camera motion |
| Brick upload (admitted) | bytes | the byte lane |
| Brick build / decompress / meshing of an edited region (admitted) | bricks | a compute lane |

### 3.3 Must never degrade

- **No hole in a volume the frame shows.** A brick not resident shows its
  octree parent: the same fallback chain as tiles (I1, I9, I11). `TileId` is
  an opaque `u64` and `Need` carries `parent` (`source.rs:42`,
  `need.rs:68–72`), so an octree fits the chain unchanged.
- **No frame lacks clouds the previous one had** (cascade swap order,
  `volumetric-clouds.md` §3.6); under pressure the budget shrinks cascades
  and resolution, never evicts the field being drawn (§6.6).
- **An edit is never lost.** An edited brick is state, not cache.
- **Films have no history** (§3.3): every film frame is full quality, no
  temporal amortisation.

### 3.4 Budget notions it needs

| Notion | How |
|---|---|
| **A new class** `Volume` (bricks) | its own target/overflow/hard, floor/ceiling. Destructible terrain and caves are ground: **members of the ground pool** with their own floor. Clouds are not ground: a class outside the pool, like 3D Tiles' 15 %. |
| Same fallback chain, same swap groups | octree parent = fallback; refining a brick = a swap group of its children. Nothing new in `select`. |
| **Lanes** | brick upload in the byte lane; brick build/decompress and cascade refill in a GPU-work lane (§5.3). |
| **Fixed reservations** | noise, cascades, shadow map; the targets as a resolution-dependent reservation. |
| **Recurring GPU time** | the march: quality via steps, resolution scale, amortisation (§5.4). |
| **Owned (dirty) resources** — new | an edited brick has no `Remote` copy: its restore cost is infinite until it is written back to a local store. Eviction of an owned resource = write-back first, then demotion to `Stored` (the OS dirty-page rule). The ledger needs a `dirty` bit and a write-back quota; I14 extends to "an edit is never refetched over". |
| Prediction (C) | identical to tiles: predicted bricks along the path, band below Ancestor. |

---

## 4. Procedural (`tuile-procedural`)

Decisions as summarised for this study: terrain, imagery and biomes from
formulas; **shader-first on the GPU, CPU fallback in workers**; detail over a
coarse host grid; unbounded detail by level; a **deterministic physical
level** (CPU f64, bit-exact, authoritative) and a **visual level** above it.
`procedural-unbounded-terrain.md` §3 (which still had the CPU as primary) is
the background.

### 4.1 Resources

| Resource | Size | Kind |
|---|---|---|
| Generated terrain meshes | as a streamed mesh (`mesh_cost`) | **same class** `TerrainMesh` |
| Generated drapes (imagery/biomes) | as a streamed drape (`drape_cost`), compressible | **same class** `Drape` |
| Host grid | a few MB (CPU), optionally a GPU texture | fixed reservation (or a streamed host) |
| Generator parameters, permutation/gradient tables | KB | fixed |
| GPU generation scratch | per dispatch | transient (CB4's `transient`) |
| Shader-only detail (normal/displacement octaves beyond the deepest level) | none resident | recurring time only |

### 4.2 Time

| Work | Est. (`procedural-unbounded-terrain.md` §3.7) | Kind |
|---|---|---|
| 65×65 mesh, CPU f64 | 0.5–1.5 ms single core (×2–4 with warp/erosion) | admitted, worker quota |
| 256² imagery, CPU | 10–40 ms | admitted, worker quota (dominant on CPU) |
| 256² imagery, GPU compute | < 0.2 ms | admitted, **GPU-work lane** |
| Shader detail octaves | per ground fragment | recurring |

### 4.3 Must never degrade

- **No hole in procedural ground.** The coarse levels (the host's own, or the
  pinned floor) are generated first and never evicted, exactly the floor of
  I1. Generated child active before the parent leaves (I9).
- **A tile is a function of (generator version, seed, level, x, y) only.**
  Quality never changes a tile's content — otherwise edges no longer share
  samples and caches lie. Quality acts only on *which levels are selected*
  (the SSE scale, as for streamed tiles) and on shader-only octaves.
- **The physical level is outside the budget.** `ground_height` at the
  physics level is computed on demand, bit-exact, independent of what is
  resident. The budget never decides physics.
- **But the shown ground must not contradict it.** If relaxation coarsens
  the display below the physics level near the player, objects float or
  sink by more than the cosmetic bound. Needed: a **local floor** — a pinned
  neighbourhood around declared points of interest whose level never relaxes
  below the physics level (a protected need, I11). Open question Q6.
- **Films are exact.** A film's generated tiles must be the same bytes on
  every node: the CPU path, or the GPU path baked once into the pack (GPU
  bits are per adapter). Batch never regenerates on the GPU at render time
  unless the digest is per adapter by contract.

### 4.4 Budget notions it needs

| Notion | How |
|---|---|
| **`Generated` tier / source** — new | `Tier` gains `Generated` (`tier.rs:13`): "nowhere held, but computable here". `Source` gains `Generate { gpu: bool }` with its own quota (`max_generations` per class for the CPU workers, like `max_local_reads` beside the network), and GPU generation in a frame lane. The presence oracle answers `Generated` for any resource of a procedural layer. |
| **Restore cost = regeneration time** | B1's `restore_cost(tier, class)` gets a `Generated` row (config constants per backend: GPU ≈ 0.2 ms, CPU 1–40 ms), next to `Stored` and `Remote`. GreedyDual-Size then evicts cheap-to-regenerate content before network content — the right order with no special case. |
| **Store as a cache of generation** | when CPU generation costs more than a local read (imagery: 10–40 ms vs ~1 ms), generated tiles may be written to the store; then `Stored` beats `Generated`. Same I14: nothing held is regenerated. |
| **GPU-work lane** | GPU generation is admitted per frame like an encode (§5.3); a GPU-generated resource needs no upload: `transfer = 0`, `staging = 0`, `transient` = scratch. |
| **Predictive generation (C)** | generation is cancellable and cheap on the GPU: predicted tiles take the lane's *spare* time only (band below Ancestor), never delay a current need. |
| **Determinism** | `decide` unchanged: generation durations are measured by the executor and never enter it; the restore costs in `decide` are config constants (B1 already says so). |

---

## 5. Execution budget: GPU time per frame

### 5.1 What consumes a frame's GPU time

| Consumer | Recurring or admitted | Knob |
|---|---|---|
| Ground draw: terrain + drapes, overdraw of fallbacks and held tiles | recurring | SSE scale (what is selected), render scale |
| Aerial perspective + ground light per fragment | recurring | march steps, K lights, render scale |
| Sky pass, discs, stars | recurring | render scale |
| Sky-view tables | recurring | steps (size fixed) |
| Frame light, metering reduction | recurring | **none** (never scaled) |
| Overlays, 3D Tiles content | recurring | SSE scale |
| Texture uploads | admitted | byte lane (exists) |
| GPU encode BC7/ASTC (9–10 ms per 256² BC7 tile, `gpu-compressed-textures.md` §9.1) | admitted | encode lane (CB5) |
| Procedural generation | admitted | generation lane |
| Voxel brick build, cascade refill | admitted | volume lane |
| Cloud march | recurring | steps, resolution, amortisation |

The split matters: **admitted** work can wait a frame (it has a plan order and
a fallback already drawn); **recurring** work cannot wait, it can only get
cheaper.

### 5.2 Measuring it

| Backend | Signal | Caveat |
|---|---|---|
| Native wgpu with `TIMESTAMP_QUERY` (Vulkan, DX12, Metal) | timestamps around each pass, resolved once per frame, read a frame or two late | Apple GPUs: at pass boundaries only (`TIMESTAMP_QUERY_INSIDE_ENCODERS` missing); per-pass is enough. The viewer already asks for both features (`tuile-viewer/src/app/setup.rs:209`), the encoder already measures (`compress.rs:241–357`). |
| WebGPU with `timestamp-query` | the same | optional feature; browsers quantise timestamps (Chrome: 100 µs by default). A slow controller averaging over ~1 s of frames is unaffected; per-pass costs below the quantum are read as a mean over many frames. |
| WebGL2 | **no GPU timer** (wgpu's GL backend has no timestamp queries; `EXT_disjoint_timer_query_webgl2` is rarely exposed) | frame pacing only: the presentation interval (rAF deltas), the fraction of frames that missed the display period, submit → `on_submitted_work_done` wall time. Coarse: the controller is slower and keeps more headroom. |
| Everywhere | presentation interval | the ground truth of "did the frame make it", and the only one that sees thermal throttling as it happens |

Measurements are the executor's. They go to `FrameRecord`, metrics and MCAP
(CB1 already adds `encode_gpu_us`); they **never** reach `decide`.

### 5.3 Admitted work: lanes in `FrameDeadline`

CB5 gives `FrameDeadline` an encode lane: integer `encode_blocks`, a token
bucket that may go into debt, no overtaking, whole swap groups, a measured
ns/block on the executor side. That is already a GPU-time lane in a unit the
core can count deterministically. The generalisation costs nothing if CB5
names it so:

```rust
// tuile-budget, sketch — CB5 lands the first field only.
#[non_exhaustive]
pub struct GpuWork {          // integer units the plan can count
    pub encode_blocks: u64,   // CB5
    // pub generated_texels: u64,  // procedural, later
    // pub brick_voxels: u64,      // volumes, later
}
pub struct FrameDeadline { pub bytes: Bytes, pub time: Micros, pub work: GpuWork }
```

- **One GPU-time bucket per frame, several units.** The executor converts
  each unit to time by its own measured rate (ns per block, per texel, per
  voxel) and admits plan items in plan order while the bucket allows. The
  core's `take_with` stays a pure function of the queue, the costs and the
  bucket's carry: testable, the same in the wgpu pump, the web page and a
  Hydra executor.
- **The plan's deterministic cap** stays in `decide`: CB5's
  `encode_blocks_per_step` generalises to a `GpuWork` cap per step.
- **Recurring work is the bucket's first claim**: the lane's share per frame
  is what the recurring passes leave of the target frame time (measured,
  executor-side), so a heavy cloud frame admits fewer encodes — never the
  reverse.

### 5.4 Recurring work: quality as a continuous dimension

The budget already trades quality for memory continuously: `relax`
(`engine.rs:2273`) moves `sse_scale` at a rate per second, deterministic
because time is an input. GPU time needs the same, with one difference: its
signal is measured, so the controller cannot live in `decide`.

**The shape:**

1. A **controller on the host** (a pure function in `tuile-budget`, its own
   module, run by the executor): inputs = the measured series (GPU time per
   pass or pacing, recorded in MCAP), the target (display period × share,
   lowered by thermal state), the current quality vector; output = a new
   quality vector, rate-limited.
2. Its output is a **recorded input of `decide`**, like `set_thermal` or
   `retarget`: e.g. `Budget::set_quality(Quality { sse_floor_milli })`. The
   other knobs (march steps, cloud resolution) are executor/renderer
   parameters set from the same vector and recorded on the same channel.
3. **Replay is deterministic**: a session replays from its MCAP with the
   recorded quality inputs, bit for bit; the controller itself, being a pure
   function of recorded measurements, replays too.

**Continuous, never stepped** (the "continuous rendering" rule):

- One scalar per knob family `q ∈ [q_min, 1]`, moved by a slow
  proportional-integral step with a **dead band** (hysteresis as "do
  nothing inside ±10 % of the target") and a **rate limit** (e.g. at most
  5 % per second), evaluated every frame. Hysteresis here is a dead band on a
  continuous signal, not a threshold that flips a mode.
- Every knob is a continuous function of `q`:
  - SSE floor: thousandths, already continuous (`sse_scale_milli`);
  - march steps: a real-valued step count — the last step weighted by its
    fraction, or a continuous step length — so 23.4 steps is a valid
    setting and the haze moves by a fraction of a level per frame;
  - render scale: a continuous viewport inside a fixed-size target (no
    reallocation); note it changes the SSE (the viewport enters it), which is
    why it must be a recorded input;
  - cloud steps, amortisation fraction, shader detail octaves (faded by
    `smoothstep` as in `procedural-unbounded-terrain.md` §3.3);
  - table *sizes* (sky-view 192×108, LUTs) are **not** knobs: resizing
    reallocates and steps. They are fixed per device profile.
- **Never touched by `q`:** the frame light / exposure, the floor, coverage
  (I1), anything a film needs (Batch: `q = 1` always, no controller).

### 5.5 Thermal throttling on phones

- The phone's thermal state is a slow, four-level OS signal; throttling shows
  first as **longer GPU times at the same work** (lower clocks). A
  GPU-time controller sees it directly and lowers `q` — no special path.
- The thermal state should lower the **target**, not the quality: Serious →
  the frame-time target drops (e.g. share of the period from 70 % to 50 %,
  or 60 → 30 Hz), Critical → further; the controller then moves `q`
  continuously toward it. Headroom below 100 % of the period is itself the
  thermal policy: a GPU kept at 100 % heats.
- Today's thermal response in `decide` is **stepped**: quotas and uploads
  divided by 1/2/4 (`engine.rs:312–318`) and an SSE floor jumping to 1.5 or
  2.0 (`engine.rs:1443–1449`). Under the continuous rule, the SSE floor
  should come from the controller's ramped output instead; the quota
  divisions are admission limits (no picture step) and can stay. Changing
  the floor moves thermal pattern goldens: Laurent's decision (Q3).

### 5.6 Recommendation

**Yes, an execution budget — in four stages, each behind measurements:**

| Stage | What | When | Changes `decide`? |
|---|---|---|---|
| **E1 — see it** (minimal first step) | per-pass GPU time (timestamps where available; pacing otherwise) and the presentation interval in `FrameRecord`, metrics `tuile_gpu_pass_us{pass}`, MCAP, HUD; run `frame_light_cost` on Mac/iPhone/WebGL2 | **folded into CB1** (which already adds `encode_gpu_us` and per-phase timings from the harvest) | no |
| **E2 — the first lane, named generically** | CB5's encode lane as `GpuWork { encode_blocks }`, one GPU-time bucket per frame, the recurring passes' measured cost subtracted from the share | **CB5/CB6**, no extra PR | no (Off byte-identical) |
| **E3 — the quality controller** | the pure controller module, `set_quality` input, continuous knobs (SSE floor first, then march steps, render scale), thermal as a target; all adjustments on an MCAP channel | **after A2.7** (MCAP replay is what makes it deterministic), beside **B6** (same machinery: a slow controller with hysteresis recorded in MCAP) | yes, through a recorded input only |
| **E4 — more lanes** | `generated_texels`, `brick_voxels` | with `tuile-procedural`'s GPU path and with voxels | no (lane fields) |

Not recommended: a GPU-time number inside `decide`'s cost model, or a
per-resource GPU-time charge in the ledger. Draw cost depends on pixels and
overdraw, not on a resource; it is controlled through what is selected (SSE),
which `decide` already owns.

---

## 6. What changes in `tuile-budget`, and when

### 6.1 Table

| Change | Field / type | Needed by | Slot in the approved order |
|---|---|---|---|
| Per-pass GPU time, presentation interval in `FrameRecord`, metrics, MCAP | `observe.rs:109` | all (E1) | **CB1** (extend its scope by one line) |
| Generic GPU-work lane | `FrameDeadline.work: GpuWork` (`#[non_exhaustive]`), `take_with` over units; `BudgetConfig` cap per step as `GpuWork` | encode now; procedural, volumes later | **CB5** (naming only) |
| Session-wide fixed reservations, outside classes | `BudgetConfig.reserved_fixed: Bytes` (and decoded), subtracted in `retarget` and `eff()` before the split; a `set_reserved(kind, bytes)` input for resolution-dependent ones (targets) | atmosphere LUTs, HDR target, cloud noise/cascades | **CB4** (beside `reserved: PerClass<Bytes>`; same red tests) |
| `Cost` fields | CB2's `transfer`, `transient`, `encode_blocks` cover generation too: GPU-generated = `transfer 0`, `staging 0`, `transient` scratch | procedural GPU | **CB2** as planned; check the GPU-generated case in its proptest |
| Quality input | `Budget::set_quality(Quality { sse_floor_milli })`, recorded; replaces the stepped thermal SSE floor | E3 | **after A2.7, with B6** |
| `Tier::Generated`, `Source::Generate { gpu }`, `max_generations` quota, presence answers `Generated` | `tier.rs`, `config.rs` | procedural | **when `tuile-procedural` reaches host integration** (its PR 3); independent of B |
| `restore_cost` row for `Generated` | B1 table | procedural | **B1** (design the table with the row; constants filled when procedural exists) |
| Spare-lane prefetch for generation | C's quotas include a GPU-work share | procedural | **C3** |
| More classes | `PerClass` is `[T; 4]` with an exhaustive `Class` (`types.rs:21–64`, `Pool::NONE` `config.rs:86`): grow to N, or a side table for extra classes | voxels (`Volume`), maybe clouds | **after O1.1** (dense arena with side columns) — wait for voxels |
| Owned (dirty) resources: dirty bit, write-back quota, infinite restore until written | `state.rs`, B1 | destructible voxels | **wait**; design after B1/B2 |
| 3D keys | none: `TileId` is opaque, `Need.parent` gives the chain | voxels | nothing to do |
| Local floor around points of interest | a protected need (I11) the host declares | procedural physics | **wait**; decide with Q6 |

### 6.2 What can wait

Everything voxel (classes, owned resources) waits for a voxel consumer;
clouds in their 2.5D form need only reservations and recurring time. The
`Generated` tier waits for `tuile-procedural`'s host integration. The
controller (E3) waits for A2.7. What should **not** wait, because it is
nearly free inside PRs already planned: E1 in CB1, the generic naming in
CB5, fixed reservations in CB4.

### 6.3 New invariants to add to the spec when each lands

| ID (proposed) | Invariant |
|---|---|
| I19 | **Reserved before split**: Σ fixed reservations + Σ class hard limits ≤ the device total after `retarget`; a resize that grows a reservation shrinks the pool in the same step, without a hole (I8). |
| I20 | **Quality is an input**: the decision stream is a function of the recorded quality inputs; with the controller off (Batch, tests), `q = 1` and the goldens are unchanged. |
| I21 | **Continuous quality**: per frame, every quality knob moves by at most its rate limit × frame time; no knob takes a value by a jump. |
| I22 | **Generation is never refetched, nothing held is regenerated** (I14 for `Generated`). |
| I23 | **No owned resource is lost**: an edited resource leaves the GPU only after its write-back is acknowledged. |

---

## 7. Risks and open questions for Laurent

| # | Question | Leaning |
|---|---|---|
| Q1 | Accept the execution budget as staged above (E1 in CB1, E2 in CB5, E3 after A2.7 with B6)? | yes |
| Q2 | Does the quality controller live in `tuile-budget` (a pure module, replayable) or in each executor? | in `tuile-budget`: one rule for wgpu, web, bevy, Hydra — the reason `FrameDeadline` lives in the core |
| Q3 | Replace the stepped thermal SSE floor (`engine.rs:1443–1449`) by the controller's ramped output? Moves thermal pattern goldens. | yes, in E3 |
| Q4 | Is the atmosphere's per-fragment march a quality knob at all, or fixed per device profile? It is the dominant mobile cost by estimate, but changing it moves the haze. | measure first (E1); if it is < 10 % of a phone frame, keep it fixed |
| Q5 | Render scale (dynamic resolution) as a knob: it changes the SSE and the picture's sharpness, and the web canvas size. | later than SSE floor; recorded input |
| Q6 | Procedural physics vs display: pin a local floor at the physics level around declared points of interest, so relaxation never shows ground the physics contradicts? | yes, as a host-declared protected need |
| Q7 | Films with GPU-generated procedural content: bake the generated tiles into the pack (bit-exact across nodes) or accept per-adapter digests? | bake into the pack |
| Q8 | Voxel bricks for destructible ground in the ground pool (sharing with terrain and imagery), or a pool of their own? | in the ground pool with a floor; simulate as D-A3 was |
| Q9 | Owned resources (edits) are a first in the budget: write-back belongs to the budget (a quota and a state) or to the store? | the budget tracks the dirty state and quota; the store does the write |

Risks:

- **Measurement quality on the web.** WebGL2 has no GPU timer; a controller on
  pacing alone may oscillate between "made it" and "missed". Mitigation:
  wider dead band, slower rate, and a test on recorded pacing traces.
- **Coupling recurring cost and admission.** If the lane's share is "what the
  recurring passes leave", a costly view starves encodes and generation;
  holes cannot appear (fallbacks stay drawn) but settling slows. Mitigation:
  a minimum share per frame (the "first item always goes" rule of I18).
- **Golden churn.** Any change to thermal or a new input must keep Off and
  Batch goldens byte-identical; the quality input must default to the
  identity.
- **Scope creep in CB.** E1 and E2 are only naming and recording inside CB1
  and CB5; anything more (the controller) stays out of CB to keep CB8's
  acceptance clean.
- **PerClass growth** costs every per-class loop and the `Entry` size; doing it
  before O1.1's arena would be paid twice.
