<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The viewer on Android

The same application as the desktop's, the phone's and the page's —
`tuile-viewer`, hosted by `examples/wgpu-viewer` — built for Android: the
fourth shell of one core (see `docs/explorations/web-native-convergence.md`).
What differs is a [`Profile`](../src/profile.rs) of numbers, the way the
system's signals are read, and a handful of platform calls; the frame step,
the gestures, the loading indicator, the lifecycle's steps, the renderer and
the controls are the shared ones.

```text
scripts/try-viewer.sh android         # emulator (or the phone on the cable): build, install,
                                      # start the catalog proxy here, launch, follow the log
scripts/try-viewer.sh android --gl    # … forcing OpenGL ES
crates/tuile-viewer/android/build.sh  # the APK alone → build/Tuile.apk
```

## What it needs

| what | why | size on disk |
|---|---|---|
| `cmdline-tools;latest` | `sdkmanager`, `avdmanager` | 0.17 GB |
| `platform-tools` | `adb` | 0.04 GB |
| `platforms;android-35` | `android.jar`, to compile the activity and link the resources | 0.13 GB |
| `build-tools;35.0.0` | `aapt2`, `d8` and R8, `zipalign`, `apksigner` | 0.19 GB |
| `ndk;27.2.12479018` | the C toolchain and linker for `aarch64-linux-android` | 2.5 GB |
| `emulator` + `system-images;android-35;default;arm64-v8a` | only to try it without a phone | 1.3 + 1.7 GB, and ~0.8 GB for the AVD |
| a JDK 17+ | `javac`, `keytool`, `d8` | (already there) |
| Kotlin's standard library 2.0.21 | what the TLS verifier's Kotlin half calls; fetched from Maven Central by `build.sh`, checked against its SHA-256, kept in `~/.cache/tuile` | 1.7 MB (a few KB after R8) |
| `cargo install cargo-ndk`, `rustup target add aarch64-linux-android` | the Rust side | small |

The first `sdkmanager` comes from Homebrew (`brew install --cask
android-commandlinetools`) or Google's zip; it installs `cmdline-tools;latest`
into the SDK, after which the Homebrew copy can go. `sdkmanager --licenses`
accepts the SDK's licences once.

```text
sdkmanager --sdk_root="$ANDROID_HOME" "cmdline-tools;latest" "platform-tools" \
    "platforms;android-35" "build-tools;35.0.0" "ndk;27.2.12479018"
sdkmanager "emulator" "system-images;android-35;default;arm64-v8a"
avdmanager create avd -n tuile-arm64 -k "system-images;android-35;default;arm64-v8a" -d pixel_6
# then, in ~/.android/avd/tuile-arm64.avd/config.ini:
#   hw.gpu.enabled=yes  hw.gpu.mode=host  hw.ramSize=4096M
```

The AOSP image (`default`), not the Google APIs or Play Store ones: the
viewer needs none of Google's services, and the image is the smallest.

## How it is built, and why this way

| step | tool | what it does |
|---|---|---|
| resources | `tuile-viewer-bundle android` | `AndroidManifest.xml`, the launch theme (the globe's blue), the launcher icon at every density — written and tested in Rust |
| native library | `cargo ndk … rustc --crate-type cdylib` | the host's crate as `libtuile_wgpu_viewer.so`, whose `android_main` the activity calls |
| code | `javac`, R8 | `TuileActivity`, the TLS verifier's Kotlin half and the few Kotlin classes it reaches, shrunk into one `classes.dex` (~20 KB) |
| package | `aapt2`, `zip`, `zipalign -P 16`, `apksigner` | linked, the library stored uncompressed on 16 KiB pages, signed with the SDK's debug key |

- **winit's Android backend over `android-activity`'s `NativeActivity`**: the
  application is the same winit `ApplicationHandler` as everywhere else. The
  native activity is part of the platform, so the APK needs no Java library
  of its own beyond one small subclass. (`GameActivity` would bring a Jetpack
  library and Gradle with it, for text input this viewer does not have.)
- **`cargo-ndk`**, not `cargo-apk` or `xbuild`: `cargo-apk` is deprecated by
  its own project, `xbuild` is pre-1.0 and unmaintained. `cargo-ndk` only
  points cargo at the NDK's linker and is what most Rust-on-Android projects
  build with.
- **No Gradle.** The application is a native library, one activity and a
  manifest: five SDK tools in a 100-line script build it reproducibly from the
  command line, where Gradle would download its own distribution and the
  Android plugin (several hundred MB) to do the same.
- **`TuileActivity`** (`java/`, 200 lines) subclasses `NativeActivity` for what a
  native thread cannot do alone on Android: receive `onTrimMemory`, post a view
  (the caption, an alert) to the UI thread, and ask the power manager and the
  clipboard. It decides nothing — every policy is in Rust and tested on the host
  (`native/android_signals.rs`, `app/lifecycle.rs`). No native method is
  registered, so the library exports nothing but `android_main`.

## The access token: none in the APK

**A tester's own token, from a configuration file.** Tapping a `.tuile`
file, sharing one to *Tuile*, or following a `tuile://import?src=https://…`
link imports it (see [Try tuile](../../../docs/try-tuile.md)): the activity
receives it (`onCreate`, `onNewIntent`) and the viewer takes it once a
second. The token is sealed with AES-GCM under a key that lives in the
Android Keystore and never leaves it (`sealSecret`, `openSecret`), and with
it the application asks the service directly. Without one, it asks the
catalog proxy as below. Not exercised on a device yet: the intent filters,
the Keystore round trip and the share sheet are written and compiled, not
run.

The Android build holds **no secret**. The catalog is asked through the
catalog proxy (`tuile-catalog-proxy`, `tuile_cesium_ion::proxy`), which holds
the token on a server — the same proxy the web build uses, for the same reason:
anything inside an APK can be unpacked by whoever has it.

- `scripts/try-viewer.sh android` starts the proxy on this machine with the
  token from `.env`, and `adb reverse tcp:8091 tcp:8091` makes it the device's
  own `127.0.0.1:8091`, for an emulator and a phone on a cable alike.
- The proxy's address is `TUILE_CATALOG_URL`: a launch setting, else the
  value set when the APK was built (`TUILE_CATALOG_URL=https://… build.sh`),
  else `http://127.0.0.1:8091/catalog/`. An address, not a secret.
- A launch setting named like a credential (`…TOKEN…`, `…KEY…`, `…SECRET…`)
  is refused and named in the log; the host on Android never reads a token at
  all. There is deliberately no developer-only token injection: the proxy
  costs one command and leaves nothing to leak.

## Launch settings

What a simulator's `SIMCTL_CHILD_` variables are to iOS, the launch intent's
string extras are here — the viewer's knobs (`TUILE_…`, `RUST_LOG`,
`RUST_BACKTRACE`), set in the process's environment before the session starts:

```text
adb shell am start -S -n dev.lapoule.tuile.viewer/dev.lapoule.tuile.TuileActivity \
    -e RUST_LOG info -e TUILE_BACKEND gl -e TUILE_PIN_LEVEL 3
```

The log is logcat, under the tag `tuile`: `adb logcat -s tuile`.

## What it does on Android

- **Launch**: the window opens at once on the launch theme's blue — the
  globe's shell's — and the loading indicator turns while the session
  connects on the async runtime. The event loop runs on `android-activity`'s
  own thread, never the system's UI thread, and never waits on the network:
  no "application not responding".
- **GPU**: Vulkan first; if the device's Vulkan driver gives no surface,
  adapter or device, OpenGL ES 3 in an instance of its own
  (`app::setup::attempts`). `TUILE_BACKEND=gl` forces it. The viewer path
  has no compute pipeline, so the WebGL2 constraints already make it fit
  OpenGL ES. Where the window has no sRGB colour space (no
  `EGL_KHR_gl_colorspace`: the emulator, older devices) and so neither an
  sRGB surface nor an sRGB view of one, the frame is resolved into an sRGB
  texture and encoded into the surface by one full-screen pass
  (`tuile_wgpu::SurfaceFormat::encode`); the adapter line and
  "the frame is encoded by a pass" say which path a session took.
- **Edge to edge**: from Android 15 the window is drawn under the status
  and navigation bars. The activity reports the bars' and the cutout's
  insets (`safeInsets`), and the controls keep inside them, clear of the
  gesture bar (`android_signals::safe_area_of_insets`).
- **Lifecycle**: Android destroys the window's surface whenever the activity
  leaves the front (winit's `suspended`). Before that call returns, loads are
  held at the gate, the residency is trimmed to a quarter, the tile store is
  written down and closed, and the surface and render targets are dropped —
  no GPU work in the background. On `resumed`: the surface first, then the
  store, then the loads, then a frame (`Lifecycle::surface_lost`).
- **Memory**: the profile is the phone's, sized from `MemAvailable` in
  `/proc/meminfo` — Android has no per-process limit for native memory, only
  what the low-memory killer leaves. `onTrimMemory` is counted by the
  activity and read once a second, in front or away: the running levels
  moderate and low trim the residency to half, critical (and the background's
  moderate and complete) to a quarter (`android_signals::pressure_of_trim`);
  `onLowMemory` arrives as winit's memory warning, trimming to half. The
  view's tiles and their ancestors are never let go of: the ground stays
  covered.
  To try it: `adb shell am send-trim-memory dev.lapoule.tuile.viewer RUNNING_LOW`
  (then `RUNNING_CRITICAL`: the system refuses a level lower than the last),
  with `-e RUST_LOG warn,tuile_viewer=info,tuile_core::runtime=info` at
  launch; the log says `residency trimmed to the budget budget_mib=…` and the
  server's `trimmed the residency released=… kept=…`.
- **Tile store** in the application's cache directory (`getCacheDir`), which
  the system may clear; a damaged one is started afresh. The HTTP cache is
  beside it.
- **Energy**: the phone's pacing — frames only while something changes, 30
  frames a second at the "moderate" thermal status or with the battery saver
  on, 20 at "severe" and above (`PowerManager`).
- **Touch**: the shared touch module, as on iOS and in a page.
- **Attribution and notices**: a caption over the bottom of the view, drawn
  by the activity in a popup window that takes no touch. **Alerts** ("cannot
  start yet", "no GPU") are the system's dialogs.
- **Copy link / go to copied link**: the system's clipboard.

## Parity

Done: the same, from the shared code. Adapted: the same function, in the
platform's own terms. N/A: why it has no place there.

| Feature | Desktop (macOS) | Phone (iOS) | Web | Android |
|---|---|---|---|---|
| Terrain and imagery mosaic, aerial perspective | done | done | done (WebGPU, or WebGL2) | done, same shaders (Vulkan, or OpenGL ES) |
| Never-black ground: shell, ancestors, new before old | done | done | done | done, same pump; the launch theme is the shell's blue |
| On-screen controls | done | done | done | done, same `tuile-ui` widget at the display's scale |
| Touch: pan, pinch, twist, tilt, double tap, hold | N/A | done | done | done, same `touch` module |
| Loading indicator at launch | holds its window until warm | done | done | done |
| Attribution and notices | title bar | caption label | caption over the canvas | adapted: caption in a popup over the view |
| Alerts | NSAlert | UIAlertController | in-page | adapted: AlertDialog |
| Session retries with back-off | N/A | done | done | done, same `Retries` |
| Lifecycle: no GPU work away, surface rebuilt | occlusion | done | hidden tab | adapted: surface destroyed by the system, released before `suspended` returns |
| Memory pressure | N/A | memory warning | N/A | adapted: `onTrimMemory` levels, `onLowMemory` |
| Profile from the device's memory and temperature | N/A | `os_proc_available_memory`, thermal state | device memory | adapted: `MemAvailable`, `PowerManager` thermal status |
| Persistent tile store | foyer | foyer, Caches | in memory | foyer, cache directory |
| Credentials | token in its environment or file | settings, bundle or file | none: catalog proxy | none: catalog proxy |
| Copy link / go to copied link | done | done | done | done, the system clipboard |
| Current location | Core Location | Core Location | Geolocation | not yet: needs the runtime permission flow |
| Menus, keys | menu bar, keys | N/A | button menu, keys | N/A, no menu bar |
| Steering URLs | done | N/A | the JS API | not yet: an intent filter for `tuile://` |
| Tapes, traces | done | done | N/A | through launch settings, into the app's files |
