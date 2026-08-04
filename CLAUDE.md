# Tuile — 3D Tiles engine and server in Rust

## What you are building

An open-source Cargo workspace implementing the **OGC 3D Tiles** standard (both consumption AND serving), with a 100% render-agnostic core and multiple backends. The product goal: to be the "Martin of 3D Tiles" — a lightweight dynamic server, deployable from a native binary to a Cloudflare Worker, plus a reusable client rendering engine (wgpu today, RealityKit/Hydra tomorrow).

**There is no equivalent.** The 3D Tiles ecosystem has only tilers (pg2b3dm, py3dtiles) and clients (web and native) tied to their own ecosystems. Dynamic serving with a pluggable cache does not exist. You are breaking new ground.

The executable heart is a **logical geometry server**, not a network server. The streaming mode (the primary mode) is defined as a **core trait** (`GeometryStream`) with three bindings: in-process (decoded — first consumer: wgpu), WebSocket via the `tuile-server` network middleware (geometry serialized as **glb** on the wire — second consumer: USD, notably the Hydra renderer), and static HTTP pull (CDN interop and existing web viewers). `DecodedTileContent` IS a `TileContent` (the `Decoded` form; `Raw` = glb) and never crosses a wire. See `docs/01-architecture.md` and `docs/12-crate-server.md`.

## Reference implementations (local, never committed)

`references/` contains local clones of the open-source reference implementations of the standard (the JS web client and the C++ engine, Apache-2.0). They are **gitignored**: never committed, never built, never linked. Use: verify a behavior in the gray areas of the spec, draw inspiration from the algorithms (traversal, SSE, tile lifecycle).

## Reference documents — READ THEM BEFORE CODING

| Document | Contents |
|---|---|
| `docs/00-vision.md` | Goals, non-goals, legal and naming constraints |
| `docs/01-architecture.md` | Workspace, crates, foundational traits, dependency rules |
| `docs/02-3d-tiles-primer.md` | The 3D Tiles standard: what to implement and how |
| `docs/03-roadmap.md` | Milestones M1→M4, implementation order, acceptance criteria |
| `docs/10-crate-core.md` | Detailed spec for `tuile-core` |
| `docs/11-crate-wgpu.md` | Detailed spec for `tuile-wgpu` + viewer example |
| `docs/12-crate-server.md` | Detailed spec for `tuile-server` (axum + Workers, pluggable cache) |
| `docs/13-crate-usd.md` | Detailed spec for `tuile-usd` (USDZ export, track animations) |
| `docs/14-crate-hydra.md` | Detailed spec for `tuile-hydra` (C ABI + OpenUSD/Hydra plugin, camera resolution, Dockerfiles) |
| `docs/20-conventions.md` | Style, tests, CI, licenses, commits |

## Non-negotiable rules

1. **`tuile-core` NEVER depends on wgpu, tokio, axum, or any backend.** All I/O and all rendering go through the traits (`TileFetcher`, `PrepareRenderResources`). If you are tempted to add a heavy dependency to the core, it means the abstraction is misplaced — stop and fix the abstraction.
2. **The core compiles to `wasm32-unknown-unknown`.** Verify on every PR (`cargo check --target wasm32-unknown-unknown -p tuile-core`).
3. **No third-party brand** in crate names, modules, public types, or public documentation. The project defines itself by the OGC 3D Tiles standard, not relative to other products. Exception: connector crates name their service (nominative use, e.g. `tuile-cesium-ion`) — and only them.
4. **No porting of third-party code.** Algorithmic inspiration is free and creates no obligation. Literal porting is the absolute exception: Apache-2.0 attribution (comment at the head of the item + entry in `NOTICE`), invisible from the public API.
5. **Project license: dual MIT OR Apache-2.0.** Copyright "lapoule.dev". SPDX headers in every file.
6. **`tuile-server` does not know about USD.** No dependency on `tuile-usd`, no USD route. USDZ export is a standalone lib + CLI that consumes tilesets like any other client.
7. **All geospatial precision in f64** until rebasing. f32 only after the switch to a local frame. Never raw ECEF coordinates in f32 — that is the source of jitter.
8. **On the wasm side, rely as much as possible on stable, widely adopted crates** (wasm-bindgen, web-sys, js-sys, gloo, getrandom/js). No experimental, exotic pre-1.0, or poorly maintained crate in the wasm path without a written justification in the PR.

## Expected workflow

- Implement milestone by milestone in the order of `docs/03-roadmap.md`. Do not start M2 until the M1 acceptance criteria pass.
- Every public trait has tests. Every parser has tests on real fixtures (`fixtures/` — public example tilesets, see the primer).
- In case of spec ambiguity, the reference is the OGC 3D Tiles 1.1 spec (https://docs.ogc.org/cs/22-025r4/22-025r4.html), and the behavior of the reference implementations (local clones in `references/`) is authoritative for the gray areas.
- When you need the documentation of a dependency (wgpu, axum, workers-rs, gltf), check the version in `Cargo.toml` and consult the documentation of THAT version — these APIs move fast.

## Useful commands

```bash
cargo check --workspace                                    # quick build
cargo check --target wasm32-unknown-unknown -p tuile-core # wasm guardrail
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo run -p wgpu-viewer -- fixtures/tileset-simple/tileset.json
```
