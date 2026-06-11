#!/bin/bash
# Package built guest images (vessel/out) into distributable artifacts:
#   dist/kernel-Image-<arch>.gz  dist/rootfs-<arch>.img.gz  dist/SHA256SUMS-<arch>
# gzip on purpose: every macOS ships it; zstd/bsdtar support varies; and the
# CLI decompresses with flate2 anyway.
# <arch> is arm64 or x86_64 (build host arch unless ARCH is set).
set -euo pipefail
cd "$(dirname "$0")/.."

case "${ARCH:-$(uname -m)}" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64) ARCH=x86_64 ;;
  *) echo "ERROR: unsupported arch ${ARCH:-$(uname -m)}"; exit 1 ;;
esac

test -f vessel/out/Image || { echo "ERROR: build the kernel first (vessel/build-kernel.sh)"; exit 1; }
test -f vessel/out/rootfs.img || { echo "ERROR: build the rootfs first (vessel/build-rootfs.sh)"; exit 1; }

rm -rf dist && mkdir -p dist
gzip -9 -c vessel/out/Image > "dist/kernel-Image-$ARCH.gz"
gzip -9 -c vessel/out/rootfs.img > "dist/rootfs-$ARCH.img.gz"
(cd dist && shasum -a 256 "kernel-Image-$ARCH.gz" "rootfs-$ARCH.img.gz" > "SHA256SUMS-$ARCH")
ls -lh dist/
