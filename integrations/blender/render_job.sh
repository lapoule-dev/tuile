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
#   JOB_LOGS_PUT_URL   if set: every process's stdout, gathered into
#                      logs.tar.gz and shipped the same way — ON EVERY EXIT
#                      PATH, because a job that failed is the one whose logs
#                      are worth having
#   JOB_TRACE_PUT_URL  if set: the per-process determinism traces, gathered
#                      into one trace.tar.gz and shipped the same way
#   JOB_PROFILE_PUT_URL if set: TUILE_PROFILE_DIR's flamegraphs, likewise
#   JOB_SSH_PUBKEY     if set: start sshd with this authorized key
#
# Contract, exact-or-die: the GPU probe must match JOB_GPUS or the job bails
# (NO-GPU-BAIL) without rendering a single CPU frame; every segment must
# exist or the job ends VIDEO-MISSING with a nonzero exit.
#
# And whatever happens, the archive is shipped. That is a trap, not a line at
# the end: the one run whose logs anyone wants is the one that did not finish,
# and the previous shape of this script uploaded nothing on the VIDEO-MISSING
# path, uploaded nothing on a bail, and wrote no process stdout to any file at
# all — it lived only in the RunPod console, which the launcher never reads.

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
outdir="$(dirname "$JOB_OUT")"
mkdir -p "$outdir"

# Everything this job says about itself, in a file rather than only in a
# console nobody will read once the pod is gone.
JOB_LOG="$outdir/job.log"
exec > >(tee -a "$JOB_LOG") 2>&1

# Ships one tarball to one presigned URL. Quiet about a URL that is not set;
# loud about one that is and fails.
ship() {
    local name="$1" url="$2"; shift 2
    [ -n "$url" ] || return 0
    ls "$@" > /dev/null 2>&1 || return 0
    tar czf "$outdir/$name" -C "$outdir" $(cd "$outdir" && ls "$@" 2>/dev/null) \
        || { echo "ARCHIVE-TAR-FAILED $name"; return 1; }
    echo "$name: $(du -h "$outdir/$name" | cut -f1)"
    if curl -fsS -T "$outdir/$name" "$url" > /dev/null; then
        echo "ARCHIVE-UP $name"
    else
        echo "ARCHIVE-UP-FAILED $name"
    fi
}

# Called on EVERY exit, successful or not. This is the whole point: three
# measurements died with their machine in two days, and the last one was a
# 60-second render whose pod was reclaimed mid-flight.
archive_everything() {
    local status=$?
    trap - EXIT
    ship logs.tar.gz    "${JOB_LOGS_PUT_URL:-}"    'log-s*.txt' 'job.log'
    ship trace.tar.gz   "${JOB_TRACE_PUT_URL:-}"   'trace-s*.jsonl'
    ship profile.tar.gz "${JOB_PROFILE_PUT_URL:-}" 'profile'
    exit $status
}
trap archive_everything EXIT

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
elif [ -n "${JOB_STAGE_URL:-}" ]; then
    # A real recorded track (thousands of exact f64 camera samples) is
    # irreducible data; it travels as a presigned GET.
    curl -fsS -o "$STAGE" "$JOB_STAGE_URL" || { echo STAGE-FETCH-FAILED; exit 1; }
elif [ -n "${JOB_TRAJECTORY:-}" ]; then
    # The generative road: the pod builds its own manifest from parameters.
    #   orbit:frames:lon:lat:radius_m:alt_m   (defaults after the kind)
    #   zoom:frames_each_way
    IFS=: read -r kind p1 p2 p3 p4 p5 <<< "$JOB_TRAJECTORY"
    case "$kind" in
        orbit) /opt/tuile/bin/orbit-tape /tmp/traj.mcap \
                   "${p1:-1440}" "${p2:-2.17}" "${p3:-42.52}" \
                   "${p4:-8000}" "${p5:-5000}" ;;
        zoom)  /opt/tuile/bin/zoom-tape /tmp/traj.mcap "${p1:-64}" ;;
        *) echo "TRAJECTORY-UNKNOWN: $kind"; exit 1 ;;
    esac
    /opt/tuile/bin/tape-to-stage /tmp/traj.mcap "$STAGE" \
        --viewport "${JOB_VIEWPORT:-1280x960}" --sse "${JOB_SSE:-3}" \
        --fps "$JOB_FPS" || { echo MANIFEST-GEN-FAILED; exit 1; }
fi
[ -s "$STAGE" ] || { echo "stage absente: $STAGE" >&2; exit 1; }

t0=$(date +%s)
for i in $(seq 0 $((jobs - 1))); do
    gpu=$((i / JOB_PROCS_PER_GPU))
    a=$((first + i * span))
    if [ "$i" = "$((jobs - 1))" ]; then b=$last; else b=$((first + (i + 1) * span - 1)); fi
    # One tile cache per process, SEEDED from the shared one: the store is a
    # single-writer design (two writers corrupt it quietly), so sharing is
    # done by copy-on-start — a warm seed means each process starts at ~100%
    # hit rate and only pays network for its own range's novelty. The real
    # shared cache is the M2 architecture (one GeometryStream session, N
    # consumers), not a shared directory.
    seed="${TUILE_CACHE_DIR:-/tmp/tuile-cache}"
    proc_cache="$seed/r$i"
    if [ -d "$seed" ] && [ ! -d "$proc_cache" ] && ls "$seed"/foyer-* > /dev/null 2>&1; then
        mkdir -p "$proc_cache"
        cp -a "$seed"/foyer-* "$proc_cache"/ 2>/dev/null || true
    fi
    CUDA_VISIBLE_DEVICES=$gpu TUILE_CACHE_DIR="$proc_cache" \
        stdbuf -oL blender -b -P /opt/render/render_usd.py -- \
        --stage "$STAGE" --engine "$JOB_ENGINE" --tier "$JOB_TIER" \
        --frames "$a:$b" --width "$JOB_WIDTH" \
        --samples "$JOB_SAMPLES" --adaptive-threshold "$JOB_THRESHOLD" \
        --batch-frames "$JOB_BATCH_FRAMES" $JOB_EXTRA_ARGS \
        --out "$outdir/s$i" --video "$outdir/seg$i.mp4" \
        2> "$outdir/trace-s$i.jsonl" \
        | grep --line-buffered -vE '^(Fra:|Saved:|Time:|Append frame)' \
        | sed -u "s/^/[gpu$gpu-j$i] /" \
        | tee -a "$outdir/log-s$i.txt" &
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
# Segments are counted by SIZE, not by existence.
#
# `render_usd.py` creates its output file up front, so a process that dies
# leaves a 48-byte container behind — which `ls | wc -l` counts as a segment.
# Five of sixteen died once (all sixteen had landed on one GPU and run it out
# of VRAM); `ffmpeg concat -c copy` stopped at the first empty file, and the
# job reported RENDER-DONE with 180 frames of 1440. Nobody could tell from the
# outside: the video played, it was simply four fifths shorter than asked for.
n=0
missing=""
for i in $(seq 0 $((jobs - 1))); do
    if [ "$(stat -c%s "$outdir/seg$i.mp4" 2>/dev/null || echo 0)" -gt 1000 ]; then
        n=$((n + 1))
    else
        missing="$missing $i"
    fi
done
if [ -n "$missing" ]; then
    echo "SEGMENTS-MISSING:$missing"
fi
if [ "$n" = "$jobs" ]; then
    for i in $(seq 0 $((jobs - 1))); do echo "file '$outdir/seg$i.mp4'"; done > "$outdir/list.txt"
    ffmpeg -y -f concat -safe 0 -i "$outdir/list.txt" -c copy "$JOB_OUT" 2>&1 | tail -3
    # And the result must carry every frame that was asked for. A concat that
    # silently drops a segment produces a shorter film, not an error.
    got=$(ffprobe -v error -count_frames -select_streams v:0 \
              -show_entries stream=nb_read_frames -of csv=p=0 "$JOB_OUT" 2>/dev/null)
    if [ -n "$got" ] && [ "$got" != "$total" ]; then
        echo "FRAME-COUNT-MISMATCH: $got frames dans la vidéo, $total demandées"
    else
        echo "frames: ${got:-inconnu}/$total"
    fi
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
    # Two hours sat here, from when the only way to get anything off a pod was
    # to be there while it lived. Everything now leaves through the archive, so
    # this is a courtesy window for an ssh session, not a lifeline.
    sleep "${JOB_DONE_SLEEP:-60}"
else
    echo VIDEO-MISSING
    exit 1
fi
