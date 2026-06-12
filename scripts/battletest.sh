#!/bin/bash
# Battle-test entry point (macOS/Linux): build, re-sign (macOS — builds
# invalidate the ad-hoc signature), run. Windows: scripts/battletest.ps1.
# Usage: scripts/battletest.sh balloon --quick
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -p nebula-cli -p nebulad -p nebula-battletest
if [ "$(uname)" = "Darwin" ]; then
    scripts/sign-dev.sh target/debug/nebula target/debug/nebulad
fi
exec env NEBULA_BATTLETEST=1 target/debug/nebula-battletest "$@"
