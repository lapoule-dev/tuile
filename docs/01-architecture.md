# 01 — Architecture

## Workspace

```
tuile/
├── Cargo.toml                 # workspace, shared lints, release profile
├── LICENSE-MIT / LICENSE-APACHE / NOTICE
├── CLAUDE.md
├── docs/
├── fixtures/                  # test tilesets (royalty-free)
├── crates/
│   ├── tuile-b3dm/          # M1 — b3dm container codec (parse + encode, zero I/O)
│   ├── tuile-core/          # M1 — 3D Tiles logic, zero backend
│   ├── tuile-cesium-ion/    # ion source connector (endpoint, bearer, refresh ; 3D Tiles + imagery)
│   ├── tuile-wgpu/          # M1 — wgpu rendering backend (native + wasm)
│   ├── tuile-server/        # M2 — HTTP server (native axum / workers-rs)
│   └── tuile-usd/           # M3 — USD/USDZ export (lib + standalone CLI, outside the server)
├── examples/
│   └── wgpu-viewer/           # M1 — winit + tuile-wgpu binary (in-process M1, --remote M2)
├── integrations/              # future, non-Rust (hydra/, swift/) — empty in v1
└── references/                # local clones of the reference implementations — GITIGNORED, never committed
```

Future crates (planned, not created in v1): three **specialized façades** — `tuile-hydra` (OpenUSD/Hydra scene index plugin), `tuile-swift` (Apple: RealityKit on iOS/iPadOS/visionOS and native macOS), `tuile-unity` — there is NO generic FFI façade: each façade exposes what ITS host consumes, as close as possible to its idioms. Plus `tuile-wasm` (JS bindings for three.js). (`tuile-cesium-ion` already exists: endpoint resolution, bearer, refresh on 401, 3D Tiles and imagery assets.)

## Dependency rules (strict)

```
tuile-b3dm  → serde_json, thiserror (pure codec, zero I/O)
tuile-core  → tuile-b3dm, serde, serde_json, glam (f64), gltf, thiserror, bytes
                async-trait OR futures-core (async traits), NO runtime
tuile-cesium-ion → tuile-core (TileFetcher), reqwest behind a feature ;
                the core does not know that ion exists
tuile-wgpu  → tuile-core, wgpu, bytemuck ; reqwest+tokio behind the "native" feature,
                wasm-bindgen/web-sys behind the "web" feature
tuile-server→ tuile-core (parsing/validation), axum+tokio ("native" feature),
                worker ("cloudflare" feature), object_store (S3/GCS).
                FORBIDDEN : tuile-usd. The tile server does not know USD.
tuile-usd   → tuile-core, gltf, zip (stored/uncompressed writing), image (re-encode if needed)
```

Forbidden in the core: tokio, wgpu, axum, reqwest, std::fs in production paths (OK in tests). The core is async-runtime-agnostic: it exposes async traits and pure structures; it is the caller who supplies the execution.

## The foundational traits

The whole project rests on these seams (plus `GeometryStream` below, the master seam). The exact signatures may be refined, the responsibilities may not.

```rust
/// Abstract I/O: the core does not know where the bytes come from.
/// Impls : Reqwest (native), Fetch (wasm), Fs (tests), Mock.
#[async_trait]
pub trait TileFetcher: Send + Sync {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError>;
}

/// Bridge to the renderer: the core decodes, the backend materializes.
/// Impls : WgpuResources, (future) RealityKitResources, HydraResources.
/// Inspired by the equivalent contracts of the reference engines (model, not port).
pub trait PrepareRenderResources: Send + Sync {
    type Prepared: Send + Sync;
    fn prepare(&self, tile: &DecodedTileContent) -> Result<Self::Prepared, PrepareError>;
    fn free(&self, prepared: Self::Prepared);
}

/// Pluggable cache, removable depending on the runtime environment.
/// Impls : NoopCache (behind a CDN), MemoryLruCache, DiskCache (native),
/// R2Cache / CacheApi (Workers).
#[async_trait]
pub trait TileCache: Send + Sync {
    async fn get(&self, key: &CacheKey) -> Option<Bytes>;
    async fn put(&self, key: &CacheKey, value: Bytes);
}
```

wasm note: `Send + Sync` is a problem in single-threaded wasm. Use the feature-gated pattern (`maybe_send`): `Send` trait bounds only off-wasm. Do not block M1 on this — start native, generalize later.

## The geometry server: a trait first, a network server second

The executable heart of the project is a **geometry server**: traversal + scheduling + fetch + decoding. It is **not a network server** — it is a logical object. The streaming mode is developed **at the protocol level, as a core trait**; the transports are merely implementations of it.

```rust
/// THE project seam. The streaming protocol, independent of any transport.
pub trait GeometryStream {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError>;
    fn messages(&mut self) -> impl Stream<Item = ServerMessage>;
}

// ClientMessage : Hello { tileset, config } | ViewerState { views: Vec<ViewState> }
//                 | Ack { tile_id } | Cancel { tile_id }
// ServerMessage : Select { tile_ids } | Content { tile_id, TileContent }
//                 | Evict { tile_ids } | Error

/// The content of a tile, in its two forms. DecodedTileContent IS a
/// TileContent : it is the same Content message everywhere, only the form varies.
pub enum TileContent {
    Raw { format: ContentFormat, bytes: Bytes }, // glb/b3dm — the form that crosses the wires
    Decoded(DecodedTileContent),                 // the form that a renderer consumes
}
```

The bindings guarantee the form: a consumer (renderer, façade) always receives `Decoded`; a wire always carries `Raw` (glb). Decoding (`content::decode : Raw → Decoded`) lives in the binding, on the consumer side.

Three implementations, a single protocol:

1. **`InProcessStream` — the logical version, decoded end to end.** A `GeometryServer` (module `runtime`) runs in the consumer's process; messages pass through channels, zero serialization. First consumer: `tuile-wgpu`.
2. **`WsStream` — the network binding.** The remote server (`tuile-server`, the network middleware) runs the `GeometryServer` and serializes the protocol over WebSocket. **On the wire, geometry is serialized as glb** (standard, compact, Draco/meshopt/KTX2 compressions preserved — no homegrown format). The client binding decodes (the same `content::decode()` of the core) before yielding. Second consumer: USD, notably the Hydra renderer, via the specialized façade `tuile-hydra`.
3. **`HttpPullStream` — the "static mode" as seen from the consumer.** No remote geometry server: the `GeometryServer` runs on the consumer side with an HTTP `TileFetcher` that pulls static files (`tuile serve` in static mode, S3, nginx…). This is what makes Tuile compatible with the entire existing 3D Tiles ecosystem — and it is merely one particular implementation of the same trait.

A renderer written against `GeometryStream` works in all three cases without modification. Absolute invariant: what travels over a network is standard 3D Tiles (glb); `DecodedTileContent` exists only in memory, on the consumer side.

## Data flow (geometry server, in-process binding)

```
Camera (pose, projection, viewport)
   │
   ▼
Traversal (tuile-core) ── computes SSE per tile, frustum culling,
   │                         decides REPLACE/ADD, emits TileSelection
   ├──► tiles to render (already resident)
   └──► prioritized ContentRequests (descending SSE)
            │
            ▼
        TileFetcher.fetch() ──► decode (gltf / b3dm) ──► DecodedTileContent
            │
            ▼
        PrepareRenderResources.prepare() ──► handle GPU/scene
            │
            ▼
        ResidentCache (LRU, byte budget, eviction of non-selected ones)
```

Traversal is a pure function of (tree, camera, residence state) → (selection, requests). No I/O inside. That is what makes it testable and portable.

## Data flow (server)

```
HTTP Request ──► Router
   ├── GET /tilesets/{id}/tileset.json     ──► Storage → (core validation) → response
   ├── GET /tilesets/{id}/{path...}.glb    ──► TileCache.get ? otherwise Storage → put → response
Headers : ETag (sha256 content), Cache-Control: public, max-age=31536000, immutable
          (tiles are immutable by construction ; tileset.json less so : short max-age)
```

## Geospatial precision (absolute rule)

3D Tiles coordinates are in **ECEF EPSG:4978** (meters from the center of the Earth, order of magnitude 6.4e6). In f32, precision at this magnitude is ~0.5 m → guaranteed visual jitter.

Protocol:
1. All upstream computation (bounding volumes, cumulative transforms, SSE) in **f64** (`glam::DVec3`, `DMat4`).
2. Choose a **local origin** (center of the root bounding volume, or re-anchored near the camera).
3. `local = ecef - origin` in f64, THEN cast to f32 for the GPU.
4. The GPU view matrix is built relative to the same origin ("relative-to-center").
