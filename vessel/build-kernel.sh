#!/bin/bash
# Build the Nebula guest kernel (arm64 Image) in a Linux container.
# Output: vessel/out/Image + vessel/out/kernel.config
set -euo pipefail
cd "$(dirname "$0")"
KERNEL_VERSION="${KERNEL_VERSION:-6.12.58}"
docker build \
    --build-arg KERNEL_VERSION="$KERNEL_VERSION" \
    --target export \
    --output type=local,dest=out \
    kernel/
ls -la out/Image
