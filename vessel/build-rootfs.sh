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
        cargo build -p vessel-init -p vessel-agent --release --target "$MUSL_TARGET"
else
    cargo build -p vessel-init -p vessel-agent --release --target "$MUSL_TARGET"
fi

mkdir -p vessel/rootfs/bin
cp "target/$MUSL_TARGET/release/vessel-init" vessel/rootfs/bin/
cp "target/$MUSL_TARGET/release/vessel-agent" vessel/rootfs/bin/
if [ "${NEBULA_STRIP:-0}" = 1 ]; then
    scripts/strip-debug.sh vessel/rootfs/bin/vessel-init vessel/rootfs/bin/vessel-agent
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
docker build \
    --build-arg FLAVOR="$FLAVOR" \
    --build-arg ROOTFS_SIZE_MB="$SIZE_MB" \
    --build-arg K3S_ARCH="$K3S_ARCH" \
    --target export \
    --output type=local,dest=/tmp/nebula-rootfs-build \
    vessel/rootfs/
mv /tmp/nebula-rootfs-build/rootfs.img "vessel/out/$OUT_NAME"
ls -la "vessel/out/$OUT_NAME"
