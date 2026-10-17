<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# GPU streaming budget: best practices for a globe / 3D Tiles engine

Independent research note, written to challenge a proposed budget core for tuile
(wgpu, Mac + iOS, quantized-mesh terrain with skirts, draped raster imagery,
3D Tiles meshes). Date: 2026-10-17.

**How to read this.** Section 1 covers what production engines actually do, from their code where
possible. Section 2 covers game and texture streaming. Section 3 covers policies. Section 4 covers
terrain and imagery specifics. Section 5 covers testing. Then come the comparison table (6), the
anti-patterns mapped to the owner's symptoms (7) and the evaluation checklist (8).

**Licences and porting.** Nothing here is ported code. Identifiers and a few
short phrases are cited for attribution only. The licences of the sources:

| Source | Licence |
|---|---|
| CesiumJS | Apache-2.0 |
| cesium-native | Apache-2.0 |
| NASA-AMMOS 3DTilesRendererJS | Apache-2.0 |
| MapLibre GL JS | BSD-3-Clause |
| deck.gl | MIT |
| OpenLayers | BSD-2-Clause |
| iTowns | CeCILL-B / MIT |
| wgpu | MIT / Apache-2.0 |

Vendor documentation (Apple, Khronos, Epic, Unity) is cited, never copied.

**Versions read.**

| Code | Version |
|---|---|
| Local CesiumJS clone | `11f203f` (2026-06-12) |
| Local cesium-native clone | `bfc2c57` (2026-06-03) |
| MapLibre `src/tile/*.ts` | `main`, fetched 2026-10-17 |
| 3DTilesRendererJS `src/core/renderer/**` | `master`, fetched 2026-10-17 |
| wgpu | 29.0.4, from the local cargo registry |

---

## 1. How production geospatial engines manage streaming budgets

### 1.1 CesiumJS: 3D Tiles path (`packages/engine/Source/Scene/Cesium3DTileset.js`)

- **The budget is in bytes and has two thresholds.**
  - `cacheBytes` defaults to 512 MiB. It is the level the cache is trimmed down to, but only by
    evicting tiles that this frame does not need.
  - `maximumCacheOverflowBytes` adds another 512 MiB of headroom that the current view may use
    (lines 79–80, 248–262).
- **Overflow degrades the LOD; it never punches holes.**
  - `processTiles()` (l. 2898–2956) stops processing (GPU upload) as soon as
    `totalMemoryUsageInBytes > cacheBytes + maximumCacheOverflowBytes`.
  - It then raises `memoryAdjustedScreenSpaceError` by ×1.02 per frame. It lowers it by ÷1.02 per
    frame only once usage is back below `cacheBytes`.
  - The band between the two thresholds is a **hysteresis band**: in it the SSE neither rises nor
    falls.
  - The adjustment is per frame and multiplicative, so it ratchets and its speed depends on the
    frame rate.
- **Eviction is LRU with a per-frame sentinel** (`Cesium3DTilesetCache.js`).
  - At the start of each frame (`reset`), the sentinel moves to the tail.
  - Every tile touched this frame is spliced to the right of the sentinel.
  - `unloadTiles` walks only from the head up to the sentinel, so a tile used this frame can
    never be evicted. Nothing in the needed set is evicted, even when over budget.
- **Request cancellation is aggressive.**
  - `cancelOutOfViewRequests` (l. 2743) cancels any in-flight request whose tile was not touched
    in the latest frame (`frameNumber - _touchedFrame >= 1`).
  - The source itself notes that this is frame-rate dependent.
- **Request scheduling** (`Core/RequestScheduler.js`) has three layers:
  - a global cap of `maximumRequests = 50`;
  - `maximumRequestsPerServer = 18`;
  - a bounded priority heap with `priorityHeapLength = 20`. When the heap is full, the
    lowest-priority request is refused and retried in a later frame.
  - Requests are typed (`TERRAIN`, `IMAGERY`, `TILES3D`, `OTHER`). They are sorted by priority
    before issue, explicitly "to make it less likely that requests will be cancelled after being
    issued" (`requestTiles`, l. 2771).
- **Priority is a packed base-10 composite.** `Cesium3DTile.updatePriority`, l. 2399, encodes:

  ```
  preloadFlight | foveatedDefer | foveated(4) | progressiveResolution | distance-or-reverseSSE(4) . depth
  ```

  Smaller is better. Two details matter:
  - **The siblings of a replace-refined parent share one "priority holder".** They are loaded as
    a set, because the parent cannot refine until all of them are ready
    (`Cesium3DTilesetBaseTraversal.js`, l. 153–175).
  - Several optimisations reshape this priority:
    - foveated SSE relaxes the edges of the screen until the centre is loaded, with a 0.2 s delay
      after the camera stops;
    - `progressiveResolutionHeightFraction = 0.3` loads a low-resolution pass first;
    - `cullRequestsWhileMoving` skips requests that will be stale on arrival;
    - `preloadFlightDestinations` preloads tiles at the destination of a camera flight.
- **Replace refinement without holes.** In the base traversal, a REPLACE parent refines only when
  all its children are loaded. Invisible children are also loaded for this purpose
  (`checkRefines`, l. 106–147).
  - Empty tiles are exempt.
  - `skipLevelOfDetail` (off by default) trades this guarantee for speed (`skipLevels`,
    `skipScreenSpaceErrorFactor`, `loadSiblings`, `immediatelyLoadDesiredLevelOfDetail`).

### 1.2 CesiumJS: globe/terrain path (`QuadtreePrimitive.js`, `GlobeSurfaceTile*.js`, `ImageryLayer.js`, `TileImagery.js`)

- **The terrain cache is a count, not bytes.** `tileCacheSize = 100` tiles (l. 120).
  - `TileReplacementQueue.trimTiles` evicts only tiles not rendered this frame
    (`_lastBeforeStartOfFrame` marker).
  - It also skips any tile whose `eligibleForUnloading` is false: tiles whose terrain is
    RECEIVING/TRANSFORMING, or whose imagery is TRANSITIONING
    (`GlobeSurfaceTile.js`, l. 80–100).
  - In other words, **never evict something that is mid-load or mid-upload.**
- **Three load queues** (l. 82–84):
  - **High:** tiles "preventing refinement", meaning the absence of the tile forces the engine to
    keep rendering more detail than desired.
  - **Medium:** tiles being rendered.
  - **Low:** preloaded ancestors and non-visible parts of a quad.
  - Each queue is sorted by `computeTileLoadPriority = (1 - dot(dirToTile, viewDir)) * distance`.
    This is a fovea × distance term (`GlobeSurfaceTileProvider.js`, l. 1040–1068).
  - The queues are processed in a **5 ms main-thread time slice** per frame
    (`_loadQueueTimeSlice`, l. 87, `processTileLoadQueue`, l. 1311).
- **The selection keeps the previous frame's result** (`visitTile`, l. 714–987).
  - A tile that meets the SSE can only replace what was drawn last frame if one of these holds:
    - it was rendered last frame;
    - it was culled last frame;
    - it is fully loaded;
    - `canRenderWithoutLosingDetail` holds: terrain is ready and no imagery that a descendant
      showed last frame would be lost.
  - Otherwise the engine keeps rendering the finer descendants from last frame, "even though its
    SSE is better than required", and queues the blocking ancestor as **High**.
  - This is how **zoom-out** works without holes. Its cost is temporary over-detail, which is
    what makes the coarse ancestor urgent.
- **"Kick" logic.** When some children of a refined tile are not renderable and none of them were
  rendered last frame, all of them are removed from the render list and the parent is drawn
  instead. Their loads continue.
  - If more than `loadingDescendantLimit = 20` descendants are pending, their loads are dropped
    and the parent is loaded instead (l. 904–963).
  - This bounds how long the screen can wait on a large frontier.
- **Ancestors and siblings.**
  - `preloadAncestors = true`: a refined tile is queued in Low (l. 965).
  - `preloadSiblings = false`.
  - When every child is "upsampled from parent" (no real data), the parent is rendered and the
    children are touched so that their upsampled status is not forgotten (l. 829–866).
- **Imagery fallback.** `TileImagery.processStateMachine` keeps a `loadingImagery` and a
  `readyImagery`.
  - While the target imagery loads, it walks up `imagery.parent` until it finds a READY ancestor.
  - It draws that ancestor with a texture translation/scale, which is a sub-rectangle of the
    parent texture. This is **upsampling by UV, not by copying data.**
  - Failed or invalid imagery permanently falls back to the best ancestor.
  - Imagery is reference-counted, so a parent texture shared by many terrain tiles stays alive.
- **Imagery level is coupled to terrain level.**
  - `ImageryLayer._createTileImagerySkeletons` (l. 795–810) chooses the imagery level by matching
    the texel spacing to the terrain tile's geometric error. The `errorRatio = 1.0` is hard-coded,
    with a comment admitting it "should really be imagerySSE / terrainSSE".
  - **Imagery therefore cannot be sharper than the terrain level allows.** When terrain is held
    coarse (by budget or by load latency), imagery is coarse too.
- **Skirts and fills.**
  - Skirt height = `levelMaximumGeometricError * 5.0` for quantized mesh
    (`Core/CesiumTerrainProvider.js:798`), and `min(4 × error, 1000 m)` for heightmaps.
  - Skirts are disabled when the camera is underground or the globe is translucent
    (`GlobeSurfaceTileProvider.js:2369`).
  - Where a rendered tile has no terrain yet, a **`TerrainFillMesh`** is synthesised from the
    neighbours' edge vertices. The globe is never empty, even before the tile's own geometry
    arrives.
- **Imagery mipmaps.** Imagery textures get `generateMipmap(NICEST)` plus anisotropic
  filtering when their dimensions are powers of two (`ImageryLayer.js`, l. 1300–1327).

### 1.3 cesium-native (`Cesium3DTilesSelection/…`, `CesiumRasterOverlays/…`)

- **Options** (`TilesetOptions.h`):

  | Option | Default | Meaning / note |
  |---|---|---|
  | `maximumCachedBytes` | 512 MiB | Cache budget in bytes |
  | `maximumSimultaneousTileLoads` | 20 | Concurrent tile loads |
  | `preloadAncestors` | true | Load the ancestors of rendered tiles |
  | `preloadSiblings` | true | Load the siblings of rendered tiles |
  | `loadingDescendantLimit` | 20 | Pending descendants that count as "too many" |
  | `forbidHoles` | false | Never refine a parent until every child can render |
  | `mainThreadLoadingTimeLimit` | 0 = unlimited | Main-thread loading time per frame |
  | `tileCacheUnloadTimeLimit` | 0 = unlimited | Unloading time per frame |
  | `enableLodTransitionPeriod` | — | Fade between levels |
  | `lodTransitionLength` | 1 s | Length of that fade |
  | `kickDescendantsWhileFadingIn` | — | Hide descendants while the parent fades in |
  | `culledScreenSpaceError` | 64 | SSE target for culled tiles |

  Two documented semantics matter most:
  - `maximumCachedBytes` "will never cause tiles that are needed for rendering to be unloaded":
    the budget is soft for the needed set.
  - `forbidHoles` means "refusing to refine a parent tile until all of its child tiles are ready
    to render".
- **Priority groups** (`TileLoadTask.h`):
  - **`Preload`**
  - **`Normal`**
  - **`Urgent`**, documented as "tiles whose absence is causing extra detail to be rendered".
    This is the same concept as CesiumJS's High queue, now an explicit enum.
- **Load queues are rebuilt every frame.**
  - `TilesetViewGroup::startNewFrame` clears the worker-thread and main-thread queues.
  - `finishFrame` sorts them.
  - A not-yet-started load that is no longer needed simply disappears. Cancellation of *queued*
    work is therefore implicit and costs nothing.
  - Concurrency is capped by `maximumSimultaneousTileLoads`
    (`TilesetContentManager::processWorkerThreadLoadRequests`, ~l. 1900).
- **Eviction uses an explicit unload queue.**
  - `TileUnloadQueue` is an intrusive list; tiles become eligible when nothing references them.
  - `unloadCachedBytes` (`TilesetContentManager.cpp:1686`) walks it head-first while
    `getTotalDataUsed() > maximumCachedBytes`, optionally time-sliced.
  - Reference counts (`Tile::getReferenceCount`) are asserted zero before children are cleared.
  - This is **reference-counted pinning, not a frame-number heuristic.**
- **Raster overlays** (`RasterOverlay.h`) have their own budget:
  - `maximumSimultaneousTileLoads = 20`;
  - `subTileCacheBytes = 16 MiB`;
  - `maximumTextureSize = 2048`;
  - `maximumScreenSpaceError = 2`.
- **Geometry follows the imagery.** If a raster has more detail than the geometry tile can carry,
  the geometry is refined by **upsampling the parent mesh** (`RasterOverlayUpsampler.cpp`,
  `UpsampledQuadtreeNode`). The upsampler asserts that the parent is loaded. Imagery can then
  drive refinement even past the terrain's real maximum level.

### 1.4 NASA-AMMOS 3DTilesRendererJS (three.js)

Sources: `src/core/renderer/utilities/LRUCache.js`, `PriorityQueue.js`, `tiles/TilesRendererBase.js`.

- **The LRU cache has two bounds on two metrics.**

  | Bound | Items | Bytes |
  |---|---|---|
  | Minimum (target after eviction) | `minSize = 6000` | `minBytesSize = 0.3 GiB` |
  | Maximum (eviction starts above) | `maxSize = 8000` | `maxBytesSize = 0.4 GiB` |

  - `unloadPercent = 0.05`: each pass unloads 5 % of the excess.
  - Unload runs in a microtask, then re-schedules itself on the next animation frame while it
    is still over the limit and still made progress. Eviction is **incremental and amortised**.
- **Used items are protected.** Items are marked used per frame; used and loaded items are never
  evicted in the byte pass.
- **The default eviction comparator** (`lruPriorityCallback`) orders by:
  1. last frame visited, oldest first;
  2. **deeper tiles first, "so parents are not disposed before children"**;
  3. earlier loading state first;
  4. external tilesets last;
  5. lower error first.
- **The cache refuses new requests when full.** `isFull()` is checked before every request; the
  refused ones are counted in `stats.refused`. This is backpressure at the request stage, but it
  starves loading if the cache is full of protected items.
- **Queues:**
  - the download queue, with `maxJobsPerOrigin = 25`;
  - the parse queue, with `maxJobs = 5`;
  - the node-processing queue, with `maxJobs = 25`.
  - Each queue is sorted once per scheduled run, highest priority last, popped from the end.
  - `PriorityQueue.remove()` rejects a *queued* job with an `AbortError`. Running jobs are not
    affected.
  - `removeUnusedPendingTiles()` runs after each traversal.
- **Load priority** (`priorityCallback`) orders by:
  1. used before unused;
  2. in-frustum before out;
  3. external tilesets first;
  4. higher error first;
  5. closer first;
  6. shallower first.
- **Fallback options.** `loadSiblings = true` and `loadAncestors = true` (both "experimental") are
  described as preventing gaps during movement and providing a fallback while children load.
  `maxTilesProcessed = 250` bounds traversal work per frame.

### 1.5 MapLibre GL JS (`src/tile/tile_manager.ts`, `terrain_tile_manager.ts`, `tile_cache.ts`)

- **Two-tier storage.**
  - `_inViewTiles` holds the retained set.
  - `_outOfViewCache` is a bounded LRU of loaded tiles that left the view.
- **The cache size is derived from the viewport.** `updateCacheSize` uses
  `(ceil(w/tileSize)+1) * (ceil(h/tileSize)+1) * MAX_TILE_CACHE_ZOOM_LEVELS`, optionally capped by
  `maxTileCacheSize`. The rationale in the code: larger viewports need larger caches.
- **Retain logic (`_updateRetainedTiles`).**
  - Every *ideal* tile is retained even before it has data.
  - For each ideal tile that has no data:
    1. first retain its **loaded children** (`_retainLoadedChildren`);
    2. failing that, walk up to `maxUnderzooming = 10` levels and retain the **first loaded
       parent**.
  - The comment says it plainly: "retain their loaded children so they can be displayed as
    substitutes … If no loaded children are available, fallback to seeking loaded parents".
  - **The order is children first, then parent.** It keeps more detail on zoom-out and avoids a
    flash to coarse.
- **Cancellation by retention.** `_removeTile` decrements `uses`. When the tile is unused:
  - if it has data, it goes to the out-of-view cache;
  - otherwise it is **aborted** (`_abortTile`).
  - `cancelPendingTileRequestsWhileZooming` controls whether intermediate ancestors are requested
    during a zoom.
- **Raster fade.** `updateFadingTiles` keeps parents or children of an ideal tile alive while it
  cross-fades in (`_rasterFadeDuration`, `fadeEndTime`).
- **Terrain.**
  - `_addTerrainIdealTiles` always adds the parent of each ideal DEM tile, plus a very low zoom
    (≤ 5) ancestor. The low ancestor is used for elevation estimates and covering-tile
    computation.
  - `deltaZoom = 1`: DEM tiles are loaded one zoom level below the visible level.
  - Draped layers are rendered to texture per terrain tile (`terrain_tile_manager.ts`,
    "render-to-texture tiles").
  - This decouples imagery from DEM level: imagery is composited into a per-tile render target
    at the *visual* zoom, whatever the DEM zoom is.

### 1.6 deck.gl TileLayer / Tileset2D

Source: https://deck.gl/docs/api-reference/geo-layers/tile-layer

- **`refinementStrategy`**:
  - `'best-available'` (default): fill gaps with cached content from the nearest zoom level;
  - `'no-overlap'`: backfill but never overlap;
  - `'never'`;
  - or a custom function.
- **Cache limits.**
  - `maxCacheSize` (count) defaults to 5 × the tiles in the viewport.
  - `maxCacheByteSize` (bytes) works only if the tile data reports a `byteLength`.
- **Requests.**
  - `maxRequests = 6` concurrent.
  - `debounceTime` holds requests until the tile set stops changing for N ms.
  - Each `getTileData` call receives an `AbortSignal`, aborted when too many requests are queued
    *and* the tile is no longer visible.
  - The docs warn that returning data after an abort will cache it.

### 1.7 OpenLayers, iTowns, Google, Bing (public information)

- **OpenLayers WebGL tile layer.**
  - `cacheSize = 512` textures. The docs say it must be large enough for **two zoom levels'
    worth** of tiles.
  - `preload` loads N lower-resolution levels for fallback; the default is 0.
  - Vector tile layers auto-grow the cache to two levels.
  - Sources: https://openlayers.org/en/latest/apidoc/module-ol_layer_WebGLTile.html and
    `src/ol/layer/BaseTile.js`.
- **iTowns.** I found no authoritative, current public description of its budget. Do not treat
  it as a reference without reading its source.
- **Google Earth / Google Maps 3D and Bing Maps 3D.**
  - No public engineering description of their GPU budgets exists, as far as I could find.
  - Google's Photorealistic 3D Tiles are consumed via OGC 3D Tiles with session-scoped URLs.
    The Map Tiles API policies constrain client caching
    (https://developers.google.com/maps/documentation/tile/policies). This matters for disk or
    HTTP caching, not for GPU budgeting.
  - Treat any claim about their internals as unsourced.

---

## 2. Game and texture streaming

- **Virtual texturing (id Tech 5).** "id Tech 5 Challenges", J.M.P. van Waveren, SIGGRAPH 2009,
  https://www.quaddicted.com/webarchive/mrelusive.com_20170131/publications/presentations/2009_siggraph/05-JP_id_Tech_5_Challenges.pdf
  - The design is a quadtree page pyramid with a fixed physical page pool. A GPU feedback
    buffer tells the CPU which pages are needed, analysed in "breadth-first quad-tree order with
    priority".
  - Three lessons apply directly:
    1. **Thrashing:** "With virtual texturing, you can globally adjust feedback LOD bias until
       working set fits." Under pressure, ask for coarser data globally instead of thrashing.
    2. **Latency:** "Upsample coarse page immediately, then blend in finer data when available."
       Never show a hole; hide the pop with a blend.
    3. "We fetch what we can": the request set is a wish list, trimmed by budget.
- **Related work.**
  - Sean Barrett, "Sparse Virtual Textures", GDC 2008: https://silverspaceship.com/src/svt/
  - Mittring, "Advanced Virtual Texture Topics", SIGGRAPH 2008 course.
  - van Waveren and Hart, "Virtual Texturing in Software and Hardware", SIGGRAPH 2012 course.
  - Unity SVT cache management (https://docs.unity3d.com/Manual/svt-cache-management.html):
    "As the cache fills, the mipmap bias increases" so that requested tiles fit. Blur is the
    symptom of a too-small cache, which is the documented, intended degradation mode.
- **Unreal texture streaming**
  (https://dev.epicgames.com/documentation/en-us/unreal-engine/texture-streaming-configuration-in-unreal-engine).
  - A fixed pool `r.Streaming.PoolSize` (in MB), optionally limited to VRAM
    (`r.Streaming.LimitPoolSizeToVRAM`).
  - Every update recomputes a *wanted* resolution per texture and issues mip load and unload
    requests.
  - Under pressure, `r.Streaming.MipBias` and `UsePerTextureBias` drop top mips to fit. The
    model is wanted versus resident: the resident set converges to the wanted set within the
    pool.
  - `r.Streaming.MaxTempMemoryAllowed` bounds staging memory for uploads.
  - `r.Streaming.FramesForFullUpdate` amortises the CPU cost of the update.
  - `r.Streaming.HiddenPrimitiveScale` lowers the wanted resolution of occluded items.
- **Unity mipmap streaming** (https://docs.unity3d.com/Manual/TextureStreaming-configure.html).
  - The Memory Budget covers streamed and non-streamed textures together.
  - Over budget, Unity "loads lower-resolution mipmap levels" and removes unused levels.
  - Levels at or above *Max Level Reduction* must stay, "so Unity may exceed the budget". This is
    an **explicit pinned floor that may overflow the budget rather than show nothing.**
- **UE5 Nanite and Virtual Shadow Maps.**
  - Both use a fixed physical page pool fed by GPU-side requests.
  - Nanite streams cluster pages into a fixed streaming pool. Root pages are always resident, so
    a coarse version always exists.
  - VSM uses a fixed physical page pool, with page caching and invalidation.
  - Karis et al., "A Deep Dive into Nanite Virtualized Geometry", SIGGRAPH 2021 Advances:
    https://advances.realtimerendering.com/s2021/
  - Exact cvar defaults could not be confirmed from Epic's current docs pages (they rendered
    empty for this research). Treat cvar names such as `r.Nanite.Streaming.StreamingPoolSize`
    and `r.Shadow.Virtual.MaxPhysicalPages` as indicative.
  - The principle is robust: **always keep a resident root/coarse level; stream everything else
    into a fixed pool.**
- **Platform memory signals.**
  - **Vulkan `VK_EXT_memory_budget`.** `heapBudget` is the per-heap amount the process can use
    before allocations may fail or degrade performance. `heapUsage` is the current usage. Both
    change over time and should be re-queried.
    (https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_memory_budget.html)
  - **Metal `MTLDevice.recommendedMaxWorkingSetSize`.** It is an *approximation* of how much the
    GPU can allocate "without affecting its runtime performance"; it is a performance guideline,
    not a hard cap.
  - **Metal `currentAllocatedSize`.** The current allocation.
  - **`os_proc_available_memory()` (iOS 13+).** Advisory headroom before the jetsam limit. It
    "should not be cached", it changes over time, and it is not physical RAM.
  - **Unified memory.** On iOS, GPU resources count against the same jetsam footprint as CPU
    memory. A GPU budget that ignores the CPU-side copies (decoded meshes, staging buffers,
    image decode buffers) is wrong on iPhone.
  - **Purgeable resources.** `MTLResource.setPurgeableState(.volatile)` lets the OS reclaim
    cached-but-unused resources; check `.empty` before reuse.
  - **Pressure and thermal signals.** Use `UIApplication.didReceiveMemoryWarningNotification`,
    `DispatchSource.makeMemoryPressureSource` and `ProcessInfo.thermalState`.
  - Sources:
    - https://developer.apple.com/documentation/metal/mtldevice/recommendedmaxworkingsetsize
    - https://developer.apple.com/documentation/os/os_proc_available_memory
    - https://developer.apple.com/documentation/foundation/processinfo/thermalstate
- **wgpu 29.**
  - **`Device::limits()`** gives `max_texture_dimension_2d`, `max_buffer_size` and binding
    limits. **No memory budget and no available-memory query is exposed.**
  - `MemoryHints` (`Performance` / `MemoryUsage` / `Manual{suballocated block range}`) only
    tunes sub-allocation.
  - **`Device::generate_allocator_report()`** is implemented only by the Vulkan and DX12 HALs
    (`wgpu-hal-29.0.4/src/{vulkan,dx12}/device.rs`), **not Metal**.
  - **`get_internal_counters()`** returns internal counters.
  - Consequences for tuile on Mac and iOS:
    - **the engine must account bytes itself** (exact texture/buffer sizes, including mips and
      alignment);
    - any OS budget signal (`recommendedMaxWorkingSetSize`, `os_proc_available_memory`, memory
      warnings) must come through a platform hook, e.g. `as_hal` or the Swift host. It cannot
      come through wgpu.
  - Compressed formats are feature-gated (`TEXTURE_COMPRESSION_ASTC`, `TEXTURE_COMPRESSION_BC`).
    Apple GPUs support ASTC; Apple-silicon Macs also support BC.

---

## 3. Algorithms and policies

### 3.1 Need set, render set, fallback chain: the core invariant

Every robust engine above separates three sets that are recomputed each frame:

1. **The desired set (D).** What the SSE asks for.
2. **The render set (R).** What is actually drawn: the best *resident* cover of the view.
3. **The protected set (P).** R, plus the fallback chain of every element of D that is not
   resident (its nearest resident ancestor, or its resident descendants), plus anything
   mid-upload or mid-transition.

**Eviction may only touch residents outside P.** Examples:

- CesiumJS's sentinel and `eligibleForUnloading`;
- cesium-native's reference counts;
- 3DTilesRenderer's used set;
- MapLibre's retain map;
- Unity's Max Level Reduction floor;
- Nanite's resident root pages.

A **resident coarse floor**, a level that is never evicted, guarantees that P is never empty
anywhere on the globe. That floor is the levels 0–N of terrain and imagery, a few MB.

### 3.2 Priority functions

- Use **lexicographic tiers** before any continuous score. Tiers are cheaper to reason about
  than blended floats. The tiers, in order:
  1. **Blocking/urgent:** the missing item forces either a hole or over-detail; this is
     CesiumJS's High queue and cesium-native's `Urgent`.
  2. **Visible at desired LOD.**
  3. **Preload:** ancestors, siblings, prefetch ring, flight destination.
- **Within a tier, use a continuous score.** Typical terms:
  - SSE excess (`sse / target`);
  - the fovea term `(1 - cos(angle to view axis)) * distance` (CesiumJS globe);
  - distance;
  - depth (shallower first, so the cover fills before it sharpens).
- **Load siblings as a group** when replace refinement needs all of them: the shared priority
  holder in CesiumJS.
- **Motion-aware terms.**
  - `cullRequestsWhileMoving`: drop requests for tiles whose screen position changes faster than
    their expected load time.
  - Foveated deferral with a settle delay.
  - Flight-destination preload.
  - Each of these must be bounded and documented, because they create frame-order dependence.
- **Normalisation hazard.** CesiumJS normalises priority terms by the min and max of the current
  frame. The same tile can therefore change rank as other tiles appear, which makes the order
  harder to predict. Prefer absolute scales.

### 3.3 Eviction policies

- **Plain LRU is not enough for hierarchical data.**
  - Leaves are touched every frame and ancestors are not, so LRU evicts **ancestors first**.
    Zoom-out then finds nothing, which is the owner's coarse-on-zoom-out symptom.
  - Two fixes:
    - touch the fallback chain every frame;
    - sort eviction by (last-used frame, **deepest first**), as 3DTilesRenderer does.
  - Better still: evict by **utility**, i.e. expected need soon × bytes⁻¹. Coarse ancestors
    cover large areas and are cheap; keep them.
- **CLOCK and ARC.**
  - **CLOCK** is an approximation of LRU with O(1) touch.
  - **ARC** (Megiddo & Modha, FAST 2003,
    https://www.usenix.org/legacy/events/fast03/tech/full_papers/megiddo/megiddo.pdf) and
    **CLOCK-Pro** (Jiang, Chen & Zhang, USENIX ATC 2005,
    https://www.usenix.org/legacy/event/usenix05/tech/general/full_papers/jiang/jiang.pdf) add
    frequency and scan resistance.
  - Scan resistance matters for a fly-over: a long pan touches thousands of tiles once each, and
    pure LRU then flushes the "home" area.
  - That said, a per-frame pinned set plus a utility score usually beats generic ARC for spatial
    data.
- **Two-tier caching.** Evicting GPU memory need not discard the data.
  - Keep the encoded or decoded tile in a CPU-side cache (MapLibre's out-of-view cache) or in a
    purgeable Metal resource.
  - A re-view then costs an upload, not a network round trip.
  - Budget each tier separately.
- **Incremental eviction.**
  - Bound the eviction work per frame: 3DTilesRenderer's `unloadPercent`, cesium-native's
    `tileCacheUnloadTimeLimit`.
  - But **eviction must be able to outpace admission**, or the cache creeps over budget.

### 3.4 Hysteresis and anti-thrashing

- **Memory thresholds.** Use a high-water mark (start degrading or evicting) and a low-water
  mark (stop or restore). Examples:
  - CesiumJS's `cacheBytes` and `cacheBytes + overflow`;
  - 3DTilesRenderer's min and max sizes.
- **SSE thresholds.**
  - Refine at `sse > T`, coarsen at `sse < T / k` with k ≈ 1.2–1.5. Otherwise a tile at the
    boundary flips every frame while the camera breathes.
  - Use temporal hysteresis too: do not coarsen something displayed less than N ms ago.
- **Pressure-driven LOD bias.**
  - Change it slowly and symmetrically, in **time** rather than frames. CesiumJS's ×1.02 per
    frame is 3.3× per second at 60 fps and 1.8× per second at 30 fps.
  - Expose it as a metric.
- **Thrash detector.** Count evict→re-request of the same key within a window. If it is above a
  threshold, raise the LOD bias rather than keep cycling.

### 3.5 Cancellation, staleness, deduplication

- **Queued work** should be implicitly cancelled by rebuilding the queue from the need set each
  frame (cesium-native) or by explicit removal (3DTilesRenderer, MapLibre).
- **In-flight network work** should be aborted when the key has left the need set for longer
  than a grace period. Measure the grace in time or in K frames, not 1 frame. With CesiumJS's
  `>= 1` frame:
  - one dropped traversal (a hidden tileset, a paused pass) cancels everything;
  - a jittering frustum boundary cancels and re-requests the same tile repeatedly.
- **Already-decoded results** for a key no longer needed should go to the CPU cache, not to the
  GPU.
- **Deduplicate by key.**
  - Request key = (layer, level, x, y, version).
  - In-flight requests for the same key must coalesce; a single pending entry counts as one slot.
  - Imagery shared by several terrain tiles must be fetched once and reference-counted.
- **Staleness and generations.**
  - Each result carries the generation or epoch of its request.
  - A result whose tile was evicted or re-requested in between must be dropped. This prevents
    the classic race of a late response resurrecting a tile into a state the selection no longer
    expects.

### 3.6 Bounded queues and backpressure

- Bound every stage: network concurrency, decode workers, the main-thread upload queue. Each
  bound is enforced at admission.
- **Never** let a full downstream stage hold an upstream slot forever. 3DTilesRenderer refuses
  to request when the cache is full; if the full cache is all pinned, loading stops. Backpressure
  must be able to *preempt* lower-priority residents.
- Keep separate concurrency pools or quotas per class (terrain, imagery, 3D Tiles, metadata).
  Otherwise one class (for example a 3D Tiles fly-over) starves the ground. CesiumJS types its
  requests and caps them per server.

### 3.7 Upload budgets

- Throttle GPU uploads per frame by **bytes and/or time**:
  - CesiumJS: a 5 ms slice for terrain/imagery processing;
  - cesium-native: `mainThreadLoadingTimeLimit`;
  - Unreal: `MaxTempMemoryAllowed`.
- Byte budgets are deterministic; ms budgets depend on the machine.
- **Prefer a byte budget with a ms safety cap.** Order uploads by the same tiers as loads:
  urgent cover first.
- Use staging belts or ring buffers (wgpu `StagingBelt`, `queue.write_texture`) and account
  the staging memory in the budget.
- **Generate mipmaps at upload time** (compute or blit chain), or ship them pre-built in KTX2,
  so they are part of the byte count. Mip generation is about +33 % bytes.

### 3.8 Prefetch, rotation, zoom-out

- **Prefetch ring.** Load the tiles one ring outside the frustum at the *coarse* level only (for
  example desired level −2), as preload tier.
- **Rotation.**
  - Predict the frustum at t + latency from angular velocity, and prefetch along the motion
    direction.
  - Rotations reveal content at the screen edges. With a fovea term, edges are deprioritised,
    so on rotation the fovea weight should relax or the prediction should move the fovea.
- **Zoom-out.**
  - The coarser tiles must already be resident: `preloadAncestors` in CesiumJS and
    cesium-native, MapLibre's terrain parent, OpenLayers' `preload`.
  - **Pin a coarse floor** (levels 0..N) permanently, and keep each rendered tile's ancestor
    chain protected.
  - On zoom-out, CesiumJS keeps the finer tiles until the ancestor arrives, marking the ancestor
    urgent. This avoids a pop to the base level, at the cost of over-detail.

### 3.9 Overflow policy

Rank the options from best to worst. A spec should say which applies at each pressure level.

1. **Temporary over-budget for P.** The protected set is never evicted, so the budget becomes
   soft for P. This is what cesium-native and CesiumJS do.
2. **Degrade the desired LOD globally** with a pressure bias, with hysteresis. Examples:
   memoryAdjustedSSE, Unity SVT mip bias, id Tech 5 feedback bias, Unreal MipBias.
3. **Degrade per class.** Drop 3D Tiles detail before ground; drop imagery mips before terrain
   level.
4. **Never** refuse a fallback or evict a cover tile. A hole is worse than blur. This is a
   project rule: "black ground is a bug".

Hard limits remain: the jetsam limit on iOS. Near it, purge the CPU caches and purgeable GPU
resources first, then raise the bias. Only then shed non-ground classes.

### 3.10 Texture strategy

- **Per-tile textures.**
  - Simple, with exact accounting.
  - They cost many bind groups. Prefer bindless or texture arrays where available.
- **Texture arrays or atlases with fixed-size slots** (for example 256² or 512² × N layers):
  - **The budget becomes a slot count, which is trivially exact.**
  - There is no fragmentation and allocation is O(1).
  - This works well because imagery tiles are fixed-size. It is the physical-page-pool model of
    virtual texturing.
  - Use a gutter or border for filtering; id Tech 5 uses a 4-texel border for anisotropic
    filtering.
- **Compressed formats.**
  - KTX2 with Basis Universal transcodes to ASTC on iOS and BC7/BC1 on Mac. It gives 4–8×
    fewer bytes than RGBA8.
  - It is the single largest multiplier of effective budget.
  - Server-side pre-encoding of imagery tiles is in tuile's own domain (tuile-server).
- **Mipmaps for draped imagery.** Without mipmaps, grazing views alias and the shimmer looks
  like instability. A full mip chain or anisotropic filtering is cheap (+33 %) and should be
  budgeted. CesiumJS mipmaps imagery when it is power-of-two.

### 3.11 Mobile pressure and thermal

- Poll `os_proc_available_memory` at low frequency, and react to memory-warning notifications by
  shrinking the cache targets (low and high water).
- On `thermalState >= .serious`, raise the SSE target and lower the request and upload rates.
  This trades sharpness for sustained frame time.
- Re-derive the budget at runtime. Never hard-code a desktop number such as 512 MiB on iPhone.

---

## 4. Terrain and imagery specifics

### 4.1 Crack-hiding techniques

| Technique | Source | How it works | Trade-off |
|---|---|---|---|
| **Skirts** | Ulrich 2002, http://tulrich.com/geekstuff/sig-notes.pdf; quantized-mesh carries edge index lists for skirts, https://github.com/CesiumGS/quantized-mesh | Vertical curtains down from each edge | Cheap and robust to any neighbour level; adds fill-rate |
| **Stitching / edge morphing** | — | Neighbour-aware index buffers or T-junction removal | Needs neighbour levels to differ by ≤ 1 (restricted quadtree) |
| **CDLOD geomorphing** | Strugar 2009, https://github.com/fstrugar/CDLOD | Vertices morph continuously toward the parent grid inside a morph band | Crack-free and pop-free, but needs regular grids (heightmap), not irregular quantized-mesh TINs |
| **Geometry clipmaps** | Losasso & Hoppe 2004, https://hhoppe.com/proj/geomclipmap/ | Nested regular rings around the viewer, toroidal update | Constant budget per ring and predictable memory, but needs a regular heightfield and is view-centric |
| **CesiumJS fill meshes** | `TerrainFillMesh.js` | Synthesised from neighbour edges when a tile's own mesh is not ready | No hole |

### 4.2 When skirts become visible

1. **A hole next to them.** When a tile is absent for even one frame, the curtains of its
   neighbours face the camera. Skirts are 4–5× the level's geometric error deep (Cesium
   constants), and they are textured with stretched edge texels.
   - **"Skirts appear when tiles refresh" is almost always this: a gap frame during a swap**
     (old mesh deactivated before the new one is active), or a tile excluded from R while it
     waits.
2. **Neighbours differing by several levels**, where the crack height exceeds the skirt length.
   It happens when a fallback ancestor sits next to a fine tile, or under a pressure bias.
   - The skirt length must be computed from the *maximum level difference allowed* (or from the
     coarse neighbour's error), not only from the tile's own level.
3. **Depth fighting.** Skirt and neighbour surfaces nearly coplanar near the edge, made worse
   with f32 ECEF positions or poor depth precision. Use reversed-Z, RTC/local frames
   (project rule 7), and skirts that go *down and slightly inward*.
4. **Lighting and imagery on skirts.**
   - Skirt normals taken from the edge vertex give the same shading, which is fine.
   - Recomputed vertical normals give dark curtains.
   - Imagery: skirt vertices duplicate edge UVs, so one texel row is smeared vertically. If the
     two neighbours draw imagery at *different* levels (one using parent upsample), the colour
     mismatch shows along the seam.
5. **Translucency or an underground camera.** Disable skirts, as CesiumJS does.

### 4.3 Imagery level versus terrain level

- **Coupling in CesiumJS.** The imagery level is chosen from the terrain geometric error
  (errorRatio 1). Imagery is never sharper than terrain, and **any terrain coarsening (budget,
  latency, pressure bias) silently coarsens the imagery.**
- **Better decoupling:**
  - cesium-native upsamples geometry to carry finer raster;
  - MapLibre renders imagery to texture per terrain tile at the visual zoom, with DEM at
    `zoom - 1`.
  - Terrain needs less resolution than imagery for the same visual quality; its geometric error
    is in metres of height, imagery error in metres of ground.
  - A sound design selects terrain by terrain SSE and imagery by imagery SSE (texel/pixel), and
    lets a terrain tile carry imagery of any finer level (multiple imagery tiles per terrain
    tile, or terrain upsampled).
- **Imagery finer than terrain.** Several imagery tiles per terrain tile (a UV sub-rectangle
  each), or upsample terrain.
- **Terrain finer than imagery.** One parent imagery tile covers many terrain tiles via
  translation/scale; reference-count it and protect it.
- **While loading.** Draw the nearest resident ancestor imagery with a UV transform (the
  `TileImagery` pattern). Then cross-fade, or id-Tech-5-style "upsample then blend".

### 4.4 Replace versus additive, and atomic swaps

- **Additive.** Children add to the parent; no swap is needed, and the parent stays.
- **Replace.** Children replace the parent. The swap must be atomic over the parent's area:
  1. create and upload *all* covering children (meshes **and** their imagery, or an acceptable
     imagery fallback);
  2. activate them;
  3. only then deactivate the parent.
- The same holds in reverse on coarsening: the parent must be resident *before* the children are
  dropped (CesiumJS's `canRenderWithoutLosingDetail` and kick logic).
- Per-region atomicity means **the parent's quad as a unit**. For terrain with imagery, "ready"
  must include both the terrain mesh and the imagery fallback chain for each child. Otherwise
  children appear with blurry or black imagery, or with a different imagery level than the
  neighbours.
- **A one-frame overlap is fine; a one-frame gap is never fine.** Overlap needs a consistent
  depth tie-break (draw the finer tile on top, use a stencil, or let fading handle it), or
  coplanar z-fighting appears.

### 4.5 Sources of unpredictability

1. **Frame-order dependence.**
   - Selection depends on what was resident last frame ("rendered last frame" rules, kick
     logic).
   - Results depend on network timing.
   - One-frame cancellation thresholds depend on the frame rate.
   - Multiplicative per-frame adjustments (×1.02) depend on the frame rate.
2. **Time-slice budgets** (5 ms) make outcomes depend on CPU speed and on concurrent load.
3. **Unordered containers.** In Rust, `HashMap`/`HashSet` with `RandomState` iterate in a
   per-process random order. Ties in priority or eviction then resolve differently on every run.
   Use `BTreeMap`/`IndexMap`, or sort by a total key including the tile ID.
4. **Unstable priority normalisation** by per-frame min/max (§3.2).
5. **Completion order of async work** applied as it arrives instead of at a frame boundary.
6. **Races between eviction and use.** A tile evicted between selection and draw, or evicted
   while its upload or decoded result is in flight. The state machines need explicit
   "transitioning, not evictable" states (`eligibleForUnloading`) and generation checks.
7. **Hidden coupling of budgets.** One shared pool for terrain, imagery and 3D Tiles, where the
   class that loads first wins.

---

## 5. Testing and observability

- **Deterministic simulation.**
  - Make the budget core a pure state machine: `step(camera, events) -> (draw set, requests,
    evictions)`.
  - Inject a simulated clock, a simulated network (per-request latency drawn from a seeded
    distribution, failures, reordering) and a simulated GPU (byte accounting, upload cost).
  - The same camera script plus the same seed must give an identical trace.
  - Prior art:
    - FoundationDB simulation testing: https://apple.github.io/foundationdb/testing.html
    - TigerBeetle VOPR: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md
- **Property-based tests** (Rust `proptest`) over random camera scripts and network traces.
  Invariants:
  - **Coverage:** every point of the visible ground is covered by exactly one or more drawn
    tiles every frame. Test this on the *ground*, not just on selected tiles: rasterise the view
    footprint.
  - **Budget:** resident bytes ≤ high-water + the size of P.
  - **Eviction safety:** nothing in P is ever evicted.
  - **Monotone convergence:** with a static camera and an infinite network, the system reaches
    the desired set in ≤ N frames and then makes zero requests.
  - **Liveness:** no request stays queued forever while slots are free.
  - **No thrash:** with a static camera, no key is evicted and re-requested.
- **Soak tests.**
  - Hours of scripted orbit, zoom and pan.
  - Watch for monotonic growth of resident bytes, queue length, in-flight count, reference
    counts and refused requests.
  - This finds leaks of pins (refcount never released): the "slower after a while" class.
- **Concurrency checks.** Use `loom` for the async boundaries, and generation counters on every
  completion.
- **Metrics to expose per frame and per class.**
  - Resident bytes by class (terrain, imagery, 3D Tiles, CPU cache), and the budget and its
    high/low water marks.
  - Bytes pinned (P) versus evictable.
  - Requests:
    - queued, in-flight, completed, cancelled, refused, deduplicated;
    - stale arrivals dropped.
  - Upload bytes per frame and upload ms.
  - Cache hit rate:
    - GPU hit;
    - CPU-cache hit;
    - network.
  - Thrash count: evict→re-request within T.
  - Pressure LOD bias.
  - The SSE histogram of drawn tiles versus target.
  - The number of drawn tiles using fallback imagery, and the level gap.
  - Frames or ms to converge after the camera stops.
  - Holes detected (should be 0) and gap frames during swaps (should be 0).
  - Examples to follow:
    - CesiumJS `Cesium3DTilesetStatistics` and the QuadtreePrimitive debug output (visited,
      rendered, culled, waiting-for-children);
    - cesium-native `ViewUpdateResult` (tilesKicked, queue lengths).
- **Debug overlays.**
  - Tile level colour.
  - Resident state (loading, uploading, ready, fallback-drawn, pinned).
  - The imagery level versus terrain level gap.
  - Skirts tinted a loud colour, so a gap frame is unmistakable.
  - Budget pressure gauge.
  - Request heatmap.
  - Prior art: the CesiumJS 3D Tiles inspector heatmaps (`Cesium3DTilesetHeatmap.js`).

---

## 6. Comparison table

| | Budget unit | Load priority | Eviction | Cancellation | Upload throttling | Terrain/imagery coupling |
|---|---|---|---|---|---|---|
| **CesiumJS 3D Tiles** | Bytes: `cacheBytes` 512 MiB, plus `maximumCacheOverflowBytes` 512 MiB; above both, SSE ×1.02/frame | Packed composite: flight, foveated defer, foveated, progressive-res, distance or SSE, depth. Siblings share a priority holder | LRU with a per-frame sentinel; never evicts what was touched this frame | In-flight cancelled if not touched in the latest frame. Bounded request heap (20); 50 global, 18 per server | Processing stops at the byte limit; no ms slice | n/a (meshes); REPLACE refines only when all children are loaded |
| **CesiumJS globe** | Count: `tileCacheSize` 100 terrain tiles; imagery refcounted | High (blocking refinement) > Medium (rendered) > Low (ancestors); fovea×distance within each | LRU tail; skips tiles rendered this frame and tiles in transition | Via RequestScheduler priority heap | 5 ms main-thread slice per frame | Imagery level derived from terrain error; ancestor imagery by UV transform; fill meshes; skirts 5× error |
| **cesium-native** | Bytes: `maximumCachedBytes` 512 MiB, soft for needed tiles; overlays have their own byte cache | Urgent / Normal / Preload groups plus a score; queues rebuilt each frame | Reference-counted unload queue; optionally time-sliced | Queued work dropped by the per-frame rebuild; 20 concurrent loads | `mainThreadLoadingTimeLimit` | Geometry upsampled to carry finer raster; preload ancestors and siblings; `forbidHoles` |
| **3DTilesRendererJS** | Bytes and count, each with min/max (0.3/0.4 GiB; 6000/8000) | Used > in-frustum > external > error > distance > depth | Used set protected; deepest-first among LRU; 5 % of excess per pass | Queued jobs removed (AbortError); new requests refused when the cache is full | Parse queue 5 jobs; 250 tiles processed per traversal | n/a; `loadSiblings` and `loadAncestors` |
| **MapLibre GL** | Count derived from viewport × zoom levels | Ideal tiles; covering order | Retain map (ideal + loaded children + first loaded parent); out-of-view LRU | Abort on drop from the retain set | Raster fade, per-tile | DEM at zoom−1 plus parent and low ancestor; imagery render-to-texture per terrain tile |
| **deck.gl Tileset2D** | Count (default 5× viewport) and optional bytes | Viewport tiles | LRU on cache size | AbortSignal when over-queued and not visible; `debounceTime` | n/a | `refinementStrategy`: best-available, no-overlap, never |
| **Unreal / Unity textures** | Bytes (pool) | Wanted resolution per texture | Drop unwanted mips; pinned floor (Max Level Reduction) | Per update | `MaxTempMemoryAllowed` | n/a; global MipBias under pressure |

---

## 7. Anti-patterns that produce the owner's symptoms

### Symptom A: skirts appear when tiles refresh

1. **The old tile is deactivated before the new one is resident and uploaded.** This is a gap
   frame: the neighbours' skirts are seen through the hole.
2. **"Refresh" (reload, content change, imagery swap) treated as unload + load** instead of
   build-new → activate → retire-old.
3. **A tile removed from the render set while it waits** for imagery or mesh, instead of
   drawing its nearest ancestor or fill.
4. **Eviction of a tile that is still drawn**, or eviction during an upload, because the evictor
   uses last frame's touched set or a count budget.
5. **Neighbours allowed to differ by more levels than the skirt depth covers.** Typical cases:
   pressure bias applied unevenly, ancestor fallback next to fine tiles.
6. **Imagery swapped per tile non-atomically.** A child appears with parent-upsampled imagery
   next to a sibling with native imagery, so the seam and the skirt colour mismatch shows.
7. **Near-coplanar overlap** of old and new for a frame without a depth tie-break, which makes
   z-fighting at the skirts.

### Symptom B: after a while, loading becomes very slow

1. **Pin leak.** A reference count or "used" flag is never released (failed loads, cancelled
   loads, tiles that left the view mid-transition). The evictable set shrinks to zero, and the
   cache refuses or starves new loads (3DTilesRenderer-style refusal when `isFull`).
2. **Stale requests keep the concurrency slots.**
   - No cancellation of in-flight requests for tiles that left the need set, so new needs queue
     behind dead ones.
   - Or the opposite: 1-frame cancellation thrash, with the same tile cancelled and re-requested
     forever.
3. **Unbounded queues sorted each frame** (O(n log n) growth). Duplicates are not coalesced, so
   the same key is queued many times.
4. **A ratcheting pressure bias.** It rises on pressure but never falls, because the low-water
   mark is never reached (pinned bytes > low water). The bias also adjusts per frame, so it
   drifts with frame rate.
5. **Count-based budgets with byte-heavy tiles.** A count cap is reached by tiny tiles, or bytes
   overshoot without eviction. Or the reverse: bytes are not accounted for mips and staging, so
   accounting drifts from reality.
6. **Eviction rate < admission rate.** Time-sliced eviction is too small, so the cache sits
   above high-water permanently and every admission triggers work.
7. **Retry storms on failed tiles** with no negative cache or back-off.
8. **Upload queue FIFO by arrival.** Stale uploads are processed before urgent ones.

### Symptom C: coarse tiles on rotation and zoom-out

1. **LRU evicts ancestors first**, because leaves are touched and ancestors are not. On zoom-out
   the coarse levels must be re-fetched, so the base level or a hole shows.
2. **No pinned coarse floor and no `preloadAncestors`.**
3. **Imagery level derived from the terrain level.** Coarse terrain (budget or latency) forces
   coarse imagery even when imagery bytes are available.
4. **One shared pool and no class quotas.** 3D Tiles or terrain eat the imagery budget, so the
   "imagery should be shown but isn't".
5. **A fovea/distance priority with no motion prediction.** Content revealed at the screen edges
   on rotation is always last. A foveated deferral delay that never expires while the camera
   keeps moving.
6. **A pressure LOD bias applied globally and stuck high** (B4).
7. **Refusing to show a loaded finer tile because its siblings are not ready**, without showing
   the parent either. That combines with a missing parent into "coarse or hole".
8. **Cancelled-then-refetched tiles on every small rotation.** This happens when the cancellation
   grace is 1 frame and the prefetch ring is missing.

### Symptom D: unpredictable behaviour

1. Decisions depend on wall-clock time slices, frame rate, or arrival order of async results.
2. `HashMap` iteration order used as a tie-break.
3. Priority normalisation by per-frame extremes.
4. State transitions applied mid-frame from callbacks, which makes eviction/use races.
5. No generation or epoch check on completions.
6. No single source of truth for "is this tile drawable": separate flags for mesh, imagery and
   upload drift apart.

---

## 8. Checklist for evaluating a proposed budget core

**Budget model**

1. Is the budget in **bytes**, per resource class (terrain mesh, imagery textures, 3D Tiles
   geometry and textures, staging, CPU-side decoded cache), with an explicit quota or priority
   between classes?
2. Is the byte accounting exact for GPU reality: mips, alignment, compressed block sizes,
   staging buffers, duplicated CPU copies on unified memory?
3. Are there high- and low-water marks (hysteresis), and is it specified what happens between
   them?
4. Is the budget derived at runtime from the platform (`recommendedMaxWorkingSetSize`,
   `os_proc_available_memory`, memory warnings, thermal state) via a host hook? wgpu exposes no
   budget, and no allocator report on Metal.

**Protected set and eviction**

5. Is there an explicit protected set (render set + fallback chain of every needed-but-missing
   tile + in-transition items), and is it **impossible** for eviction to touch it?
6. Can eviction ever remove a tile's **only drawable fallback**, including the imagery ancestor
   that a terrain tile is currently drawing with a UV transform?
7. Is there a permanently resident coarse floor (levels 0..N of terrain and imagery) so that
   global coverage always exists?
8. Does the eviction order protect ancestors (deepest first, or utility-based) rather than plain
   LRU, which evicts ancestors first?
9. Is eviction guaranteed to outpace admission? What happens when P alone exceeds the budget
   (soft overflow + LOD bias, never holes)?
10. Is there a CPU-side or purgeable second tier, so that evicting GPU memory does not force a
    network refetch?

**Requests and scheduling**

11. Are priorities tiered (urgent/blocking > visible > preload), with a total, deterministic
    tie-break (tile ID)?
12. Is "urgent" defined as "absence forces a hole *or* over-detail", so that zoom-out ancestors
    get top priority?
13. Are queued requests for tiles that left the need set dropped in the same frame, and
    in-flight ones aborted after a bounded grace (K frames or T ms, not exactly 1 frame)?
14. Are requests deduplicated by key, with shared imagery fetched once and reference-counted?
15. Are all queues bounded, with admission control that can preempt lower-priority work rather
    than refuse urgent work?
16. Are there per-class concurrency quotas, so that 3D Tiles cannot starve ground (terrain +
    imagery)?
17. Do failed loads have back-off and a negative cache, and do they release every pin?
18. Is there motion-aware prefetch (predicted frustum, a coarse prefetch ring) for rotation?

**Swaps and rendering**

19. Are terrain and imagery swapped atomically **per region** (a parent's quad): new mesh + its
    imagery (or fallback) uploaded and active before the old is deactivated, in both refine and
    coarsen directions?
20. Is the zero-gap guarantee also stated for "refresh" (content reload, imagery layer change,
    budget-driven downgrade), not only for refinement?
21. Is the imagery level selected independently of the terrain level (imagery SSE), with
    terrain upsample or multi-imagery per tile, so that coarse terrain does not imply coarse
    imagery?
22. Is the maximum level difference between neighbours bounded and consistent with the skirt
    depth? Are skirts sized from the coarser neighbour's error?
23. Is the upload budget expressed in bytes per frame (with a ms safety cap), ordered by the same
    tiers, and does it include mip generation?
24. Does the overlap during a swap have a defined depth or draw-order tie-break, so that
    coplanar z-fighting is impossible?

**Determinism and observability**

25. Is behaviour **deterministic** for a given camera script and network trace: no wall-clock
    slices in decisions, no `HashMap` order, completions applied at frame boundaries, generation
    checks on every result?
26. Are all adjustments (pressure bias, cancellation grace, fade) expressed in time, not frames,
    so that 30 fps and 120 fps behave the same?
27. Is there a simulation harness (simulated network + GPU accounting) with property tests:
    coverage of the *ground* every frame, budget respected, P never evicted, convergence when
    static, no thrash when static?
28. Are the metrics exposed?
    - per-class resident and pinned bytes;
    - queued, in-flight, cancelled, refused and stale counts;
    - hit rates (GPU, CPU cache, network);
    - thrash count;
    - LOD bias;
    - frames to converge;
    - gap frames (must be 0).
29. Is there a debug overlay for level, resident state, fallback in use, imagery/terrain level
    gap and budget pressure, with skirts tinted so that a gap frame is visible?
30. Is there a soak test (hours of scripted motion) that checks for monotonic growth of
    pinned bytes, queue length and reference counts?

---

## Sources (summary)

**Local reference code** (gitignored clones, Apache-2.0):
- `references/cesium/packages/engine/Source/Scene/`:
  - `Cesium3DTileset.js` (l. 79–102, 246–281, 2669–2956)
  - `Cesium3DTilesetCache.js`
  - `Cesium3DTilesetBaseTraversal.js` (l. 94–236)
  - `Cesium3DTile.js` (l. 2399)
  - `QuadtreePrimitive.js` (l. 82–152, 714–987, 1311–1370)
  - `TileReplacementQueue.js`
  - `GlobeSurfaceTile.js` (l. 80–100)
  - `GlobeSurfaceTileProvider.js` (l. 916–1068, 2369)
  - `TileImagery.js`
  - `ImageryLayer.js` (l. 686–830, 1300–1327, 1682–1706)
  - `TerrainFillMesh.js`
- `references/cesium/packages/engine/Source/Core/`:
  - `RequestScheduler.js`
  - `RequestType.js`
  - `CesiumTerrainProvider.js:798`
  - `HeightmapTerrainData.js:230`
- `references/cesium-native/Cesium3DTilesSelection/`:
  - `include/.../TilesetOptions.h`
  - `TileLoadTask.h`
  - `TileUnloadQueue.h`
  - `src/Tileset.cpp` (`loadTiles`)
  - `src/TilesetContentManager.cpp:1686`
  - `src/TilesetViewGroup.cpp:113–150`
  - `src/RasterOverlayUpsampler.cpp`
- `references/cesium-native/CesiumRasterOverlays/include/CesiumRasterOverlays/RasterOverlay.h`

**Fetched code:**
- 3DTilesRendererJS: https://github.com/NASA-AMMOS/3DTilesRendererJS (`LRUCache.js`,
  `PriorityQueue.js`, `TilesRendererBase.js`)
- MapLibre GL JS: https://github.com/maplibre/maplibre-gl-js (`src/tile/tile_manager.ts`,
  `terrain_tile_manager.ts`, `tile_cache.ts`)
- wgpu 29.0.4 / wgpu-hal 29.0.4 / wgpu-types 29.0.4 (`~/.cargo/registry`)

**Documentation:**
- deck.gl TileLayer: https://deck.gl/docs/api-reference/geo-layers/tile-layer
- OpenLayers WebGLTile: https://openlayers.org/en/latest/apidoc/module-ol_layer_WebGLTile.html
- Unreal texture streaming:
  https://dev.epicgames.com/documentation/en-us/unreal-engine/texture-streaming-configuration-in-unreal-engine
- Unity mip streaming: https://docs.unity3d.com/Manual/TextureStreaming-configure.html
- Unity SVT cache: https://docs.unity3d.com/Manual/svt-cache-management.html
- Apple:
  - https://developer.apple.com/documentation/metal/mtldevice/recommendedmaxworkingsetsize
  - https://developer.apple.com/documentation/os/os_proc_available_memory
  - https://developer.apple.com/documentation/foundation/processinfo/thermalstate
- Vulkan: https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_memory_budget.html
- Quantized-mesh: https://github.com/CesiumGS/quantized-mesh

**Papers and talks:**
- van Waveren, "id Tech 5 Challenges", SIGGRAPH 2009 (link in §2)
- Barrett, "Sparse Virtual Textures", 2008
- Karis et al., "Nanite", SIGGRAPH 2021
- Ulrich, "Rendering Massive Terrains using Chunked LOD", 2002
- Strugar, "CDLOD", 2009/2010
- Losasso & Hoppe, "Geometry Clipmaps", SIGGRAPH 2004
- Megiddo & Modha, "ARC", FAST 2003
- Jiang et al., "CLOCK-Pro", USENIX ATC 2005
- FoundationDB simulation testing; TigerBeetle VOPR

**Not verified** (marked as such in the text): exact Nanite and VSM cvar defaults; iTowns, Google
Earth and Bing Maps 3D internals.
