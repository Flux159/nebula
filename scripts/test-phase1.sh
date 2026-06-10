#!/bin/bash
# Phase 1 acceptance: Vessel lifecycle, agent control plane, shell, doctor.
# Requires guest images built (vessel/build-kernel.sh + vessel/build-rootfs.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

NEBULA=target/debug/nebula
PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}

cargo build -p nebula-cli -p nebulad >/dev/null 2>&1
cargo build -p vessel-init -p vessel-agent --release --target aarch64-unknown-linux-musl >/dev/null 2>&1
scripts/sign-dev.sh $NEBULA target/debug/nebulad >/dev/null

# Clean slate.
$NEBULA down --force >/dev/null 2>&1 || true
sleep 1

echo "--- nebula up (cold boot)"
T0=$(date +%s)
$NEBULA up
T1=$(date +%s)
check "cold boot under 10s" "[ $((T1-T0)) -lt 10 ]"

echo "--- status & agent"
$NEBULA status > /tmp/status-debug.txt 2>&1 || echo "status exit=$?" >> /tmp/status-debug.txt
check "status shows running"           "grep -q 'nebula: running' /tmp/status-debug.txt"
check "agent healthy"                  "$NEBULA status | grep -q 'agent:.*healthy'"
check "exec uname"                     "$NEBULA exec uname -a | grep -q 'Linux nebula'"
check "custom kernel"                  "$NEBULA exec uname -r | grep -q '6.12'"
check "data disk mounted"              "$NEBULA exec df /var/lib/nebula | grep -q vdb"
check "psi available"                  "$NEBULA exec cat /proc/pressure/memory | grep -q avg10"
check "exec exit code propagates"      "! $NEBULA exec sh -c 'exit 3'"

echo "--- shell (scripted pty)"
printf 'echo SHELL_OK_$((6*7))\nexit\n' | script -q /tmp/nebula-shell-test.log $NEBULA shell >/dev/null 2>&1 || true
check "interactive shell round-trip"   "grep -aq SHELL_OK_42 /tmp/nebula-shell-test.log"

echo "--- doctor"
check "doctor passes"                  "$NEBULA doctor"

echo "--- restart cycle"
$NEBULA down
check "down stops daemon"              "! $NEBULA status | grep -q 'nebula: running'"
$NEBULA up >/dev/null
check "second boot healthy"            "$NEBULA status | grep -q 'agent:.*healthy'"
check "data persists across restarts"  "$NEBULA exec ls /var/lib/nebula"
$NEBULA down >/dev/null

echo
echo "phase 1: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
