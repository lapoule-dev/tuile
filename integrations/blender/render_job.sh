#!/bin/bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The farm job: renders one frame range of one OpenUSD stage into ONE video,
# split across every GPU of the host. Baked into the image at
# /opt/render/render_job.sh; a pod's args are just `bash /opt/render/render_job.sh`
# plus environment. Everything is a parameter — no constant in here deserves
# an image rebuild.
#
#   JOB_STAGE_B64_GZ   gzip+base64 of the .usda (or JOB_STAGE=path in image)
#   JOB_FRAMES         "1:1440" (inclusive)
#   JOB_ENGINE         native | hydra            (default native; hydra = the
#                      manifest path, HYDRA_STORM, procedurals cook — needs
#                      TUILE_ION_TOKEN in the pod env)
#   JOB_TIER           cycles | eevee            (default cycles; native only)
#   JOB_WIDTH          pixels                    (default 1920)
#   JOB_SAMPLES        max samples               (default 128)
#   JOB_THRESHOLD      adaptive noise threshold  (default 0.05)
#   JOB_GPUS           expected GPU count        (default 1; probe must agree)
#   JOB_PROCS_PER_GPU  concurrent renders/GPU    (default 4)
#   JOB_BATCH_FRAMES   progress log granularity  (default 60)
#   JOB_EXTRA_ARGS     extra render_usd.py args  (e.g. "--demo-fixups --no-dof")
#   JOB_OUT            final video path          (default /out/render.mp4)
#   JOB_UPLOAD_PUT_URL if set: curl -T the finished video to this presigned
#                      URL (no credential ever reaches the pod)
#   JOB_SSH_PUBKEY     if set: start sshd with this authorized key
#
# Contract, exact-or-die: the GPU probe must match JOB_GPUS or the job bails
# (NO-GPU-BAIL) without rendering a single CPU frame; every segment must
# exist or the job ends VIDEO-MISSING with a nonzero exit.

set -uo pipefail

JOB_FRAMES="${JOB_FRAMES:?JOB_FRAMES requis (ex: 1:1440)}"
JOB_ENGINE="${JOB_ENGINE:-native}"
JOB_TIER="${JOB_TIER:-cycles}"
JOB_WIDTH="${JOB_WIDTH:-1920}"
JOB_SAMPLES="${JOB_SAMPLES:-128}"
JOB_THRESHOLD="${JOB_THRESHOLD:-0.05}"
JOB_GPUS="${JOB_GPUS:-1}"
JOB_PROCS_PER_GPU="${JOB_PROCS_PER_GPU:-4}"
JOB_BATCH_FRAMES="${JOB_BATCH_FRAMES:-60}"
JOB_FPS="${JOB_FPS:-24}"
JOB_EXTRA_ARGS="${JOB_EXTRA_ARGS:-}"
JOB_OUT="${JOB_OUT:-/out/render.mp4}"

first="${JOB_FRAMES%%:*}"; last="${JOB_FRAMES##*:}"
total=$((last - first + 1))
jobs=$((JOB_GPUS * JOB_PROCS_PER_GPU))
span=$((total / jobs))
[ "$span" -ge 1 ] || { echo "plage trop courte pour $jobs processus" >&2; exit 1; }

# GPU probe first: never a silent CPU render. Storm draws through GL, not
# OptiX, so the hydra engine probes the driver itself.
if [ "$JOB_ENGINE" = "hydra" ]; then
    probe="NGPU $(nvidia-smi -L 2>/dev/null | grep -c '^GPU')"
else
    probe=$(blender -b --python-expr "import bpy; p = bpy.context.preferences.addons['cycles'].preferences; p.compute_device_type = 'OPTIX'; p.get_devices(); print('NGPU', sum(1 for d in p.devices if d.type == 'OPTIX'))" 2>&1 | grep -oE 'NGPU [0-9]+' | head -1)
fi
echo "probe: $probe (attendu: NGPU $JOB_GPUS)"
if [ "$probe" != "NGPU $JOB_GPUS" ]; then
    echo NO-GPU-BAIL
    sleep "${JOB_BAIL_SLEEP:-600}"
    exit 1
fi
if [ "$JOB_ENGINE" = "hydra" ] && [ -z "${TUILE_ION_TOKEN:-}" ]; then
    echo NO-TOKEN-BAIL
    sleep "${JOB_BAIL_SLEEP:-600}"
    exit 1
fi

if [ -n "${JOB_SSH_PUBKEY:-}" ]; then
    apt-get update -qq && apt-get install -y -qq openssh-server > /dev/null
    mkdir -p /root/.ssh /run/sshd
    echo "$JOB_SSH_PUBKEY" > /root/.ssh/authorized_keys
    chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys
    /usr/sbin/sshd -p 22
fi

STAGE="${JOB_STAGE:-/tmp/job-stage.usda}"
if [ -n "${JOB_STAGE_B64_GZ:-}" ]; then
    echo "$JOB_STAGE_B64_GZ" | base64 -d | gunzip > "$STAGE"
fi
[ -s "$STAGE" ] || { echo "stage absente: $STAGE" >&2; exit 1; }

outdir="$(dirname "$JOB_OUT")"
mkdir -p "$outdir"
t0=$(date +%s)
for i in $(seq 0 $((jobs - 1))); do
    gpu=$((i / JOB_PROCS_PER_GPU))
    a=$((first + i * span))
    if [ "$i" = "$((jobs - 1))" ]; then b=$last; else b=$((first + (i + 1) * span - 1)); fi
    CUDA_VISIBLE_DEVICES=$gpu stdbuf -oL blender -b -P /opt/render/render_usd.py -- \
        --stage "$STAGE" --engine "$JOB_ENGINE" --tier "$JOB_TIER" \
        --frames "$a:$b" --width "$JOB_WIDTH" \
        --samples "$JOB_SAMPLES" --adaptive-threshold "$JOB_THRESHOLD" \
        --batch-frames "$JOB_BATCH_FRAMES" $JOB_EXTRA_ARGS \
        --out "$outdir/s$i" --video "$outdir/seg$i.mp4" 2>&1 \
        | grep --line-buffered -vE '^(Fra:|Saved:|Time:|Append frame)' \
        | sed -u "s/^/[gpu$gpu-j$i] /" &
done
wait
echo "WALL: $(($(date +%s) - t0))s pour $total frames en $jobs processus / $JOB_GPUS GPU"

# Blender 5.x has no built-in encoder: the driver leaves PNG sequences and
# each range is encoded here with the static ffmpeg.
for i in $(seq 0 $((jobs - 1))); do
    if [ ! -s "$outdir/seg$i.mp4" ] && ls "$outdir/s$i".*.png > /dev/null 2>&1; then
        a=$((first + i * span))
        ffmpeg -y -framerate "$JOB_FPS" -start_number "$a" \
            -i "$outdir/s$i.%d.png" -c:v libx264 -pix_fmt yuv420p -crf 18 \
            "$outdir/seg$i.mp4" > /dev/null 2>&1 && rm -f "$outdir/s$i".*.png
    fi
done
n=$(ls "$outdir"/seg*.mp4 2>/dev/null | wc -l)
if [ "$n" = "$jobs" ]; then
    for i in $(seq 0 $((jobs - 1))); do echo "file '$outdir/seg$i.mp4'"; done > "$outdir/list.txt"
    ffmpeg -y -f concat -safe 0 -i "$outdir/list.txt" -c copy "$JOB_OUT" > /dev/null 2>&1
fi
if [ -s "$JOB_OUT" ]; then
    ls -la "$JOB_OUT"
    if [ -n "${JOB_UPLOAD_PUT_URL:-}" ]; then
        # Presigned PUT: the pod ships its own result home and no credential
        # ever reaches it.
        if curl -fsS -T "$JOB_OUT" "$JOB_UPLOAD_PUT_URL" > /dev/null; then
            echo UPLOAD-DONE
        else
            echo UPLOAD-FAILED
        fi
    fi
    echo RENDER-DONE
    sleep "${JOB_DONE_SLEEP:-7200}"
else
    echo VIDEO-MISSING
    exit 1
fi
