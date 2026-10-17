<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — how established engines and the literature do it

Algorithmic inspiration only (CLAUDE.md rule 4). Local reference paths are under the
gitignored `references/`; external sources are cited from the literature and were **not
re-verified in this session**.

## Local reference clients

**JS client — `references/cesium/packages/engine/Source/`**

- `Scene/Cesium3DTileset.js`
  - 79-80, 248-262: two byte numbers, `cacheBytes` (trim target, 512 MiB) and
    `maximumCacheOverflowBytes` (512 MiB). Overflow is allowed only for what the current
    view needs.
  - 2898-2956 (`processTiles`): above `cacheBytes + overflow`, processing stops and
    `memoryAdjustedScreenSpaceError` is multiplied by 1.02 per frame. Below `cacheBytes` it
    decays by /1.02 back to the configured error. This degrades smoothly instead of evicting
    what is visible.
  - 2743 `cancelOutOfViewRequests`: requests of tiles not visited this frame are cancelled.
  - 82-83 `cullRequestsWhileMoving`: no requests that will be stale on arrival.
- `Scene/Cesium3DTile.js`
  - 2401-2460: the priority is one number built from digit fields: preload flight, foveated
    deferral, foveated factor, progressive resolution, then distance or depth. It is
    re-sorted when the error adjusts.
  - 800-900: foveated deferral, which relaxes the error off-centre.
- `Core/RequestScheduler.js` 26, 57, 65: a bounded priority heap (20); 50 requests in total
  and 18 per server; cancelled requests are dropped.
- `Scene/QuadtreePrimitive.js` (globe)
  - 82-87: three load queues — high (blocks refinement), medium (rendered), low (refined
    past or invisible) — processed under a 5 ms time slice.
  - 120: `tileCacheSize`.
  - 350: `markStartOfRenderFrame` (what was used this frame is never unloaded).
  - 142-152: preload ancestors and siblings.
- Imagery
  - `Scene/ImageryLayer.js` 801, 1682: the imagery level is chosen **per terrain tile** from
    its geometric error (`getLevelWithMaximumTexelSpacing`).
  - `Scene/TileImagery.js` 16-17: `readyImagery` (an ancestor's imagery) is drawn while
    `loadingImagery` loads. The swap is new-before-old per tile.
- `Scene/TerrainFillMesh.js`: fill meshes built from neighbours' edges for missing tiles,
  instead of exposing skirts.

**C++ engine — `references/cesium-native/Cesium3DTilesSelection/include/Cesium3DTilesSelection/TilesetOptions.h`**

| Line | Option | Meaning |
|---|---|---|
| 103 | `maximumSimultaneousTileLoads = 20` | load concurrency |
| 131 | `loadingDescendantLimit = 20` | hold cut-off |
| 202 | `maximumCachedBytes = 512 MiB` | byte budget |
| 266 | `enableLodTransitionPeriod` | dithered cross-fade |
| 302 | `mainThreadLoadingTimeLimit` | time budget for main-thread loading |
| 309 | `tileCacheUnloadTimeLimit` | time budget for unloading |

## Other sources (from the literature, not re-verified here)
- MapLibre and Mapbox GL source caches: the retained set is the ideal tiles plus loaded
  parents and children that cover them; LRU sized from viewport coverage; raster and terrain
  share the tile pyramid.
- three.js 3D Tiles renderer: an LRU with `minSize`/`maxSize`/`maxBytesSize`/`unloadPercent`,
  a priority queue with `maxJobs` re-sorted per frame, `errorTarget`.
- Unreal texture streaming: a fixed pool; per-texture "wanted mip" from screen size;
  over-budget drops mips of the least-recently-seen textures; per-frame streaming
  bandwidth limits.
- Unity mipmap streaming: a memory budget; priority by screen-space size.
- Virtual texturing: Barrett, "Sparse Virtual Textures" (2008); Mittring, "Advanced Virtual
  Texture Topics" (2008); van Waveren, id Tech 5 (2009). Feedback buffers tell which page
  and mip are needed; a fixed physical page cache; LRU over pages; coarse mips always
  resident.
- Terrain LOD:
  - Ulrich, Chunked LOD (2002): skirts, with parent-child swap only when children are ready;
  - Losasso and Hoppe, Geometry Clipmaps (2004);
  - Strugar, CDLOD (2009): vertex morphing removes popping and T-junctions;
  - Lindstrom and Pascucci, view-dependent refinement (2001–2002);
  - Cozzi and Ring, 3D Engine Design for Virtual Globes (2011): out-of-core request
    prioritisation and cancellation.
- Apple platforms:
  - `os_proc_available_memory`, `didReceiveMemoryWarning`, `ProcessInfo.thermalState`;
  - Metal `currentAllocatedSize`, `recommendedMaxWorkingSetSize`;
  - purgeable resources.

## Proven patterns (synthesis)
1. **Byte budgets per resource class**, measured as the GPU pays them: mesh buffers,
   textures with mips, uniforms. A count is at most a secondary guard.
2. **One priority function, re-evaluated every frame.** Its terms:
   - a class first: holes, then refinement blockers, then rendered, then preload;
   - then screen-space-error excess × centrality (foveation) × proximity;
   - plus an age term against starvation;
   - plus an ancestor bonus: a coarse tile covering many waiting tiles first.
3. **Eviction never touches the drawn set or its fallback chain.** Beyond that, evict by
   "frames since last needed", then by low value.
4. **Requests:**
   - deduplicated; bounded heap;
   - cancelled only when a tile has left the need set for a short grace period, never because
     a pass reordered;
   - the cancellation propagates to decode.
5. **Upload budgets in bytes and ms**, ordered by the same priority. Stale content is dropped
   before upload.
6. **Hysteresis on LOD**, keyed on what was **drawn**, plus memory-adjusted SSE hysteresis
   (×1.02 / ÷1.02).
7. **Prefetch:** ancestors (cheap, a third of the frontier) and a ring around the frustum at
   preload priority. Prefetch must never displace visible work.
8. **Overflow policy:** a temporary, bounded over-budget for the visible set; above it,
   degrade to coarser by raising the error, **never a hole**.
9. **Atlases or texture arrays** of fixed-size imagery pages: a fixed pool, exact byte
   budget, no allocation churn, cheap eviction. Per-tile textures are simpler but fragment
   memory and cost an allocation per upload.
10. **Mobile:** budget derived from available memory; shrink on warning, then recover with
    hysteresis; frame rate and upload budget lowered when hot.
11. **Terrain and imagery coupled per tile:**
    - the imagery level is a function of the tile's own error and distance;
    - an ancestor's ready imagery is drawn while the right level loads;
    - a parent-to-children swap happens when the children's terrain **and** imagery are
      ready;
    - skirts or fill meshes only where levels differ, and never allowed to win over ground in
      front of them.
