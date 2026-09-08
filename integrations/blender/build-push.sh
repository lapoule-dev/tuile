#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds a farm image on the k8s builder and pushes it where the pods are.
#
# # Why a second registry at all
#
# The images are built in Europe and pulled by GPU pods in the United States.
# Measured on a cold US-IL host pulling stl/blender-globe from Harbor:
# **seventeen minutes still downloading**, before a single frame — at
# $0.74/h that is a quarter of a dollar of rent paid to a network, and it is
# paid again on every pod. The same image in ECR us-west-1 (Northern
# California) sits one region away from the pods instead of one ocean.
#
# Harbor stays the source of truth for the layered stack (the base images and
# their cache live there); ECR is a delivery mirror for what a pod pulls.
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
#   ./integrations/blender/build-push.sh globe            # build + push to ECR
#   ./integrations/blender/build-push.sh globe --runpod   # …and refresh RunPod
#   ./integrations/blender/build-push.sh globe --harbor   # …and mirror Harbor
#
set -euo pipefail

AWS_PROFILE_NAME="${TUILE_AWS_PROFILE:-sportstracklive}"
AWS_REGION_NAME="${TUILE_AWS_REGION:-us-west-1}"
ECR_ACCOUNT="${TUILE_ECR_ACCOUNT:-057321054380}"
ECR_HOST="${ECR_ACCOUNT}.dkr.ecr.${AWS_REGION_NAME}.amazonaws.com"
HARBOR_HOST="harbor.sportstracklive.com"
BUILDER="${TUILE_BUILDER:-stl-builder-k8s}"
RUNPOD_CRED_NAME="${TUILE_RUNPOD_CRED:-ecr-usw1-stl}"

# Which image, and how it is built. The tag travels with the name: a pod
# pulling `globe` must get the same bits the builder just made.
case "${1:-}" in
    globe)
        DOCKERFILE=integrations/blender/Dockerfile.globe
        REPO=stl/blender-globe
        TAG=5.1-su
        CONTEXT=.
        ;;
    render)
        DOCKERFILE=integrations/blender/Dockerfile
        REPO=stl/blender-render
        TAG=5.1
        CONTEXT=integrations/blender
        ;;
    *)
        echo "usage: $0 {globe|render} [--runpod] [--harbor]" >&2
        exit 2
        ;;
esac
shift

WITH_RUNPOD=0
WITH_HARBOR=0
for arg in "$@"; do
    case "$arg" in
        --runpod) WITH_RUNPOD=1 ;;
        --harbor) WITH_HARBOR=1 ;;
        *) echo "argument inconnu: $arg" >&2; exit 2 ;;
    esac
done

cd "$(dirname "$0")/../.."

# The repository must exist before a push names it; creating it here keeps a
# new region one command away instead of a console visit.
if ! aws ecr describe-repositories --profile "$AWS_PROFILE_NAME" \
        --region "$AWS_REGION_NAME" --repository-names "$REPO" \
        > /dev/null 2>&1; then
    echo "== création du dépôt ECR $REPO"
    aws ecr create-repository --profile "$AWS_PROFILE_NAME" \
        --region "$AWS_REGION_NAME" --repository-name "$REPO" \
        --image-tag-mutability MUTABLE > /dev/null
fi

# buildx forwards the client's credentials to the builder for the push, so the
# login happens here even though nothing is built here.
echo "== login ECR ($AWS_REGION_NAME)"
aws ecr get-login-password --profile "$AWS_PROFILE_NAME" \
    --region "$AWS_REGION_NAME" \
    | docker login --username AWS --password-stdin "$ECR_HOST" > /dev/null

TAGS=(-t "${ECR_HOST}/${REPO}:${TAG}")
[ "$WITH_HARBOR" = 1 ] && TAGS+=(-t "${HARBOR_HOST}/${REPO}:${TAG}")

echo "== build sur $BUILDER → ${ECR_HOST}/${REPO}:${TAG}"
t0=$(date +%s)
docker buildx build --builder "$BUILDER" --platform linux/amd64 \
    -f "$DOCKERFILE" "${TAGS[@]}" --push "$CONTEXT"
echo "== poussée en $(($(date +%s) - t0))s"

if [ "$WITH_RUNPOD" = 1 ]; then
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
