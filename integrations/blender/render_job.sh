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
#   JOB_BLENDER_ARGS   extra blender args, before -P  (e.g. "--debug-cycles")
#   JOB_OUT            final video path          (default /out/render.mp4)
#   JOB_UPLOAD_PUT_URL if set: curl -T the finished video to this presigned
#                      URL (no credential ever reaches the pod)
#   JOB_LOGS_PUT_URL   if set: every process's stdout, gathered into
#                      logs.tar.gz and shipped the same way — ON EVERY EXIT
#                      PATH and every JOB_ARCHIVE_EVERY seconds while it runs,
#                      because a job that failed is the one whose logs are
#                      worth having and a pod that is killed runs no trap
#   Les erreurs de stderr remontent AUSSI sur stdout, donc dans les journaux
#   du fournisseur, pendant que la tâche tourne. Elles n'y étaient pas : stderr
#   partait dans trace-s<i>.jsonl et ce fichier n'arrive qu'à la fin, dans une
#   archive. Le 16 septembre, un comptage fait sur les journaux en ligne a
#   conclu « zéro erreur » alors que la trace en portait cent quatre-vingt-dix
#   — deux flux, deux destinations, et la mauvaise interrogée. La trace
#   complète reste dans le fichier ; seules les lignes qui portent une erreur
#   sont dupliquées.
#   JOB_SEG_PUT_URL_<i> if set: segment i goes to R2 THE MOMENT it is encoded,
#                      not at the end. A pod reclaimed at frame 900 of 1440
#                      has still deposited the nine hundred.
#                      With several tasks, <i> is the GLOBAL segment number:
#                      task t owns t*jobs .. (t+1)*jobs-1, so the launcher can
#                      concatenate them in order without knowing who made what.
#   CLOUD_RUN_TASK_INDEX / _COUNT   set by Cloud Run Jobs, not by us. When
#                      COUNT > 1 this script renders only its own slice of
#                      JOB_FRAMES and does NOT concatenate: no task sees every
#                      segment, so the film is assembled from R2 afterwards
#                      (`launch_job.py --assemble <run-id>`).
#   JOB_LOGS_PUT_URL_<t> / JOB_TRACE_PUT_URL_<t> / JOB_PROFILE_PUT_URL_<t>
#                      per-task variants, preferred over the unsuffixed ones
#                      when present. Without them N tasks would overwrite one
#                      another's logs.tar.gz and the survivor would be whoever
#                      finished last — which is never the one that failed.
#   JOB_OPTIX_CACHE_GET_URL / _PUT_URL  presigned GET/PUT for the OptiX disk
#                      cache. Without it every process pays the driver's
#                      PTX-to-machine-code compilation again: **338 seconds**,
#                      measured on an L4 on 16 September 2026, against 0.17 s
#                      for the frame that follows it. The PTX itself is already
#                      precompiled and shipped in the image — this is the step
#                      after it, and it cannot be done at build time because
#                      the builder has no NVIDIA card and the result is
#                      specific to the card and driver anyway.
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
# Des drapeaux pour BLENDER lui-même, avant `-P`, là où `JOB_EXTRA_ARGS` va au
# script Python d'après `--`. La distinction a coûté une soirée : Cycles ne
# journalise que si Blender le lui demande en ligne de commande, et sans ça un
# rendu qui ne démarre pas reste parfaitement muet — CPU à 0,6 %, GPU à 0 %, et
# rien à lire nulle part. Typiquement `--debug-cycles` ou `--log "*cycles*"`.
JOB_BLENDER_ARGS="${JOB_BLENDER_ARGS:-}"
JOB_OUT="${JOB_OUT:-/out/render.mp4}"
outdir="$(dirname "$JOB_OUT")"
mkdir -p "$outdir"

# Which of how many.
#
# Cloud Run Jobs sets these; a pod sets neither, and a lone container reads as
# task 0 of 1 — the behaviour this script had before tasks existed.
#
# It is resolved HERE, above the archive helpers, because it is an identity and
# not a division of labour. The tarballs are named from it: N tasks sharing one
# JOB_LOGS_PUT_URL overwrite each other, and the survivor is whoever finished
# last — which is never the one that failed.
task_index="${CLOUD_RUN_TASK_INDEX:-0}"
task_count="${CLOUD_RUN_TASK_COUNT:-1}"
for var in JOB_LOGS_PUT_URL JOB_TRACE_PUT_URL JOB_PROFILE_PUT_URL; do
    eval "mine=\${${var}_${task_index}:-}"
    [ -n "$mine" ] && eval "$var=\$mine"
done

# Everything this job says about itself, in a file rather than only in a
# console nobody will read once the pod is gone.
JOB_LOG="$outdir/job.log"
# Les descripteurs d'origine, gardés ouverts.
#
# Tout ce qui suit passe par `tee`, qui est un PROCESSUS : ce qu'on lui écrit
# vit dans un tampon jusqu'à ce qu'il l'écrive. Un shell qui sort tout de suite
# après avoir signalé une erreur est tué avec lui, et le message n'atteint ni
# le fichier ni la console. Mesuré le 17 septembre 2026 : deux tâches mortes
# sur « plage trop courte pour 4 processus », zéro ligne dans Cloud Logging,
# zéro `logs.tar.gz` dans l'archive — une heure passée à soupçonner l'image.
#
# `archive_everything` referme le tuyau sur ces descripteurs-là avant de
# ranger, ce qui donne à `tee` son EOF et donc son vidage.
exec 3>&1 4>&2
exec > >(tee -a "$JOB_LOG") 2>&1

# Ships one tarball to one presigned URL. Quiet about a URL that is not set;
# loud about one that is and fails.
ship() {
    local name="$1" url="$2"; shift 2
    [ -n "$url" ] || return 0

    # Les motifs arrivent ici NON développés ('log-s*.txt'), et rien ne les
    # développait. Deux causes, toutes deux silencieuses :
    #
    #   * `ls "$@"` ne fait pas de glob. Le mot est entre guillemets, donc le
    #     shell ne l'étend pas, et `ls` reçoit `log-s*.txt` au pied de la
    #     lettre — un fichier qui n'existe pas.
    #   * l'image n'a pas de WORKDIR, donc le répertoire courant est `/`, où
    #     aucun journal ne se trouve de toute façon.
    #
    # La garde rendait donc 0 à tous les coups et `ship` sortait sans rien
    # faire. Mesuré le 15 septembre sur les dix runs archivés depuis le début
    # du projet : **zéro archive déposée, jamais**. C'est exactement le
    # mécanisme censé empêcher que trois mesures meurent avec leur machine —
    # et il n'a pas manqué son but une fois, il ne l'a jamais visé.
    #
    # C'est au shell de développer, et dans $outdir.
    local files pat f
    files=$(cd "$outdir" 2>/dev/null && for pat in "$@"; do
                for f in $pat; do [ -e "$f" ] && printf '%s\n' "$f"; done
            done)
    [ -n "$files" ] || return 0

    # `tar` rend 1 — pas 2 — quand un fichier a changé pendant qu'il le lisait,
    # et l'archive produite reste complète et lisible. Le traiter comme fatal
    # condamnait le flush périodique, qui existe précisément pour tourner
    # pendant que `tee` écrit dans job.log. Seul un code >= 2, ou une archive
    # vide, est une vraie panne.
    local status=0
    tar czf "$outdir/$name" -C "$outdir" $files 2> "$outdir/.tar-$name.err" \
        || status=$?
    if [ "$status" -ge 2 ] || [ ! -s "$outdir/$name" ]; then
        echo "ARCHIVE-TAR-FAILED $name (tar=$status)"
        sed 's/^/  /' "$outdir/.tar-$name.err" 2>/dev/null | head -3
        return 1
    fi
    echo "$name: $(du -h "$outdir/$name" | cut -f1)"
    if curl -fsS -T "$outdir/$name" "$url" > /dev/null; then
        echo "ARCHIVE-UP $name"
    else
        echo "ARCHIVE-UP-FAILED $name"
        return 1
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
        # Discret quand ça marche, bruyant quand ça rate. Tout envoyer dans
        # /dev/null a caché pendant une demi-heure que rien ne partait — et
        # c'est exactement le genre de panne qu'un flush est censé survivre,
        # pas commettre.
        out=$(ship logs.tar.gz "$JOB_LOGS_PUT_URL" 'log-s*.txt' 'job.log' 2>&1)
        case "$out" in *FAILED*) echo "$out" ;; esac
    done
}

# Called on EVERY exit, successful or not. This is the whole point: three
# measurements died with their machine in two days, and the last one was a
# 60-second render whose pod was reclaimed mid-flight.
archive_everything() {
    # Le code de sortie est passé en argument, pas lu dans `$?`.
    #
    # Il était lu — `local status=$?` — et le piège de sortie exécute un `kill`
    # juste avant d'appeler cette fonction. `$?` était donc le code de ce
    # `kill`, qui échoue dès que l'échantillonneur GPU s'est déjà arrêté seul.
    # Mesuré le 16 septembre 2026 : `TASK-DONE 2/3`, suivi immédiatement de
    # `Container called exit(1)`. Les trois tâches d'une minute de film ont
    # rendu leurs frames, déposé leurs segments, et se sont déclarées en
    # échec — après quoi le montage automatique, gardé derrière « aucune tâche
    # en échec », ne s'est pas lancé.
    local status="${1:-$?}"
    trap - EXIT
    # Rendre la sortie directe AVANT de ranger : `tee` voit alors la fin de son
    # entrée, vide son tampon dans job.log et s'arrête. Sans ça on empaquette
    # un fichier que personne n'a fini d'écrire.
    exec 1>&3 2>&4
    # Le temps que `tee` voie l'EOF et écrive. Attendre les tâches de fond
    # serait faux : l'échantillonneur GPU et le flush tournent en boucle, et on
    # les attendrait toujours.
    sleep 0.3
    ship logs.tar.gz    "${JOB_LOGS_PUT_URL:-}"    'log-s*.txt' 'job.log'
    ship trace.tar.gz   "${JOB_TRACE_PUT_URL:-}"   'trace-s*.jsonl'
    ship profile.tar.gz "${JOB_PROFILE_PUT_URL:-}" 'profile'
    exit $status
}
trap 'rc=$?; archive_everything "$rc"' EXIT
flush_logs &
log_flusher=$!

first="${JOB_FRAMES%%:*}"; last="${JOB_FRAMES##*:}"

# JOB_FRAMES is the range of the WHOLE render, identical in every task's
# environment — the launcher sends one env to N tasks and cannot address them
# individually. So each task cuts its own slice out of it here.
#
# The remainder is spread one frame at a time over the first `rem` tasks rather
# than dumped on the last one: with 48 frames over 5 tasks that is 10,10,10,9,9
# instead of 9,9,9,9,12, and the slowest task sets the wall clock. Contiguous
# and exhaustive by construction — start(t+1) is exactly end(t)+1, so the
# concatenated film has no gap and no repeat.
if [ "$task_count" -gt 1 ]; then
    all=$((last - first + 1))
    base=$((all / task_count))
    rem=$((all % task_count))
    [ "$task_index" -lt "$rem" ] && extra=1 || extra=0
    ahead=$task_index
    [ "$ahead" -gt "$rem" ] && ahead=$rem
    first=$((first + task_index * base + ahead))
    last=$((first + base + extra - 1))
    echo "task $task_index/$task_count: frames $first:$last"
fi

total=$((last - first + 1))
jobs=$((JOB_GPUS * JOB_PROCS_PER_GPU))
# Moins de frames que de processus n'est pas une erreur, c'est un petit rendu.
#
# Ce cas sortait en code 1 sur « plage trop courte ». C'est défendable pour une
# ferme de production et faux pour tout le reste : les rendus de vérification
# — deux frames pour regarder une texture — sont précisément ceux qu'on lance
# le plus souvent, et un job qui refuse de rendre deux frames sur quatre
# processus refuse de faire moins que ce qu'on lui a permis.
if [ "$jobs" -gt "$total" ]; then
    echo "frames $first:$last — $total frame(s) pour $jobs processus," \
         "donc $total processus"
    jobs=$total
fi
# Segments are numbered globally, so `--assemble` can order them across tasks
# without asking who produced which.
seg_base=$((task_index * jobs))
span=$((total / jobs))

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
# Measured on four different hosts, and the ratio is the whole story:
# 8 GPUs advertised / 1 node, 5 / 4, 5 / 1, 5 / 1. Never equal, never working.
# This is not a broken machine to retry past — it is what taking a SHARE of a
# machine looks like, so the cure is to take all of it.
if command -v nvidia-smi > /dev/null 2>&1; then
    # Counted, not computed — and deliberately without an interpreter.
    #
    # The first version ran a ctypes cuInit, and this image has no python3:
    # Blender bundles its own and nothing else is installed. So the check
    # printed a shell error where a number belonged, and the bail fired on a
    # non-empty string. Right answer, wrong reason, twice.
    #
    # Two `ls` are enough, because the fault IS structural. Every broken host
    # so far advertised more GPUs in /proc than it handed out device nodes:
    # the pod holds a subset of the machine without a filtered procfs, UVM
    # refuses to open, and cuInit returns 999. Comparing two counts needs no
    # CUDA, no interpreter, and no guess about what an error code means.
    in_proc=$(ls /proc/driver/nvidia/gpus 2>/dev/null | wc -l | tr -d " ")
    in_dev=$(ls /dev/nvidia[0-9]* 2>/dev/null | wc -l | tr -d " ")
    echo "gpus: $in_proc advertised in procfs, $in_dev device nodes"
    if [ "$in_proc" != "$in_dev" ]; then
        echo "GPU-PARTIAL-HOST: the driver advertises $in_proc GPUs and this"
        echo "  container has $in_dev device nodes. UVM initialises across every"
        echo "  GPU the driver knows about, cannot reach the ones this pod was"
        echo "  not given, and refuses to open — cuInit then returns 999."
        echo "  Nothing in the image can help, and retrying is not the answer:"
        echo "  four different hosts have done this (8/1, 5/4, 5/1, 5/1), so it"
        echo "  is how a partial machine is handed out, not a broken machine."
        echo "  Ask for --gpu-count equal to the host's full complement."
        ls /dev/nvidia[0-9]* 2>/dev/null | tr "\n" " " | sed "s/^/  nodes: /"; echo
        nvidia-smi --query-gpu=uuid --format=csv,noheader 2>/dev/null \
            | sed "s/^/  uuid: /"
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
    # Blender's python, because the image has no other one.
    blender -b --python-expr "
import ctypes, os
try:
    lib = ctypes.CDLL('libcuda.so.1')
except OSError as e:
    print('libcuda.so.1 will not load:', e)
else:
    for label, env in (('as the job runs', None), ('with one device', '0')):
        if env is not None:
            os.environ['CUDA_VISIBLE_DEVICES'] = env
        rc = lib.cuInit(0)
        n = ctypes.c_int(-1)
        rc2 = lib.cuDeviceGetCount(ctypes.byref(n))
        print(f'{label}: cuInit={rc} cuDeviceGetCount={rc2} devices={n.value}')
" 2>&1 | grep -E "cuInit=|will not load" | sed 's/^/  /'
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
    #   orbit:frames:lon:lat:radius_m:alt_m     (defaults after the kind)
    #   pyrenees:minutes:fps:alt_m:offset_deg   (boucle nadir autour du massif)
    #   zoom:frames_each_way
    IFS=: read -r kind p1 p2 p3 p4 p5 <<< "$JOB_TRAJECTORY"
    case "$kind" in
        orbit) /opt/tuile/bin/orbit-tape /tmp/traj.mcap \
                   "${p1:-1440}" "${p2:-2.17}" "${p3:-42.52}" \
                   "${p4:-8000}" "${p5:-5000}" ;;
        pyrenees) /opt/tuile/bin/pyrenees-tape /tmp/traj.mcap \
                      "${p1:-2}" "${p2:-24}" "${p3:-50000}" "${p4:-0.40}" ;;
        zoom)  /opt/tuile/bin/zoom-tape /tmp/traj.mcap "${p1:-64}" ;;
        *) echo "TRAJECTORY-UNKNOWN: $kind"; exit 1 ;;
    esac
    /opt/tuile/bin/tape-to-stage /tmp/traj.mcap "$STAGE" \
        --viewport "${JOB_VIEWPORT:-1280x960}" --sse "${JOB_SSE:-3}" \
        --fps "$JOB_FPS" || { echo MANIFEST-GEN-FAILED; exit 1; }
fi
[ -s "$STAGE" ] || { echo "stage absente: $STAGE" >&2; exit 1; }

# Le cache OptiX, tiré avant le premier rendu.
#
# OptiX compile le PTX en code machine pour la carte au premier chargement et
# garde le résultat dans un cache disque — `OPTIX_CACHE_PATH`, par défaut
# /var/tmp/OptixCache_$USER, qui ne survit pas à un conteneur. Mesuré sur L4 :
# 338 s pour la première frame, 0,17 s pour la suivante. Sur trois tâches c'est
# dix-sept minutes de compilation pour une minute de film.
#
# Le tirage est facultatif par construction : une clef absente est le cas
# normal la première fois, et un cache illisible vaut un cache vide. Ce qu'on
# ne veut pas, c'est qu'un cache manquant fasse échouer un rendu.
export OPTIX_CACHE_PATH="${OPTIX_CACHE_PATH:-$outdir/optix-cache}"
mkdir -p "$OPTIX_CACHE_PATH"
if [ -n "${JOB_OPTIX_CACHE_GET_URL:-}" ]; then
    if curl -fsS -o "$outdir/optix-cache.tar.gz" "$JOB_OPTIX_CACHE_GET_URL" \
       && tar xzf "$outdir/optix-cache.tar.gz" -C "$OPTIX_CACHE_PATH" 2>/dev/null; then
        echo "OPTIX-CACHE-HIT ($(du -sh "$OPTIX_CACHE_PATH" | cut -f1))"
    else
        echo "OPTIX-CACHE-MISS — la première frame paiera la compilation"
    fi
fi

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
    # `rc` d'abord, le ménage ensuite : un `kill` qui rate ne doit pas devenir
    # le verdict de la tâche.
    trap 'rc=$?; kill "$gpu_watch" "${log_flusher:-0}" 2>/dev/null || true; archive_everything "$rc"' EXIT
fi

# Les PID des rendus, et d'eux seuls.
#
# `wait` sans argument attend TOUS les enfants — dont le sampler GPU et le
# flush de journaux, qui sont des boucles infinies. Le script ne pouvait donc
# jamais franchir cette ligne, même une fois tous les rendus morts : mesuré le
# 16 septembre, un Blender a crashé au bout de dix secondes et la tâche a
# continué d'échantillonner un GPU inactif jusqu'au délai d'une heure. Elle
# facturait, et de l'extérieur elle avait l'air de travailler.
renders=()

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
        stdbuf -oL blender -b $JOB_BLENDER_ARGS -P /opt/render/render_usd.py -- \
        --stage "$STAGE" --engine "$JOB_ENGINE" --tier "$JOB_TIER" \
        --delegate "$JOB_DELEGATE" \
        --frames "$a:$b" --width "$JOB_WIDTH" --fps "$JOB_FPS" \
        --samples "$JOB_SAMPLES" --adaptive-threshold "$JOB_THRESHOLD" \
        --batch-frames "$JOB_BATCH_FRAMES" $JOB_EXTRA_ARGS \
        --out "$outdir/s$i" --video "$outdir/seg$i.mp4" \
        2> >(tee "$outdir/trace-s$i.jsonl" \
             | grep --line-buffered -E 'ERROR|Error|error:|Could not|FATAL|Warning:' \
             | sed -u "s/^/[err$i] /") \
        | grep --line-buffered -vE '^(Fra:|Saved:|Time:|Append frame)' \
        | sed -u "s/^/[gpu$gpu-j$i] /" \
        | tee -a "$outdir/log-s$i.txt" &
    # Le PID du dernier maillon du pipeline : il se termine quand le rendu qui
    # l'alimente se termine, quelle qu'en soit la manière.
    renders+=($!)
done
wait "${renders[@]}"
kill "${gpu_watch:-0}" "${log_flusher:-0}" 2>/dev/null || true
echo "WALL: $(($(date +%s) - t0))s pour $total frames en $jobs processus / $JOB_GPUS GPU"

# Le cache OptiX repart, pour que la prochaine exécution ne recompile pas.
#
# Seule la tâche 0 dépose : le contenu est équivalent d'une tâche à l'autre —
# même carte, même pilote, mêmes noyaux — donc écrire à plusieurs ne gagnerait
# rien et ferait dépendre le résultat de l'ordre d'arrivée.
#
# Le dépôt a lieu APRÈS le rendu, et c'est une limite assumée : les tâches d'un
# même run démarrent à quelques minutes d'intervalle (20:47, 20:48, 20:51 le
# 16 septembre) et compilent donc toutes les trois. Elles le font en parallèle,
# soit cinq minutes au total et non quinze, et le gain du cache est entre runs.
# Déposer plus tôt demanderait de savoir quand la compilation finit, ce que
# rien ici ne dit.
if [ -n "${JOB_OPTIX_CACHE_PUT_URL:-}" ] && [ "$task_index" = "0" ] \
   && [ -d "$OPTIX_CACHE_PATH" ]; then
    if tar czf "$outdir/optix-cache-out.tar.gz" -C "$OPTIX_CACHE_PATH" . \
       && curl -fsS -T "$outdir/optix-cache-out.tar.gz" \
               "$JOB_OPTIX_CACHE_PUT_URL" > /dev/null; then
        echo "OPTIX-CACHE-UP ($(du -h "$outdir/optix-cache-out.tar.gz" | cut -f1))"
    else
        echo "OPTIX-CACHE-UP-FAILED"
    fi
fi
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
    g=$((seg_base + i))
    eval "url=\${JOB_SEG_PUT_URL_$g:-}"
    if [ -n "$url" ] && [ -s "$outdir/seg$i.mp4" ]; then
        if curl -fsS -T "$outdir/seg$i.mp4" "$url" > /dev/null; then
            echo "SEG-UP $g ($(du -h "$outdir/seg$i.mp4" | cut -f1))"
        else
            echo "SEG-UP-FAILED $g"
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
# With several tasks, this one is done.
#
# No task holds every segment, so concatenating here would produce N films of
# one Nth each and upload them over one another at JOB_UPLOAD_PUT_URL. The
# assembly moves to the launcher, which reads them back from R2
# (`launch_job.py --assemble <run-id>`) — and can do it long after every task
# has exited, which is the point: nobody has to be watching at the end.
if [ "$task_count" -gt 1 ]; then
    if [ "$n" = "$jobs" ]; then
        echo "TASK-DONE $task_index/$task_count ($n segments, frames $first:$last)"
        exit 0
    fi
    echo "TASK-INCOMPLETE $task_index/$task_count ($n/$jobs segments)"
    exit 1
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
    # this is a courtesy window for an ssh session, not a lifeline — and it is
    # only opened when someone asked for ssh. Cloud Run bills the GPU by the
    # second, so an unconditional minute of sleep is a minute of L4 bought to
    # watch a finished job do nothing.
    [ -n "${JOB_SSH_PUBKEY:-}" ] && sleep "${JOB_DONE_SLEEP:-60}" || true
else
    echo VIDEO-MISSING
    exit 1
fi
