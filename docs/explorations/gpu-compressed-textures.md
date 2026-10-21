<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# GPU-compressed imagery in the viewer

Status: design, 21 Oct. 2026, revised the same day with Laurent's answers
(§1) and the first measurements (§9.5). The GPU encoder is built, in the
`texcomp` repository (§4); nothing is wired into the viewer yet. This note is
the first PR of roadmap item A6. The PRs that follow it are listed in §10.
Scope: the interactive viewer, native and web (`tuile-wgpu`, `tuile-viewer`,
`tuile-web`, the decode jobs in `tuile-planetary`). The film renderer
(`tuile-film*`) is out of scope, and its goldens must stay byte-identical (§8.4).

Numbers marked *(to measure)* are engineering estimates. They are not
measurements, and the measurement plan in §9 exists to replace them.

## 1. Decisions this note builds on (Laurent, 21 Oct.)

- **Encoded on the fly, client side.** Tiles arrive as they do today (JPEG/PNG
  from the sources). They are decoded, then encoded to the GPU's native block
  format just before upload. Nothing is stored compressed, and the stocks and
  stl-tiles don't change. No derived imagery layer is stored anywhere, which
  keeps Bing imagery within its terms.
- **Two encoders, both in one crate of their own,
  [`lapoule-dev/texcomp`](https://github.com/lapoule-dev/texcomp)**, which
  tuile depends on by git tag (tags cut by the main session):
  1. a **WGSL compute encoder**, behind texcomp's `gpu` feature (wgpu 29, the
     same major as tuile). It is the fast path wherever the adapter has compute
     and a block format the kernels write;
  2. a **pure-Rust CPU encoder ported from Basis Universal**, texcomp's default
     build, dependency-light and `wasm32-unknown-unknown` clean. It runs in the
     decode workers and covers the rest (WebGL2, adapters without compute,
     formats the kernels don't write).
- **Selection comes strictly from what wgpu reports** (features, downlevel
  capabilities, limits): the GPU encoder, else the CPU encoder, else
  uncompressed. There is no flag and no switch. Environment variables may tune
  quality and effort, and never choose a path.
- **Target platforms:** Metal (macOS and iOS), Meta Quest (Android, Vulkan,
  Adreno), WebGPU, Vulkan. The Quest and iOS expose ASTC and ETC2 but no BC,
  so ASTC is not left for last (§4.3).
- **÷4 only:** BC7 and ASTC 4×4, 8 bits per texel. No 4-bit tier (BC1, ETC2
  RGB8) for now.
- **UASTC only** in the CPU port; no ETC1S.
- **ΔE2000 from `palette`**, as a dev-dependency.
- **Fixtures:** NASA imagery (public domain) and synthetic patterns. Never Bing.

### Where each part lives

| Part | Where |
|---|---|
| `BlockFormat`, block sizes | `texcomp` (default build) |
| WGSL kernels (BC7, ASTC 4×4; ETC2 later), pipelines, buffers, the buffer→texture copy | `texcomp::gpu` (feature `gpu`) |
| What a device needs to run the kernels (`texcomp::gpu::runs_on`) | `texcomp::gpu` |
| Quality instruments (PSNR, ΔE2000), NASA and synthetic fixtures, encoder tests, the pass-leak measurement | `texcomp` tests and examples |
| CPU encoder and transcoders (the port) | `texcomp` (default build) |
| `select`: the plan for a device, from its capabilities | `tuile-wgpu` |
| Integration: the decode job's target, `Texels::Blocks`, the wire format, `upload_texture`'s branch, the device's requested features | `tuile-planetary`, `tuile-core`, `tuile-web`, `tuile-wgpu`, `tuile-viewer` |
| The budget's real bytes (cost inputs) | `tuile-core` (`content.rs`) |
| Headless and web-bench measurements, the PNG decode bench | tuile |

## 2. Where the viewer stands today

| Step | Today | File |
|---|---|---|
| Decode | `image` 0.25 (jpeg, png) → `to_rgba8` | `tuile-core/src/raster.rs:1610` `decode_image` |
| Reproject | Web Mercator → geographic spacing, CPU | `raster.rs:1635` `decode_and_reproject` |
| Job | `ImageryDecode` (`Portable`, kind `"imagery"`): decode + reproject + opaque-black check; the output is the raw RGBA plus w, h and flags | `tuile-planetary/src/jobs.rs:46,68,130` |
| Workers | native `ThreadPool` (`tuile-decode-N`); web `WebDecode` workers (`runDecodeWorker`), with an inline fallback | `tuile-core/src/offload.rs:280`, `tuile-viewer/src/web/decode.rs:63,149` |
| Web wire | the geometry worker posts raw RGBA8 to the page | `tuile-web/src/wire.rs:603,650,877,920` |
| Mips | CPU, `raster::mip_chain` (Triangle filter), on the uploading thread. GPU blit chains were rejected because of the Metal render-pass leak (gfx-rs/wgpu#8768) | `tuile-wgpu/src/prepare.rs:554-584` |
| Upload | one `Rgba8UnormSrgb` texture per imagery tile, one `write_texture` per level, shared by `(source, coord)` | `prepare.rs:564`, `context.rs:36,88,142` |
| Device | the only features requested are the timestamp ones. **Nothing detects BC, ETC2 or ASTC**, and the viewer has no compute pipeline | `tuile-viewer/src/app/setup.rs:133-191` |
| Cost | `budget::cost::drape_cost(w, h, TextureFormat)` already knows `Rgba8`, `Block16` and `Block8`. Every caller hard-codes `Rgba8` | `tuile-core/src/budget/cost.rs:65-103`, `content.rs:161-253` |
| Ack | the pump acknowledges a drape with `drape.cost().gpu`, the core's estimate. The measured `GpuImagery.bytes` appears only in `ImageryTextures::live()` | `pump.rs:537`, `context.rs:97-112` |

Imagery tiles are 256² almost everywhere; Bing uses the provider's width. The
ground shader uses alpha (`raster.wgsl:45`, `mix(.., texel.rgb, mask * texel.a)`),
so a layer with partial coverage needs an alpha-capable format.

Film does not use `tuile-wgpu`. It shares only core's decode
(`decode_and_reproject`) and `DecodedTexture`, and it builds its own mips by
compute in `tuile-film-gpu`.

## 3. Formats per backend and what wgpu 29 exposes

wgpu exposes three relevant features: `TEXTURE_COMPRESSION_BC`, `_ETC2` and
`_ASTC`, plus their HDR/3D variants, which are not needed here. Read in
`wgpu-hal` 29.0.4:

| Platform / backend | BC (BC1–7) | ETC2 | ASTC LDR | Compute | Note |
|---|---|---|---|---|---|
| macOS, Metal (Apple silicon) | yes | yes (Apple7+) | yes | yes | `metal/adapter.rs:720-725`: `format_bc = (os == macOS)` |
| iOS / iPadOS / visionOS, Metal | **no** | yes | yes | yes | wgpu reports no BC off macOS, whatever the hardware does |
| Windows DX12 / Vulkan desktop | yes | rarely | rarely | yes | NVIDIA/AMD/Intel desktop: BC only |
| Android Vulkan | rarely | yes | mostly yes (Mali, Adreno, PowerVR) | yes | |
| Linux GL fallback (`TUILE_BACKEND`) | if s3tc + rgtc + bptc | if ES3 compat | if KHR_astc_ldr | GL 4.3 / ES 3.1 | `gles/adapter.rs:497-575` |
| Browser WebGPU, desktop | `texture-compression-bc` | on Apple, `-etc2` too | on Apple, `-astc` too | yes | |
| Browser WebGPU, Android | rarely | `-etc2` | `-astc` | yes | |
| Browser WebGL2 (wgpu GL) | only if **all four** of `WEBGL_compressed_texture_s3tc`, `_s3tc_srgb`, `EXT_texture_compression_rgtc` and `_bptc` are present | `WEBGL_compressed_texture_etc` | `WEBGL_compressed_texture_astc` | **none** | the CPU encoder is the only path |

Formats the viewer uses, all sRGB so that sampling stays exactly what
`Rgba8UnormSrgb` does today:

| Format | wgpu name | bpp | vs RGBA8 | Alpha | Use |
|---|---|---|---|---|---|
| BC7 | `Bc7RgbaUnormSrgb` | 8 | 4× | full | default where BC exists |
| ASTC 4×4 | `Astc { B4x4, UnormSrgb }` | 8 | 4× | full | default where ASTC exists and BC does not (iOS, Quest, Android, mobile WebGPU) |
| ETC2 RGBA8 (EAC alpha) | `Etc2Rgba8UnormSrgb` | 8 | 4× | full | later, for adapters with ETC2 alone, which none of the targets is |

Order of preference for one adapter: **BC7, then ASTC 4×4**, then
uncompressed. The 4-bit formats (BC1, ETC2 RGB8, 8×) are out for now (§1).

Constraints these formats impose, which the plan honours:

- A block-compressed texture can be neither a render target nor a storage
  texture. The GPU encoder writes blocks into a **storage buffer**, then
  `copy_buffer_to_texture` fills each level. `bytes_per_row` is `blocks_x × 16`,
  which is 1,024 bytes at 256² and is padded to 256 for the small levels.
- Level 0 must be a multiple of 4. Levels below 4×4 are stored as one padded
  block, which wgpu handles through the level's physical size. Tile widths that
  aren't multiples of 4 (none today) stay uncompressed.
- The device must **request** the chosen feature. `bring_up` (`setup.rs:175`)
  gains the selected compression feature, intersected with the adapter's
  features as the timestamp features already are.

## 4. The GPU encoder (WGSL, `texcomp::gpu`)

### 4.1 Shape (as built)

- **Input:** the RGBA8 levels, mips included, written with `queue.write_buffer`
  into a storage buffer for the batch.
- **Kernel:** one invocation per 4×4 block, in 8×8 workgroups (64 invocations,
  within every downlevel limit that has compute). It writes `vec4<u32>` per
  block into a second storage buffer, rows padded to 256 bytes.
- **Copy:** `copy_buffer_to_texture` into the final sRGB texture, one copy per
  level at its physical (block-rounded) size, in the caller's command buffer.
- **Batching:** `GpuEncoder::encode` takes every tile of a call and records
  **one compute pass**: one dispatch per (tile, level), each with its job
  uniform at a dynamic offset. Tiles beyond one storage binding go into a
  second batch in the same pass.
- **Mips:** the CPU `mip_chain` stays as it is, and the kernels run on each
  level. Building mips by compute is a later optimisation.
- **Failure:** a tile that can't be encoded (not a multiple of 4, not a mip
  chain, larger than a binding) gets an `EncodeError` and no texture. The
  caller uploads it uncompressed (§8.1).

### 4.2 BC7: modes 6 and 1

Imagery is mostly opaque and photographic: smooth gradients, with some sharp
edges (roads, coastlines, field boundaries).

| Mode | Subsets | Endpoints | Indices | Use |
|---|---|---|---|---|
| 6 | 1 | RGBA 7.7.7.7 + p-bit | 4-bit | every block first, and every block with alpha |
| 1 | 2 (64 partitions) | RGB 6.6.6 + shared p-bit | 3-bit | opaque blocks mode 6 fits badly |

- **Fit:** principal axis by power iteration, endpoints at the extreme
  projections, then quantise, assign indices, and refine by least squares,
  three rounds, keeping the best.
- **Mode 1:** the partition is chosen by an estimate, the variance each
  subset leaves off its principal axis plus the quantisation noise of 8 steps.
  Both subsets' moments come from one pass (the second is the total minus the
  first). The chosen partition is then fitted fully.
- **Early out:** a block that mode 6 fits within an RMS of about one code per
  channel skips the partition search.
- **Opaque alpha stays 255:** mode 6's p-bit is shared by all four channels,
  so an opaque block weighs alpha heavily when choosing it. Without that,
  opaque tiles came back at alpha 254 in places.
- **Mode 5**, planned in the first version of this note, was not needed: the
  alpha-edge tile decodes at 62 dB alpha PSNR with mode 6 alone.

The WGSL is our own, written from the Khronos Data Format Specification 1.3
(tables 111–119). Nothing is transliterated from another encoder.

### 4.3 ASTC 4×4: in the GPU encoder from the start

The Quest and iOS are priority targets and expose ASTC, not BC. Two ways led
to ASTC there: the CPU path (UASTC → ASTC is lossless) or a GPU kernel.
**The GPU kernel is in the first texcomp PR**, for three reasons:

1. By the selection rule, a Quest or an iPhone (compute and ASTC) ends up on
   the GPU encoder anyway. The CPU path would only be a stopgap there.
2. The CPU path needs the UASTC encoder (series 3c), the largest part of the
   port, plus the integration. The GPU kernel needed one more WGSL file.
3. It costs less per tile: 1.7–2 ms against 5 ms for BC7 on the M2, wall
   clock (§9.5).

Two fixed configurations, both one partition with a 4×4 weight grid:

| Block | CEM | Weight range | Weight bits | Endpoints | Block mode |
|---|---|---|---|---|---|
| opaque | 8 (RGB direct) | 0..11 (trit + 2 bits) | 58 | 0..255, plain bytes, 48 bits | `0x251` |
| with alpha | 12 (RGBA direct) | 0..5 (trit + 1 bit) | 42 | 0..255, plain bytes, 64 bits | `0x043` |

- The endpoints are ordered so the decoder never applies blue contraction.
- The trit packing table and the weight unquantisation are generated from the
  spec's own decoding procedure (§23.12, §23.17) and checked against its
  published values.
- Its weakness is a single partition: bright points on black (night lights)
  are its worst tile, at 36.4 dB (§9.5). A two-partition mode is the obvious
  next step if that matters on screen.

**ETC2** stays for later (PR 5). None of the four targets needs it: every one
that has ETC2 also has ASTC or BC.

### 4.4 Capability detection (in tuile)

`tuile_wgpu::texcomp::select(report, cpu_formats)` returns one of:

- `Gpu(BlockFormat)`: `texcomp::gpu::runs_on(downlevel, limits)` holds
  (compute shaders, two storage buffers per compute stage, an 8×8 workgroup,
  1,024 workgroups per dimension, a 16 MiB storage binding), and the adapter
  samples one of `texcomp::gpu::GPU_FORMATS` filtered behind its feature;
- `Cpu(BlockFormat)`: the adapter samples a format the CPU encoder writes;
- `Uncompressed`.

`select` is a pure function of what the adapter reported. The capability
matrix is a unit test over synthetic reports, and names the four target
platforms explicitly:

| Report | Plan |
|---|---|
| Metal, macOS (BC, ASTC, ETC2) | `Gpu(Bc7)` |
| Metal, iOS (ASTC, ETC2, no BC) | `Gpu(Astc4x4)` |
| Quest: Vulkan on Adreno (ASTC, ETC2, no BC) | `Gpu(Astc4x4)` |
| Vulkan desktop (BC) | `Gpu(Bc7)` |
| WebGPU desktop (`texture-compression-bc`) | `Gpu(Bc7)` |
| WebGPU mobile / Quest browser (`-astc`, `-etc2`) | `Gpu(Astc4x4)` |
| WebGL2 with the four BC extensions | `Cpu(Bc7)` once the CPU encoder lands, `Uncompressed` before |
| WebGL2 on mobile (ASTC) | `Cpu(Astc4x4)` once the CPU encoder lands |
| No block format | `Uncompressed` |

A live test asks every adapter the machine enumerates for its plan and builds
a device with the plan's features. The M2 answers `Gpu(Bc7)` on Metal.

## 5. The CPU encoder: a Rust port of Basis Universal

### 5.1 Repository, name and licence

- **Repository:** [`lapoule-dev/texcomp`](https://github.com/lapoule-dev/texcomp),
  private, created 21 Oct. Apache-2.0 doesn't require publishing a port, and
  nothing in the licence analysis argues for public.
- **Crate:** `texcomp`. Its public surface is our own types (`BlockFormat`,
  and in `gpu` `GpuEncoder`, `Level`, `EncodeError`). The CPU side adds its
  own (`encode(..) -> Blocks`) with the port.
- **Features:** the default build is the CPU side, `std` only. `gpu` adds
  wgpu 29 (no default features: the application picks its backends) and
  bytemuck.
- **Licensing:**
  - our code: dual MIT OR Apache-2.0, with SPDX headers;
  - every ported item: an Apache-2.0 attribution comment at its head, and a
    prominent "modified" notice as Apache-2.0 §4(b) requires;
  - `NOTICE` names upstream Basis Universal **v2.50**, tag `v2_50`, commit
    `9bebe16726b3a61c8c213eeee3b7cffb462ef34e`, Apache-2.0, © Binomial LLC;
  - the upstream `LICENSE` is kept as `LICENSE-APACHE-UPSTREAM`.
- **Naming:** the README names Basis Universal nominatively, as the project it
  ports. The crate name and every type stay generic. ("Basis Universal" is a
  Binomial trademark; nominative use only.)
- **CI:** fmt, clippy `-D warnings` with and without `gpu`, tests on Linux and
  macOS with and without `gpu`, `cargo check --target wasm32-unknown-unknown`
  with and without `gpu`, docs. Pure Rust, no `build.rs`, no C toolchain,
  `unsafe` forbidden. The CPU side depends on `thiserror` alone; `rayon` stays
  out because the callers own the threads.
- **tuile's side:** tuile depends on it by git tag, and the main session tags
  its releases. `tuile-planetary` (the decode job) uses the CPU side,
  `tuile-wgpu` the `gpu` side. `tuile-core` gets the format enum and the texel
  container (§6), never an encoder.

### 5.2 What gets ported

The input arrives as RGBA, gets encoded, and goes into a GPU texture. It never
touches a disk or a network. Supercompression (ETC1S codebooks, RDO, Zstd,
`.basis`/`.ktx2` containers) exists to make files small, which is a cost we
don't pay. So the port takes the **block layer** of Basis and leaves the
container layer:

| Part | Upstream (v2.50) | Ported | Why |
|---|---|---|---|
| UASTC LDR 4×4 encoder | `encoder/basisu_uastc_enc.cpp` (≈150 KB) + the ASTC/BC7 helpers it uses | **yes**, effort levels 0–2 first, higher levels later | the high-quality intermediate, about 8 bpp |
| UASTC → ASTC 4×4 | `transcoder/basisu_transcoder.cpp` (UASTC part), `basisu_astc_helpers.h` | **yes** | lossless by construction (UASTC is an ASTC subset) |
| UASTC → BC7 | same | **yes** | upstream states 0.75–1.5 dB below ASTC |
| UASTC → ETC2 RGBA + EAC | same, with the ETC1 hint bits | **later** | no target needs it (§4.3) |
| UASTC → BC1 | same, with the BC1 hint bits | **no** | ÷4 only (§1) |
| UASTC unpack (reference decode) | transcoder | **yes**, in tests and as the quality instrument | |
| ETC1S encoder (frontend, backend, codebooks) | `basisu_frontend.cpp`, `basisu_backend.cpp`, `basisu_etc.cpp` | **no** (decided 21 Oct.) | heavy clustering encoder, lower quality; its only benefit is a small file |
| RDO, Zstd, KTX2/.basis/.DDS, XUASTC, XUBC7, HDR, PVRTC, ATC, FXT1, OpenCL, SSE/SPMD kernels, resamplers, image loaders | | **no** | containers, transmission and formats wgpu doesn't expose |

Sizing *(to measure)*: about 6–9k lines of Rust plus generated tables.
Upstream's per-format transcoder tables for UASTC are small; the large `.inc`
tables in `transcoder/` serve ETC1S and aren't needed.

### 5.3 Correctness against upstream

- **Reference vectors** are captured once from upstream `basisu` v2.50 on a
  remote builder (never in CI, never on the laptop's critical path). For each
  fixture tile and each effort level:
  - the raw UASTC blocks (`-uastc -uastc_level N`, no RDO, no Zstd);
  - each transcode to ASTC 4×4 and BC7 (ETC2 RGBA when it is ported).

  They are committed as fixtures with the exact command lines and the commit
  hash.
- **Transcoders:** integer code, so the tests require **bit-exact** output.
- **UASTC encoder:** bit-exact where upstream is deterministic, which is
  single-threaded and integer through the levels we port. Any block that
  differs must still meet the block's PSNR as upstream encodes it, within
  0.1 dB, and the test reports the count of differing blocks. If a level turns
  out to rely on float ordering that can't be reproduced, the test for that
  level drops to the quality bound and the note records why.
- **wasm32:** `cargo check` in CI, plus the reference tests run under
  `wasm32-wasip1` if a runner is cheap to add.

### 5.4 Speed budget

- A 256² tile with its mips is about 87k texels, or 5,461 blocks.
- UASTC level 0–1 runs at roughly 3–10 Mtexel/s per native thread *(to
  measure)*, which is 10–30 ms per tile.
- wasm runs 1.5–3× slower.
- The transcode is a few ms.

That fits a decode worker, where today's JPEG decode plus reprojection already
takes several ms. A slow encode only delays a tile, which the ancestor climb
already covers (§8.1).

## 6. Integration: decode workers, wire, upload

### 6.1 The target format is the host's

- The device decides the plan (§4.4) once, at `bring_up`.
- **Native:** the plan goes to the runtime, through the viewer's profile/config,
  before the first load.
- **Web:** the page owns the device, and the geometry worker and decode
  workers don't. The page sends the plan to the geometry worker in the same
  host message family as `Retarget` (D-A6). The geometry worker passes it with
  each `ImageryDecode` job.
- A plan change (device lost, re-created on another adapter) sends the message
  again. Tiles already resident keep the format they were made with.

### 6.2 Core types (texels, not encoders)

`DecodedTexture { width, height, rgba8 }` becomes
`DecodedTexture { width, height, texels: Texels }` with:

- `Texels::Rgba8(Vec<u8>)`: today's form. The GPU path and the uncompressed
  path keep it.
- `Texels::Blocks { format: TextureFormat, levels: Vec<Vec<u8>> }`: the CPU
  path's output, with the mips already encoded.

`TextureFormat` is the one the budget already has (`budget/cost.rs:65`), grown
to name the concrete formats (BC7, ASTC 4×4) while keeping
its 16-byte/8-byte block arithmetic. Core gains no dependency. Film reads
`Texels::Rgba8` only and is handed exactly the bytes it has today (§8.4).

### 6.3 The decode job (`tuile-planetary/src/jobs.rs`)

- `ImageryDecode` gains `target: Option<BlockFormat>`. `None` gives today's
  output, byte for byte.
- With `Some(f)`, the job decodes, reprojects and checks for opaque black
  (unchanged). It then builds the mip chain in the worker, moving it off the
  uploading thread, and encodes each level to UASTC and transcodes it to `f`.
- The output is serialised as a new tagged layout: the format byte, then
  w, h, flags and the level lengths.
- **If the encode fails or overruns** its per-tile time cap (an env-tunable
  value, not a switch), the job returns `Texels::Rgba8`. The tile is then drawn
  uncompressed, and it is never missing (§8.1).
- Native pool and web workers run the same `jobs::serve`, so one
  implementation covers both. The web inline fallback (`decode.rs:68`) runs it
  too.

### 6.4 Wire and upload

- `tuile-web/src/wire.rs` `texture()`/reader gain the format tag and the levels.
  The page receives blocks, 4× smaller to post across the worker boundary.
- `prepare.rs` `upload_texture` branches on `Texels`:
  - `Blocks` creates a texture of that format and makes one `write_texture`
    per level, with block-row strides;
  - `Rgba8` under a `Gpu(f)` plan writes to the encoder's scratch, queues the
    encode into the frame's compute pass and copies into an `f` texture;
  - `Rgba8` under `Uncompressed` takes today's path.

  The bind group layout doesn't change: a `texture_2d<f32>` filterable binding
  accepts any filterable format, so `PreparedTile::dress` and the shader are
  untouched.
- glTF base-colour textures take the same path once it exists. That is a
  follow-up, not this item.

## 7. Budget: real bytes

The budget code itself doesn't change. Its **cost inputs** do:

- `DrapeContent::cost` (`content.rs:223`) and `DecodedTileContent::cost`
  (`content.rs:161`) stop hard-coding `Rgba8`.
- **CPU path:** the format comes from `Texels::Blocks`. Then
  `gpu = drape_cost(w, h, Block16).gpu` and the staging is the blocks'
  staging. Both drop 4×.
- **GPU path:** the texels are RGBA, but the resident texture is `f`. The
  runtime holds the host's plan (§6.1) and passes it to `cost`. Then
  `gpu = drape_cost(.., f).gpu` (resident) and
  `staging = drape_cost(.., Rgba8).staging` (what crosses the bus this frame).
  `Cost { gpu, staging }` already separates the two, which is what makes this
  work.
- **Drape billing (49fc6e4):** the drape's charged share (`ppm` of the whole)
  multiplies the true whole, which is now the compressed one. The rule doesn't
  change.
- **Drift check:** a test charges a drape, uploads it on a headless device and
  compares the cost to the measured `GpuImagery.bytes`, within the 5 %
  `within_drift`. This makes estimate and reality agree by test rather than by
  hope. The pump's ack (`pump.rs:537`) keeps using `cost().gpu`, which becomes
  right.
- **The GPU encoder's scratch** (the RGBA scratch plus the block buffer for one
  frame's batch) is a fixed transient allocation. It is sized from the pump's
  per-frame upload pacing and is reported as a fixed overhead, not per tile.
- The **budget agents** (A1, A3) work in `tuile-core/src/budget`. Nothing here
  edits that directory except, if needed, extending `TextureFormat`'s variants
  in `cost.rs`. That is coordinated through the main session before it lands.

## 8. Never black ground, and what must not move

### 8.1 Black ground

- **Encode failure** (CPU) gives an RGBA tile, never a missing one (§6.3).
- **GPU path failure:** the encode is wrapped in a validation error scope.
  If it fails, the tile is re-uploaded as RGBA in the next frame, and the
  fallback texture is created before the failed one is dropped.
- **Slow encode:** a tile still in a worker is a tile not yet resident. The
  consumer's climb to the nearest resident ancestor covers it, exactly as it
  covers a slow network fetch today. Nothing here refuses to draw a fallback.
- **Swap order:** unchanged, new before old. A compressed texture is created,
  written, encoded and copied in the same submission that first binds it, and
  the replaced tile is released only after.
- **Test:** `no_black_ground.rs` and `fast_zoom_never_empties.rs` run under
  each plan the test machine can produce (`Gpu(Bc7)`, `Gpu(Astc4x4)`, `Cpu(Bc7)` /
  `Cpu(Astc)` where the adapter has them, and `Uncompressed`). One more case
  injects an encode failure on every second tile.

### 8.2 No seams from compression

Block artefacts at tile borders could read as seams. Imagery edges already
clamp (`ClampToEdge`), and blocks never straddle tiles (sizes are multiples of
4). `no_seams.rs` runs under the compressed plans with its current thresholds.

### 8.3 Colour

Encoders measure error on the sRGB-encoded values, the same space the texels
are stored in, and the hardware decodes to linear exactly as it does for
`Rgba8UnormSrgb`. Radiometric calibration is film-side and untouched.

### 8.4 Film goldens

- Film never calls `upload_texture` and never runs the `ImageryDecode` job with
  a target.
- The `DecodedTexture` field change is a type change, not a byte change. Film's
  readers move from `.rgba8` to `Texels::Rgba8`.
- Every PR runs the film golden tests (`tuile-film-gpu/tests/headless.rs`,
  frozen PNGs) and requires them **byte-identical**.

## 9. Quality gate and measurements

### 9.1 Instruments

In texcomp's tests (`tests/gpu_quality.rs`):

- **PSNR** over RGB, and over alpha;
- **ΔE2000** (CIEDE2000) per texel, sRGB → CIELAB D65 by `palette`
  (dev-dependency), giving the mean, the p99 and the max;
- **holes**: texels the source draws (opaque, not near black) that decode to
  black or transparent. Any hole fails, on every tile and every level;
- a sheet per tile (source | decoded | |difference|×8) in
  `target/test-renders/`, for a human to open.

The blocks are decoded **by the GPU** (`textureLoad` on the linear variant of
the format in a compute pass), so the gate measures what the hardware decodes,
not what a reference decoder of ours believes.

### 9.2 Fixtures

In `texcomp/tests/fixtures/imagery`, with provenance in its README: five NASA
GIBS tiles (Blue Marble NG over the Alps and the Nile, the shaded-relief
bathymetry, a MODIS true-colour day, VIIRS Black Marble over Paris), 512² as
served and cut into four 256² tiles each. Plus four synthetic tiles: a gradient,
hard stripes, an alpha edge and white noise. Bing is never committed.

### 9.3 Gates (measured, Apple M2, 21 Oct.)

Each gate sits about 1 dB or 20 % beyond the worst measured tile:

| | PSNR RGB ≥ | ΔE2000 mean ≤ | ΔE2000 p99 ≤ | alpha PSNR ≥ |
|---|---|---|---|---|
| BC7, NASA tiles, levels ≥ 8×8 | 39.0 dB (worst 40.30) | 2.5 (2.09) | 6.5 (5.35) | |
| BC7, synthetic, level 0 | 45.0 (48.13) | 0.5 (0.36) | 1.5 (1.18) | 55.0 (62.44) |
| ASTC 4×4, NASA tiles, levels ≥ 8×8 | 35.0 (36.41) | 3.0 (2.43) | 11.5 (9.40) | |
| ASTC 4×4, synthetic, level 0 | 48.0 (51.14) | 0.4 (0.26) | 1.5 (1.14) | 50.0 (56.65) |

The first version of this note proposed 38 dB for both formats. BC7 clears it
by 2 dB. ASTC misses it on one tile, night lights (bright points on black),
for the reason in §4.3; its other NASA tiles are 40–60 dB at level 0.

Not gated, only held to the hole check:

- levels below 8×8 (one to four blocks, drawn only where a tile covers a few
  pixels, with statistics over at most 64 texels);
- the synthetic tiles' small levels, which are steep 2-D colour ramps that no
  one-line-per-block format fits and no imagery has;
- the noise tile, incompressible by construction (16.6 dB BC7, 13.9 dB ASTC).

Typical level 0 on the NASA tiles: BC7 46–59 dB, ΔE mean 0.03–0.9; ASTC 41–60
dB, ΔE mean 0.03–1.2. The sheets show no visible difference at 1:1.

**The test fails when the encoder is broken:** with the kernels' `encode()`
call removed (all-zero blocks), it fails with 214 hole reports and every PSNR
below 14 dB. It was run that way before the gate was committed.

### 9.4 Viewer measurements (PR 4)

Each measurement is taken before and after, on the same trajectory:

- headless viewer (`examples/wgpu-viewer/examples/headless`, the `control`
  verdict's `worst_resident_bytes`, `ImageryTextures::live()`), native Metal;
- `scripts/web-bench.py`, under WebGPU and WebGL2.

Recorded per run: GPU imagery bytes resident (live and by level), bytes moved
per frame, upload time per tile, decode-worker time per tile, and frame time
p50/p99.

**Visual check:** the same view, uncompressed and compressed, screenshots side
by side, a ΔE2000 map of the drape region, both opened before the PR asks for
review.

### 9.5 Measured so far, and expected gains

**Encode time** (Apple M2, Metal, release; twenty 256² tiles with their mips in
one call, wall clock from `write_buffer` to the GPU's completion, mean of 5):

| | per tile |
|---|---|
| BC7 | 5.1 ms (11.0 ms before the partition-search optimisations of §4.2) |
| ASTC 4×4 | 1.7–2.0 ms |

This is wall clock and includes the upload and the copy. It is a cost on the
GPU queue, not on the CPU thread. At a pump pacing of a few tiles per frame,
BC7's 5 ms per tile is too much for one frame. PR 4 spreads it (a frame's
batch is bounded by the pump's upload pacing) and measures GPU time with
timestamps.

**Pass leak (gfx-rs/wgpu#8768)**, measured with `examples/pass-footprint`
(process footprint by `footprint -p`, after a warm-up tenth):

| Run | Footprint growth |
|---|---|
| 80,000 clear-only render passes | none (−0.2 KiB per 1,000 passes) |
| 80,000 compute passes | none (−0.2 KiB per 1,000) |
| 30,000 encoder calls (one 256² tile each) | none: ±0.7 MiB of noise, no trend |

The render-pass leak does not show in this harness on this machine (macOS 27,
M2), so the harness doesn't reproduce whatever triggered it (clear-only passes,
nothing sampled). Compute passes and the encoder show no growth either. The
CPU mips stay as they are; nothing here argues for or against moving them.

**PNG decoding** (`examples/decode-bench` in tuile, 25 VIIRS Black Marble
tiles, 512² RGB, 270 KiB average; outputs checked identical):

| | `png` 0.18 via `image` 0.25 | `zune-png` 0.5.2 | ratio |
|---|---|---|---|
| native, M2 | 1.63 ms/tile | 2.11 ms | 0.78× |
| wasm32-wasip1 (Node 26) | 2.17 ms | 2.38 ms | 0.91× |
| wasm32-wasip1 + `simd128` | 2.25 ms | 2.49 ms | 0.91× |

`zune-png` is slower on both. **Dropped**: PNG keeps `image` and `png` 0.18.

**Expected gains** (arithmetic), 256² tile with its full mip chain:

| | RGBA8 | BC7 / ASTC 4×4 |
|---|---|---|
| Resident GPU bytes | 349,524 | 87,408 (4×) |
| Staging per tile, CPU path | 360,192 | 90,368 |
| Staging per tile, GPU path | 360,192 | 360,192 (plus a transient block buffer) |
| Web worker → page post | 262,144 (level 0 only) | 87,408 (all levels) |

At a fixed budget, the imagery share holds 4× as many tiles, which means a
sharper ground at the same memory. Upload bandwidth drops 4× on the CPU path
and stays the same on the GPU path.

## 10. Order of the PRs

1. **This note** (tuile).
2. **texcomp: the GPU module** (the PR formerly planned as tuile's PR 2). The
   BC7 and ASTC 4×4 kernels, their dispatch, the quality gate and fixtures,
   the pass-leak example, and CI. Covers macOS, iOS, the Quest, Vulkan and
   WebGPU.
3. **texcomp: the CPU port**, in several PRs:
   - 3a: reference vectors captured from upstream v2.50, the Basis
     attribution and NOTICE entries, UASTC unpack and UASTC → ASTC;
   - 3b: UASTC → BC7;
   - 3c: the UASTC encoder, levels 0–2.

   The main session tags `v0.1.0` when the GPU module lands. That tag is all
   PR 4 needs for every target except WebGL2. It tags the next version after
   3c.
4. **tuile integration**: `select` and its capability matrix, the device's
   requested features, `Texels`, the job target, the wire, `upload_texture`'s
   branch, the cost inputs, the measurements and the visual check. It needs
   texcomp `v0.1.0` and may split into 4a (native) and 4b (web).
5. **tuile: the PNG decode bench**, which records why `zune-png` was dropped
   (§9.5).
6. **Later:** ETC2 in the kernels, an ASTC two-partition mode if night lights
   need it, and the CPU path into the decode workers for WebGL2 once 3c lands.

## ⚑ Open questions for Laurent

Answered on 21 Oct.: the repository name (`lapoule-dev/texcomp`), ÷4 only,
UASTC only, `palette`, NASA fixtures, the target platforms. Still open:

1. **ASTC on night lights.** The single-partition ASTC kernel gives 36.4 dB on
   the Black Marble tile over Paris, against 42 dB for BC7 (other tiles: 40–60
   dB). Good enough for the Quest and iOS, or should a two-partition mode come
   before PR 4?
2. **BC7 cost per tile.** 5 ms of GPU time per 256² tile (wall clock on an M2)
   is affordable at one or two tiles a frame but not at ten. PR 4 measures it
   with timestamps in the real pump. If it bites, the next lever is a cheaper
   partition estimate or mode 6 only (both cost quality, and both can be
   measured with the gate).
3. **The `v0.1.0` tag** of texcomp after the GPU module lands, so PR 4 can
   start before the CPU port is done.
