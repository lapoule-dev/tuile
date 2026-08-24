# 15 — The OpenUSD scene index

The Hydra 2.0 core of tuile's OpenUSD story, and the trajectory it serves.
Decisions recorded here were taken with Laurent on 2026-08-24, on
`feat/usd-scene-index`.

## Vision — what pulls this

**The video product pulls the trajectory.** The deterministic recorder
(today: wgpu, exact-or-die contract) gains an OpenUSD twin: a track becomes a
stage — an animated `UsdGeomCamera` plus one `Globe` prim — and `usdrecord`
renders the flight on any Hydra renderer. The DCC ecosystem (Solaris,
Omniverse) is a consequence of the same core, not the first target.

Renderers, in order: **Storm** (the GPU rasterizer that ships with OpenUSD —
open source, zero friction, closest to today's output), then **Cycles via
hdCycles** (the open-source GPU path tracer) for photorealism. Commercial
renderers (Karma, RenderMan) plug into the same core later; nothing in the
design may depend on any one of them.

## Architecture — a scene-index-shaped core behind the hdGp prim

The industry's asset-portable mechanism for generated content is the
`GenerativeProcedural` prim (doc 14): stages travel between studios, plugin
installations do not. And in Hydra 2.0 a generative procedural already speaks
the scene-index language — its children are `HdContainerDataSource`s. So the
two are one piece of work:

- **The core**: `tuileGlobeSceneIndex` — thin C++ over the `tuile-hydra`
  C ABI (in-process; the geometry server runs inside the host). It translates
  the stream into prim population: `Select`/`Content`/`Evict`, correlated by
  the protocol **generation** (doc: `ViewerState → Select`), become add/dirty/
  remove notices; tile meshes become mesh prims with baked-imagery materials.
- **Facade 1 (now)**: the `Globe` GenerativeProcedural of doc 14, resolved by
  hdGp — the asset that works in usdview, usdrecord and Solaris.
- **Facade 2 (later)**: direct insertion of the same core where a host offers
  it (Omniverse extension, Solaris scene index plugin) — no rework, the core
  is already the right shape.

Camera dependency follows doc 14's ladder unchanged (explicit
`primvars:tuile:cameras` first, `primaryCameraPrim` as the 25.05+ rung, fixed
geometric error as the view-independent floor).

## Imagery — bake the mosaic, one texture per tile

Today's wgpu renderer drapes up to N imagery layers in passes — a rasterizer
trick no offline renderer wants. The portable, fidelity-preserving form is to
**bake each tile's imagery mosaic into a single texture at decode time**
(the compositor already exists in `tuile-planetary`), exposed as
`UsdUVTexture → UsdPreviewSurface` first, MaterialX if a renderer needs
more. One representation, every renderer; and one day the wgpu path can draw
one pass instead of N from the same bake.

## The eager contract — identical, fatal

The recorder's decree transposes verbatim: a frame is never delivered soft.
The scene index refuses to hand Hydra a frame while `provisional() > 0`
(absent OR stand-in — stand-ins are disabled outright in exact rendering),
and the chroma-key pixel guard runs on `usdrecord` output exactly as it runs
on the wgpu recorder's. A slow source slows the farm; a dead one fails the
job loudly. No degraded frame ever reaches a deliverable.

## Trajectory

| M | Deliverable | Proves |
|---|---|---|
| M1 | `usdview` + Storm: a stage with one `Globe` prim streams live | the core, the C ABI, the camera ladder — 90 % of doc 14 |
| M2 | `usdrecord` batch: track → stage (animated camera) → video, eager-fatal | the product twin; `--dump-camera` output becomes the `UsdGeomCamera` |
| M3 | Solaris, then Omniverse | the facades; no core changes allowed |

Throughout: `tuile-usd` (doc 13, baked USDZ files) remains the zero-plugin
compatibility floor, unrelated to this crate by design.

## Backlog (M1 → M2)

1. Bake the per-tile imagery mosaic to one texture (Rust, before the ABI).
2. The C++ core: Hydra 2.0 data sources (topology, points, primvars,
   material) from the C ABI; add/remove notices on generation-correlated
   `Select`/`Evict`.
3. The hdGp prim + `plugInfo.json` + the camera ladder (doc 14, rungs 1–4).
4. Pinned OpenUSD build in CI (doc 14's Dockerfiles); the
   `HDGP_INCLUDE_DEFAULT_RESOLVER` trap asserted at plugin load, not just
   documented.
5. M2: the stage generator (track → animated camera + `Globe` prim) and the
   eager gate on the scene-index side; pixel guard on `usdrecord` output.

## Version posture

Develop pinned ≥ 25.05 (`primaryCameraPrim`); the camera ladder degrades
gracefully on 24.x hosts, and nothing else may assume a newer API without a
note here.
