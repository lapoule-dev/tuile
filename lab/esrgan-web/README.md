<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# esrgan-web — a super-resolution network on WebGPU, from Rust

A demonstrator for the lab's overzoom work (`lab/overzoom`). It runs the compact Real-ESRGAN network ×4 in Chrome on
the GPU through WebGPU. The network is written in Rust with Burn and compiled to wasm. The page takes a real imagery
tile from the lab's dataset and shows four panels side by side:

- the input, at nearest neighbour;
- bicubic ×4;
- the network's output;
- the served z19 ground under that tile, for looking only and never scored against.

It also shows the timings, the WebGPU adapter and its limits, and the difference from the PyTorch reference.

## Run

```bash
# 1. data/: weights, a z17 tile of the lab's zone, its z19 ground, the PyTorch reference (needs lab/overzoom/data)
../overzoom/.venv/bin/python prepare.py --lab ../overzoom
# 2. the wasm
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/esrgan_web.wasm
# 3. serve and open
python3 -m http.server 8891 --bind 127.0.0.1
open -a "Google Chrome" http://127.0.0.1:8891/
```

The wasm-bindgen CLI must match the `wasm-bindgen` crate in `Cargo.lock` (0.2.129). `data/` and `pkg/` are
gitignored: imagery is never committed.

## The network

**What it is.** SRVGGNetCompact, as published with Real-ESRGAN for `realesr-general-x4v3`: about 1.2 M parameters.

**How it computes.**
- 34 convolutions, all 3×3 at the input's resolution: 3 → 64, then 32 × 64 → 64, then 64 → 48;
- per-channel PReLU after every convolution but the last;
- a pixel shuffle by 4;
- the input upsampled by nearest neighbour, added back.

**How it is written.** In `src/lib.rs`, from that description with Burn's functional ops (`conv2d`, `prelu`,
`PixelShuffle`). No code is ported. The nearest-neighbour upsampling is a reshape and expand, so it is exact.

**How the weights get in.**
- `prepare.py` loads the checkpoint with spandrel (the lab's loader).
- It writes its tensors as raw little-endian f32, in forward order.
- The page hands them to `Upscaler::create` as one `Float32Array`.

No ONNX step and no `burn-import`: a fixed-order flat file is all a stack of convolutions needs.

## Weights and licence

| | |
|---|---|
| file | `realesr-general-x4v3.pth`, from the Real-ESRGAN v0.2.5.0 release on GitHub |
| sha256 | `8dc7edb9ac80ccdc30c3a5dca6616509367f05fbc184ad95b731f05bece96292` (checked by `prepare.py`) |
| licence | BSD-3-Clause, the licence of the Real-ESRGAN repository (Xintao Wang, 2021); the release ships no separate licence for the weights |
| training data | not restated here; check it before a deployed derived product |

The weights are downloaded by `prepare.py` and never committed.

## Measured (Chrome, Apple GPU, `metal-3` adapter; one 256² tile → 1024²)

| | |
|---|---|
| wasm load | 150–230 ms (21.7 MB, unoptimised `.wasm`) |
| device + weights | 170–220 ms |
| first run | 1.6–2.1 s (shader compilation included; lower once Chrome has cached them) |
| later runs | 1.51–1.58 s (median of 5, readback included) |
| PyTorch reference, f32 on the laptop's CPU | 0.75 s |
| vs PyTorch, raw f32 output | max \|Δ\| 4.8e-6, mean \|Δ\| 2.1e-7 |
| vs PyTorch, as 8-bit images | PSNR 90.4 dB (a handful of rounding flips) |

The computation runs in f32 on WebGPU. The adapter has `shader-f16`, but Burn's default float stays f32 and nothing
here asks for f16. That is why the output matches the reference to f32 rounding.

**Speed is not tuned.** 1.5 s for about 160 GFLOP is roughly 0.1 TFLOP/s, far below what the GPU can do. Enabling Burn's
`autotune` feature changed nothing (1.51 s) and added 8 MB of wasm, so it is off. `fusion` is off as well. Not tried:
- f16;
- a tiled or batched input;
- profiling where the time goes (the convolution kernel choice, the per-op dispatch, the readback).

## Why Burn, pre-1.0 (tuile's rule 8)

Rule 8 asks for stable, widely adopted crates in the wasm path, or a written reason. Burn is pre-1.0. It is used
here because:

- **It is the subject of the experiment.** The question is whether a Rust tensor framework can carry a model to
  WebGPU from the same code that runs natively. Burn is the established Rust option with a WebGPU backend
  (wgpu/CubeCL), and it is actively maintained.
- **It is confined to the lab.** The crate lives in `lab/`, outside the tuile workspace, with its own `[workspace]`.
  No tuile crate depends on it.
- **It is pinned exactly** (`=0.22.0`). Its API moves between minor versions: 0.22 replaced backend-generic tensors
  with `Tensor<D>` on a runtime `Device`, and setup is now `Device::wgpu_options()`. Upgrades are deliberate.

Before anything here moves into a tuile crate, the network should be reconsidered as a few hand-written WGSL compute
shaders on tuile's own wgpu device. It is 34 identical convolutions.

## Not verified

- **Any other browser or GPU.** Only Chrome on this Mac's Apple GPU has run it. Safari, Firefox and discrete GPUs are
  untested.
- **The page's limits.** It reports the adapter's limits, not the device's. The adapter is requested separately from
  Burn's, so it is the same browser adapter, but not necessarily the same object.
- **Larger inputs.** Tiles larger than 256² have not been tried, so neither have buffer-size limits.
- **A native test.** There is no native run of the Burn model. The only check is the page's comparison against the
  PyTorch output.
