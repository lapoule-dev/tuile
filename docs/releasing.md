<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Releasing

A release is cut by hand, and automation follows it:

1. a `chore: version X.Y.Z` pull request raises `[workspace.package] version`
   (and the `version = "X.Y.Z"` of every internal dependency), merged on
   `main`;
2. the tag `vX.Y.Z` is pushed on that merge commit.

The tag triggers two workflows. Neither creates a tag, changes a version or
opens a pull request; both check that the tag equals the workspace version
and stop if it does not.

| Workflow | On a tag | On `workflow_dispatch` |
|---|---|---|
| `release-apps.yml` | builds, signs and hands out the viewer for macOS, iOS and Android; a GitHub Release with the downloads | a dry run: builds, signs if the secrets are there, uploads nothing but the run's own artefacts |
| `release-crates.yml` | publishes the crates on crates.io — **only** when the repository variable `CRATES_IO_PUBLISH` is `true` | `cargo publish --workspace --dry-run` |

A missing secret never fails a run: its step is skipped with a warning that
names it, and what it would have produced is not attached anywhere. A run
that would put a credential in a public build fails
(`scripts/release/no-bundled-credential.sh`, below).

## The viewer applications

The point of these builds is that **anyone can download the viewer and try
tuile on their own Mac or phone** with their own Cesium ion account
([Try tuile](try-tuile.md)). Each platform's step is a script in
`scripts/release/` that runs the same way on a Mac or a runner.

| Platform | Script | What comes out | Where it goes |
|---|---|---|---|
| macOS | `macos.sh` | `Tuile-X.Y.Z-macos-universal.dmg` (arm64 + x86_64): Developer ID, hardened runtime, notarised, stapled; opens without a Gatekeeper warning | the GitHub Release |
| Android | `android.sh` | `Tuile-X.Y.Z-android-arm64.apk` and `.aab`, signed with the upload key | the APK on the GitHub Release (sideloading); the AAB on Google Play's internal track, if a Play account is set |
| iOS | `ios.sh` | an App Store-signed `.ipa` | TestFlight, reached by its public link |

The GitHub Release is created as a **draft** as soon as the tag arrives, with
a short "Try it" header (download names, the guide, the TestFlight link from
`TESTFLIGHT_PUBLIC_LINK`) and then **GitHub's own release notes**: the pull
requests merged since the previous tag (`gh release create
--generate-notes --notes-start-tag`). It is made public once the `.dmg` and
the `.apk` are both on it; otherwise it stays a draft for a person to finish.
A release that exists already keeps its notes; the downloads are added to it.

### Version and build numbers

Both come from the tag, through the workspace version that the tag must
equal — the bundler (`tuile-viewer-bundle`) writes them, as for a local
build:

| Platform | Shown version | Build number |
|---|---|---|
| macOS, iOS | `CFBundleShortVersionString` = `X.Y.Z` | `CFBundleVersion` = `X.Y.Z` |
| Android | `versionName` = `X.Y.Z` | `versionCode` = `X·10000 + Y·100 + Z` (0.15.1 → 1501) |

Both increase with every release, which is what the stores require. The
price: **one version, one upload.** A store that has accepted a build of
`X.Y.Z` refuses a second one; a fix is a new patch version, not a re-run.
Re-running *failed jobs* of a tag's workflow is fine — a job that never
uploaded has nothing to collide with. Minor and patch stay under 100 (the
bundler refuses otherwise).

### No credential in a public build

`scripts/release/no-bundled-credential.sh` unpacks every artefact — the
`.app` and the `.dmg`, the `.xcarchive` and the `.ipa`, the `.apk` and the
`.aab` — and fails the job if a property list has a key starting with
`crates/tuile-viewer/ios/bundled-credential-prefix.txt` (the key under which
`ios/build.sh device` writes a developer's token into the phone's
`Info.plist`), or if any file holds a complete JSON Web Token. It prints file
names only. The release scripts also clear `TUILE_BUNDLED_CREDENTIAL`,
`TUILE_TILES_TOKEN`, `TUILE_ION_TOKEN` and `CESIUM_ION_TOKEN` before they
build, and the iOS one never goes through `build.sh device`.

### macOS

`macos.sh`: `cargo build` for `aarch64-apple-darwin` and `x86_64-apple-darwin`,
`lipo` into one executable, `tuile-viewer-bundle app --binary` for the bundle,
then `codesign --options runtime --timestamp` with the Developer ID and
`scripts/release/macos.entitlements` (the location, nothing else),
`notarytool submit --wait` and `stapler staple` on the app, `hdiutil` for the
disk image, which is signed, notarised and stapled in turn, and `spctl` asks
Gatekeeper's question of it.

**Mac App Store: not set up.** The application reads its token from
`~/Library/Application Support/Tuile` and is not sandboxed; a store build
would need the App Sandbox, its own provisioning profile and an installer
package. Developer ID is the route here.

### iOS: TestFlight with a public link

An `.ipa` cannot be handed out outside the App Store, so the public route is
**TestFlight external testing with a public link**:

1. `ios.sh` builds the property list and assets (`tuile-viewer-bundle
   phone`, no token), the project (`xcodegen`), the archive (`xcodebuild
   archive`, manual signing: the Apple Distribution certificate and the App
   Store profile) and the `.ipa` (`xcodebuild -exportArchive`, method
   `app-store-connect`), and sends it with `xcrun altool --upload-app` and the
   App Store Connect API key.
2. App Store Connect processes the build (minutes to an hour).
3. **By hand, once per version:** in App Store Connect → TestFlight, add the
   build to the external group that has the public link, and submit it for
   **Beta App Review**. The first build of each version needs that review
   (usually under a day); later builds of the same version usually do not —
   but here every release is a new version, so every release is reviewed
   once. Export compliance: the app uses only the system's HTTPS; answer the
   question in App Store Connect, or add `ITSAppUsesNonExemptEncryption =
   NO` to the bundler's property list once that answer is settled.
4. The public link is the repository variable `TESTFLIGHT_PUBLIC_LINK`; the
   release notes print it.

**Is App Store Connect set up for this app?** Nothing in this repository says
so: the iOS build has only ever been signed for a developer's own device
(automatic signing, `TUILE_TEAM`). Before the first tag: register the bundle
identifier `dev.lapoule.tuile.viewer`, create the app record, an external
TestFlight group with a public link, and the Apple Distribution certificate
and App Store provisioning profile below.

### Android

`android.sh` runs `crates/tuile-viewer/android/build.sh` (the developer
build: manifest and resources, native library, dex), then packages its
intermediates twice: an APK (`aapt2 link`, the library uncompressed,
`zipalign -P 16`, `apksigner`) and an App Bundle (`aapt2 link
--proto-format`, the base module laid out, `bundletool build-bundle` at a
pinned version checked by digest, `jarsigner`). No Gradle, as for the
developer build.

- **The APK on the GitHub Release** is for sideloading: the tester allows
  their browser or file manager to install unknown apps
  ([Try tuile](try-tuile.md)). It is signed with the *upload* key. A Play
  install is signed by Google's app signing key instead, so the two do not
  update each other: a tester moving from one to the other uninstalls first.
- **Google Play, internal track** — optional: only when
  `PLAY_SERVICE_ACCOUNT_JSON` is set, through
  [`r0adkll/upload-google-play`](https://github.com/r0adkll/upload-google-play).
  The app must exist in the Play Console and have had one bundle uploaded by
  hand (Play needs that first upload to enrol the app in Play App Signing).
  Until the app has been published once, set `PLAY_RELEASE_STATUS` to
  `draft`.
- `ANDROID_CATALOG_URL` (a variable, optional) is built in as the catalog
  proxy's address; without it the APK uses the developer default
  (`127.0.0.1:8091`). It is an address, not a secret.

### Tools, and why

| Need | Chosen | Why |
|---|---|---|
| CI | GitHub Actions, every action pinned by commit | already the project's CI; pinned so a moved tag cannot change a release |
| Apple signing, notarisation, upload | `codesign`, `notarytool`, `stapler`, `hdiutil`, `xcodebuild`, `altool` | Apple's own tools, on the runner already; the bundle is laid out by the project's bundler, not by an Xcode project |
| Android packaging | the SDK's `aapt2`, `zipalign`, `apksigner`, the JDK's `jarsigner`, Google's `bundletool` | the developer build already uses the SDK's tools without Gradle; bundletool is the reference AAB builder |
| Play upload | `r0adkll/upload-google-play` | the most used Play Developer API action; a pinned commit |
| Release notes | GitHub's generated notes (`gh release create --generate-notes`) | built in, nothing to install, reads the merged pull requests |
| Fastlane | not used | the three scripts are each a handful of tool calls; Fastlane would add Ruby and a Gemfile to the build for no step it does better |
| Release bots (release-plz, release-please) | not used | they open release pull requests and push tags; here a person does both |

### Secrets and variables

Set in **Settings → Secrets and variables → Actions**. The workflows name
them; none is ever printed, committed or passed on a command line. Files are
stored base64-encoded (`base64 -i file | pbcopy` on a Mac).

| Name | Kind | Used by | What it is, and how to make it |
|---|---|---|---|
| `MACOS_DEVELOPER_ID_P12_BASE64` | secret | macOS | The **Developer ID Application** certificate with its private key, exported as `.p12`. developer.apple.com → Certificates → + → Developer ID Application (an Account Holder creates it), then Keychain Access → export the identity as `.p12`. |
| `MACOS_DEVELOPER_ID_P12_PASSWORD` | secret | macOS | The password chosen at that export. |
| `IOS_DISTRIBUTION_P12_BASE64` | secret | iOS | The **Apple Distribution** certificate with its key, as `.p12` (Certificates → + → Apple Distribution). |
| `IOS_DISTRIBUTION_P12_PASSWORD` | secret | iOS | Its export password. |
| `IOS_PROVISIONING_PROFILE_BASE64` | secret | iOS | An **App Store** distribution provisioning profile for `dev.lapoule.tuile.viewer`, made with that certificate (Profiles → + → App Store Connect). The `.mobileprovision` file, base64. |
| `ASC_KEY_ID` | secret | macOS, iOS | The App Store Connect API key's ID. App Store Connect → Users and Access → Integrations → App Store Connect API → Team Keys → +, role **App Manager** (notarisation and TestFlight uploads). |
| `ASC_ISSUER_ID` | secret | macOS, iOS | The issuer ID shown above that list. |
| `ASC_KEY_P8_BASE64` | secret | macOS, iOS | The key's `AuthKey_<ID>.p8`, base64 (downloadable once). |
| `APPLE_TEAM_ID` | variable | iOS | The ten-character team ID (Membership details). Not a secret. |
| `TESTFLIGHT_PUBLIC_LINK` | variable | release notes | The external group's public link, `https://testflight.apple.com/join/…`. |
| `ANDROID_UPLOAD_KEYSTORE_BASE64` | secret | Android | The **upload** keystore: `keytool -genkeypair -v -keystore upload.jks -alias upload -keyalg RSA -keysize 4096 -validity 10000`, base64. Kept also somewhere safe offline: Play can reset a lost upload key, the GitHub Release's sideloaded APK cannot. |
| `ANDROID_UPLOAD_KEYSTORE_PASSWORD` | secret | Android | The keystore's password. |
| `ANDROID_UPLOAD_KEY_ALIAS` | secret | Android | The key's alias (`upload` above). |
| `ANDROID_UPLOAD_KEY_PASSWORD` | secret | Android | The key's password. |
| `PLAY_SERVICE_ACCOUNT_JSON` | secret | Android, optional | A Google Cloud service account's JSON key, invited in Play Console → Users and permissions with *Release to testing tracks* on this app. Unset: nothing goes to Play. |
| `PLAY_RELEASE_STATUS` | variable | Android, optional | `completed` (default) or `draft` while the app has never been published. |
| `ANDROID_CATALOG_URL` | variable | Android, optional | The catalog proxy's address built into the APK. |
| `CARGO_REGISTRY_TOKEN` | secret | crates | A crates.io API token (crates.io → Account Settings → API Tokens), scopes `publish-new` and `publish-update`. |
| `CRATES_IO_PUBLISH` | variable | crates | `true` to publish on a tag. Unset or anything else: nothing is published. |

What only these secrets can show — the Developer ID signature, notarisation,
the TestFlight upload, Play's acceptance — has not been run yet: the first
tag with the secrets in place is that test. Before it, a `workflow_dispatch`
of *Release apps* with the secrets set exercises everything but the uploads.

## crates.io

### What is published

Every crate whose manifest does not say `publish = false`. Not published:

| Crate | Why |
|---|---|
| `tuile-viewer`, `tuile-viewer-bundle`, `tuile-record`, `tuile-star-catalogue` | the applications, their packager and their tools |
| `tuile-hydra` | a static library for the OpenUSD/Hydra plugin's C++ build |
| `tuile-pack-api` | a deployed service, one binary |
| everything under `examples/` | examples and benches |

The metadata a published crate carries — licence, repository, homepage,
keywords, categories, `rust-version` — is set once in the root
`[workspace.package]` and inherited; the description and the README are each
crate's own. Test data (`tests/fixtures/`, `tests/frozen/`) is excluded from
the packages; the largest package is about 1.5 MB. Every dependency on
another crate of the workspace carries its `version` beside its `path`, so
the version PR changes both. A path *dev*-dependency without a version is
intentional: cargo strips it from the package.

### Order

`cargo publish --workspace` works out the order itself; for the record, a
topological order of the dependency graph:

```text
tuile-b3dm, tuile-overlay, tuile-radiometry, tuile-mp4
tuile-core
tuile-atmosphere, tuile-bing, tuile-terrain, tuile-pack, tuile-metrics,
  tuile-camera, tuile-tile-server, tuile-storage-foyer
tuile-planetary, tuile-tape, tuile-farm, tuile-ui, tuile-wgpu
tuile-cesium-ion, tuile-usd
tuile-native-fetchers, tuile-web, tuile-film, tuile-repository
tuile-bake, tuile-film-gpu, tuile-bevy
tuile-film-native
```

Each line depends only on the lines above it. Every `tuile-*` name was free
on crates.io on 2026-10-31; `tuile` alone is taken by another project.

### What stands in the way

crates.io refuses a dependency it cannot fetch from itself, and ignores
`[patch]`. Today:

| Dependency | Source | Crates it blocks | What publishing needs |
|---|---|---|---|
| `texcomp` | `lapoule-dev/texcomp`, tag `v0.2.0` (own crate, `publish = false`, name free on crates.io) | `tuile-core`, `tuile-wgpu` — so every crate but four | publish `texcomp` itself (remove its `publish = false`), then depend on it by version |
| `astro` | fork of `saurvs/astro-rust` at `09bf619` (one commit: the obliquity polynomial in centuries) | `tuile-atmosphere` and all that use it (`tuile-wgpu`, `tuile-film`, …) | upstream 2.0.0 is from 2016 and unmaintained: publish the fork under another name (e.g. `astro-lapoule`), or vendor the few functions used |
| `mp4-atom` | fork of `kixelated/mp4-atom` at `b491c22` (one commit: Apple's stereo video boxes) | `tuile-mp4`, `tuile-farm`, `tuile-repository`, `tuile-film-native` | upstream is active (0.17.0, 2026-10-30): get the boxes released upstream, or publish the fork under another name |
| `nvidia-video-codec-sdk` (optional) | fork of `ViliamVadocz/nvidia-video-codec-sdk` at `ee2e29f` | `tuile-film-native` (optional dependencies count too) | upstream release of NVENC 13 support, or the fork under another name |
| `pmtiles` (`[patch.crates-io]`) | fork of `stadiamaps/pmtiles-rs`, branch `tuile-accessors` | `tuile-tile-server`, `tuile-repository`: published, they would build against upstream 0.24 without the accessors and fail | the accessors released upstream (0.26 is out; check whether it has them), then drop the patch |

Publishable today, with no blocker: `tuile-b3dm`, `tuile-overlay`,
`tuile-radiometry`. Until the rest is cleared, `cargo publish --workspace`
fails while packaging — before any upload — so turning on `CRATES_IO_PUBLISH`
early publishes nothing.

### Turning it on

1. Clear the blockers above; `workflow_dispatch` of *Release crates* is the
   dry run that says whether they are cleared.
2. Add `CARGO_REGISTRY_TOKEN`; set `CRATES_IO_PUBLISH` to `true`.
3. The next tag publishes. Versions on crates.io cannot be deleted, only
   yanked.

Once the crates exist, crates.io's *trusted publishing* (an OpenID Connect
exchange with GitHub Actions, no long-lived token) can replace the token.
