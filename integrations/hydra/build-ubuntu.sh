#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Native Ubuntu (24.04, x86_64) build of the render stack: OpenUSD 26.08 plus
# the Cycles Hydra delegate — CUDA when nvcc is present, CPU otherwise. The
# twin of build-macos-metal.sh: same pins, same patches, same guards; the apt
# list and the headless/EGL posture come from Dockerfile.openusd.
#
# NOT yet executed on a real Ubuntu host — transposed from the proven macOS
# recipe and the proven arm64 Docker build. First run on the farm should be
# watched.
#
# Idempotent and resumable. Result: $WORKSPACE/env.sh.
#
#   ./integrations/hydra/build-ubuntu.sh [workspace]   # default ~/openusd
#
# usdview is off by default (headless nodes); TUILE_USDVIEW=1 enables it.

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
say "prerequisites (apt list from Dockerfile.openusd + git-lfs, venv)"
missing=""
for pkg in build-essential cmake git git-lfs curl ca-certificates \
           python3 python3-dev python3-venv python3-pip \
           libx11-dev libxt-dev libgl1-mesa-dev libglu1-mesa-dev \
           libglvnd-dev libegl1 libegl1-mesa-dev \
           zlib1g-dev libbz2-dev liblzma-dev; do
    dpkg -s "$pkg" >/dev/null 2>&1 || missing="$missing $pkg"
done
[ -z "$missing" ] || { echo "run first: sudo apt-get install -y$missing" >&2; exit 1; }

# Job count bounded by memory, not cores: 1.5 GB per compiler is measured
# (hdSt/usdImaging TUs). Read the cgroup limit when there is one — inside a
# pod, /proc/meminfo lies (it reports the node).
MEM_BYTES="$(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo max)"
case "$MEM_BYTES" in
    ''|max|*[!0-9]*) MEM_BYTES="$(awk '/MemTotal/ {print $2 * 1024}' /proc/meminfo)" ;;
esac
JOBS=$(( MEM_BYTES / 1500000000 ))
CPUS=$(nproc)
[ "$JOBS" -lt 1 ] && JOBS=1
[ "$JOBS" -gt "$CPUS" ] && JOBS=$CPUS
echo "building with -j $JOBS ($((MEM_BYTES / 1024 / 1024)) MiB, $CPUS cpus)"

mkdir -p "$WORKSPACE/src"

# ------------------------------------------------------------------------ venv
say "python venv"
[ -d "$VENV" ] || python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip jinja2
USDVIEW_FLAG="--no-usdview"
if [ "${TUILE_USDVIEW:-0}" = "1" ]; then
    "$VENV/bin/pip" install --quiet PySide6 PyOpenGL
    USDVIEW_FLAG="--usdview"
fi

# ---------------------------------------------------------------- OpenUSD source
say "OpenUSD $USD_VERSION source"
if [ ! -d "$USD_SRC" ]; then
    curl -fL --retry 5 -o "$WORKSPACE/src/openusd.tar.gz" \
        "https://github.com/PixarAnimationStudios/OpenUSD/archive/refs/tags/v$USD_VERSION.tar.gz"
    tar xzf "$WORKSPACE/src/openusd.tar.gz" -C "$WORKSPACE/src"
    rm "$WORKSPACE/src/openusd.tar.gz"
fi

# Same determinism as macOS: OIIO's ffmpeg plugin is off everywhere (nothing
# here decodes video, and OIIO 2.5 does not build against ffmpeg 8 anyway).
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

# Classic-TBB remnants in an existing prefix: the two ABIs are incompatible,
# and stale dylibs keep two-level (macOS) / versioned (ELF) bindings to
# symbols oneTBB no longer exports. Purge and force-rebuild the TBB linkers.
FORCE_DEPS=""
if [ -f "$PREFIX/include/tbb/tbb_stddef.h" ]; then
    say "purging classic TBB from the prefix"
    rm -rf "$PREFIX/include/tbb" "$PREFIX/include/oneapi"
    rm -f "$PREFIX"/lib/libtbb*
    FORCE_DEPS="--force OpenImageIO --force OpenVDB --force Embree"
fi

# ---------------------------------------------------------------- OpenUSD build
# --onetbb is REQUIRED: the Cycles delegate ships against oneTBB and a
# classic-TBB USD cannot host it. (Dockerfile.openusd predates this and must
# gain the same flag.)
# CMAKE_POLICY_VERSION_MINIMUM=3.5 only matters on CMake >= 4; harmless below.
build_usd() {
    CMAKE_POLICY_VERSION_MINIMUM=3.5 PATH="$VENV/bin:$PATH" \
    "$VENV/bin/python" -u "$USD_SRC/build_scripts/build_usd.py" \
        --python --usd-imaging --embree \
        --materialx --openimageio --opencolorio --openvdb \
        --alembic --ptex --draco "$USDVIEW_FLAG" \
        --onetbb $FORCE_DEPS \
        --no-examples --no-tutorials --no-tests --no-docs --no-python-docs \
        --no-prman --no-vulkan \
        -j "$JOBS" "$PREFIX"
}

say "OpenUSD build"
if ! build_usd; then
    # Alembic 1.8.5 hard-sets CMP0042 OLD; CMake 4 rejects it (the env floor
    # cannot override an explicit SET). Patch the extracted source and resume.
    alembic=$(ls -d "$PREFIX"/src/alembic-*/CMakeLists.txt 2>/dev/null | head -1)
    if [ -n "$alembic" ] && grep -q 'CMP0042 OLD' "$alembic"; then
        say "patching Alembic CMP0042 OLD -> NEW, resuming"
        sed -i 's/CMP0042 OLD/CMP0042 NEW/' "$alembic"
        build_usd
    else
        echo "OpenUSD build failed for a reason this script does not know" >&2
        exit 1
    fi
fi

ldd "$PREFIX/lib/libusd_usd.so" | grep -q 'libtbb.so.12' \
    || { echo "USD is not linked against oneTBB — refusing to continue" >&2; exit 1; }

# ---------------------------------------------------------------------- Cycles
DEVICE_FLAGS="-DWITH_CYCLES_DEVICE_CUDA=OFF"
DEVICE_NAME="CPU"
if command -v nvcc >/dev/null; then
    DEVICE_FLAGS="-DWITH_CYCLES_DEVICE_CUDA=ON"
    DEVICE_NAME="CUDA"
fi
say "Cycles delegate ($DEVICE_NAME)"
if [ ! -d "$CYCLES_SRC" ]; then
    git clone https://projects.blender.org/blender/cycles.git "$CYCLES_SRC"
fi
git -C "$CYCLES_SRC" checkout --quiet "$CYCLES_REF"
# Fetches the precompiled dependency libs (lib/linux_x86_64, git-lfs).
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
# references symbols USD's libtbb.so.12 does not export and dlopen fails at
# run time. USD's libtbb must exist before Cycles configures, and a build dir
# configured before it did must be thrown away.
[ -f "$PREFIX/lib/libtbb.so.12" ] \
    || { echo "no libtbb.so.12 in the prefix — USD build incomplete" >&2; exit 1; }

# Embree must be USD's too: the bundled libembree4 was compiled against the
# bundled (newer) oneTBB and carries symbol references USD's libtbb cannot
# satisfy — same dlopen death as above, one dependency deeper (proven on
# macOS; the precompiled linux libs are built the same way).
cmake -S "$CYCLES_SRC" -B "$CYCLES_SRC/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DPXR_ROOT="$PREFIX" \
    -DEMBREE_EMBREE4_LIBRARY="$PREFIX/lib/libembree4.so" \
    -DEMBREE_INCLUDE_DIR="$PREFIX/include" \
    -DWITH_CYCLES_HYDRA_RENDER_DELEGATE=ON \
    $DEVICE_FLAGS \
    -DCMAKE_INSTALL_PREFIX="$CYCLES_SRC/install"
cmake --build "$CYCLES_SRC/build" -j "$JOBS"
cmake --install "$CYCLES_SRC/build" --prefix "$CYCLES_SRC/install"

[ -f "$CYCLES_SRC/install/hydra/hdCycles.so" ] \
    || { echo "hdCycles.so missing after install" >&2; exit 1; }

# ---------------------------------------------------------------------- env.sh
say "writing $WORKSPACE/env.sh"
PYSP="$(ls -d "$PREFIX"/lib/python*/site-packages | head -1)"
cat > "$WORKSPACE/env.sh" <<EOF
# Generated by build-ubuntu.sh — source before usdrecord / usdview.
export PATH="$PREFIX/bin:$VENV/bin:\$PATH"
export LD_LIBRARY_PATH="$PREFIX/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
export PYTHONPATH="$PYSP\${PYTHONPATH:+:\$PYTHONPATH}"
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
# Storm on a headless node has no other way to get a GL context.
export PXR_ENABLE_GL_CONTEXT_EGL=1
EOF

# ---------------------------------------------------------------------- verify
say "verify"
PATH="$PREFIX/bin:$VENV/bin:$PATH" LD_LIBRARY_PATH="$PREFIX/lib" PYTHONPATH="$PYSP" \
    python3 -c 'import pxr; print("pxr importable")'
for lib in hdGp usdProcImaging usdImaging; do
    [ -e "$PREFIX/lib/libusd_${lib}.so" ] || { echo "missing lib: $lib" >&2; exit 1; }
done
[ -e "$PREFIX/plugin/usd/hdEmbree.so" ] || { echo "missing hdEmbree" >&2; exit 1; }

# The only verification that counts is the image, not the file listing: an
# actual Cycles render forces the dlopen that every linkage mistake above
# breaks (classic-TBB bindings, bundled-vs-USD oneTBB skew).
say "smoke render (Cycles, $DEVICE_NAME)"
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
PATH="$PREFIX/bin:$VENV/bin:$PATH" LD_LIBRARY_PATH="$PREFIX/lib" \
PYTHONPATH="$PYSP" \
PXR_PLUGINPATH_NAME="$CYCLES_SRC/install/hydra" \
    env USDIMAGINGGL_ENGINE_ENABLE_SCENE_INDEX=0 CYCLES_SAMPLES=16 \
    usdrecord --renderer Cycles --imageWidth 160 \
    "$SMOKE/smoke.usda" "$SMOKE/smoke-cycles.png"
[ -s "$SMOKE/smoke-cycles.png" ] || { echo "smoke render produced nothing" >&2; exit 1; }

echo "stack verified by render: pxr, hdGp, usdProcImaging, usdImaging, hdEmbree, hdCycles ($DEVICE_NAME)"
echo "next: source $WORKSPACE/env.sh && usdrecord --renderer Cycles <stage> <out.png>"
