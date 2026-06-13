# 13 — `tuile-usd` spec

The differentiating feature: the only 3D Tiles → Apple ecosystem bridge (AR Quick Look on any iPhone/iPad, RealityKit, visionOS). Generation **in pure Rust, without OpenUSD binding**: USDA is text, USDZ is an uncompressed zip with an alignment constraint. No C++ dependency.

## USDZ format — packaging rules (normative, source of 90% of failures)

1. **zip** archive where EVERY entry is **stored** (compression 0).
2. The start of the DATA of each file must be aligned on **64 bytes** from the start of the archive → padding via the "extra" field of the local file header. Implement a dedicated writer (`UsdzWriter`); test the alignment byte by byte.
3. First file in the archive: the root `.usda` (or .usdc).
4. Embedded textures in PNG/JPEG referenced by internal relative path.
5. Validation: `usdview` (if available in CI via container) + homemade structural assertions (magic, alignments, readability by standard `zip`).

## USDA generation

Emit USDA text (readable, diffable, sufficient — binary usdc = post-v1 optimization).

Scene structure:

```usda
#usda 1.0
(
    defaultPrim = "Root"
    metersPerUnit = 1
    upAxis = "Y"
)
def Xform "Root" {
    def Scope "Tiles" {
        def Mesh "tile_0" ( ... ) {
            point3f[] points = [...]
            int[] faceVertexIndices = [...]
            int[] faceVertexCounts = [3, 3, ...]
            normal3f[] normals = [...]   (interpolation = "vertex")
            texCoord2f[] primvars:st = [...] (interpolation = "vertex")
            rel material:binding = </Root/Materials/mat_0>
        }
    }
    def Scope "Materials" {
        def Material "mat_0" { UsdPreviewSurface + UsdUVTexture st reader }
    }
}
```

Points of attention:
- **Coordinate frame**: USD/Quick Look = Y-up, meters. The content comes out of the core in local Z-up (rebased ECEF) → apply the inverse rotation. Origin of the USDZ = center of the requested bbox, placed so that the ground is at y=0 (Quick Look anchors on y=0).
- Materials: `UsdPreviewSurface` only (that is what Quick Look understands) — diffuseColor from texture via `UsdUVTexture` + `UsdPrimvarReader_float2` on `st`, metallic/roughness as constants.
- glTF indices (triangles) → `faceVertexCounts` filled with 3.
- Float formatting: bounded precision (6-7 decimals), no exotic scientific notation.
- Budget: option to merge meshes by material (reduces the number of prims, Quick Look does not like thousands of prims).

## Deliverable shape: lib + CLI, NEVER in the server

`tuile-usd` is a library plus a `tuile-export-usd` binary. **The tile server does not depend on this crate and exposes no USD route** — this is an architecture boundary, not a detail. The export consumes a tileset like any client (local path or HTTP URL, including a `tuile serve` instance), via the core's `TileFetcher`.

```bash
tuile-export-usd ./fixtures/demo/tileset.json --bbox -0.51,44.83,-0.49,44.85 --target-error 4 -o zone.usdz
tuile-export-usd https://tiles.example.com/tilesets/demo/tileset.json --bbox ... --track track.json -o replay.usdz
```

If an HTTP export service ever becomes relevant, it will be a separate binary composing this lib.

## Tile selection for export

Reuse the core's traversal in bounded mode, without a camera:
- Input: WGS84 `bbox` (degrees in the CLI, converted) + `target_error` (target level of detail) → we descend the tree as long as `geometricError > target_error` AND there is intersection with the bbox.
- Hard bounds: `max_tiles` (default 256), source `max_bytes` (default 128 MiB) → explicit error if exceeded, with a message indicating how to reduce (smaller bbox or larger target error).
- Determinism: same inputs → same output bytes (apart from timestamp metadata, disableable via `--reproducible`). This is what will allow any external orchestrator to cache the results.

## Animations — trajectory replay

Crate API (exposed by the CLI via `--track <file.json>`; input format: JSON `[{t, lat, lon, h}]`):

```rust
pub struct TrackSample { pub t_seconds: f64, pub position_wgs84: (f64, f64, f64) }
pub struct TrackExport {
    pub samples: Vec<TrackSample>,    // timestamped, monotonic
    pub marker: MarkerStyle,          // colored sphere v1; custom mesh post-v1
    pub resample_max_points: usize,   // default 2000 — Quick Look does not like 50k samples
}
```

Generation:
- An `Xform "Track/Marker"` prim with `double3 xformOp:translate.timeSamples = { 0: (...), 0.5: (...), ... }`.
- Stage metadata: `startTimeCode`, `endTimeCode`, `timeCodesPerSecond` (map t_seconds → timeCodes; `speedup` option to compress 4h into 60s).
- Resampling: spatio-temporal Douglas-Peucker or fixed step, capped at `resample_max_points`.
- Marker orientation along the trajectory (yaw from the current segment): optional, behind a flag.
- A static polyline of the full path (`BasisCurves`) under the marker, for visual context.

Quick Look constraints to respect (document in the crate's README): a single global timeline, automatic looping playback, no clip selection, no animated materials.

## Tests

- Golden files: generated USDA compared to committed snapshots (with float normalization).
- Packaging: 64-byte alignment verified programmatically on each entry; archive re-read by standard `zip-rs`.
- Geometry round-trip: known cube → USDA → homemade re-parse of the points → identity.
- Animation: monotonicity of the timeCodes, correct start/end bounds, resampling ≤ cap.
- Documented manual test: opening in AR Quick Look (link served locally + iPhone) and usdview — checklist in `docs/manual-tests.md`.
