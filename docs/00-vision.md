# 00 — Vision and scope

## Why this project exists

**The end goal of the project: a geometry server that produces renders of global tiles** — a terrain globe (Cesium World Terrain in quantized-mesh) draped with imagery (Bing), served and rendered in pure Rust, end to end. The 3D Tiles engine (glTF/b3dm geometry, photogrammetry, digital twins) is the foundation; the terrain+imagery globe is the target it scales up toward. Both pipelines share the same logical geometry server, the same f64 rebasing, the same cache, the same `GeometryStream`.

The OGC 3D Tiles standard is the dominant format for geospatial 3D streaming. Its ecosystem is lopsided:

- **Production**: rich (pg2b3dm, py3dtiles, mago 3DTiler, PLATEAU converter…) — all produce static files.
- **Consumption**: concentrated in a single commercial vendor (JS web client, C++ engine), plus a three.js renderer (NASA AMMOS).
- **Serving**: nonexistent. Either static on S3/nginx, or the SaaS from the same vendor (commercial self-hosted, Kubernetes).
- **Terrain+imagery globe**: no Rust engine consumes quantized-mesh terrain + raster imagery to produce a globe render. That is where we are going.

The 2D world solved the serving problem with **Martin** (MapLibre, Rust): a lightweight dynamic server, multiple sources, cache, one binary. Nobody has done it in 3D, neither for 3D Tiles nor for the terrain globe. And no mature native Rust/wgpu renderer exists for either.

**Data sources: ion AND non-ion.** The globe feeds on Cesium ion (Cesium World Terrain + Bing imagery, via a token supplied at runtime — never committed, cf. `tuile-cesium-ion`) as well as on equivalent free sources (open quantized-mesh terrain, slippy-map imagery). The core knows neither ion nor Bing: it consumes abstract terrain and imagery sources.

## Goals (in order)

1. **`tuile-core`**: the complete 3D Tiles logic (parsing, SSE traversal, scheduling, memory cache) in pure Rust, render-agnostic, wasm-compatible, structured as a **logical geometry server**: the streaming mode is a core trait (`GeometryStream`), not a networking matter. This is the central asset — everything else is a façade or a binding.
2. **`tuile-wgpu`**: a reference native + wasm renderer proving the core end to end.
3. **`tuile-server`**: the **network middleware** of the geometry server, plus the static mode. **Streaming mode (the primary mode)**: WebSocket binding of the `GeometryStream` trait — the server runs the traversal per session and pushes geometry, **serialized as glb on the wire**; first consumer the wgpu viewer (end-to-end tests), second consumer USD, notably the Hydra renderer. **Static mode**: tileset.json + glb contents over cacheable HTTP GET (pluggable cache Noop/LRU/disk/R2, abstract storage fs/S3/R2) — interop with the existing ecosystem, and the pull implementation of the same protocol as seen from the consumer. Native binary (axum) and Cloudflare Worker (workers-rs, static mode).
4. **`tuile-usd`**: USDZ export of a region (`bbox → USDZ`) as a standalone lib + CLI, including animations via time samples (replay of GPS trajectories). This is the differentiating feature: the only bridge from 3D Tiles to the Apple ecosystem (AR Quick Look iOS, RealityKit, visionOS). Strict separation: the tile server does not know about USD; export is a tileset consumer like any other.

## The terrain + imagery globe (structuring target)

Flagship goal: display Cesium World Terrain draped with Bing imagery, as a globe render. What this implies, added on top of the 3D Tiles engine without undoing it:

1. **quantized-mesh decoding** (`tuile-terrain`): 88-byte header, u/v/height vertices in u16 zigzag-delta, high-water-mark indices, skirts on the 4 edges, octvertexnormals extension. A `.terrain` tile → a `DecodedTileContent` (rebased ECEF positions, normals) — the same type as glTF content, so `tuile-wgpu` renders it without any change.
2. **Geographic implicit tiling**: TMS quadtree in EPSG:4326 (2 root tiles, whole world), availability described by a `layer.json` (not a `tileset.json`). Geometric error per level. The core's SSE traversal applies via a common source abstraction (3D Tiles and terrain are two `TileSource`).
3. **Draped raster imagery** (`core::raster`, already started): providers (slippy-map, Bing via ion, ion direct), WebMercator/Geographic projections, per-terrain-tile UV mapping. Geometry↔imagery crossover at a single point (`OverlayAttachment`).
4. **ion connectors** (`tuile-cesium-ion`): TERRAIN assets (endpoint resolution + layer.json + `.terrain` tiles, asset token refresh on expiry) and IMAGERY/BING (metadata → tile templates). ion attributions displayed (obligation).

## Non-goals (v1)

- **No tiling/tile generation** from raw sources (PostGIS, mesh). We serve and consume existing data. (On-the-fly generation = post-v1 track.)
- **No live Hydra/OpenUSD delegate** nor RealityKit client in v1 — but Hydra is the **designated second consumer of the streaming mode** (after the wgpu viewer): the `GeometryStream` trait and the future specialized façade `tuile-hydra` (there is no generic FFI façade — each host has its own: hydra, swift, unity) must be designed for it from the start, without refactoring the core.
- **No gRPC in v1.** The static mode is HTTP GET (CDN-cacheable); the streaming mode goes over WebSocket. Connect RPC is the chosen track for later (control plane), never pure gRPC as the sole interface.
- **No pnts point clouds** in v1 (a natural extension afterward).

## Legal and naming constraints

- The project is called **Tuile** (crates `tuile-core`, `tuile-wgpu`, `tuile-server`, `tuile-usd`; binary `tuile`). Name verified free on crates.io — to be reserved quickly (publish a 0.0.1 skeleton); check the GitHub org `tuile` / `tuile-rs` and possibly `tuile.dev`.
- The 3D Tiles spec is a community OGC standard, free to implement. No obligation.
- The open-source reference implementations of the standard are Apache-2.0: algorithmic inspiration is free and without obligation; literal porting (to be avoided) requires attribution (NOTICE + comment at the head of the ported code).
- No third-party brand in crate/type/binary names nor in public documentation: the project defines itself by the OGC standard, not relative to other products. Single exception: **connector** crates name the service they connect to (nominative use, e.g. `tuile-cesium-ion`) — and only them.
- No tokens or data from proprietary services (tile SaaS, Google, Bing) in the repo, the fixtures, or the examples. Fixtures = free tilesets only.
- License: `MIT OR Apache-2.0`. Copyright lapoule.dev.

## Target user v1

1. A dev who has a tileset (out of pg2b3dm or a photogrammetry pipeline) and wants to serve it cleanly: `tuile serve ./my-tileset/` and that's it.
2. A Rust dev who wants to display a tileset in their wgpu/Bevy app without embedding C++.
3. An iOS dev who wants a USDZ of a geographic area for AR Quick Look / RealityKit.
