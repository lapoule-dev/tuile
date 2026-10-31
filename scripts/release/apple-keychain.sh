#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Imports one signing certificate into a keychain of its own, for a CI run.
#
#   P12_BASE64=… P12_PASSWORD=… scripts/release/apple-keychain.sh
#
# Prints the keychain's path. The keychain is made with a random password,
# unlocked, put first in the search list so `codesign` and `xcodebuild` find
# the identity, and allowed to sign without a prompt. It lives under
# $RUNNER_TEMP and goes with the runner; `security delete-keychain` removes
# it sooner.
#
# The certificate and its password come from the environment, never from an
# argument, and are never printed.
set -euo pipefail

: "${P12_BASE64:?P12_BASE64 is not set}"
: "${P12_PASSWORD:?P12_PASSWORD is not set}"
dir="${RUNNER_TEMP:-$(mktemp -d)}"
keychain="$dir/tuile-signing-$$.keychain-db"
password="$(openssl rand -base64 24)"
p12="$dir/signing-$$.p12"

umask 077
printf '%s' "$P12_BASE64" | base64 --decode >"$p12"
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$password" "$keychain"
security import "$p12" -P "$P12_PASSWORD" -A -t cert -f pkcs12 -k "$keychain" >/dev/null
rm -f "$p12"
security set-key-partition-list -S apple-tool:,apple: -k "$password" "$keychain" >/dev/null
# shellcheck disable=SC2046 # the existing list, one path per word
security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
echo "$keychain"
