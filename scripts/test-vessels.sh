#!/bin/bash
# Vessels acceptance: named microVMs on both backends, disk snapshots, and
# live memory-state snapshots (vz: pause -> save -> clone -> resume).
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

# Fresh slate (ignore failures: vessels may not exist).
for v in tv-krun tv-vz tv-fork-1 tv-fork-2; do
    "$NEBULA" vessels rm "$v" --force >/dev/null 2>&1
done

echo "--- both backends boot a named vessel"
check "krun vessel boots"            "$NEBULA vessels new tv-krun --mem 1024 && $NEBULA vessels exec tv-krun -- uname -r"
check "vz vessel boots"              "$NEBULA vessels new tv-vz --backend vz --mem 1024 && $NEBULA vessels exec tv-vz -- uname -r"
check "vz backend recorded"          "$NEBULA vessels info tv-vz | grep -q 'backend:  vz'"
check "vz outbound network"          "$NEBULA vessels exec tv-vz -- wget -q -T 10 -O /dev/null http://1.1.1.1"

echo "--- disk snapshots still work on both"
check "krun disk snapshot"           "$NEBULA vessels snapshot tv-krun base"
check "vz disk snapshot"             "$NEBULA vessels snapshot tv-vz base"

echo "--- live memory snapshot (vz)"
# RAM-only witnesses: a tmpfs file and a background process. Disk snapshots
# cannot preserve either; only a true memory-state snapshot can.
check "plant RAM-only state"         "$NEBULA vessels exec tv-vz -- sh -c 'echo golden > /tmp/witness; nohup sleep 86400 >/dev/null 2>&1 & sleep 0.2; pgrep -x sleep'"
check "memory snapshot, vm stays up" "$NEBULA vessels snapshot tv-vz cp --memory && $NEBULA vessels exec tv-vz -- true"
check "snapshot listed with memory"  "$NEBULA vessels snapshots tv-vz | grep -q 'cp.*memory'"
check "corrupt RAM state"            "$NEBULA vessels exec tv-vz -- sh -c 'echo corrupted > /tmp/witness; pkill -x sleep; true'"
check "restore resumes mid-exec"     "$NEBULA vessels restore tv-vz cp | grep -q 'live resume'"
check "tmpfs witness restored"       "$NEBULA vessels exec tv-vz -- cat /tmp/witness | grep -q golden"
check "killed process is alive"      "$NEBULA vessels exec tv-vz -- pgrep -x sleep"

echo "--- live branch fan-out from a memory snapshot"
check "branch 2 live clones"         "$NEBULA vessels branch tv-vz tv-fork --snapshot cp --count 2 | grep -q 'live resume'"
check "fork-1 woke mid-execution"    "$NEBULA vessels exec tv-fork-1 -- sh -c 'cat /tmp/witness | grep -q golden && pgrep -x sleep'"
check "fork-2 woke mid-execution"    "$NEBULA vessels exec tv-fork-2 -- sh -c 'cat /tmp/witness | grep -q golden && pgrep -x sleep'"
check "forks are independent"        "$NEBULA vessels exec tv-fork-1 -- sh -c 'echo diverged > /tmp/witness' && $NEBULA vessels exec tv-fork-2 -- cat /tmp/witness | grep -q golden"

echo "--- guardrails"
# capture-then-grep: pipefail + an intentionally-failing left side would
# otherwise fail the pipeline even when grep matches.
check "--memory rejects krun vessel" "out=\$($NEBULA vessels snapshot tv-krun mem --memory 2>&1); echo \"\$out\" | grep -q 'need a vz vessel'"
check "--memory rejects stopped vm"  "$NEBULA vessels stop tv-vz && out=\$($NEBULA vessels snapshot tv-vz mem2 --memory 2>&1); echo \"\$out\" | grep -q 'not running'"
check "stopped vz cold-boots again"  "$NEBULA vessels start tv-vz && $NEBULA vessels exec tv-vz -- true"

for v in tv-krun tv-vz tv-fork-1 tv-fork-2; do
    "$NEBULA" vessels rm "$v" --force >/dev/null 2>&1
done

echo
echo "vessels: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
