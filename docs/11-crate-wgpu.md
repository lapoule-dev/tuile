# 11 — `tuile-wgpu` spec + the viewer

Reference rendering backend. Goal: correct and readable before fast. It is the showcase for the core, not a game engine.

Status as of v0.14.0 (wgpu 30): everything below is **shipped** unless marked otherwise. Backends in use: Metal (macOS, iOS), Vulkan (Linux, Android; lavapipe in CI), OpenGL ES (Android fallback), WebGPU and WebGL2 (browsers).

## `tuile-wgpu`

### Responsibilities
1. **Prepare** (`prepare`, `prepare_drape` → `PreparedTile`): decoded content → GPU resources (interleaved buffers, mipmapped sRGB textures). A tile's mesh and each of its drapes are separate resources; a mesh is *dressed* at draw time with the drapes the budget's draw set says it wears — its own, or an ancestor's through a UV transform — which binds textures and uploads nothing.
2. **Render** (`TileRenderer`): one ground pipeline (position, normal, UV; `view_proj` relative-to-center; base colour × imagery, simple lighting, aerial perspective from `tuile-atmosphere`), a wireframe pipeline where the device supports it, debug lines for bounding volumes. The ground's ownership mark — the bit that keeps a fallback ancestor from taking a pixel from the surface that owns it — is a stencil.
3. **The budget's executor** (`ContentPump`): drains the `ServerMessage`s of any `GeometryStream`, uploads what was delivered in the plan's order within the frame's deadline, acknowledges each upload with its actual bytes (`Ack`) or refuses one that would cross its class's hard limit (`Discarded`), draws the draw set, and frees what is evicted after the draw that stops using it — new before old, never a frame with a hole. The pump does not know which binding feeds it.
4. **A frame the host owns** (`HostPass`, `TargetLayout`, `TileRenderer::for_layout`): an engine that embeds tuile in its own render graph hands over its device (`GpuContext::new`), its colour target, its depth buffer and the frame's encoder. Any sample count, forward or reverse-Z depth, any colour format; nothing cleared unless asked, nothing submitted or presented. A host whose depth carries a stencil lends it; one whose depth has none (`Depth32Float`) gets a companion depth-stencil, its depth copied in before the ground is drawn and the ground's depth copied back where it is nearer — the path follows from the depth format, not from a setting. Such a depth must be created with `TEXTURE_BINDING`; on WebGL2 it must be single-sampled. Our own frame (`draw_to_surface`, `draw_to_texture`) goes through the same pass.
5. **Block-compressed imagery** (`compress`) — the consumer's choice, off by default. A consumer that opts in resolves its `ImageryCompression` against its adapter (`texture_caps`), creates its device with the features the plan needs (`plan_features`) and hands the plan to its context (`GpuContext::with_imagery_plan`). RGBA8 marked for a block format is encoded on the GPU at upload by texcomp's compute kernels, mip chain included; imagery stored as KTX2 arrives already in blocks and is uploaded as is. Any encode that cannot be done uploads RGBA8 instead, with the same swap order. Encodes are paced by the pump under their own per-frame share of GPU time (`EncodePace`, timestamps where the device has them). *Planned:* moving encode time and transient memory into the core budget's frame deadline (the compression-budget plan, B1–B8), which removes `EncodePace`.
6. **Overlays**: `OverlayRenderer` (pixel-space triangles for the controls a host builds with `tuile-ui`) and world overlays (`tuile-overlay` geometry drawn in the world, meeting the ground).
7. **WebGL2 held by test** (`webgl2`): every shader of the interactive path is validated with no optional capability and written out as GLSL ES 3.00, and the ground shader is held to WebGL2's sixteen texture units.
8. **Headless**: `GpuContext::headless`, `draw_to_texture`, `Readback` — the GPU tests and the offscreen snapshot run through them.

### Features
None. Native and wasm builds use the same public API; transports live in `tuile-native-fetchers` and `tuile-web`, not here.

### Constraints
- Uploads via `Queue::write_buffer` / `write_texture`.
- Mipmaps are generated for every texture — without them the rendering shimmers and makes the project look broken.
- Formats: `Rgba8UnormSrgb` by default; `Bc7RgbaUnormSrgb` / `Astc4x4RgbaUnormSrgb` when the consumer chose compression and the device samples it.
- No reference to winit in the lib. The crate never creates a window or a surface.
- The ground is never black: a replacement is created and activated before what it replaces is released.

### Offscreen snapshot of a 3D Tiles tileset

```text
cargo run -p tuile-wgpu --example snapshot -- <tileset.json> <out.png> [size]
```

Loads a tileset, runs the geometry server in-process until the tiles settle, renders one frame and writes a PNG. It is the shipped way to *see* a 3D Tiles tileset; the interactive viewer below draws the terrain-and-imagery globe and does not load a `tileset.json`.

## The viewer: `tuile-viewer` + `examples/wgpu-viewer`

The interactive viewer is a library, `tuile-viewer` (window, camera, controls, steering, scripting, the budget's profiles per device), generic over where terrain and imagery come from (`ViewerHost`, see `16-embedding-the-viewer.md`). `examples/wgpu-viewer` is the binary `tuile-wgpu-viewer`: that library over the public connectors, with the token in `CESIUM_ION_TOKEN`.

One application, four shells:
- **Desktop** (macOS, Linux): `cargo run -p tuile-wgpu-viewer`; on macOS also an application bundle (`tuile-viewer-bundle app`) steerable by `tuile://` URLs and scripts (`crates/tuile-viewer/macos/README.md`).
- **iOS**: `crates/tuile-viewer/ios/build.sh` (simulator or device).
- **Android**: `scripts/try-viewer.sh android` or `crates/tuile-viewer/android/build.sh`; the APK holds no token and asks a catalog proxy.
- **Web**: `packages/tuile-viewer`, an ES module — WebGPU, or WebGL2 where there is none — with its geometry server in a Web Worker; `scripts/try-viewer.sh web`.

CLI (desktop): `--lon --lat --altitude --heading --pitch`, `--here`, `--imagery <layer>`, `--imagery-compression off|auto|bc7|astc` (off by default), and sources by address with no catalog and no token: `--terrain-url <layer.json URL>`, `--imagery-template <{z}/{x}/{y} URL>`, `--imagery-scheme webmercator|geographic`, `--imagery-max-level <n>` (`examples/heightfield-serve` prints a working set). `--help` lists them all.

Keys: drag moves the globe, right-drag tilts and turns, wheel zooms; `N` north up, `L` current location, `C`/`V` copy/go to a view link, `I` next imagery layer, `W` wireframe, `D` diagnostic views, `F` freeze the traversal (the camera moves, the selection does not — essential for debugging the LOD). Touch gestures on phones. `O` (or `--record <file.mcap>`) records the session — camera, clocks, atmosphere, input and the pictures drawn — and `TUILE_REPLAY` flies a recorded camera path again exactly; `tuile-record` exports and diagnoses a recording.

Headless: `cargo run --release -p tuile-wgpu-viewer --example headless -- run all` plays the graphics budget's motion patterns on the real application without a window and holds each run to the specification's obligations (it needs the network, a GPU and a token served by a catalog proxy).

### Specific quality criteria
- Continuous zoom from the global orbit down to the ground: no jitter, no hole after stabilization, never a black tile.
- Window resize: the SSE reacts (viewport changes → the LOD changes).
- wgpu device loss handled without panic.
- Two render modes disagreeing on screen is a bug: the film renderer and the viewer draw the same ground.
