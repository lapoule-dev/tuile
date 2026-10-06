# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# tuile-film-render for a Linux x86-64 machine with an NVIDIA card: the
# render on Vulkan, the film encoded by the card (NVENC, over CUDA).
#
#   docker build -f integrations/film/Dockerfile -t tuile-film-render .
#   docker run --gpus all -e TUILE_STORE_ENDPOINT=… -e TUILE_STORE_BUCKET=… \
#     -e TUILE_TILES_BUCKET=… -e TUILE_STORE_ACCESS_KEY_ID=… \
#     -e TUILE_STORE_SECRET_ACCESS_KEY=… -v film-cache:/cache \
#     tuile-film-render <packs prefix> --out /out/film.mp4
#
# NOT YET BUILT: written on a machine that can compile neither the NVENC
# bindings nor their CUDA side. The first build is the first time the
# `nvenc` feature meets a compiler; expect to correct it.

ARG CUDA=12.4.1
ARG UBUNTU=22.04
ARG NVIDIA_DRIVER=550

FROM nvidia/cuda:${CUDA}-devel-ubuntu${UBUNTU} AS build
ARG NVIDIA_DRIVER
# libnvidia-encode comes with the driver, not with CUDA: the linker needs
# one here, and at run time the host's own is mounted in its place.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake curl nasm pkg-config \
        libnvidia-encode-${NVIDIA_DRIVER} \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH CUDA_PATH=/usr/local/cuda
WORKDIR /src
COPY . .
# x86 has an assembler here, so rav1e's assembly comes along for the
# machines whose card encodes nothing.
RUN cargo build --release -p tuile-film-native --features nvenc,asm \
    && cp target/release/tuile-film-render /usr/local/bin/

FROM nvidia/cuda:${CUDA}-runtime-ubuntu${UBUNTU}
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libvulkan1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/tuile-film-render /usr/local/bin/
# The card's Vulkan driver, its encoder and CUDA, all from the host.
ENV NVIDIA_DRIVER_CAPABILITIES=all TUILE_CACHE_DIR=/cache
VOLUME /cache
ENTRYPOINT ["tuile-film-render"]
