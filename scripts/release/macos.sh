#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The macOS release: a universal Tuile.app in a disk image, signed with a
# Developer ID, notarised and stapled, so that it opens on any Mac without a
# Gatekeeper warning.
#
#   OUT=dist scripts/release/macos.sh
#
# Steps, each one tool:
#
#   1. cargo, twice: tuile-wgpu-viewer for aarch64 and x86_64; `lipo` joins
#      them into one universal executable;
#   2. `tuile-viewer-bundle app --binary`: the bundle, its Info.plist, icon
#      and scripting dictionary, sealed ad hoc and checked (as for a local
#      build);
#   3. the credential check (`no-bundled-credential.sh`);
#   4. with MACOS_SIGNING_IDENTITY: `codesign` again, with that Developer ID,
#      the hardened runtime, a secure timestamp and `macos.entitlements`;
#   5. with an App Store Connect API key: `notarytool submit --wait` on the
#      app, `stapler staple`;
#   6. `hdiutil`: a compressed disk image holding the app and a link to
#      /Applications, signed, notarised and stapled in turn; `spctl` then
#      asks Gatekeeper the question a downloader's Mac will ask.
#
# Environment:
#   OUT                     where the artefacts go (default: target/release-macos)
#   MACOS_SIGNING_IDENTITY  a "Developer ID Application" identity in the
#                           keychain (name or SHA-1); unset → ad hoc only
#   ASC_KEY_PATH, ASC_KEY_ID, ASC_ISSUER_ID
#                           the App Store Connect API key notarytool uses;
#                           unset → not notarised
#
# Without the identity and the key the image is built and named
# `…-unsigned.dmg`: for a dry run, never for a release.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../.." && pwd)"
out="${OUT:-$workspace/target/release-macos}"
target_dir="${CARGO_TARGET_DIR:-$workspace/target}"
package=tuile-wgpu-viewer
version="$(cargo metadata --format-version 1 --no-deps --manifest-path "$workspace/Cargo.toml" |
  python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tuile-viewer-bundle"))')"

# No credential reaches a public build, whatever this shell holds.
unset TUILE_BUNDLED_CREDENTIAL TUILE_TILES_TOKEN TUILE_ION_TOKEN CESIUM_ION_TOKEN

rm -rf "$out"
mkdir -p "$out"

# 1. One executable, both architectures.
for triple in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --locked --manifest-path "$workspace/Cargo.toml" --target "$triple" -p "$package"
done
lipo -create -output "$out/$package" \
  "$target_dir/aarch64-apple-darwin/release/$package" \
  "$target_dir/x86_64-apple-darwin/release/$package"
lipo -verify_arch arm64 x86_64 "$out/$package"

# 2. The bundle, as a local build lays it out.
mkdir -p "$out/stage"
cargo run --release --locked --quiet --manifest-path "$workspace/Cargo.toml" -p tuile-viewer-bundle -- \
  app --out "$out/stage" --binary "$out/$package" >/dev/null
app="$out/stage/Tuile.app"
rm -f "$out/$package"

# 3. Nothing of anyone's in it.
"$here/no-bundled-credential.sh" "$app"

identity="${MACOS_SIGNING_IDENTITY:-}"
notarise=""
if [[ -n "${ASC_KEY_PATH:-}" && -n "${ASC_KEY_ID:-}" && -n "${ASC_ISSUER_ID:-}" ]]; then
  notarise=yes
fi
notary() {
  xcrun notarytool submit "$1" --key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" \
    --issuer "$ASC_ISSUER_ID" --wait --timeout 30m
}

# 4. The Developer ID signature.
if [[ -n "$identity" ]]; then
  codesign --force --options runtime --timestamp \
    --entitlements "$here/macos.entitlements" \
    --identifier dev.lapoule.tuile.viewer --sign "$identity" "$app"
  codesign --verify --strict --verbose=2 "$app"
else
  echo "notice: MACOS_SIGNING_IDENTITY is not set: the app keeps its ad-hoc seal (dry run)" >&2
fi

# 5. The app notarised and stapled, so it opens offline once copied out.
if [[ -n "$identity" && -n "$notarise" ]]; then
  ditto -c -k --keepParent "$app" "$out/notarise.zip"
  notary "$out/notarise.zip"
  rm -f "$out/notarise.zip"
  xcrun stapler staple "$app"
elif [[ -n "$identity" ]]; then
  echo "notice: no App Store Connect key: not notarised (dry run)" >&2
fi

# 6. The disk image.
name="Tuile-$version-macos-universal"
[[ -n "$identity" && -n "$notarise" ]] || name="$name-unsigned"
ln -s /Applications "$out/stage/Applications"
dmg="$out/$name.dmg"
hdiutil create -quiet -volname "Tuile $version" -srcfolder "$out/stage" -fs HFS+ \
  -format UDZO -ov "$dmg"
rm -rf "$out/stage"
if [[ -n "$identity" ]]; then
  codesign --force --timestamp --sign "$identity" "$dmg"
fi
if [[ -n "$identity" && -n "$notarise" ]]; then
  notary "$dmg"
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$dmg"
  # The question a downloader's Mac asks of the image, and then of the app.
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
fi
"$here/no-bundled-credential.sh" "$dmg"
(cd "$out" && shasum -a 256 "$(basename "$dmg")" >"$(basename "$dmg").sha256")
echo "$dmg"
