#!/bin/bash
# Build the fully self-contained Nebula.app + DMG:
#   UI + nebula/nebulad sidecars + guest images (gz) as resources.
# Result: install the .app, open it, click Start — no downloads, no Docker.
#
# NEBULA_STRIP=1 (opt-in, see strip-debug.sh): build our binaries WITH line
# tables, split the debug info into dist/debug-symbols/, ship stripped — and
# strip the UI binary, dylibs, and bundled docker CLI too.
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE=aarch64-apple-darwin
STRIP="${NEBULA_STRIP:-0}"

echo "==> release binaries"
if [ "$STRIP" = 1 ]; then
    # Override the workspace profile (strip=true, debug=0): keep line tables
    # at link, then strip-debug.sh separates them so traces are recoverable.
    CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1 \
        cargo build --release -p nebula-cli -p nebulad
    scripts/strip-debug.sh target/release/nebula target/release/nebulad
else
    cargo build --release -p nebula-cli -p nebulad
fi
scripts/sign-dev.sh target/release/nebula target/release/nebulad

echo "==> guest images"
# dist/ artifacts are arch-suffixed (package-images.sh / the guest-images job);
# the in-app resource keeps the plain name (the .app is arm64-only).
case "$(uname -m)" in
    arm64|aarch64) IMG_ARCH=arm64 ;;
    *) IMG_ARCH=x86_64 ;;
esac
if [ ! -f "dist/kernel-Image-$IMG_ARCH.gz" ] || [ ! -f "dist/rootfs-$IMG_ARCH.img.gz" ]; then
    test -f vessel/out/Image && test -f vessel/out/rootfs.img \
        || { echo "ERROR: build guest images first (vessel/build-*.sh) or fetch dist/"; exit 1; }
    scripts/package-images.sh
fi

echo "==> host CLIs (docker/kubectl/helm)"
scripts/fetch-host-clis.sh

echo "==> relocatable libkrun (sandbox/GPU/krun vessels on user machines)"
NEBULA_STRIP="$STRIP" scripts/package-libkrun.sh dist/libkrun

echo "==> staging sidecars + resources"
mkdir -p ui/src-tauri/binaries ui/src-tauri/resources ui/src-tauri/frameworks
cp target/release/nebula  "ui/src-tauri/binaries/nebula-$TRIPLE"
cp target/release/nebulad "ui/src-tauri/binaries/nebulad-$TRIPLE"
cp "dist/kernel-Image-$IMG_ARCH.gz" ui/src-tauri/resources/kernel-Image.gz
cp "dist/rootfs-$IMG_ARCH.img.gz" ui/src-tauri/resources/rootfs.img.gz
cp apps/catalog.json ui/src-tauri/resources/apps-catalog.json
# -> Nebula.app/Contents/Frameworks (the nebula sidecar in Contents/MacOS
#    resolves Frameworks/libkrun.dylib via its ancestor walk).
cp dist/libkrun/*.dylib ui/src-tauri/frameworks/

if [ "$STRIP" = 1 ]; then
    echo "==> stripping bundled host CLIs (kubectl/helm ship pre-stripped; docker doesn't)"
    scripts/strip-debug.sh ui/src-tauri/resources/bin/docker
fi

echo "==> tauri bundle"
# NEBULA_STRIP: cargo strips the UI binary at link (8.1 -> 6.5 MB). It's UI
# glue — Rust panics in it are not the traces we need to recover.
if [ "$STRIP" = 1 ]; then
    (cd ui/src-tauri && CARGO_PROFILE_RELEASE_STRIP=symbols cargo tauri build)
else
    (cd ui/src-tauri && cargo tauri build)
fi

echo "==> done"
ls -lh ui/src-tauri/target/release/bundle/dmg/*.dmg
