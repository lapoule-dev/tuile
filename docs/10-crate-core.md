# 10 — `tuile-core` spec

The central asset. Everything in it is pure, testable, portable. If a decision makes you hesitate, optimize for: testability > performance > API ergonomics (the public API will be stabilized later).

## Modules

```
tuile_core
├── tileset      # serde types of tileset.json (1.0 + 1.1), external tilesets
├── implicit     # implicit tiling: URI templates, subtree decoding (bitstreams)
├── content      # glb / b3dm decoding → DecodedTileContent
├── geo          # WGS84 ↔ ECEF, ellipsoid, bounding regions → OBB
├── math         # f64: OBB, sphere, frustum, planes, point-volume distance
├── raster       # M2.5 — raster imagery overlay: ImageryProvider (z/x/y → texture),
│                #   WebMercator/Geographic projections, tile → UV mapping. Imagery
│                #   IS PART of the core (decision): we don't render bare terrain.
│                #   The ion transport is already ready (tuile-cesium-ion::ImageryEndpoint).
├── traversal    # SSE selection, refinement, prioritization — PURE FUNCTIONS
├── protocol     # streaming mode IS this module: ClientMessage / ServerMessage,
│                #   TileContent { Raw, Decoded }, GeometryStream trait + InProcessStream,
│                #   HttpPullStream (the network bindings live in tuile-server/tuile-wgpu)
├── runtime      # GeometryServer: state (residency, in-flight), orchestration
├── fetch        # TileFetcher trait + FsFetcher ; HttpFetcher behind a feature
├── cache        # ResidentCache LRU (byte budget)
└── render       # PrepareRenderResources trait + TileContent/DecodedTileContent
```

## Key types

```rust
pub enum TileContent {
    Raw { format: ContentFormat, bytes: Bytes }, // glb/b3dm — the only form that crosses a wire
    Decoded(DecodedTileContent),                 // what a renderer consumes — never serialized
}

pub struct DecodedTileContent {
    pub meshes: Vec<DecodedMesh>,      // LOCAL f32 positions (already rebased),
    pub textures: Vec<DecodedTexture>, // RGBA8 + dims (KTX2 transcoding will come as a feature)
    pub local_origin_ecef: DVec3,      // rebasing origin, f64
    pub transform_local: Mat4,         // f32, relative to local_origin
}

pub struct DecodedMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub uvs: Option<Vec<[f32; 2]>>,
    pub indices: Vec<u32>,
    pub material: MaterialDesc,        // base_color_factor, base_color_texture: Option<usize>
}

/// A view. Modeled on the reference implementations (model, not port).
/// The traversal accepts MULTIPLE views (visionOS/Hydra stereo, multi-viewport):
/// selection = union of the views, priority = best view per tile.
pub struct ViewState {                  // traversal input, ALL in f64, ECEF frame
    pub position: DVec3,
    pub direction: DVec3,               // normalized
    pub up: DVec3,                      // normalized
    pub viewport_px: DVec2,             // width AND height (SSE uses only the
                                        // height; foveation and culling use the rest)
    pub fovy_rad: f64,                  // symmetric perspective in v1; ortho and
                                        // general projection = post-v1 constructors
}
// derived at construction: view matrix, culling volume

pub enum PriorityGroup { Preload, Normal, Urgent }  // inter-group sort of requests

pub struct ContentRequest {
    pub tile_id: TileId,
    pub group: PriorityGroup,            // inter-group sort first
    pub priority: f64,                   // intra-group sort; v1: camera distance
}

pub struct TraversalOutput {
    pub selected: Vec<(TileId, f64)>,    // tile + current SSE (debug overlay, future fades)
    pub requests: Vec<ContentRequest>,   // sorted: group, then priority
    pub to_evict_hint: Vec<TileId>,
    pub stats: TraversalStats,           // visited, culled, max_depth… — the viewer overlay lives off it
}
```

## Decisions made

- **Rebasing lives in the core**, not in the backends: `DecodedTileContent` comes out in local f32 coordinates + f64 origin. All backends benefit, none can get it wrong.
- **The traversal is synchronous and pure — but the system IS NOT stateless.** State exists (residency, in-flight requests, selection history for hold-until-ready): it lives in the `GeometryServer` and ENTERS the traversal as explicit inputs (`ResidencyView` — which includes the previous frame's selection —, `frame: FrameNumber`), instead of being stored on the tiles (`_visitedFrame`/`_selectedFrame`-style fields of legacy engines). Decisive reason: on the streaming server side, N sessions share ONE immutable tile arena — any per-tile state would make the tree non-shareable without locks or copies. The reference engines are themselves migrating in this direction (selection state extracted from tiles into per-consumer "view groups"); we start directly from the destination. Perf: no per-frame allocations — `TraversalScratch` reused (arrays indexed by `TileId`, which replace the per-object caches of legacy engines).
- **Config**: names and defaults aligned with the reference implementations — `maximum_screen_space_error` (16.0), `forbid_holes` (our REPLACE hold-until-ready), `loading_descendant_limit` (20), `preload_ancestors`/`preload_siblings` — behavior comparable to the pixel.
- **Streaming mode is a core trait** (`protocol::GeometryStream`), not a network matter: `InProcessStream` (decoded, channels) and `HttpPullStream` (static pull) live here; the WebSocket binding lives in `tuile-server`/the clients. On the consumer side, `Content` is always `TileContent::Decoded`; on a wire, always `TileContent::Raw` (glb).
- **Errors**: `thiserror`, one enum per module, `#[from]` conversion. `UnsupportedContent(String)` for i3dm/pnts/cmpt.
- **IDs**: `TileId` = index into an arena (flattened `Vec<Tile>` + parent/child indices), no Rc/RefCell, no pointer graph. External tilesets graft into the same arena.
- **Decode off-frame**: `content::decode(TileContent::Raw, ContentHints) -> DecodedTileContent` is a free, blocking function, with no I/O — the caller puts it wherever they want (spawn_blocking, wasm worker, rayon).
- **Features**: `default = []`; `http` (reqwest); `draco`; `meshopt`; `ktx2`. The default build must stay lightweight and wasm-clean.

## Required tests (non-negotiable)

- `tileset`: serde round-trip on each fixture; 1.0 tileset with inherited refine; grafted external tileset.
- `math`: camera-OBB distance (inside, outside, on a face); frustum vs sphere/OBB, edge cases.
- `geo`: WGS84↔ECEF round-trip < 1e-9 m on a set of known points (equator, poles, negative altitude).
- `content`: b3dm fixture → same vertices as the equivalent glb; RTC_CENTER composed; up-axis verified on a known asymmetric mesh.
- `traversal`: on the hand-crafted mini-tileset —
  1. camera at infinity → selection = {root};
  2. camera glued → selection = leaves, requests ordered by SSE;
  3. REPLACE: non-resident children → parent stays selected; all resident → parent deselected;
  4. tile outside the frustum never selected nor requested.
- `cache`: budget respected to the byte; selected tile never evicted.
- `protocol`: end-to-end `InProcessStream` session on the mini-tileset (Hello → ViewerState → Select/Content/Evict consistent with the pure traversal on the same camera); determinism: same `ViewerState` sequence → same `Select` sequence (this is what will make it possible to test the parity of the network bindings in M2).

## What NOT to do in the core

- No logging to stdout (use `tracing`, feature-gated, debug level max by default).
- No real time (`Instant::now`) in the logic — inject via parameter if needed (test determinism).
- No avoidable per-frame allocations in the traversal: reuse buffers (`TraversalScratch` passed as &mut).
