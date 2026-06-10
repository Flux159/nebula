#!/bin/bash
# Phase 8 acceptance: GPU sidecars (virtio-gpu Venus device level).
# Userspace Vulkan (mesa-venus + vulkaninfo + llama.cpp benchmark) needs the
# GPU guest image — tracked in tasks/issues.md as Phase 8 follow-up.
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA="$PWD/target/debug/nebula"
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

echo "--- GPU device attachment (libkrun fork, GPU=1 build)"
check "fork exports gpu symbol"     "nm -gU \$NEBULA_LIBKRUN_PATH | grep -q krun_set_gpu_options"
check "--gpu exposes drm card"      "$NEBULA sandbox run --gpu -- ls /sys/class/drm/ | grep -q card0"
check "--gpu exposes render node"   "$NEBULA sandbox run --gpu -- ls /dev/dri/ | grep -q renderD128"
check "virtio-gpu driver bound"     "$NEBULA sandbox run --gpu -- cat /sys/class/drm/card0/device/uevent | grep -qi virtio"

echo "--- isolation: no GPU without the flag"
check "no drm card without --gpu"   "! $NEBULA sandbox run -- ls /sys/class/drm/ | grep -q card0"

echo "--- GPU sandbox basics still work"
check "exit code with --gpu"        "$NEBULA sandbox run --gpu -- sh -c 'exit 4'; [ \$? = 4 ]"
check "vessel unaffected"           "$NEBULA exec true || $NEBULA up >/dev/null && $NEBULA exec true"

echo
echo "phase 8: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
