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
#                      manifest path, procedurals cook — needs a pack
#                      (JOB_PACK_URL) or TUILE_ION_TOKEN in the pod env)
#   JOB_DELEGATE       storm | cycles            (hydra only; default cycles.
#                      Storm is an OpenGL rasteriser and never calls CUDA, so
#                      CUDA_VISIBLE_DEVICES steers nothing and four processes
#                      told to take four GPUs were measured all on GPU 2.)
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
#                      PATH and every JOB_ARCHIVE_EVERY seconds while it runs,
#                      because a job that failed is the one whose logs are
#                      worth having and a pod that is killed runs no trap
#   JOB_SEG_PUT_URL_<i> if set: segment i goes to R2 THE MOMENT it is encoded,
#                      not at the end. A pod reclaimed at frame 900 of 1440
#                      has still deposited the nine hundred.
#   JOB_ARCHIVE_EVERY  seconds between log flushes while rendering (default 300)
#   JOB_TRACE_PUT_URL  if set: the per-process determinism traces, gathered
#                      into one trace.tar.gz and shipped the same way
#   JOB_PROFILE_PUT_URL if set: TUILE_PROFILE_DIR's flamegraphs, likewise
#   JOB_PACK_URL       if set: a presigned GET for a pre-baked pack. The job
#                      downloads it ONCE and every process reads it — no ion
#                      token, no network, no traversal. This is the normal
#                      shape of a render now; the streaming path is what runs
#                      when nobody baked.
#   JOB_SCENE          the scene digest the pack must answer, or empty to
#                      accept whatever pack it is given
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
JOB_DELEGATE="${JOB_DELEGATE:-cycles}"
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

# Flushes the logs while the job is still running.
#
# The trap below covers every way this script can decide to stop. It does not
# cover the way a pod actually dies: reclaimed, SIGKILL, no trap, nothing
# written. So the logs go up periodically as well — the cost is one upload of a
# few hundred kilobytes every five minutes, and what it buys is knowing what a
# machine was doing at the moment it was taken away.
flush_logs() {
    [ -n "${JOB_LOGS_PUT_URL:-}" ] || return 0
    while sleep "${JOB_ARCHIVE_EVERY:-300}"; do
        ship logs.tar.gz "$JOB_LOGS_PUT_URL" 'log-s*.txt' 'job.log' > /dev/null 2>&1
    done
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
flush_logs &
log_flusher=$!

first="${JOB_FRAMES%%:*}"; last="${JOB_FRAMES##*:}"
total=$((last - first + 1))
jobs=$((JOB_GPUS * JOB_PROCS_PER_GPU))
span=$((total / jobs))
[ "$span" -ge 1 ] || { echo "plage trop courte pour $jobs processus" >&2; exit 1; }

# sshd first, when one was asked for.
#
# It sat after the GPU probe, which is precisely backwards: ssh exists to
# diagnose a job that went wrong, and the probe is the thing that goes wrong.
# A bail exits before this line ever ran, so the one pod launched WITH a key
# was the one pod with no sshd on it.
if [ -n "${JOB_SSH_PUBKEY:-}" ]; then
    apt-get update -qq && apt-get install -y -qq openssh-server > /dev/null
    mkdir -p /root/.ssh /run/sshd
    echo "$JOB_SSH_PUBKEY" > /root/.ssh/authorized_keys
    chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys
    /usr/sbin/sshd -p 22
    echo "sshd up"
fi

# Wake the driver before asking anything about it.
#
# `nvidia_uvm` is not initialised inside a container until some NVIDIA
# application triggers it, and until then `cuInit` returns CUDA_ERROR_UNKNOWN
# — nvidia-smi works throughout, because NVML is a different path. Documented
# on NVIDIA's own forum for containers and sandboxes, and reproduced here
# exactly: this job ran Blender first and called nvidia-smi only afterwards,
# in the bail diagnostic, so the wake-up always came one step too late.
#
# Costs a fraction of a second. Its output is kept, because "which GPUs did
# this pod actually have" is the first question of any post-mortem.
echo "--- driver wake-up ---"
nvidia-smi -L 2>&1 | head -8 || echo "  nvidia-smi absent (CPU host?)"

# Can CUDA start at all on this host?
#
# Asked before Blender, because it is cheaper and because it separates two
# faults that look identical from the outside: an image with no GPU backend,
# and a host whose driver will not initialise. Measured on a community-cloud
# 4x5090 node — every ioctl on /dev/nvidia* succeeding, the base driver fine,
# and `open("/dev/nvidia-uvm")` returning EIO with the correct major, correct
# minor and 0666. libcuda is the HOST's, injected by the container runtime, so
# nothing in our image can cause or cure it; `nvidia-modprobe` cannot load a
# module from inside a container.
#
# The tell is printed with it: that host listed FIVE GPUs in
# /proc/driver/nvidia/gpus while /dev held four nodes with a gap at nvidia3.
# A pod given a subset of a machine's GPUs without a filtered procfs is a pod
# where UVM refuses to open, and the only cure is a different host.
if command -v nvidia-smi > /dev/null 2>&1; then
    cuda_ok=$(python3 - <<'CUDA' 2>/dev/null
import ctypes
try:
    print(ctypes.CDLL("libcuda.so.1").cuInit(0))
except OSError:
    print(-1)
CUDA
)
    in_proc=$(ls /proc/driver/nvidia/gpus 2>/dev/null | wc -l)
    in_dev=$(ls /dev/nvidia[0-9]* 2>/dev/null | wc -l)
    echo "cuda: cuInit=$cuda_ok  gpus in procfs=$in_proc  device nodes=$in_dev"
    if [ "$cuda_ok" != "0" ]; then
        echo "GPU-HOST-BROKEN: cuInit returned $cuda_ok on this host."
        [ "$in_proc" != "$in_dev" ] && echo             "  the driver advertises $in_proc GPUs and this container has $in_dev"             "device nodes — the pod holds a subset of the machine without a"             "filtered procfs, and UVM refuses to open. Retry on another host."
        ls /dev/nvidia[0-9]* 2>/dev/null | tr '\n' ' ' | sed 's/^/  nodes: /'; echo
        sleep "${JOB_BAIL_SLEEP:-600}"
        exit 1
    fi
fi

# GPU probe first: never a silent CPU render.
#
# What is counted matters, and it was wrong twice.
#
# `nvidia-smi -L | grep -c GPU` counts cards the driver can see, which is not
# the same question as "how many devices can the renderer put work on" — and
# answering the easy question is how sixteen processes ended up sharing one GPU
# while the probe reported four. Only Storm, which draws through GL, has any
# business counting cards.
#
# And OPTIX is not a given. Measured 2026-09-10 on stl/blender-globe:5.1-su:
# `compute_device_type` accepts only ('NONE','CUDA','HIP','ONEAPI') — this
# Blender was built without OptiX. So the probe asks the build what it has,
# preferring OPTIX and settling for CUDA, and SAYS WHICH. A backend chosen
# silently is a render that is slower than it should be with nothing to say so.
if [ "$JOB_ENGINE" = "hydra" ] && [ "$JOB_DELEGATE" = "storm" ]; then
    probe="NGPU $(nvidia-smi -L 2>/dev/null | grep -c '^GPU')"
    CYCLES_BACKEND=none
else
    probe_out=$(blender -b --python-expr "
import bpy
prefs = bpy.context.preferences.addons['cycles'].preferences
why = []
for backend in ('OPTIX', 'CUDA'):
    try:
        prefs.compute_device_type = backend
    except TypeError as e:
        # Blender's own message names what this build really offers, and it is
        # the only reliable source: reading enum_items answers [] here.
        why.append(str(e).split('not found in')[-1].strip())
        continue
    prefs.get_devices()
    n = sum(1 for d in prefs.devices if d.type == backend)
    why.append('%s:%d' % (backend, n))
    if n:
        print('NGPU', n, backend)
        break
else:
    print('NGPU 0 none')
print('BACKENDS', ' '.join(why))
" 2>&1)
    echo "$probe_out" | grep -oE '^BACKENDS .*' | head -1
    probe_out=$(echo "$probe_out" | grep -oE 'NGPU [0-9]+ [A-Za-z]+' | head -1)
    probe="${probe_out% *}"
    CYCLES_BACKEND="${probe_out##* }"
fi
echo "probe: $probe backend=$CYCLES_BACKEND (attendu: NGPU $JOB_GPUS)"
if [ "$probe" != "NGPU $JOB_GPUS" ]; then
    # Everything a person would ask for next, gathered before the pod is gone.
    #
    # A bail that only says "no GPU" costs another pod to diagnose, and this
    # one has already cost three. The build side and the driver side fail
    # identically from the outside — Cycles reports zero devices either way —
    # so both are printed: what the driver can see, whether its libraries are
    # reachable, and what Blender was offered.
    echo "--- what the driver sees ---"
    nvidia-smi -L 2>&1 | head -8 || echo "nvidia-smi absent"
    nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>&1 | head -4
    echo "--- driver libraries in the container ---"
    ldconfig -p 2>/dev/null | grep -E "libcuda\.so|libnvoptix|libnvidia-ml" \
        | sed 's/^\s*/  /' | head -6 || echo "  none"
    echo "  NVIDIA_VISIBLE_DEVICES=${NVIDIA_VISIBLE_DEVICES:-<unset>}"
    echo "  NVIDIA_DRIVER_CAPABILITIES=${NVIDIA_DRIVER_CAPABILITIES:-<unset>}"
    echo "  CUDA_VISIBLE_DEVICES=${CUDA_VISIBLE_DEVICES:-<unset>}"
    echo "--- device nodes ---"
    ls /dev/nvidia* 2>&1 | tr '\n' ' ' | sed 's/^/  /'; echo
    echo "--- cuInit, with its number ---"
    python3 - <<'CUDA' 2>&1 | sed 's/^/  /'
import ctypes, os
try:
    lib = ctypes.CDLL("libcuda.so.1")
except OSError as e:
    print("libcuda.so.1 will not load:", e); raise SystemExit
# The numeric code is the diagnosis. 100 = no device, 304 = OS call failed,
# 802 = system not yet initialised, 999 = unknown — four different faults that
# Cycles prints identically as "Unknown error".
for label, env in (("as the job runs", None), ("with one device", "0")):
    if env is not None:
        os.environ["CUDA_VISIBLE_DEVICES"] = env
    rc = lib.cuInit(0)
    n = ctypes.c_int(-1)
    rc2 = lib.cuDeviceGetCount(ctypes.byref(n))
    print(f"{label}: cuInit={rc} cuDeviceGetCount={rc2} devices={n.value}")
CUDA
    echo "--- what Cycles says when asked to explain itself ---"
    blender -b --debug-cycles --python-expr "
import bpy
p = bpy.context.preferences.addons['cycles'].preferences
for b in ('OPTIX', 'CUDA'):
    try:
        p.compute_device_type = b
    except TypeError:
        continue
    p.get_devices()
    print('DEVICES', b, [(d.type, d.name) for d in p.devices])
" 2>&1 | grep -iE "DEVICES|cuda|optix|device" | head -20
    echo "--- what Blender was built with ---"
    ls /opt/blender/*/scripts/addons_core/cycles/lib/ 2>/dev/null | head -8 \
        || find /opt/blender -name '*.cubin*' -o -name '*.ptx*' 2>/dev/null \
           | sed 's/.*\//  /' | head -8
    echo NO-GPU-BAIL
    sleep "${JOB_BAIL_SLEEP:-600}"
    exit 1
fi
# One download for the whole pod. Sixteen processes then read the same file,
# which the page cache is already holding — as against sixteen cold globes,
# each of which cost about 500 s and 89 % of a 48-frame job.
if [ -n "${JOB_PACK_URL:-}" ]; then
    PACK="${JOB_PACK:-/tmp/scene.tuilepack}"
    if ! curl -fsS -o "$PACK" "$JOB_PACK_URL"; then
        echo PACK-FETCH-FAILED
        exit 1
    fi
    export TUILE_PACK="$PACK"
    [ -n "${JOB_SCENE:-}" ] && export TUILE_SCENE="$JOB_SCENE"
    echo "pack: $(du -h "$PACK" | cut -f1) -> $PACK"
    # A pack carries its own imagery, and a token in the environment beside it
    # is a token that can still be reached. Nothing should want it; unsetting
    # it is what makes that a fact rather than an intention.
    unset TUILE_ION_TOKEN
fi

# Streaming needs a token; a pack needs nothing.
if [ "$JOB_ENGINE" = "hydra" ] && [ -z "${TUILE_PACK:-}" ] \
       && [ -z "${TUILE_ION_TOKEN:-}" ]; then
    echo NO-SOURCE-BAIL
    sleep "${JOB_BAIL_SLEEP:-600}"
    exit 1
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

# Are the GPUs actually all working?
#
# The question that was never asked, and the answer was no: four processes,
# one CUDA_VISIBLE_DEVICES each, all four measured on GPU 2. Nothing in the
# job said so — the probe counted cards the driver could see, the render
# finished, and the only trace of it was a screenshot somebody happened to
# take. This samples utilisation per device while the render runs, so the
# log answers it whether or not anyone is watching.
if command -v nvidia-smi > /dev/null 2>&1; then
    (
        while sleep "${JOB_GPU_SAMPLE:-30}"; do
            line=$(nvidia-smi --query-gpu=index,utilization.gpu,memory.used \
                       --format=csv,noheader,nounits 2>/dev/null \
                   | awk -F', ' '{printf "gpu%s=%s%%/%sMiB ", $1, $2, $3}')
            [ -n "$line" ] || break
            busy=$(nvidia-smi --query-gpu=utilization.gpu \
                       --format=csv,noheader,nounits 2>/dev/null \
                   | awk '$1 > 5' | wc -l)
            echo "GPU-USE $busy/$JOB_GPUS actifs  $line"
        done
    ) &
    gpu_watch=$!
    trap 'kill "$gpu_watch" "${log_flusher:-0}" 2>/dev/null; archive_everything' EXIT
fi

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
    # CYCLES_DEVICE is not decoration. The Cycles Hydra delegate reads its
    # device from a render setting, then from this variable, and **falls back
    # to CPU** when neither says anything (`cycles/src/hydra/
    # render_delegate.cpp`). Without it four RTX 5090s would sit idle while
    # sixteen processes path-traced on the host CPU, and the only symptom would
    # be a job that took all night.
    #
    # Combined with one CUDA_VISIBLE_DEVICES per process it is also what places
    # the work: the delegate takes every visible device of its type, and each
    # process is shown exactly one.
    CUDA_VISIBLE_DEVICES=$gpu TUILE_CACHE_DIR="$proc_cache" \
        CYCLES_DEVICE="$CYCLES_BACKEND" \
        stdbuf -oL blender -b -P /opt/render/render_usd.py -- \
        --stage "$STAGE" --engine "$JOB_ENGINE" --tier "$JOB_TIER" \
        --delegate "$JOB_DELEGATE" \
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
kill "${gpu_watch:-0}" "${log_flusher:-0}" 2>/dev/null || true
echo "WALL: $(($(date +%s) - t0))s pour $total frames en $jobs processus / $JOB_GPUS GPU"
# The verdict, in one line, from the samples above. A run that used one GPU of
# four is not a slow run, it is a broken one, and it must not need a human to
# notice.
if [ -s "$JOB_LOG" ]; then
    peak=$(grep -o 'GPU-USE [0-9]*' "$JOB_LOG" | awk '{print $2}' | sort -rn | head -1)
    if [ -n "$peak" ] && [ "$peak" -lt "$JOB_GPUS" ]; then
        echo "GPU-UNDERUSED: au mieux $peak GPU sur $JOB_GPUS ont travaillé"
    else
        echo "GPU-OK: ${peak:-?}/$JOB_GPUS"
    fi
fi

# Blender 5.x has no built-in encoder: the driver leaves PNG sequences and
# each range is encoded here with the static ffmpeg.
#
# Each segment leaves for R2 the moment it exists, rather than waiting for the
# concat. Waiting is how a reclaimed pod loses everything it had already made:
# the last 60-second job died at its own pace with 1440 frames rendered and
# nothing deposited. A segment that is already on R2 is a segment nobody has to
# render again.
for i in $(seq 0 $((jobs - 1))); do
    if [ ! -s "$outdir/seg$i.mp4" ] && ls "$outdir/s$i".*.png > /dev/null 2>&1; then
        a=$((first + i * span))
        ffmpeg -y -framerate "$JOB_FPS" -start_number "$a" \
            -i "$outdir/s$i.%d.png" -c:v libx264 -pix_fmt yuv420p -crf 18 \
            "$outdir/seg$i.mp4" > /dev/null 2>&1 && rm -f "$outdir/s$i".*.png
    fi
    eval "url=\${JOB_SEG_PUT_URL_$i:-}"
    if [ -n "$url" ] && [ -s "$outdir/seg$i.mp4" ]; then
        if curl -fsS -T "$outdir/seg$i.mp4" "$url" > /dev/null; then
            echo "SEG-UP $i ($(du -h "$outdir/seg$i.mp4" | cut -f1))"
        else
            echo "SEG-UP-FAILED $i"
        fi
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
