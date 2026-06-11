#!/bin/bash
# Build the guest binaries (slimd + the pod-sandbox pause) for
# <arch>-unknown-linux-musl. ARCH=arm64|aarch64|x86_64 (default aarch64).
set -euo pipefail
cd "$(dirname "$0")/.."
case "${ARCH:-aarch64}" in
  arm64|aarch64) MUSL_TARGET=aarch64-unknown-linux-musl ;;
  x86_64|amd64)  MUSL_TARGET=x86_64-unknown-linux-musl ;;
  *) echo "ERROR: unsupported ARCH ${ARCH}"; exit 1 ;;
esac
chmod +x scripts/zigcc-aarch64-musl scripts/zigcc-x86_64-musl scripts/zigar
cargo build --release -p slimd -p pause --target "$MUSL_TARGET" "$@"
ls -la "target/$MUSL_TARGET/release/slimd" "target/$MUSL_TARGET/release/pause"
