<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Rebalancing the render modes, and the road to 1.x — an impact analysis

This is an analysis and a set of proposals. It changes no code. It measures `origin/main` at
`07be3d0` (v0.7.0) against the "one foundation for every render mode" manifesto
(`docs/00-vision.md` on branch `docs/manifesto-universal`), against `CLAUDE.md`, and against
`docs/01-architecture.md`.

The owner's position: film is one render mode among the others. The goal is a solid foundation
that serves everywhere and compiles everywhere: wasm, headset, mobile, film and interactive.

**Method.**
- Sources: `cargo metadata`, grep and line counts. Every claim cites `crate/file:line` on `07be3d0`.
- Builds: no full workspace build was run. Other builds were running on the machine throughout,
  so no extra `cargo check --target wasm32` was started either; wasm verdicts are reasoned from
  the dependency graph and the source, and are marked so.
- In-flight branches: `../tuile-sky` and `../tuile-ios` were read and not modified. Both are
  uncommitted work on `3703e07`, two commits behind main.

**Contents.**
- [Executive summary](#executive-summary)
- [1. Workspace map](#1-workspace-map)
- [2. Duplication and divergence between modes](#2-duplication-and-divergence-between-modes)
- [3. Portability per target](#3-portability-per-target)
- [4. Dead or orphaned code](#4-dead-or-orphaned-code)
- [5. Rebalancing plan](#5-rebalancing-plan)
- [6. Risks and open questions](#6-risks-and-open-questions)
- [7. Road to 1.x](#7-road-to-1x)
- [8. Industrialising the render targets: protocols, bundles, layout](#8-industrialising-the-render-targets-protocols-bundles-layout)

---

## Executive summary

**Effort has gone to the film path.**
- Since 2026-09-01, about **73k lines** were added to the film family, against **~9k** for the
  interactive renderers. Of those 9k, `tuile-viewer` is 8.3k, mostly its extraction from the
  example, and `tuile-wgpu` is 720 (section 1.3).
- The film family alone is now about half the Rust in the workspace.

**What is genuinely shared.**
- The SSE traversal, `ResidentCache`, tile decoding, seam stitching and the imagery-layer blend
  are one code path for every mode, film included (the bake drives the core `GeometryServer`).
- That is the strong part of the foundation.

**What is not shared.**
- Everything after the traversal is written once per mode:
  - GPU tile residency and imagery texture caches;
  - rebasing and the projection;
  - lighting and atmosphere;
  - tone mapping;
  - overlays;
  - stereo;
  - radiometric calibration.
- The interactive viewer has none of the following, and the browser viewer has even less:
  - the physical sky;
  - the per-frame light (`Look`/`FrameLight`);
  - exposure and tone mapping;
  - shadows;
  - host overlays;
  - stereo;
  - radiometric calibration.
- The browser viewer draws through a hand-written GLSL copy of the interactive shader, with a
  fixed sun and no air.
- There are three atmosphere models in WGSL, and none is used by both interactive and film.

**The browser path is broken on main.**
- `tuile-web` matches `ServerMessage::Select { tiles, stats }` (`crates/tuile-web/src/worker.rs:209`).
  The core message now carries `ancestry` and `generation` (`crates/tuile-core/src/protocol.rs:93-114`).
- The crate is `#![cfg(target_arch = "wasm32")]` (`lib.rs:33`), and CI only checks `tuile-core`
  for wasm, so nothing notices.
- Even once it compiles, the page never receives the ancestry closure that prevents black ground.

**Mobile is not on main.**
- The viewer hard-codes desktop assumptions:
  - budgets of 4 GiB / 16 000 tiles;
  - `$HOME/.cache`;
  - a metrics HTTP listener;
  - a tray icon;
  - no `suspended` handling;
  - `block_on` on the main thread.
- The uncommitted `tuile-ios` worktree fixes most of these, inside `tuile-viewer`.

**1.0 is not near.**
- There is no `#[non_exhaustive]` anywhere in the workspace.
- Pre-1.0 dependency types leak everywhere: glam, wgpu, flatbuffers, pmtiles, object_store, mcap.
- Every crate is publishable by default, including the farm and the bake tools.
- CI has no semver, public-API, MSRV, wasm-matrix or iOS job.

## 1. Workspace map

### 1.1 Crates

Line counts are lines of `.rs` (tests included) plus WGSL, measured with `wc -l`. Mode columns:
**B** browser, **D** desktop interactive, **M** mobile, **H** headset/stereo, **F** film, **S** server.

| Crate | Role | Rust / WGSL | Depends on (tuile) | Modes |
|---|---|---|---|---|
| tuile-b3dm | b3dm codec | 188 | — | all |
| tuile-core | 3D Tiles + globe engine, traversal, `GeometryServer`, cache, raster | 17 069 / 51 | b3dm | all |
| tuile-terrain | quantized-mesh, tiling | 4 447 | core | all |
| tuile-planetary | terrain × imagery assembly | 3 043 | core, terrain | B D M F(bake) |
| tuile-atmosphere | sun, aerial perspective, Hillaire LUTs, illumination | 2 626 / 433 | core | D (aerial only), F (full) |
| tuile-camera | globe camera, controller, path following | 2 642 | core | D only |
| tuile-tape | MCAP camera record/replay | 2 874 | core | D, F (bake), usd |
| tuile-bing, tuile-cesium-ion | source connectors | 510, 1 321 | core (+terrain) | B D F |
| tuile-native-fetchers | reqwest + foyer transport | 844 | core, ion | D F |
| tuile-storage-foyer | `ContentStore` on foyer | 507 | core | D F S |
| tuile-metrics | Prometheus endpoint | 231 | core | D |
| tuile-wgpu | interactive wgpu renderer | 7 780 / 333 | core, atmosphere | D (M in flight) |
| tuile-ui | on-screen controls, statusbar | 909 | camera, core | D |
| tuile-viewer | windowed viewer as a library | 7 707 | atmosphere, camera, core, metrics, planetary, foyer, tape, terrain, ui, wgpu | D (M in flight) |
| tuile-viewer-bundle | app icon generator (bin) | 851 | — | D |
| tuile-web | wasm worker transport + engine worker | 606 | core, ion, bing, planetary | B only |
| tuile-film | pack reader, `Look`, overlays, stereo, film camera | 4 204 | atmosphere, core, pack, terrain | F, H |
| tuile-film-gpu | film wgpu renderer (visibility buffer + compute) | 6 953 / 1 098 | atmosphere, core, film | F (native + web) |
| tuile-film-native | native film renderer, encoders, serve | 10 555 | bake, core, farm, film, film-gpu, mp4, pack, radiometry, repository, foyer | F, H |
| tuile-film-web | film in a browser (WebGPU + WebCodecs) | 4 370 | core, film, film-gpu, mp4, radiometry, repository | F (B) |
| tuile-mp4 | mp4 / MV-HEVC muxing | 1 770 | — | F, H |
| tuile-bake | camera path → pack | 10 145 | bing, ion, core, film, native-fetchers, pack, planetary, tape, terrain, tile-server | F (+hydra) |
| tuile-pack | baked-scene container (flatbuffers) | 4 956 | core | F |
| tuile-farm | farm object storage, jobs, concat | 9 172 | mp4, pack | F |
| tuile-radiometry | imagery colour harmonisation | 9 589 | — | F only |
| tuile-repository | where films live; bench routes | 8 803 | core, farm, pack, radiometry, tile-server | F, S |
| tuile-tile-server | source tile store (PMTiles) | 14 460 | core | S, F (bake) |
| tuile-pack-api, tuile-pack-worker | bench API, native and edge | 1 063, 646 | repository… | S (film bench) |
| tuile-usd | `.usda` manifest stage from a tape | 679 | core, tape | USD/Blender farm path |
| tuile-hydra | C ABI for the Hydra plugin | 893 | bake, core, pack | USD hosts |

Examples: `wgpu-viewer` (D), `globe-bulk` and `orbit-probe` (headless D probes), `warm-cache`,
`wasm-globe` (B, no rendering), plus `web-viewer` and `film-web` (www only).

### 1.2 Dependency graph

The graph below shows normal dependencies only. Mode-exclusive crates are marked: [F] film only,
[D] desktop only, [B] browser only.

```
                          tuile-b3dm
                              │
                          tuile-core ───────────────────────────────────────────────┐
     ┌──────────┬────────────┼──────────────┬──────────┬───────────┬─────────┐      │
  terrain   atmosphere    camera[D]      tape       bing/ion    pack[F]   tile-server
     │          │            │             │           │           │           │
 planetary      │          ui[D]           │      native-fetchers  │           │
     │          ├──────► tuile-wgpu[D] ◄───┼───────────┤           │           │
     │          │            │             │           │           │           │
     └──────────┴────► tuile-viewer[D] ◄───┘   foyer, metrics[D]   │           │
                │                                                  │           │
                └──► tuile-film[F] ◄───────────────────────────────┘           │
                          │                                                    │
                     tuile-film-gpu[F]       radiometry[F]                     │
                          │                       │                            │
     tuile-bake[F] ◄── (bing, ion, fetchers, planetary, tape, film, pack) ─────┤
          │                                       │                            │
          ├──► tuile-hydra                 tuile-repository[F/S] ◄── farm[F] ◄─ mp4[F]
          │                                       │
          └──► tuile-film-native[F] ◄─────────────┘──► pack-api / pack-worker [S]
                                     tuile-film-web[F] ◄── film-gpu, repository, radiometry, mp4
                                     tuile-web[B] ◄── core, ion, bing, planetary
```

What the graph shows:
- **Film and the interactive renderer share no rendering crate.**
  - `tuile-film-gpu` does not depend on `tuile-wgpu`.
  - The only rendering-adjacent crate both use is `tuile-atmosphere`, and they use different
    modules of it.
- **`tuile-camera` is desktop-only.** No film crate uses it, and `tuile-film` has its own
  `FrameCamera`.
- **`tuile-repository` pulls film concerns into the server side.** It depends on `tuile-farm`
  natively (Cargo.toml target section).
- **`tuile-hydra` depends on `tuile-bake`.** That is the whole film-bake stack, with
  object_store and the tile-server, in a USD façade.

### 1.3 Where the effort went

These are lines added per crate since 2026-09-01, from `git log --numstat` with author dates.

| Family | Lines added | Crates |
|---|---|---|
| Film | ~73 000 | film-native 12.5k, farm 11.3k, radiometry 10.6k, repository 9.5k, bake 9.5k, film-gpu 8.5k, pack 5.5k, film-web 5.1k, film 4.5k, mp4 1.9k, pack-api 2.0k, pack-worker 0.9k |
| Server | 17 800 | tile-server |
| Shared engine | ~13 000 | core 5.7k, atmosphere 1.9k, tape 1.5k, planetary 1.4k, camera 1.1k, terrain 1.0k |
| Interactive | ~9 000 | viewer 8.3k (mostly extracted from the example), wgpu 0.7k, web 0.05k |

## 2. Duplication and divergence between modes

### 2.1 Traversal and selection — shared, by design for film

- **One SSE traversal.**
  - `tuile_core::traversal::traverse` (`crates/tuile-core/src/traversal.rs:529`) is driven by
    `GeometryServer::retraverse` (`runtime.rs:513`).
  - Interactive: `crates/tuile-viewer/src/lib.rs:263` with `Config::interactive_globe`
    (`traversal.rs:388`).
  - Browser: `crates/tuile-web/src/worker.rs:129`.
  - Film: the bake (`crates/tuile-bake/src/session.rs:1051`) with `exact_traversal`
    (`session.rs:1529`).
- **The film renderers never traverse.** They replay the bake's selection: `tuile_film::Cursor`
  and `FrameDiff` (`crates/tuile-film/src/cursor.rs:41-60`). This is the frame-exact contract,
  and it is the right design.
- **Divergence: consumer-side selection bookkeeping exists three times.**
  - `tuile_core::drive::SceneState` (`drive.rs:29`) claims every façade uses it (`drive.rs:6-9`).
    Only the bake does.
  - `tuile_wgpu::ContentPump` keeps its own selection, generation and ancestry
    (`crates/tuile-wgpu/src/pump.rs:31-56`).
  - The web page keeps a JS `tiles` Map (`examples/web-viewer/www/main.js:163-262`).
- **Copy: `examples/wasm-globe`** calls `traverse` directly (`examples/wasm-globe/src/lib.rs:283`),
  with its own residency and request loop. It is a fourth driver.
- **Broken: `tuile-web` encodes a stale protocol** (`worker.rs:209, 244, 269` against
  `protocol.rs:93-114, 131, 150`). It does not forward `ancestry`, which is the field that
  prevents black ground.

### 2.2 Residency and caches — server shared, consumer copied

- **Shared.** `ResidentCache` (`crates/tuile-core/src/cache.rs:76`) is bounded by bytes and by
  count with high and low water marks.
  - The interactive viewer sets 4 GiB / 16 000 (`traversal.rs:394-395`). On wasm32, 4 GiB
    saturates the address space.
  - The bake sets both limits to `usize::MAX` (`session.rs:1532-1533`).
- **GPU residency, three versions.**
  - Interactive: `ContentPump` / `prepare` (tuile-wgpu).
  - Film: a 16 384-slot table (`crates/tuile-film-gpu/src/lib.rs:72, 357, 1238`).
  - Browser: a THREE.js Map.
- **Imagery textures, four versions.**
  - Core `ImageryPool` with `Weak` refcounts (`crates/tuile-core/src/raster.rs:1067`), used by
    tuile-wgpu.
  - Film native `TEXTURES_HELD = 2048`, cleared all at once when full
    (`crates/tuile-film-native/src/render.rs:199, 451, 666`).
  - The same logic copied into film-web with `1024` (`crates/tuile-film-web/src/worker.rs:273, 288, 365`).
  - An unbounded JS `textures` Map on the web page (`examples/web-viewer/www/main.js:39, 247`).
- **Block caches, two versions.**
  - `Held` in film-native, bounded at 1 GiB (`crates/tuile-film-native/src/source.rs:119-151`).
  - `RemoteBlocks` in repository, 8 blocks (`crates/tuile-repository/src/remote.rs:309-430`).

### 2.3 Camera, rebasing, projection — two runtime cameras (three with the browser)

**Interactive.**
- `tuile_camera::GlobeCamera` (`crates/tuile-camera/src/lib.rs:79`) builds a standard-Z
  `perspective_rh` with a finite far plane (`lib.rs:352-369`).
- The core rebasing utility `tuile_core::geo::rebased_model` (`geo.rs:19`) is called **only** by
  tuile-wgpu (`prepare.rs:264, 282`).

**Film.**
- `tuile_film::FrameCamera` (`crates/tuile-film/src/camera.rs:22`) is eye-relative, with an
  infinite reverse-Z projection (`camera.rs:50`) and its own near rule (`camera.rs:45-46`).
- Rebasing is done again inline in `crates/tuile-film-gpu/src/lib.rs:2078, 2154, 2254`.
- The resolve shader rebuilds rays from basis vectors rather than from the matrix
  (`crates/tuile-film-gpu/src/shaders/common.wgsl:8-12`).

**Browser.**
- A THREE `PerspectiveCamera`, rebased in JS (`examples/web-viewer/www/main.js:300-312`).

**Paths.**
- `tuile-camera/src/path.rs` (smoothing, leash, pursuit) is not used by film.
- The film's camera comes from `tuile-tape` through the bake (`tuile_pack::BakedView`,
  `crates/tuile-pack/src/lib.rs:197`).
- `tuile-tape` is the only camera piece both modes share.

**Verdict.** Two projections disagree on near/far and depth convention. Swapping one mode's
camera into the other changes depth precision and clipping. Unify on reverse-Z infinite, which
is the better choice for a globe.

### 2.4 Shading and shaders — two renderers, one shared snippet

**Shared WGSL.**
- `tuile-core/src/raster.wgsl` (51 lines, `blend_layer`): used by tuile-wgpu only.
- `tuile-atmosphere/src/aerial.wgsl` (153 lines): used by tuile-wgpu only.
- `tuile-atmosphere/src/hillaire.wgsl` (280 lines): used by tuile-film-gpu only
  (`lib.rs:971, 993`; `atmosphere.rs:268`).

**tuile-wgpu.**
- `shader.wgsl` (261 lines) is assembled from core raster + aerial + its own code
  (`renderer.rs:574-590`).
- `overlay.wgsl` (38 lines) is 2D UI; `lines.wgsl` (34 lines) draws debug bounds.

**tuile-film-gpu.** 15 shaders, 1 098 lines:
- `common`, `raster` (visibility buffer), `bin`, `resolve` (compute shading), `present` (sky,
  exposure, grade, OETF, overlay compositing);
- `overlay` (world-space, depth-tested, shadowed);
- `air`, `sunlit`, `shadow`, `sky_view`;
- `compose` (drape and grade);
- `mips`, `stitch`, `i420`.

**Browser.** `examples/web-viewer/www/ground-material.js` (177 lines of GLSL ES 1.00) is a
hand transposition of `shader.wgsl`.

| Concern | Interactive (tuile-wgpu) | Browser (web-viewer) | Film (tuile-film-gpu) | Distance |
|---|---|---|---|---|
| Imagery blend | core `blend_layer` per fragment (`shader.wgsl:173-182`) | GLSL copy | baked by `compose.wgsl` (GPU twin of `raster::bake_layers`, `raster.rs:1239`) | same maths, three places |
| Lighting | `albedo*(ambient+(1-ambient)*lambert)`, ambient 0.5 (`crates/tuile-viewer/src/app/mod.rs:42`) | same, ambient 0.35, fixed sun (`main.js:53`) | RGB dome + RGB sun + L1 sky SH + shadows (`resolve.wgsl:130-183`) | diverged |
| Aerial perspective | closed form (`aerial.wgsl`) | none | analytic haze (`air.wgsl`) or Hillaire march | three models, none common to interactive and film |
| Sky | black clear (`frame.rs:276-281`), `SkyShell` unused (`tuile-atmosphere/src/sky.rs:77`) | flat colour | dome, gradient, or physical sky + sun disc (`present.wgsl:30-86`) | film-only |
| Exposure / tone map | none; sRGB surface | none; output encode probably missing (inferred) | exposure, white balance, contrast, saturation, tanh knee, OETF (`present.wgsl:97-127`) | film-only |
| Bare ground colour | ocean blue (`backdrop.rs:24`) | green | look-driven | diverged |

### 2.5 Atmosphere, light, Look, overlays, stereo — film-only

- **Atmosphere.**
  - The interactive viewer uses only `Sun` and `AerialPerspective`
    (`crates/tuile-viewer/src/app/frame.rs:159-189`).
  - `LutAtmosphere`, `SkyLight` and `ReferenceIllumination` are consumed only by film
    (`crates/tuile-film/src/look.rs:5`, `crates/tuile-film-gpu/src/atmosphere.rs:15-16`).
- **Look and per-frame light.**
  - `Look` (`crates/tuile-film/src/look.rs:130`) and `FrameLight` (`look.rs:303`) live in
    `tuile-film`, are applied by `Look::lit_by`, and are fed per frame by hosts through
    `Overlays::light` (`crates/tuile-film/src/overlay.rs:233`).
  - No interactive crate depends on `tuile-film`.
  - A second, unrelated `Look` exists in `tuile-radiometry/src/matrix.rs:128`.
- **Overlays.**
  - The host API is `trait Overlays`, `OverlayMesh` and `OverlayCamera`
    (`crates/tuile-film/src/overlay.rs:21-233`).
  - It is bound by `OverlayFactory` in film-native (`crates/tuile-film-native/src/serve.rs:100-108`).
  - It is not consumed by film-web, the interactive viewer or the browser.
  - Ribbon and marker helpers exist only in a CLI (`crates/tuile-film-native/src/bin/tuile-film-render.rs:245, 284`).
- **Stereo.**
  - `Stereo` and `EyePair` (`crates/tuile-film/src/stereo.rs:85, 145`), `StereoSpec`
    (`crates/tuile-film-native/src/engine.rs:75`), MV-HEVC (`crates/tuile-film-native/src/spatial.rs`,
    `crates/tuile-mp4/src/spatial.rs:123`).
  - No stereo or XR code exists in tuile-wgpu, tuile-viewer, tuile-web or tuile-camera.

### 2.6 Draping and radiometric calibration

- **Layer selection is shared.** tuile-planetary selects layers and the core packs the layer
  table, in every mode.
- **Film drapes from the bake.** It uses CPU `bake_layers` (`crates/tuile-film/src/from_store.rs:196`)
  or GPU `compose` with `LayerGrade` (`crates/tuile-film-gpu/src/lib.rs:455`) and the grades in
  `compose.wgsl:82-144`.
- **Calibration is film-only.** `tuile-radiometry` is consumed only by film-native, film-web,
  repository and pack-api. Interactive mode shows imagery uncalibrated, so the same place looks
  different in the app and in the film.

### 2.7 Summary

| Feature | tuile-wgpu / tuile-viewer | tuile-web / browser | Film | Verdict |
|---|---|---|---|---|
| SSE traversal, `ResidentCache` | core | core (broken encoder) | core via bake, replayed | **shared** |
| Consumer selection state | own (`pump.rs`) | JS | `SceneState` / `Cursor` | copies |
| GPU residency, texture caches | own | JS, unbounded | own ×2 (native, web) | copies |
| Rebasing / projection | core + tuile-camera | JS | tuile-film + inline | copies, different conventions |
| Seam stitching | core | missing | core | shared, absent on web |
| Imagery blend | core WGSL | GLSL copy | baked compose | three copies of one maths |
| Physical sky, LUTs | missing | missing | yes | film-only |
| Per-frame light, `Look`, exposure | missing | missing | yes | film-only |
| Shadows | missing | missing | yes | film-only |
| Host overlays (3D) | missing (2D UI only) | missing | native film only | film-only |
| Stereo, spatial video | missing | missing | yes | film-only |
| Radiometric calibration | missing | missing | yes | film-only |

## 3. Portability per target

### 3.1 wasm32-unknown-unknown

CI checks only `-p tuile-core` (`.github/workflows/ci.yml:53`). The verdicts below were reasoned
from dependencies and source, not compiled, because other builds held the machine.

| Crate | Verdict | Why |
|---|---|---|
| tuile-core, b3dm, terrain, planetary, bing, atmosphere, camera, pack | likely OK | pure Rust; `?Send` gates on the async traits |
| tuile-cesium-ion | likely OK | reqwest gated `cfg(not(wasm32))` |
| tuile-film, tuile-film-gpu | OK in practice | compiled as dependencies of tuile-film-web |
| tuile-radiometry | OK in practice | nalgebra / petgraph; consumed by film-web |
| tuile-repository | OK in practice | non-wasm deps gated; consumed by film-web |
| **tuile-web** | **fails (by reading)** | stale `ServerMessage` patterns (§2.1) |
| tuile-wgpu | unknown, probably OK | no wasm cfg anywhere; `GpuContext` assumes a native adapter flow; never built for web |
| **tuile-tape** | **likely fails** | `mcap 0.25` pulls `zstd` and `lz4` (C, via `-sys`), and `std::fs::File` in the lib (`crates/tuile-tape/src/lib.rs:57`) |
| tuile-usd | likely fails | through tape |
| tuile-viewer, native-fetchers, foyer, metrics, ui (statusbar), bake, farm, film-native, tile-server | no | tokio, foyer, winit desktop, axum, object_store |

Consequences:
- Camera record/replay (`tuile-tape`, the determinism principle) is unavailable in the browser.
- The interactive wgpu renderer has never run on WebGPU, even though the film renderer does
  through film-web.

### 3.2 iOS and mobile (main; the `tuile-ios` branch addresses most of this)

- **Tokio.** A default `new_multi_thread` runtime with one worker per core
  (`crates/tuile-viewer/src/lib.rs:205`). Two `rt.block_on` calls run on the main thread before
  the event loop (`lib.rs:215, 231`).
- **Threads.**
  - A geometry-server thread (`lib.rs:264`).
  - A decode pool of cores−1 (`crates/tuile-core/src/offload.rs:105-108`).
  - The tokio workers.
  - foyer's own I/O threads.
  - The total is unbounded by device class.
- **Filesystem.** `default_cache_dir` (`crates/tuile-storage-foyer/src/lib.rs:265-279`,
  `crates/tuile-native-fetchers/src/lib.rs:237-251`) and `crates/tuile-viewer/src/host.rs:88-135`
  resolve to `$HOME/.cache` / `~/.config` on iOS, which is outside the sandbox. The store then
  silently falls back to none (`globe.rs:146-153`).
- **Budgets are hard-coded.**
  - 4 GiB / 16 000 resident (`traversal.rs:388-397`).
  - foyer 256 MiB memory / 4 GiB disk (`crates/tuile-storage-foyer/src/lib.rs:64-70`).
  - 64 connections per host (`native-fetchers/src/lib.rs:276`).
  - `HighPerformance` with `required_limits: adapter.limits()` (`crates/tuile-viewer/src/app/setup.rs:62, 105`).
  - Only the imagery slot count is device-sized (`setup.rs:123`).
- **Lifecycle.**
  - No `suspended`; `ControlFlow::Poll` redraws forever (`lib.rs:320`).
  - The shutdown after `run_app` never runs on iOS (`lib.rs:323-347`).
- **Desktop-only pieces wired in unconditionally.**
  - The metrics listener on 127.0.0.1:9464 (`lib.rs:314`).
  - `ctrlc`, `rlimit` and Apple Events.
  - `tuile-ui/statusbar`, whose tray-icon and muda do not build for iOS, enabled
    unconditionally (`tuile-viewer/Cargo.toml:37`).
- **Embedding.** `tuile_viewer::main(host) -> !` (`lib.rs:98`) owns the process. A mobile host
  cannot hand it a view or a layer, nor receive lifecycle or budget hooks.

### 3.3 Unbounded caches, pools and queues

| Item | Where | Modes hit |
|---|---|---|
| `InProcessStream` channels, `mpsc::unbounded()` both ways; `Content` carries decoded meshes | `crates/tuile-core/src/protocol.rs:273-296` | **all** |
| offload job queue, std `mpsc::channel()` | `crates/tuile-core/src/offload.rs:113` | all native |
| `ContentPump.ancestry`, never pruned (by design) | `crates/tuile-wgpu/src/pump.rs:49-56` | D, M |
| `ContentPump.pending`, input unbounded | `pump.rs:22-71` | D, M |
| `sent_imagery` HashSet | `crates/tuile-web/src/worker.rs:64, 340` | B |
| JS `textures` Map, no delete | `examples/web-viewer/www/main.js:39, 247` | B |
| wasm-globe `ResidencyView` and `got`, grow only | `examples/wasm-globe/src/lib.rs:82, 185` | B (example) |
| bake residency and `LayerBudget::set_unbounded` | `crates/tuile-bake/src/session.rs:585, 1532`; `globe.rs:402` | F (bounded by `TUILE_BAKE_KEEP_GIB` only) |
| std unbounded channels for steer and globe switch | `crates/tuile-viewer/src/desktop.rs:288`, `globe.rs:233` | D (low volume) |
| film texture caches, clear-all instead of LRU | film-native `render.rs:199`, film-web `worker.rs:273` | F (bounded, but every clear drops everything at once) |

### 3.4 Blocking calls on render or main threads

- **Interactive.**
  - Startup `pollster::block_on` (`crates/tuile-viewer/src/app/setup.rs:61, 103`) and
    `rt.block_on(connect/assemble)` (`lib.rs:215, 231`).
  - Exit `block_on(store.close())` (`settings.rs:125`).
  - Opt-in readback `device.poll(wait_indefinitely)` (`crates/tuile-wgpu/src/readback.rs:97`).
  - A `std::fs` write on close (`crates/tuile-viewer/src/app/mod.rs:302-309`).
  - A per-tile `Mutex` on imagery (`pump.rs:272`, `context.rs:149`).
- **No blocking `recv()`** on any interactive or web render path.
- **Film.** Blocking is by design and acceptable offline (`render.rs:322, 970`; `nvenc.rs:197`).

### 3.5 Headset

- Stereo exists only as an offline film product: `EyePair` frames encoded as MV-HEVC.
- The interactive viewer has no second eye, no per-eye projection and no XR surface.
- `tuile-camera` has no notion of eye offset.
- Live headset rendering needs, at minimum:
  - the shared camera of move 5.3;
  - `Stereo` moved out of `tuile-film`;
  - a multi-view render in tuile-wgpu.

## 4. Dead or orphaned code

| Item | Remaining consumers | To park it |
|---|---|---|
| **tuile-usd** (679 lines) | only its binary `tuile-tape-to-usd-stage`, run by tuile-farm (`crates/tuile-farm/src/job/inputs.rs:116`, `config.rs:88`), `integrations/blender/Dockerfile.globe:66-67`, `render_job.sh:635` | drop the farm's Blender path and Dockerfile.globe. Docs 13 and the site describe a USDZ exporter that does not exist. |
| **tuile-hydra** (893 lines) | staticlib for `integrations/hydra/CMakeLists.txt:25-54` and Dockerfile.globe. A façade over `tuile_bake::Session` (`lib.rs:33-36`, `ffi.rs:22-340`). `tuile-pack` is test-only (`ffi.rs:568`). | remove from members (`Cargo.toml:22`); drop the hydra stage and the site/README mentions. Roadmap M4 describes a `WsStream` client, which it is not. |
| **integrations/blender, hydra** | the farm's Blender render path (`crates/tuile-farm/src/job/render.rs:124-155`) | parked together with usd and hydra. Blender has no USD plugin surface, so this path cannot be extended anyway. |
| **integrations/bake, film** | farm images (`integrations/bake/Dockerfile:34`, `integrations/film/Dockerfile:71-102`) | live; keep |
| **tuile-bake** | film-native (`TapeBake`, `Tiles`: `engine.rs:10-11`), hydra | not orphaned. Its pack *reader* (`tiles::Tiles`) belongs with the film reader, not the baker. |
| tuile-core `http` feature, `HttpFetcher` | none (`crates/tuile-core/src/fetch.rs:120-127`) | delete, or move to native-fetchers |
| tuile-cesium-ion `reqwest` feature, `ReqwestHttp` | none (`lib.rs:365-372`) | delete |
| tuile-film-native `videotoolbox` feature | nothing enables it | keep, but build it in a macOS CI job |
| `drive::SceneState` as "every façade's state" | only the bake | adopt it in tuile-wgpu (move 5.2) or narrow its doc |
| `SkyShell` (`crates/tuile-atmosphere/src/sky.rs:77`) | none | delete, or use it as the interactive sky in move 5.1 |
| `pixel_list_bytes` (`crates/tuile-film-gpu/src/lib.rs:2644`), `blocks_read` (`crates/tuile-film-web/src/store.rs:112`) | none | delete |
| `WsStream`, `HttpPullStream`, `tuile-server` (docs 01, 03, 12) | do not exist; only `InProcessStream` (`protocol.rs:277`) | rewrite the docs, or schedule the bindings. They are what make the "server" mode real. |

## 5. Rebalancing plan

The moves are ordered. Each one is a separate PR from its own worktree, and each must keep the
film's frame-exact tests and the never-black rule green. "Host" below means the downstream host,
in generic terms.

### Quick wins

**5.0a — Fix and guard the browser path (S).**
- Repair `tuile-web/src/worker.rs:209-269` against the current protocol and forward `ancestry`
  and `generation`.
- Extend the CI wasm job to `tuile-web`, `tuile-film-web`, terrain, planetary, atmosphere,
  camera, bing, cesium-ion and pack.
- Impact: tuile-web only. No API break, no film risk.
- Unlocks: the browser can be trusted again, and it stops drawing black ground.
- Dependencies: none.

**5.0b — Bound the stream and the browser maps (S).**
- Replace the `unbounded()` pair in `protocol.rs:273-296` with a bounded channel plus explicit
  back-pressure. A byte budget is better, with `Content` waiting on capacity.
- Give the JS `textures` Map and `sent_imagery` eviction tied to `Evict`.
- Bound the offload queue.
- Impact: tuile-core runtime; the bake's throughput must be re-measured.
- Film risk: low, because the bake waits for convergence anyway. Rerun the bake exactness test
  (`crates/tuile-bake/src/session.rs:2125`).
- Unlocks: mobile and the browser.

**5.0c — Park the USD/Blender path (S).**
- Take `tuile-hydra` and `tuile-usd` out of workspace members and move them under
  `integrations/` with their own `Cargo.toml` (or `publish = false` + an `experimental/`
  directory).
- Make `tuile-pack` a dev-dependency of hydra.
- Drop the farm's Blender branch, behind a flag first, per the "neutralise, never delete" rule.
- Fix docs 03, 12 and 13.
- Impact: the farm and Dockerfile.globe. The host is unaffected unless it renders through
  Blender.

**5.0d — Delete dead features (S).** Remove `tuile-core/http`, `cesium-ion/reqwest`,
`SkyShell` and the unused pub fns. Small API break; nobody consumes them.

### Structural moves

**5.1 — Extract a shared look crate: `tuile-look` (L).**
- **Contents.**
  - From `tuile-film/src/look.rs`: `Look`, `FrameLight`, `Imagery` decoding, exposure and grade
    parameters.
  - The WGSL for exposure, white balance, the highlight knee and the OETF, extracted from
    `present.wgsl:97-127`.
  - The lighting function: dome + sun + L1 SH, from `resolve.wgsl:130-183`.
  - The single aerial-perspective entry point, which picks the closed form or Hillaire.
  - It depends on `tuile-atmosphere`, and on wgpu only for optional LUT builders, or none at
    all if it stays WGSL strings plus uniforms.
- **Consumers.**
  - tuile-film-gpu includes the snippets instead of its own.
  - tuile-wgpu replaces `ambient + lambert` (`shader.wgsl:221-224`) and the black clear with
    the same functions.
  - It renders to an `Rgba16Float` target and adds a present pass. `docs/19-sky-atmosphere.md`
    §4.4 already plans this.
- **Breaks.** `tuile_film::Look` / `FrameLight` move, and are re-exported from `tuile-film` for
  one minor release. A host that sets a `Look` or feeds `FrameLight` through `Overlays::light`
  changes an import.
- **Film golden risk: high.** Moving WGSL must keep the film output bit-identical. Gate it with
  a byte-compare of the headless film tests (`crates/tuile-film-gpu/tests/headless.rs`) and of a
  pinned reference film before and after. Move the code first and change no maths.
- **wasm.** The crate must compile for wasm32 and is added to the guard.
- **Conflicts.** It collides head-on with **tuile-sky**, which edits `hillaire.rs`,
  `present.wgsl`, `resolve.wgsl`, `sky_view.wgsl` and `look.rs` (`instant`, `moon`). Land
  tuile-sky first, then extract.
- **Unlocks.** The interactive viewer, mobile and headset get the same sky, sun, exposure and
  look as the film. The "two modes disagree" bug class becomes testable.

**5.2 — Shared consumer-side scene state (M).**
- Make `tuile_core::drive::SceneState` (or a new `tuile-core::scene` module) the single
  selection/ancestry/generation book.
- tuile-wgpu `ContentPump` (`pump.rs:31-56`) and tuile-web adopt it. The web page then receives
  "what to draw" already resolved, including the ancestor climb, instead of re-deriving it in JS.
- Bound `ancestry` by pruning on evict, since the server already sends the closure.
- Breaks: tuile-wgpu internal. The JS message shape changes.
- Film: none (the bake already uses it).
- Unlocks: the never-black guarantee in every consumer, and an easier mobile and headset port.

**5.3 — One camera model (M).**
- Move `FrameCamera`'s projection (reverse-Z infinite, eye-relative) into `tuile-camera` as a
  `Projection` / `EyeCamera` type.
- tuile-film builds it from `BakedView`; `GlobeCamera::view_proj` (`lib.rs:322-369`) produces the
  same type.
- Use `tuile_core::geo` as the one rebasing utility in film-gpu instead of the inline copies
  (`lib.rs:2078, 2154, 2254`).
- tuile-film then depends on tuile-camera (pure, wasm-OK).
- Breaks: `tuile_camera::view_proj` signature, `FrameCamera` location.
- Film risk: medium. Reverse-Z is the film's existing choice, so film output must stay
  identical. Interactive depth changes, so re-run the orbit probe.
- **Conflicts with tuile-ios**, which touches `app/frame.rs`, `setup.rs` and `input.rs`. Land
  tuile-ios first.

**5.4 — Overlays and per-frame light as a shared host API (M, after 5.1 and 5.3).**
- Move `Overlays`, `OverlayMesh`, `OverlayCamera` and `OverlayEyes` (`crates/tuile-film/src/overlay.rs`)
  to a neutral crate: `tuile-look`, or `tuile-scene` if you prefer.
- Port the world-space overlay pass (`crates/tuile-film-gpu/src/shaders/overlay.wgsl`) to
  tuile-wgpu.
- Add typed helpers for lines, ribbons and markers in the library, lifted from
  `crates/tuile-film-native/src/bin/tuile-film-render.rs:245, 284`.
- Keep `OverlayFactory` and `SegmentOrder` in film-native: they are the film-farm binding of
  that API.
- Breaks: a host implementing `Overlays` changes its import path; the trait itself is
  unchanged.
- Film risk: medium (overlay goldens, `crates/tuile-film-native/tests/overlays.rs`).
- Unlocks: the host's overlays live in the app, in the browser and in the headset, from one
  implementation.

**5.5 — Stereo in the live viewer (L, after 5.3).**
- Move `Stereo` and `EyePair` (`crates/tuile-film/src/stereo.rs`) into tuile-camera.
- Teach tuile-wgpu to render N views from one prepared scene: a multiview pass, or two passes
  sharing residency.
- The traversal already accepts several `ViewState`s (`ClientMessage::ViewerState { views }`).
- Film risk: low if the stereo maths moves verbatim.
- Unlocks: live headset output. Spatial video stays film-side, in tuile-mp4 and film-native.

**5.6 — Bounded and device-sized everything (M).**
- Introduce two plain runtime values in tuile-core (pure, wasm-OK):
  - `DeviceProfile`: memory, GPU limits as plain numbers (not wgpu types), decode and I/O
    threads, disk cache, connections, and a device class (phone, laptop, farm, browser).
  - `RenderIntent`: real-time or frame-exact, mono or stereo, window or offscreen, look tier.
- `cfg` stays in the platform crates; the shared layers only read these two values.
- `TraversalConfig::for_profile(..)` replaces `interactive_globe` and `exact_traversal`. The
  bake is then simply `FrameExact` on a `Farm` profile.
- Derive from it:
  - `Config::interactive_globe` budgets;
  - the foyer sizes;
  - connections per host;
  - the offload pool size;
  - the tokio worker count.
- Replace the film clear-all texture caches with the core `ImageryPool` or an LRU, shared
  between film-native and film-web.
- **Conflicts with tuile-ios**, which adds `viewer/src/profile.rs` and `device.rs`. Fold that
  branch's profile into the core type rather than writing a second one.

**5.7 — Radiometry for every mode (M, after 5.1).**
- Calibration is a store-side product (`docs`: "calibration in the store").
- Expose the calibrated layer as an ordinary imagery source, or as a grade in `tuile-look`
  applied by tuile-wgpu, so the app shows the film's colours.
- Film risk: low.
- Open question: the licence of derived layers for commercial imagery (§6).

**5.8 — Split the film-family plumbing (M).**
- Move the pack reader `tuile_bake::tiles::Tiles` and `TapeBake` out of the baker so that
  `tuile-film-native` stops depending on `tuile-bake` (`crates/tuile-film-native/Cargo.toml:22`).
- Merge the film-native and film-web `TEXTURES_HELD` and `Held`/`RemoteBlocks` caches into one
  module, in `tuile-film` or `tuile-repository`.
- Cut `tuile-repository → tuile-farm`.
- **Conflicts with tuile-sky** (`tuile-bake/src/tape.rs`, film-native `engine.rs`).

**5.9 — Make the interactive renderer run on the web (L, after 5.1 and 5.2).**
- Build tuile-wgpu for WebGPU, as tuile-film-gpu already is.
- Retire the GLSL copy, or keep THREE.js as a façade fed by the same scene state and a
  generated shader. The manifesto keeps "a JavaScript scene library on the page" as a target.
- Decide which one is tier 1 (§6).

### Order and dependencies

| Group | Moves |
|---|---|
| Now | 5.0a, 5.0b, 5.0c, 5.0d |
| After tuile-sky lands | 5.1 → 5.4, 5.7 |
| After tuile-ios lands | 5.3, 5.6 → 5.5 |
| Any time after 5.0 | 5.2, 5.8 |
| Last | 5.9 |

## 6. Risks and open questions

1. **Bit-exactness against unification.** Sharing WGSL between a forward fragment shader
   (tuile-wgpu) and a compute resolve (film-gpu) can change float evaluation order. Is the
   contract "film bit-identical across the move", or "film within tolerance after one
   re-baseline"? This decides how mechanical 5.1 must be.
2. **What does interactive pay for physical air?** Hillaire LUTs per frame on a phone may be
   too slow.
   - One option is a tiered `Look`: a closed form on mobile, LUTs on desktop and film.
   - Is a visible difference between tiers acceptable, given the manifesto says
     "disagreement is a bug"?
3. **Which browser renderer is tier 1:** tuile-wgpu on WebGPU, or THREE.js? Maintaining both
   doubles the shading surface.
4. **Park or keep USD/Hydra.** Parking them removes a "bridge to other ecosystems" from the
   goals. Keep them outside the workspace and outside CI?
5. **Bake unboundedness.** The bake trades memory for exactness (`usize::MAX`). Is a farm job
   allowed an unbounded heap, or must the bake page out residency like everything else?
6. **Two in-flight branches touch the very files the moves need.** Landing order matters more
   than the plan.
7. **Calibrated layers from commercial imagery.** Serving them interactively makes a derived
   layer user-visible. The licence must be checked before move 5.7.
8. **Main-thread `block_on` at startup** (`crates/tuile-viewer/src/lib.rs:215, 231`) violates
   principle 5. The tuile-ios `launch.rs` fixes it for iOS. Should desktop adopt it too?

## 7. Road to 1.x

### 7.1 Crate set for 1.0

The target is a small, layered public surface: core → render → modes → hosts and connectors.

| Layer | 1.0 published crate | Built from | Notes |
|---|---|---|---|
| core | **tuile-core** | core + b3dm (merge b3dm in as a module, or keep it as a tiny codec) | `pub use glam;` |
| core | **tuile-globe** | terrain + planetary (merge) | terrain decoding and globe assembly are one concern for users |
| look | **tuile-atmosphere** | as is | pure, wasm |
| look | **tuile-look** | new, move 5.1; also absorbs `Overlays` and stereo types (moves 5.4 and 5.5), or a separate **tuile-scene** for those | pure, wasm |
| camera | **tuile-camera** | camera + film camera + stereo (moves 5.3, 5.5) + tape as a `record` feature (needs a wasm-safe MCAP or the feature off on wasm) | |
| render | **tuile-wgpu** | as is, with the look | `pub use wgpu;` |
| render | **tuile-film** | film + film-gpu merged (reader and GPU renderer of a pack), with `gpu` as a default feature | the pack format comes with it |
| format | **tuile-pack** | as is, flatbuffers private | |
| platform | **tuile-native** | native-fetchers + storage-foyer merged, features `http`, `disk-cache` | |
| platform | **tuile-web** | as is | npm package too (§8) |
| connectors | **tuile-cesium-ion**, **tuile-bing** | as is | nominative names are allowed |
| server | **tuile-tile-server** | as is, once `WsStream` / static serving exist, or stays 0.x | |
| app | **tuile-viewer** | `publish = true` only if `ViewerHost` is a promise; otherwise keep it as an app crate | |

**Unpublished or experimental** (`publish = false`, version 0.x, outside the semver promise):
- tuile-film-native (a binary-heavy farm tool; it could become a `tuile-film` `native` feature later);
- tuile-film-web, which ships as an npm bundle, not a crate;
- tuile-bake, tuile-farm, tuile-repository, tuile-pack-api and tuile-pack-worker;
- tuile-radiometry, until its API settles (research front);
- tuile-mp4, tuile-metrics and tuile-ui;
- tuile-viewer-bundle;
- tuile-usd and tuile-hydra (parked, move 5.0c).

**Splits.**
- `tuile-core::raster` (116 KB) splits into a public layer-table API and private machinery.
- `tuile-tile-server` (20 public modules, `crates/tuile-tile-server/src/lib.rs:34-53`) splits
  into its storage API and its internal tiers.

### 7.2 Public API review

**Dependency types that leak.**
- glam is pre-1.0 and appears in nearly every public struct, for example:
  - `ContentHints` (`crates/tuile-core/src/content.rs:64-71`);
  - `GlobeCamera` (`crates/tuile-camera/src/lib.rs:79-84`);
  - `Look` (`crates/tuile-film/src/look.rs:130`);
  - `ReferenceIllumination` (`crates/tuile-atmosphere/src/illumination.rs:49`).
- wgpu:
  - `GpuContext { pub device, pub queue }` (`crates/tuile-wgpu/src/context.rs:115-117`);
  - public buffer fields (`prepare.rs:50-51, 86`, `targets.rs:27-28`);
  - `FilmGpu::new(wgpu::Device, wgpu::Queue, ..)` (`crates/tuile-film-gpu/src/lib.rs:624`).
- flatbuffers: `pub use generated::tuile::pack as fb` (`crates/tuile-pack/src/lib.rs:64`),
  re-exported again by `crates/tuile-film/src/lib.rs:45`.
- pmtiles: `pub use pmtiles::{Compression, TileType}` (`crates/tuile-tile-server/src/lib.rs:65`).
- object_store: `TileStore::new(Arc<dyn ObjectStore>)` (`store.rs:363`).
- Error variants wrapping external types: `mcap::McapError` (`crates/tuile-tape/src/lib.rs:68`),
  `gltf::Error` (`content.rs:23`), `object_store::Error` and `PmtError`
  (`crates/tuile-tile-server/src/lib.rs:84-86`).
- tokio: `tuile_metrics::serve(&Handle)` (`crates/tuile-metrics/src/lib.rs:57`).
- axum: `serve(axum::Router)` (`crates/tuile-film-native/src/serve.rs:653`).
- Policy: re-export the dependencies a user must name (glam, wgpu, bytes, url), and wrap the
  ones they should not (flatbuffers, pmtiles, object_store, mcap, gltf errors).

**What to change.**
- **`#[non_exhaustive]`: zero in the workspace.** It is needed on:
  - the protocol: `ClientMessage`, `ServerMessage`, `StreamError` (`protocol.rs:29, 90, 232`);
  - content: `TileContent`, `ContentFormat` (`content.rs:32, 43`; i3dm, pnts and cmpt are
    planned);
  - every error enum;
  - config and data structs: `traversal::Config` (15 public fields, `traversal.rs:165`),
    `GlobeOptions` (`crates/tuile-planetary/src/lib.rs:493`), `Look`, `FrameLight`,
    `ReferenceIllumination`, `DecodedMesh`, `Settings`.
- **Configuration.** Everything is a struct literal with public fields. At 1.0, keep public
  fields on plain data and make config `#[non_exhaustive]` with `Default` plus `with_*` setters.
  Rename the re-export `tuile_core::Config` (`lib.rs:64`) to `TraversalConfig`.
- **Errors.**
  - Use one `thiserror` type per crate.
  - Remove `Result<_, String>`:
    - `planetary::Sources` (`sources.rs:39-43`);
    - `ViewerHost::connect` (`crates/tuile-viewer/src/embed.rs:231`);
    - `Notify::segment_done` (`crates/tuile-film-native/src/serve.rs:138`);
    - all of tuile-bake.
  - Replace `pub type Error = Box<dyn Error>` (`crates/tuile-film-native/src/lib.rs:61`) and the
    stringly `NativeFetchError(String)`, `StoreError(String)` and
    `TerrainSourceError(pub String, pub Absence)` (`crates/tuile-terrain/src/source.rs:30`).
  - anyhow appears only in tuile-viewer internals, which is fine.
- **Async and Send.**
  - Finish the `maybe_send` pattern. `TileFetcher`, `ContentStore`, `TileLoader`, `TileTree`,
    `ImageryProvider` and `GeometryStream` keep `Send + Sync` supertraits even on wasm
    (`fetch.rs:72`, `storage.rs:42`, `source.rs:109, 195`, `raster.rs:1332`, `protocol.rs:240`).
  - Introduce `MaybeSend` / `MaybeSync` aliases so `!Send` browser transports (WebSocket, JS
    objects) can implement them.
  - Keep `async_trait`, since every trait is used as `dyn`, and decide it once.
- **Public modules.**
  - Shrink tuile-core's 22 `pub mod`s (`crates/tuile-core/src/lib.rs:30-51`).
  - `determinism`, `offload`, `seam`, `stitch`, `metrics`, `implicit` and most of `raster`
    become `pub(crate)`, or move behind an `unstable` feature.
- **Features.**
  - The tuile-viewer defaults (`application`, `current-location`) are macOS-specific.
  - `tuile-ui/statusbar` is forced on (`crates/tuile-viewer/Cargo.toml:37`).
  - Optional backends (reqwest, foyer, object_store) should be features of the merged platform
    crates.

**The main traits.**
- **`GeometryStream`** (`protocol.rs:240-264`).
  - Change the bound from `Send` to `MaybeSend`.
  - Fix `next_message`, which returns a future only when `Self: Unpin`, without saying so.
  - It has a single implementor. At least one more binding (`WsStream` or an HTTP pull) should
    exist before 1.0 freezes the trait; otherwise the trait is unproven.
- **`TileFetcher`** (`fetch.rs:70-86`): sound, apart from the Send bound.
- **`PrepareRenderResources`** (doc 01) does not exist. Either drop it from the architecture
  doc or make tuile-wgpu's `prepare` implement it. Freezing a documented-but-absent trait is
  worse than removing it.
- **`Overlays`** (`crates/tuile-film/src/overlay.rs:222`): synchronous and object-safe, which is
  good. Add a `Send` variant or a `MaybeSend` supertrait.
- **`OverlayFactory`** (`serve.rs:100-108`): its associated `Order` type makes it non-object-safe
  and spreads `SegmentOrder<O>` / `Notify<O>` through serve. Keep it in the unpublished
  film-native.
- **`Look`, `FrameLight`, `ReferenceIllumination`**: structs with many public fields.
  - Make them `#[non_exhaustive]` with constructors.
  - Rename the radiometry `Look` (`crates/tuile-radiometry/src/matrix.rs:128`) to remove the
    clash.
- **Order types.** There are three `Order`s (`film-native render.rs:55`, `farm produce.rs:84`,
  `serve.rs:44`), all film-side. `SegmentOrder` uses `deny_unknown_fields`, so a new field
  breaks old readers. Version the wire form instead.

**Concrete breaking changes before 1.0.**
1. `#[non_exhaustive]` on the protocol and content enums (`crates/tuile-core/src/protocol.rs:29, 90, 232`; `content.rs:32, 43`).
2. `#[non_exhaustive]` on every error enum (`content.rs:17`, `fetch.rs:16`, `source.rs:267`, `raster.rs:30`, `crates/tuile-pack/src/lib.rs:133`, `crates/tuile-film-gpu/src/lib.rs:90`, `crates/tuile-tile-server/src/lib.rs:78`, `crates/tuile-tape/src/lib.rs:64`).
3. Rename `Config` to `TraversalConfig`, make it `#[non_exhaustive]` with setters (`traversal.rs:165`, `lib.rs:64`); the same for `GlobeOptions` and the native config structs.
4. `MaybeSend` supertraits on the six I/O traits plus `GeometryStream`.
5. Fix `GeometryStream::next_message` (`protocol.rs:247-264`).
6. Make `tuile_pack::fb` private and drop the re-export in `tuile-film` (`crates/tuile-pack/src/lib.rs:64, 836`; `crates/tuile-film/src/lib.rs:45`).
7. Wrap the pmtiles, object_store, mcap and gltf types (`crates/tuile-tile-server/src/lib.rs:65, 84-86`; `crates/tuile-tape/src/lib.rs:68`; `content.rs:23`).
8. Private `GpuContext` fields with accessors; `pub use wgpu` (`crates/tuile-wgpu/src/context.rs:115-117`).
9. Typed errors in place of `String` and `Box<dyn Error>` (`sources.rs:39-43`, `embed.rs:231`, `crates/tuile-film-native/src/lib.rs:61`, `crates/tuile-terrain/src/source.rs:30`).
10. Replace the tuple returns in `crates/tuile-planetary/src/lib.rs:1608-1672` with a named `Globe` struct, and unify `in_process` / `in_process_with` (`crates/tuile-core/src/runtime.rs:53, 68`).
11. Shrink tuile-core's public modules (`crates/tuile-core/src/lib.rs:30-51`).
12. Move `Look`, `FrameLight`, `Overlays` and stereo into shared crates (moves 5.1, 5.4, 5.5) **before** 1.0, so that 1.x does not freeze them in `tuile-film`.
13. Unify the camera types (move 5.3).
14. Delete the dead `http` and `reqwest` features (move 5.0d).
15. Decide what is published: set `publish = false` on the tools (§7.1).

### 7.3 Stability tooling

What exists: one workflow (`.github/workflows/ci.yml`) running fmt, clippy `-D warnings`,
`cargo test --workspace` on ubuntu and macOS (doctests included), the wasm guard on tuile-core
only, and `cargo doc -D warnings`.

| Tool | Proposal |
|---|---|
| MSRV | `rust-version = "1.91"` is declared but never tested. Add an MSRV job. Policy: MSRV may rise only in a minor release, at most to stable − 2. |
| cargo-semver-checks | Run on every PR against the last release, for published crates only. |
| cargo-public-api | Commit snapshot files per published crate; a diff in review shows any API change. |
| Docs | `#![warn(missing_docs)]` on published crates (none today); add docs.rs metadata. |
| Examples | Build examples in CI (they are, as members) and run headless ones as tests (`orbit-probe`, `globe-bulk` with fixtures). |
| Changelog | `CHANGELOG.md` per published crate, or one with crate sections; release-plz or cargo-release (none today). |
| Deprecation | Keep `#[deprecated]` re-exports for one minor release before removal in the next major. A moved type keeps its old path as a re-export through all of 1.x. |
| Target matrix | wasm32 for every crate that claims it; `aarch64-apple-ios` and the simulator; `cargo hack --each-feature`; cargo-deny (licences and advisories, important for a dual-licensed project). |
| Supply chain | Pin actions by SHA. |

### 7.4 What can stay unstable after 1.0

- **The `unstable` cargo feature on published crates.** It covers:
  - atmosphere tuning constants and calibration (`hillaire::Calibration`);
  - `Look` presets;
  - raster internals;
  - determinism counters.
- **Unpublished or 0.x side crates:**
  - radiometry (colour calibration research);
  - super-resolution, when it lands;
  - the farm (`tuile-farm`, `tuile-film-native` serve and encoders);
  - bake;
  - repository and the bench;
  - USD/Hydra.
- **The pack format.** Version it in the file with a reader that accepts N−1. It must not be
  frozen by the crate's semver.

### 7.5 Roadmap to 1.0

| Step | Size | Depends on | Host impact on upgrade |
|---|---|---|---|
| 1. CI first: wasm matrix, iOS check, MSRV, cargo-deny (§7.3) | S | — | none |
| 2. Moves 5.0a–d | S | 1 | a host using `HttpFetcher` or `ReqwestHttp` must supply its own transport |
| 3. Land tuile-sky and tuile-ios | M | — | new fields on `Look` (moon, instant); viewer gets a lifecycle |
| 4. Moves 5.1, 5.3, 5.4 (shared look, camera, overlays) | L | 3 | a host using overlays or `FrameLight` changes imports; deprecated re-exports for one minor |
| 5. API hygiene: non_exhaustive, errors, MaybeSend, wrapped deps, private modules (§7.2 items 1–11) | M | 4 | struct literals become constructors or setters; matches on enums need `_ =>` |
| 6. Crate merges and publication set (§7.1) | M | 4, 5 | `Cargo.toml` dependency names change (e.g. `tuile-terrain` + `tuile-planetary` → `tuile-globe`) |
| 7. A second `GeometryStream` binding (`WsStream` or HTTP pull) to prove the trait | M | 5 | none |
| 8. semver-checks and public-api snapshots switched on; 1.0.0-rc | S | 6 | none |
| 9. Moves 5.5, 5.6, 5.9 may land in 1.x as additive features | L | 8 | additive |

## 8. Industrialising the render targets: protocols, bundles, layout

The goal is to treat the targets as one systematic whole: browser, desktop, mobile, headset,
film on a laptop or a farm, server, and USD hosts. In practice that rests on three things,
which this section takes in turn:

- **Protocols:** a few versioned contracts between parts.
- **Bundles:** one reproducible bundle per target.
- **Layout:** a repository whose layout makes the layering visible.

### 8.1 The stack, today and target

**Today.** Each column below is a vertical silo. The only shared floor is the core plus the
terrain and connectors.

```text
 BUNDLES   web example     .app (viewer)    film CLI / farm image     Cloud Run / Worker     Blender+USD image
           (wasm-pack by   (cargo, no       (integrations/film,       (pack-api,             (Dockerfile.globe:
            hand)           xtask)           integrations/bake)        pack-worker)           hydra .a + plugin + usd)
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 MODES     interactive     interactive      film / stereo (MV-HEVC)   server (tile store,    USD host (offline)
           (browser)       (desktop)                                  bench API)
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 BACKENDS  THREE.js +      tuile-wgpu       tuile-film-gpu            —                      tuile-hydra C ABI
           GLSL copy       (own WGSL)       (own 15 WGSL)                                    → C++ Hydra plugin
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 RENDER    none            tuile-camera,    tuile-film: Look,         —                      tuile-usd stage
 LAYERS                    aerial.wgsl      FrameLight, Overlays,
                                            Stereo, FrameCamera,
                                            Hillaire, radiometry
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 PROTOCOLS JS messages     InProcessStream  pack v3 (flatbuffers),    bench HTTP routes,     C ABI (ffi.h hand copy)
           (ad hoc)        (in-process)     SegmentOrder JSON,        PMTiles + catalog
                                            MCAP tapes
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 CORE      tuile-core (formats, SSE traversal, GeometryServer, ResidentCache, raster), b3dm, terrain, planetary
```

**Target.** The shared layers span every column. Every column reaches the core through a named
protocol. `→` marks a move, with the section number that carries it.

```text
 BUNDLES   npm @tuile/web    .app / .ipa      CLI tuile-film +      server binary /     C ABI + OpenUSD
           (+ film-web)      (xtask app)      farm image            Worker / image      plugin (tier 3)
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 MODES     interactive (browser · desktop · mobile) · headset (live stereo §5.5) · film · server · USD host
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 BACKENDS  tuile-wgpu (native + WebGPU §5.9)  ·  tuile-film (film renderer on the same look)  ·  hydra façade
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 RENDER    tuile-look (Look, FrameLight, tone, lighting WGSL §5.1) · overlays §5.4 · tuile-atmosphere
 LAYERS    · tuile-camera (one projection, EyePair §5.3/5.5) · instruments (§8.6)
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 PROTOCOLS tuile-protocol: stream messages + glb wire (InProcess · WebSocket · HTTP pull) · pack vN ·
           tile-store API · segment order v1 · tape/plan MCAP schema · C ABI (generated header)
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 PLATFORM  tuile-native (fetch + disk cache) · tuile-web (fetch, worker) · DeviceProfile / RenderIntent
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 CORE      tuile-core (+b3dm) · tuile-globe (terrain + planetary)
```

What moves:
- `Look`, `FrameLight`, `Overlays`, `Stereo` and `FrameCamera` leave `tuile-film` and go down
  into the render layers.
- The wire types leave `tuile-core::protocol` (which keeps the in-process binding) and the
  film-native crate, and go into a protocol layer.
- The GLSL copy disappears.
- Fetchers and caches merge into one platform crate per family.
- The Blender image is parked (§4, move 5.0c).

### 8.2 The protocols

| Contract | Defined in | Versioned? | Specified in a doc? | One definition or copies | Compatibility rule today |
|---|---|---|---|---|---|
| GeometryStream messages (`ClientMessage` / `ServerMessage`) | `crates/tuile-core/src/protocol.rs:29, 90` | no | `docs/01-architecture.md` (outdated field list) | one Rust definition, plus an ad-hoc JS mirror in `crates/tuile-web/src/worker.rs:206-290` and `examples/web-viewer/www/main.js` (already out of sync, §2.1) | none; no serde derive, so **no wire form exists** |
| WebSocket + glb wire | — | — | docs 01 and 12 | **does not exist** (no `WsStream`) | — |
| Static HTTP 3D Tiles pull | OGC 3D Tiles 1.1; `TileFetcher` | the standard's | primer (doc 02) | standard | standard |
| Pack format | `crates/tuile-pack/schema/tuile_pack.fbs` + `crates/tuile-pack/src/lib.rs:71-80` (`VERSION = 3`, `EMBEDDED_VERSION = 2`, `READABLE` range) | **yes**, layout version + readable range | in code comments | one schema, generated code | reader accepts a range; schema test at `crates/tuile-pack/tests/schema.rs` |
| Tile store / PMTiles | `crates/tuile-tile-server/src/{catalog,manifest,archive}.rs`; manifests have optimistic `UpdateVersion` (`manifest.rs:74`) | per manifest, not per format | no | one Rust definition | PMTiles standard for archives; catalog JSON unversioned |
| Bench / store HTTP API | `crates/tuile-repository/src/bench.rs:1-20` (route table) | no | in that comment | **one** definition, two adapters (pack-api native, pack-worker edge) — the good pattern | none |
| Film segment order (JSON to the render service) | `SegmentOrder<O>` (`crates/tuile-film-native/src/serve.rs:44-61`), POST `/segments` (`serve.rs:648`) | no | no | one Rust definition, but the host's `O` is generic and the host builds the JSON on its side (a copy by construction) | `deny_unknown_fields`: any new field breaks older services |
| Tapes and plans (MCAP) | `crates/tuile-tape/src/lib.rs:117-126` (`tuile.Camera` schema + embedded JSON schema) | schema name, no version number | no | one Rust definition | readers ignore unknown channels (MCAP) |
| tuile-hydra C ABI | `crates/tuile-hydra/src/ffi.rs` ↔ `integrations/hydra/src/ffi.h` (hand-written, 170 lines) | no ABI version symbol | doc 14 | **two copies** (no cbindgen) | none; a field order mismatch is silent UB |

**Proposal.**
- **A `tuile-protocol` crate** (pure, wasm-OK) holds every wire type that crosses a process
  boundary:
  - stream messages with serde and a `glb` content form;
  - the segment order;
  - catalog and manifest JSON;
  - the tape schema descriptor.
  - Each family carries a `const VERSION` and a `Hello`/header version field.
  - The pack keeps its own crate (it is a file format with generated code), with the same
    discipline.
- **Rules.**
  - Wire structs are `#[non_exhaustive]`, with `#[serde(default)]` on new fields.
  - `deny_unknown_fields` is dropped on anything a newer peer may send. Unknown fields are
    tolerated, and version gates refuse what cannot be understood.
- **Schemas next to the types.**
  - `schema/` holds the `.fbs`, JSON Schema files generated from the serde types (`schemars`),
    and the C header generated by `cbindgen` in `build.rs`, replacing the hand copy.
  - The JS side gets TypeScript types generated from the same JSON Schema, which removes the
    hand-mirrored message shapes in `worker.rs` and `main.js`.
- **Conformance tests between both ends.**
  - Golden encoded messages, one file per version, under `tuile-protocol/tests/vectors/`.
  - Both the Rust reader and each foreign reader (JS worker, C++ plugin, the host's JSON
    builder) must decode them.
  - A round-trip test runs per binding: InProcess against WebSocket against HTTP pull, on the
    same scene, with the same selection.

### 8.3 Bundling

There is one entry point, `cargo xtask bundle <target>`. A `bundles/` directory holds the
per-target recipes: Dockerfiles, Xcode project, npm `package.json`, CMake.

| Bundle | Contents | Fed by | Built how | Artifact → published to |
|---|---|---|---|---|
| web | worker wasm + JS glue + TS types | tuile-web, tuile-core, globe, connectors, tuile-protocol | wasm-pack / wasm-bindgen, wasm-opt | npm `@tuile/web` |
| film-web | film worker wasm + page | tuile-film (gpu), tuile-protocol, repository client | wasm-pack (today by hand, see `crates/tuile-pack-worker/wrangler.toml:17`) | npm or the edge bucket |
| app-macos | `.app` (icon from tuile-viewer-bundle) | tuile-viewer, tuile-wgpu, tuile-look, tuile-native | cargo + bundle step | GitHub release, dmg |
| app-ios | `.ipa` | same, iOS profile (tuile-ios branch) | cargo `aarch64-apple-ios` + xcodebuild | TestFlight (later) |
| app-linux / windows | binary | same | cargo | GitHub release |
| film-cli | `tuile-film-render`, `tuile-film-produce` | tuile-film, film-native, bake | cargo | GitHub release |
| film-farm image | CLI + Vulkan loader + encoders | `integrations/film/Dockerfile:71-102` | remote BuildKit | container registry, Cloud Run job |
| bake image | bake + farm | `integrations/bake/Dockerfile:34` | remote BuildKit | registry |
| server | tile store + bench API, native and Worker | tile-server, repository, pack-api, pack-worker | cargo / worker-build | registry, edge |
| hydra plugin (tier 3) | `libtuile_hydra.a` + C++ procedural plugin + `plugInfo.json` | tuile-hydra, `integrations/hydra` (CMake) | CMake against a prebuilt OpenUSD image (`integrations/hydra/Dockerfile.openusd`, USD v26.08) | registry image only |
| blender (parked) | Blender with shared USD + plugin + usd stage | `integrations/blender/Dockerfile.globe` (+ `Dockerfile.blender-shared-usd`, 48 KB) | GCP VM | parked: not rebuilt |

Every bundle writes a `bundle.json` recording:
- the engine version and git SHA;
- the features and target;
- the toolchain;
- the protocol versions it speaks.

That is the reproducibility receipt. Heavy bundles (images, OpenUSD, Blender) build only on the
remote builders, with one cache key per stage.

### 8.4 Crate and directory layout

The options:

| Option | Pros | Cons |
|---|---|---|
| Flat `crates/` (today) | path deps trivial; `cargo new` conventions; nothing to learn | 31 crates with no visible layering; CI path filters must list crates; ownership invisible |
| **By layer** (`crates/core`, `crates/protocol`, `crates/render`, `crates/platform`, `crates/modes`, `crates/hosts`) | the dependency rule *is* the tree ("no arrow points up"); CI path filters per layer (`crates/core/**` triggers everything, `crates/modes/film/**` only film); CODEOWNERS by layer; discoverable | one-time move breaks in-flight branches; deeper relative paths |
| By backend (`wgpu/`, `hydra/`, `usd/`) | groups GPU code | most crates have no backend (core, protocol, connectors); hides layering |
| By target (`targets/web`, `targets/ios`, `targets/film`) | matches bundles | invites the per-mode fork the manifesto forbids; shared crates have no home |

**Recommendation: by layer for crates, by target for bundles.** Bundles are recipes, not
libraries, so the target axis lives in `bundles/`, and libraries stay layer-sorted.

```text
tuile/
├── crates/
│   ├── core/
│   │   ├── tuile-core/            (+ b3dm merged as a module)
│   │   └── tuile-globe/           (terrain + planetary)
│   ├── protocol/
│   │   ├── tuile-protocol/        (stream wire, segment order, catalog, tape schema, C ABI header gen)
│   │   └── tuile-pack/            (pack format, fbs)
│   ├── render/
│   │   ├── tuile-atmosphere/
│   │   ├── tuile-look/            (new: Look, FrameLight, tone, lighting WGSL, overlays)
│   │   ├── tuile-camera/          (+ film camera, stereo, tape as a feature)
│   │   ├── tuile-wgpu/
│   │   └── tuile-film/            (film + film-gpu merged)
│   ├── platform/
│   │   ├── tuile-native/          (native-fetchers + storage-foyer)
│   │   └── tuile-web/
│   ├── connectors/
│   │   ├── tuile-cesium-ion/
│   │   └── tuile-bing/
│   ├── modes/                     (applications and services; publish = false unless stated)
│   │   ├── viewer/                tuile-viewer, tuile-ui, tuile-metrics, tuile-viewer-bundle
│   │   ├── film/                  tuile-film-native, tuile-film-web, tuile-mp4, tuile-bake,
│   │   │                          tuile-farm, tuile-radiometry
│   │   └── server/                tuile-tile-server, tuile-repository, tuile-pack-api, tuile-pack-worker
│   └── hosts/                     (tier 3, excluded from default members)
│       ├── tuile-hydra/
│       └── tuile-usd/
├── bundles/                       (one directory per target: web, film-web, app-macos, app-ios,
│                                   film-farm, bake, server, hydra; Dockerfiles, Xcode, CMake, package.json)
├── integrations/                  → removed: film/ and bake/ go to bundles/,
│                                    hydra/ C++ goes to bundles/hydra (or crates/hosts/tuile-hydra/cpp),
│                                    blender/ goes to an archive branch (parked)
├── examples/                      (unchanged role: wgpu-viewer, web-viewer, orbit-probe, globe-bulk,
│                                   warm-cache; wasm-globe retired once tuile-web is fixed)
├── xtask/
└── docs/
```

Cargo notes:
- Crate *names* do not change with directories, so publishing is unaffected.
- `workspace.members` can use globs (`crates/*/*`, `crates/modes/*/*`).
- `default-members` excludes `crates/hosts/*`, so the laptop never builds them by accident.

**Migration order.** A move-only PR is cheap for Cargo (paths only, no code), but every
in-flight branch then needs a rename-aware rebase. Do it:
1. after tuile-sky and tuile-ios land;
2. as one PR containing only `git mv` and Cargo path edits;
3. before the merges of §7.1, so each merge PR is small and lands in the right place;
4. with the CI path filters and CODEOWNERS in the same PR.

### 8.5 Is the Hydra/USD part under CI?

**Verified, and the caller's reading is correct.** `.github/workflows/ci.yml` has only cargo
jobs:
- fmt;
- clippy `--workspace --all-targets` (l.24-25);
- test `--workspace` on Linux and macOS (l.43);
- the wasm guard on tuile-core (l.53);
- doc `--workspace` (l.63).

What that means for each part:
- `tuile-usd` and `tuile-hydra` are workspace members (`Cargo.toml:22, 28`), so they get lint,
  test and doc. Their Rust tests run; tuile-hydra's tests write packs through `tuile-pack`
  (`ffi.rs:568, 772`).
- **Nothing builds `integrations/hydra`.** That covers CMake (`CMakeLists.txt:25-54`), the
  C++ in `src/`, `Dockerfile.openusd`, and `build-macos-metal.sh` / `build-ubuntu.sh`.
- **Nothing tests `integrations/blender`.** That covers the Dockerfiles, `render_usd.py`,
  `render_job.sh`, and `test_job_scripts.py`, a 47 KB pytest suite that no workflow runs.
- **Nothing checks the C ABI against `ffi.h`.** That is the riskiest gap: a Rust-side change
  passes CI and breaks the plugin silently.

**What CI for them would cost.**

| Job | Cost |
|---|---|
| OpenUSD from source (v26.08, with OpenVDB per the Dockerfile's comments) | about 1–2 h on 8 cores; memory-bound (the Dockerfile caps `-j` because 4 GiB pods OOM'd, `Dockerfile.openusd:86-132`). Not viable per PR on GitHub runners. |
| Plugin against a **prebuilt, cached OpenUSD image** pulled from the registry | about 5–10 min (cargo staticlib + CMake + C++), feasible nightly or on path filter `crates/hosts/**` |
| An ABI conformance check (cbindgen-generated header diffed against the C++ include, plus one C smoke test calling `tuile_session_*`) | about 1 min, no USD needed: the cheapest real protection |
| Blender image | GCP VM build (c2d-standard-32), not CI material |
| `test_job_scripts.py` | seconds, plain pytest |

**Recommendation: park, with a minimal guard while parked.**
- The downstream host has dropped the Blender/USD path, and Blender exposes no USD plugin
  surface.
- Take `crates/hosts/*` out of default members. Keep the cheap ABI check and pytest as a
  path-filtered job, and build no OpenUSD in CI.
- Revive a nightly prebuilt-USD plugin job only if a USD host (hdGp/usdrecord) becomes a
  goal again.
- The Blender integration goes to an archive branch with its image recipe.

### 8.6 One consolidated order

This merges §5 (rebalancing), §7 (1.x) and §8 (protocols, bundles, layout). Sizes: S, M, L.

| # | Step | Size | From | Why here |
|---|---|---|---|---|
| 1 | CI guards: wasm matrix incl. tuile-web and film-web, iOS check, MSRV, cargo-deny, ABI-header and pytest path jobs | S | §7.3, §8.5 | catches what is already broken before anything moves |
| 2 | Fix tuile-web against the protocol; forward `ancestry` | S | 5.0a | browser black ground, now |
| 3 | Bound the stream channels, offload queue and browser maps | S | 5.0b | mobile and browser safety; small and local |
| 4 | Delete dead features (`http`, `reqwest`, `SkyShell`, unused pub fns) | S | 5.0d | shrinks the surface before review |
| 5 | Park USD/Blender: hosts out of default members, Blender to an archive branch, docs 03/12/13 fixed | S | 5.0c, §8.5 | removes the heaviest build from the picture |
| 6 | **Land tuile-sky and tuile-ios** (rebased on 0.7) | M | branches | everything below touches their files |
| 7 | Move-only layout PR (by layer, `bundles/`, `xtask/`, path filters, CODEOWNERS) | S–M | §8.4 | cheapest right after the branches land, before more branches start |
| 8 | `DeviceProfile` / `RenderIntent` in core, folding in the tuile-ios profile | M | 5.6 | budgets everywhere derive from one place |
| 9 | `tuile-protocol`: serde wire types, versions, schemas, cbindgen header, TS types, vectors | M | §8.2 | unblocks the JS mirror fix for good and the second binding |
| 10 | Shared consumer scene state (`SceneState`) in tuile-wgpu and tuile-web | M | 5.2 | never-black in every consumer |
| 11 | One camera model (reverse-Z, `EyeCamera`, one rebasing) | M | 5.3 | prerequisite for look sharing and stereo |
| 12 | `tuile-look`: move `Look`/`FrameLight`/tone/lighting WGSL, film bit-identical; then tuile-wgpu adopts it with an HDR target | L | 5.1 | the core of "one shading" |
| 13 | Overlays as a shared API, ported to tuile-wgpu | M | 5.4 | host overlays everywhere |
| 14 | Film plumbing split (pack reader out of bake, one texture/block cache, repository ↛ farm) | M | 5.8 | clean film family before merges |
| 15 | Crate merges (core+b3dm, globe, native, film+film-gpu) and publish set | M | §7.1 | in the new tree, one merge per PR |
| 16 | API hygiene: non_exhaustive, errors, MaybeSend, wrapped deps, private modules | M | §7.2 | after the moves, so it is done once |
| 17 | Second `GeometryStream` binding (WebSocket + glb), with conformance vectors | M | §7.5, §8.2 | proves the trait before freezing it |
| 18 | xtask bundles + `bundle.json` receipts; nightly GPU, WebGPU and iOS simulator jobs; cross-target golden scene with the never-black instrument | L | §8.3, §8.6 | targets become repeatable and compared |
| 19 | semver-checks + public-api snapshots; **1.0.0-rc** | S | §7.3 | freeze |
| 20 | 1.x additive: live stereo (5.5), radiometry for every mode (5.7), tuile-wgpu on WebGPU (5.9) | L | §5 | features, not breaks |

**Cross-target verification** (part of step 18):
- One canonical scene and tape is rendered by tuile-wgpu (Metal, lavapipe), the browser
  (headless Chrome WebGPU), film-native, film-web and, nightly, the iOS simulator.
- **Never-black first.** A library version of `tuile-scan-black`
  (`crates/tuile-tape/src/bin/tuile-scan-black.rs`) runs on every frame.
- **Film stays bit-exact against itself.** Split independence and byte-identical bakes
  (`crates/tuile-bake/src/bake.rs:553`, `crates/tuile-farm/tests/assemble_film.rs`).
- **Interactive against film.** A ΔE tolerance per backend, but only after step 12.
- **Budgets.** Frame time and memory per `DeviceProfile`.
- **Instruments.** The film `Observer` trait (`crates/tuile-film-native/src/observe.rs:146`)
  becomes renderer-agnostic, and `orbit-probe` becomes its interactive twin.
- **Human check.** The nightly job publishes a contact sheet and the films, and they get opened,
  following the "every render gets opened" rule.

**Support tiers.**

| Tier | Targets |
|---|---|
| 1 | Linux and macOS desktop, wasm32 + WebGPU (Chrome), film native |
| 2 | iOS, Windows, film-web, the THREE.js façade |
| 3 | Android, live visionOS, Hydra/USD |

**Versioning.**
- Published crates and the npm package move in lockstep through 1.x.
- Apps, images and the plugin record the engine version, the SHA and their protocol versions
  in `bundle.json`.
