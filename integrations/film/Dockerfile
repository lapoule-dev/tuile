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
# The build has been through a compiler; nothing here has yet run on a card.

ARG CUDA=12.4.1
ARG UBUNTU=22.04
ARG NVIDIA_DRIVER=550

FROM nvidia/cuda:${CUDA}-devel-ubuntu${UBUNTU} AS build
ARG NVIDIA_DRIVER
# libnvidia-encode and libnvcuvid come with the driver, not with CUDA. The
# bindings' build wants both, side by side and under their unversioned
# names, which only the linker ever asks for and no package ships; at run
# time the host's own are mounted in their place. CUDA itself is not linked:
# it is opened at run time, and here only `nvcc` is asked its version.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake curl nasm pkg-config \
        libnvidia-encode-${NVIDIA_DRIVER} libnvidia-decode-${NVIDIA_DRIVER} \
    && rm -rf /var/lib/apt/lists/* \
    && cd /usr/lib/x86_64-linux-gnu \
    && for lib in libnvidia-encode libnvcuvid; do \
        test -e $lib.so.1 && ln -sf $lib.so.1 $lib.so || exit 1; \
    done
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH CUDA_PATH=/usr/local/cuda
WORKDIR /src
COPY . .
# How many compilers at once: the builder's memory decides, not the recipe.
ARG FILM_BUILD_JOBS=4
# x86 has an assembler here, so rav1e's assembly comes along for the
# machines whose card encodes nothing. The registry and the build directory
# stay with the builder between two builds, so a correction costs the crates
# it touches and not the whole tree. The last line shows what the binary
# asks the machine for: the card's encoder must be in it.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -j ${FILM_BUILD_JOBS} \
        -p tuile-film-native --features nvenc,asm \
    && cp target/release/tuile-film-render /usr/local/bin/ \
    && ldd /usr/local/bin/tuile-film-render \
    && ldd /usr/local/bin/tuile-film-render | grep -q libnvidia-encode

FROM nvidia/cuda:${CUDA}-runtime-ubuntu${UBUNTU}
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libvulkan1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/tuile-film-render /usr/local/bin/
# The card's Vulkan driver, its encoder and CUDA, all from the host.
ENV NVIDIA_DRIVER_CAPABILITIES=all TUILE_CACHE_DIR=/cache
VOLUME /cache
ENTRYPOINT ["tuile-film-render"]
