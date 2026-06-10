#!/bin/bash
# Build the fully self-contained Nebula.app + DMG:
#   UI + nebula/nebulad sidecars + guest images (gz) as resources.
# Result: install the .app, open it, click Start — no downloads, no Docker.
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE=aarch64-apple-darwin

echo "==> release binaries"
cargo build --release -p nebula-cli -p nebulad
scripts/sign-dev.sh target/release/nebula target/release/nebulad

echo "==> guest images"
if [ ! -f dist/kernel-Image.gz ] || [ ! -f dist/rootfs.img.gz ]; then
    test -f vessel/out/Image && test -f vessel/out/rootfs.img \
        || { echo "ERROR: build guest images first (vessel/build-*.sh) or fetch dist/"; exit 1; }
    scripts/package-images.sh
fi

echo "==> host CLIs (docker/kubectl/helm)"
scripts/fetch-host-clis.sh

echo "==> relocatable libkrun (sandbox/GPU/krun vessels on user machines)"
scripts/package-libkrun.sh dist/libkrun

echo "==> staging sidecars + resources"
mkdir -p ui/src-tauri/binaries ui/src-tauri/resources ui/src-tauri/frameworks
cp target/release/nebula  "ui/src-tauri/binaries/nebula-$TRIPLE"
cp target/release/nebulad "ui/src-tauri/binaries/nebulad-$TRIPLE"
cp dist/kernel-Image.gz dist/rootfs.img.gz ui/src-tauri/resources/
# -> Nebula.app/Contents/Frameworks (the nebula sidecar in Contents/MacOS
#    resolves Frameworks/libkrun.dylib via its ancestor walk).
cp dist/libkrun/*.dylib ui/src-tauri/frameworks/

echo "==> tauri bundle"
(cd ui/src-tauri && cargo tauri build)

echo "==> done"
ls -lh ui/src-tauri/target/release/bundle/dmg/*.dmg
