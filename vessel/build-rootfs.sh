#!/bin/bash
# Build the Vessel rootfs image. Output: vessel/out/rootfs.img
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
cd "$(dirname "$0")/.."

cargo build -p vessel-init -p vessel-agent --release --target aarch64-unknown-linux-musl

mkdir -p vessel/rootfs/bin
cp target/aarch64-unknown-linux-musl/release/vessel-init vessel/rootfs/bin/
cp target/aarch64-unknown-linux-musl/release/vessel-agent vessel/rootfs/bin/

docker build \
    --target export \
    --output type=local,dest=vessel/out \
    vessel/rootfs/
ls -la vessel/out/rootfs.img
