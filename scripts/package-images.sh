#!/bin/bash
# Package built guest images (vessel/out) into distributable artifacts:
#   dist/kernel-Image.gz  dist/rootfs.img.gz  dist/SHA256SUMS
# gzip on purpose: every macOS ships it; zstd/bsdtar support varies.
set -euo pipefail
cd "$(dirname "$0")/.."

test -f vessel/out/Image || { echo "ERROR: build the kernel first (vessel/build-kernel.sh)"; exit 1; }
test -f vessel/out/rootfs.img || { echo "ERROR: build the rootfs first (vessel/build-rootfs.sh)"; exit 1; }

rm -rf dist && mkdir -p dist
gzip -9 -c vessel/out/Image > dist/kernel-Image.gz
gzip -9 -c vessel/out/rootfs.img > dist/rootfs.img.gz
(cd dist && shasum -a 256 kernel-Image.gz rootfs.img.gz > SHA256SUMS)
ls -lh dist/
