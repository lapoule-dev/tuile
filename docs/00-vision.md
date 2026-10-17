# 00 — Vision and scope

## What tuile is

**One foundation for the geospatial 3D scene, that runs everywhere and draws
in every way.** Tuile streams, decodes and renders the globe (terrain draped
with imagery) and OGC 3D Tiles content (glTF/b3dm geometry, photogrammetry,
digital twins), in pure Rust, from the same core, wherever a GPU or a browser
exists:

| Target | How it draws |
|---|---|
| **Browser** | the core in wasm in a Web Worker; WebGPU, or a JavaScript scene library on the page |
| **Desktop** | the native viewer (wgpu: Metal, Vulkan, DX12) |
| **Mobile** | the same viewer embedded in a phone app, with budgets, threading and lifecycle sized for it |
| **Headset** | two converged eyes from one scene: stereo frames, spatial video (MV-HEVC), and later live headset rendering |
| **Film** | frame-exact offline rendering of a camera path, on a laptop or a farm of GPUs |
| **Server** | the same geometry served to others: streaming over the wire, or static, cacheable 3D Tiles over HTTP |

**None of these is the product; the foundation is.** A film is one render mode
among the others, not a specialisation. Interactive, film, headset and mobile
share one traversal, one cache, one f64 rebasing, one atmosphere, one camera
model and one shading. A feature built for one mode (a physical sky, a per-frame
light, an overlay API for hosts, a stereo pair) is built in the core or in a
shared crate, so that every mode gets it. When two modes disagree about what is
on screen, the disagreement is a bug in a renderer, not a difference by design.

## Why it exists

The OGC 3D Tiles standard is the dominant format for geospatial 3D streaming.
Its ecosystem is lopsided:

- **Production** is rich (pg2b3dm, py3dtiles, mago 3DTiler, PLATEAU
  converter…), and all of it produces static files.
- **Consumption** is concentrated in a few clients, each tied to its own
  ecosystem and language, and none of them runs the same engine across browser,
  native, mobile, headset and offline rendering.
- **Serving** is static files on object storage, or a hosted service.
- **The terrain + imagery globe**: no Rust engine consumes quantized-mesh
  terrain with raster imagery to draw a globe.

The 2D world has lightweight, portable building blocks: a small dynamic tile
server, renderers on every platform. 3D has none that is open, portable and
renderer-independent. Tuile aims to be that block for 3D: small, embeddable,
compiled for every target, and owned by no renderer.

## Principles that make "everywhere" possible

1. **A render-agnostic, I/O-agnostic core.** `tuile-core` holds the 3D Tiles
   and globe logic (parsing, screen-space-error traversal, scheduling, memory
   cache) and depends on no GPU API, runtime or network stack. Fetching,
   storing and drawing plug in through traits (`TileFetcher`,
   `PrepareRenderResources`, `GeometryStream`). It compiles to
   `wasm32-unknown-unknown` on every change.
2. **A logical geometry server.** Streaming is a core trait (`GeometryStream`),
   not a networking matter. It has three bindings: in-process and decoded (the
   viewers, the film renderer), over a WebSocket with glb on the wire (remote
   hosts), and static HTTP pull (CDN interop and existing clients).
3. **One renderer family.** wgpu is the reference backend, native and WebGPU.
   Other hosts (OpenUSD/Hydra, platform scene graphs, game engines) get a
   specialised façade over the geometry server. There is no generic FFI.
4. **Precision by construction.** All geospatial arithmetic stays in f64 until
   the switch to a local frame. Raw Earth-centred coordinates never reach f32.
5. **Embedded-first resource discipline.** Every cache, pool and queue is
   bounded and sized from the device it runs on. The main thread never blocks.
   Ground is never black: a coarser ancestor stays drawn until its replacement
   is resident.
6. **Determinism where it is needed.** A camera path can be recorded and
   replayed exactly (`tuile-tape`). Film rendering is frame-exact and
   independent of how the frames are split across machines.
7. **Hosts stay outside.** A product that builds on tuile (its overlays, its
   data, its camera rules) plugs in through typed, neutral interfaces
   (overlays, per-frame light, segment orders). No downstream product is named
   in tuile, and downstream conventions are configuration.

## Goals

1. **`tuile-core`**: the complete 3D Tiles and globe logic, render-agnostic and
   wasm-compatible. This is the central asset; everything else is a façade, a
   binding or a renderer.
2. **The globe**: quantized-mesh terrain (`tuile-terrain`) crossed with raster
   imagery (`tuile-planetary`), from interchangeable sources. Commercial
   sources come through connector crates with runtime credentials, never
   committed; open sources (open terrain, open orthophotos, slippy maps) are
   first-class, not a fallback.
3. **Renderers for every mode** on one shading and one scene:
   - interactive native and browser (`tuile-wgpu`, `tuile-viewer`, `tuile-web`);
   - mobile;
   - stereo and spatial video;
   - offline film (`tuile-film-*`).
4. **A physically grounded look shared by every mode**:
   - an atmosphere and sky that follow the time and the place (`tuile-atmosphere`);
   - radiometric calibration of imagery (`tuile-radiometry`);
   - per-frame light.
5. **Serving** (`tuile-tile-server`, `tuile-pack-*`): the geometry server's
   network middleware, a tile store of PMTiles archives, and static 3D Tiles
   over cacheable HTTP. Deployable as a native binary or at the edge.
6. **Bridges to other ecosystems**, as consumers of the geometry server like
   any other: OpenUSD export and Hydra (`tuile-usd`, `tuile-hydra`), later
   platform-native façades.

## Non-goals (for now)

- **No tile generation** from raw sources (PostGIS, meshes). Tuile serves and
  consumes existing data, and on-the-fly generation stays a later track.
  Caching, re-packing and calibrating existing tiles are in scope.
- **No gRPC as the sole interface.** Static mode is HTTP GET (CDN-cacheable)
  and streaming is a WebSocket. Connect RPC is the chosen path for a control
  plane.
- **No pnts point clouds** yet. They are a natural extension.
- **No mode-specific fork of the engine.** A render mode that needs its own
  traversal, cache or shading is a design error to fix in the shared layers.

## Legal and naming constraints

- The project is called **Tuile**: crates `tuile-*`, binary `tuile`.
- The 3D Tiles spec is a community OGC standard, free to implement.
- The open-source reference implementations of the standard are Apache-2.0.
  Algorithmic inspiration is free and creates no obligation. Literal porting,
  to be avoided, requires attribution: NOTICE plus a comment at the head of the
  ported code.
- No third-party brand in crate, type or binary names, nor in public
  documentation: the project defines itself by the OGC standard, not relative
  to other products. Single exception: **connector** crates name the service
  they connect to (nominative use, e.g. `tuile-cesium-ion`), and only them.
- No tokens or data from proprietary services in the repository, the fixtures
  or the examples. Fixtures are free tilesets only. Derived layers built from
  commercial imagery are checked against its licence before they are stored or
  served.
- License: `MIT OR Apache-2.0`. Copyright lapoule.dev.

## Who it is for

1. **A web developer** who wants a globe or a tileset in a page, without a
   proprietary client.
2. **A Rust or game-engine developer** who wants geospatial 3D in a native, mobile
   or headset app, without embedding C++.
3. **A product** that needs the same scene interactive in its app, as a film
   for sharing, and in a headset, from one engine and one look.
4. **Someone with a tileset** who wants to serve it cleanly: `tuile serve
   ./my-tileset/` and that's it.
5. **A host ecosystem** (OpenUSD, platform scene graphs) that wants the
   geometry streamed in and drawn its own way.
