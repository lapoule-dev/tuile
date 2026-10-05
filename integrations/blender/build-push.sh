#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds a farm image on the k8s builder and pushes it where the workers are.
#
# # Why more than one registry
#
# The images are built in Europe and pulled by whatever rents us a GPU.
# Measured on a cold US-IL host pulling blender-globe from the home registry:
# **seventeen minutes still downloading**, before a single frame — at
# $0.74/h that is a quarter of a dollar of rent paid to a network, and it is
# paid again on every worker. An image one region away from the worker instead
# of one ocean is the whole point.
#
# So: **the home registry (`$TUILE_REGISTRY`) is the source of truth** for the layered stack — the base
# images and their build cache live there, and nothing pulls them at run time.
# The others are delivery mirrors, each next to a place we can rent a card:
#
#   ecr     us-west-1, Northern California — next to the RunPod pods
#   gar     europe-west1, Belgium — next to the Cloud Run L4s
#
# # The ECR wrinkle worth knowing before it bites
#
# A private ECR repository cannot be pulled anonymously — no repository policy
# makes it public — and its authorisation token **expires after 12 hours**. A
# RunPod registry credential holding a stale token fails the pull with a
# generic "unauthorized", which reads like a wrong image name. So `--runpod`
# re-registers the credential with a fresh token as part of the push, and a
# job launched more than twelve hours later must push (or refresh) again.
#
# Google's token expires too (one hour), but nothing stores it: Cloud Run pulls
# with the job's own service account, which holds `artifactregistry.reader`
# permanently. There is no credential to go stale.
#
#   ./integrations/blender/build-push.sh shared-usd          # the base, → registry
#   ./integrations/blender/build-push.sh globe --gcp         # → ECR + GAR
#   ./integrations/blender/build-push.sh globe --runpod      # → ECR, refresh cred
#   ./integrations/blender/build-push.sh globe --registry    # …and mirror the registry
#
# # Configuration
#
# Nothing about the deployment is baked in; it all comes from the environment.
#
#   TUILE_BUILDER        buildx builder to build on                  (always)
#   TUILE_REGISTRY       home registry, with any namespace, e.g.
#                        `registry.example.com/team` — the base images
#                        live there and every Dockerfile is built with
#                        `--build-arg REGISTRY=$TUILE_REGISTRY`         (always)
#   TUILE_REPO_PREFIX    optional namespace prepended to the ECR repository
#   TUILE_AWS_PROFILE    AWS CLI profile                             (ecr)
#   TUILE_AWS_REGION     ECR region, default us-west-1               (ecr)
#   TUILE_ECR_ACCOUNT    AWS account id owning the ECR repositories  (ecr)
#   TUILE_GCP_PROJECT    Google Cloud project                        (gar)
#   TUILE_GCP_REGION     Artifact Registry region, default europe-west1
#   TUILE_GCP_REPO       Artifact Registry repository, default tuile
#   TUILE_RUNPOD_CRED    name of the RunPod registry credential      (--runpod)
#
set -euo pipefail

# Fails with the variable's name and what it is for when it is unset.
need() {
    if [ -z "${!1:-}" ]; then
        echo "error: $1 is not set — $2" >&2
        exit 2
    fi
}

AWS_REGION_NAME="${TUILE_AWS_REGION:-us-west-1}"
GCP_REGION="${TUILE_GCP_REGION:-europe-west1}"
GCP_REPO="${TUILE_GCP_REPO:-tuile}"
GAR_HOST="${GCP_REGION}-docker.pkg.dev"

# Which image, how it is built, and where it is expected.
#
# The tag travels with the name: a worker pulling `globe` must get the same bits
# the builder just made. `shared-usd` carries a date tag because it is rebuilt
# rarely and its contents change meaningfully when it is : le nom porte la
# version de Blender puis celle de l'USD contre lequel il est lié — 5.2-2603,
# c'est Blender 5.2.2 sur OpenUSD 26.03.
case "${1:-}" in
    shared-usd)
        DOCKERFILE=integrations/blender/Dockerfile.blender-shared-usd
        IMAGE=blender-shared-usd
        TAG=5.2-2605
        CONTEXT=.
        # A base image. No worker ever pulls it, so it needs no mirror.
        DESTS=(registry)
        ;;
    globe)
        DOCKERFILE=integrations/blender/Dockerfile.globe
        IMAGE=blender-globe
        TAG="${TUILE_GLOBE_TAG:-5.1-su}"
        CONTEXT=.
        DESTS=(ecr)
        ;;
    render)
        DOCKERFILE=integrations/blender/Dockerfile
        IMAGE=blender-render
        TAG=5.1
        CONTEXT=integrations/blender
        DESTS=(ecr)
        ;;
    *)
        echo "usage: $0 {shared-usd|globe|render} [--gcp] [--runpod] [--registry]" >&2
        exit 2
        ;;
esac
shift

need TUILE_BUILDER "the buildx builder to build on"
need TUILE_REGISTRY "the home registry the base images are pulled from"
BUILDER="$TUILE_BUILDER"
REGISTRY="$TUILE_REGISTRY"
# The ECR repository name; the registry and Artifact Registry use the bare
# image name under their own namespace.
REPO="${TUILE_REPO_PREFIX:+${TUILE_REPO_PREFIX}/}${IMAGE}"

WITH_RUNPOD=0
BASE_OVERRIDE=""
prev=""
for arg in "$@"; do
    # `--base <tag>` : construire `globe` sur une autre base que celle du
    # Dockerfile. Sert quand la base suivante est encore en compilation et
    # qu'on veut livrer avec celle d'avant — ce qui est arrivé le 16 septembre,
    # une base instrumentée mettant une heure et demie pendant qu'un rendu
    # attendait.
    if [ "$prev" = "--base" ]; then BASE_OVERRIDE="$arg"; prev=""; continue; fi
    case "$arg" in
        --base)   prev="--base" ;;
        --gcp)    DESTS+=(gar) ;;
        --registry) DESTS+=(registry) ;;
        --runpod) DESTS+=(ecr); WITH_RUNPOD=1 ;;
        *) echo "argument inconnu: $arg" >&2; exit 2 ;;
    esac
done

# A destination named twice must be logged into once and pushed to once.
seen=()
for dest in "${DESTS[@]}"; do
    case " ${seen[*]-} " in *" $dest "*) ;; *) seen+=("$dest") ;; esac
done
DESTS=("${seen[@]}")
if [ "$WITH_RUNPOD" = 1 ]; then
    need TUILE_RUNPOD_CRED "the name of the RunPod registry credential to refresh"
fi

cd "$(dirname "$0")/../.."

TAGS=()
IMAGES=()
for dest in "${DESTS[@]}"; do
    case "$dest" in
        ecr)
            need TUILE_AWS_PROFILE "the AWS CLI profile used to push to ECR"
            need TUILE_ECR_ACCOUNT "the AWS account id owning the ECR repositories"
            AWS_PROFILE_NAME="$TUILE_AWS_PROFILE"
            ECR_HOST="${TUILE_ECR_ACCOUNT}.dkr.ecr.${AWS_REGION_NAME}.amazonaws.com"
            # The repository must exist before a push names it; creating it
            # here keeps a new region one command away instead of a console
            # visit.
            if ! aws ecr describe-repositories --profile "$AWS_PROFILE_NAME" \
                    --region "$AWS_REGION_NAME" --repository-names "$REPO" \
                    > /dev/null 2>&1; then
                echo "== création du dépôt ECR $REPO"
                aws ecr create-repository --profile "$AWS_PROFILE_NAME" \
                    --region "$AWS_REGION_NAME" --repository-name "$REPO" \
                    --image-tag-mutability MUTABLE > /dev/null
            fi
            # buildx forwards the client's credentials to the builder for the
            # push, so the login happens here even though nothing is built here.
            echo "== login ECR ($AWS_REGION_NAME)"
            aws ecr get-login-password --profile "$AWS_PROFILE_NAME" \
                --region "$AWS_REGION_NAME" \
                | docker login --username AWS --password-stdin "$ECR_HOST" \
                > /dev/null
            IMAGES+=("${ECR_HOST}/${REPO}:${TAG}")
            ;;
        gar)
            need TUILE_GCP_PROJECT "the Google Cloud project hosting Artifact Registry"
            # Artifact Registry nests images under a *repository*, so any
            # namespace prefix is dropped: the repository is the namespace, and
            # it is created by whoever provisions the cloud project, not here.
            # The job definition that pulls the image must name the very path
            # built below, so a change to either must be a change to both.
            #
            # gcloud's python is picked up from the environment, and a terminal
            # opened before ~/.zshrc was fixed still exports a version Homebrew
            # removed. Pinning it here makes the script independent of which
            # shell started it.
            #
            # An inherited value is honoured only if it EXISTS. `:-` alone was
            # not enough: a shell exporting a removed interpreter is precisely
            # the case this guard is for, and `:-` keeps a set-but-dead value.
            # gcloud then dies on `exec: cannot execute`, in a sentence that
            # says nothing about python and nothing about authentication.
            [ -x "${CLOUDSDK_PYTHON:-}" ] \
                || CLOUDSDK_PYTHON=/opt/homebrew/bin/python3.13
            [ -x "$CLOUDSDK_PYTHON" ] || CLOUDSDK_PYTHON=$(command -v python3)
            export CLOUDSDK_PYTHON
            echo "== login Artifact Registry ($GCP_REGION)"
            gcloud auth print-access-token \
                | docker login --username oauth2accesstoken \
                    --password-stdin "$GAR_HOST" > /dev/null
            IMAGES+=("${GAR_HOST}/${TUILE_GCP_PROJECT}/${GCP_REPO}/${IMAGE}:${TAG}")
            ;;
        registry)
            IMAGES+=("${REGISTRY}/${IMAGE}:${TAG}")
            ;;
    esac
done

for image in "${IMAGES[@]}"; do TAGS+=(-t "$image"); done

echo "== build sur $BUILDER"
printf '   → %s\n' "${IMAGES[@]}"
t0=$(date +%s)
BUILD_ARGS=(--build-arg "REGISTRY=${REGISTRY}")
if [ -n "$BASE_OVERRIDE" ]; then
    BUILD_ARGS+=(--build-arg "BASE=${REGISTRY}/blender-shared-usd:${BASE_OVERRIDE}")
    echo "   base forcée: ${BASE_OVERRIDE}"
fi
# Extra `--build-arg` pairs, because the job counts belong to the machine and
# not to the recipe.
#
# `BUILD_JOBS` and `USD_BUILD_JOBS` default to what fits the reference k8s builder —
# 8 cores for 12 GiB, where four parallel compilers already reached the ceiling.
# On a 32-core builder those defaults leave 28 cores idle, so `build-on-gcp.sh`
# raises them here. Space-separated `name=value`, forwarded verbatim.
for pair in ${TUILE_EXTRA_BUILD_ARGS:-}; do
    BUILD_ARGS+=(--build-arg "$pair")
    echo "   build-arg: $pair"
done
docker buildx build --builder "$BUILDER" --platform linux/amd64 \
    -f "$DOCKERFILE" "${TAGS[@]}" "${BUILD_ARGS[@]}" \
    --push "$CONTEXT"
echo "== poussée en $(($(date +%s) - t0))s"

if [ "$WITH_RUNPOD" = 1 ]; then
    RUNPOD_CRED_NAME="$TUILE_RUNPOD_CRED"
    # RUNPOD_API_KEY lives in tuile/.env, gitignored. The password is an ECR
    # token: it is written to the API and never echoed here.
    set -a; . ./.env; set +a
    : "${RUNPOD_API_KEY:?RUNPOD_API_KEY absent de .env}"
    token=$(aws ecr get-login-password --profile "$AWS_PROFILE_NAME" \
                --region "$AWS_REGION_NAME")
    api="https://rest.runpod.io/v1"
    ua="tuile-build-push/1.0"
    # Replace rather than update: a credential is a password, and a stale one
    # fails the pull with an unauthorised that reads like a missing image.
    old=$(curl -fsS -H "Authorization: Bearer $RUNPOD_API_KEY" \
              -H "User-Agent: $ua" "$api/containerregistryauth" \
          | python3 -c "import json,sys
data = json.load(sys.stdin)
rows = data if isinstance(data, list) else data.get('containerRegistryAuths', [])
print(next((r['id'] for r in rows if r.get('name') == '$RUNPOD_CRED_NAME'), ''))")
    if [ -n "$old" ]; then
        curl -fsS -X DELETE -H "Authorization: Bearer $RUNPOD_API_KEY" \
            -H "User-Agent: $ua" "$api/containerregistryauth/$old" > /dev/null
    fi
    python3 - "$RUNPOD_API_KEY" "$RUNPOD_CRED_NAME" "$token" <<'PY' > /dev/null
import json, sys, urllib.request
key, name, token = sys.argv[1], sys.argv[2], sys.argv[3]
body = json.dumps({"name": name, "username": "AWS", "password": token}).encode()
req = urllib.request.Request(
    "https://rest.runpod.io/v1/containerregistryauth", data=body,
    headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json",
             "User-Agent": "tuile-build-push/1.0"}, method="POST")
urllib.request.urlopen(req).read()
PY
    echo "== credential RunPod '$RUNPOD_CRED_NAME' rafraîchi (valable 12 h)"
    echo "   lancer avec: --image ${ECR_HOST}/${REPO}:${TAG} --registry-name $RUNPOD_CRED_NAME"
fi
