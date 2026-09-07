# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The farm render image: Blender LTS headless on NVIDIA — Cycles/OptiX for
# the photoreal tier, EEVEE for the fast tier — consuming OpenUSD stages
# natively (Blender's own USD importer; no OpenUSD build in this image).
#
#   docker buildx build --platform linux/amd64 \
#       -f integrations/blender/Dockerfile \
#       -t harbor.sportstracklive.com/tuile/blender-render:4.5 --push .
#
# Runtime (RunPod or any NVIDIA container host):
#   blender -b -P /opt/render/render_usd.py -- \
#       --stage /opt/render/stages/gate-anim.usda --tier cycles \
#       --frames 1:48 --width 960 --out /out/seq --video /out/seg.mp4
#
# Blender 4.5 LTS deliberately, not 5.x: the LTS still carries the built-in
# FFMPEG video output (5.x removed it) so a shard can encode its own segment,
# and an LTS is the right pin for a farm.

# The newest CUDA base that is actually schedulable: the base's
# NVIDIA_REQUIRE_CUDA gate demands a host driver at least this new, and
# RunPod's newest hosts run CUDA 13.2 (13.3 images would match no host).
# Pair pod creation with gpu.allowedCudaVersions >= 13.2. Blender ships its
# own CUDA/OptiX runtimes, so the base only sets driver expectations.
FROM nvidia/cuda:13.2.1-base-ubuntu24.04

ARG BLENDER_VERSION=4.5.13

ENV DEBIAN_FRONTEND=noninteractive

# libglvnd + EGL + X libs: EEVEE creates its GPU context headless through
# NVIDIA EGL (no X server); Cycles/OptiX only needs the driver, which the
# container runtime injects. The X client libs are Blender's own link-time
# dependencies — present even when no display ever exists.
# ffmpeg: a shard pod concatenates its per-process segments into ONE final
# video (stream copy, no re-encode) — assembly happens where the render is.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl xz-utils ffmpeg \
        libglvnd0 libgl1 libegl1 libgles2 \
        libx11-6 libxi6 libxxf86vm1 libxfixes3 libxrender1 libxkbcommon0 \
        libsm6 libice6 \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fL --retry 5 \
        -o /tmp/blender.tar.xz \
        "https://download.blender.org/release/Blender${BLENDER_VERSION%.*}/blender-${BLENDER_VERSION}-linux-x64.tar.xz" \
    && mkdir -p /opt/blender \
    && tar -xJf /tmp/blender.tar.xz -C /opt/blender --strip-components=1 \
    && rm /tmp/blender.tar.xz

ENV PATH=/opt/blender:$PATH

# Graphics capability so EEVEE gets an EGL context; harmless for Cycles.
ENV NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics

COPY integrations/blender/render_usd.py /opt/render/render_usd.py
COPY integrations/blender/render_job.sh /opt/render/render_job.sh
COPY integrations/blender/stages/ /opt/render/stages/

# Build-time assertion: Blender runs and the USD importer is present.
RUN blender -b --factory-startup --python-expr \
        "import bpy; assert hasattr(bpy.ops.wm, 'usd_import'); print('blender', bpy.app.version_string, 'usd import OK')"

CMD ["blender", "--version"]
