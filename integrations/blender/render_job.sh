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
#                      (JOB_PACK_KEY) or TUILE_ION_TOKEN in the pod env)
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
#
# # Object storage: keys, not URLs
#
# Every byte in and out goes through `/opt/tuile/bin/tuile-farm`, with the
# job's own credentials (TUILE_STORE_ENDPOINT, TUILE_STORE_BUCKET,
# TUILE_STORE_ACCESS_KEY_ID, TUILE_STORE_SECRET_ACCESS_KEY — set in the job's
# DEFINITION, never in an execution's overrides, which anyone reading the
# execution can see). Downloads are parallel ranged reads and uploads are
# multipart: a presigned URL was one TCP stream, and a 2.3 GB pack took 88 s
# of every task before a single frame.
#
#   JOB_RUN_PREFIX     where this run's outputs go, e.g. `renders/<run-id>`:
#                      seg<g>.mp4 as each is encoded, logs[-t<t>].tar.gz,
#                      trace[-t<t>].tar.gz, profile[-t<t>].tar.gz — ON EVERY
#                      EXIT PATH and every JOB_ARCHIVE_EVERY seconds while it
#                      runs, because a job that failed is the one whose logs
#                      are worth having and a pod that is killed runs no trap —
#                      then tasks/<t>.json, and finally render.mp4.
#                      Unset: nothing leaves the machine (a local run).
#   Les erreurs de stderr remontent AUSSI sur stdout, donc dans les journaux
#   du fournisseur, pendant que la tâche tourne. Elles n'y étaient pas : stderr
#   partait dans trace-s<i>.jsonl et ce fichier n'arrive qu'à la fin, dans une
#   archive. Le 16 septembre, un comptage fait sur les journaux en ligne a
#   conclu « zéro erreur » alors que la trace en portait cent quatre-vingt-dix
#   — deux flux, deux destinations, et la mauvaise interrogée. La trace
#   complète reste dans le fichier ; seules les lignes qui portent une erreur
#   sont dupliquées.
#   CLOUD_RUN_TASK_INDEX / _COUNT   set by Cloud Run Jobs, not by us. When
#                      COUNT > 1 this script renders only its own slice of
#                      JOB_FRAMES. Segments are numbered globally — task t owns
#                      t*jobs .. (t+1)*jobs-1 — and each task, once its
#                      segments are up, leaves a receipt and asks for the film:
#                      the one that finds every receipt assembles it
#                      (`tuile-farm assemble`). Nobody has to be watching at
#                      the end.
#   JOB_OPTIX_CACHE_KEY  the OptiX disk cache, read before the first render
#                      and written back by task 0. Without it every process
#                      pays the driver's PTX-to-machine-code compilation again:
#                      **338 seconds**, measured on an L4 on 16 September 2026,
#                      against 0.17 s for the frame that follows it. It cannot
#                      be done at build time: the builder has no NVIDIA card
#                      and the result is specific to the card and driver.
#   JOB_ARCHIVE_EVERY  seconds between log flushes while rendering (default 300)
#   JOB_PACK_KEY       a pre-baked pack. The job downloads it ONCE and every
#                      process reads it — no ion token, no network, no
#                      traversal. This is the normal shape of a render now; the
#                      streaming path is what runs when nobody baked.
#   JOB_STAGE_KEY      a stage too large for JOB_STAGE_B64_GZ
#   JOB_TAPE_KEY       the tape the pack was baked from (`<pack>.mcap`)
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
# La cadence, une seule valeur pour tout le job.
#
# `JOB_FPS` fait autorité. À défaut elle se lit dans la trajectoire, qui la
# porte en deuxième position — et pour le seul genre `pyrenees` : `orbit` met
# une longitude à cette place, et la lire comme une cadence donnerait un film
# à deux images par seconde sans que rien ne s'en plaigne.
#
# Le lanceur pose la même valeur, calculée par `fps_of`. Deux endroits pour un
# même nombre, c'est deux endroits pour qu'ils divergent : un test tient les
# deux dérivations ensemble, et les deux jobs portent celle-ci mot pour mot —
# une cuisson et un rendu qui n'échantillonnent pas la même bande décrivent
# deux tournages différents, et les deux annoncent une réussite.
if [ -z "${JOB_FPS:-}" ]; then
    IFS=: read -r _kind _p1 _p2 _rest <<< "${JOB_TRAJECTORY:-}"
    case "$_kind" in
        pyrenees) JOB_FPS="${_p2:-24}" ;;
        *)        JOB_FPS=24 ;;
    esac
fi
JOB_EXTRA_ARGS="${JOB_EXTRA_ARGS:-}"
# Des drapeaux pour BLENDER lui-même, avant `-P`, là où `JOB_EXTRA_ARGS` va au
# script Python d'après `--`. La distinction a coûté une soirée : Cycles ne
# journalise que si Blender le lui demande en ligne de commande, et sans ça un
# rendu qui ne démarre pas reste parfaitement muet — CPU à 0,6 %, GPU à 0 %, et
# rien à lire nulle part. Typiquement `--debug-cycles` ou `--log "*cycles*"`.
JOB_BLENDER_ARGS="${JOB_BLENDER_ARGS:-}"

# Et allumé par défaut, parce que l'avoir sans s'en servir revient au même.
#
# Le 22 septembre 2026, un rendu 4K s'est arrêté net — CPU conteneur à zéro,
# GPU à zéro, 6,3 Go résidents sur la carte — et le journal n'avait rien à en
# dire entre « frame 1 : début » et le silence. Le drapeau existait déjà, avec
# le commentaire ci-dessus qui raconte la soirée qu'il avait coûtée la première
# fois. Il n'était pas mis.
#
# Sans jokers : `--log cycles` prend déjà toute catégorie COMMENÇANT par
# « cycles » (lu dans `creator_args.cc`), et un `*cycles*` non quoté se ferait
# développer par le shell contre le répertoire courant avant d'arriver à Blender.
#
# `JOB_TRACE=0` pour un film long, où le journal coûte plus qu'il ne rapporte.
# The batch-render configuration, as defaults any job can override.
#
# Measured 22 September 2026 on one pack, eight renders a side (videos/README.md):
#
#   CYCLES_BACKGROUND=1   hdCycles hardcodes an interactive session, in which
#                         the render thread parks in `pause_cond_.wait()` still
#                         flagged as rendering; waiting on it never returned.
#   CYCLES_AUTO_TILE=0    above one 2048x2048 tile Cycles renders to disk and
#                         hands the frame back through a callback hdCycles never
#                         wires: every render above 4.19 Mpx looped forever,
#                         2560x1440 passed and 2880x1620 spun.
#   TUILE_WAIT_MODE       `command` blocks once per frame on the session; `poll`
#                         sleeps 50 ms per turn. Same images, same times; the
#                         blocking wait is one turn a frame instead of dozens.
#
# Here and not in each launcher, because tuile's jobs and STL's (which ends by
# exec'ing this script) must not drift apart on the one thing that decides
# whether a render ends at all.
export CYCLES_BACKGROUND="${CYCLES_BACKGROUND:-1}"
export CYCLES_AUTO_TILE="${CYCLES_AUTO_TILE:-0}"
export TUILE_WAIT_MODE="${TUILE_WAIT_MODE:-command}"
echo "batch config: CYCLES_BACKGROUND=$CYCLES_BACKGROUND CYCLES_AUTO_TILE=$CYCLES_AUTO_TILE TUILE_WAIT_MODE=$TUILE_WAIT_MODE"

JOB_TRACE="${JOB_TRACE:-1}"
if [ "$JOB_TRACE" = "1" ] && [ -z "$JOB_BLENDER_ARGS" ]; then
    JOB_BLENDER_ARGS="--debug-cycles --log cycles,render,usd,hydra,depsgraph,wm --log-level 2 --log-show-source"
fi

# Le côté USD, qui a son propre système et n'écoute pas celui de Blender.
#
# Laissé vide : les jetons `TF_DEBUG` se nomment un par un et un nom inventé ne
# produit rien de visible, donc il se met à la main quand on sait ce qu'on
# cherche — par exemple `JOB_TF_DEBUG="HD_SAFE_MODE HDGP_PLUGIN_DISCOVERY"`.
# `TF_DEBUG='*'` existe et noie tout ; il dépanne une fois, jamais deux.
if [ -n "${JOB_TF_DEBUG:-}" ]; then
    export TF_DEBUG="$JOB_TF_DEBUG"
    echo "TF_DEBUG=$TF_DEBUG"
fi
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
# One suffix per task on everything a task archives: N tasks writing one
# logs.tar.gz overwrite each other, and the survivor is whoever finished last —
# which is never the one that failed.
task_tag=""
[ "$task_count" -gt 1 ] && task_tag="-t$task_index"

FARM="${TUILE_FARM:-/opt/tuile/bin/tuile-farm}"
JOB_RUN_PREFIX="${JOB_RUN_PREFIX%/}"
# Where a name of this run lives in the bucket.
run_key() { printf '%s/%s' "$JOB_RUN_PREFIX" "$1"; }

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

# Ships one tarball to `<run>/<name>` (with the task's suffix). Quiet when the
# run has no prefix; loud when it has one and the upload fails.
ship() {
    local name="$1"; shift
    [ -n "$JOB_RUN_PREFIX" ] || return 0
    local key
    key=$(run_key "${name%.tar.gz}${task_tag}.tar.gz")

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
    if "$FARM" put "$outdir/$name" "$key" 2> "$outdir/.put-$name.err"; then
        echo "ARCHIVE-UP $key"
    else
        sed 's/^/  /' "$outdir/.put-$name.err" | tail -3
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
    [ -n "$JOB_RUN_PREFIX" ] || return 0
    while sleep "${JOB_ARCHIVE_EVERY:-300}"; do
        # Discret quand ça marche, bruyant quand ça rate. Tout envoyer dans
        # /dev/null a caché pendant une demi-heure que rien ne partait — et
        # c'est exactement le genre de panne qu'un flush est censé survivre,
        # pas commettre.
        out=$(ship logs.tar.gz 'log-s*.txt' 'job.log' 'tmp-s*/blender.crash.txt' 2>&1)
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
    ship logs.tar.gz    'log-s*.txt' 'job.log' 'tmp-s*/blender.crash.txt'
    ship trace.tar.gz   'trace-s*.jsonl'
    ship profile.tar.gz 'profile'
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
# Et l'ordre de préférence se force, parce qu'un backend se soupçonne.
#
# La sonde prend OptiX quand il est là et c'est le bon défaut. Mais le
# 22 septembre 2026 un rendu s'est immobilisé — zéro CPU, zéro GPU — et
# distinguer « OptiX est en cause » de « le chemin Cycles l'est » demandait de
# rendre la même image sur CUDA. Sans ce réglage il fallait recompiler l'image
# pour poser la question.
#
#   JOB_CYCLES_BACKENDS="CUDA"        force CUDA, ignore OptiX
#   JOB_CYCLES_BACKENDS="CUDA OPTIX"  essaie CUDA d'abord
JOB_CYCLES_BACKENDS="${JOB_CYCLES_BACKENDS:-OPTIX CUDA}"
_py_backends="('$(echo "$JOB_CYCLES_BACKENDS" | sed "s/  */','/g")',)"

if [ "$JOB_ENGINE" = "hydra" ] && [ "$JOB_DELEGATE" = "storm" ]; then
    probe="NGPU $(nvidia-smi -L 2>/dev/null | grep -c '^GPU')"
    CYCLES_BACKEND=none
else
    probe_out=$(blender -b --python-expr "
import bpy
prefs = bpy.context.preferences.addons['cycles'].preferences
why = []
for backend in ${_py_backends}:
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
if [ -n "${JOB_PACK_KEY:-}" ]; then
    PACK="${JOB_PACK:-/tmp/scene.tuilepack}"
    if ! "$FARM" get "$JOB_PACK_KEY" "$PACK"; then
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
elif [ -n "${JOB_STAGE_KEY:-}" ]; then
    # A real recorded track (thousands of exact f64 camera samples) is
    # irreducible data; it travels as an object.
    "$FARM" get "$JOB_STAGE_KEY" "$STAGE" || { echo STAGE-FETCH-FAILED; exit 1; }
elif [ -n "${JOB_TAPE_KEY:-}" ] || [ -n "${JOB_TRAJECTORY:-}" ]; then
    # A supplied tape wins over a generated one — the same rule as the bake,
    # and for the same reason, except that here it is not a convenience but a
    # correctness condition.
    #
    # A pack answers a camera by pose: the frame it holds must match the one
    # the stage asks for within a metre and a milliradian, or the tile does
    # not exist and the render fails loudly. The generators move — today's
    # `pyrenees-tape` tilts 20 degrees off nadir where the one that shot the
    # first films looked straight down — so regenerating from the same
    # argument string describes a different flight. Measured on 20 September
    # 2026: position right to six millimetres, orientation off by 0.349066 rad,
    # every task dead in nine seconds.
    #
    # The bake leaves its tape beside the pack (`<pack>.mcap`) precisely so
    # that the render can fly it again. Tape, pack and film are one shot.
    if [ -n "${JOB_TAPE_KEY:-}" ]; then
        "$FARM" get "$JOB_TAPE_KEY" /tmp/traj.mcap \
            || { echo TAPE-DOWNLOAD-FAILED; exit 1; }
        echo "bande: fournie ($(du -h /tmp/traj.mcap | cut -f1)), trajectoire ignorée"
    else
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
                          "${p1:-2}" "$JOB_FPS" "${p3:-50000}" "${p4:-0.40}" ;;
            zoom)  /opt/tuile/bin/zoom-tape /tmp/traj.mcap "${p1:-64}" ;;
            *) echo "TRAJECTORY-UNKNOWN: $kind"; exit 1 ;;
        esac
    fi
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
if [ -n "${JOB_OPTIX_CACHE_KEY:-}" ]; then
    if "$FARM" get "$JOB_OPTIX_CACHE_KEY" "$outdir/optix-cache.tar.gz" 2>/dev/null \
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
    # Its own TMPDIR, under $outdir: Blender writes its crash report to
    # `<tmp>/blender.crash.txt`, one path for every process of the machine,
    # and nothing shipped it. On 23 September two processes out of four
    # crashed on their second frame and all that came back was the line
    # "Writing: /tmp/blender.crash.txt" — the stack stayed in a container
    # that no longer existed. Now each report is its process's, and goes up
    # with the logs.
    mkdir -p "$outdir/tmp-s$i"
    CUDA_VISIBLE_DEVICES=$gpu TUILE_CACHE_DIR="$proc_cache" TMPDIR="$outdir/tmp-s$i" \
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
if [ -n "${JOB_OPTIX_CACHE_KEY:-}" ] && [ "$task_index" = "0" ] \
   && [ -d "$OPTIX_CACHE_PATH" ]; then
    if tar czf "$outdir/optix-cache-out.tar.gz" -C "$OPTIX_CACHE_PATH" . \
       && "$FARM" put "$outdir/optix-cache-out.tar.gz" "$JOB_OPTIX_CACHE_KEY"; then
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

# Blender writes each segment's MP4 itself (`render_usd.py`, "direct video"):
# the base image is built with its encoder and asserts it at build time. A
# static ffmpeg used to sit here to encode PNG sequences, for a Blender without
# one; no job has taken that road on this image, and it cost the image two
# large binaries. A process that left frames and no film is now said aloud.
#
# Each segment leaves for the bucket the moment it exists, rather than waiting
# for the concat. Waiting is how a reclaimed pod loses everything it had
# already made: the last 60-second job died at its own pace with 1440 frames
# rendered and nothing deposited. A segment that is already up is a segment
# nobody has to render again.
for i in $(seq 0 $((jobs - 1))); do
    if [ ! -s "$outdir/seg$i.mp4" ] && ls "$outdir/s$i".*.png > /dev/null 2>&1; then
        echo "NO-ENCODER j$i: frames rendered as PNG and no video — this Blender has no built-in encoder"
    fi
    g=$((seg_base + i))
    if [ -n "$JOB_RUN_PREFIX" ] && [ -s "$outdir/seg$i.mp4" ]; then
        if "$FARM" put "$outdir/seg$i.mp4" "$(run_key "seg$g.mp4")"; then
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
# The film.
#
# With a run prefix, every task — one or many — leaves a receipt naming its
# frames and its segments, then asks for the film. The task that finds every
# receipt assembles it from the bucket: segments read back in parallel,
# concatenated in frame order, the frame count checked against the receipts,
# the film uploaded multipart. The others say TASK-DONE and leave. The launcher
# used to do this once every task had exited, which meant somebody had to be
# watching at the end — and a launcher that returned early made no film.
#
# Without a prefix nothing leaves the machine, and a lone task concatenates
# locally into JOB_OUT as it always did.
if [ "$n" != "$jobs" ]; then
    echo "TASK-INCOMPLETE $task_index/$task_count ($n/$jobs segments)"
    echo VIDEO-MISSING
    exit 1
fi
if [ -n "$JOB_RUN_PREFIX" ]; then
    segs=$(seq -s, "$seg_base" $((seg_base + jobs - 1)))
    "$FARM" receipt "$JOB_RUN_PREFIX" "$task_index" "$first" "$last" "$segs" \
        || { echo RECEIPT-FAILED; exit 1; }
    verdict=$("$FARM" assemble "$JOB_RUN_PREFIX" "$task_count" "$outdir/assemble" "$JOB_FPS")
    rc=$?
    echo "$verdict"
    [ "$rc" = 0 ] || exit 1
    case "$verdict" in
        FILM-UP*) echo RENDER-DONE ;;
        *) echo "TASK-DONE $task_index/$task_count ($n segments, frames $first:$last)"
           exit 0 ;;
    esac
elif [ "$task_count" -gt 1 ]; then
    echo "TASK-DONE $task_index/$task_count — no JOB_RUN_PREFIX, so no film: the segments never left"
    exit 1
else
    # Joined in Rust — samples copied, nothing decoded — and counted from the
    # film written, because a concat that silently drops a segment produces a
    # shorter film, not an error.
    verdict=$("$FARM" concat "$JOB_OUT" $(for i in $(seq 0 $((jobs - 1))); do echo "$outdir/seg$i.mp4"; done))
    echo "$verdict"
    got=$(echo "$verdict" | awk '$1 == "FILM" {print $3}')
    if [ "$got" != "$total" ]; then
        echo "FRAME-COUNT-MISMATCH: ${got:-?} frames dans la vidéo, $total demandées"
        exit 1
    fi
    echo "frames: $got/$total"
    ls -la "$JOB_OUT"
    echo RENDER-DONE
fi
# Two hours sat here, from when the only way to get anything off a pod was
    # to be there while it lived. Everything now leaves through the archive, so
    # this is a courtesy window for an ssh session, not a lifeline — and it is
    # only opened when someone asked for ssh. Cloud Run bills the GPU by the
    # second, so an unconditional minute of sleep is a minute of L4 bought to
    # watch a finished job do nothing.
[ -n "${JOB_SSH_PUBKEY:-}" ] && sleep "${JOB_DONE_SLEEP:-60}" || true
