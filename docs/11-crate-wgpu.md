# 11 — `tuile-wgpu` spec + `examples/wgpu-viewer`

Reference rendering backend. Goal: correct and readable before fast. It is the showcase for the core, not a game engine.

## `tuile-wgpu`

### Responsibilities
1. Impl `PrepareRenderResources`: `DecodedTileContent` → `PreparedTile { vertex_buf, index_buf, textures, bind_group, index_count, transform }`.
2. Rendering pipeline: a single minimal PBR render pipeline —
   - vertex: position, normal, uv; `view_proj` uniform (f32, relative-to-center) + per-tile `model` (push constant or dynamic uniform);
   - fragment: base color texture × factor, simple Lambert + ambient (full PBR = post-v1);
   - `Depth32Float` depth, back culling, configurable swapchain `TextureFormat`.
3. `TileRenderer::render(&mut self, pass: &mut RenderPass, selected: &[&PreparedTile])` — the host owns the surface, the event loop and the pass; the crate creates NEITHER window NOR surface.
4. async→frame bridge: a `ContentPump` that drains the `ServerMessage`s of a `GeometryStream` (the `Content`s are already decoded there, whatever the binding), performs the GPU uploads (spread out: a budget of N uploads/frame to avoid hitches), returns the `Ack`s. The crate does not know whether the stream is in-process, WebSocket or HTTP pull — the host injects it.

### Features
- `native`: reqwest/tokio for the re-exported HttpFetcher.
- `web`: wasm-bindgen, fetch via web-sys; same public APIs.

### Constraints
- Uploads via `Queue::write_buffer`/`write_texture` in v1 (staging belt = later optimization).
- Mipmaps: generate them for textures (simple compute pass or successive copies) — without mips, the rendering shimmers and makes the project look broken.
- Formats: RGBA8UnormSrgb for base color. KTX2/BCn = post-M1 feature.
- No reference to winit in the lib.

## `examples/wgpu-viewer`

Demonstration binary and daily debug tool.

- winit + pollster/tokio, resizable window.
- CLI: `wgpu-viewer <tileset.json path|URL> [--max-sse 16] [--budget-mb 512]`. In M2, `--remote ws://…` is added: the `WsStream` binding replaces the `InProcessStream` — same renderer, only the injected `GeometryStream` changes.
- Orbital camera (drag = orbit, wheel = log zoom, right click = pan) initialized to frame the root bounding volume.
- Text overlay (can be a simple `log` or an egui overlay behind the `debug-ui` feature): FPS, selected/resident tiles, in-flight requests, GPU bytes, current max SSE.
- Keys: `W` wireframe (secondary pipeline), `B` display of bounding volumes (lines), `F` freeze the traversal (the camera moves, the selection does not — essential for debugging the LOD).

### Specific quality criteria
- Continuous zoom from the global orbit down to 1 m from the ground on the deepest fixture: no jitter, no REPLACE hole after stabilization.
- Window resize: the SSE reacts (viewport_height changes → the LOD changes).
- wgpu device loss handled without panic (log + recreation or clean exit).
