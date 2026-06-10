#!/bin/bash
# Phase 9 acceptance: Tauri UI builds and runs against the engine API.
# (Visual checks are manual; this verifies build, frontend sanity, process
# liveness, and that the API endpoints the UI consumes respond.)
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA="$PWD/target/debug/nebula"
cargo build -p nebula-cli -p nebulad >/dev/null 2>&1
scripts/sign-dev.sh target/debug/nebula target/debug/nebulad >/dev/null
PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}

echo "--- build"
(cd ui/src-tauri && cargo build >/dev/null 2>&1)
check "tauri app builds"        "[ -f ui/src-tauri/target/debug/nebula-ui ]"
check "frontend js parses"      "node --check ui/frontend/main.js"
check "frontend html present"   "grep -q 'Nebula' ui/frontend/index.html"

echo "--- engine API the UI consumes"
$NEBULA up >/dev/null 2>&1
for _ in $(seq 1 30); do curl -fsS http://127.0.0.1:7440/healthz >/dev/null 2>&1 && break; sleep 1; done
check "status endpoint"         "curl -fsS http://127.0.0.1:7440/v1alpha1/status"
check "stats endpoint"          "curl -fsS http://127.0.0.1:7440/v1alpha1/stats"
check "containers endpoint"     "curl -fsS http://127.0.0.1:7440/v1alpha1/containers"

echo "--- app process smoke (launches a window briefly)"
ui/src-tauri/target/debug/nebula-ui >/dev/null 2>&1 &
UI_PID=$!
sleep 4
ALIVE=0
kill -0 $UI_PID 2>/dev/null && ALIVE=1
kill $UI_PID 2>/dev/null; wait $UI_PID 2>/dev/null
check "app stays alive 4s"      "[ $ALIVE = 1 ]"

echo
echo "phase 9: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
