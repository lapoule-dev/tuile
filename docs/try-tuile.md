<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Try tuile

Tuile's viewer is a globe of terrain and aerial imagery that streams as you
fly. It runs on your Mac, iPhone, iPad or Android phone. The terrain and
imagery come from Cesium ion, through **your own** free account. The
application ships with no access token; you give it yours, once, with one
small file.

Four steps:

1. [Download the application](#1-download-the-application).
2. [Get your own Cesium ion token](#2-get-your-own-cesium-ion-token).
3. [Put the token in a configuration file](#3-put-the-token-in-a-configuration-file).
4. [Open the file in the application](#4-open-the-file-in-the-application).

## 1. Download the application

Every release is on the
[releases page](https://github.com/lapoule-dev/tuile/releases).

| Device | What to get | How |
|---|---|---|
| **Mac** (Apple silicon or Intel, macOS 11 or later) | `Tuile-X.Y.Z-macos-universal.dmg` | Open the disk image and drag **Tuile** to **Applications**. It is signed and notarised by Apple, so it opens without a warning. |
| **iPhone, iPad** (iOS 16 or later) | the **TestFlight** link in the release notes | Install Apple's [TestFlight](https://apps.apple.com/app/testflight/id899247664) app, open the link on your device, and tap **Install**. A new version shows up in TestFlight once Apple's beta review has passed, usually within a day of the release. |
| **Android** (arm64, Android 8 or later) | `Tuile-X.Y.Z-android-arm64.apk` | Download it on the phone and open it. Android asks once to allow your browser or file manager to *install unknown apps*: allow it, then install. To check the download, compare it with the `.sha256` file next to it. |

## 2. Get your own Cesium ion token

1. Create a free account at [ion.cesium.com](https://ion.cesium.com/signup).
   A new account includes the assets the viewer uses: Cesium World Terrain
   (asset 1), Bing Maps Aerial (2), Aerial with Labels (3) and Sentinel-2
   (3954).
2. Open **Access Tokens** ([ion.cesium.com/tokens](https://ion.cesium.com/tokens)).
   Either copy the **Default Token**, or create a new token with the
   `assets:read` scope and access to those four assets.

A token is like a password for your account. Keep it to yourself, and use
your own: whatever the viewer streams counts against the account whose
token it uses.

## 3. Put the token in a configuration file

1. Download the example file,
   [`config.example.tuile`](../examples/wgpu-viewer/config.example.tuile).
2. Rename your copy so that it ends in **`.tuile`**, for example
   `my-tuile.tuile`. The extension is what makes your device open it in
   Tuile.
3. Open it in a text editor and replace `YOUR-ION-TOKEN` with your token,
   keeping the quotes:

   ```toml
   [ion]
   token = "eyJhbGciOi…your whole token…"
   ```

The rest of the file is optional and commented out:
- the imagery layers and the terrain, by asset number;
- the API address;
- where the first session opens.

Leave it as it is to start.

## 4. Open the file in the application

Opening the file once sets everything up:
- The token goes into your device's secure storage: the Keychain on a Mac or
  an iPhone, the Android Keystore on Android.
- The other settings go into the application's own settings.
- A short message names your file and what it set up.

The message never shows the token.

| Device | How to open the file |
|---|---|
| **Mac** | Double-click the file. Or, in Tuile: **File › Open Configuration…** (⌘O). From a terminal: `open my-tuile.tuile`. |
| **iPhone, iPad** | Get the file onto the device: AirDrop it, mail it to yourself, save it to iCloud Drive, or download it. Then tap it, and pick **Tuile** (or **Share › Tuile**). |
| **Android** | Tap the file in your file manager or downloads, or share it to **Tuile** from any app. |

On a first launch with no token, Tuile says it cannot start yet and waits. As
soon as you open your file it tries again: there is no need to quit.

**An import link.** If someone hosts a configuration file for you,
`tuile://import?src=https://…/your-file.tuile` opens Tuile, which fetches the
file and imports it the same way. The link carries only the address, never
a token: Tuile refuses a link whose address names a token, a key or a
password. Only open links from people you trust, since importing replaces
your configuration.

**From a terminal** (the desktop viewer): `tuile-wgpu-viewer --config
my-tuile.tuile` reads the file for that session only and keeps nothing.
Flags such as `--lon` win over the file. A file called `config.tuile` in the
application's support directory (`~/Library/Application Support/Tuile` on a
Mac) is read the same way at every start.

## What you should see

The globe, from about 2,000 km above France:
- The terrain is draped with aerial photography.
- Tiles sharpen as you zoom in.
- The ground is never black: a coarser tile stays in place until the finer
  one arrives.

Move around:
- **Mac:** drag to move, scroll to zoom.
- **Phone:** one finger to move, two to zoom and turn, two sliding together
  to tilt.

**I** (or the **Imagery** menu on a Mac) switches between the layers.

If something is wrong, the message names the file and the key. For
example, `my-tuile.tuile: ion.token: is still the example's placeholder`, or
`ion.terrain: must be a number, without quotes`. Fix that line and open the
file again.

## Attribution

The pictures belong to their owners, and the viewer always shows whose they
are:
- The name of the current imagery layer and its attribution stay on screen:
  in the title bar on a Mac, in a caption at the bottom of the view on a
  phone. For example, *© Microsoft and its data suppliers* for the aerial
  layers, and *Contains modified Copernicus Sentinel data* for Sentinel-2.
- The terrain is Cesium World Terrain, served by Cesium ion.

Your use of Cesium ion, and of the data you stream through it, is governed by
your Cesium ion account's terms. Tuile is an independent, open-source
implementation of the OGC 3D Tiles standard, unaffiliated with Cesium or any
data provider.
