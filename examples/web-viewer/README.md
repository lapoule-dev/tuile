<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# web-viewer — the streaming globe, in a browser

The same engine as `wgpu-viewer`, rendered by THREE.js instead of wgpu.

Not a demo and not a port: the **real** `GeometryServer`, the real SSE traversal
and the real planetary loader run inside a Web Worker (`tuile-web`), and the page
only draws what they select. That division is what makes this useful beyond the
browser — if this page and the native viewer disagree about what is on screen,
the disagreement is in a *renderer*, not in the engine.

## Build and run

```bash
wasm-pack build crates/tuile-web --target web --out-dir ../../examples/web-viewer/www/pkg
cd examples/web-viewer/www && python3 -m http.server 8080
```

Then open <http://localhost:8080> and paste a Cesium ion token.

A plain static server is enough: no COOP/COEP headers, no `SharedArrayBuffer`,
no nightly toolchain. The worker owns its own wasm instance and everything that
crosses to the page is data.

## What it does the same, and what it does not

Same: traversal, screen-space error, refinement, residency and eviction,
tile lifetime, imagery mosaic selection, f64 geodesy with per-frame rebasing
against the camera.

Different, and deliberately stated rather than hidden:

- **One imagery layer per tile.** The engine sends a mosaic of up to eight
  layers with per-layer uv rectangles; the native backend blends them in one
  pass. Doing that here needs a custom shader — until then the layer covering
  the most of a tile wins, so a multi-part mosaic is drawn coarser than the
  native viewer draws it. Never bare, never wrong, sometimes blurrier.
- **No atmosphere, no wireframe view, no terrain-aware camera collision.**
- **No tile store.** Nothing persists between reloads; the native viewer keeps a
  foyer cache on disk.
