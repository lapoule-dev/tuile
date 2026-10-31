#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Fails if a release artefact carries a credential.
#
#   scripts/release/no-bundled-credential.sh Tuile.app Tuile.dmg Tuile.ipa Tuile.apk Tuile.aab
#
# A public build is downloaded by anyone, and anything inside an application
# can be read back out of it. The phone's developer build (`ios/build.sh
# device`) can write a token into its `Info.plist`, under a key that starts
# with the prefix in `crates/tuile-viewer/ios/bundled-credential-prefix.txt`.
# A release build must never do that. Two checks, on every file of every
# artefact (archives are unpacked first, disk images mounted read-only):
#
#   1. no property list has a key that starts with that prefix;
#   2. no file holds a complete JSON Web Token (three base64url parts, the
#      first two starting `eyJ`): the form of a tile service's access token.
#
# Only file names are printed, never what matched.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../.." && pwd)"
prefix="$(tr -d '[:space:]' <"$workspace/crates/tuile-viewer/ios/bundled-credential-prefix.txt")"
jwt='eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}'

if [[ $# -eq 0 ]]; then
  echo "usage: $0 ARTEFACT..." >&2
  exit 2
fi

scratch="$(mktemp -d)"
mounts=()
cleanup() {
  for m in "${mounts[@]+"${mounts[@]}"}"; do hdiutil detach -quiet "$m" || true; done
  rm -rf "$scratch"
}
trap cleanup EXIT

found=0
scan() {
  local root="$1" label="$2"
  # 1. Property lists: read as XML, whatever their encoding on disk.
  while IFS= read -r -d '' plist; do
    local xml
    if command -v plutil >/dev/null; then
      xml="$(plutil -convert xml1 -o - "$plist" 2>/dev/null || true)"
    else
      xml="$(cat "$plist")"
    fi
    if grep -q "<key>$prefix" <<<"$xml"; then
      echo "error: $label: ${plist#"$root"/} has a bundled-credential key" >&2
      found=1
    fi
  done < <(find "$root" -type f -name '*.plist' -print0)
  # 2. A whole token, anywhere, in any file.
  local hits
  hits="$(grep -rlaE "$jwt" "$root" || true)"
  if [[ -n "$hits" ]]; then
    while IFS= read -r hit; do
      echo "error: $label: ${hit#"$root"/} holds what looks like an access token" >&2
    done <<<"$hits"
    found=1
  fi
}

n=0
for artefact in "$@"; do
  n=$((n + 1))
  if [[ ! -e "$artefact" ]]; then
    echo "error: $artefact does not exist" >&2
    exit 2
  fi
  case "$artefact" in
    *.apk | *.aab | *.ipa | *.zip)
      mkdir -p "$scratch/$n"
      unzip -q "$artefact" -d "$scratch/$n"
      scan "$scratch/$n" "$artefact"
      ;;
    *.dmg)
      mkdir -p "$scratch/mnt$n"
      hdiutil attach -quiet -readonly -nobrowse -mountpoint "$scratch/mnt$n" "$artefact"
      mounts+=("$scratch/mnt$n")
      scan "$scratch/mnt$n" "$artefact"
      ;;
    *)
      scan "$artefact" "$artefact"
      ;;
  esac
done

if [[ "$found" -ne 0 ]]; then
  echo "error: a release artefact carries a credential; nothing is signed or published" >&2
  exit 1
fi
echo "no bundled credential in: $*"
