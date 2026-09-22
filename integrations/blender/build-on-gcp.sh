#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Wakes a large machine, builds the heavy image on it, puts it back to sleep.
#
# # What this is for, and what it is not for
#
# `blender-shared-usd`, and nothing else. It is the only image that compiles
# OpenUSD and then 8119 objects of Blender; every other image stacks layers on
# top of it, and `stl-builder-k8s` does that perfectly well.
#
# Measured on that builder — 8 cores, 12 GiB — on 22 September 2026: about
# ninety minutes cold, and the heavy stage's cache does not survive from one run
# to the next. `docker buildx du` reported `Reclaimable: 0B` against 136.9 GB,
# the large layers swept by the GC. So fixing one line cost ninety minutes, six
# times in one day. On 32 cores and 128 GiB the same compile takes about twenty.
#
# # Suspended, not destroyed
#
# The disk carries the docker cache, and the cache is the difference between
# twenty minutes and three. So the machine is created **once** and suspended
# between sessions: the bill is the disk, not the cores. This script resumes it
# if it finds it, creates it if it does not, and puts it back to sleep on the
# way out — success, failure or Ctrl-C, through a `trap` on EXIT armed before
# creation. A 32-core machine forgotten over a weekend costs more than the whole
# day of builds it was meant to save.
#
# `--destroy` really removes it, disk and cache included, for when the cache has
# lived long enough or the recipe has changed underneath it.
#
# # What does NOT travel to the machine
#
# No registry credentials. `docker buildx` forwards the **client's** credentials
# to the builder at push time — the k8s path already relies on this, and
# `build-push.sh` says so itself. The machine therefore only compiles: Harbor,
# ECR and GAR stay on the workstation with its own tokens. That is the reason
# for this arrangement rather than a `git clone` on the VM followed by a push
# from the VM, which would mean copying three keyrings onto it. It is also born
# with no service account and no public address.
#
#   ./integrations/blender/build-on-gcp.sh shared-usd
#   ./integrations/blender/build-on-gcp.sh --awake shared-usd   # leave it running
#   ./integrations/blender/build-on-gcp.sh --destroy            # give it back
#
# Targets are `build-push.sh`'s own; whatever follows a `:` is passed straight
# through to it.
set -euo pipefail

ZONE="${TUILE_GCP_ZONE:-europe-west1-b}"
PROJECT="${TUILE_GCP_PROJECT:-first-parser-498510-a4}"
# c2d, and the family matters more than it looks.
#
# `n2d-standard-32` was the obvious pick and it cannot be created here: the
# project's `N2D_CPUS` quota in europe-west1 is **16**, so the request dies on
# `Quota 'N2D_CPUS' exceeded` after the machine name is already printed. The
# region's other families are wide open — `N2_CPUS` 200, `C2D_CPUS` 100,
# `CPUS` 200 — and c2d is the compute-optimised one, which is what a compiler
# farm wants. `n2-standard-32` is the fallback if c2d ever runs out of capacity
# in the zone.
MACHINE="${TUILE_BUILD_MACHINE:-c2d-standard-32}"
DISK_GB="${TUILE_BUILD_DISK_GB:-300}"
VM="${TUILE_BUILD_VM:-tuile-builder}"
BUILDER="tuile-gcp"
AWAKE=0

say() { printf '== %s\n' "$*"; }

state() {
    gcloud compute instances describe "$VM" --zone="$ZONE" --project="$PROJECT" \
        --format='value(status)' 2>/dev/null || true
}

case "${1:-}" in
    --awake) AWAKE=1; shift ;;
    --destroy)
        gcloud compute instances delete "$VM" --zone="$ZONE" --project="$PROJECT" --quiet
        # Here removing them is right: the machine holding the buildkit
        # container is gone, so the builder and context point at nothing.
        docker buildx rm "$BUILDER" >/dev/null 2>&1 || true
        docker context rm -f "$VM" >/dev/null 2>&1 || true
        say "$VM destroyed, disk and cache with it"
        exit 0
        ;;
esac
if [[ $# -eq 0 ]]; then
    echo "usage: $0 [--awake] <target>[:<flag>] ..." >&2
    echo "       $0 --destroy" >&2
    echo "  eg:  $0 shared-usd" >&2
    exit 2
fi
TARGETS=("$@")

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

# ── putting it back to sleep, whatever happens ───────────────────────────────
#
# Armed before creation: if `instances create` dies halfway the machine may
# exist anyway, and that is exactly the case where it gets forgotten.
#
# `suspend` first, because it hands the cores back while keeping everything;
# `stop` as a fallback, because not every machine type can suspend and a stopped
# machine keeps its disk — hence its cache — either way.
cleanup() {
    local code=$?
    # The builder and its context STAY.
    #
    # `docker buildx rm` was here, and it is what emptied the cache this script
    # exists to keep. The `docker-container` driver runs buildkit *in a
    # container on the remote host*, and removing the builder removes that
    # container and the volume holding every cached layer. Measured 22 September
    # 2026, right after boasting that a suspended machine keeps its cache:
    # `docker buildx du` answered `Total: 0B` and `docker images` answered 1.
    # The disk had survived the suspend perfectly; the cleanup had wiped it.
    #
    # Only the default builder is handed back, so the next `docker buildx build`
    # typed by hand does not silently reach for a machine that is now asleep.
    docker buildx use "${TUILE_HOME_BUILDER:-stl-builder-k8s}" >/dev/null 2>&1 || true
    if [[ $AWAKE -eq 1 ]]; then
        say "$VM left running (--awake) — to sleep it: gcloud compute instances suspend $VM --zone=$ZONE"
        return $code
    fi
    if [[ -n "$(state)" ]]; then
        say "suspending $VM"
        gcloud compute instances suspend "$VM" --zone="$ZONE" --project="$PROJECT" --quiet 2>/dev/null \
            || gcloud compute instances stop "$VM" --zone="$ZONE" --project="$PROJECT" --quiet \
            || echo "!! $VM is STILL running — stop it by hand: gcloud compute instances stop $VM --zone=$ZONE" >&2
    fi
    return $code
}
trap cleanup EXIT

# ── the machine ──────────────────────────────────────────────────────────────
#
# Created once, resumed thereafter: the disk carries the docker cache, and the
# resume is what turns a twenty-minute compile into a three-minute one.
#
# `--no-service-account --no-scopes`: it talks to nothing. It compiles what it
# is handed and keeps quiet.
case "$(state)" in
    RUNNING)
        say "$VM already running"
        ;;
    SUSPENDED)
        say "resuming $VM (docker cache intact)"
        gcloud compute instances resume "$VM" --zone="$ZONE" --project="$PROJECT" --quiet
        ;;
    TERMINATED|STOPPED)
        say "starting $VM (docker cache intact)"
        gcloud compute instances start "$VM" --zone="$ZONE" --project="$PROJECT" --quiet
        ;;
    "")
        say "creating $VM ($MACHINE, ${DISK_GB} GB disk, $ZONE)"
        gcloud compute instances create "$VM" \
            --project="$PROJECT" --zone="$ZONE" \
            --machine-type="$MACHINE" \
            --image-family=ubuntu-2404-lts-amd64 --image-project=ubuntu-os-cloud \
            --boot-disk-size="${DISK_GB}GB" --boot-disk-type=pd-balanced \
            --no-service-account --no-scopes \
            --metadata=startup-script='#!/bin/bash
set -eux
command -v docker >/dev/null || curl -fsSL https://get.docker.com | sh
usermod -aG docker "$(getent passwd 1000 | cut -d: -f1)" || true
touch /var/lib/docker-ready
' >/dev/null
        ;;
    *)
        say "waiting: $VM is $(state)"
        for _ in $(seq 1 30); do
            [[ "$(state)" == "RUNNING" ]] && break
            sleep 10
        done
        ;;
esac

say "waiting for ssh"
for _ in $(seq 1 40); do
    if gcloud compute ssh "$VM" --zone="$ZONE" --project="$PROJECT" --tunnel-through-iap \
            --command=true >/dev/null 2>&1; then
        break
    fi
    sleep 10
done

# The startup script cannot know who will log in.
#
# It adds uid 1000 — `ubuntu` on this image — to the `docker` group, but
# `gcloud compute ssh` logs in as a user named after the local account, created
# on first connection. That user is not in the group, so `docker info` is denied
# and a wait on it never ends. Measured 22 September 2026: docker was installed
# and ready at 14:10:49 while the script sat waiting for it.
#
# So the real user is added here, once it exists, and the group only takes
# effect in a NEW session — which is exactly what buildx opens next.
say "granting docker to the ssh user"
gcloud compute ssh "$VM" --zone="$ZONE" --project="$PROJECT" --tunnel-through-iap --command='
    for _ in $(seq 1 60); do
        [ -f /var/lib/docker-ready ] && break
        sleep 5
    done
    [ -f /var/lib/docker-ready ] || { echo "docker never came up on the builder" >&2; exit 1; }
    sudo usermod -aG docker "$(id -un)"
    sudo docker info >/dev/null || { echo "the docker daemon is not answering" >&2; exit 1; }'

say "waiting for docker to answer as that user"
for _ in $(seq 1 12); do
    if gcloud compute ssh "$VM" --zone="$ZONE" --project="$PROJECT" --tunnel-through-iap \
            --command='docker info >/dev/null 2>&1' >/dev/null 2>&1; then
        break
    fi
    sleep 5
done

# ── the remote builder ───────────────────────────────────────────────────────
#
# `config-ssh` writes the entry `ssh://` has to resolve: buildx's docker driver
# goes through the system `ssh`, not through gcloud.
say "attaching the builder"
gcloud compute config-ssh --project="$PROJECT" --quiet >/dev/null
SSH_ALIAS="${VM}.${ZONE}.${PROJECT}"

# Reused when it is there, created when it is not — never recreated.
#
# Recreating is what throws the cache away, so both the context and the builder
# are only made when absent. The context is client-side and cheap; the builder
# is the buildkit container on the machine, and it is the one holding the
# layers.
docker context inspect "$VM" >/dev/null 2>&1 \
    || docker context create "$VM" --docker "host=ssh://${SSH_ALIAS}" >/dev/null
docker buildx inspect "$BUILDER" >/dev/null 2>&1 \
    || docker buildx create --name "$BUILDER" --driver docker-container "$VM" >/dev/null
docker buildx use "$BUILDER"
docker buildx inspect --bootstrap "$BUILDER" >/dev/null
say "builder $BUILDER up on $MACHINE"

# ── the images ───────────────────────────────────────────────────────────────
#
# `build-push.sh` runs HERE, on the workstation: it is the one holding the
# Harbor, ECR and GAR tokens. Only the compiling travels.
#
# And the job counts come from the machine, not from the recipe. The Dockerfile
# defaults fit `stl-builder-k8s` — 8 cores for 12 GiB, where four parallel
# compilers already hit the ceiling and the kernel killed one. Here there are
# 32 cores and 128 GiB, which at the measured ~1.5 GiB per `cc1plus` leaves the
# memory nowhere near the limit; the cores are the constraint, so match them.
JOBS="${TUILE_BUILD_JOBS:-$(( $(echo "$MACHINE" | sed 's/.*-//') ))}"
[[ "$JOBS" =~ ^[0-9]+$ ]] && (( JOBS > 0 )) || JOBS=16
export TUILE_EXTRA_BUILD_ARGS="BUILD_JOBS=${JOBS} USD_BUILD_JOBS=${JOBS}"
say "compiling with -j${JOBS}"

for spec in "${TARGETS[@]}"; do
    target="${spec%%:*}"
    flags="${spec#"$target"}"
    flags="${flags#:}"
    say "image $target ${flags:-}"
    # shellcheck disable=SC2086
    TUILE_BUILDER="$BUILDER" "$REPO_ROOT/integrations/blender/build-push.sh" "$target" $flags
done

say "every image went through"
