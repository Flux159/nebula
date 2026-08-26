#!/bin/bash
# Build the Nebula guest kernel (arm64 Image) in a Linux container.
# Output: vessel/out/Image + vessel/out/kernel.config
set -euo pipefail
# Pick a working build engine. Honor an explicit override; otherwise prefer
# the current context unless it is nebula-while-down, falling back to any
# context whose daemon responds. (Building via the Nebula engine itself is
# fine — the output is just a file.)
if [ -n "${NEBULA_BUILD_DOCKER_CONTEXT:-}" ]; then
    export DOCKER_CONTEXT="$NEBULA_BUILD_DOCKER_CONTEXT"
elif docker version >/dev/null 2>&1; then
    : # current context works
else
    for ctx in $(docker context ls -q); do
        if DOCKER_CONTEXT="$ctx" docker version >/dev/null 2>&1; then
            export DOCKER_CONTEXT="$ctx"
            echo "using docker context: $ctx" >&2
            break
        fi
    done
fi
docker version >/dev/null 2>&1 || { echo "ERROR: no working docker engine for the build" >&2; exit 1; }
cd "$(dirname "$0")"
KERNEL_VERSION="${KERNEL_VERSION:-6.12.94}"
# ARCH=arm64 (raw Image, VZ/HVF) | x86_64 (ELF vmlinux, KVM/WHP).
# Cross-compiles fine from either container arch.
case "${ARCH:-$(uname -m)}" in
    arm64|aarch64) ARCH=arm64 ;;
    x86_64|amd64)  ARCH=x86_64 ;;
    *) echo "unsupported kernel arch: ${ARCH:-$(uname -m)}" >&2; exit 1 ;;
esac
docker build \
    --build-arg KERNEL_VERSION="$KERNEL_VERSION" \
    --build-arg ARCH="$ARCH" \
    --target export \
    --output type=local,dest=out \
    kernel/
# Canonical artifact name is out/Image on both arches (x86_64 emits an
# ELF vmlinux; everything downstream — install-image, packaging — keys
# off the single name).
if [ "$ARCH" = "x86_64" ] && [ -f out/vmlinux ]; then
    # Overwrite: a stale Image from a previous build must not shadow the
    # fresh vmlinux (the docker export only refreshes the arch's own file).
    #
    # Gate on ARCH, or this runs the other way round: an arm64 build emits
    # out/Image and then a stale x86_64 out/vmlinux from an earlier
    # cross-build clobbers it, silently yielding an x86_64 kernel that VZ
    # cannot boot. BuildKit's local output only refreshes the files the
    # target actually produced, so the other arch's leftovers always survive.
    cp -f out/vmlinux out/Image
fi
ls -la out/Image out/vmlinux 2>/dev/null || true
