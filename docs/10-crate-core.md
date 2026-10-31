# 10 — `tuile-core` spec

The central asset. Everything in it is pure, testable, portable. If a decision makes you hesitate, optimize for: testability > performance > API ergonomics (the public API is not stable yet; the workspace is pre-1.0).

Status as of v0.14.0: everything below is **shipped** unless marked *planned*.

## Modules

```
tuile_core
├── tileset      # serde types of tileset.json (1.0 + 1.1), the tile arena, external tilesets grafted in
├── tiles3d      # TilesetTree + TilesetLoader: a tileset driven as a TileTree/TileLoader
├── implicit     # implicit tiling: serde types only — subtree decoding is planned
├── content      # TileContent { Raw, Decoded }; glb / b3dm → DecodedTileContent; DrapeContent
├── texels       # the imagery format seam: Texels (RGBA8 or BC7/ASTC blocks), ImageryCompression,
│                #   GpuTextureCaps → ImageryPlan, select(), the CPU encoder entry point
├── stored       # imagery stored compressed (KTX2: UASTC, BC7, ASTC) — transcode only
├── geo          # WGS84 ↔ ECEF, ellipsoid, bounding regions → OBB
├── math         # f64: OBB, sphere, frustum, planes, point-volume distance
├── raster       # imagery: ImageryProvider (z/x/y → texture), WebMercator/Geographic schemes,
│                #   tile → UV mapping, reprojection, imagery layers. Imagery IS part of the core.
├── source       # TileTree / TileLoader: the abstraction the traversal and the server consume,
│                #   so 3D Tiles, quantized-mesh terrain and compositions share one engine
├── traversal    # SSE selection, refinement, prioritization — PURE FUNCTIONS
├── budget       # re-export of the `tuile-budget` crate: the graphics budget, one pure,
│                #   deterministic decide() per step — what is loaded, delivered, uploaded,
│                #   drawn and evicted, per memory class; Policy::Interactive (viewers) and
│                #   Policy::Batch (films)
├── protocol     # ClientMessage / ServerMessage, the GeometryStream trait + InProcessStream
├── runtime      # GeometryServer: the session loop around traverse() and Budget::decide();
│                #   loads inline (default, wasm) or as tasks on a host tokio handle
│                #   (native feature "rt-tokio", `Spawn::Tokio`)
├── drive        # SceneState / drive_until_complete: a consumer's side of the protocol, for bulk use
├── offload      # where blocking work runs: Inline, ThreadPool, or bytes to a worker (Portable)
├── fetch        # TileFetcher + FsFetcher (native); HttpFetcher behind the "http" feature
├── storage      # ContentStore: the byte cache a loader consults before fetching
├── ground       # GroundHeight: how high the ground stands under a point
├── seam, stitch # whether the ground is closed where two tiles meet, and the stitching that closes it
├── metrics      # counters and gauges, one vocabulary for tests, logs and endpoints
├── report       # a serialisable summary of the geometry produced
└── determinism  # digests that make "same inputs → same bytes" checkable
```

There is no `cache` module and no `render` module any more: residency belongs to the budget (`budget`), byte caching to `storage::ContentStore`, and preparing GPU resources to each backend (see `11-crate-wgpu.md`).

## Key types

```rust
pub enum TileContent {
    Raw { format: ContentFormat, bytes: Bytes }, // glb/b3dm — the only form that crosses a wire
    Decoded(DecodedTileContent),                 // what a renderer consumes — never serialized as glb
}

pub struct DecodedTileContent {
    pub meshes: Vec<DecodedMesh>,          // LOCAL f32 positions (already rebased)
    pub textures: Vec<DecodedTexture>,     // a glb's own textures
    pub imagery: Vec<raster::ImageryLayer>,// imagery attached to this tile (terrain)
    pub withheld_drape: Option<u64>,
    pub local_origin_ecef: DVec3,          // rebasing origin, f64
    pub transform_local: Mat4,             // f32, relative to local_origin
}

pub struct DecodedTexture {
    pub width: u32,
    pub height: u32,
    pub texels: texels::Texels,            // Rgba8 { pixels, resident } | Blocks { format, levels }
}

/// A tile's imagery of one layer at one level: a resource of its own,
/// delivered beside the mesh, so imagery changes without resending geometry.
pub struct DrapeContent { pub layer: LayerId, pub level: u32, pub tiles: Vec<raster::ImageryLayer>, /* … */ }

/// A view. The traversal accepts MULTIPLE views (stereo, multi-viewport):
/// selection = union of the views, priority = best view per tile.
pub struct ViewState { /* built from ViewStateParams */ }
pub struct ViewStateParams {           // ALL in f64, ECEF frame
    pub position: DVec3,
    pub direction: DVec3,              // normalized
    pub up: DVec3,                     // normalized
    pub viewport_px: DVec2,
    pub fovy_rad: f64,
    pub fov: Option<Fov>,              // asymmetric frusta (headsets)
    pub scale: f64,                    // detail scale
}
// derived at construction: frustum, SSE factors

pub enum PriorityGroup { Preload, Normal, Urgent }  // inter-group sort of requests
pub struct ContentRequest { /* tile, group, priority */ }
pub struct TraversalOutput { /* selected, requests (sorted), stats */ }
```

Imagery compression (`texels`):

```rust
pub enum ImageryCompression { Off /* default */, Auto, Format(BlockFormat) }
pub enum BlockFormat { Bc7, Astc4x4 }
pub struct GpuTextureCaps { pub bc: bool, pub astc: bool, pub compute: bool }   // filled by the host
pub struct ImageryPlan { pub target: ImageryFormat, pub encode: ImageryEncode } // None | Gpu | Cpu | Transcode
```

## Decisions made

- **Rebasing lives in the core**, not in the backends: `DecodedTileContent` comes out in local f32 coordinates + f64 origin. All backends benefit, none can get it wrong.
- **The traversal is synchronous and pure — but the system IS NOT stateless.** State exists (residency, in-flight requests, selection history for hold-until-ready): it lives in the `GeometryServer` and its budget and ENTERS the traversal as explicit inputs (`Residency`, the previous selection, the frame), instead of being stored on the tiles. Reason: N sessions could share ONE immutable tile arena — any per-tile state would make the tree non-shareable without locks or copies. No per-frame allocations in the traversal: scratch buffers are reused.
- **The graphics budget is one pure crate** (`tuile-budget`, re-exported here as `budget`, with its `TileId` re-exported from `source`; it depends on nothing in the workspace): it reads no clock, spawns nothing, touches no GPU, iterates no hash map; the same inputs give the same `Decisions`, bit for bit (`Decisions::digest`). The consumer executes its decisions and reports the bytes it really holds (`Ack`). Its invariants (I1–I15) and simulation are in `docs/explorations/graphics-budget-*.md`; CI runs the simulation and the properties in release (`budget-sim`).
- **Config**: names and defaults aligned with the reference implementations — `maximum_screen_space_error` (16.0), `forbid_holes` (our REPLACE hold-until-ready), `loading_descendant_limit`, `preload_ancestors`/`preload_siblings` — plus the globe's own (`horizon_culling`, `uniform_detail`, `refine_hysteresis`).
- **Streaming mode is a core trait** (`protocol::GeometryStream`), not a network matter. `InProcessStream` lives here; the browser's worker binding lives in `tuile-web`; a WebSocket binding would live in `tuile-server` (planned). On the consumer side, `Content` always carries decoded content.
- **Compression is the consumer's choice, off by default.** `ImageryCompression::Off` is byte-identical to the path without compression. `select(caps)` is a helper behind `Auto`, a pure function of `GpuTextureCaps`; nothing in tuile calls it on a consumer's behalf, and no licence or source policy is coded in tuile. A format the device cannot sample falls back to RGBA8.
- **Errors**: `thiserror`, one enum per module, `#[from]` conversion. `ContentError::Unsupported(String)` for i3dm/pnts/cmpt.
- **IDs**: a 3D Tiles `TileId` is an index into an arena (flattened tiles + parent/child indices), no Rc/RefCell, no pointer graph; external tilesets graft into the same arena. A terrain `TileId` packs `(level, x, y)`. The consumer never interprets a `TileId`: `Ancestry` travels with every message that names a tile.
- **Decode off-frame**: decoding is a blocking function with no I/O; the loader runs it through an `Offload` the host chooses (a thread pool natively, decode workers in a browser).
- **Features**: `default = []`; `http` (reqwest, native); `rt-tokio` (loads as tasks on a host runtime, native). The default build is wasm-clean.

## glTF extensions

- Supported today: plain glb (and the glb inside b3dm) with JPEG/PNG textures.
- *Planned* — **meshopt, built in**: reading `EXT_meshopt_compression` / `KHR_meshopt_compression` and `KHR_mesh_quantization` will be part of the core's glTF decoding, always on — **no `meshopt` feature**. It will rest on a fork of the pure-Rust `meshoptimizer-rs` crate under lapoule-dev, pinned by revision, with no C or C++ dependency, wasm-clean, and checked against the reference implementation's vectors. Nothing of it is on `main` yet.
- Not supported: `KHR_draco_mesh_compression`, `KHR_texture_basisu` on glTF textures. (KTX2 is read today for *imagery tiles* — `stored` — not for glTF textures.)

## Required tests (non-negotiable)

- `tileset`: serde round-trip; 1.0 tileset with inherited refine; grafted external tileset.
- `math`: camera-OBB distance (inside, outside, on a face); frustum vs sphere/OBB, edge cases.
- `geo`: WGS84↔ECEF round-trip < 1e-9 m on a set of known points (equator, poles, negative altitude).
- `content`: b3dm → same vertices as the equivalent glb; RTC_CENTER composed; up-axis verified on a known asymmetric mesh.
- `traversal`: on a hand-crafted mini-tileset — camera at infinity → root only; camera close → leaves, requests ordered by SSE; REPLACE: non-resident children → parent stays selected; all resident → parent deselected; a tile outside the frustum never selected nor requested.
- `budget`: the scenario simulation with its golden decision digests, the property tests, the per-class hard limits, the "never black" invariants (`lost`, `bare` = 0) — in the `tuile-budget` crate (`crates/tuile-budget/tests/`, `benches/budget_decide.rs`); `tests/budget_imagery.rs` stays here, since it drives the budget from the traversal's views.
- `protocol`/`runtime`: an end-to-end `InProcessStream` session (ViewerState → Select/Content/Evict consistent with the pure traversal on the same camera); determinism: the same `ViewerState` sequence gives the same `Select` sequence.
- `texels`/`stored`: the capability matrix of `select`; `Off` byte-identical; KTX2 fixtures (`tests/fixtures/uastc`) transcoded bit-exact; any bytes give a texture or an error, never a panic.

## What NOT to do in the core

- No logging to stdout (use `tracing`).
- No real time (`Instant::now`) in the logic — time is an input (test determinism).
- No avoidable per-frame allocations in the traversal: reuse buffers.
