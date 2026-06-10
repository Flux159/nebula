#!/bin/bash
# Build the Vessel rootfs image. Output: vessel/out/rootfs.img
set -euo pipefail
# Build on the host engine even when `nebula use docker` is active.
export DOCKER_CONTEXT="${NEBULA_BUILD_DOCKER_CONTEXT:-default}"
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
