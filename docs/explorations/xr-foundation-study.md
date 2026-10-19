<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# XR foundation: headsets, browsers and simulators on one engine

A study, 2026-10-19, written before any code. It asks what tuile needs in order to run on:

- standalone headsets (Meta Quest; Android XR devices such as the Samsung Galaxy XR; Pico);
- PC VR through OpenXR;
- Apple Vision Pro;
- WebXR in browsers;
- the image generators of flight and driving simulators.

It answers with a target architecture, a phased roadmap, the risks and the decisions that
fall to the owner.

**Method.**
- The code was read on `main` at `65b04cb` (v0.8.3).
- The wgpu facts were read in the wgpu 29.0.3 sources the workspace locks, and in 30.0.1
  where a later behaviour matters.
- The budget work (`feat/budget-core`, `docs/explorations/graphics-budget-*.md` on
  `docs/graphics-budget`) is taken as decided where its owner decided it (D-A0 to D-A8).
- Everything else comes from four research passes over primary sources, cited inline with
  their dates:
  - the Khronos registry, inventory and conformance lists;
  - the Meta, Android, Apple and W3C documentation;
  - the wgpu, naga and openxrs repositories and issue trackers;
  - SISO, OGC, FAA and VESA documents.
- What could not be verified is marked **unverified**.

**On names.** Products, stores and tools are named where a fact is about one of them
(nominative use). None of these names belongs in a crate, a module or a public type
(CLAUDE.md rule 3). OpenXR, Vulkan, WebXR, MPCDI and CIGI are open standards and are named as
such.

## 0. Summary

**The answer in one line.**
- A headset is two more views in the traversal tuile already has.
- A simulator is N more.
- What is missing is mostly in the shared layers:
  - asymmetric views;
  - a renderer that draws N views;
  - reverse-Z depth in the viewer;
  - a frame step that winit does not own;
  - the budget's stereo invariants.
- The rest is one new shell and one new session crate.

**Decisive facts.**

1. **OpenXR from Rust works today with wgpu 29, without a fork, for the minimum.**
   - The `openxr` crate is at 0.22.0 (2026-09-17, OpenXR 1.1.58 headers).
   - The working pattern: the runtime creates the Vulkan instance and device through
     `XR_KHR_vulkan_enable2`, using wgpu-hal's own extension and feature lists, and wgpu
     wraps them (`from_raw`, `device_from_raw`, `from_hal`, `create_device_from_hal`,
     `texture_from_raw`).
   - It is in production in Bevy's XR plugin (wgpu 29) and in a shipped Rust rhythm game
     (wgpu 30, single-pass multiview, Quest APK and PC VR).
2. **Foveated rendering is the one hard gap.**
   - No version of wgpu has a fragment density map, shading rate or rasterization rate
     map, and no issue or PR asks for one (searched 2026-10-19).
   - Quest, Android XR and Pico foveation all need the density-map attachment in the
     render pass. Vision Pro needs a rasterization rate map.
   - Either a fork of wgpu-hal or no foveation.
3. **Quest specifics are manageable and align with the Android shell.**
   - OpenXR only; VrApi has been unsupported since August 2022.
   - The Khronos loader (≥ 1.0.34, Horizon OS v62+), no proprietary loader.
   - A NativeActivity APK, 64-bit, `targetSdk` 34 required for apps created since
     March 2026, APK < 1 GB.
   - A 60 fps floor at 72–120 Hz display rates.
   - An app memory limit of 5.75 GiB on Quest 3.
   - A recommended budget under 200 draw calls a frame (200–300 for busy scenes) and
     ~1.5 M triangles. **That budget, not the GPU memory, is where tuile's ~500 drawn
     tiles will hurt first.**
4. **Android XR and Pico implement Meta's foveation, space-warp and refresh-rate
   extensions.** One OpenXR code path covers the standalone headsets. Passthrough differs:
   Meta's passthrough layer on Quest, `ALPHA_BLEND` elsewhere.
5. **Vision Pro can very probably be driven by the same wgpu renderer** through Compositor
   Services' C API.
   - The `layered` layout is multiview; the depth is reverse-Z; the Rust target is now Tier 2.
   - It needs a Swift shell and has no foveation without a fork.
   - The planned RealityKit façade serves a different use (volumes beside other apps), not
     the same one.
6. **WebXR is reachable from the web shell.**
   - WebGL2 path: two passes in the Quest Browser and Chrome today.
   - WebGPU binding: ships in Safari on visionOS, experimental in the Quest Browser,
     flag-only in Chrome. wgpu still lacks an `xrCompatible` adapter option, a small patch.
7. **Simulators need what headsets need, plus:**
   - off-axis channels from display geometry;
   - an in-app warp and blend pass (VESA MPCDI);
   - a CIGI host interface with height-of-terrain queries;
   - one budget per GPU;
   - cluster-wide consistency at channel seams.
   The simulation world is converging on 3D Tiles: OGC CDB X served CDB as 3D Tiles; the
   U.S. Army's One World Terrain format extends 3D Tiles.
8. **Testing without a headset is possible in CI.**
   - Monado (Boost licence, v25.1.0) runs on a GitHub runner without a GPU over lavapipe,
     the software Vulkan: one project is green that way, with XCB under Xvfb.
   - Its null compositor runs a session with no display, as WebKit's tests do. The two
     together on a hosted runner are not yet shown.
   - On the Mac, the Meta XR Simulator is an OpenXR runtime (proprietary, test-only).
   - The rest needs the device, over adb.

**Recommendation.**
- **Quest first for the product; OpenXR on a desktop runtime first for the development
  loop and CI.**
- The minimum viable "globe in a headset" is: two passes, the budget's stereo invariants,
  the coarse ring of guard views, and attribution in a quad layer, at 72 Hz on Quest 3.
- Then comfort and performance: multiview, 90 Hz, space warp, passthrough, foveation if
  forked. Then the other runtimes, then simulators, then Vision Pro and WebXR. §11 argues
  the order.

## 1. The question, and the frame it is asked in

- **Universal foundation.** tuile is a foundation for every render mode (manifesto,
  PR #59): interactive on desktop, phone and browser, film, server, and headset. A mode
  that needs something builds it into the shared layers, so that every mode gets it. A fork
  of the engine for one mode is a design error. This study holds the headset and the
  simulator to that rule.
- **Two rules make headsets demanding.**
  - **No black ground** (CLAUDE.md). In a headset a hole is not an ugly frame, it is a gap
    in the floor under the user's feet. Every no-hole invariant of the budget (I1, I9, I11)
    becomes a comfort requirement.
  - **f64 until rebasing.** A head-tracked eye a few centimetres from a near plane, over a
    planet, is the case the rule was written for.

## 2. What tuile already has, and what it lacks

Read on `main` at `65b04cb` (v0.8.3), wgpu 29.0.3 and naga 29.0.3 from `Cargo.lock`; the wgpu
facts below were read in the 29.0.3 sources themselves.

| Piece | Today | What a headset or a channel needs from it |
|---|---|---|
| Traversal | `traverse(&[ViewState])` selects the **union** of several views, the best view per tile sets the priority (`crates/tuile-core/src/traversal.rs`); `ClientMessage::ViewerState { views, generation }` carries one view per eye (`protocol.rs`). Near plane 0.05 m, far 1e10 m. | An **asymmetric** frustum: `ViewState::perspective` is symmetric (one `fovy`, aspect from the viewport). OpenXR gives four angles per eye; a simulator channel gives an eye point and a screen rectangle. `ViewState::from_fov` is decided (budget D-A7); an off-axis constructor from screen corners is the same math. |
| Culling margin | `CULL_MARGIN = 0.25`: the culling frustum is 25 % wider than the screen, to cover the round trip between the camera and its selection. The SSE is not widened. | It is exactly what head rotation needs, and not enough at 300°/s (§9.4). |
| Stereo in films | `tuile-film/src/stereo.rs`: off-axis eyes (lens shift, never toe-in), a baseline scaled from a divergence budget (hyperstereo), each eye's window narrowed so that it never shows an unselected tile. `tuile-mp4/src/spatial.rs` writes MV-HEVC spatial video. `tuile-film-gpu` draws one `Shot` per eye; every render pass has `multiview_mask: None`. | The film's rule "never a tile less than the frame selected" becomes, live, "each eye is traversed": with one `ViewState` per eye, the union covers both by construction. |
| Renderer | `tuile-wgpu`: the host owns the pass; `TileRenderer::paint` fixes the order (backdrop depthless, tiles, fallback, overlay). One `ViewUniform` buffer written by `set_view` (`queue.write_buffer`), `Depth24PlusStencil8` (the stencil marks owned ground for the fallback pass), 4× MSAA. Per-tile model matrices rebased to a render origin near the eye, rewritten only when it moves more than 1 m (`prepare.rs`). | **One view per submission today**: two passes in one submission would both read the last `write_buffer`. The view must become an array indexed by the view (multiview's `view_index`, or a per-pass index). Depth must be reverse-Z in a float format for a 5 cm near plane and a horizon thousands of kilometres away; the film renderer already is (`Depth32Float`, reverse-Z, infinitely far). |
| Overlay | `tuile-ui`: compass, tilt, zoom, attribution caption, drawn in screen space. | No screen in a headset: controls and attribution go into a quad layer (world- or head-locked). Attribution of imagery must stay visible. |
| Shared app | `tuile-viewer`: one winit `ApplicationHandler` for desktop, iOS, Android and the web (`app/`), shells under `native/`, `web/`, `android`, `ios/`. | An OpenXR app is not driven by winit: `xrWaitFrame` paces it and input comes from OpenXR actions. The frame step (stream, pump, renderer, globe, profile, lifecycle) has to be callable without winit. |
| Android shell | `crates/tuile-viewer/android/build.sh`: an APK without Gradle (`cargo ndk`, `javac` + R8, `aapt2`, `zipalign -P 16`, `apksigner`), `TuileActivity extends NativeActivity`; the manifest written by `tuile-viewer-bundle` requires GLES 3, Vulkan optional. | A Quest build is this pipeline plus an OpenXR loader `.so` in the APK and a different manifest (§4.2). |
| Budget | `feat/budget-core`: one arbiter; stereo invariants I16 (one charge per resource for both eyes), I17 (both eyes from one decision), I18 (per-frame deadline); `FrameDeadline` from the display rate (D-A8). `decide` ≈ 1.1 ms mean still, 2.3 ms mean / 6.9 ms p99 in a zoom, O(view · log N). | The deadline comes from the runtime (`predictedDisplayPeriod`); the incremental need (O(changes · log N)) becomes a requirement at 90 Hz. |
| Ground heights | `tuile-film` `Ground::height_at`, timed at ~43 µs a query (commit `65b04cb`). | A simulator host asks the image generator for heights of terrain (CIGI HOT/HAT, §8.4). |
| wgpu 29.0.3 | `Features::MULTIVIEW` (Vulkan, Metal, DX12; GL on the web only), `SELECTIVE_MULTIVIEW` (Vulkan, DX12), `Limits::max_multiview_view_count`, `multiview_mask: Option<NonZeroU32>` on passes and pipelines; Metal multiview through vertex amplification. Raw interop: wgpu-hal Vulkan `Instance::from_raw`, `Instance::init_with_callback`, `Adapter::device_from_raw`, `Adapter::open_with_callback` (add device extensions and `pNext` features), `Device::texture_from_raw`; DX12 and Metal `texture_from_raw`; wgpu `Instance::from_hal`, `Adapter::create_device_from_hal`, `Device::create_texture_from_hal`. **No fragment shading rate, no fragment density map, no foveation** in 29.0.3 nor in 30.0.1. | Enough to wrap OpenXR swapchain images and to share the device OpenXR wants; not enough for foveated rendering (§3.5). |

## 3. OpenXR from Rust, and wgpu

### 3.1 The bindings

- **`openxr` 0.22.0** (2026-09-17) and **`openxr-sys` 0.14.0** are generated from the
  registry at OpenXR 1.1.58. The newest SDK is 1.1.65 (2026-10-16)
  ([crates.io](https://crates.io/crates/openxr), [generated.rs](https://github.com/Ralith/openxrs/blob/master/sys/src/generated.rs),
  [SDK releases](https://github.com/KhronosGroup/OpenXR-SDK-Source/releases)).
- **Linking.** `loaded` (the default, dlopen) or `linked`. `static` builds the loader with
  CMake and has an open path bug (#229, 2026-10-19), and the maintainer recommends the
  other two ([repo](https://github.com/Ralith/openxrs), [#229](https://github.com/Ralith/openxrs/issues/229)).
- **Android.** `AndroidPlatformInfo` (the VM and the activity context) and
  `XR_KHR_android_create_instance`.
- **Graphics bindings.** Vulkan (enable2, falling back to enable), D3D11, D3D12, OpenGL,
  OpenGL ES, headless. **Metal is on master since 2026-10-13, unreleased**
  ([graphics/](https://github.com/Ralith/openxrs/tree/master/openxr/src/graphics)).
- **Extensions.** Vendor extensions come from the registry's own definitions: the FB and META
  foveation, space warp, passthrough, refresh rate, performance metrics and settings, and
  `XR_ANDROID_*` ([docs.rs](https://docs.rs/openxr/0.22.0)). No proprietary SDK or header is
  needed to use them.
- **Status.** The crate is labelled experimental, actively committed (October 2026), and has no
  maintained alternative. A wgpu example PR has been open since 2022
  ([#124](https://github.com/Ralith/openxrs/pull/124)).
- **Licence.** MIT OR Apache-2.0, as tuile's.

### 3.2 Sharing one Vulkan device with the runtime

**The runtime must create, or at least approve, the instance and the device it will read
the swapchain images from.** With `XR_KHR_vulkan_enable2` it creates them itself
(`xrCreateVulkanInstanceKHR`, `xrCreateVulkanDeviceKHR`) from create-infos the application
fills.

The pattern that works with wgpu 29, as Bevy's XR plugin does it
([vulkan.rs](https://github.com/awtterpip/bevy_oxr/blob/main/crates/bevy_openxr/src/openxr/graphics/vulkan.rs)):

1. Ask wgpu-hal what it needs: `vulkan::Instance::desired_extensions`, then, once the
   runtime has named the physical device (`xrGetVulkanGraphicsDevice2KHR`),
   `Adapter::required_device_extensions` and `Adapter::physical_device_features`. All three
   are public in 29.0.3 (read in the sources).
2. Let the runtime create the instance with those extensions, then wrap it with
   `vulkan::Instance::from_raw` and expose the adapter.
3. Let the runtime create the device with wgpu's extensions and features, then wrap it with
   `Adapter::device_from_raw(raw, …, family_index, queue_index)`.
4. Lift both into wgpu with `Instance::from_hal`, `create_adapter_from_hal` and
   `Adapter::create_device_from_hal`.
5. Bind the session with `XrGraphicsBindingVulkan2KHR`, built from the same raw handles
   (`raw_instance`, `raw_physical_device`, `raw_device`, `queue_family_index`).

**Prior art.**

| Project | wgpu | Rendering | Note |
|---|---|---|---|
| Bevy's XR plugin, `bevy_mod_openxr` 0.6.0 (2026-06-20) | 29 | per eye; single-pass multiview is open PR #210 | Vulkan 1.1 on Android, 1.2 on desktop; pins OpenXR API 1.0 for SteamVR (PR #167) ([repo](https://github.com/awtterpip/bevy_oxr)) |
| rsaber (2026-10-16) | 30 | single-pass multiview on 2-layer sRGB swapchains | ships a Quest APK and PC VR builds ([repo](https://github.com/bandipapa/rsaber)) |
| Stardust XR | a fork | — | a forked wgpu for explicit sync ([Cargo.toml](https://github.com/StardustXR/server/blob/main/Cargo.toml)) |
| the 2022 wgpu + OpenXR example | 0.13 | — | archived ([repo](https://github.com/philpax/wgpu-openxr-example)) |

wgpu itself has no in-repo example. #602 was closed in 2024 as "now possible", and #8329 asks
for documentation and an `xr_compatible` option
([#602](https://github.com/gfx-rs/wgpu/issues/602), [#8329](https://github.com/gfx-rs/wgpu/issues/8329)).

**D3D12 on Windows.** Bevy matches the adapter LUID that the runtime asks for and hands
wgpu-hal's raw device and queue to `XrGraphicsBindingD3D12KHR`. wgpu-hal's DX12 backend has
`texture_from_raw` but no device-from-raw, so wgpu creates the device itself
([d3d12.rs](https://github.com/awtterpip/bevy_oxr/blob/main/crates/bevy_openxr/src/openxr/graphics/d3d12.rs)).
Every PC runtime surveyed also takes Vulkan (§5.3), so D3D12 is optional.

### 3.3 Swapchain images

- **Wrapping.** A swapchain image is a `VkImage` the runtime owns. Each one is wrapped once
  with `vulkan::Device::texture_from_raw(image, desc, None, TextureMemory::External)` and
  `create_texture_from_hal`:
  - `depth_or_array_layers: 2` for a two-view array;
  - an sRGB colour format among those the runtime offers (rsaber uses `Rgba8UnormSrgb` and
    `Bgra8UnormSrgb`).
- **wgpu 30 changes the call.** `create_texture_from_hal` takes an `initial_state`
  (PR #9496, [CHANGELOG](https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md)). Moving to
  30 is a small migration in one crate.
- **Layouts and queue.** OpenXR expects a released image in `COLOR_ATTACHMENT_OPTIMAL` (or
  depth-stencil attachment optimal), owned by the bound queue. The runtime may touch that
  queue inside `xrBeginFrame`, `xrEndFrame` and the acquire/release calls, so those calls
  must be externally synchronised with wgpu's submissions
  ([khr_vulkan_enable.adoc](https://github.com/KhronosGroup/OpenXR-Docs/blob/main/specification/sources/chapters/extensions/khr/khr_vulkan_enable.adoc)).
  wgpu has `CommandEncoder::transition_resources` (since v25) for the final transition.
- **Depth.** wgpu-hal maps `Depth24PlusStencil8` to `D24_UNORM_S8_UINT` where the device
  supports it, and to `D32_SFLOAT_S8_UINT` otherwise
  ([conv.rs](https://github.com/gfx-rs/wgpu/blob/v29.0.3/wgpu-hal/src/vulkan/conv.rs)).
- **No depth resolve.** wgpu 29's depth attachment has no resolve target (read in
  `render_pass.rs`). A multisampled depth cannot be resolved inside the pass for submission
  to the compositor. Either a separate resolve pass, which on a tiled GPU means storing the
  multisampled depth, or no depth submission in the main pass (§4.5).

### 3.4 Multiview and naga

- **wgpu 29.**
  - `Features::MULTIVIEW` on Vulkan, Metal (vertex amplification) and DX12 (view
    instancing), and GL on the web only; `multiview_mask: Option<NonZeroU32>` on the
    pipeline and the pass.
  - `SELECTIVE_MULTIVIEW` on Vulkan and DX12.
  - `MULTISAMPLE_ARRAY` (since v28, Vulkan) allows MSAA on the view array
    ([#8571](https://github.com/gfx-rs/wgpu/pull/8571)).
- **naga 29.**
  - WGSL `@builtin(view_index)` must be a `u32` (since 28) and needs the multiview
    capability. It is an input of the vertex, fragment, task and mesh stages.
  - Back ends: SPIR-V `ViewIndex` with `SPV_KHR_multiview`; MSL `amplification_id`
    (MSL ≥ 2.2); HLSL `SV_ViewID` (SM ≥ 6.1); GLSL `gl_ViewIndex`, or `gl_ViewID_OVR` on
    WebGL.
  - No WGSL `enable` directive appears necessary (read in the parser, untested)
    ([naga back ends](https://github.com/gfx-rs/wgpu/tree/v29.0.3/naga/src/back)).
- **Open edges** ([tracking #9873](https://github.com/gfx-rs/wgpu/issues/9873)):
  - a partial view mask exposes uninitialised layers (#9620);
  - a hidden `maxMultiviewInstanceIndex` (#8333);
  - layered rendering issues and DX12's four-view limit
    ([#8509](https://github.com/gfx-rs/wgpu/issues/8509));
  - DX12 on one vendor's GPUs (#8645);
  - on GL, `max_multiview_view_count` is hard-coded to 0 (read in 29.0.3).
- **For tuile.** Vulkan multiview with two views and a full mask is the well-trodden case
  (rsaber ships it). Keep to it.

### 3.5 Foveation: the gap

- **What the runtimes ask.** On Quest, Android XR and Pico, fixed and eye-tracked foveation
  go through `XR_FB_foveation` + `XR_FB_foveation_vulkan`:
  - the swapchain is created with the fragment-density-map flag;
  - the runtime returns a density-map `VkImage` per swapchain image;
  - the levels are none, low, medium, high, or dynamic
    ([fb_foveation_vulkan](https://github.com/KhronosGroup/OpenXR-Docs/blob/main/specification/sources/chapters/extensions/fb/fb_foveation_vulkan.adoc));
  - the application attaches that image to **every foveated render pass**
    (`VK_EXT_fragment_density_map`; R8G8, about 1/32 of the resolution)
    ([Meta blog](https://developers.meta.com/horizon/blog/vulkan-for-mobile-vr-rendering/)).
- **What wgpu exposes.** Nothing:
  - no fragment density map, shading rate or rasterization rate map in 29.0.3 or 30.0.1
    (read in the sources);
  - no issue or PR in gfx-rs/wgpu (searched 2026-10-19);
  - WebGPU's variable-rasterization proposal was closed unmerged on 2026-06-18
    ([gpuweb #6271](https://github.com/gpuweb/gpuweb/pull/6271)).
- **What a fork would touch.** wgpu-hal's Vulkan backend caches `VkRenderPass` objects by a
  `RenderPassKey` and begins them itself. Adding the attachment means patching the key, the
  render pass and framebuffer create-infos, and the public pass descriptor
  ([device.rs](https://github.com/gfx-rs/wgpu/blob/v29.0.3/wgpu-hal/src/vulkan/device.rs)).
  This is contained work, but a fork to carry across every wgpu major.
- **A device risk.** Meta has an open investigation of a headset freeze on Quest 3, OS v207,
  with a density-map attachment at the high foveation level
  ([report](https://developers.meta.com/horizon/feedback/vr/investigations/1044460291548939/)).
- **Eye-tracked foveation** (`XR_META_foveation_eye_tracked`) is for Quest Pro and Meta's
  newer glasses, not Quest 3 or 3S, and Vulkan only
  ([ETFR](https://developers.meta.com/horizon/documentation/unreal/unreal-eye-tracked-foveated-rendering/)).
- **Conclusion: ship the first headset build without foveation.**
  - tuile's shading is cheap: Lambert, the imagery mosaic, aerial perspective.
  - Its cost is in the number of draws and the imagery slots per fragment, which foveation
    does not reduce.
  - Measure on the device, then decide on the fork (D-X5).

### 3.6 wgpu issues that bite on headsets

- **#10564 (open).** wgpu-hal uses the GPU's Vulkan version rather than the instance's.
  Quest's runtime advertises 1.0–1.2 while the Adreno 740 reports 1.3; the fix PR was
  closed unmerged. The workaround is to create the instance at 1.3
  ([#10564](https://github.com/gfx-rs/wgpu/issues/10564)).
- **#8801 (open).** `draw_indexed_indirect` draws nothing on Quest 3 and 2 (wgpu 27/28), and
  validation layers break OpenXR device creation on Quest
  ([#8801](https://github.com/gfx-rs/wgpu/issues/8801)). tuile draws directly today. Any move
  to GPU-driven drawing must be checked on the device first.
- **#10568 (merged, unreleased).** spv-in accepts a signed `ViewIndex`.
- **No subpasses or input attachments.** Every post pass on a tiled GPU pays a full store and
  load. tuile's viewer path has none, and must keep it so in the headset.

## 4. Meta Quest

### 4.1 API and loader

- **OpenXR is the only supported API for new applications.** VrApi has been unsupported
  since 2022-08-31
  ([OpenXR vs VrApi](https://developers.meta.com/horizon/documentation/unity/os-openxr-vrapi/)).
- **GLES is legacy; new features ship on Vulkan.** No hard Vulkan-only submission rule was
  found (unverified), but eye-tracked foveation is Vulkan only
  ([Vulkan and OpenGL](https://developers.meta.com/horizon/documentation/unity/os-vulkan-opengl/)).
  wgpu's Vulkan backend is the path.
- **The loader is Khronos's Android loader, ≥ 1.0.34**, on Horizon OS v62+: "no proprietary
  loader" ([mobile OpenXR](https://developers.meta.com/horizon/documentation/native/android/mobile-openxr/)).
  Meta's OpenXR SDK is only headers for experimental extensions and samples (v85,
  2026-02-12) ([repo](https://github.com/meta-quest/Meta-OpenXR-SDK)).

### 4.2 Manifest, packaging, store, against tuile's Android shell

| Requirement | Source | tuile's shell today | Change |
|---|---|---|---|
| `uses-feature android.hardware.vr.headtracking` (required, version 1); category `com.oculus.intent.category.VR`; `singleTask`; landscape; `com.oculus.supportedDevices` (`quest2\|questpro\|quest3\|quest3s\|vrglasses`) | [native manifest](https://developers.meta.com/horizon/documentation/native/android/mobile-native-manifest/) (2026-09-01) | `singleTask`, `LAUNCHER`, GLES 3 required, Vulkan optional (`tuile-viewer-bundle` `android.rs`) | a headset manifest variant: Vulkan required, the VR category, the head-tracking feature |
| The loader's permissions (`org.khronos.openxr.permission.OPENXR`, `OPENXR_SYSTEM`) and `<queries>` for the runtime broker providers | [loader manifest](https://github.com/KhronosGroup/OpenXR-SDK-Source/blob/main/src/loader/AndroidManifest.xml.in) | none | written by the bundler, as the AAR would merge them |
| `libopenxr_loader.so` in the APK | the Khronos AAR on Maven Central | none | `build.sh` fetches it as it fetches Kotlin's standard library, checked by digest; a `NOTICE` entry (Apache-2.0) |
| `targetSdk` **34** mandatory for apps created since 2026-03-01; `minSdk` 32 recommended; `debuggable` false | [release manifest](https://developers.meta.com/horizon/resources/publish-mobile-manifest/) (2026-10-15) | `targetSdk` 35, `minSdk` 26 | a store build pins them; development builds can keep the phone's |
| 64-bit only | [64-bit policy](https://developers.meta.com/horizon/blog/quest-submission-policy-update-64-bit-by-default/) | arm64-v8a only | none |
| APK < 1 GB, each OBB < 4 GB | [VRC packaging](https://developers.meta.com/horizon/resources/vrc-quest-packaging-5/) | small (no data inside) | none |
| NativeActivity | Khronos's `hello_xr` uses `android.app.NativeActivity` ([manifest](https://github.com/KhronosGroup/OpenXR-SDK-Source/blob/main/src/tests/hello_xr/AndroidManifest.xml)) | `TuileActivity extends NativeActivity` | keep it; the trim-memory and caption hooks still apply |

- **Store.** App Lab was merged into the Horizon Store in 2024. Apps are no longer refused on
  subjective quality; there is an Early Access tag
  ([UploadVR](https://www.uploadvr.com/quest-app-lab-merged-into-meta-horizon-store/)).
- **Performance check** (VRC.Quest.Performance.1, 2025-10-22): display at 72, 80, 90, 96,
  100 or 120 Hz, and **render at no less than 60 fps**. Half rate is allowed with space warp
  and motion vectors ([VRC](https://developers.meta.com/horizon/resources/vrc-quest-performance-1/)).
- **Sideloading** needs a verified developer account and Developer Mode, then `adb install`
  ([device setup](https://developers.meta.com/horizon/documentation/native/android/mobile-device-setup/)).
  This is what `build.sh --install` already does.
- **No Gradle.** The pipeline stays as it is: the APK is still one native library plus one
  small activity.

### 4.3 GPU, memory and what tuile draws

**Quest 3** ([comparison](https://developers.meta.com/horizon/resources/device-optimization-comparison/),
[Unity perf](https://developers.meta.com/vr/documentation/unity/unity-perf/),
[memory](https://developers.meta.com/horizon/essentials/memory-ram/),
[CPU/GPU levels](https://developers.meta.com/horizon/documentation/unity/os-cpu-gpu-levels/),
[boost](https://developers.meta.com/horizon/documentation/native/android/po-quest-boost/)):

| What | Quest 3 | What it means for tuile |
|---|---|---|
| GPU | XR2 Gen 2, Adreno 740, ~2.5× Quest 2, tiled (~2 MB tile memory), 285–640 MHz | tiled-GPU rules apply (below) |
| Per-eye panel | 2064×2208, 72/80/90/120 Hz | 13.9 / 11.1 / 8.3 ms a frame |
| App memory | **5.75 GiB** PSS (4.4 GiB on Quest 2/Pro), enforced by low-memory kills | `Profile::headset`: the budget's hard limits from it, as the phone's come from jetsam |
| Draw calls | recommended < 200; 200–300 "busy", up to 700–1000 for light scenes | **the first limit tuile meets**: ~500 drawn tiles (spec §11.7), each a draw per mesh and per extra imagery pass, **per eye** with two passes |
| Triangles | ~1.3–1.8 M a frame | to measure: a terrain tile is thousands of triangles |
| Levels | `SustainedHigh` by default; GPU levels 3–4 need passthrough off; level 4 is the real budget; boost ≤ 45 s and ≤ 20 % of runtime | the thermal input of the budget (`set_thermal`) has a headset meaning |

**Tiled-GPU rules** ([Meta, GPU pipelines](https://developers.meta.com/horizon/documentation/unity/po-advanced-gpu-pipelines/)):

- Clear or discard on load.
- Store neither depth nor the multisampled colour.
- Resolve in the pass.
- Never store mid-pass.

tuile's surface pass today stores both the 4× multisampled colour and the depth
(`StoreOp::Store`, `tuile-wgpu/src/surface.rs`). Harmless on a desktop, expensive on a tiler.
The headset target discards them (§10.3).

**What brings the draw count down,** in order of cost:

1. **Multiview.** One draw for both eyes.
2. **A coarser SSE target in the headset profile,** justified by its angular resolution.
3. **Render bundles** for the static part of the frame.
4. **Imagery as texture arrays or atlases** (spec D4, deferred), which removes the extra
   passes per drape.
5. **GPU-driven drawing,** only after #8801 is checked on the device.

### 4.4 Frame timing and the `FrameDeadline`

- **What `xrWaitFrame` returns** ([XrFrameState](https://registry.khronos.org/OpenXR/specs/1.1/man/html/XrFrameState.html),
  [xrWaitFrame](https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrWaitFrame.html)):
  - it throttles the application;
  - `predictedDisplayTime` is the midpoint of the frame's display interval;
  - `predictedDisplayPeriod` is what to use to predict later display times;
  - it may differ from the panel's refresh.
- **Views are located at `predictedDisplayTime`,** and the same time is carried through the
  whole frame.
- **Refresh rate.** Requested with `XR_FB_display_refresh_rate`, as a request, not a
  guarantee ([xrRequestDisplayRefreshRateFB](https://registry.khronos.org/OpenXR/specs/1.1/man/html/xrRequestDisplayRefreshRateFB.html)).
- **Phase sync** is reportedly always on under Meta's runtime (second-hand). No late-latching
  mechanism was found in OpenXR (unverified).
- **The budget's deadline follows directly.**
  - `FrameDeadline::from_period(predictedDisplayPeriod, share)`: the executor's upload share
    is a fraction of the period, converted to bytes at the measured upload throughput.
  - It is re-read every frame, so a runtime that drops to half rate under space warp, or
    changes the refresh rate, changes the deadline the same frame.
  - How the runtime signals half rate (a doubled period, or the cadence of `xrWaitFrame`) is
    unverified; reading the period every frame covers both.
- **The decision stays off the frame.** The traversal and `decide` run on the server task,
  not the render thread (architecture §4.6). On the headset's CPU they bound how fresh a
  decision is, not the frame. The incremental need is the requirement that keeps them fresh
  at 90 Hz.

### 4.5 Reprojection, depth, space warp

- **Rotational timewarp** is the runtime's "pothole insurance". It corrects rotation, not
  translation, animation or view-dependent shading
  ([ATW](https://developers.meta.com/horizon/blog/asynchronous-timewarp-examined/)).
- **Depth layer.** With `XR_KHR_composition_layer_depth` the runtime "may use [depth] to
  perform more accurate reprojections". Reversed depth and an infinite far plane are allowed:
  `nearZ > farZ`, `farZ` up to +∞, depth attached to every view
  ([spec](https://github.com/KhronosGroup/OpenXR-Docs/blob/main/specification/sources/chapters/extensions/khr/khr_composition_layer_depth.adoc),
  [XrCompositionLayerDepthInfoKHR](https://registry.khronos.org/OpenXR/specs/1.1/man/html/XrCompositionLayerDepthInfoKHR.html)).
  Quest 3, SteamVR, Link, Android XR and Pico expose it (OpenXR inventory). Whether Quest's
  positional reprojection uses it is unverified.
- **Application SpaceWarp** (`XR_FB_space_warp`): the app renders at half rate and gives the
  runtime what it needs to synthesise the other frames — up to 70 % more GPU headroom, on one
  projection layer only
  ([spec](https://github.com/KhronosGroup/OpenXR-Docs/blob/main/specification/sources/chapters/extensions/fb/fb_space_warp.adoc),
  [guide](https://developers.meta.com/horizon/documentation/native/android/mobile-asw/)).
  It needs:
  - a **motion-vector swapchain** (current NDC minus previous NDC, `R16G16B16A16_SFLOAT`);
  - a **depth swapchain** (`D24_UNORM_S8`), both at the resolution
    `XrSystemSpaceWarpPropertiesFB` recommends;
  - near and far (reversed allowed), and an `appSpaceDeltaPose` for locomotion.
- **For tuile this is unusually cheap.** The scene is static: a pixel's motion vector is its
  world position under the previous frame's view-projection, minus the current one. The
  motion-vector and depth pass is a separate, low-resolution, single-sample pass. It sidesteps
  the missing depth resolve of §3.3.

### 4.6 Passthrough and mixed reality

- **Quest.** `XR_FB_passthrough` with the feature `com.oculus.feature.PASSTHROUGH`. The
  passthrough layer goes under the projection layer, which carries source alpha, and the
  environment blend mode stays **opaque**
  ([passthrough](https://developers.meta.com/horizon/documentation/native/android/mobile-passthrough/)).
  Quest 3's runtime also lists `ALPHA_BLEND`
  ([inventory](https://github.com/KhronosGroup/OpenXR-Inventory/blob/main/runtimes/meta_quest_3_mobile.json)).
- **Android XR and Pico.** No FB passthrough on Android XR; both use `ALPHA_BLEND`.
- **The portable path:**
  1. enumerate the blend modes;
  2. premultiplied alpha in the projection layer;
  3. Meta's layer as the Quest path.
- **In tuile terms.** A mixed-reality globe is the miniature scale of §9.2, a table-top
  globe over the room, with the backdrop and sky transparent. Black ground stays forbidden on
  the globe itself.

## 5. Other headsets and runtimes

### 5.1 Android XR

- **The runtime.** OpenXR 1.1 ([Android XR OpenXR](https://developer.android.com/develop/xr/openxr)).
- **Extensions.** Graphics through Vulkan (enable and enable2) or GLES. It implements
  Meta's `XR_FB_foveation`, `XR_FB_foveation_vulkan`, `XR_FB_space_warp`,
  `XR_FB_display_refresh_rate` and `XR_META_foveation_eye_tracked`, plus
  `XR_EXT_performance_settings` and `XR_ANDROID_*`
  ([extensions](https://developer.android.com/develop/xr/openxr/extensions)).
  The Quest code path carries over almost whole.
- **The manifest** ([get started](https://developer.android.com/develop/xr/openxr/get-started)):
  - the `XR_ACTIVITY_START_MODE_FULL_SPACE_UNMANAGED` property;
  - `<uses-native-library libopenxr.google.so required=false>`;
  - `uses-feature android.software.xr.api.openxr` at 1.1;
  - input features for controllers, hands and eyes.
- **Galaxy XR.** Launched on 21–22 October 2025, XR2+ Gen 2, 16 GB, OpenXR 1.1 conformant
  ([Tom's Guide](https://www.tomsguide.com/computing/vr-ar/samsung-galaxy-xr-launches-for-usd1-700-less-than-apple-vision-pro-release-date-price-specs-and-more),
  [Khronos conformance](https://www.khronos.org/conformance/adopters/conformant-products/openxr)).
- **The emulator** does not run OpenXR apps
  ([XR AVD](https://developer.android.com/develop/xr/jetpack-xr-sdk/run/create-avds/xr-headsets-glasses)).
- **One APK for Quest and Android XR.** Godot states one APK can be deployed to several
  devices for development, while store builds need vendor settings
  ([Godot](https://docs.godotengine.org/en/4.7/tutorials/xr/deploying_to_android.html)).
  Whether one APK passes both stores is unverified. The bundler can write one manifest per
  store from the same code.

### 5.2 Pico

- **Pico 4 Ultra** is OpenXR 1.1 conformant (2024-11-18). It exposes FB foveation, space
  warp and passthrough, and `ALPHA_BLEND`
  ([inventory](https://github.com/KhronosGroup/OpenXR-Inventory/blob/main/runtimes/bytedance_pico_4_ultra.json)).
- **Loader.** Pico's older documentation describes a loader of its own
  ([Pico SDK](https://sdk.picovr.com/docs/OpenXRMobileSDKv2/en/chapter_four.html)). Whether
  current Pico OS takes the Khronos loader is unverified: a packaging question, not a code one.

### 5.3 PC VR and Linux

From the OpenXR inventory ([runtimes](https://github.com/KhronosGroup/OpenXR-Inventory/tree/main/runtimes)):

| Runtime | Graphics | Notes |
|---|---|---|
| SteamVR on Windows (2.19) | Vulkan (enable, enable2), D3D11, D3D12, GL | opaque only, no foveation or space warp extensions; **conformant at OpenXR 1.0 only** ([conformance](https://www.khronos.org/conformance/adopters/conformant-products/openxr)), hence Bevy's API-version pin |
| SteamVR on Linux | Vulkan, GL, GLES | headless null driver fails (§12) |
| Meta's PC runtime (Link, Air Link) | Vulkan, D3D11, D3D12, GL | opaque |
| Monado | Vulkan, GL, GLES on Linux; plus D3D11/12 on Windows | open source; the CI runtime (§12) |
| Meta XR Simulator for macOS | **Metal and Vulkan** | the only OpenXR runtime on a Mac; simulates a Quest 3 ([inventory](https://github.com/KhronosGroup/OpenXR-Inventory/blob/main/runtimes/meta_xr_simulator_mac.json)) |

- **Vulkan is the one binding every headset runtime takes,** and wgpu's Vulkan backend is the
  one path tuile needs.
- **Two-layer array swapchains with multiview on SteamVR** are allowed by the core spec, and
  rsaber's single multiview path ships for SteamVR (indirect evidence). Valve's confirmation
  was not found.
- **Streaming a Quest from Linux.** WiVRn (v26.9, 2026-09-12) streams from Monado to Quest,
  Pico and Galaxy XR; ALVR streams SteamVR ([WiVRn](https://github.com/WiVRn/WiVRn),
  [ALVR](https://github.com/alvr-org/ALVR)). A Linux desktop with a GPU can therefore drive
  a Quest through Monado, open source end to end.

## 6. Apple Vision Pro

visionOS 27 shipped on 14 September 2026 ([9to5Mac](https://9to5mac.com/2026/09/09/visionos-27-release-date-heres-when-the-new-update-launches/)).
Apple's WWDC26 session 287 names **Compositor Services** as the way to bring an engine of
one's own to visionOS ([WWDC26 287](https://developer.apple.com/videos/play/wwdc2026/287/)).
No change to Compositor Services specific to visionOS 27 was found.

### 6.1 Compositor Services: what it asks of a renderer

- **The shell.** A SwiftUI app presents
  `ImmersiveSpace { CompositorLayer { layerRenderer in … } }`. The system hands over a
  `LayerRenderer`, and a Metal loop runs on a thread of its own
  ([Compositor Services](https://developer.apple.com/documentation/compositorservices),
  [drawing fully immersive content](https://developer.apple.com/documentation/compositorservices/drawing-fully-immersive-content-using-metal)).
  The thin Swift shell is unavoidable; winit plays no part.
- **Layouts.** `dedicated` (one texture per eye), `shared`, or `layered` (one slice per eye
  in a 2D array, single pass). Apple calls `layered` the optimal layout
  ([layout](https://developer.apple.com/documentation/compositorservices/layerrenderer/layout),
  [WWDC23 10089](https://developer.apple.com/videos/play/wwdc2023/10089/)).
  In wgpu terms, `layered` is multiview, and wgpu implements multiview on Metal through
  vertex amplification (read in the 29.0.3 sources).
- **The frame**, in order ([WWDC23 10089](https://developer.apple.com/videos/play/wwdc2023/10089/),
  [WWDC25 294](https://developer.apple.com/videos/play/wwdc2025/294/)):
  1. `queryNextFrame`, `predictTiming`, then the update phase.
  2. Wait until `optimalInputTime`.
  3. Query the drawables. The device anchor comes from ARKit's world tracking, read at the
     presentation time.
  4. Encode, `encodePresent`, commit.

  A missed deadline makes the compositor reuse the previous frame.
- **The drawable.** It gives the views, colour and depth textures, rasterization rate maps,
  `depthRange`, and `computeProjection(convention:viewIndex:)`; the view tangents are
  deprecated ([drawable](https://developer.apple.com/documentation/compositorservices/layerrenderer/drawable)).
- **Depth.** Reverse-Z, cleared to 0, valid on every pixel, colour in premultiplied alpha
  ([WWDC24 10092](https://developer.apple.com/videos/play/wwdc2024/10092/)).
  This is the film renderer's depth convention, and not yet the viewer's (§2).
- **Foveation.** System-managed: a `MTLRasterizationRateMap` per frame. Gaze is never given
  to the app ([Apple privacy](https://www.apple.com/legal/privacy/data/en/eyes-hands/)).
- **Immersion.**
  - Full and mixed are supported. Mixed immersion with Metal arrived in visionOS 2.
  - Progressive immersion needs the `layered` layout
    ([immersion level](https://developer.apple.com/documentation/compositorservices/controlling-metal-rendering-immersion-level)).
  - Passthrough is composited by the system.
- **Hardware.** The M5 Vision Pro has a 10-core GPU and 16 GB, and runs at 90, 96, 100 or
  120 Hz ([Apple support](https://support.apple.com/en-us/125436)).

### 6.2 Can wgpu drive it?

**Very probably, without a fork, but with foveation off.** No project has done it with wgpu
yet.

- Compositor Services has a C API (`cp_*`), and ARKit has one too (`ar_*`). Apple's samples
  run the whole loop in C. `objc2-compositor-services` 0.3.2 binds it for Rust
  ([docs.rs](https://docs.rs/objc2-compositor-services/latest/objc2_compositor_services/)).
- `aarch64-apple-visionos` is now a **Tier 2** Rust target without host tools
  ([platform support](https://doc.rust-lang.org/nightly/rustc/platform-support/apple-visionos.html),
  [MCP 918](https://github.com/rust-lang/compiler-team/issues/918)).
  wgpu's CI builds visionOS ([ci.yml](https://github.com/gfx-rs/wgpu/blob/trunk/.github/workflows/ci.yml)).
- The hooks exist in wgpu 29:
  - `wgpu_hal::metal::Device::texture_from_raw` plus `create_texture_from_hal` wrap the
    drawable's textures every frame;
  - the raw command buffer and queue let `cp_drawable_encode_present` be encoded on wgpu's
    queue
    ([metal/device.rs](https://github.com/gfx-rs/wgpu/blob/v29.0.4/wgpu-hal/src/metal/device.rs)).

  Inferred from the sources, untested.
- **Missing in wgpu: rasterization rate maps.** They are not exposed (none in wgpu-hal's Metal
  backend). Foveation is either off or a fork.
- **Closest template:** Godot's visionOS XR module, merged on 5 August 2026
  ([godot#109975](https://github.com/godotengine/godot/pull/109975)). It drives Compositor
  Services from C++ and routes the rate map to its Metal driver as a variable-rate-shading
  texture.

### 6.3 Three routes, and how they relate to the planned RealityKit façade

| Route | What it reuses | What it gives | What it costs |
|---|---|---|---|
| **Compositor Services + wgpu** (`tuile-xr`'s visionOS binding, Metal) | the whole wgpu renderer, the budget, the shared app | full and mixed immersion, the same picture as every other mode | a Swift shell; foveation off until a fork; immersive only (no windows or volumes beside other apps) |
| **RealityKit façade** (`tuile-swift`, roadmap M4, LowLevelMesh) | the core and the stream; not the renderer | windows and volumes in the Shared Space; the system handles stereo, foveation and hover | a second renderer (mesh per tile, RealityKit's materials: no CustomMaterial on visionOS, [CustomMaterial](https://developer.apple.com/documentation/realitykit/custommaterial)); RealityKit and Compositor Services cannot share one immersive space ([forum](https://developer.apple.com/forums/thread/766378)) |
| **RemoteImmersiveSpace** (macOS 26+) | the existing Mac viewer | a Mac renders and streams to the headset, progressive and full styles | latency and bandwidth unmeasured; a Mac in the loop ([RemoteImmersiveSpace](https://developer.apple.com/documentation/swiftui/remoteimmersivespace)) |

- RealityKit is the right tool for a **tabletop globe in a volume** next to other apps.
  Compositor Services is the right tool for **standing on the terrain**.
- They are complementary, not alternatives. visionOS 27's `LowLevelDeviceResource` can carry
  textures rendered elsewhere into RealityKit, through shared Metal textures or IOSurface
  ([LowLevelDeviceResource](https://developer.apple.com/documentation/realitykit/lowleveldeviceresource)).
- The spatial video already written by `tuile-mp4` is a third, offline route that works
  today.

## 7. WebXR in browsers

### 7.1 Status

- **The spec.** The WebXR Device API is a W3C Candidate Recommendation Draft (9 June 2026)
  ([TR](https://www.w3.org/TR/webxr/)).
- **Where it runs.**
  - Chrome 79+ on desktop and Android, the Quest Browser and Samsung Internet support it;
    Firefox does not ([MDN BCD](https://github.com/mdn/browser-compat-data/blob/main/api/XRSession.json)).
  - Chrome on Android XR adds hand input, anchors, depth sensing and hit testing
    ([Android XR web](https://developer.android.com/develop/xr/web)).
  - Safari on visionOS has had WebXR since visionOS 2 (immersive-vr, transient pointer and
    hands) ([WebKit](https://webkit.org/blog/15443/news-from-wwdc24-webkit-in-safari-18-beta/)).
- **WebGPU binding** (`XRGPUBinding`, projection layers as texture arrays, depth and motion
  vectors): an Editor's Draft (12 August 2026)
  ([spec](https://immersive-web.github.io/webxr-webgpu-binding/)).
  - It **ships in Safari 26.2 on visionOS**
    ([WebKit 26.2](https://webkit.org/blog/17640/webkit-features-for-safari-26-2/)).
  - It is experimental in the Quest Browser, whose 146–152 release notes list WebGPU depth,
    space warp, foveation and composition layers
    ([Quest Browser notes](https://developers.meta.com/horizon/release-notes/web/)).
  - In Chrome it is behind a flag, and the chromestatus entry has had no activity since
    2024 ([chromestatus](https://chromestatus.com/feature/5077077997649920)).
- **WebXR Layers** ships in Chrome M147 ([chromestatus](https://chromestatus.com/feature/6634466544058368)).

### 7.2 What wgpu can do with it

- **WebGPU path.**
  - wgpu 30.0.0 (July 2026) added `Device::as_webgpu()` and
    `create_texture_from_webgpu_handle` ([CHANGELOG](https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md)),
    enough to wrap an `XRGPUSubImage`'s textures.
  - There is **no `xrCompatible` adapter option**, so `new XRGPUBinding` would refuse the
    device. PR #9350 was closed unmerged ([#9350](https://github.com/gfx-rs/wgpu/pull/9350));
    issue [#8329](https://github.com/gfx-rs/wgpu/issues/8329) is open.
  - A small fork, or an upstream change, is needed.
  - WebGPU has no multiview, so two passes, one per array layer.
- **WebGL2 path.**
  - `XRWebGLLayer` is an opaque framebuffer, and wgpu's GL backend has
    `TextureInner::ExternalFramebuffer` (read in 29.0.3) and `Adapter::new_external` over an
    XR-compatible context ([PR #2609](https://github.com/gfx-rs/wgpu/pull/2609)).
  - Multiview: the GL backend reports `MULTIVIEW` when `OVR_multiview2` is present, but
    hard-codes `max_multiview_view_count: 0` (read in 29.0.3,
    `wgpu-hal/src/gles/adapter.rs`). A multiview pass on GL would fail validation.
  - Viewports side by side, two passes.
- **Bindings.** `web-sys`'s unstable WebXR IDL covers the Device API, gamepads and hands only
  (2022); there is nothing for Layers or `XRGPUBinding`. Hand-written `wasm-bindgen`
  externs are needed ([webidls](https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/web-sys/webidls/unstable)).
- **Prior art.** expenses/mateversum shows WebXR on wgpu's GL path
  ([repo](https://github.com/expenses/mateversum)); Bevy has no working WebXR backend.

### 7.3 Relevance

- For `tuile-web` and the web shell, WebXR is the cheapest way to put the globe in a headset
  *without a store*: a URL in the Quest Browser.
- It reuses the web shell whole: worker, page, budget. It needs the stereo renderer and the
  asymmetric views of the shared layers anyway.
- The realistic order:
  1. WebGL2 with two passes, which works in the Quest Browser and Chrome today, with a wgpu
     patch for the external framebuffer path if one is needed.
  2. WebGPU projection layers once wgpu can request an XR-compatible adapter (Safari on
     visionOS first).
- The viewer path's rule from the convergence work still holds: compute-free, so that
  WebGL2 stays a target.

## 8. Simulators

Flight and driving simulators use image generators (IGs): domes, cockpits with several
displays, CAVEs, fixed-base trainers. They often run on Windows or Linux, with several GPUs
and outputs and often one PC per channel.

### 8.1 Channels are views

- **One channel is one off-axis frustum.** It is built from the eye point and three screen
  corners (generalized perspective projection, Kooima 2008; [CAVE projection](https://www.evl.uic.edu/pape/caveproj/)).
- **CIGI describes a view natively that way:** left, right, top and bottom half-angles,
  near and far, mirror mode. Views can be grouped so that one packet moves a panorama
  ([CIGI 3.0 ICD](https://cigi.sourceforge.io/files/CIGI_ICD_3.pdf)).
- **For tuile:** `ViewState::off_axis(eye, corners, viewport)` beside `from_fov`. A channel
  set is the same `&[ViewState]` the traversal already unions.
- **Orders of magnitude:**
  - an airline Level D trainer: 3 channels, 200°×40°
    ([brochure](https://www.indragroup.com/sites/default/files/indra_b737_level_d_full_flight_simulator_en_2019_1.pdf));
  - a fast-jet dome: 360°×135° at 120 Hz
    ([press](https://news.flightsafety.com/pressrelease/flightsafety-international-introduces-new-evolution-360-direct-view-projected-dome-display/));
  - an "eye-limited" dome at I/ITSEC 2024: 7 projectors at 8K and 240 Hz, on an 8-GPU IG
    ([press](https://www.scalabledisplay.com/company/news/itsec-2024-eye-limited-display/)).

### 8.2 Warp and blend

- **Curved screens need a per-projector geometric warp and an edge-blend mask.**
- **The open interchange is VESA MPCDI** ([v1.0, 2013](https://vesa.org/?p=2258)):
  - an XML descriptor;
  - a warp map in PFM;
  - an alpha (blend) map and a beta (black level) map in PNG;
  - a gamma value.
- **Driver-level warping exists for one vendor's professional cards on Windows**
  ([warp and blend](https://developer.nvidia.com/warp-and-blend)). It is not portable.
- **The portable answer is an in-app pass:**
  1. render the channel offscreen with overscan;
  2. draw the warp mesh or PFM lookup into the output;
  3. multiply by alpha, add beta, in linear light;
  4. apply gamma.

  It adds no frame of latency if it runs in the same frame. It belongs in `tuile-wgpu` as a
  generic post pass, fed by a calibration file.

### 8.3 Synchronisation

- **Genlock and framelock are hardware** (sync boards for up to 8 GPUs and 32 displays).
- **APIs:**
  - OpenGL has `NV_swap_group`.
  - Vulkan has `VK_NV_present_barrier`, which is vendor-specific and not ratified. It takes
    effect if swapchain creation succeeds, and is reported failing with more than one
    swapchain ([refpage](https://docs.vulkan.org/refpages/latest/refpages/source/VK_NV_present_barrier.html),
    [forum](https://forums.developer.nvidia.com/t/cant-create-more-than-1-swapchain-with-vk-nv-present-barrier/349276)).
  - D3D12 goes through a vendor API
    ([PresentBarrier](https://developer.nvidia.com/blog/synchronizing-present-calls-between-applications-on-distributed-systems-with-directx-12)).
- **wgpu exposes none of these.**
  - A device extension can be added through wgpu-hal's `open_with_callback`.
  - The swapchain is created inside wgpu, so a present barrier needs either a fork or
    presenting through raw Vulkan.
- **Software sync is an application barrier:** finish, barrier, swap
  ([Equalizer](https://www.equalizergraphics.com/documents/design/swapBarrier.html)).
- **Existing clusters:** a primary node broadcasts the frame number, time and eye point every
  frame, and the cluster stops if a node stops
  ([nDisplay ports](https://docs.unrealengine.com/4.27/WorkingWithMedia/IntegratingMedia/nDisplay/Ports)).

### 8.4 The host interface: CIGI

- **The standard.** CIGI 4.0 is SISO-STD-013-2014 (August 2014), free from SISO, usually
  over UDP ([overview](https://en.wikipedia.org/wiki/Common_Image_Generator_Interface)).
- **Host to IG:** IG Control (also selects the database), Entity Control, View
  Definition/Control, weather and environment, Earth reference model (WGS 84),
  collision segments, **HAT/HOT requests** (height above and of terrain), line-of-sight
  requests.
- **IG to host:** Start of Frame, HAT/HOT responses, LOS responses, collision
  notifications ([CIGI 3.0 ICD](https://cigi.sourceforge.io/files/CIGI_ICD_3.pdf)).
- **Synchronous mode is recommended.** The IG's Start of Frame at vsync is the heartbeat; the
  host answers at once.
- **The open class library is C++ under the LGPL and stops at 3.3**
  ([CCL](https://cigi.sourceforge.io/product_ccl.php)). No permissive CIGI 4.0 library was
  found.
- **For tuile:** a connector-like crate `tuile-cigi`, written from the standard.
  - Height queries map onto `Ground::height_at`, measured at about 43 µs a query. At 60 Hz,
    a few hundred queries a frame fit on one core.
  - The policy that needs deciding: answer from the drawn LOD, or from the finest data.
    Correlation between visuals and collisions asks for the drawn one, or for a declared
    rule.

### 8.5 Databases: OpenFlight, CDB, 3D Tiles

- **OpenFlight** is an OGC Community Standard ([OGC](https://www.ogc.org/standards/openflight)).
- **OGC CDB** ([OGC CDB](https://www.ogc.org/standards/cdb/)):
  - CDB 1.3 (Vol. 1: 15-113r7) was published in January 2026;
  - **CDB 2.0 Core** (23-034, a major revision with a core plus parts and profiles) was
    published in January 2026, with tiling still future work
    ([23-034](https://docs.ogc.org/is/23-034/23-034.html)).
- **OGC CDB X** (Discussion Paper 20-092, 2022) ([20-092](https://docs.ogc.org/dp/20-092.html)):
  - recommends GeoPackage as the container and glTF 2.0 for models;
  - its prototype served the whole San Diego CDB **as 3D Tiles** through an OGC API server;
  - says streaming 3D Tiles straight from CDB X GeoPackages should be achievable.
- **One World Terrain's format** (a U.S. Army training programme) is being built as an
  extension of 3D Tiles ([OWT](https://cesium.com/industries/mission-support/owt/)).
- **For tuile:** the simulation world is converging on 3D Tiles as a delivery format for
  terrain and models. An IG built on tuile reads what such pipelines publish. CDB becomes a
  source to convert, or later a `TileFetcher` over GeoPackage, not a format to render
  natively.

### 8.6 Consistency across channels and nodes

**The visible failure is a seam:** two adjacent channels drawing the same ground at two LODs
inside the blend zone. Regulators list "optical discontinuities and artifacts" among visual
system faults (14 CFR Part 60, Appendix A §6.e,
[eCFR text](https://www.law.cornell.edu/cfr/text/14/appendix-A_to_part_60)).

- **Several channels in one process** (one GPU or several): one traversal over all channels,
  one decision. A tile on a seam is the same tile in both channels by construction. This is
  I17 ("both eyes from one decision") with N eyes.
- **One process per GPU, or one node per channel:**
  - **Inputs.** Each node runs its own budget. Determinism (I12) makes the decision a
    function of the inputs, but the inputs differ: each node's arrivals and residency are
    its own.
  - **Angular resolution.** If every channel has the same pixels per degree and the same
    eye point, a tile's SSE is the same in every channel. Nodes then agree on what is
    *wanted*.
  - **Agreeing on what is drawn takes two more things:**
    1. a cluster-wide relaxation (the master broadcasts the budget's `sse_scale`, so a node
       under pressure does not coarsen alone);
    2. a cluster-wide commit for seam tiles: a refinement is activated on the frame every
       node holds it. This is the "new before old" rule across machines, and the Batch
       policy's "frame ready" gate (I13) applied per seam rather than per frame.
  - **Neighbour views.** A node also traverses its neighbours' channels as **guard views**
    (§10.3), at a reduced resolution, so the tiles on its edges are resident before they
    are needed.

### 8.7 Multi-GPU and refresh

- wgpu 29 opens one `Device` per `Adapter`, with no device groups or cross-device sharing
  ([Instance](https://docs.rs/wgpu/29.0.3/wgpu/struct.Instance.html)). IGs typically run one
  process or render thread per GPU.
- In tuile terms: **one budget per GPU**, the pool being per GPU (D-A0: one arbiter per
  physical pool).
- Frame-locked at 60 Hz, and 120 or 240 Hz for some domes. Transport delay of visual cues
  ≤ 120 ms for Level C/D (14 CFR Part 60, Appendix A, Table A1A 2.g.2; FAA material earlier
  quoted 150 ms) ([govinfo](https://www.govinfo.gov/content/pkg/CFR-2024-title14-vol2/pdf/CFR-2024-title14-vol2-part60-appA.pdf)).

### 8.8 Paging along a planned path

- IGs page ahead of the aircraft: paging pools for the area of regard, and path predictors
  feeding future poses. Patents describe both
  ([US6714203](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/6714203)).
- In tuile, future poses are **guard views at t + Δ** with the Preload tier. The 3D Tiles
  primer already has `PriorityGroup::Preload`.
- The film path already knows its whole trajectory. A planned flight is the interactive
  version of the same knowledge.

## 9. Geospatial XR: precision, scale, comfort, LOD

### 9.1 Precision with head tracking

- **The pose.** OpenXR poses are f32 metres in a local reference space (`local`,
  `local-floor`, `stage`). They are already local: no ECEF ever enters f32.
- **The composition, per frame, per eye, in f64:**
  `eye_ecef = anchor_ecef + R_enu(anchor) · (pose / scale)`, where `anchor` is the f64 ECEF
  point the user stands on, `R_enu` the local east-north-up frame, and `scale` the world
  scale (1 for human scale).
- **`ViewState::from_fov` is built from `eye_ecef`.** The GPU view matrix is built relative
  to the render origin, the same protocol as `prepare.rs`'s `rebase`.
- **The render origin stays at the anchor** while the user walks a few metres. The 1 m
  rewrite threshold of `rebase` means nothing is rewritten. When the anchor moves (flying),
  the threshold already bounds the rewrites.
- **f32 headroom.** One engine's documentation gives about 10.5 km from the origin as the
  limit of plain floats
  ([large world coordinates](https://dev.epicgames.com/documentation/en-us/unreal-engine/large-world-coordinates-in-unreal-engine-5)).
  The render origin may drift tens of kilometres from the eye before precision shows at
  head-tracked scale; a few kilometres is a safer budget for a 5 cm near plane.
- **Depth.** Near 5 cm, horizon thousands of kilometres away: reverse-Z in a float format
  with an infinite far plane is the only depth that holds
  ([Reed](https://www.reedbeta.com/blog/depth-precision-visualized/)).
  OpenXR's depth layer accepts it: `nearZ > farZ` means reversed, and `farZ` may be +∞
  ([XrCompositionLayerDepthInfoKHR](https://registry.khronos.org/OpenXR/specs/1.1/man/html/XrCompositionLayerDepthInfoKHR.html)).
  The viewer's `Depth24PlusStencil8` does not, and its coplanar ground (fallback above
  owned surfaces) already relies on the stencil, not on depth.

### 9.2 Scale: standing on the ground, or holding the globe

- **Human scale.** IPD 63 mm, the user's eye height above the terrain. Stereo depth is
  useful to a few tens of metres; beyond that the terrain reads as a picture, as in a
  cockpit.
- **Miniature, a tabletop globe.** A scale s ≪ 1:
  - the head's metre is 1/s metres of the world;
  - the eyes are IPD/s apart, which is hyperstereo, the film's own tool;
  - a tilted globe on a table reads as an object.
- **The traversal does not care.** The SSE is invariant under a uniform scale of eye and
  world: distance and geometric error scale together. A miniature view is the same
  `ViewState`, its eye placed at `pose / s`.
- **Studies on scaled eye separation.** A 150 mm IPD made objects look 38 % smaller, while a
  10 mm change had no significant effect ([Kim & Interrante 2017](https://www-users.cse.umn.edu/~interran/johnny17.pdf)).
- **Switching scales.** The switch between scales is a user action with a fade, never a
  glide.

### 9.3 Comfort

- **The rules** (Meta: [locomotion comfort](https://developers.meta.com/horizon/design/locomotion-comfort-usability/);
  Apple: [HIG Motion](https://developer.apple.com/design/human-interface-guidelines/motion),
  [WWDC23 10078](https://developer.apple.com/videos/play/wwdc2023-10078)):
  - **No motion the user does not drive.** The anchor moves only on input; the viewer's
    glides (`flight.rs`) become teleports with a fade, or a constant speed under a vignette.
  - **A stable horizon.** The ENU frame keeps up equal to the real up of the XR space; no
    roll, no pitch of the world.
  - **Snap turns** (15–180°) rather than smooth turns. Fixed speed steps. Brief, infrequent
    accelerations.
  - **A ground plane, always.** Black ground is a bug on a screen and a fall in a headset.
    The no-hole invariants (I1, I9, I11) are comfort requirements here, not only quality.
  - **No oscillation near 0.2 Hz.** The budget's I4 (no level changes more than once in
    10 s while still) covers LOD flapping.
- **Motion to photon** under ~20 ms ([Meta blog](https://developers.meta.com/horizon/blog/the-latent-power-of-prediction/)).
  - The runtime predicts the head pose; the app must render with the predicted pose of
    `xrLocateViews` at `predictedDisplayTime`.
  - Rotational timewarp corrects rotation only, not translation, animated content, or
    view-dependent shading
    ([ATW](https://developers.meta.com/horizon/blog/asynchronous-timewarp-examined/)).
  - With depth submitted, positional reprojection becomes possible
    ([KHR depth](https://github.com/KhronosGroup/OpenXR-Docs/blob/main/specification/sources/chapters/extensions/khr/khr_composition_layer_depth.adoc)).
- **A geospatial scene is the best case for reprojection.** It is static: nothing animates
  but the swaps. Motion vectors for space warp come from the camera alone (each pixel's
  world position under the previous frame's view-projection).

### 9.4 LOD at head rate

- **How fast heads turn** ([Grossman et al. 1988](https://link.springer.com/doi/10.1007/BF00247595),
  [Corbillon 2017](https://dl.acm.org/doi/pdf/10.1145/3083187.3083215)):
  - walking: a median maximum under 90°/s;
  - vigorous voluntary yaw: a median of 780°/s;
  - watching 360° video: 95 % of users move less than 90° in 2 s.
- **What a load latency sweeps.** At 300 ms of load latency, 90°/s sweeps 27° and 300°/s
  sweeps 90°.
- **The present margin.** The 25 % culling margin of a ~100° headset is ~25°: enough for
  what is resident, not for what has to be fetched.
- **The answer is a coarse sphere, always resident.**
  - Guard views: a cube of six small views around the head, at 64 pixels or so.
  - They keep a complete coarse cover of everything the user could turn to. The backdrop
    and the pinned floor already guarantee a whole-planet shell.
  - A turn then shows coarse ground at once and refines, never a hole.
- **Peripheral popping.** Flicker sensitivity is highest in the near-to-mid periphery
  ([Krajancich et al. 2021](https://arxiv.org/pdf/2104.13514)). That is where a turning head
  brings new tiles.
  - The atomic swaps (I9) avoid holes, not pops.
  - A short cross-fade at a swap, or a lower SSE in the periphery, may be needed. Both need
    measuring first.

### 9.5 SSE per eye

- **Asymmetric frusta.** Each eye has its own; `from_fov` keeps its four angles. The SSE
  denominator is the vertical tangent extent (§10.3).
- **The union, max of needs per tile.** This is what tuile already does, and what the
  reference engine does: a tile is culled only if outside every view, and refined on the
  largest SSE of any view ([selection algorithm](https://cesium.com/learn/cesium-native/ref-doc/selection-algorithm-details.html)).
- **No combined "cyclops" frustum built from min/max angles.** On headsets whose displays
  are canted, it is wrong; one engine's fix widened a culling frustum from ±47.5° to ±54.6°
  ([forum](https://forums.unrealengine.com/t/pico-openxr-fix-sps-mono-culling-frustum-incorrectly-culling-edge-objects/2763119)).
  Two views in the union need no combined frustum at all. Under multiview, the per-draw
  cull is "outside both", the same rule.
- **IPD differences.** The two eyes' SSEs differ by IPD over distance, a fraction of a
  percent at terrain distances. The union costs almost nothing over one eye (I16: one
  charge per resource).

## 10. Target architecture

### 10.1 The rule it follows

A headset is a render mode, not a product (the manifesto, PR #59). Everything a headset
needs that another mode could use goes into the shared layers: asymmetric views into the
core, N views into the renderer, the deadline into the budget, the winit-free frame step into
the shared application. What is left for the headset alone is a **shell**: the OpenXR
session, its swapchains, its frame loop, its input. The same holds for a simulator, whose
shell is the channel layout, the warp and the cluster.

### 10.2 Layers

```text
 shells        desktop · phones · browser · headset (new) · simulator (later)
               winit     winit    winit     OpenXR loop     channels, cluster
                  │         │        │            │                │
 session layer    │         │        │      tuile-xr (new)    tuile-ig (later)
                  │         │        │      instance, system,      channel geometry,
                  │         │        │      session, swapchains,   warp/blend, sync,
                  │         │        │      frame loop, actions,   CIGI (tuile-cigi)
                  │         │        │      extension discovery
                  ▼         ▼        ▼            ▼                ▼
 shared app    tuile-viewer: the frame step without winit at its centre
               (session, pump, globe, profile, lifecycle, demand); winit drives it on
               three targets, the headset loop on the fourth
                  │
 renderer      tuile-wgpu: N views (multiview, or one pass per view), view-indexed
               uniforms, reverse-Z depth, the paint order unchanged; a quad-layer
               target for the overlay
                  │
 core          tuile-core: traversal over N views (from_fov, off-axis), the budget
               (I16–I18, FrameDeadline, incremental need), f64 until rebasing
```

### 10.3 Crate by crate

**`tuile-core`** — no new dependency, wasm32 kept.
- `ViewState::from_fov(position, orientation, viewport_px, Fov { left, right, up, down })`.
  The SSE denominator becomes the vertical tangent extent, `tan(up) − tan(down)`, in place of
  `2·tan(fovy/2)`; the culling margin widens the four angles, not the SSE.
- `ViewState::off_axis(eye, screen corners, viewport_px)` for a channel: the same frustum from
  a display's geometry (§8.1).
- Views with a *role*: drawn (an eye, a channel), or **guard** — a low-resolution view that
  only keeps a coarse ring resident (a cube of six 64-pixel views around the head, or a
  view a fraction of a second ahead along a flight plan). The traversal already takes the
  union; the budget gives guard needs the Ring/Preload tier. One mechanism serves head
  rotation (§9.4), prefetch along a path (§8.8) and neighbouring channels (§8.6).
- The budget's stereo invariants and `FrameDeadline::from_period(predicted_display_period,
  share)`; the incremental need.

**`tuile-wgpu`** — no OpenXR, no winit.
- `ViewUniform` becomes `views: array<ViewUniform, N>`. Under multiview the vertex stage
  reads `views[view_index]`; with one pass per view, a per-pass index (an immediate or a
  dynamic offset). The WGSL is already assembled from parts (`ground_wgsl(slots)`); the
  multiview variant is one more part, chosen from the device's features, not a switch.
- Reverse-Z, a float depth with stencil (`Depth32FloatStencil8` where the device has it), an
  infinite far plane — the film renderer's convention, brought to the viewer path. It is also
  what the compositor's depth-based reprojection reads.
- Culling per draw against the union of the views' frusta under multiview, per view
  otherwise; one `DrawSet` for all views (I17).
- On a tiled GPU, a headset target clears on load, resolves the multisampled colour in the
  pass and **discards** the multisampled colour, the depth and the stencil
  (`surface.rs` stores the first two today). The depth handed to the compositor comes from
  the space-warp pass (§4.5), single-sample and low-resolution, since wgpu has no depth
  resolve (§3.3).
- Draw count is the headset's first limit (§4.3): multiview, render bundles for what does
  not change, and later imagery texture arrays (spec D4).
- The overlay drawn into its own small target, which a headset submits as a quad layer and
  a screen composites as today.

**`tuile-xr`** (new) — the session layer, usable by any host, the viewer or someone else's
app.
- `openxr` (Rust bindings, MIT OR Apache-2.0) over the Khronos loader (Apache-2.0); `ash` and
  `wgpu-hal` for the interop; nothing proprietary.
- Graphics: Vulkan through `XR_KHR_vulkan_enable2` — the runtime creates the `VkInstance`
  and `VkDevice` with wgpu-hal's own extension lists, and wgpu wraps them (`from_raw`,
  `device_from_raw`, `from_hal`, `create_device_from_hal`); swapchain images wrapped with
  `texture_from_raw` + `create_texture_from_hal`. D3D12 only if a Windows runtime ever
  requires it.
- The frame loop (`xrWaitFrame` → `xrBeginFrame` → `xrLocateViews` → render → `xrEndFrame`),
  session states mapped onto the viewer's lifecycle steps, the action set (controllers and
  hands), reference spaces.
- Extensions discovered at run time and named by what they do (`foveation`, `refresh`,
  `passthrough`, `depth`, `space_warp`), never by their vendor prefix in a public name.

**`tuile-viewer`** — an `xr` shell beside `native/` and `web/`, behind a build feature.
- The frame core lifted out of the winit `ApplicationHandler` so that both drivers call it.
- An `XrNavigation` beside `tuile-camera`'s `CameraController`: the anchor (f64 ECEF point,
  local east-north-up frame, heading, world scale) moves only on the user's input; the head
  pose rides on it.
- `Profile::headset`: budget fractions, upload share, refresh preference.
- The Android packaging grows a headset variant of the manifest and the loader library;
  the same `build.sh`.

**What stays per platform**: the loader and manifest (Android headsets), the graphics binding
(Vulkan everywhere OpenXR runs; Metal and Compositor Services on visionOS, §6), the
extension set, the store packaging.

### 10.4 Dependency rules added

- `tuile-core` and `tuile-wgpu` never depend on `openxr`, `ash` or any runtime.
- Only `tuile-xr` touches `wgpu-hal`; a wgpu upgrade is checked there first (the raw APIs are
  `unsafe` and move between majors).
- No proprietary SDK in any build path. Vendor extensions are used only through the
  registry's definitions in `openxr-sys`, behind a capability check, with the plain path
  working without them.
- No third-party brand in a crate, module or public type: `tuile-xr`, not a headset's name.
  OpenXR, Vulkan, WebXR and CIGI are open standards, named as such.

## 11. Roadmap

### 11.1 Quest first, or PC VR first

**Recommended: Quest first for the product, a desktop OpenXR runtime first for the
development loop and CI.**

**For Quest first:**

1. **It is where the users are and where the constraints are.** Standalone, tiled GPU,
   5.75 GiB, thermal levels, 200–300 draws a frame. The budget work exists to make tuile hold
   on constrained devices. Sizing the headset profile on the hardest target first means PC VR
   later only relaxes numbers.
2. **The shell is already there.** A Quest is an Android device, and the APK pipeline
   (`build.sh`, NativeActivity, `cargo ndk`, no Gradle) runs from the Mac today. The
   headset variant is a manifest, a loader library and a session crate.
3. **The development machine is a Mac.** It has no PC VR runtime. PC VR first would mean a
   Windows or Linux machine with a GPU and a tethered headset before writing a line. The
   Mac does have an OpenXR runtime for development, the Meta XR Simulator (Metal and Vulkan,
   proprietary, a test tool only), and the Quest itself is one `adb install` away.
4. **One code path covers both.** Vulkan through `XR_KHR_vulkan_enable2` is what Quest,
   Android XR, Pico, SteamVR, Meta's PC runtime and Monado all take (§5.3). PC VR is the
   same `tuile-xr` on another runtime, with SteamVR's API-version pin.
5. **CI is open source either way.** Monado over lavapipe on a Linux runner exercises the
   same session code without any headset (§12).

**Against, honestly:**

- Debugging on the device is slower than on a desktop. RenderDoc's Meta fork, Perfetto and
  OVR Metrics help, but a desktop debugger does not reach a Quest.
- Validation layers break OpenXR device creation on Quest (#8801).

The desktop runtime loop (Monado on Linux, the simulator on the Mac) is what keeps the
iteration fast. It is not an alternative to Quest; it is the bench Quest work is done on.

### 11.2 Phases

Each phase ends on acceptance criteria that are seen, not counted: the headset run is
recorded and the recording opened, as every render is.

**X0 — Shared foundations, no headset.** Everything here serves every mode, and lands
before any headset code. It fits Phase 3 and 4 of the budget plan (P6 stereo is already in
it).

- Core:
  - `ViewState::from_fov`, `ViewState::off_axis`, guard views (§10.3);
  - a property test: `from_fov` with symmetric angles selects exactly what `perspective`
    selects.
- Budget:
  - I16–I18;
  - `FrameDeadline::from_period`;
  - the incremental need;
  - P6 scenarios driven by recorded head traces at 72, 90 and 120 Hz, including 300°/s yaw.
- `tuile-wgpu`:
  - view-indexed uniforms;
  - reverse-Z float depth in the viewer path;
  - discarded multisample stores where a target allows it.
- Shared app: the frame step callable without winit.
- Headless harness:
  - N eyes, with the black-pixel scan per eye;
  - the stereo tour recorded and opened.

Acceptance:
- zero black pixels in either eye over the head-trace tour;
- film goldens unchanged;
- the desktop, phone and web viewers unchanged by eye.

**X1 — The globe in a headset (Quest 3, 72 Hz).**
- `tuile-xr`: Vulkan enable2, the swapchains, the frame loop, the session states, the
  actions.
- The viewer's `xr` shell: two passes, `local-floor`, the anchor, thumbstick flight at a
  fixed speed under a vignette, snap turns, a teleport to a place.
- A quad layer for the controls and **the attribution**.
- `Profile::headset`.
- The APK variant: manifest, loader, development `targetSdk`.

Acceptance:
- a 10-minute tour on the device at ≥ 72 Hz with no dropped frame in the runtime's
  metrics;
- no black ground in either eye (the recording opened);
- memory under the profile's hard limit;
- attribution readable;
- the same session green on Monado in CI.

**X2 — Comfort and performance.**
- Multiview, checked pixel-identical against two passes in the headless harness.
- 90 Hz, then 120 Hz.
- Performance levels.
- Depth submission.
- Space warp.
- The tabletop scale and passthrough.
- Hand input.
- The draw-count work of §4.3.
- Foveation if D-X5 decides for the fork.

Acceptance:
- 90 Hz sustained over the tour at GPU level 4;
- the space-warp tour at half rate without visible artefacts on the swaps.

**X3 — The other runtimes.**
- SteamVR and Meta's PC runtime on Windows; Monado and WiVRn on Linux.
- Android XR (manifest variant, `ALPHA_BLEND` passthrough).
- Pico (loader question).
- D3D12 only if a runtime requires it.

Acceptance: the X1 tour on each.

**X4 — Simulators.**
- Channels from display geometry.
- N channels per process, one budget per GPU.
- The warp and blend pass (MPCDI).
- Cluster consistency: broadcast relaxation, seam commit, guard views of the neighbours.
- A software swap barrier first, hardware present barrier after.
- `tuile-cigi` with height queries.
- Guard views along a planned path.

Acceptance:
- a three-channel cockpit at 60 Hz, frame-locked;
- no seam in the blend zones over a recorded flight (the recording opened);
- HAT/HOT answered within the frame.

**X5 — Vision Pro and WebXR.**
- The visionOS shell: Swift, `cp_*`, wgpu's Metal textures wrapped, the `layered` layout as
  multiview, foveation off.
- WebXR in the web shell: WebGL2 with two passes first; the WebGPU binding once wgpu can
  ask for an XR-compatible adapter.
- The RealityKit façade (`tuile-swift`, roadmap M4) for volumes, on its own schedule.

## 12. Testing

| Level | What | Where | Notes |
|---|---|---|---|
| L1–L3 | `from_fov`, `off_axis`, guard views; budget P6 with recorded head traces (I16–I18) | CI, every PR | pure, deterministic; the sim's goldens per pattern |
| L4 headless stereo | the headless viewer with N eyes: black scan per eye, multiview and two passes pixel-identical, the deadline respected | Mac (Metal), Linux CI (lavapipe, which has Vulkan 1.3 and therefore multiview) | no OpenXR involved; tests the engine's stereo path |
| L4b OpenXR session | `tuile-xr` against Monado: instance, device sharing, swapchains, frame loop, session states; images read back by the app from its own swapchain before release | GitHub ubuntu runner without GPU: Monado over lavapipe (XCB under Xvfb, or the null compositor) | shown by others, see below |
| L5 desktop runtimes | the shell on the Meta XR Simulator (Mac) and SteamVR's null driver (Windows) | the developer's machines | proprietary tools, test only, never linked |
| L5 device | Quest 3 over adb: install, start, logcat, runtime metrics CSV, Perfetto; RenderDoc's Meta fork for captures | the device | Android XR and Pico likewise; their emulator does not run OpenXR apps |

### Monado in CI

- **Version and licence.** Monado is at v25.1.0 (2025-12-09), Boost licence; Ubuntu 26.04
  packages 25.0.0 ([tags](https://gitlab.freedesktop.org/api/v4/projects/monado%2Fmonado/repository/tags),
  [Repology](https://repology.org/project/monado/versions)).
- **The null compositor** (`XRT_COMPOSITOR_NULL=1`, with `XRT_NO_STDIN=1`):
  - runs a full session with no display;
  - paces at `XRT_COMPOSITOR_NULL_FPS`, default 20;
  - still needs Vulkan with external memory, fences and semaphores
    ([null_compositor.c](https://gitlab.freedesktop.org/monado/monado/-/blob/main/src/xrt/compositor/null/null_compositor.c)).
- **Who runs it already.**
  - WebKit runs its OpenXR tests against it
    ([monadodriver.py](https://github.com/WebKit/WebKit/blob/main/Tools/Scripts/webkitpy/port/monadodriver.py)).
  - yarp-device-openxrheadset runs green on a GitHub ubuntu runner without a GPU, with
    lavapipe and XCB under Xvfb (2026-09-16)
    ([ci.yml](https://github.com/gbionics/yarp-device-openxrheadset/blob/master/.github/workflows/ci.yml)).
- **Poses.** The simulated head wobbles with time by default (stationary and rotating modes
  exist) ([simulated_hmd.c](https://gitlab.freedesktop.org/monado/monado/-/blob/main/src/xrt/drivers/simulated/simulated_hmd.c)).
  - Deterministic pixel tests take their poses from the test, not from the runtime: the
    session test checks the plumbing, the headless harness checks the pixels.
- **Validation layers.** Khronos's core validation and best-practices layers (SDK 1.1.65)
  check the call order and that the views located match the views submitted
  ([Khronos blog](https://www.khronos.org/blog/new-openxr-validation-layer-helps-developers-build-robustly-portable-xr-applications)).
  They run in the Monado job.
- **Conformance.** The OpenXR CTS (1.1.63.0) tests runtimes, not applications
  ([CTS](https://github.com/KhronosGroup/OpenXR-CTS/releases)).

### Elsewhere

- **The Meta XR Simulator** (207.1, 2026-10-14):
  - Windows and Apple-silicon macOS;
  - native OpenXR apps, recordable sessions;
  - under Meta's SDK licence
    ([intro](https://developers.meta.com/horizon/documentation/native/xrsim-intro/),
    [download](https://developers.meta.com/horizon/downloads/package/meta-xr-simulator-windows/)).
  - Whether it runs on hosted CI runners is unverified.
- **SteamVR's null driver**
  ([setup](https://github.com/username223/SteamVRNoHeadset),
  [issue #889](https://github.com/ValveSoftware/SteamVR-for-Linux/issues/889)):
  - opens a window showing the headset's view on Windows;
  - fails headless on Linux.
- **On a Quest** ([adb](https://developers.meta.com/horizon/documentation/native/android/ts-adb/),
  [OVR Metrics](https://developers.meta.com/horizon/documentation/unity/ts-ovrmetricstool/),
  [Perfetto](https://developers.meta.com/horizon/documentation/unreal/ts-perfettoguide/),
  [RenderDoc fork](https://developers.meta.com/horizon/downloads/package/renderdoc-oculus/)):
  - `adb install -r`, `adb shell am start -n …`;
  - the metrics tool's CSV;
  - Perfetto, on by default in developer mode;
  - RenderDoc's Meta fork, 68.18, for development builds.
- **The budget plan's L5 for the Android emulator carries over.** A Quest is an adb target
  like any Android device: the same scripts, plus the stereo profile and the metrics CSV.

## 13. Risks and unknowns

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| **No foveation in wgpu** (density map, rate map) | certain | GPU headroom on Quest, Android XR, Pico, Vision Pro | ship without; measure; a contained wgpu-hal fork (D-X5); Meta's open freeze report with density maps at the high level argues for waiting |
| **Draw calls over the Quest budget** (~500 tiles × passes × eyes) | high | CPU-bound frames below 72 Hz | multiview first; headset SSE; render bundles; imagery texture arrays (D4); measure in L4 |
| **`decide` freshness at 90 Hz** (O(view · log N), 6.9 ms p99 in a zoom) | high | selection lagging the head | the incremental need (I6) as a P6 requirement; guard views so a late decision is coarse, not empty |
| **Vulkan version mismatch** (#10564) | certain on Quest | device creation | instance at 1.3, as others do; carry a patch if the runtime refuses |
| **Indirect draws broken on Adreno** (#8801) | known | blocks GPU-driven drawing | stay on direct draws; test on device before any change |
| **Tiled-GPU bandwidth** (multisample and depth stored today) | certain | fill-rate and power | discard stores in the headset target; no post pass; depth for the compositor from the space-warp pass |
| **No depth resolve in wgpu** | certain | depth submission with MSAA | a separate low-resolution pass (space warp's), or no depth layer at first |
| **Multiview edge bugs** (#9620, #8333, #8509) | medium | wrong layers | two views, full mask, Vulkan; pixel equality with two passes in L4 |
| **wgpu major-version churn** in the raw APIs (30's `initial_state`) | certain | upgrade work | all raw interop in `tuile-xr`; upgrade checked there first |
| **SteamVR at OpenXR 1.0** | certain | API version | request 1.0 when the runtime says so |
| **Store policy** (`targetSdk` 34, VRCs, account verification) | medium | publication | sideloaded development builds first; a store manifest variant later (D-X7) |
| **Pico loader** | unknown | packaging | check on a device in X3 |
| **One APK for two stores** | unknown | packaging | one manifest per store from the bundler |
| **visionOS path unproven with wgpu** | medium | X5 schedule | the C API, Godot's module as a template, threers' loop; RemoteImmersiveSpace as a stopgap |
| **WebXR: no `xrCompatible`, GL multiview count 0** | certain | X5 web | a small wgpu patch, or the WebGL2 path with two passes |
| **Peripheral popping at head rate** | medium | comfort | guard views; measure flicker on swaps; a short cross-fade only if measured necessary |
| **Comfort of navigation** (glides, accelerations) | certain if ported as is | sickness | anchor moves only on input; fades and snaps; no world rotation the user does not drive |
| **Cluster seams in simulators** | high without design | visible discontinuities | one process per GPU with all its channels; broadcast relaxation; seam commit |
| **Present barrier vendor-specific, outside wgpu** | certain | hardware framelock | software barrier first; raw Vulkan present for the barrier later |
| **Licences** | low | distribution | Khronos loader Apache-2.0 (NOTICE entry); openxr MIT/Apache; Monado Boost (test only); the Meta simulator, RenderDoc fork and SteamVR proprietary, never linked; the CIGI class library LGPL, not used: CIGI written from the standard |
| **Imagery terms in a headset** | open | publication | attribution in a quad layer always visible; the licence question of a commercial imagery layer stays the one already noted for any deployed layer |

## 14. Decisions for Laurent

| | Question | Recommended |
|---|---|---|
| **D-X1** | Order: Quest first (product) on a desktop-runtime development loop, or PC VR first? | Quest first, Monado and the Mac simulator as the bench (§11.1) |
| **D-X2** | Layout: a session crate `tuile-xr` (OpenXR, Vulkan interop) plus an `xr` shell in `tuile-viewer`, with the frame step lifted out of the winit handler? | Yes |
| **D-X3** | Stereo path: view-indexed uniforms with two passes for X1, multiview in X2 proven pixel-identical; or multiview from the first build? | Two passes first: they also serve simulator channels of different sizes, WebGPU and WebGL2 |
| **D-X4** | Depth: reverse-Z with a float depth-stencil in the shared viewer path for every mode, not only the headset? | Yes: one path, no switch; it is the film's convention and Vision Pro's requirement |
| **D-X5** | Foveation: a lapoule-dev fork of wgpu-hal for the density-map attachment (Vulkan), or none until measured? | None in X1; decide on device measurements in X2 |
| **D-X6** | Guard views in the core (a coarse ring around the head, neighbours' channels, a path ahead) as one mechanism? | Yes |
| **D-X7** | Distribution: sideloaded development builds only, or the Horizon Store (account verification, VRCs, `targetSdk` 34) from X1? | Sideload in X1; store after X2 |
| **D-X8** | Simulators: a goal for tuile (an IG with CIGI and multi-node consistency), or multi-channel displays only? | Multi-channel in one process first; the IG with CIGI as X4's second half, once a user asks |
| **D-X9** | Height queries for a simulator host: from the drawn LOD (correlated with what is seen) or from the finest data? | From the drawn LOD, with the rule declared |
| **D-X10** | Vision Pro: Compositor Services with the wgpu renderer for immersion, and the RealityKit façade for volumes, in that order? | Yes |
| **D-X11** | WebXR: a wgpu patch for an XR-compatible adapter (WebGPU binding), or WebGL2 with two passes only? | WebGL2 first; the patch when the binding ships beyond Safari |

Beside these, the budget's own open decisions bear on the headset (`graphics-budget-architecture.md` §6):
- **D-A1** (no bare floor mesh at start-up): a bare mesh in a headset is a hole under the
  feet.
- **D-A3** (the per-class split): the headset's imagery share is the larger, at a long
  view distance.
- **D-A7 and D-A8** (`from_fov`, `FrameDeadline`): taken here as decided.

## Primary references

- OpenXR 1.1 specification and registry: <https://registry.khronos.org/OpenXR/specs/1.1/html/xrspec.html>;
  the runtime inventory: <https://github.com/KhronosGroup/OpenXR-Inventory>.
- `openxr` crate: <https://github.com/Ralith/openxrs>; wgpu: <https://github.com/gfx-rs/wgpu>
  (29.0.3 sources locked by this workspace).
- Meta Horizon developer documentation (native Android): <https://developers.meta.com/horizon/documentation/native/android/mobile-openxr/>.
- Android XR OpenXR: <https://developer.android.com/develop/xr/openxr>.
- Apple Compositor Services: <https://developer.apple.com/documentation/compositorservices>.
- WebXR Device API: <https://www.w3.org/TR/webxr/>; WebXR/WebGPU binding:
  <https://immersive-web.github.io/webxr-webgpu-binding/>.
- CIGI: <https://cigi.sourceforge.io/files/CIGI_ICD_3.pdf> (3.0 ICD; 4.0 is SISO-STD-013-2014);
  OGC CDB: <https://www.ogc.org/standards/cdb/>; CDB X: <https://docs.ogc.org/dp/20-092.html>.
- 14 CFR Part 60, Appendix A: <https://www.govinfo.gov/content/pkg/CFR-2024-title14-vol2/pdf/CFR-2024-title14-vol2-part60-appA.pdf>.
- Monado: <https://monado.freedesktop.org/>.
