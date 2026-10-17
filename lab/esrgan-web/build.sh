#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The wasm variants the page's selector loads, one directory each:
#   pkg/             release (thin LTO, opt-level 3)
#   pkg-fusion/      the same with Burn's fusion
#   pkg-opt/         pkg/ through wasm-opt -O3
#   pkg-fusion-opt/  pkg-fusion/ through wasm-opt -O3
# Usage: ./build.sh [profile]   (release by default; release-fat, release-s, release-z for the size study)
# Needs wasm-bindgen-cli 0.2.129 (= the crate in Cargo.lock) and binaryen's wasm-opt (version 123 measured).
set -euo pipefail
cd "$(dirname "$0")"
profile=${1:-release}
for feat in "" fusion; do
  dir=pkg${feat:+-$feat}
  [ "$profile" != release ] && dir=$dir-$profile
  cargo build --profile "$profile" --target wasm32-unknown-unknown ${feat:+--features $feat}
  wasm-bindgen --target web --out-dir "$dir" "target/wasm32-unknown-unknown/$profile/esrgan_web.wasm"
  if [ "$profile" = release ]; then
    rm -rf "$dir-opt" && cp -r "$dir" "$dir-opt"
    # wasm-bindgen output uses these post-MVP features; wasm-opt must be told so
    wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals \
      --enable-reference-types --enable-multivalue "$dir/esrgan_web_bg.wasm" -o "$dir-opt/esrgan_web_bg.wasm"
  fi
done
ls -l pkg*/esrgan_web_bg.wasm
