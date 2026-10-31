#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The iOS release: an App Store-signed .ipa, uploaded to TestFlight.
#
#   OUT=dist scripts/release/ios.sh
#
# The same three tools as `crates/tuile-viewer/ios/build.sh` — the property
# list and assets from `tuile-viewer-bundle phone`, the project from
# `xcodegen`, the bundle from `xcodebuild` — but archived and exported for
# App Store Connect rather than built for a developer's device, and with
# signing given explicitly (a distribution certificate and an App Store
# provisioning profile) instead of Xcode's automatic signing.
#
# THE RELEASE BUILD CARRIES NO TOKEN. `build.sh device` can build one in for
# a developer's own phone; this script never does — it clears every variable
# that could carry one — and checks the property list, the archive and the
# .ipa with `no-bundled-credential.sh` before anything is signed or sent.
#
# Environment:
#   OUT                         where the artefacts go (default: target/release-ios)
#   APPLE_TEAM_ID               the team that signs
#   IOS_PROFILE_UUID            the App Store provisioning profile, installed
#   ASC_KEY_ID, ASC_ISSUER_ID   the App Store Connect API key (its .p8 in
#                               ~/.appstoreconnect/private_keys/AuthKey_<id>.p8)
#   IOS_UPLOAD                  "yes" to send the .ipa to TestFlight
#
# Without APPLE_TEAM_ID and IOS_PROFILE_UUID the archive is built unsigned
# (a dry run: it proves the build, nothing more) and nothing is exported.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../.." && pwd)"
ios="$workspace/crates/tuile-viewer/ios"
out="${OUT:-$workspace/target/release-ios}"
identifier=dev.lapoule.tuile.viewer

# No credential reaches a public build, whatever this shell holds.
unset TUILE_BUNDLED_CREDENTIAL TUILE_TILES_TOKEN TUILE_ION_TOKEN CESIUM_ION_TOKEN

rm -rf "$out"
mkdir -p "$out"

# The settings page asks for the host's credential by the host's name: read
# as data from the host's file, as build.sh does. Only the field's name and
# title; never a value.
CREDENTIAL_VARIABLE="TUILE_TILES_TOKEN"
CREDENTIAL_TITLE="Tile service token"
host_conf="${TUILE_HOST_CONF:-$workspace/examples/wgpu-viewer/phone.conf}"
if [[ -f "$host_conf" ]]; then
  while IFS='=' read -r key value; do
    case "$key" in
      CREDENTIAL_VARIABLE) CREDENTIAL_VARIABLE="$value" ;;
      CREDENTIAL_TITLE) CREDENTIAL_TITLE="$value" ;;
    esac
  done < <(grep -E '^[A-Z_]+=' "$host_conf")
fi
unset "$CREDENTIAL_VARIABLE"

cargo run --release --locked --quiet --manifest-path "$workspace/Cargo.toml" -p tuile-viewer-bundle -- \
  phone --out "$ios/Generated" --credential "$CREDENTIAL_VARIABLE" --credential-title "$CREDENTIAL_TITLE" >/dev/null
"$here/no-bundled-credential.sh" "$ios/Generated"
(cd "$ios" && xcodegen generate --quiet)

team="${APPLE_TEAM_ID:-}"
profile="${IOS_PROFILE_UUID:-}"
archive="$out/Tuile.xcarchive"
if [[ -n "$team" && -n "$profile" ]]; then
  signing=(CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="$team"
    CODE_SIGN_IDENTITY="Apple Distribution" PROVISIONING_PROFILE_SPECIFIER="$profile")
else
  echo "notice: no team or provisioning profile: the archive is built unsigned (dry run)" >&2
  signing=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO DEVELOPMENT_TEAM=)
fi

xcodebuild -project "$ios/Tuile.xcodeproj" -scheme Tuile -configuration Release \
  -destination "generic/platform=iOS" -derivedDataPath "$ios/build" \
  -archivePath "$archive" -quiet "${signing[@]}" archive
"$here/no-bundled-credential.sh" "$archive"

if [[ -z "$team" || -z "$profile" ]]; then
  echo "$archive"
  exit 0
fi

# Exported for App Store Connect, with the same signing, said explicitly.
cat >"$out/ExportOptions.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>method</key><string>app-store-connect</string>
    <key>destination</key><string>export</string>
    <key>teamID</key><string>$team</string>
    <key>signingStyle</key><string>manual</string>
    <key>signingCertificate</key><string>Apple Distribution</string>
    <key>provisioningProfiles</key>
    <dict><key>$identifier</key><string>$profile</string></dict>
    <key>manageAppVersionAndBuildNumber</key><false/>
    <key>uploadSymbols</key><true/>
</dict>
</plist>
PLIST
xcodebuild -exportArchive -archivePath "$archive" -exportOptionsPlist "$out/ExportOptions.plist" \
  -exportPath "$out/export" -quiet
ipa="$(find "$out/export" -name '*.ipa' | head -n 1)"
"$here/no-bundled-credential.sh" "$ipa"

if [[ "${IOS_UPLOAD:-}" == yes ]]; then
  : "${ASC_KEY_ID:?ASC_KEY_ID is not set}"
  : "${ASC_ISSUER_ID:?ASC_ISSUER_ID is not set}"
  xcrun altool --upload-app --type ios --file "$ipa" \
    --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID"
else
  echo "notice: IOS_UPLOAD is not \"yes\": the .ipa is not sent to TestFlight (dry run)" >&2
fi
echo "$ipa"
