#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Proves the OpenUSD toolchain end to end: the plugin loads, the procedural is
# resolved, the camera dependency fires, and geometry reaches the renderer.
#
# The test is that two frames of the same stage differ. Both are rendered in a
# *single* usdrecord process on purpose — two processes would each cook from
# scratch and differ even if invalidation were completely broken, which is the
# one failure this is meant to catch.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:-/tmp/tuile-spike}"
renderer="${TUILE_SPIKE_RENDERER:-Embree}"

if [[ -z "${PXR_PLUGINPATH_NAME:-}" ]]; then
    echo "PXR_PLUGINPATH_NAME is unset — the plugin would not be found." >&2
    exit 2
fi
if [[ "${HDGP_INCLUDE_DEFAULT_RESOLVER:-0}" != "1" ]]; then
    # Worth its own check: without it the stage renders empty and every other
    # symptom points somewhere else entirely.
    echo "HDGP_INCLUDE_DEFAULT_RESOLVER is not 1 — procedurals would be" \
         "silently skipped and the images would be empty but valid." >&2
    exit 2
fi

rm -rf "$out"
mkdir -p "$out"

gpu_args=()
if [[ "$renderer" == "Embree" ]]; then
    # Embree needs no GPU, and a headless container has no GL context to give
    # it. Without this the recorder fails while setting up, long before any
    # geometry is asked for.
    gpu_args+=(--disableGpu)
fi

echo "== rendering frames 1 and 2 in one process (renderer: $renderer)"
# `###` and not `#`: usdrecord requires a hash-mark placeholder, and the count
# of hashes is the zero-padding, so the frames land as frame.001/frame.002.
usdrecord \
    --renderer "$renderer" \
    --frames 1:2 \
    --imageWidth 512 \
    --camera /World/ShotCam \
    "${gpu_args[@]}" \
    "$here/spike.usda" \
    "$out/frame.###.png"

for frame in 001 002; do
    if [[ ! -s "$out/frame.$frame.png" ]]; then
        echo "FAIL: frame $frame was not written" >&2
        ls -la "$out" >&2
        exit 1
    fi
done

# The assertion measures the images rather than comparing them byte for byte.
#
# `cmp -s` was the first version of this and it was worthless: two blank white
# frames differing by three anti-aliased pixels satisfy "not identical", so the
# check passed green while the render showed nothing whatsoever. Coverage first,
# difference second — in that order, because a blank frame makes the difference
# meaningless.
python3 "$here/compare_frames.py" \
    "$out/frame.001.png" "$out/frame.002.png" \
    "${TUILE_SPIKE_MIN_COVERAGE:-0.02}" \
    "${TUILE_SPIKE_MIN_DIFFERENCE:-0.01}"

echo "Images in $out"
