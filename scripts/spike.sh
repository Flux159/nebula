#!/bin/bash
# Phase 0 acceptance: boot a throwaway microVM on each backend and verify the
# guest boot marker. Usage: scripts/spike.sh [vz|krun|all]
set -euo pipefail
cd "$(dirname "$0")/.."

WHICH="${1:-all}"

cargo build -p vessel-init --release --target aarch64-unknown-linux-musl
cargo build -p nebula-cli
scripts/sign-dev.sh target/debug/nebula

run_one() {
    echo "=== spike: $1 ==="
    target/debug/nebula up --dev --backend "$1"
}

case "$WHICH" in
    vz|krun) run_one "$WHICH" ;;
    all)
        run_one vz
        # Known issue: stock Alpine kernels panic before console under libkrun;
        # resolved by the Phase 1 custom kernel (see tasks/issues.md).
        run_one krun || echo "NOTE: krun spike failed (known issue: needs Phase 1 custom kernel)"
        ;;
    *) echo "usage: $0 [vz|krun|all]" >&2; exit 2 ;;
esac
