# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The film for a Linux x86-64 machine with an NVIDIA card: the render on
# Vulkan, the film encoded by the card (NVENC, over CUDA).
#
# tuile-film-produce, the entry point, bakes, renders and joins a whole film
# kept in the run store; on a Cloud Run job of N tasks, each task takes its
# share of the packs and the last to finish joins. tuile-film-serve is the
# same render as a service: segments asked over HTTP, on a Cloud Run service
# (`--command tuile-film-serve`). tuile-film-render is beside them, for one
# render by hand.
#
#   docker build -f integrations/film/Dockerfile -t tuile-film .
#   docker run --gpus all -e TUILE_STORE_ENDPOINT=… -e TUILE_STORE_BUCKET=… \
#     -e TUILE_STORE_ACCESS_KEY_ID=… -e TUILE_STORE_SECRET_ACCESS_KEY=… \
#     -e TUILE_TILES_BUCKET=… tuile-film store:tapes/flight.mcap --resolution 3840x2160
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
# The Vulkan loader, at the version of the API the card's driver offers: on
# the Cloud Run L4, driver 580.126 gives Vulkan 1.4.312, and the system's
# loader on this Ubuntu is 1.3.204. Built from Khronos's sources at that
# tag, with no window system: nothing here has a display.
ARG VULKAN=1.4.312
RUN mkdir -p /tmp/vk && cd /tmp/vk \
    && curl -sSfL https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/v${VULKAN}.tar.gz | tar xz \
    && curl -sSfL https://github.com/KhronosGroup/Vulkan-Loader/archive/refs/tags/v${VULKAN}.tar.gz | tar xz \
    && cmake -S Vulkan-Headers-${VULKAN} -B headers -DCMAKE_INSTALL_PREFIX=/opt/vulkan \
    && cmake --install headers \
    && cmake -S Vulkan-Loader-${VULKAN} -B loader -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/opt/vulkan -DVULKAN_HEADERS_INSTALL_DIR=/opt/vulkan \
        -DBUILD_WSI_XCB_SUPPORT=OFF -DBUILD_WSI_XLIB_SUPPORT=OFF \
        -DBUILD_WSI_WAYLAND_SUPPORT=OFF -DBUILD_WSI_DIRECTFB_SUPPORT=OFF -DBUILD_TESTS=OFF \
    && cmake --build loader -j 4 && cmake --install loader \
    && ls -l /opt/vulkan/lib/libvulkan.so.1 \
    && rm -rf /tmp/vk
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH CUDA_PATH=/usr/local/cuda
# sccache, installed always and used only when the build is given a bucket and
# a key (integrations/docker/sccache-env.sh): without them, nothing changes.
COPY integrations/docker/sccache-install.sh integrations/docker/sccache-env.sh /usr/local/share/tuile/
RUN sh /usr/local/share/tuile/sccache-install.sh
WORKDIR /src
COPY . .
# How many compilers at once: the builder's memory decides, not the recipe.
ARG FILM_BUILD_JOBS=4
# The shared compilation cache, all optional (integrations/docker/sccache-env.sh):
# the bucket by build arg, the key by BuildKit secret only.
ARG SCCACHE_BUCKET
ARG SCCACHE_REGION
ARG SCCACHE_ENDPOINT
ARG SCCACHE_PREFIX
# x86 has an assembler here, so rav1e's assembly comes along for the
# machines whose card encodes nothing. The registry and the build directory
# stay with the builder between two builds, so a correction costs the crates
# it touches and not the whole tree. The last line shows what the binary
# asks the machine for: the card's encoder must be in it.
RUN --mount=type=secret,id=sccache_key_id \
    --mount=type=secret,id=sccache_secret \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    . /usr/local/share/tuile/sccache-env.sh \
    && cargo build --release --locked -j ${FILM_BUILD_JOBS} \
        -p tuile-film-native --features nvenc,asm \
        --bin tuile-film-produce --bin tuile-film-serve --bin tuile-film-render --bin tuile-film-compress \
    && tuile_sccache_stop \
    && cp target/release/tuile-film-produce target/release/tuile-film-serve target/release/tuile-film-render target/release/tuile-film-compress /usr/local/bin/ \
    && ldd /usr/local/bin/tuile-film-produce \
    && for bin in tuile-film-produce tuile-film-serve tuile-film-render tuile-film-compress; do \
        ldd /usr/local/bin/$bin | grep -q libnvidia-encode || exit 1; \
        ldd /usr/local/bin/$bin | grep -q libnvcuvid || exit 1; \
    done

FROM nvidia/cuda:${CUDA}-runtime-ubuntu${UBUNTU}
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libegl1 libgbm1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/tuile-film-produce /usr/local/bin/tuile-film-serve /usr/local/bin/tuile-film-render /usr/local/bin/tuile-film-compress /usr/local/bin/
# The loader built above, in place of the system's.
COPY --from=build /opt/vulkan/lib/libvulkan.so* /usr/lib/x86_64-linux-gnu/
# The Vulkan loader finds a driver by its manifest, and the host mounts the
# NVIDIA driver's libraries (on Cloud Run: /usr/local/nvidia/lib64, on the
# library path) without one. Measured on a Cloud Run L4, driver 580:
# - no manifest: "vulkan drivers/libraries could not be loaded";
# - the GLX one (libGLX_nvidia): loaded, then no vkCreateInstance — with no
#   display it does not start;
# - the EGL one (libEGL_nvidia): the L4, Vulkan 1.4. That one.
# The library is named, not pathed: the loader looks it up on the library path.
RUN mkdir -p /etc/vulkan/icd.d \
    && printf '%s\n' '{"file_format_version": "1.0.0", "ICD": {"library_path": "libEGL_nvidia.so.0", "api_version": "1.3.0"}}' \
        > /etc/vulkan/icd.d/nvidia_icd.json
# The card's Vulkan driver, its encoder and CUDA, all from the host.
# The render asks wgpu for Vulkan by name: a card that offers it is not
# left to pick another backend.
ENV NVIDIA_DRIVER_CAPABILITIES=all TUILE_CACHE_DIR=/cache WGPU_BACKEND=vulkan
VOLUME /cache
ENTRYPOINT ["tuile-film-produce"]
