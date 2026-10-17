<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Browser / WebAssembly viewer — status

Status review, 2026-10-17, on `origin/main` at `70e17d9` (PR #62 merged). Read-only:
no code was changed. Builds ran in a throwaway detached worktree with
`CARGO_BUILD_JOBS=4`, logs redirected to files.

**Summary.**
- The interactive browser viewer does not build on main and has not since
  2026-08-18/20. `tuile-web` fails `cargo check --target wasm32` with 3 errors, so
  `examples/web-viewer` cannot run.
- Every other crate that claims wasm passes `cargo check` for wasm32.
- `examples/wasm-globe` builds with `wasm-pack` and loads in Chrome. It draws nothing:
  it is a geometry report.
- The only browser path that is alive and maintained is the **film** path
  (`tuile-film-web`, wgpu on WebGPU plus WebCodecs). It does not traverse and has no
  interaction.
- CI checks only `tuile-core` for wasm.

## 1. Inventory

| Item | Purpose | Last meaningful change (excluding version bumps and renames) |
|---|---|---|
| `crates/tuile-web` | The real `GeometryServer`, SSE traversal and `tuile_planetary::globe` in a Web Worker; `fetch` through `web-sys`; encodes `ServerMessage` into JS objects | 2026-08-20 `5a6fe90` (generation); content 2026-08-16 `e766006` |
| `examples/web-viewer/www` | THREE.js r171 page (unpkg import map), `engine-worker.js` mailbox, a GLSL ground material that blends the imagery mosaic | 2026-08-16 `e766006` |
| `examples/wasm-globe` | Raw wasm, no renderer: drives `traverse` directly, with its own residency and request loop, and prints a `GeometryReport` | 2026-08-16 `e766006` (created 2026-06-15 `20f91c0`) |
| `crates/tuile-film-web` + `examples/film-web/www` | Plays back a baked pack in the browser: wgpu on WebGPU through `tuile-film-gpu`, WebCodecs H.264 with a rav1e fallback, page UI in Rust | 2026-10-13 `8d24189`; features up to 2026-10-10 |
| `crates/tuile-pack-worker` | Cloudflare Worker that serves the bench API and the store (demonstrator `tuile-demo.lapoule.dev`) | 2026-10-10 `4008227` |
| `crates/tuile-pack-api` | The same bench API as a native axum binary; serves `examples/film-web/www` locally | — |
| `crates/tuile-wgpu` | Interactive renderer; its description says "native + wasm" | 2026-10-17 `e4b6e2e` |
| `site/` | Astro static site and rustdoc under `/docs`; it has no viewer | 2026-10-09 `c5bbce2` |

## 2. What builds (wasm32-unknown-unknown, `cargo check`)

| Crate | Result |
|---|---|
| tuile-core | OK |
| **tuile-web** | **FAIL**: 3 × E0027 |
| tuile-wgpu | OK (check only; there is no wasm consumer of it) |
| tuile-planetary, tuile-terrain, tuile-camera, tuile-atmosphere | OK |
| tuile-film-web | OK, with 2 dead-code warnings (`store.rs:112 blocks_read`, `store.rs:300 small`). It would fail under the CI's `RUSTFLAGS=-D warnings` if CI checked it |
| tuile-ui | OK (the tray and menu are optional features) |
| tuile-wasm-globe, tuile-pack-worker, tuile-film | OK |
| tuile-viewer (not claimed) | FAIL: `getrandom 0.3` without `wasm_js`, pulled in by foyer (`tuile-storage-foyer`) and by `metrics-exporter-prometheus` (`tuile-metrics`) |

The `tuile-web` errors confirm the PR #60 claim exactly:

```
error[E0027]: pattern does not mention fields `ancestry`, `generation`  --> crates/tuile-web/src/worker.rs:210  (Select)
error[E0027]: pattern does not mention field `ancestry`                --> crates/tuile-web/src/worker.rs:244  (Fill)
error[E0027]: pattern does not mention field `ancestry`                --> crates/tuile-web/src/worker.rs:269  (Content)
```

- The core added `ancestry` in `9d2f662` (2026-08-18) and `generation` on `Select` in
  `5a6fe90` (2026-08-20). See `crates/tuile-core/src/protocol.rs:93-114`.
- `tuile-web/src/lib.rs:33` is `#![cfg(target_arch = "wasm32")]`, so the native
  workspace build, clippy and test never compile it.
- `wasm-pack build --dev --target web` (wasm-pack is installed):
  - `examples/wasm-globe` builds;
  - `crates/tuile-web` fails with the same 3 errors.
- A minimal local fix (adding `..` to the patterns) was not applied: no code change was
  allowed.

## 3. What runs

Both pages were served with `python3 -m http.server` from their `www/` directories, as
the README says, and opened in Chrome 155 (`navigator.gpu` present).

- **web-viewer** (`videos/web-status-web-viewer-no-pkg.jpg`):
  - The page loads.
  - The worker's `import "../pkg/tuile_web.js"` returns 404: the module cannot be
    built. Nothing can start.
  - A stale `pkg/` dated 2026-08-09 sits in the main checkout. It predates the
    current JS, which expects `layerTable`, so it was not used.
  - **Page bug:** `render()` overwrites `#status` on every frame (`main.js`, end of
    `render`). The token prompt and any `engine error: …` message
    (`worker.onmessage`) vanish within one frame, so errors are invisible.
- **wasm-globe** (`videos/web-status-wasm-globe-loaded.jpg`):
  - The module loads, `init()` succeeds, and it exports `Globe`, `DecodedTile` and
    `ImageryDecoder`.
  - A run needs a Cesium ion token pasted into the page. The local `.env` has one, but
    it is a real third-party credential, and entering it into a page is outside what
    the agent may do. **The runtime globe was not exercised.** Paste it yourself to
    test.
- **film-web**: not run.
  - It needs a release build of `tuile-pack-api` (native, heavy), a fresh
    `wasm-pack` of `tuile-film-web` (wgpu and rav1e), and the production bucket
    credentials from `.env.film`.
  - Not cheap, and it touches production storage. The `pkg` in the main checkout is
    dated 2026-10-07.
- Both screenshots are also copied to `~/Documents/stl-videos/`.

## 4. Parity with the native viewer (`tuile-viewer` + `tuile-wgpu`)

The "Web (interactive)" column describes what the code would do once `tuile-web`
compiles again.

| Feature | Web (interactive) | Film-web | Evidence |
|---|---|---|---|
| Terrain + imagery globe | **Partial**: the real planetary loader; the full mosaic table is blended by the GLSL shader | Done (replays the bake) | `worker.rs:119`, `ground-material.js:49-60`. The README still says "one imagery layer per tile", which is stale |
| Never-black ground, ancestor fallback | **Missing**. `ancestry` is not forwarded. The page draws only `selected ∩ resident` over a near-black clear (`0x05070d`), so a selected tile not yet loaded is a hole | n/a (frame-exact) | `main.js:166-172`, `worker.rs:210` |
| New before old | Partial: `addTile` drops then adds in one synchronous call (no frame gap). **A late `Fill` overwrites real content**: `m.fill` is ignored. **`retire` is not handled**: there is no case in `apply()` | n/a | `main.js:155-185, 188-190` |
| Stitching / skirts | Same geometry as native, from the engine | Same as bake | — |
| Camera controls | Minimal: drag orbit and wheel zoom on a sphere; no `tuile-camera`, no touch, no collision | Pack camera | `main.js:96-133` |
| On-screen UI (`tuile-ui`) | Missing: one status line | Own Rust page UI | — |
| Atmosphere / sky (Hillaire, PR #57) | Missing: flat lighting with a fixed sun | Done (`tuile-film-gpu` uses `hillaire::WGSL`) | `main.js:51`, `film-gpu/src/lib.rs:971` |
| Per-frame light | Missing | Done (film `Look`) | — |
| Overlays API | Missing | Done (`tuile-film/overlay.rs`) | `tuile-wgpu/src/overlay.rs` exists for native |
| Stereo | Missing | Done in the film (`tuile-film/stereo.rs`) | — |
| Persistent cache (IndexedDB/OPFS) | Missing ("IndexedDB is its own piece of work") | Partial: block store over HTTP, in memory | `worker.rs:113-115` |
| Budgets / memory | Partial: `Config::default()` (512 MiB resident) and not `interactive_globe`. The page's `textures` Map is never freed (unbounded) | Fixed 1024-texture cap | `worker.rs:122`, `main.js:39, 211` |
| Lifecycle (hidden tab, context loss) | Missing | — | No `visibilitychange` handler |
| Loading indicator | Partial: priming progress, but overwritten (see §3) | Progress bars | `main.js:174-181, 330-345` |
| Error handling | Partial: errors are sent but become invisible | Done | — |
| Tokens and auth | Paste an ion token; JWT shape check; not stored | Server-side credentials | `main.js:350-365` |
| Attribution display | **Missing** (a licence issue for Bing/ion) | Unverified | Native: `command.rs:183` |
| 3D Tiles content (b3dm/glb) | Missing: globe only | Only what the bake packs | — |
| Film playback | — | Done (WebGPU + WebCodecs/rav1e) | `tuile-film-web/src/lib.rs` |

## 5. Architecture gap

- **Shared with native:**
  - the traversal, `GeometryServer`, `InProcessStream`, planetary loader, terrain
    decode, `imagery_layer_table` and f64 rebasing.
  - This is the right half.
- **Duplicated:**
  1. the consumer bookkeeping: a JS `tiles` Map instead of `tuile-wgpu`'s `pump.rs`
     and `tuile_core::drive::SceneState`;
  2. the GPU residency;
  3. the ground shading: a GLSL copy of the mosaic blend and the lighting;
  4. a camera in JS;
  5. `wasm-globe`, a fourth traversal driver (`examples/wasm-globe/src/lib.rs:283`).
- **Under the manifesto** (one foundation, every render mode; a fork for one mode is a
  design error), the THREE façade is exactly such a fork:
  - every renderer feature (Hillaire, overlays, per-frame light, stereo, ancestor
    climb, swap order) must be written twice, in WGSL and GLSL, and the GLSL copy has
    already fallen behind;
  - wgpu on WebGPU is already proven in this repo by the film path (`tuile-film-web`,
    `wgpu` with `webgpu`);
  - `tuile-wgpu` already passes `cargo check` for wasm32.
- **Recommendation:**
  - The reference web renderer is `tuile-wgpu` on WebGPU in an `OffscreenCanvas`
    worker, with `tuile-camera` for input.
  - The THREE façade becomes an optional integration package for people who already
    have a THREE scene, fed by the same messages. It is not the reference.
  - What blocks the wgpu path:
    - `tuile-viewer` does not build for wasm (foyer, metrics exporter, tokio, winit
      desktop paths);
    - so a `tuile-viewer` "web profile" is needed: the store behind a trait, with an
      IndexedDB/OPFS store on the web; no metrics listener; `spawn_local`.
- **What the budget core spec (PR #64) implies:**
  - `tuile_core::budget` is pure and builds for wasm32, so the browser gets it for
    free.
  - `Credit { bytes }` on `InProcessStream` is how a worker should push meshes to the
    page or to the renderer without unbounded `postMessage` queues.
  - `Tier::Stored` needs a browser store that answers `holds(key)` from an index, so an
    IndexedDB/OPFS store must implement it. The HTTP disk cache is not queryable from
    JS without the Cache API, which is an option.
  - The headless control API (§4c) is native-socket only. A web equivalent
    (`postMessage`/JS API, `frame()` readback) is what a headless-browser test would
    drive.
  - The spec lists `tuile-web/src/worker.rs:129` as a downstream change. It cannot land
    while the crate does not compile and nothing checks it.

## 6. CI

- **Today:** `wasm-guard` runs only `cargo check --target wasm32-unknown-unknown -p
  tuile-core` (`.github/workflows/ci.yml:45-53`).
- **Missing:**
  - wasm checks of `tuile-web`, `tuile-film-web`, `tuile-wgpu`, `tuile-planetary`,
    `tuile-terrain`, `tuile-camera`, `tuile-atmosphere`, `tuile-ui`,
    `tuile-wasm-globe` and `tuile-pack-worker`, under `-D warnings`;
  - a `wasm-pack build` of the examples;
  - any browser test. There is no headless Chrome with WebGPU (Playwright with
    `--enable-unsafe-webgpu`, or SwiftShader), no black-pixel scan and no recorded
    network.

## 7. Recommendation (ordered)

1. **S** — Repair `tuile-web`:
   - forward `ancestry` and `generation`;
   - handle `retire`;
   - refuse a `Fill` over real content;
   - stop the status line from overwriting errors.
2. **S** — CI: put every wasm crate in `wasm-guard` with `-D warnings`; fix the 2
   film-web warnings.
3. **S** — Web viewer README: the stale "one layer" note, the build path, and how to
   get a token.
4. **M** — Ancestor climb on the page, which needs `ancestry` from step 1. This is the
   minimum that satisfies "never black" in THREE.
5. **M** — Attribution display (a licence requirement before any public page) and a
   visibility/lifecycle handler.
6. **M** — Headless-browser smoke test (Playwright with WebGPU):
   - load a recorded tileset;
   - scan for clear-colour pixels;
   - fail on console errors.
7. **L** — Decision point (see below). If wgpu: a `tuile-viewer` web profile, with
   `tuile-wgpu`, `tuile-camera`, `tuile-ui` and the atmosphere in a worker
   `OffscreenCanvas`, sharing `pump.rs` and the shaders.
8. **L** — A browser store (IndexedDB or OPFS) behind the store trait, answering
   `holds()` for `Tier::Stored`.
9. **M** — Retire `examples/wasm-globe` (the fourth driver) or rebuild it on
   `GeometryServer`.

- **Dependencies:**
  - Items 1 to 6 are independent of the budget core and the rebalancing, and should go
    first.
  - Items 7 and 8 should follow PR #64 (`Credit`, tiers, `Profile`) and the PR #60
    rebalancing (one consumer bookkeeping, `SceneState` or `pump`), or they will be
    written against APIs about to change.
- **Owner decisions:**
  - (a) THREE façade versus wgpu/WebGPU as the reference web renderer. The
    recommendation is wgpu, with THREE as an optional adapter.
  - (b) Browser targets: WebGPU only (Chrome/Edge, Safari 26+, Firefox 141+ on
    Windows) or a WebGL2 fallback, which wgpu's GL backend allows at the cost of
    compute-free paths.
  - (c) Hosting: a static page on the site or demo Worker, how tokens are handled
    (a user token versus a proxy), and attribution.

## Not verified

- No interactive globe has run in a browser on current main: `tuile-web` does not
  compile.
- `wasm-globe` and film-web were not exercised at runtime: they need a token or
  production credentials.
- `tuile-wgpu` on wasm was only `cargo check`ed: no wasm consumer exists.
- The public demo was not touched.
- The parity column for web interactive is read from the code, not observed.
