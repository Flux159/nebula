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

# Guest arch follows the build host (the microVM runs the host ISA).
case "$(uname -m)" in
    arm64|aarch64) MUSL_TARGET=aarch64-unknown-linux-musl; K3S_ARCH=arm64 ;;
    x86_64)        MUSL_TARGET=x86_64-unknown-linux-musl;  K3S_ARCH=amd64 ;;
    *) echo "unsupported build arch: $(uname -m)" >&2; exit 1 ;;
esac

if [ "${NEBULA_STRIP:-0}" = 1 ]; then
    # Opt-in (see scripts/strip-debug.sh): keep line tables at link, then
    # objcopy-split them so guest panics stay symbolicatable.
    CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1 \
        cargo build -p vessel-init -p vessel-agent -p vessel-display --release --target "$MUSL_TARGET"
else
    cargo build -p vessel-init -p vessel-agent -p vessel-display --release --target "$MUSL_TARGET"
fi

mkdir -p vessel/rootfs/bin
cp "target/$MUSL_TARGET/release/vessel-init" vessel/rootfs/bin/
cp "target/$MUSL_TARGET/release/vessel-agent" vessel/rootfs/bin/
cp "target/$MUSL_TARGET/release/vessel-display" vessel/rootfs/bin/
if [ "${NEBULA_STRIP:-0}" = 1 ]; then
    scripts/strip-debug.sh vessel/rootfs/bin/vessel-init vessel/rootfs/bin/vessel-agent vessel/rootfs/bin/vessel-display
fi

# slim flavor: stage slimd from the canonical in-repo slim workspace (slim/,
# built by slim/scripts/build-musl.sh). vessel-init starts it instead of
# dockerd+containerd when /usr/local/bin/slimd is present.
# .slimkeep is the always-present anchor for the optional-COPY in the
# Dockerfile (so non-slim builds don't error on a missing slimd).
rm -f vessel/rootfs/bin/slimd vessel/rootfs/bin/pause
touch vessel/rootfs/bin/.slimkeep
if [ "${FLAVOR:-full}" = "slim" ]; then
    SLIMD_BIN="${SLIMD_BIN:-slim/target/$MUSL_TARGET/release/slimd}"
    test -f "$SLIMD_BIN" || { echo "ERROR: slimd not found at $SLIMD_BIN (build it: slim/scripts/build-musl.sh)" >&2; exit 1; }
    # Staleness guard: refuse to ship a slimd older than the slim sources — this
    # silently shipped stale binaries twice when SLIMD_BIN defaulted to a
    # different checkout. Pass SLIMD_BIN explicitly to override.
    if [ -n "$(find slim/crates -name '*.rs' -newer "$SLIMD_BIN" -print -quit 2>/dev/null)" ]; then
        echo "ERROR: $SLIMD_BIN is older than slim/crates sources — rebuild it (slim/scripts/build-musl.sh) or pass SLIMD_BIN explicitly" >&2
        exit 1
    fi
    cp "$SLIMD_BIN" vessel/rootfs/bin/slimd
    # pause: tiny static pod-sandbox binary (nebula/pause:slim). Optional — if
    # absent, slimd falls back to the app image + sleep for the sandbox.
    PAUSE_BIN="${PAUSE_BIN:-slim/target/$MUSL_TARGET/release/pause}"
    if [ -f "$PAUSE_BIN" ]; then
        cp "$PAUSE_BIN" vessel/rootfs/bin/pause
    else
        echo "warn: pause binary not found at $PAUSE_BIN — pod sandboxes fall back to app-image+sleep" >&2
    fi
fi

# Embedder hooks: OVERLAY=dir (copied over /), SETUP=script (runs in build).
OVERLAY="${OVERLAY:-}"
SETUP="${SETUP:-}"
rm -rf vessel/rootfs/overlay vessel/rootfs/setup.sh
mkdir -p vessel/rootfs/overlay
if [ -n "$OVERLAY" ]; then
    test -d "$OVERLAY" || { echo "ERROR: OVERLAY dir not found: $OVERLAY" >&2; exit 1; }
    cp -R "$OVERLAY"/ vessel/rootfs/overlay/
    echo "overlay: $OVERLAY"
fi
if [ -n "$SETUP" ]; then
    test -f "$SETUP" || { echo "ERROR: SETUP script not found: $SETUP" >&2; exit 1; }
    cp "$SETUP" vessel/rootfs/setup.sh
    echo "setup: $SETUP"
else
    printf '#!/bin/sh
true
' > vessel/rootfs/setup.sh
fi
trap 'rm -rf vessel/rootfs/overlay vessel/rootfs/setup.sh' EXIT

FLAVOR="${FLAVOR:-full}"
if [ "$FLAVOR" = "full" ]; then OUT_NAME=rootfs.img; else OUT_NAME="rootfs-$FLAVOR.img"; fi
SIZE_MB="${ROOTFS_SIZE_MB:-2048}"
[ "$FLAVOR" = "minimal" ] && SIZE_MB="${ROOTFS_SIZE_MB:-512}"
[ "$FLAVOR" = "slim" ] && SIZE_MB="${ROOTFS_SIZE_MB:-1024}"
docker build \
    --build-arg FLAVOR="$FLAVOR" \
    --build-arg ROOTFS_SIZE_MB="$SIZE_MB" \
    --build-arg K3S_ARCH="$K3S_ARCH" \
    --target export \
    --output type=local,dest=/tmp/nebula-rootfs-build \
    vessel/rootfs/
mkdir -p vessel/out
mv /tmp/nebula-rootfs-build/rootfs.img "vessel/out/$OUT_NAME"
ls -la "vessel/out/$OUT_NAME"
