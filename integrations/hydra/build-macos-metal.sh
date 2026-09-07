#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Native macOS (Apple Silicon) build of the render stack: OpenUSD 26.08 with
# usdview/Storm, plus the Cycles Hydra delegate on Metal. This is the recipe
# that was proven end to end on an M2 — every non-obvious step below was paid
# for with a real failure, recorded next to the line that prevents it.
#
# Idempotent and resumable: rerunning after an interruption continues the
# incremental build. Result: $WORKSPACE/env.sh to source before usdview /
# usdrecord.
#
#   ./integrations/hydra/build-macos-metal.sh [workspace]   # default ~/openusd
#
# Linux twin: build-ubuntu.sh (same pins, same patches).

set -euo pipefail

WORKSPACE="${1:-$HOME/openusd}"
USD_VERSION="26.08"
PREFIX="$WORKSPACE/$USD_VERSION"
VENV="$WORKSPACE/venv"
USD_SRC="$WORKSPACE/src/OpenUSD-$USD_VERSION"
CYCLES_SRC="$WORKSPACE/src/cycles"
# Pinned: "Link hdsi for external OpenUSD" — the first ref that builds against
# an external 26.08 at all.
CYCLES_REF="8424ed531b0d0b56667418d4a8d09452957b7904"
PATCHES="$(cd "$(dirname "$0")/patches" && pwd)"

say() { printf '\n== %s\n' "$*"; }

# ---------------------------------------------------------------- prerequisites
say "prerequisites"
for tool in cmake git git-lfs python3.11 curl; do
    command -v "$tool" >/dev/null || {
        echo "missing: $tool (brew install ${tool/python3.11/python@3.11})" >&2
        exit 1
    }
done
xcode-select -p >/dev/null || { echo "missing: Xcode command line tools" >&2; exit 1; }

# Job count bounded by memory, not cores: 1.5 GB per compiler is measured
# (hdSt/usdImaging TUs); more jobs than memory collapses into swap.
MEM_BYTES=$(sysctl -n hw.memsize)
JOBS=$(( MEM_BYTES / 1500000000 ))
CPUS=$(sysctl -n hw.ncpu)
[ "$JOBS" -lt 1 ] && JOBS=1
[ "$JOBS" -gt "$CPUS" ] && JOBS=$CPUS
echo "building with -j $JOBS ($((MEM_BYTES / 1024 / 1024)) MiB, $CPUS cpus)"

mkdir -p "$WORKSPACE/src"

# ------------------------------------------------------------------------ venv
# usdview is PySide6; build_usd.py needs pyside6-uic ON PATH (it does not look
# inside the venv on its own), hence venv/bin prepended for the build below.
say "python venv"
[ -d "$VENV" ] || python3.11 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip PySide6 PyOpenGL jinja2

# ---------------------------------------------------------------- OpenUSD source
say "OpenUSD $USD_VERSION source"
if [ ! -d "$USD_SRC" ]; then
    curl -fL --retry 5 -o "$WORKSPACE/src/openusd.tar.gz" \
        "https://github.com/PixarAnimationStudios/OpenUSD/archive/refs/tags/v$USD_VERSION.tar.gz"
    tar xzf "$WORKSPACE/src/openusd.tar.gz" -C "$WORKSPACE/src"
    rm "$WORKSPACE/src/openusd.tar.gz"
fi

# OIIO 2.5's ffmpeg plugin does not build against Homebrew ffmpeg 8
# (avcodec_close removed) and nothing here needs video decode.
if ! grep -q 'USE_FFMPEG=OFF' "$USD_SRC/build_scripts/build_usd.py"; then
    say "patching build_usd.py: OIIO without ffmpeg"
    python3 - "$USD_SRC/build_scripts/build_usd.py" <<'EOF'
import sys
path = sys.argv[1]
src = open(path).read()
anchor = "extraArgs = ['-DOIIO_BUILD_TOOLS={}'.format(buildOIIOTools),"
assert anchor in src, "OIIO extraArgs anchor not found — build_usd.py changed"
src = src.replace(anchor, anchor + "\n                     '-DUSE_FFMPEG=OFF',")
open(path, 'w').write(src)
EOF
fi

# If the prefix holds a classic-TBB (2020) install, the two ABIs are
# incompatible (tbb::internal::thread_get_id_v3) and mixed headers make
# tbb::split ambiguous. Purge, then force-rebuild every dependency that
# linked it — a stale dylib keeps its two-level-namespace bindings to symbols
# the old libtbb exported and oneTBB does not (seen live: libOpenImageIO
# expecting std::length_error's destructor *from libtbb*).
FORCE_DEPS=""
if [ -f "$PREFIX/include/tbb/tbb_stddef.h" ]; then
    say "purging classic TBB from the prefix"
    rm -rf "$PREFIX/include/tbb" "$PREFIX/include/oneapi"
    rm -f "$PREFIX"/lib/libtbb*
    FORCE_DEPS="--force OpenImageIO --force OpenVDB --force Embree"
fi

# ---------------------------------------------------------------- OpenUSD build
# --onetbb is REQUIRED: the Cycles delegate ships against oneTBB and a
# classic-TBB USD cannot host it (ABI clash above).
# CMAKE_POLICY_VERSION_MINIMUM=3.5: CMake 4 refuses the pre-3.5 minimums
# several bundled dependencies still declare.
build_usd() {
    CMAKE_POLICY_VERSION_MINIMUM=3.5 PATH="$VENV/bin:$PATH" \
    "$VENV/bin/python" -u "$USD_SRC/build_scripts/build_usd.py" \
        --python --usd-imaging --embree \
        --materialx --openimageio --opencolorio --openvdb \
        --alembic --ptex --draco --usdview \
        --onetbb $FORCE_DEPS \
        --no-examples --no-tutorials --no-tests --no-docs --no-python-docs \
        --no-prman --no-vulkan \
        -j "$JOBS" "$PREFIX"
}

say "OpenUSD build"
if ! build_usd; then
    # Alembic 1.8.5 hard-sets CMP0042 OLD, which CMake 4 rejects outright —
    # the env floor above cannot override an explicit SET. Patch the extracted
    # source and resume.
    alembic=$(ls -d "$PREFIX"/src/alembic-*/CMakeLists.txt 2>/dev/null | head -1)
    if [ -n "$alembic" ] && grep -q 'CMP0042 OLD' "$alembic"; then
        say "patching Alembic CMP0042 OLD -> NEW, resuming"
        sed -i '' 's/CMP0042 OLD/CMP0042 NEW/' "$alembic"
        build_usd
    else
        echo "OpenUSD build failed for a reason this script does not know" >&2
        exit 1
    fi
fi

otool -L "$PREFIX/lib/libusd_usd.dylib" | grep -q 'libtbb.12' \
    || { echo "USD is not linked against oneTBB — refusing to continue" >&2; exit 1; }

# ---------------------------------------------------------------------- Cycles
say "Cycles delegate (Metal)"
if [ ! -d "$CYCLES_SRC" ]; then
    git clone https://projects.blender.org/blender/cycles.git "$CYCLES_SRC"
fi
git -C "$CYCLES_SRC" checkout --quiet "$CYCLES_REF"
# Fetches the precompiled dependency libs (lib/macos_arm64, ~1.3 GB, git-lfs).
make -C "$CYCLES_SRC" update

# Two things upstream Cycles does not know: the 26.08 IsSupported signature,
# and env overrides (CYCLES_SAMPLES / CYCLES_DENOISE / CYCLES_TIME_LIMIT /
# CYCLES_BACKGROUND) so a recorder can control the sample budget and batch
# mode when the host cannot deliver render settings. Idempotent.
if ! grep -q 'CYCLES_SAMPLES' "$CYCLES_SRC/src/hydra/render_delegate.cpp"; then
    say "patching Cycles for the 26.08 IsSupported signature"
    git -C "$CYCLES_SRC" apply "$PATCHES/cycles-usd2608-hydra.patch"
fi

# FindUSDPixar silently falls back to Cycles' *bundled* oneTBB (newer than
# USD's) when $PREFIX/lib has no libtbb at configure time — the delegate then
# references symbols USD's libtbb.12 does not export and dlopen fails at run
# time. Seen live. USD's libtbb must exist before Cycles configures, and a
# build dir configured before it did must be thrown away.
[ -f "$PREFIX/lib/libtbb.12.dylib" ] \
    || { echo "no libtbb.12 in the prefix — USD build incomplete" >&2; exit 1; }

# Embree must be USD's too: the bundled libembree4 was compiled against the
# bundled (newer) oneTBB and carries symbol references USD's libtbb.12 cannot
# satisfy — same dlopen death as above, one dependency deeper. USD's embree
# exports every rtc* symbol the delegate uses (verified with nm/comm).
cmake -S "$CYCLES_SRC" -B "$CYCLES_SRC/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DPXR_ROOT="$PREFIX" \
    -DEMBREE_EMBREE4_LIBRARY="$PREFIX/lib/libembree4.dylib" \
    -DEMBREE_INCLUDE_DIR="$PREFIX/include" \
    -DWITH_CYCLES_HYDRA_RENDER_DELEGATE=ON \
    -DWITH_CYCLES_DEVICE_METAL=ON \
    -DCMAKE_INSTALL_PREFIX="$CYCLES_SRC/install"
cmake --build "$CYCLES_SRC/build" -j "$JOBS"
cmake --install "$CYCLES_SRC/build" --prefix "$CYCLES_SRC/install"

[ -f "$CYCLES_SRC/install/hydra/hdCycles.dylib" ] \
    || { echo "hdCycles.dylib missing after install" >&2; exit 1; }
otool -L "$CYCLES_SRC/install/hydra/hdCycles.dylib" | grep -q 'libtbb.12' \
    || { echo "hdCycles is not linked against oneTBB" >&2; exit 1; }

# ---------------------------------------------------------------------- env.sh
say "writing $WORKSPACE/env.sh"
cat > "$WORKSPACE/env.sh" <<EOF
# Generated by build-macos-metal.sh — source before usdview / usdrecord.
export PATH="$PREFIX/bin:$VENV/bin:\$PATH"
export PYTHONPATH="$PREFIX/lib/python3.11/site-packages\${PYTHONPATH:+:\$PYTHONPATH}"
export PXR_PLUGINPATH_NAME="$CYCLES_SRC/install/hydra\${PXR_PLUGINPATH_NAME:+:\$PXR_PLUGINPATH_NAME}"
# hdGp resolves generative procedurals only with this set (docs/14).
export HDGP_INCLUDE_DEFAULT_RESOLVER=1
# hdCycles renders black through the scene-index path of this build (lights
# and materials never reach the delegate) — force the legacy path for Cycles
# renders until that is understood (first W2 investigation item).
export USDIMAGINGGL_ENGINE_ENABLE_SCENE_INDEX=0
# Render controls (patched into hdCycles, see patches/): CYCLES_DEVICE=METAL
# selects the GPU, CYCLES_SAMPLES + CYCLES_DENOISE=1 + CYCLES_BACKGROUND=1 is
# the batch recipe (sample straight to target, denoise once) — measured 4.2
# s/frame at 960px/64 samples on an M2 vs 17 s at the 1024-sample default.
EOF

# ---------------------------------------------------------------------- verify
say "verify"
PATH="$PREFIX/bin:$VENV/bin:$PATH" \
PYTHONPATH="$PREFIX/lib/python3.11/site-packages" \
    python3.11 -c 'import pxr; print("pxr", pxr.Tf.__file__ and "importable")'
for lib in hdGp usdProcImaging usdImaging; do
    [ -e "$PREFIX/lib/libusd_${lib}.dylib" ] || { echo "missing lib: $lib" >&2; exit 1; }
done
[ -e "$PREFIX/plugin/usd/hdEmbree.dylib" ] || { echo "missing hdEmbree" >&2; exit 1; }

# The only verification that counts is the image, not the file listing: an
# actual Cycles render forces the dlopen that every linkage mistake above
# breaks (classic-TBB bindings, bundled-vs-USD oneTBB skew).
say "smoke render (Cycles, Metal)"
SMOKE="$WORKSPACE/smoke"
mkdir -p "$SMOKE"
cat > "$SMOKE/smoke.usda" <<'EOF'
#usda 1.0
(defaultPrim = "World")
def Xform "World" {
    def Sphere "ball" { double radius = 1 }
    def DistantLight "sun" { float inputs:intensity = 3000 }
}
EOF
PATH="$PREFIX/bin:$VENV/bin:$PATH" \
PYTHONPATH="$PREFIX/lib/python3.11/site-packages" \
PXR_PLUGINPATH_NAME="$CYCLES_SRC/install/hydra" \
    env USDIMAGINGGL_ENGINE_ENABLE_SCENE_INDEX=0 CYCLES_SAMPLES=16 \
    usdrecord --renderer Cycles --imageWidth 160 \
    "$SMOKE/smoke.usda" "$SMOKE/smoke-cycles.png"
[ -s "$SMOKE/smoke-cycles.png" ] || { echo "smoke render produced nothing" >&2; exit 1; }

echo "stack verified by render: pxr, hdGp, usdProcImaging, usdImaging, hdEmbree, hdCycles (Metal)"
echo "next: source $WORKSPACE/env.sh && usdrecord --renderer Cycles <stage> <out.png>"
