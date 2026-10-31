# 01 — Architecture

Status as of v0.14.0 (2026-10-25). Each part below says whether it is **shipped** (on `main`), **in review** (an open pull request) or **planned** (designed, not built).

## Workspace

```
tuile/
├── Cargo.toml                 # workspace: one dependency table, shared lints, rust-version 1.97
├── LICENSE-MIT / LICENSE-APACHE / NOTICE
├── CLAUDE.md
├── docs/                      # these documents; docs/explorations/ holds research notes and measurements
├── crates/
│   │   — the standard and the globe (render-agnostic)
│   ├── tuile-core/            # 3D Tiles parsing, SSE traversal, decoding, the geometry server, the protocol; re-exports the budget
│   ├── tuile-budget/          # the graphics budget: pure, deterministic decide(); at the bottom of the graph
│   ├── tuile-b3dm/            # the b3dm container: parse + encode, zero I/O
│   ├── tuile-terrain/         # quantized-mesh decode/encode, geographic tiling, heightfield source, terrain over HTTP
│   ├── tuile-planetary/       # terrain × imagery as one TileTree + TileLoader
│   ├── tuile-camera/          # globe camera and input controller → ViewState
│   ├── tuile-atmosphere/      # sun position, aerial perspective, sky
│   ├── tuile-ui/              # on-screen controls (compass, tilt, zoom), no renderer
│   ├── tuile-overlay/         # geometry a host hands a renderer to draw in the world
│   │   — sources and transports
│   ├── tuile-cesium-ion/      # connector: asset endpoints, tokens, 3D Tiles, terrain, imagery
│   ├── tuile-bing/            # connector: imagery metadata, quadkeys, tiles
│   ├── tuile-native-fetchers/ # pooled HTTP client + browser-style disk/memory cache (native)
│   ├── tuile-storage-foyer/   # memory-over-disk ContentStore (native)
│   ├── tuile-web/             # browser transport: fetch, and the protocol over postMessage
│   │   — rendering and applications
│   ├── tuile-wgpu/            # the reference wgpu backend (native, WebGPU, WebGL2)
│   ├── tuile-bevy/            # the globe in a Bevy application: tuile-wgpu's renderer and budget on Bevy's device, in its colour and depth
│   ├── tuile-viewer/          # the interactive viewer as a library: desktop, iOS, Android and web shells
│   ├── tuile-viewer-bundle/   # packages the viewer as a macOS application, and the phone resources
│   ├── tuile-hydra/           # C ABI over the geometry server, for the OpenUSD/Hydra plugin
│   ├── tuile-usd/             # a recorded camera path + a Globe prim, as a .usda manifest stage
│   │   — films: bake once, render often
│   ├── tuile-tape/            # record a camera path (MCAP) and fly it again, exactly
│   ├── tuile-bake/            # resolve a frame range into a pack
│   ├── tuile-pack/            # the pack container (FlatBuffers + LZ4)
│   ├── tuile-film/            # a baked film read frame by frame — no GPU, no I/O
│   ├── tuile-film-gpu/        # a film on wgpu: one visibility raster, the rest in compute
│   ├── tuile-film-native/     # a film rendered natively to an mp4
│   ├── tuile-radiometry/      # radiometric harmonisation of imagery tiles
│   ├── tuile-mp4/             # H.264 samples into an mp4, in memory
│   │   — storage and serving
│   ├── tuile-tile-server/     # the source-tile store: PMTiles archives per zone and layer on object storage
│   ├── tuile-repository/      # repositories over the buckets; the read API as request → reply
│   ├── tuile-pack-api/        # a native, read-only HTTP host of that API
│   ├── tuile-farm/            # a render farm's object storage: ranged reads, multipart writes, film assembly
│   └── tuile-metrics/         # the core's counters on a Prometheus endpoint
├── examples/
│   ├── wgpu-viewer/           # `tuile-wgpu-viewer`: tuile-viewer over the public connectors (see 16-embedding-the-viewer.md)
│   ├── bevy-viewer/           # a Bevy application over tuile-bevy (native, WebGPU, WebGL2)
│   ├── heightfield-serve/     # a synthetic relief served as quantized-mesh + XYZ imagery: the token-free source
│   ├── catalog-proxy/         # holds the connector token server-side for the web page and the APK
│   ├── globe-bulk/ warm-cache/ orbit-probe/   # headless harnesses: bulk frames, cache warming, traversal probe
│   └── web-viewer/ web-viewer-vite/           # pages importing the web package
├── packages/tuile-viewer/     # the ES module a page imports: the viewer in wasm, its geometry server in a worker
├── integrations/              # non-Rust: hydra/ (C++ plugin), blender/, bake/, film/ (images and job scripts)
├── scripts/                   # try-viewer.sh (web, mac, ios, android), web-bench.py
├── site/                      # the project site (Astro) and the API reference
└── references/                # local clones of the reference implementations — GITIGNORED, never committed
```

Shipped: **`tuile-bevy`** (#136 native, #142 web on WebGPU and WebGL2) — the globe in a Bevy 0.20 application, drawn by `tuile-wgpu`'s renderer and budget on Bevy's device, into Bevy's colour and depth targets — with `examples/bevy-viewer`.

A crate of our own outside the workspace: **`texcomp`** (`github.com/lapoule-dev/texcomp`, pinned by tag in `Cargo.toml`), the GPU compression kernels and the CPU encoders and transcoders behind imagery compression.

Not built, and not scheduled today: `tuile-server`, the network middleware (`12-crate-server.md`); a RealityKit façade (`tuile-swift`) — phones and tablets are served today by the wgpu viewer's iOS and Android shells; JS bindings of the core for third-party web renderers — the browser is served today by `packages/tuile-viewer`; a Unity façade. There is still NO generic FFI façade: each façade exposes what its host consumes (`tuile-hydra` is the one that exists).

## Dependency rules (strict)

```
tuile-budget     → thiserror (pure, deterministic: no clock, no GPU, no runtime, no I/O, no other
                   tuile crate; owns TileId)
tuile-b3dm       → serde_json, thiserror (pure codec, zero I/O)
tuile-core       → tuile-budget (re-exported as `budget`), tuile-b3dm, serde, glam (f64), gltf, image, texcomp (no GPU feature), bytes,
                   async-trait + futures (async traits), thiserror, tracing — NO runtime;
                   reqwest only behind the optional "http" feature, tokio only behind the
                   optional native feature "rt-tokio"
tuile-terrain    → tuile-core                      (all I/O through TileFetcher)
tuile-planetary  → tuile-core, tuile-terrain       (generic over TerrainSource × ImageryProvider)
tuile-camera, tuile-ui, tuile-atmosphere → tuile-core (renderer-agnostic)
tuile-cesium-ion, tuile-bing → tuile-core (+ terrain, planetary); the core does not know they exist
tuile-wgpu       → tuile-core, tuile-atmosphere, tuile-overlay, wgpu, texcomp ("gpu"), bytemuck
tuile-viewer     → tuile-wgpu, tuile-planetary, tuile-camera, tuile-ui, winit; native fetchers and
                   foyer on native, tuile-web on wasm
tuile-hydra      → tuile-core, tuile-bake, tuile-pack — a C ABI and nothing else
tuile-usd        → tuile-core, tuile-tape — writes text, links no OpenUSD
tuile-bevy       → tuile-wgpu, tuile-planetary, bevy (render + core pipeline); no shader and
                   no executor of its own: tuile-wgpu's HostPass draws into the camera's targets
```

The graphics budget is its own crate, `tuile-budget`, at the bottom of the graph (decided 2026-10-31): it depends on no other tuile crate and owns the tile handle `TileId`; `tuile-core` depends on it and re-exports it whole (`tuile_core::budget`, and `TileId` from `tuile_core::source`), so every path into the budget goes through the core unchanged. Everything forbidden in the core is forbidden there too, and its own lints hold crate-wide: no iteration over a hash container, no narrowing `as`, no float arithmetic in the decision engine and its state. Its simulation, property tests, goldens and the `decide` bench live in it.

Forbidden in the core: wgpu or any renderer, axum or any HTTP framework, reqwest outside its own optional `http` feature, `std::fs` in production paths (OK in tests), and tokio unconditionally. The core is async-runtime-agnostic: it exposes async traits and pure structures, and the caller supplies the execution. Under `rt-tokio` the caller supplies it as a runtime handle (`runtime::Spawn::Tokio`) on which the geometry server spawns its loads; the core never builds a runtime. On wasm the loads stay in the server's own `FuturesUnordered` (`Spawn::Inline`). CI checks the rule on every pull request: the `wasm-guard` job builds the budget, the core and every crate that claims wasm for `wasm32-unknown-unknown`, with `-D warnings`.

## The foundational traits (shipped)

The signatures below are abridged; rustdoc is the authority.

```rust
/// Abstract I/O: the core does not know where the bytes come from.
/// Impls: NativeHttp (tuile-native-fetchers), WebFetcher (tuile-web), FsFetcher
/// (native: tests, local files), HttpFetcher (core, "http" feature), IonTileFetcher.
pub trait TileFetcher: Send + Sync {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError>;
    async fn fetch_cacheable(&self, url: &Url) -> Result<Fetched<Bytes>, FetchError>; // + freshness
}

/// A byte cache a loader consults before fetching — a cache, not a database:
/// any get may miss, every value can be recomputed. Impls: memory over disk
/// (tuile-storage-foyer), the tile store's archives (tuile-tile-server), in-memory.
pub trait ContentStore: Send + Sync {
    async fn get(&self, key: &str) -> Option<Bytes>;
    async fn put(&self, key: &str, value: Bytes, ttl: Option<Duration>);
}

/// The tile hierarchy as the traversal sees it: pure, synchronous.
/// Impls: TilesetTree (3D Tiles), the terrain quadtree, CompositeTileTree.
pub trait TileTree: Send + Sync { fn roots(&self) -> Vec<TileId>; fn children(&self, id: TileId) -> Vec<TileId>; /* … */ }

/// Loads and decodes a tile, and composes its imagery drapes. Async, off the frame.
/// Impls: TilesetLoader (3D Tiles), PlanetaryLoader (terrain × imagery).
pub trait TileLoader: Send + Sync {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError>;
    async fn load_drape(&self, id: TileId, layer: LayerId, level: u32) -> Result<DrapeContent, LoadError>;
    /* drapes, holds, warm_up … */
}

/// Raster imagery: z/x/y → decoded texture, per tiling scheme.
pub trait ImageryProvider: Send + Sync { /* … */ }

/// How high the ground stands under a point, for a camera that must not enter it.
pub trait GroundHeight: Send + Sync { /* … */ }

/// Where blocking work (decode, reprojection, CPU encode) runs: Inline, a ThreadPool,
/// or — in a browser — decode workers reached with bytes (`Portable`, `Remote`).
pub trait Offload: Send + Sync { fn submit(&self, job: Job); /* remote … */ }
```

Two traits of the original plan were not built in that form. `PrepareRenderResources` became the backend's own business: `tuile-wgpu` turns delivered content into GPU resources (`prepare` → `PreparedTile`) and tells the server what it holds through the protocol (`Ack { resource, gen, bytes }`), which is the contract every backend shares. `TileCache` became `ContentStore` (string keys, the origin's TTL, best-effort writes).

wasm note: the async traits drop their `Send` bound on wasm32 (`async_trait(?Send)` under `cfg(target_arch = "wasm32")`).

## The geometry server: a trait first, a network server second

The executable heart of the project is a **geometry server**: traversal + graphics budget + fetch + decode. It is **not a network server** — it is a logical object (`runtime::GeometryServer`). The streaming mode is defined **at the protocol level, as a core trait**; transports are implementations of it.

```rust
/// THE project seam. The streaming protocol, independent of any transport.
pub trait GeometryStream: Send {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError>;   // non-blocking
    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerMessage>>;
}

// ClientMessage: ViewerState { views, generation } | Ack { resource, gen, bytes }
//                | Discarded { resource, gen } | Pressure | Thermal | Retarget { total_bytes }
//                | ReleaseBeyond { bytes } | Ahead { views }
// ServerMessage: Select { draw, plan, ancestry, stats, hard_limit, frame_ready, converged, lost, bare, generation }
//                | Content { resource, gen, class, ancestry, content } | Cancel | Evict | Error | Priming

/// The content of a tile, in its two forms.
pub enum TileContent {
    Raw { format: ContentFormat, bytes: Bytes }, // glb/b3dm — the form that crosses wires
    Decoded(DecodedTileContent),                 // the form a renderer consumes
}
```

The server decides and the consumer executes. `Select` carries the draw set — built from acknowledged resources only, so drawable by construction — and the upload plan in priority order; the consumer uploads within its frame's deadline, acknowledges each upload with the bytes it really holds, and releases what `Evict` names after the draw that stops using it. A tile's mesh and each of its imagery drapes are separate resources (`ResourceContent::Mesh` / `Drape`), so imagery changes without resending geometry.

Bindings, one protocol:

1. **`InProcessStream` — shipped.** The `GeometryServer` runs in the consumer's process; messages cross a channel pair, nothing is serialised. Consumers: the native viewer, the film bake, the headless harnesses, `tuile-hydra`.
2. **The worker binding — shipped.** In a browser the geometry server runs in a Web Worker, and each message crosses `postMessage` as one transferred buffer (`tuile-web::wire`, a private format between two halves of the same wasm module, read back into the same types). Consumer: the web viewer.
3. **Static HTTP pull — shipped, without a type of its own.** A `GeometryServer` on the consumer's side over an HTTP `TileFetcher` reading static files is the "static mode" seen from the consumer; it is how every viewer reads terrain, imagery and tilesets today. The `HttpPullStream` type of the original plan was never needed.
4. **`WsStream` — planned.** A remote `GeometryServer` per session in `tuile-server`, geometry on the wire as glb, decoded on the client by the core's `content::decode`. Not built; its transport is an open decision (D-T1, `12-crate-server.md`).

**Where the `GeometryServer` runs** (decided 2026-10-19, `explorations/graphics-budget-architecture.md` D-A5). It owns the graphics budget, and runs where that budget's inputs are. On an interactive device — desktop, phone, headset, browser — it runs locally (`InProcessStream`, or a Web Worker on the web): memory pressure must act at once, the local store's presence index is synchronous, and a head-rate rotation cannot wait a round trip. For a consumer that declares its budget — a render node, a farm, a consumer that cannot link the core — it would run in a `tuile-server` session (planned), its budget fed by the declared memory and the consumer's `Ack` bytes.

A renderer written against `GeometryStream` works over every binding without modification. Invariant: what travels over a network is a standard format (glb, quantized-mesh, image tiles); `DecodedTileContent` exists only in the consumer's memory.

## Data flow (geometry server, in-process binding)

```
ViewerState (one ViewState per view: pose, projection, viewport)
   │
   ▼
Traversal (tuile-core) ── SSE per tile, frustum culling, REPLACE/ADD, hold-until-ready,
   │                       the ancestor chain and the pinned coarse floor
   ▼
Budget::decide (tuile-budget)     ── pure and deterministic: what to load, cancel, deliver,
   │                                   draw and evict, per memory class, under the hard limits
   ├──► TileLoader::load / load_drape ──► TileFetcher (+ ContentStore) ──► decode off the frame (Offload)
   ├──► Content { resource, … }  ──► the consumer uploads in plan order, within its frame deadline ──► Ack { bytes }
   ├──► Select { draw, plan, … } ──► the consumer draws (new before old: never a gap)
   └──► Evict                    ──► the consumer releases after the draw that stops using it
```

Traversal and budget are pure functions of their inputs (tree, views, residency, arrivals, platform signals) — no I/O, no clock inside. That is what makes them testable, portable and replayable (`core::determinism`, the budget simulation run by CI's `budget-sim` job).

## Imagery formats: the consumer's choice (shipped)

`core::texels` is the seam between decoded imagery and the GPU. A texture's texels are `Texels::Rgba8` or `Texels::Blocks` (BC7 or ASTC 4×4, with mips), and the format it will be held in is recorded on the texture, so the budget charges what will really be resident. **Compression is the consumer's choice and is off by default** (`ImageryCompression::Off`, byte-identical to the uncompressed path). A consumer that opts in describes its adapter with `GpuTextureCaps` (no graphics-API type) and resolves an `ImageryPlan`: GPU encode at upload (`tuile-wgpu`, with texcomp's kernels), CPU encode (the web viewer's decode workers when the page has no GPU encoder), or transcode of imagery stored as KTX2 (`core::stored`). An encode that cannot be done uploads RGBA8 instead — the ground is never left untextured. Batch renders (bake, film) stay RGBA8. Accounting the encode's GPU time and transient memory inside the budget is planned (`explorations/` — the compression budget, B1–B8).

## Host embedding (shipped)

- **A frame the host owns** — `tuile_wgpu::HostPass`: an engine hands tuile its device, colour target, depth buffer and encoder; tuile records its pass and submits, presents and clears nothing it was not asked to. Any sample count, forward or reverse-Z, with or without a stencil (a companion depth-stencil when the host's depth has none). The viewer's own frames go through the same pass. `tuile-bevy` is built on it.
- **The viewer as a library** — `tuile_viewer::ViewerHost` says where a session's terrain and imagery come from (`16-embedding-the-viewer.md`); `frame_host` lets an application place the camera every frame and read the ground under it.

## Data flow (server) — planned

```
HTTP Request ──► Router
   ├── GET /tilesets/{id}/tileset.json     ──► Storage → (core validation) → response
   ├── GET /tilesets/{id}/{path...}.glb    ──► ContentStore.get ? otherwise Storage → put → response
Headers : ETag (content hash), Cache-Control: public, max-age=31536000, immutable
          (tiles are immutable by construction ; tileset.json less so : short max-age)
```

What serves today is the film and tile-store API of `tuile-repository` (request → reply, hosted by `tuile-pack-api`), the `heightfield-serve` example and the catalog proxy — see `12-crate-server.md`.

## Geospatial precision (absolute rule)

3D Tiles coordinates are in **ECEF EPSG:4978** (meters from the center of the Earth, order of magnitude 6.4e6). In f32, precision at this magnitude is ~0.5 m → guaranteed visual jitter.

Protocol:
1. All upstream computation (bounding volumes, cumulative transforms, SSE) in **f64** (`glam::DVec3`, `DMat4`).
2. Choose a **local origin** (center of the root bounding volume, or re-anchored near the camera).
3. `local = ecef - origin` in f64, THEN cast to f32 for the GPU.
4. The GPU view matrix is built relative to the same origin ("relative-to-center").
