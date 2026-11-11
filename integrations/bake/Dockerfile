# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The baker and the driver that re-bakes a pack API's packs in place, for a
# Linux x86-64 machine with no card: a bake selects tiles and fetches them,
# and draws nothing.
#
#   docker buildx build --builder "$TUILE_BUILDER" --platform linux/amd64 \
#     -f integrations/bake/Dockerfile -t "$TUILE_REGISTRY/tuile-bake:<tag>" \
#     --push .
#
# Nothing of a deployment is in the image: the buckets, their keys and the
# source token come from the environment of whatever runs it (see
# `rebake-job.yaml`), and `.dockerignore` keeps the local ones out of the
# build context.

ARG DEBIAN=bookworm

FROM rust:1-slim-${DEBIAN} AS build
# The TLS provider (aws-lc) and zstd are C, built from source by their crates.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake curl pkg-config perl \
    && rm -rf /var/lib/apt/lists/*
# sccache, installed always and used only when the build is given a bucket and
# a key (integrations/docker/sccache-env.sh): without them, nothing changes.
COPY integrations/docker/sccache-install.sh integrations/docker/sccache-env.sh /usr/local/share/tuile/
RUN sh /usr/local/share/tuile/sccache-install.sh
WORKDIR /src
COPY . .
# How many compilers at once: the builder's memory decides, not the recipe.
ARG BAKE_BUILD_JOBS=4
# The shared compilation cache, all optional (integrations/docker/sccache-env.sh):
# the bucket by build arg, the key by BuildKit secret only.
ARG SCCACHE_BUCKET
ARG SCCACHE_REGION
ARG SCCACHE_ENDPOINT
ARG SCCACHE_PREFIX
# The registry and the build directory stay with the builder between two
# builds. The build directory has an id of its own: another image's, built on
# another distribution, holds C objects this one must not link.
RUN --mount=type=secret,id=sccache_key_id \
    --mount=type=secret,id=sccache_secret \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=tuile-bake-target,target=/src/target \
    . /usr/local/share/tuile/sccache-env.sh \
    && cargo build --release --locked -j ${BAKE_BUILD_JOBS} \
        -p tuile-bake -p tuile-farm --bin tuile-bake --bin tuile-migrate-packs \
    && tuile_sccache_stop \
    && cp target/release/tuile-bake target/release/tuile-migrate-packs /usr/local/bin/ \
    && ldd /usr/local/bin/tuile-bake /usr/local/bin/tuile-migrate-packs

FROM debian:${DEBIAN}-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --no-create-home bake
# Side by side: the driver looks for the baker next to itself.
COPY --from=build /usr/local/bin/tuile-bake /usr/local/bin/tuile-migrate-packs /usr/local/bin/
USER 10001:10001
# The ledger, the table being read, the pack being written and the tile cache.
WORKDIR /work
VOLUME /work
ENTRYPOINT ["tuile-migrate-packs"]
