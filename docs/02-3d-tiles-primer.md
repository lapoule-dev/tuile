# 02 — 3D Tiles primer (what to implement)

Normative reference: OGC 3D Tiles 1.1 — https://docs.ogc.org/cs/22-025r4/22-025r4.html
Behavioral reference (gray areas): the open source reference implementations of the standard, as local clones in `references/` (gitignored, never committed) — on the web side, the traversals under `packages/engine/Source/Scene/`; on the C++ side, the 3D Tiles selection module. (Apache-2.0 — free inspiration, attribution if literal port.)

## 1. tileset.json

Tile tree. Fields to model (serde):

- `asset.version` ("1.0" / "1.1"), root `geometricError`.
- `root`: recursive. Each `Tile`: `boundingVolume`, `geometricError`, `refine` ("REPLACE" | "ADD", inherited if absent), `transform` (column-major mat4, optional, composes with parents), `content.uri` (optional — an internal tile without content is possible), `children`.
- `content.uri` can point to **another tileset.json** (external tileset) → tree grafting at load time. Must be supported from M1 (very common).
- Bounding volumes, the three variants:
  - `box`: 12 floats (center + 3 half-axes) — OBB.
  - `region`: 6 floats (west, south, east, north in **radians**, minHeight, maxHeight) — WGS84.
  - `sphere`: 4 floats (center + radius).
- **Implicit tiling (1.1)**: `implicitTiling` on root — `subdivisionScheme` (QUADTREE/OCTREE), `subtreeLevels`, `availableLevels`, URI templates `{level}/{x}/{y}[/{z}]`. Availability is encoded in **binary subtree** files (availability bitstreams: tile/content/childSubtree). To implement in M1 if simple, otherwise M2 — but the serde types must exist from M1.

## 2. Content formats

- **glTF/GLB direct (1.1)**: the content IS a glb. `gltf` crate (`import_slice`). Main path.
- **b3dm (1.0 legacy)**: 28-byte header — magic "b3dm", version u32, byteLength u32, featureTableJSONByteLength, featureTableBinaryByteLength, batchTableJSONByteLength, batchTableBinaryByteLength — then the tables, then an embedded glb → same glTF path afterwards. Read `RTC_CENTER` from the feature table if present (translation to compose).
- **i3dm, pnts, cmpt**: out of scope for v1. Return a typed `UnsupportedContent` error, not a panic.
- glTF extensions to handle at decode time: `KHR_draco_mesh_compression` (optional feature, behind a flag), `EXT_meshopt_compression` (`meshopt` crate), `KHR_texture_basisu` (KTX2 — transcoding via `basis-universal` or pass the bytes through to a backend that supports it). In M1: uncompressed meshes + JPEG/PNG textures are enough; the extensions are M2+ features.

## 3. Coordinates and up-axis

- Tileset frame: ECEF (Z-up). glTF frame: Y-up, meters. The spec mandates an implicit Y-up→Z-up rotation applied to glTF content (+90° rotation around X). Don't forget it — classic mistake #1 (models lying on their side).
- Tile `transform`s compose from root to leaves: `world = parent_world * tile.transform`.
- WGS84 (lat/lon/h) ↔ ECEF conversions are needed for bounding regions and USD export: implement in `core::geo` (standard WGS84 ellipsoid formulas, in f64).

## 4. Screen-Space Error and traversal (the core)

For each visible tile:

```
sse = (geometricError * viewportHeight) / (distanceToCamera * 2 * tan(fovY / 2))
```

- `distanceToCamera`: distance from camera → bounding volume (not to the center: to the volume; clamp to ~epsilon if the camera is inside).
- If `sse > maximumScreenSpaceError` (default: 16) → the tile is too coarse → refine (descend to children).
- **REPLACE**: children replace the parent. Subtlety: only hide the parent when ALL selected children are resident (otherwise visual holes). Implement "hold until children ready".
- **ADD**: children add to the parent (the parent stays displayed).
- Frustum culling on the bounding volume before any computation (frustum planes vs OBB/sphere/region — for region, conversion to an approximating OBB is enough in v1).
- Traversal output: `selected` (tiles to render, with their SSE), `requests: Vec<ContentRequest>` sorted by priority group (Preload/Normal/Urgent) then by intra-group priority (v1: increasing camera distance; foveation/depth = post-v1 refinements).
- Cancellation: an in-flight request for a tile that has left the selection must be cancellable (the caller drops the future; the core just marks the state).

## 5. Lifecycle and memory budget

Tile states: `Unloaded → Loading → Decoded → Resident → (Evicted)`.
Resident cache: LRU bounded by a byte budget (default 512 MiB, configurable). Eviction: never a tile currently selected; LRU among the non-selected ones.

## 6. Test fixtures

Build `fixtures/` with:
- The official sample tilesets from the 3D Tiles spec repo (`Samples/` — permissive license, verify and note in NOTICE): at minimum a simple b3dm tileset, a glTF 1.1, an external tileset, an implicit tiling.
- A hand-crafted mini-tileset (cube + 4 children) for the traversal unit tests, committed in readable JSON.
- NO data from proprietary services (tile SaaS, Google, Bing).

## 7. Known pitfalls (review checklist)

- [ ] Y-up/Z-up rotation applied to glTF content.
- [ ] `geometricError = 0` on leaves → never any refinement attempt.
- [ ] `refine` inherited from the parent when absent.
- [ ] Regions in radians, not degrees.
- [ ] `transform` column-major (like glTF).
- [ ] b3dm RTC_CENTER composed into the transform.
- [ ] f64 everywhere before rebasing.
- [ ] External tilesets: junction geometricError = that of the referencing tile.
