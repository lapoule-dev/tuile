#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Declares the film jobs (`__main__.py`) from the operator's environment:
#
#   TUILE_GCP_PROJECT, TUILE_FILM_IMAGE, TUILE_FILM_SERVICE_ACCOUNT,
#   TUILE_STORE_ENDPOINT, TUILE_STORE_BUCKET, TUILE_STORE_ACCESS_KEY_ID,
#   TUILE_STORE_SECRET_ACCESS_KEY, TUILE_TILES_BUCKET
#
# The GCP provider reads application-default credentials; where those are a
# person's login under a reauthentication policy, give it the token of the
# identity gcloud runs as: GOOGLE_OAUTH_ACCESS_TOKEN=$(gcloud auth
# print-access-token).
#
# The state is kept where PULUMI_BACKEND_URL says (default: a directory in
# the operator's home); its secrets under PULUMI_CONFIG_PASSPHRASE.
#
#   integrations/film/infra/up.sh            preview
#   integrations/film/infra/up.sh --yes      apply
set -euo pipefail
cd "$(dirname "$0")"
: "${TUILE_GCP_PROJECT:?}" "${TUILE_FILM_IMAGE:?}" "${TUILE_FILM_SERVICE_ACCOUNT:?}"
: "${TUILE_STORE_ENDPOINT:?}" "${TUILE_STORE_BUCKET:?}" "${TUILE_STORE_ACCESS_KEY_ID:?}"
: "${TUILE_STORE_SECRET_ACCESS_KEY:?}" "${TUILE_TILES_BUCKET:?}"
export PULUMI_BACKEND_URL="${PULUMI_BACKEND_URL:-file://$HOME/.pulumi-tuile-film}"
export PULUMI_CONFIG_PASSPHRASE="${PULUMI_CONFIG_PASSPHRASE-}"
mkdir -p "${PULUMI_BACKEND_URL#file://}" 2>/dev/null || true
pulumi stack select --create film >/dev/null
pulumi config set gcp:project "$TUILE_GCP_PROJECT"
pulumi config set image "$TUILE_FILM_IMAGE"
pulumi config set serviceAccount "$TUILE_FILM_SERVICE_ACCOUNT"
pulumi config set storeBucket "$TUILE_STORE_BUCKET"
pulumi config set tilesBucket "$TUILE_TILES_BUCKET"
pulumi config set --secret storeEndpoint "$TUILE_STORE_ENDPOINT"
pulumi config set --secret storeAccessKeyId "$TUILE_STORE_ACCESS_KEY_ID"
pulumi config set --secret storeSecretAccessKey "$TUILE_STORE_SECRET_ACCESS_KEY"
if [ "${1:-}" = "--yes" ]; then
    pulumi up --yes --skip-preview
else
    pulumi preview
fi
