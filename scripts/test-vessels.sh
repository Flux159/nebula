#!/bin/bash
# Vessels acceptance: named microVMs on both backends, disk snapshots, and
# live memory-state snapshots (vz: pause -> save -> clone -> resume).
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA="$PWD/target/debug/nebula"
cargo build -p nebula-cli -p nebulad >/dev/null 2>&1
scripts/sign-dev.sh target/debug/nebula target/debug/nebulad >/dev/null

# vz (memory snapshots) is macOS-only; volumes/network/branch checks ride a
# vz vessel on macOS and a krun vessel on Linux.
if [ "$(uname)" = "Darwin" ]; then V2="tv-vz"; V2_ARGS="--backend vz"; HAS_VZ=1
else V2="tv-vz"; V2_ARGS=""; HAS_VZ=0; fi

PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}

# Fresh slate (ignore failures: vessels may not exist).
for v in tv-krun tv-vz tv-fork-1 tv-fork-2; do
    "$NEBULA" vessels rm "$v" --force >/dev/null 2>&1
done

echo "--- both backends boot a named vessel"
check "krun vessel boots"            "$NEBULA vessels new tv-krun --mem 1024 && $NEBULA vessels exec tv-krun -- uname -r"
check "2nd vessel boots (+volume)"   "$NEBULA vessels new $V2 $V2_ARGS --mem 1024 --volume vtest:1 && $NEBULA vessels exec $V2 -- uname -r"
[ "$HAS_VZ" = 1 ] && check "vz backend recorded" "out=\$($NEBULA vessels info $V2); echo \"\$out\" | grep -q 'backend:  vz'"
check "vessel outbound network"      "$NEBULA vessels exec $V2 -- wget -q -T 15 -O /dev/null http://example.com"
check "extra volume mounted ext4"    "$NEBULA vessels exec $V2 -- sh -c 'mount | grep -q \"/mnt/vtest type ext4\"'"
check "volume sized right (-b 4096)" "$NEBULA vessels exec $V2 -- sh -c 'df -m /mnt/vtest | tail -1 | awk \"{exit (\\\$2 > 900 && \\\$2 < 1100) ? 0 : 1}\"'"

echo "--- snapshot defaults: memory+disk on vz, graceful disk-only elsewhere"
check "krun snapshot (auto->disk)"   "out=\$($NEBULA vessels snapshot tv-krun base); echo \"\$out\" | grep -q 'disk-only'"
check "vz --no-memory is disk-only"  "$NEBULA vessels snapshot $V2 base --no-memory && { out=\$($NEBULA vessels snapshots $V2); echo \"\$out\" | grep 'base' | grep -vq memory; }"

if [ "$HAS_VZ" = 1 ]; then
echo "--- live memory snapshot (vz, the default)"
# RAM-only witnesses: a tmpfs file and a background process. Disk snapshots
# cannot preserve either; only a true memory-state snapshot can.
check "plant RAM-only state"         "$NEBULA vessels exec tv-vz -- sh -c 'echo golden > /tmp/witness; echo vol-golden > /mnt/vtest/witness; nohup sleep 86400 >/dev/null 2>&1 & sleep 0.2; pgrep -x sleep'"
check "default snapshot is memory"   "out=\$($NEBULA vessels snapshot tv-vz cp); echo \"\$out\" | grep -q 'memory snapshot' && $NEBULA vessels exec tv-vz -- true"
check "snapshot listed with memory"  "out=\$($NEBULA vessels snapshots tv-vz); echo \"\$out\" | grep -q 'cp.*memory'"
check "corrupt RAM state"            "$NEBULA vessels exec tv-vz -- sh -c 'echo corrupted > /tmp/witness; echo vol-corrupted > /mnt/vtest/witness; pkill -x sleep; true'"
check "restore resumes mid-exec"     "out=\$($NEBULA vessels restore tv-vz cp); echo \"\$out\" | grep -q 'live resume'"
check "tmpfs witness restored"       "out=\$($NEBULA vessels exec tv-vz -- cat /tmp/witness); echo \"\$out\" | grep -q golden"
check "volume witness restored"      "out=\$($NEBULA vessels exec tv-vz -- cat /mnt/vtest/witness); echo \"\$out\" | grep -q vol-golden"
check "killed process is alive"      "$NEBULA vessels exec tv-vz -- pgrep -x sleep"

echo "--- live branch fan-out from a memory snapshot"
check "branch 2 live clones"         "out=\$($NEBULA vessels branch tv-vz tv-fork --snapshot cp --count 2); echo \"\$out\" | grep -q 'live resume'"
check "fork-1 woke mid-execution"    "$NEBULA vessels exec tv-fork-1 -- sh -c 'cat /tmp/witness | grep -q golden && pgrep -x sleep'"
check "fork-2 woke mid-execution"    "$NEBULA vessels exec tv-fork-2 -- sh -c 'cat /tmp/witness | grep -q golden && pgrep -x sleep'"
check "forks are independent"        "$NEBULA vessels exec tv-fork-1 -- sh -c 'echo diverged > /tmp/witness' && out=\$($NEBULA vessels exec tv-fork-2 -- cat /tmp/witness); echo \"\$out\" | grep -q golden"

else
echo "--- disk snapshot round-trip (krun)"
check "plant disk state"             "$NEBULA vessels exec $V2 -- sh -c 'echo golden > /mnt/vtest/witness'"
check "snapshot (auto->disk)"        "out=\$($NEBULA vessels snapshot $V2 cp); echo \"\$out\" | grep -q 'snapshot'"
check "corrupt disk state"           "$NEBULA vessels exec $V2 -- sh -c 'echo corrupted > /mnt/vtest/witness'"
check "restore round-trip"           "$NEBULA vessels restore $V2 cp && out=\$($NEBULA vessels exec $V2 -- cat /mnt/vtest/witness); echo \"\$out\" | grep -q golden"
check "branch 2 from snapshot"       "$NEBULA vessels branch $V2 tv-fork --snapshot cp --count 2 && $NEBULA vessels exec tv-fork-1 -- true && $NEBULA vessels exec tv-fork-2 -- true"
check "forks independent"            "$NEBULA vessels exec tv-fork-1 -- sh -c 'echo diverged > /mnt/vtest/witness' && out=\$($NEBULA vessels exec tv-fork-2 -- cat /mnt/vtest/witness); echo \"\$out\" | grep -q golden"
fi

echo "--- guardrails"
# capture-then-grep: pipefail + an intentionally-failing left side would
# otherwise fail the pipeline even when grep matches.
check "--memory rejects krun vessel" "out=\$($NEBULA vessels snapshot tv-krun mem --memory 2>&1); echo \"\$out\" | grep -q 'need a vz vessel'"
check "--memory rejects stopped vm"  "[ \"$HAS_VZ\" = 0 ] || { $NEBULA vessels stop $V2 && out=\$($NEBULA vessels snapshot $V2 mem2 --memory 2>&1); echo \"\$out\" | grep -q 'not running'; }"
check "auto on stopped vz -> disk"   "$NEBULA vessels stop $V2 >/dev/null 2>&1; out=\$($NEBULA vessels snapshot $V2 coldsnap); echo \"\$out\" | grep -q 'disk-only'"
check "stopped vz cold-boots again"  "$NEBULA vessels start $V2 && $NEBULA vessels exec $V2 -- true"

for v in tv-krun tv-vz tv-fork-1 tv-fork-2; do
    "$NEBULA" vessels rm "$v" --force >/dev/null 2>&1
done

echo
echo "vessels: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
