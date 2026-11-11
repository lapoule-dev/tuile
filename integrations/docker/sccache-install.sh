#!/bin/sh
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Installs sccache, the compiler cache, from its official static release:
# version and checksums pinned here, the archive checked before anything of it
# runs. Installing it turns nothing on — `sccache-env.sh` decides that, per
# build, from what the build was given.
#
#   COPY integrations/docker/sccache-install.sh integrations/docker/sccache-env.sh /usr/local/share/tuile/
#   RUN sh /usr/local/share/tuile/sccache-install.sh
#
# Needs curl, sha256sum and tar. A machine with no pinned release gets no
# sccache, and its builds compile as they always did.
set -eu

version=0.19.0
case "$(uname -m)" in
    x86_64)
        arch=x86_64
        sum=aa73c7270c997894038c0d8f0d297865f57569e1687883322bc431713d4bb9dd ;;
    aarch64 | arm64)
        arch=aarch64
        sum=3e47fc4a71b1ddd1b4294a0de9d34498922fdf41e3cd0bda5df3954e3d6deb9d ;;
    *)
        echo "sccache: no release pinned for $(uname -m), not installed" >&2
        exit 0 ;;
esac

name="sccache-v${version}-${arch}-unknown-linux-musl"
tmp="$(mktemp -d)"
curl -sSfL -o "${tmp}/sccache.tar.gz" \
    "https://github.com/mozilla/sccache/releases/download/v${version}/${name}.tar.gz"
echo "${sum}  ${tmp}/sccache.tar.gz" | sha256sum -c -
tar xzf "${tmp}/sccache.tar.gz" -C "${tmp}"
install -m 0755 "${tmp}/${name}/sccache" /usr/local/bin/sccache
rm -rf "${tmp}"
sccache --version
