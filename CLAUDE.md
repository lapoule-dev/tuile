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
| `docs/15-usd-scene-index.md` | Vision et trajectoire OpenUSD : le cœur scene index Hydra 2.0, la façade hdGp, le jumeau usdrecord du recorder |
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

## Black ground is a bug, always

**No black square, no black region, not for one frame.** A black quad on a globe
is indistinguishable from a rendering fault, and it is what a client sees. Soft,
blurry or stale ground is *always* preferable to absent ground.

Two consequences that have each been violated and had to be undone:

- **Never refuse to draw a fallback in order to avoid another artefact.** Every
  refusal is a hole. Overlap, shimmer and blur are all strictly better than
  black — fix them without ever removing coverage. Switching off the consumer's
  climb to the nearest resident ancestor produced large black rectangles at both
  zoom-in and zoom-out.
- **Swap surfaces in this order: create the new mesh and its texture in GPU
  memory, activate it, and only then deactivate the old one.** There must be no
  instant where neither is active. A replacement may overlap for one frame; it
  may never gap for one frame.

Note also that any instrument counting "did every *selected* tile draw
something" is blind to ground that was never selected. A green counter is not a
covered globe.

## Every render gets opened

**When a render finishes, fetch the video and open it.** Not a frame count, not
a byte size, not a green log line — the film itself, on screen.

A render is a picture, and every instrument that stands in for looking at it has
already lied here. `RENDER-DONE` was printed over four-fifths of a missing film
because `ffmpeg concat` stopped at the first empty segment. A frame counter said
2/2 while the whole globe came out flat pink, textures silently unbound. A
"green" job had never rendered a frame at all: it had crashed in ten seconds and
the script sat in `wait` for the remaining hour.

```bash
# after any run that produced renders/<run>/render.mp4
open <the downloaded mp4>
```

The cost is two seconds. What it buys is the one check no counter can fake.

## Renders live in `./videos`, never in `/tmp`

A film that only exists in `/tmp` is a film one reboot away from gone, and a
two-minute render costs twelve minutes of three L4s. Move it to `videos/` as
soon as it is assembled — the directory is gitignored, because seven hundred
megabytes has no place in the history.

Name it for what it *is*, and add its entry to `videos/README.md` with **what
it would take to make it again**: the trajectory string, the pack key, the
scene digest, the imagery asset, the viewport, the sample count. An mp4 without
its parameters compares to nothing, and comparing renders is most of the work.

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

**Never pipe a test, build or clippy run through `head` or `tail`.** `head` closes
the pipe as soon as it has its lines and kills the run partway through, so the
`test result:` summary never arrives and a truncated run is indistinguishable from
a hang; `tail` shows nothing until the process exits. Redirect the whole run to a
file and grep it afterwards:

```bash
cargo test --workspace > /tmp/tests.log 2>&1
grep -E "(test result|FAILED|panicked)" /tmp/tests.log
```

**A green suite is not proof.** Write the test in the same change as the behaviour,
then revert the behaviour and confirm the test fails. Say plainly what a test does
not cover rather than letting a passing run imply coverage it does not have.
