# The viewer on a phone

The same application as the desktop one — `tuile-viewer`, hosted by
`examples/wgpu-viewer` — built for iOS. One program, one library: what differs
is a [`Profile`](../src/profile.rs) of numbers and a handful of platform calls.

```text
xcrun simctl boot "iPhone 16 Pro"                # once: a booted simulator
./build.sh simulator --run                       # build, install on the booted simulator, launch
TUILE_TEAM=ABCDE12345 ./build.sh device --install # build signed, install on the connected iPhone
TUILE_DEVICE="my phone" TUILE_TEAM=… ./build.sh device --install  # one of several
```

Needs Xcode, `xcodegen` (`brew install xcodegen`) and the two Rust targets
(`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`).

## How it is built

| step | tool | what it does |
|---|---|---|
| resources | `tuile-viewer-bundle phone` | `Generated/Info.plist` and the asset catalogue (icon, launch colour), written and tested in Rust |
| project | `xcodegen` | `Tuile.xcodeproj` from `project.yml` — generated, never committed |
| bundle | `xcodebuild` | runs `cargo build --target aarch64-apple-ios[-sim]` for the executable, compiles the assets, embeds a provisioning profile and signs |

Why this and not another tool: the executable is an ordinary cargo binary
(winit runs `UIApplicationMain` itself), so nothing needs to generate Rust or
wrap it in a static library — which is most of what `cargo-mobile2` does, along
with templates this workspace would then carry. What cannot be done without
Apple's tools is signing for a device: a provisioning profile is created by
Xcode's automatic signing, and `xcodebuild -allowProvisioningUpdates` is the
command-line door to it. `xcodegen` keeps that project a 60-line YAML file
instead of a committed `.pbxproj`.

## The access token

**The one-step way: open a configuration file.** Tap a `.tuile` file in
Files, Mail, Messages or a download, or AirDrop it, and pick *Tuile*; or
follow a `tuile://import?src=https://…` link. The token goes to the
Keychain, the rest to the application's settings, and a waiting start-up
begins at once — see [Try tuile](../../../docs/try-tuile.md). The bundle
declares the type and the scheme, and names the application class that
receives them (`NSPrincipalClass`, `TuileApplication`). The Keychain comes
second, after the environment and before everything below.

The host says which credential its sources need — the public viewer's is in
`examples/wgpu-viewer/phone.conf`: the variable's name, which is also the key
of the field on the settings page, and that field's title. At run time the
token is looked for in this order, first found wins:

1. the environment variable (a simulator launched with
   `SIMCTL_CHILD_<VARIABLE>=…`);
2. the secure field in **Settings › Tuile**, typed by the person — it wins
   over everything below, so a token pasted there replaces a built-in one;
3. a token **built into the bundle**;
4. the file `token` in the application's **Documents** — Files › On My
   iPhone › Tuile — on a line of its own;
5. otherwise a message that says where to put one, and a new attempt every
   time the application comes back to the front (and every 2 s to 1 min).

**A device build embeds the token by default**: `build.sh device` reads
`TUILE_TILES_TOKEN` (or the host's older name for it, `TUILE_ION_TOKEN` for the
public viewer) from the environment, else from the workspace's `.env` — a
worktree's main checkout's, failing that — and writes it into the generated,
gitignored `Info.plist`. `TUILE_TILES_TOKEN=none` builds without one. It is
never printed, never a command-line argument, and kept out of xcodebuild's
environment, which xcodebuild writes into its log.

> **The developer's own device only.** Anything in a bundle can be read back
> out of the `.app`. A build for the App Store, TestFlight or anyone else must
> not embed a token: build it with `TUILE_TILES_TOKEN=none` and let the person
> type theirs in Settings.

## Imagery compression

Off by default. **Settings › Tuile › Imagery compression** — Off, Auto, BC7,
ASTC 4x4 — is read at launch: Auto holds the imagery as ASTC 4×4 on an
iPhone, a quarter of the memory, encoded on the GPU as each tile is
uploaded. A launch argument wins over the page:
`../../../scripts/try-viewer.sh ios --imagery-compression auto` on the
simulator, `xcrun devicectl device process launch --device <id>
dev.lapoule.tuile.viewer --imagery-compression auto` on a phone, or the
scheme's *Arguments Passed On Launch* in Xcode. The caption at the bottom
says what is held: `imagery ASTC 4x4 23 MiB (276 × 85 KiB)`.

## What it does differently on a phone

- **Launch**: the window opens at once on the globe's shell — the launch
  screen is the same blue, so there is no step — with a loading indicator
  while the session connects and the coarse pyramid arrives, all of it on the
  async runtime. The main thread never waits on the network
  (`launch::tests::polling_a_stalled_launch_never_waits`,
  `app::stall`).
- **Memory**: the resident budget is a fifth of what the system leaves the
  process (`os_proc_available_memory`), between 160 and 768 MiB; a memory
  warning trims residency to half, the background to a quarter — never the
  tiles the view draws or their ancestors. The tile store lives in
  `Library/Caches` — the directory the system may purge — with a 768 MiB
  disk cap. If it does not open because it is damaged (purged half-way, a
  file where a directory should be) it is started afresh; a passing
  condition (descriptors, a lock, a full disk) never clears it, and on a
  desktop nothing ever does.
- **Threads**: two runtime workers, four blocking threads, one to three
  decoders, sixteen fetches in flight over one HTTP/2 connection, and an
  upload budget per frame of four tiles or 6 ms.
- **Lifecycle**: no frame and no GPU work outside the foreground; loads held
  at a gate while away; the surface and render targets released in the
  background and rebuilt before the first frame back (again on every turn
  until that succeeds). The tile store is written down and closed on the
  way into the background and opened again on the way back: a phone ends a
  background application without a word, and a store that is never closed
  keeps nothing of its memory tier for the next launch. The background trim's evictions are taken in the
  background too, without touching the GPU, so the memory is back while
  the system weighs the application.
- **Start-up**: given up after 20 s, then tried again after 2, 4, 8… s up to
  a minute, and at once on coming back to the front; the caption says so,
  and a launch that waits is drawn at 15 frames a second.
- **Energy**: frames are drawn only while something changes — a gesture, an
  ease, an upload, the indicator — and four times a second otherwise; 30 fps
  when the device runs hot or saves power, 20 when critical.
- **Touch**: one finger drags the ground, two pinch and twist, two sliding
  together tilt, a double tap zooms in, a finger held still centres on the
  current location. The compass and zoom buttons sit in the safe area at the
  display's scale.

`TUILE_TOUCH_SCRIPT=tour` plays a fixed set of gestures through the same path
once the first view is covered, for a film that can be made again.
