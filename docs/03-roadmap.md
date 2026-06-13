# 03 — Roadmap and acceptance criteria

Implement in order. A milestone is only done when ALL of its criteria pass. Don't anticipate the features of later milestones (YAGNI), but never break the seams that make them possible.

> **Product north star**: the structuring target is **globe rendering** (Cesium World Terrain + Bing imagery via ion). The 3D Tiles engine (M1–M3) is the foundation; the terrain+imagery globe is the big piece (MG below), interleaved as soon as the M1 rendering foundation is proven.

## MG — Globe: quantized-mesh terrain + imagery (the target)

Scope: `tuile-terrain`, `core::raster`, `tuile-cesium-ion` (terrain + Bing), integration into the geometry server.

Deliverables:
1. **`tuile-terrain` — quantized-mesh-1.0 decoding**: header (center ECEF, min/max height, bounding sphere, horizon occlusion), u/v/height vertices (u16 zigzag-delta), high-water-mark indices, skirts on all 4 edges, `octvertexnormals` extension. Minimal encoder for tests (round-trip). A `.terrain` tile → `DecodedTileContent` (rebased ECEF, normals) — type shared with glTF content.
2. **TMS geographic tiling** (EPSG:4326, 2 root tiles): a tile's bbox, `layer.json` (per-level availability ranges), geometric error per level, u/v/height → ECEF conversion via `geo`.
3. **Source abstraction**: `TileSource` trait consumed by the SSE traversal — `TilesetSource` (3D Tiles, existing) and `TerrainSource` (terrain quadtree); the traversal, the cache, and the `GeometryServer` become generic over the source.
4. **`tuile-cesium-ion` terrain + Bing**: TERRAIN asset (endpoint → layer.json → tiles, gzip, refresh of the asset token on expiry ~1 h), IMAGERY/BING asset (`dev.virtualearth.net` metadata → `{quadkey}` templates). Attributions displayed.
5. **Draped imagery**: `core::raster` crossed with the terrain (UV per terrain tile), draping in `tuile-wgpu` (base color × overlay).
6. **Non-ion** variant: a free quantized-mesh terrain source (or a generated terrain) and free slippy-map imagery, proving that the core is unaware of ion.

Acceptance criteria:
- [ ] A real CWT `.terrain` tile decodes into a coherent mesh (vertices within the bbox, unit normals, skirts identified) — test on a local tile, never committed.
- [ ] The snapshot renders a real globe patch (CWT terrain) without jitter at any scale (proof of f64 rebasing on planetary data).
- [ ] The same patch draped with Bing imagery displays textured; ion attributions present.
- [ ] The traversal refines the terrain by SSE (close camera → deep levels) via `TileSource`, with no terrain-specific selection code.
- [ ] ion token read from `CESIUM_ION_TOKEN`, never present in the repo (verified); build without a token OK (free source).


## M1 — Core + wgpu viewer (the MVP that proves everything)

Scope: `tuile-core`, `tuile-wgpu`, `examples/wgpu-viewer`, fixtures.

Deliverables:
1. Complete serde parsing of tileset.json (1.0 + 1.1, external tilesets, implicit tiling types present even if subtree decoding may slip to M2).
2. Content decoding: glb direct + b3dm → `DecodedTileContent` (positions, normals, UV, indices, decoded RGBA8 textures, composed f64 transform, up-axis rotation applied).
3. Pure, tested SSE traversal: frustum culling, REPLACE with hold-until-ready, ADD, request prioritization.
4. `protocol` module: `ClientMessage`/`ServerMessage`, `TileContent { Raw, Decoded }`, `GeometryStream` trait, `InProcessStream` (decoded) and `HttpPullStream` bindings. The viewer consumes the geometry server EXCLUSIVELY through this trait — this is what guarantees that switching to `--remote` (M2) won't touch the renderer.
5. `TileFetcher`: `FsFetcher` (tests) and `HttpFetcher` (reqwest, native feature) impls.
6. `tuile-wgpu`: `PrepareRenderResources` impl (buffers, textures, bind groups), minimal PBR pipeline (base color texture + factor, Depth32Float depth), relative-to-center rendering.
7. `wgpu-viewer`: winit binary, orbital camera, loads a tileset by path/URL, displays stats (resident tiles, in-flight requests, GPU bytes).

Acceptance criteria:
- [ ] `cargo test --workspace` green; traversal coverage on the hand-crafted mini-tileset (cases: camera far → root only; camera close → leaves; REPLACE never displays parent + children simultaneously after stabilization).
- [ ] `cargo check --target wasm32-unknown-unknown -p tuile-core` green.
- [ ] The viewer correctly displays the b3dm AND glTF 1.1 fixtures, without jitter at max zoom (proof of f64 rebasing).
- [ ] No frame > 33 ms attributable to decode (decode off the render thread).
- [ ] Zero `unwrap()`/`panic!` in the core's production paths.

## M2 — Network middleware: WebSocket streaming + static mode + pluggable cache

Scope: `tuile-server`, client `WsStream` binding, `wgpu-viewer --remote`.

Deliverables:
1. **Network binding of streaming mode**: WebSocket endpoint (one session = one core `GeometryServer`), envelope serialization (versioned from the first byte), geometry as glb on the wire. Client `WsStream` binding (decodes on the client side) + `wgpu-viewer --remote ws://…`.
2. `tuile` native binary (axum): `tuile serve <dir|s3://|gs://>` via `object_store`. Routes: WS streaming, tileset.json, contents, health, basic metrics.
3. `TileCache` trait + impls: `NoopCache`, `MemoryLruCache` (byte budget), `DiskCache`. Selection by config/CLI — the cache is removable depending on the environment.
4. Correct HTTP headers (static mode): ETag, immutable Cache-Control on contents, configurable CORS, Range requests on glb.
5. Cloudflare Workers target (`cloudflare` feature, `worker` crate): static mode (same routes, R2 storage, cache = Cache API or Noop — the CDN does the work). Streaming on Workers = Durable Objects track, non-blocking. A `wrangler.toml` example in `examples/`.
6. Implicit tiling subtree decoding (deferred from M1 if applicable): the server can return a clean 404 on an unavailable tile.

Acceptance criteria:
- [ ] `wgpu-viewer --remote` displays the fixtures with visual AND selection parity vs local mode (same `Select` sequence for a replayed camera trajectory — programmatic test, not just visual).
- [ ] The main open source 3D Tiles web viewers (including three.js 3DTilesRenderer) consume a tileset served by `tuile serve` without error (manual test documented in `docs/`, test HTML pages committed).
- [ ] The M1 wgpu viewer works against the server in static mode (`HttpPullStream` — full homemade loop).
- [ ] Integration tests: end-to-end WS session (+ cancellation, timeout, malformed messages → Error), ETag/304, immutability, LRU respects the budget, NoopCache = passthrough.
- [ ] Workers build OK (`worker-build`), deployment documented step by step.

## M2.5 — Raster imagery (`core::raster` module)

Imagery is part of the core: indispensable for rendering terrain/untextured meshes. Scope:
1. `ImageryProvider` trait (z/x/y → decoded texture) + impls: generic slippy-map, ion (transport already in `tuile-cesium-ion`).
2. WebMercator/Geographic projections, mapping quadtree: for each geometry tile, choose the imagery tiles at the right level (density ~SSE), generate the overlay UV.
3. Extended `DecodedTileContent`: overlay UV sets + overlay texture references (the `Content` protocol carries them like the rest).
4. Draping in `tuile-wgpu` (base color × overlay multiplication).

Criteria: an untextured tileset + an imagery layer display draped in the viewer; attributions displayed; imagery memory budget respected.

## M3 — USD/USDZ export

Scope: `tuile-usd` only (lib + CLI binary). NO modification to `tuile-server` — the server is unaware of USD.

Deliverables: see `docs/13-crate-usd.md`. Summary: `DecodedTileContent` → USDA conversion + conformant USDZ packaging (stored zip, 64-byte alignment), `tuile-export-usd <tileset.json|URL> --bbox w,s,e,n --target-error N -o out.usdz` binary, time-sample animations from a timestamped polyline (trajectory replay).

Acceptance criteria:
- [ ] An exported USDZ opens in AR Quick Look (iOS) and `usdview` without a blocking warning.
- [ ] An export with an animated trajectory plays automatically in Quick Look.
- [ ] The export is deterministic (same inputs → same bytes, excluding timestamps) and bounded (configurable tile/byte limit, clear error beyond it).
- [ ] The CLI works against a local tileset AND against a tileset served by `tuile serve` (standard HTTP consumer, like any other client).

## M4 and beyond (do not implement — keep possible)

In priority order:

1. **`tuile-hydra`**: specialized OpenUSD facade — Hydra scene index plugin, **designated second consumer of streaming mode** (`WsStream` client, with the core's decoding embedded in the facade). This is the target that justifies the cleanliness of the `GeometryStream` trait.
2. **`tuile-swift`**: specialized Apple facade — RealityKit via LowLevelMesh, iOS/iPadOS/visionOS AND native macOS targets (stereo rendering on visionOS: this is what requires multi-view traversal). Priority raised: it's the project's third renderer, just behind wgpu and OpenUSD/Hydra.
3. `tuile-wasm`: JS bindings of the core for three.js.
4. `tuile-cesium-ion`: Cesium ion source connector — endpoint resolution (`/v1/assets/{id}/endpoint`), Authorization bearer, token refresh on 401, credits/attribution. A `TileFetcher` impl on top of the HTTP fetcher: zero changes in the core (this is the seam purity test). Take inspiration from the behavior of the reference clients (`references/`). Never a token in the repo; imagery tiles (raster overlay) are a separate feature, post-connector.
5. `tuile-unity`: specialized Unity facade.
5. Connect RPC: control plane (auth, index, streamed invalidations). Never a replacement for the HTTP data plane.
6. On-the-fly tile generation from GeoParquet/custom sources (the "complete Martin").
7. pnts / gaussian splatting.

There is NO generic FFI facade: each facade is specialized for its host.
