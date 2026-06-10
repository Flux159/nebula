#!/bin/bash
# Phase 7 acceptance: libkrun sidecar engine — ephemeral isolated microVMs.
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA="$PWD/target/debug/nebula"
# Builds invalidate ad-hoc signatures; always re-sign before touching VMs.
cargo build -p nebula-cli -p nebulad >/dev/null 2>&1
cargo build -p vessel-init --release --target aarch64-unknown-linux-musl >/dev/null 2>&1
scripts/sign-dev.sh target/debug/nebula target/debug/nebulad >/dev/null

export NEBULA_LIBKRUN_PATH="${NEBULA_LIBKRUN_PATH:-$PWD/third_party/libkrun/target/release/libkrun.1.18.0.dylib}"

PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}

echo "--- basic sandbox lifecycle"
check "uname in sandbox"            "$NEBULA sandbox run -- uname -a | grep -q 'Linux.*aarch64'"
check "exit code propagates"        "$NEBULA sandbox run -- sh -c 'exit 3'; [ \$? = 3 ]"
check "stdout captured"             "$NEBULA sandbox run -- echo sandbox-stdout-marker | grep -q sandbox-stdout-marker"

echo "--- boot speed (ms-class)"
T0=$(python3 -c 'import time; print(int(time.time()*1000))')
$NEBULA sandbox run -- true >/dev/null 2>&1
T1=$(python3 -c 'import time; print(int(time.time()*1000))')
echo "    boot->run->teardown: $((T1-T0))ms"
check "full cycle under 3s"         "[ $((T1-T0)) -lt 3000 ]"

echo "--- isolation"
check "sandbox sees own hostname"   "! $NEBULA sandbox run -- cat /proc/sys/kernel/hostname | grep -q nebula"
check "no vessel data visible"      "$NEBULA sandbox run -- ls / | grep -vq var/lib/nebula"

echo "--- cwd sharing (opt-in virtiofs)"
TMPD=$(mktemp -d) && echo "p7-marker-$$" > "$TMPD/f.txt"
check "shared cwd readable"         "(cd $TMPD && $NEBULA sandbox run --share-cwd -- cat /workdir/f.txt | grep -q p7-marker)"
check "sandbox write reaches host"  "(cd $TMPD && $NEBULA sandbox run --share-cwd -- sh -c 'echo from-sbx > /workdir/out.txt') && grep -q from-sbx $TMPD/out.txt"
rm -rf "$TMPD"

echo "--- concurrency (3 sidecars at once)"
OK=0
for i in 1 2 3; do
    $NEBULA sandbox run -- sh -c "echo sbx-$i" > /tmp/p7-sbx-$i.out 2>/dev/null &
done
wait
for i in 1 2 3; do grep -q "sbx-$i" /tmp/p7-sbx-$i.out && OK=$((OK+1)); done
check "3 concurrent sandboxes"      "[ $OK = 3 ]"

echo "--- coexistence with the Vessel"
$NEBULA up >/dev/null 2>&1
check "vessel healthy"              "$NEBULA exec true"
check "sandbox runs alongside"      "$NEBULA sandbox run -- uname -m | grep -q aarch64"
check "vessel still healthy"        "$NEBULA exec true"

echo
echo "phase 7: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
