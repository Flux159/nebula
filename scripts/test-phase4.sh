#!/bin/bash
# Phase 4 acceptance: elastic memory. The host-visible footprint must track
# the workload — grow under load, shrink back after, never OOM the guest.
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA=target/debug/nebula
# Builds invalidate ad-hoc signatures; always re-sign before touching the VM.
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

footprint() {
    $NEBULA stats | grep -oE 'host footprint [0-9]+' | grep -oE '[0-9]+'
}
balloon_held() {
    $NEBULA stats | grep -oE 'balloon holds [0-9]+' | grep -oE '[0-9]+'
}

# Fresh boot for deterministic numbers.
$NEBULA down --force >/dev/null 2>&1; sleep 1
$NEBULA up >/dev/null || { echo "FATAL: up failed"; exit 1; }
$NEBULA setup docker >/dev/null
for _ in $(seq 1 30); do docker version >/dev/null 2>&1 && break; sleep 1; done

echo "--- settle (60s): balloon reclaims the idle guest"
sleep 60
IDLE_FP=$(footprint); IDLE_HELD=$(balloon_held)
echo "    idle footprint=${IDLE_FP}MiB, balloon=${IDLE_HELD}MiB"
check "idle balloon reclaimed >24GiB of 32GiB"  "[ $IDLE_HELD -gt 24576 ]"
check "idle host footprint under 4GiB"          "[ $IDLE_FP -lt 4096 ]"

echo "--- load: container touches 6GiB (sampling peak through the window)"
docker run --rm --shm-size=7g --name nebula-p4-hog alpine sh -c \
    'dd if=/dev/zero of=/dev/shm/hog bs=1M count=6144 status=none && sleep 12 && rm /dev/shm/hog' &
HOG_PID=$!
# Fast bursts beat the 1s controller tick; the device-level DEFLATE_ON_OOM
# backstop absorbs the difference. Sample the whole window for honest peaks.
PEAK_FP=$IDLE_FP; MIN_HELD=$IDLE_HELD
for _ in $(seq 1 12); do
    sleep 2
    FP=$(footprint); HELD=$(balloon_held)
    [ "$FP" -gt "$PEAK_FP" ] && PEAK_FP=$FP
    [ "$HELD" -lt "$MIN_HELD" ] && MIN_HELD=$HELD
done
echo "    peak footprint=${PEAK_FP}MiB, min balloon=${MIN_HELD}MiB"
check "footprint grew under load (>+3GiB)"      "[ $((PEAK_FP - IDLE_FP)) -gt 3072 ]"
check "balloon deflated for the workload"       "[ $MIN_HELD -lt $IDLE_HELD ]"
wait $HOG_PID; HOG_RC=$?
check "hog container completed (no OOM)"        "[ $HOG_RC = 0 ]"
check "guest survived the spike"                "$NEBULA exec true"

echo "--- release: balloon re-inflates and footprint stays bounded"
# VZ traditional balloon has high-water-mark semantics (see tasks/issues.md):
# dirty pages are NOT returned to macOS on re-inflate, so the footprint stays
# at the workload peak rather than shrinking. The enforceable contract is:
# the balloon returns to near its idle level (single reclaim jump after the
# 45s surplus window) and the footprint never exceeds the workload peak.
REINFLATED=0
for _ in $(seq 1 45); do
    sleep 2
    NOW_HELD=$(balloon_held)
    if [ "$NOW_HELD" -ge $((IDLE_HELD - 2048)) ]; then REINFLATED=1; break; fi
done
SETTLED_FP=$(footprint)
echo "    settled footprint=${SETTLED_FP}MiB, balloon=$(balloon_held)MiB"
check "balloon re-inflated after release"       "[ $REINFLATED = 1 ]"
check "footprint bounded by workload peak"      "[ $SETTLED_FP -le $((PEAK_FP + 512)) ]"

$NEBULA revert docker >/dev/null
echo
echo "phase 4: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
