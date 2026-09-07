# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The farm render image, layered on the shared blender-usd base (Ubuntu +
# Blender 5.1 + OpenUSD 25.08 prefix). Adds only what render jobs need: a
# static ffmpeg for the segment concat (Blender 5.1 linux encodes video
# natively; ffmpeg only stream-copies segments together) and the
# driver/job scripts.
#
#   docker buildx build --builder stl-builder-k8s --platform linux/amd64 \
#       -f integrations/blender/Dockerfile \
#       -t harbor.sportstracklive.com/stl/blender-render:5.1 --push .
FROM harbor.sportstracklive.com/stl/blender-usd-base:5.1-2508

# Static ffmpeg (one binary): segment encode + stream-copy concat on the pod.
RUN curl -fL --retry 5 -o /tmp/ffmpeg.tar.xz \
        "https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz" \
    && tar -xJf /tmp/ffmpeg.tar.xz -C /tmp \
    && mv /tmp/ffmpeg-*-static/ffmpeg /usr/local/bin/ffmpeg \
    && rm -rf /tmp/ffmpeg*

COPY integrations/blender/render_usd.py /opt/render/render_usd.py
COPY integrations/blender/render_job.sh /opt/render/render_job.sh
COPY integrations/blender/stages/ /opt/render/stages/

# Build-time assertions: the USD importer is present, and the built-in
# video encoder actually ENCODES — two Cycles CPU frames become a non-empty
# mp4 during the build, or the image does not exist. "The enum lists FFMPEG"
# is a claim; this is a proof.
RUN blender -b --factory-startup --python-exit-code 1 --python-expr \
        "import bpy; assert hasattr(bpy.ops.wm, 'usd_import'); print('blender', bpy.app.version_string, 'usd import OK')" \
    && blender -b --factory-startup --python-exit-code 1 --python-expr "import bpy; sc = bpy.context.scene; sc.render.engine = 'CYCLES'; sc.cycles.samples = 1; sc.render.resolution_x = 64; sc.render.resolution_y = 48; sc.frame_start = 1; sc.frame_end = 2; sc.render.image_settings.media_type = 'VIDEO'; sc.render.image_settings.file_format = 'FFMPEG'; sc.render.ffmpeg.format = 'MPEG4'; sc.render.ffmpeg.codec = 'H264'; sc.render.filepath = '/tmp/encode-proof.mp4'; bpy.ops.render.render(animation=True)" \
    && test -s /tmp/encode-proof*.mp4 && echo "encodeur video intégré: PROUVÉ" && rm -f /tmp/encode-proof*

CMD ["blender", "--version"]
