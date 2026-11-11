# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The tuile Hydra plugin, on top of a prebuilt OpenUSD.
#
#   docker build -f integrations/hydra/Dockerfile -t tuile-hydra:cpu .
#   docker build -f integrations/hydra/Dockerfile \
#       --build-arg BASE=tuile-openusd:26.08-gpu -t tuile-hydra:gpu .
#
# Build the base first — see Dockerfile.openusd. It takes the better part of an
# hour; everything here takes seconds, which is the entire reason the two are
# separate files rather than stages of one build.
#
# Run the spike inside the result:
#
#   docker run --rm tuile-hydra:cpu integrations/hydra/tests/run-spike.sh
#
# The base already sets HDGP_INCLUDE_DEFAULT_RESOLVER, without which procedurals
# are silently skipped and every stage renders blank.

ARG BASE=tuile-openusd:26.08-cpu
FROM ${BASE}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
# sccache, installed always and used only when the build is given a bucket and
# a key (integrations/docker/sccache-env.sh): without them, nothing changes.
COPY integrations/docker/sccache-install.sh integrations/docker/sccache-env.sh /usr/local/share/tuile/
RUN sh /usr/local/share/tuile/sccache-install.sh

WORKDIR /work

# Where CMake writes the plugin: plugin/tuileHydra/{tuileHydra.so,resources/}.
# OpenUSD is pointed at the directory holding plugInfo.json, not at the library.
ENV PXR_PLUGINPATH_NAME=/work/build/plugin/tuileHydra/resources

# Manifests before sources, so a source edit does not re-resolve the dependency
# graph or re-download a crate.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The workspace lists the examples as members, so cargo refuses to resolve
# without their manifests present — even though `-p tuile-hydra` builds none of
# them and downloads none of their dependencies.
COPY examples ./examples
COPY integrations ./integrations

# The shared compilation cache, all optional (integrations/docker/sccache-env.sh):
# the bucket by build arg, the key by BuildKit secret only. CMake's cargo step
# inherits the wrapper from the environment.
ARG SCCACHE_BUCKET
ARG SCCACHE_REGION
ARG SCCACHE_ENDPOINT
ARG SCCACHE_PREFIX
RUN --mount=type=secret,id=sccache_key_id \
    --mount=type=secret,id=sccache_secret \
    . /usr/local/share/tuile/sccache-env.sh \
    && cmake -S integrations/hydra -B build \
        -DCMAKE_BUILD_TYPE=Release \
        -DTUILE_WORKSPACE_ROOT=/work \
        -DCMAKE_PREFIX_PATH=/opt/usd \
    && cmake --build build -j "$(nproc)" \
    && tuile_sccache_stop

CMD ["/bin/bash"]
