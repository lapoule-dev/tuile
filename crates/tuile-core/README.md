<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-core

Render-agnostic OGC 3D Tiles engine: tileset parsing, screen-space-error
traversal, content decoding, and a **logical geometry server** exposed
through the [`protocol::GeometryStream`] trait.

The core never depends on a renderer, an async runtime or an HTTP
framework, and compiles to `wasm32-unknown-unknown` — that property is
CI-enforced. All geospatial math is f64 until positions are rebased onto
a local origin (the anti-jitter protocol of `docs/01-architecture.md`).

Module map (see `docs/10-crate-core.md`):
- [`tileset`] — tileset.json (1.0/1.1) serde types + flat tile arena
- [`implicit`] — implicit-tiling types (subtree decoding lands in M2)
- [`content`] — glb / b3dm → [`content::DecodedTileContent`]
- [`geo`] — WGS84 ↔ ECEF, regions → OBB
- [`math`] — f64 OBB / sphere / frustum / distances
- [`raster`] — imagery overlays: providers, tiling schemes, UV mapping
- [`traversal`] — pure SSE selection, multi-view
- [`protocol`] — the streaming protocol trait and in-process binding
- [`runtime`] — the geometry server driving it all
- [`tiles3d`] — the 3D Tiles binding of the source seam (tree + loader)
- [`drive`] — render-agnostic consumer driving (progressive + bulk)
- [`fetch`] — abstract byte source ([`fetch::FsFetcher`], `HttpFetcher`)
- [`budget`] — the graphics budget: pure, deterministic GPU residency decisions
  (the `tuile-budget` crate, re-exported)
- [`texels`] — the imagery format seam: GPU capabilities → plan, texels

## Where it sits

Near the bottom of the workspace: its only workspace dependencies are
`tuile-budget` (re-exported as [`budget`]) and `tuile-b3dm`. Every other crate builds on it — the connectors
(`tuile-cesium-ion`, `tuile-bing`), the data crates (`tuile-terrain`,
`tuile-planetary`, `tuile-pack`, `tuile-tile-server`), the renderers
(`tuile-wgpu`, `tuile-film-gpu`) and the hosts (`tuile-viewer`, `tuile-web`).

## Example

Parse a tileset and walk its tiles:

```rust
use tuile_core::tileset::Tileset;
use url::Url;

let json = br#"{
    "asset": { "version": "1.1" },
    "geometricError": 100.0,
    "root": {
        "boundingVolume": { "sphere": [0.0, 0.0, 0.0, 50.0] },
        "geometricError": 10.0,
        "refine": "REPLACE",
        "content": { "uri": "root.glb" }
    }
}"#;
let base = Url::parse("https://example.com/city/tileset.json")?;
let tileset = Tileset::from_json_bytes(json, &base)?;
assert_eq!(tileset.len(), 1);
let root = tileset.tile(tileset.root());
assert_eq!(root.geometric_error, 10.0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

- *(default)* — none: no async runtime, no HTTP client; builds for
  `wasm32-unknown-unknown`.
- `http` — `fetch::HttpFetcher`, a `TileFetcher` over `reqwest`.
- `rt-tokio` — loads run as tokio tasks on a runtime handle the host passes
  in (`runtime::Spawn::Tokio`). Native only; on wasm32 it compiles to
  nothing.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/02-3d-tiles-primer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/02-3d-tiles-primer.md)
- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
- [`docs/03-roadmap.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/03-roadmap.md)
