<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Tuile

**An OGC 3D Tiles engine and server in Rust** — and, built on it, a globe of
terrain draped with imagery that can be flown interactively or rendered into a
film, frame-exact, on a laptop, in a browser, or on a farm of GPUs.

The 3D Tiles world has tilers that produce static files and clients tied to
their own ecosystems. What it lacks is the part in the middle: a small,
dynamic server with a cache it does not own, and a rendering engine that is
not married to one renderer. Tuile is that part.

Everything is one Cargo workspace. The core knows nothing of GPUs, sockets or
runtimes; everything that draws, fetches or stores plugs into it through
traits.

> **Status: pre-1.0.** The interfaces move. Version numbers are cut when a
> consumer needs one, not when an API is declared stable.

## What is in it

**A render-agnostic core.** `tuile-core` parses tilesets, traverses them by
screen-space error, schedules and decodes content, and hands geometry out as a
*logical geometry server*: a stream of decoded tiles for whoever draws them. It
compiles to `wasm32-unknown-unknown` and depends on no backend.

**A globe.** Quantized-mesh terrain (`tuile-terrain`) crossed with raster
imagery (`tuile-planetary`), with all geospatial arithmetic in `f64` until the
switch to a local frame — raw Earth-centred coordinates never reach `f32`.

**Renderers.** `tuile-wgpu` is the reference backend, native and WebGPU.
`tuile-hydra` exposes the same geometry server through a C ABI for an
OpenUSD/Hydra scene index; `tuile-usd` writes a recorded flight as a USD stage.

**Films.** A camera path is recorded once (`tuile-tape`), baked once into a
*pack* of exactly the tiles each frame needs (`tuile-bake`, `tuile-pack`), and
rendered as many times as wanted without touching a tile source again
(`tuile-film`, `tuile-film-gpu`, `tuile-film-native`, `tuile-film-web`). The
same film renders natively and in a browser worker.

**A tile store.** Source tiles are kept once, in PMTiles archives per zone and
per layer on object storage (`tuile-tile-server`), and read back by blocks
that any HTTP cache can keep (`tuile-repository`).

## The crates

| Area | Crate | What it is |
|---|---|---|
| Core | `tuile-core` | 3D Tiles parsing, SSE traversal, content decoding, geometry streaming |
| | `tuile-b3dm` | The legacy Batched 3D Model container, parsed and encoded |
| | `tuile-terrain` | Quantized-mesh decoding and geographic tiling |
| | `tuile-planetary` | Terrain × imagery, assembled as a tile tree and a loader |
| | `tuile-camera` | A globe camera and its input controller |
| | `tuile-atmosphere` | Sun position, aerial perspective and sky |
| | `tuile-ui` | On-screen controls for viewers |
| Sources | `tuile-cesium-ion` | Connector: asset endpoints, tokens, 3D Tiles, terrain and imagery |
| | `tuile-bing` | Connector: imagery metadata, quadkeys, tiles |
| | `tuile-native-fetchers` | Pooled HTTP client with a browser-style cache |
| | `tuile-storage-foyer` | A memory-over-disk content store |
| | `tuile-web` | Browser transport and an in-worker geometry server |
| Rendering | `tuile-wgpu` | The reference wgpu backend, native and wasm |
| | `tuile-hydra` | C ABI over the geometry server, for a Hydra plugin |
| | `tuile-usd` | A recorded camera path and a globe, as a USD stage |
| Films | `tuile-tape` | Record a camera path and fly it again, exactly |
| | `tuile-bake` | Bake a frame range into a pack |
| | `tuile-pack` | The pack container |
| | `tuile-film` | A baked film read frame by frame — no GPU, no I/O |
| | `tuile-film-gpu` | A film on wgpu: one visibility raster, the rest in compute |
| | `tuile-film-native` | A film rendered natively to an mp4 |
| | `tuile-film-web` | A film rendered in a browser worker |
| | `tuile-radiometry` | Imagery tiles brought to one another's light |
| | `tuile-mp4` | H.264 samples into an mp4, in memory |
| Storage and serving | `tuile-tile-server` | The tile store: archives per zone and layer, compacted in stream |
| | `tuile-repository` | Repositories over the buckets that hold packs, tapes and tiles |
| | `tuile-pack-api` | A read-only HTTP API over those buckets |
| | `tuile-pack-worker` | The same API as an edge worker |
| | `tuile-farm` | A render farm's storage: ranged reads, multipart writes, film assembly |
| | `tuile-metrics` | The core's counters on a metrics endpoint |

Programs carry the `tuile-` prefix: `tuile-bake`, `tuile-film-render`,
`tuile-tape` (`border`, `merge`, `orbit`, `pyrenees`, `zoom`), `tuile-farm`,
`tuile-launch-job`, `tuile-scan-black`, `tuile-trace-dump`, and the examples
under `examples/` (`tuile-wgpu-viewer`, `tuile-orbit-probe`,
`tuile-warm-cache`, …).

## Getting started

You need a recent stable Rust (see `rust-version` in `Cargo.toml`).

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings

# The core must keep compiling for the browser.
cargo check --target wasm32-unknown-unknown -p tuile-core
```

Fly the globe in a window — terrain and imagery come from the connectors, with
a token you supply at run time and never commit:

```bash
CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer
```

Drag to orbit, right-drag to pan, wheel to zoom. `TUILE_RECORD=path.jsonl`
writes the camera path; `TUILE_REPLAY=path.jsonl` flies it again exactly.

Render a baked film to an mp4:

```bash
cargo run --release -p tuile-film-native --bin tuile-film-render -- <packs prefix> --out film.mp4
```

The film renderer reads its packs and the tile store from the buckets named in
its environment. Its options and variables are listed at the head of
[`tuile-film-render.rs`](crates/tuile-film-native/src/bin/tuile-film-render.rs);
[`film-bench.example.toml`](examples/film-web/film-bench.example.toml) shows
how buckets are named.

## How it holds together

- **`tuile-core` never depends on wgpu, tokio, axum or any backend.** I/O and
  rendering go through traits. A heavy dependency tempting its way into the
  core means an abstraction is in the wrong place.
- **The core compiles to wasm**, and the wasm path leans on stable, widely
  adopted crates only.
- **Precision is `f64` until rebasing.** `f32` begins at the local frame.
- **Ground is never black.** A soft, blurry or stale tile is always preferred
  to an absent one: a replacement is created and activated before what it
  replaces is switched off.
- **The project is defined by the standard**, not by other products. Only
  connector crates name the service they connect to.

## Documentation

The design documents live in [`docs/`](docs/):

| Document | Contents |
|---|---|
| [`00-vision.md`](docs/00-vision.md) | Goals, non-goals, constraints |
| [`01-architecture.md`](docs/01-architecture.md) | The workspace, the foundational traits, the dependency rules |
| [`02-3d-tiles-primer.md`](docs/02-3d-tiles-primer.md) | The standard: what is implemented and how |
| [`03-roadmap.md`](docs/03-roadmap.md) | Milestones and acceptance criteria |
| [`20-conventions.md`](docs/20-conventions.md) | Style, tests, CI, licences, commits |

Each crate has its own specification beside them (`10-crate-core.md`,
`11-crate-wgpu.md`, …), and `cargo doc --no-deps --workspace` builds the API
reference. The project's site, with that reference under `/docs`, is built
from [`site/`](site/).

## Contributing

Read [`docs/20-conventions.md`](docs/20-conventions.md) first. In short: every
public trait has tests, every parser is tested on real data, and a test is
written with the behaviour it covers — then the behaviour is reverted to watch
the test fail. A green suite is not proof.

A render is a picture: when one is produced, it is opened and looked at.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. Copyright lapoule.dev. Third-party attributions are in
[`NOTICE`](NOTICE).
