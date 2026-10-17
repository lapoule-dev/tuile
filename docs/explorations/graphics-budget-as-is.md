<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — AS-IS (desktop and iOS viewers)

Read-only study. All references are `crate/path:line` on `feat/ios-app` @ `a6691de`
(which contains `origin/main` @ `07be3d0`). The iOS worktree also holds uncommitted
edits (notably `tuile-wgpu/src/pump.rs`: `drain`, `send_view`); they do not change
anything below. Nothing was built or run. "Confirmed" = visible by reading the code;
"Hypothesis" = plausible mechanism that needs the reproduction given.

## 0. One-paragraph summary

There is one budget, on the geometry server (`ResidentCache`), counted in **decoded CPU
bytes** and, more decisively, in **tile count**. The GPU has no budget of its own: it holds
exactly what the server has sent and not yet evicted, plus an unbounded FIFO of decoded
content waiting for uploads. Requests, decode, delivery and upload are four independent
queues with four different orderings. Nothing couples the terrain level of a tile to the
imagery level it wears, and the imagery level is frozen at load time.

## 1. The chain, end to end

### 1.1 Selection (tuile-core/src/traversal.rs)
- Screen-space error: `screen_space_error` 157-159. The cull frustum is widened by
  `CULL_MARGIN = 0.25` (97, applied 121-126). That is +25 % field of view, about +55 % tiles.
  It is a latency compensation, not a prefetch ring.
- `Config::interactive_globe` 388-399: `maximum_screen_space_error = 2.0`, 64 fetches,
  4 GiB, 16 000 tiles. Defaults are at 402-422. `refine_hysteresis = 0.15` and
  `loading_descendant_limit = 20`.
- Hysteresis 906-915 is keyed on `rendered_last`. That is the **server's previous pass**, and
  a pass runs on every load completion (runtime.rs 270-276). It is not the consumer's
  previous frame.
- REPLACE hold-until-ready: 1019-1174. A hold is cut loose after 20 waiting descendants
  (1125-1141), unless the tile was drawn in the previous pass.
- Preload ancestors are on, at the `Preload` priority group (929-931). Preload siblings are
  off (825-834).
- The priority within a group is `(1 - cos off-axis) × distance` (847-858). There is no SSE
  term and no age term. Sort: group, then priority, then id (582-587).
- The traversal's residency input is `ResidencyView` (428-454) = **server-loaded**, not
  consumer-drawable.

### 1.2 Requests (tuile-core/src/runtime.rs)
- The run loop alternates client and load events (214-243). It re-traverses on every camera
  burst and on every arrival (254-276).
- Each pass, in order:
  1. coarse pyramid first (752-771);
  2. then `out.requests` in order, up to `maximum_simultaneous_fetches` (775-784);
  3. then epoch refreshes with whatever slots are left (787, 848-876).
- There is no request queue. The full wish list is recomputed every pass, and the in-flight
  set is the only state.
- **Cancellation** (722-745) fires on `camera_moved`. That flag is set by **every**
  `ViewerState` (365-372), and `ContentPump::advance_while` sends one **every frame**
  (pump.rs 389-393). In practice, in-flight loads absent from the current wish list are
  aborted at frame rate. **Confirmed.**
- `Cancel` from the client: 386-392. It is never sent by the pump.
- A first-load failure, after transport retries (native-fetchers lib.rs 109-118: 3 attempts,
  30 s timeout at 367), **ends the session** (runtime.rs 1103-1117, by design for film).
  This also applies to the interactive viewer. What the viewer does then is **unverified**.

### 1.3 Decode
- `tuile_core::offload::ThreadPool`: unbounded `std::sync::mpsc` FIFO (offload.rs 113).
- Jobs are not cancellable: `run` (193-206) only drops the receiver, so an aborted load's
  decode still runs. **Confirmed.**
- Decode threads: desktop = cores-1; iOS = cores-2, between 1 and 3 (profile.rs 126-129, 164).
- Planetary in-process caches are FIFO by count:
  - decoded imagery, 512 entries (planetary lib.rs 1712, struct at 427-483);
  - meshes, 512 entries;
  - fill meshes, 256 entries.

### 1.4 CPU residency (tuile-core/src/cache.rs)
- An entry is charged on arrival: geometry `byte_size`, plus imagery refcounted once per
  `ImageryCoord` with a 4/3 mip factor (147-191; content.rs 179-181).
- Sweep: from 90 % to 80 % of **either** ceiling (31, 38, 345-355). Victims are sorted
  deepest level first, then least recently used, then id (424-428). Every sweep collects and
  sorts the whole map (385-428).
- Protected, and never evicted even when over budget:
  - the frontier;
  - every ancestor to the root;
  - every requested tile, preloads included;
  - `awaiting`;
  - anything touched this pass (592-640).
- Imagery at or above `pinned_level` is never freed (179-189).
- `Trim { budget_bytes }` → `release_beyond` (300-334) drops everything unprotected at
  once, when over budget. It is used by memory warnings (viewer lifecycle.rs 154-155,
  gestures.rs 111-114) and by bake between frames (bake session.rs 1254-1262).

### 1.5 Delivery
- `InProcessStream`: `mpsc::unbounded` both ways (protocol.rs 272-307). **Confirmed.**
- `Content` is sent once per residency (protocol.rs 124-130). The server marks the tile
  resident at **load** time (runtime.rs 1064), not at upload.
- `Select` carries the full protected closure as `ancestry` on every pass (678-692).

### 1.6 Upload (tuile-wgpu/src/pump.rs)
- `pending: VecDeque` is unbounded and **FIFO** (28, pop at 267). Fills are `push_front` (480).
- Content that is no longer selected is still uploaded in arrival order. Only `Evict` purges
  it (530).
- Budget:
  - desktop: 8 uploads/frame (`UPLOADS_PER_FRAME` 89);
  - iOS: 4 uploads and 6 ms (`UploadBudget` profile.rs 66-79, 165-168; wired in frame.rs
    142-164).
  - It is not counted in bytes.
- Per upload:
  - `prepare` on the frame thread (prepare.rs 277-410). CPU mip chain on first use of a
    texture (422-466). A CPU copy of every mesh is kept for re-stitching (`source`, 99,
    372-382). An 8 272-byte uniform per tile (27).
  - `live_by_level()` over the **whole** imagery pool, under its lock (pump.rs 287-297),
    O(textures). **Confirmed.**
  - `ImageryPool::get_or_insert_from` counts all live entries on every insert (raster.rs
    1111-1122), O(entries). **Confirmed.**
- The `Ack` is sent only after upload (317-320).

### 1.7 GPU residency and eviction
- Nobody on the consumer side decides. `prepared` only shrinks on `Evict` or `Retire`
  (490-531).
- `gpu_bytes` (59) and the imagery pool bytes are observed, never enforced.
- Imagery textures die by `Arc` refcount (context.rs 60-153; raster.rs 1052-1143).
- Units: the server counts decoded bytes and tiles. The GPU really pays about 1.35× the
  decoded bytes, plus a CPU mesh copy and uniforms (traversal.rs 377-381 says so).

### 1.8 Fallback and ancestors
- The consumer's `ancestry` map is never pruned (pump.rs 51-56, extended 441). It grows with
  every tile ever named. **Confirmed.**
- `walk` (894-951): each selected tile either draws itself or its nearest prepared ancestor.
  The climb is never refused.
- Stand-ins (`Fill`): sent for every selected, un-acked tile on every pass (runtime.rs
  435-500). They are upsampled ancestors with cache-only imagery (planetary lib.rs
  1378-1477) and often "THIN" (1466-1476).
- Backdrop: shell plus the complete `BASE_LEVEL` (frame.rs 272-288). This scans all prepared
  tiles twice per frame (`at_level`, pump.rs 748-753).

### 1.9 Draw (tuile-wgpu/src/renderer.rs)
- Order (`paint` 522-539):
  1. backdrop: no depth;
  2. Solid: depth write, `Less`, stencil `Replace` = OWNED (142-150, 192-197);
  3. Fallback: depth `Less`, **no depth write**, stencil `NotEqual` OWNED (152-157, 401-421).
- Skirts are part of every mesh's index buffer (terrain mesh.rs 270-310). They hang 5 ×
  geometric error, extended to the depth of a tile 4 levels coarser and capped at the level-6
  depth (≈ 6 km) (132-141, 178-199). They carry the edge vertex's UV (297).
- Stitching welds only the edges between exact tiles. Edges beside a fallback are left "over
  their skirt" (pump.rs 137-141), and edges slide over 12 frames when neighbours change
  (`STITCH_SLIDE_FRAMES` 79).

### 1.10 Imagery level (tuile-planetary)
- The viewer sets one global texel target from **altitude** each frame (frame.rs 113-115 →
  `ImageryDetail` planetary lib.rs 297-312).
- At **load time** a tile takes `max(level matching its geometric error, level for the
  altitude target)` (1043-1060). `imagery_boost_cap` defaults to `u32::MAX` (618).
- The loader epoch = imagery generation only (1495-1497). An altitude change never
  re-drapes a resident tile. **Confirmed.**

## 2. Where the budgets live

| Constant | Desktop | iOS (`Profile::handheld`) | Unit | Enforced? |
|---|---|---|---|---|
| `resident_budget_bytes` | 4 GiB (traversal.rs 394) | usable/5 clamped 160–768 MiB (profile.rs 150) | decoded CPU bytes | Soft: protected set may exceed it without limit |
| `resident_tile_limit` | 16 000 (395) | bytes/256 KiB, ≥1024 (154) | tiles | Soft, same |
| `maximum_simultaneous_fetches` | 64 | 16 | loads | Hard |
| `pinned_level` | 4 (profile.rs 123) | `BASE_LEVEL` (160) | level | Hard (never evicted) |
| uploads per frame | 8 | 4 and 6 ms | tiles / ms | Hard, but not bytes |
| decode threads | cores-1 | 1–3 | threads | Queue unbounded |
| foyer store | 256 MiB RAM / 4 GiB disk | 32 MiB / 768 MiB | bytes | Hard (foyer) |
| HTTP client cache (native-fetchers) | 256 MiB / 4 GiB | 16 / 128 MiB (`handheld`) | bytes | Hard |
| planetary caches | 512/512/256 entries | same | entries | Hard, FIFO |
| `Trim` on memory warning / background | — | budget/2, budget/4 (217-225) | decoded bytes | One-shot |
| GPU bytes | none | none | — | **Not enforced anywhere** |

- **Coupling.**
  - CPU and GPU are coupled only implicitly: the GPU mirrors server residency.
  - Request concurrency is independent of both.
  - The upload budget is independent of the request rate. Nothing slows fetching when
    `pending` grows.
- **Unbounded queues.**
  - `InProcessStream` both ways;
  - `ContentPump.pending`;
  - `ContentPump.ancestry`;
  - decode `mpsc`;
  - `out.requests` (the wish list itself);
  - per-pass `Select.ancestry` allocations.
  - PR #60 also lists `tuile-viewer` std channels (desktop.rs 288, globe.rs 233); their
    volume is low.

## 3. Symptoms and mechanisms

### S1 — skirts visible when tiles refresh
- **M1.1 (confirmed by reading).** Skirt fragments of exact tiles stamp the OWNED stencil
  (renderer.rs 145-150). Fallback ground drawn later in front of such a skirt passes depth
  but fails stencil (152-157). The skirt therefore shows **through** the coarser ground
  meant to hide it.
  - This needs a fine tile next to an un-uploaded neighbour, which is exactly the refresh
    state.
  - Skirts are up to kilometres deep, so the wall is large.
  - It is textured with stretched edge texels (mesh.rs 297): vertical streaks.
- **M1.2 (confirmed by reading).** The fallback pass does not write depth (192). A fallback
  tile's skirts are appended last in its index buffer (mesh.rs 303-308). They are drawn after
  its own surface, and over the neighbouring fallback surfaces, without self-occlusion.
- **M1.3 (hypothesis).** Stitching ignores exact-to-fallback edges (pump.rs 137-141). During
  the 12-frame slide an edge moves, and the crack is closed only by the skirt, which is
  visible.
- **M1.4 (hypothesis).** Terrain and imagery levels are decoupled (1.10). Two neighbours of
  equal terrain level may wear imagery of different levels, so the crack shows a texel
  colour mismatch on the wall.
- Z-order: skirts are drawn in the same pass and order as the surface (no separate pass).
- **What a reproduction must decide:** M1.1/M1.2 versus M1.3. A diagnostic colouring of
  skirt fragments settles it (§4).

### S2 — loading slows down over time
- **M2.1 (confirmed by reading; impact to measure).** Cancellation runs at frame rate,
  because every `ViewerState` counts as a move (runtime.rs 365-372, 722-745).
  - Hold flapping (cut-loose 1125-1141, held-but-drawn 1088-1102) drops tiles out of the
    wish list for one pass. They are aborted and requested again, which is the livelock the
    comment at 701-716 describes, only not prevented.
  - Combined with M2.2, each abort still costs a full decode.
- **M2.2 (confirmed by reading).** Decode jobs are not cancellable and the queue is unbounded
  FIFO (offload.rs 113, 193-206).
  - The in-flight cap counts server slots, not decode work.
  - During motion the decode pool fills with orphaned work. Fresh loads wait behind it, so
    the load latency p95 climbs.
- **M2.3 (confirmed by reading).** Per-upload O(N) scans grow with the session:
  - `live_by_level` (pump.rs 287-297);
  - `ImageryPool::live_count` (raster.rs 1111);
  - `pending.retain(Vec::contains)` on Evict (530);
  - `pending.iter().any` per Fill (470);
  - per-frame `at_level` ×2 (748);
  - `coplanar` O(F×E) (639-646).
  - On iOS the upload budget is **time-based** (6 ms). As N grows, fewer uploads fit per
    frame, `pending` grows, and the picture lags further behind.
- **M2.4 (hypothesis).** Cache thrash at the cap. On iOS (160–768 MiB, ≥1024 tiles) the
  protected set, which includes every request and preload, can exceed the 80 % low-water
  mark. Every insert then re-sorts the whole map (cache.rs 375-428) and evicts little.
  Unprotected but soon-needed tiles (the camera's previous view) are evicted and refetched.
- **M2.5 (confirmed, magnitude unknown).** `ancestry` grows for ever (pump.rs 56): about
  40–60 B per entry including the hashmap, so tens of MB per hour of flight. The server-side
  `Select.ancestry` is rebuilt per pass.
- **M2.6 (hypothesis).** Priority inversion in uploads: FIFO `pending`, stale content first,
  and fills always jumping ahead (480) and consuming upload slots.
- **M2.7 (hypothesis).** Stalled HTTP requests hold slots up to 30 s × 3 attempts. Disk-cache
  growth is bounded by foyer, so it is unlikely.

### S3 — coarse tiles on rotation and zoom-out
- **M3.1 (confirmed by reading).** The imagery level is frozen at load from the global
  altitude (1.10).
  - A tile loaded during a zoom-out keeps coarse imagery when the camera comes back.
    Nothing re-drapes it until it is evicted.
  - A tile loaded low keeps an oversized mosaic when the camera climbs, which wastes the
    budget.
- **M3.2 (confirmed by reading).** "Selected" means server-loaded. The consumer still needs
  queue, upload and ack. Meanwhile it draws stand-ins (cache-only, often thin imagery) or an
  ancestor.
  - At 4–8 uploads per frame, a 180° turn that needs hundreds of tiles shows coarse ground
    for seconds.
  - Stand-ins consume the same upload slots (480).
- **M3.3 (confirmed by reading).** Eviction ignores visibility. Deepest-first then LRU
  (cache.rs 424-428) evicts exactly the fine tiles of the view just left. A rotation back
  must refetch them; the store makes that cheaper, not free.
- **M3.4 (confirmed).** No prefetch ring beyond `CULL_MARGIN`; preload siblings are off.
- **M3.5.** Hysteresis exists, but is keyed on the server pass rather than the drawn frame.
  Its effect on S3 is unknown.
- Texture budget per tile versus per byte: the CPU charges imagery once per coord in bytes
  (correct). It keys on coord without source (cache.rs 101), which undercounts during an
  imagery switch.

## 4. Reproductions and instrumentation

### 4.1 Instrumentation to add first: a `BudgetObserver`
- Following the `Observer` pattern of `tuile-film-native/src/observe.rs:146` — watch-only,
  takes nothing back — one observer is called once per frame by the pump.
- Fields:
  - frame and generation;
  - drawn exact, fallback and stand-ins; `lost`, `worst_gap`;
  - **underdraped** (tiles whose `sharpest_imagery_level` is below the level needed by their
    SSE − 1);
  - `pending` count and bytes; uploads count, bytes and µs;
  - prepared count, GPU bytes and imagery bytes;
  - ancestry length; decode queue depth (new gauge);
  - loads started, completed and cancelled this frame; in flight; `load_seconds` p50/p95;
  - traversal µs; passes per frame;
  - skirt pixels (diagnostic view only).
- Two sinks: JSONL (`TUILE_BUDGET_LOG=path`), and the existing `/metrics` on desktop.

Diagnostic view: flag skirt vertices, e.g. a fourth UV component or a dedicated attribute,
and paint skirt fragments magenta. The readback through `TUILE_TRACE` counts them.

### 4.2 Reproductions

| Symptom | Desktop | iOS | Pass/fail metric |
|---|---|---|---|
| S1 | `tuile-tape zoom` and `orbit` over the Alps, Pyrenees or Mont Blanc at 3–10 km, replayed with `TUILE_REPLAY`, diagnostic view on | `TUILE_TOUCH_SCRIPT` with a pinch-out then a pinch-in over relief | Magenta pixels per frame. Expected > 0 while `coarser > 0` if M1.1 holds |
| S2 | `tuile-tape pyrenees <path> 60` (one hour), log per minute | `TUILE_TOUCH_SCRIPT=tour` looped for 30 min | Slopes over time of `load_seconds` p95, `upload_seconds` per upload, decode queue, cancel/complete ratio while still; ancestry length |
| S3 | orbit at 2 km with heading +180° in 2 s, then stop; `zoom` out to 200 km then back to 2 km | same with a touch script | Frames until `coarser==0 && stand_ins==0 && underdraped==0` after the stop; underdraped stays > 0 if M3.1 holds |

**Headless tests to write before any fix**, in `tuile-wgpu/tests`, following the pattern of
`fast_zoom_never_empties.rs` and `no_two_surfaces.rs`:
- `skirt_never_shows_through_fallback`: an exact tile beside an un-uploaded sibling, over
  synthetic relief. Expected to fail today.
- `still_camera_cancels_nothing`: N frames of an identical `ViewerState`; assert
  `loads_cancelled` delta == 0. Expected to fail today.
- `zoom_back_redrapes_sharp`: zoom out, then back; assert underdraped == 0 within K frames.
  Expected to fail today.
