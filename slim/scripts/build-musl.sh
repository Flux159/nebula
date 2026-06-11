#!/bin/bash
# Build the guest binaries (slimd + the pod-sandbox pause) for
# aarch64-unknown-linux-musl.
set -euo pipefail
cd "$(dirname "$0")/.."
chmod +x scripts/zigcc-aarch64-musl scripts/zigar
cargo build --release -p slimd -p pause --target aarch64-unknown-linux-musl "$@"
ls -la target/aarch64-unknown-linux-musl/release/slimd target/aarch64-unknown-linux-musl/release/pause
