#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The Android release: an APK for sideloading from the GitHub Release, and an
# Android App Bundle for Google Play, both signed with the upload key.
#
#   OUT=dist scripts/release/android.sh
#
# `crates/tuile-viewer/android/build.sh` does the building — the manifest and
# resources, the native library, the dex — exactly as for a developer; this
# script takes its intermediate files and packages them twice:
#
#   APK  `aapt2 link`, the library stored uncompressed, `zipalign -P 16`,
#        `apksigner` with the upload key;
#   AAB  `aapt2 link --proto-format`, the module laid out as bundletool wants
#        it (manifest/, dex/, lib/, res/, resources.pb), `bundletool
#        build-bundle`, `jarsigner` with the upload key.
#
# No Gradle, as for the developer build. bundletool is Google's own, fetched
# at a pinned version and checked against its digest.
#
# Environment:
#   OUT                          where the artefacts go (default: target/release-android)
#   ANDROID_KEYSTORE             the upload keystore (a file); unset → no
#                                release signature, and the artefacts are named
#                                `…-unsigned` (a dry run)
#   ANDROID_KEYSTORE_PASSWORD, ANDROID_KEY_ALIAS, ANDROID_KEY_PASSWORD
#   TUILE_CATALOG_URL            built in as the catalog proxy's address (an
#                                address, not a secret: see the Android README)
#
# No token goes into either package: the Android build never reads one, and
# `no-bundled-credential.sh` checks both.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../.." && pwd)"
android="$workspace/crates/tuile-viewer/android"
build="$android/build"
out="${OUT:-$workspace/target/release-android}"
version="$(cargo metadata --format-version 1 --no-deps --manifest-path "$workspace/Cargo.toml" |
  python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tuile-viewer-bundle"))')"
bundletool_version=1.18.3
bundletool_sha256=a099cfa1543f55593bc2ed16a70a7c67fe54b1747bb7301f37fdfd6d91028e29
platform=35

unset TUILE_BUNDLED_CREDENTIAL TUILE_TILES_TOKEN TUILE_ION_TOKEN CESIUM_ION_TOKEN

# The developer build, whose intermediates are packaged below. Its own APK
# (debug-signed) is left where it is and not shipped.
"$android/build.sh" >/dev/null

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
tools="$(find "$sdk/build-tools" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -n 1)"
android_jar="$sdk/platforms/android-$platform/android.jar"
library="$(cd "$build/apk/lib/arm64-v8a" && ls)"

rm -rf "$out"
mkdir -p "$out/work"
work="$out/work"

signed=""
if [[ -n "${ANDROID_KEYSTORE:-}" ]]; then
  : "${ANDROID_KEYSTORE_PASSWORD:?ANDROID_KEYSTORE_PASSWORD is not set}"
  : "${ANDROID_KEY_ALIAS:?ANDROID_KEY_ALIAS is not set}"
  : "${ANDROID_KEY_PASSWORD:?ANDROID_KEY_PASSWORD is not set}"
  signed=yes
else
  echo "notice: ANDROID_KEYSTORE is not set: no release signature (dry run)" >&2
fi
suffix=""
[[ -n "$signed" ]] || suffix="-unsigned"
name="Tuile-$version-android-arm64$suffix"

# The APK.
"$tools/aapt2" link -I "$android_jar" --manifest "$build/generated/AndroidManifest.xml" \
  -o "$work/unaligned.apk" "$build/resources.zip"
mkdir -p "$work/apk/lib/arm64-v8a"
cp "$build/apk/lib/arm64-v8a/$library" "$work/apk/lib/arm64-v8a/"
cp "$build/dex/classes.dex" "$work/apk/"
(cd "$work/apk" && zip -q -0 -r "$work/unaligned.apk" lib && zip -q "$work/unaligned.apk" classes.dex)
"$tools/zipalign" -P 16 -f 4 "$work/unaligned.apk" "$work/aligned.apk"
apk="$out/$name.apk"
if [[ -n "$signed" ]]; then
  # Passwords through the environment, never on the command line.
  "$tools/apksigner" sign --ks "$ANDROID_KEYSTORE" --ks-key-alias "$ANDROID_KEY_ALIAS" \
    --ks-pass env:ANDROID_KEYSTORE_PASSWORD --key-pass env:ANDROID_KEY_PASSWORD \
    --out "$apk" "$work/aligned.apk"
  "$tools/apksigner" verify --verbose "$apk" >/dev/null
  rm -f "$apk.idsig"
else
  cp "$work/aligned.apk" "$apk"
fi

# The bundle's base module.
"$tools/aapt2" link --proto-format -I "$android_jar" \
  --manifest "$build/generated/AndroidManifest.xml" -o "$work/proto.apk" "$build/resources.zip"
mkdir -p "$work/base/manifest" "$work/base/dex" "$work/base/lib/arm64-v8a"
(cd "$work/base" && unzip -q "$work/proto.apk")
mv "$work/base/AndroidManifest.xml" "$work/base/manifest/"
cp "$build/dex/classes.dex" "$work/base/dex/"
cp "$build/apk/lib/arm64-v8a/$library" "$work/base/lib/arm64-v8a/"
(cd "$work/base" && zip -q -r "$work/base.zip" .)

bundletool="${XDG_CACHE_HOME:-$HOME/.cache}/tuile/bundletool-$bundletool_version.jar"
if [[ ! -f "$bundletool" ]] || ! echo "$bundletool_sha256  $bundletool" | shasum -a 256 -c --status; then
  mkdir -p "$(dirname "$bundletool")"
  curl -sfL -o "$bundletool.part" \
    "https://github.com/google/bundletool/releases/download/$bundletool_version/bundletool-all-$bundletool_version.jar"
  if ! echo "$bundletool_sha256  $bundletool.part" | shasum -a 256 -c --status; then
    rm -f "$bundletool.part"
    echo "error: bundletool did not match its digest" >&2
    exit 2
  fi
  mv "$bundletool.part" "$bundletool"
fi
aab="$out/$name.aab"
java -jar "$bundletool" build-bundle --modules="$work/base.zip" --output="$aab"
java -jar "$bundletool" validate --bundle="$aab" >/dev/null
if [[ -n "$signed" ]]; then
  jarsigner -keystore "$ANDROID_KEYSTORE" -storepass:env ANDROID_KEYSTORE_PASSWORD \
    -keypass:env ANDROID_KEY_PASSWORD -sigalg SHA256withRSA -digestalg SHA-256 \
    "$aab" "$ANDROID_KEY_ALIAS" >/dev/null
  jarsigner -verify "$aab" >/dev/null
fi
rm -rf "$work"

"$here/no-bundled-credential.sh" "$apk" "$aab"
(cd "$out" && shasum -a 256 "$(basename "$apk")" >"$(basename "$apk").sha256")
echo "$apk"
echo "$aab"
