# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Sourced by a cargo step. Turns sccache on, with an S3 bucket as its cache,
# when the build was given everything that takes:
#
#   build arg  SCCACHE_BUCKET       the bucket; no default
#   build arg  SCCACHE_REGION       its region (optional)
#   build arg  SCCACHE_ENDPOINT     an S3-compatible endpoint (optional)
#   build arg  SCCACHE_PREFIX       key prefix (optional; default <arch>-gnu)
#   secret     sccache_key_id       the access key id
#   secret     sccache_secret       the secret access key
#
# The key comes ONLY as BuildKit secrets, mounted for the one step that
# compiles: never a build arg, never an ENV, so nothing of it reaches a layer,
# the image's history or its build cache.
#
#   RUN --mount=type=secret,id=sccache_key_id \
#       --mount=type=secret,id=sccache_secret \
#       . /usr/local/share/tuile/sccache-env.sh \
#       && cargo build … \
#       && tuile_sccache_stop
#
#   docker buildx build --build-arg SCCACHE_BUCKET=<bucket> \
#       --build-arg SCCACHE_REGION=<region> \
#       --secret id=sccache_key_id,env=<VAR holding the key id> \
#       --secret id=sccache_secret,env=<VAR holding the secret> …
#
# Anything missing and it does nothing: no wrapper, the build compiles as it
# would without this file.

tuile_sccache=off
if [ -n "${SCCACHE_BUCKET:-}" ] \
    && [ -s /run/secrets/sccache_key_id ] && [ -s /run/secrets/sccache_secret ] \
    && command -v sccache > /dev/null 2>&1; then
    AWS_ACCESS_KEY_ID=""
    AWS_SECRET_ACCESS_KEY=""
    IFS= read -r AWS_ACCESS_KEY_ID < /run/secrets/sccache_key_id || true
    IFS= read -r AWS_SECRET_ACCESS_KEY < /run/secrets/sccache_secret || true
    export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY SCCACHE_BUCKET
    SCCACHE_S3_KEY_PREFIX="${SCCACHE_PREFIX:-$(uname -m)-gnu}"
    export SCCACHE_S3_KEY_PREFIX
    if [ -n "${SCCACHE_REGION:-}" ]; then export SCCACHE_REGION; fi
    if [ -n "${SCCACHE_ENDPOINT:-}" ]; then export SCCACHE_ENDPOINT; fi
    # The server on a socket inside this step's container: a builder that runs
    # steps on the host network would otherwise hand two builds one server on
    # 127.0.0.1:4226, and one build's compiles to a server that cannot see its
    # files.
    SCCACHE_SERVER_UDS=/tmp/sccache.sock
    RUSTC_WRAPPER=sccache
    CARGO_INCREMENTAL=0
    export SCCACHE_SERVER_UDS RUSTC_WRAPPER CARGO_INCREMENTAL
    tuile_sccache=on
    echo "sccache: on, s3://${SCCACHE_BUCKET}/${SCCACHE_S3_KEY_PREFIX}"
else
    echo "sccache: off (no bucket, no key or no sccache), compiling without it"
fi

# Stops the server once the step's compiles are done, so its last writes reach
# the bucket before the step's container goes, and prints what it hit.
tuile_sccache_stop() {
    if [ "${tuile_sccache}" = on ]; then
        sccache --stop-server || true
    fi
}
