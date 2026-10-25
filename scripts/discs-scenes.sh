#!/bin/zsh
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The discs' acceptance scenes (`tuile_atmosphere::scenes`), rendered by the
# film and by the viewer, into <out>/film/<scene>/ and <out>/viewer/<scene>/:
# the film's frames with the disc instrument's discs.parquet beside them, the
# viewer's frames and its sheet. Then `scripts/discs-sheet.py` lays them
# side by side.
#
#   scripts/discs-scenes.sh videos/discs-s1 [scene ...]
#
# Needs TUILE_ION_TOKEN (read from ./.env or the main checkout's, never
# printed), the release binaries:
#   cargo build --release -p tuile-film-native --features videotoolbox \
#       --bin tuile-film-render --bin tuile-film-produce --bin tuile-film-scene
#   cargo build --release -p tuile-wgpu-viewer --example discs
# and keeps its stores (run store, tile store, cache, tapes) under
# $TUILE_DISCS_STORES (default ~/.cache/tuile-discs).
set -e
out=$1; shift
if (( $# == 0 )); then set -- pic-du-midi-sunset full-moon-rise crescent-at-dusk sun-behind-ridge; fi
target=${CARGO_TARGET_DIR:-target}
B=$target/release
stores=${TUILE_DISCS_STORES:-$HOME/.cache/tuile-discs}
mkdir -p $stores/runs $stores/tiles $stores/cache $stores/tapes
env_file=${TUILE_ENV_FILE:-.env}
set -a; . $env_file; set +a
export TUILE_STORE_DIR=$stores/runs TUILE_TILES_LOCAL=$stores/tiles TUILE_RESIDENT_BUDGET_GB=${TUILE_RESIDENT_BUDGET_GB:-4}
for s in "$@"; do
  tape=$stores/tapes/$s.mcap
  clock=$($B/tuile-film-scene $s $tape | sed -E 's/.*clock //')
  key=$($B/tuile-film-produce $tape --resolution 960x540 --atmosphere --fps 1 --codec h264 \
      --cache $stores/cache 2>&1 | grep -oE 'packs/[0-9a-f]+\.references' | head -1)
  mkdir -p $out/film/$s
  $B/tuile-film-render $key --atmosphere --clock $clock --fps 1 --cache $stores/cache \
      --pictures $out/film/$s --discs $out/film/$s > $out/film/$s/render.log 2>&1
  echo "$s: clock $clock, $key"
  grep "^discs: the" $out/film/$s/render.log || true
  $target/release/examples/discs $s --out $out/viewer | grep -v ERROR
done
