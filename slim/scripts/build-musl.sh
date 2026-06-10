#!/bin/bash
# Build the guest binary (slimd) for aarch64-unknown-linux-musl.
set -euo pipefail
cd "$(dirname "$0")/.."
chmod +x scripts/zigcc-aarch64-musl scripts/zigar
cargo build --release -p slimd --target aarch64-unknown-linux-musl "$@"
ls -la target/aarch64-unknown-linux-musl/release/slimd
