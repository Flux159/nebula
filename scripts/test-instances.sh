#!/bin/bash
# Multi-instance acceptance (issues #22, #23): port collisions must fail
# loudly before the VM boots, and every exit must say why.
#
# Runs entirely in throwaway NEBULA_HOMEs under /private/tmp — it never
# touches ~/.nebula, so it is safe to run while your own engine is up. The
# guest images are cloned (APFS clonefile) from ~/.nebula, so that install
# has to be current.
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA=target/debug/nebula
NEBULAD=target/debug/nebulad
# Short paths on purpose: a unix socket path over ~104 bytes fails with
# "path must be shorter than SUN_LEN", and $TMPDIR is already long.
A=/private/tmp/nebula-inst-a
B=/private/tmp/nebula-inst-b

cargo build -p nebula-cli -p nebulad >/dev/null 2>&1 || { echo "FATAL: build failed"; exit 1; }
# Builds invalidate ad-hoc signatures; VZ refuses to run unsigned.
scripts/sign-dev.sh $NEBULA $NEBULAD >/dev/null

PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}
cleanup() {
    for home in "$A" "$B"; do
        [ -S "$home/run/nebulad.sock" ] && NEBULA_HOME="$home" $NEBULA down --force >/dev/null 2>&1
        [ -f "$home/run/nebulad.pid" ] && kill -9 "$(cat "$home/run/nebulad.pid")" 2>/dev/null
    done
    rm -rf "$A" "$B"
}
trap cleanup EXIT

for img in ~/.nebula/kernel/Image ~/.nebula/disks/rootfs.img; do
    [ -f "$img" ] || { echo "FATAL: $img missing — run \`nebula install-image\` first"; exit 1; }
done

seed_home() {  # seed_home <dir> <api> <dns> <k8s> <zone> [extra config lines]
    rm -rf "$1"; mkdir -p "$1/kernel" "$1/disks"
    cp -c ~/.nebula/kernel/Image "$1/kernel/Image"
    cp -c ~/.nebula/disks/rootfs.img "$1/disks/rootfs.img"
    cat > "$1/config.toml" <<EOF
api_port = $2
dns_port = $3
k8s_port = $4
dns_zone = "$5"
max_ram_mib = 2048
cpus = 2
data_disk_gib = 8
${6:-}
EOF
}
# Never pipe the CLI into `grep -q` under `pipefail`: grep closes early, the
# CLI takes SIGPIPE, and the pipeline "fails" on a daemon that is perfectly
# healthy. Capture, then grep the file.
running() {  # running <home>
    NEBULA_HOME="$1" $NEBULA status > /tmp/inst-running.txt 2>&1
    grep -q "nebula: running" /tmp/inst-running.txt
}
wait_up() {  # wait_up <home> <seconds>
    for _ in $(seq 1 "$(( $2 * 2 ))"); do
        if [ -S "$1/run/nebulad.sock" ] && running "$1"; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}

echo "--- instance A comes up on its own ports"
seed_home "$A" 7541 42153 6543 a.local
NEBULA_HOME="$A" nohup $NEBULAD >/dev/null 2>&1 &
disown 2>/dev/null
check "A boots"                        "wait_up $A 60"
NEBULA_HOME="$A" $NEBULA status > /tmp/inst-a-status.txt 2>&1
check "status reports effective ports" "grep -q 'ports:.*api 127.0.0.1:7541.*dns udp 42153.*k8s 6543' /tmp/inst-a-status.txt"
check "status reports listeners bound" "grep -qE 'listeners: [0-9]+ bound, all healthy' /tmp/inst-a-status.txt"

echo "--- #22: instance B with A's ports must refuse, not half-start"
seed_home "$B" 7541 42153 6543 b.local
NEBULA_HOME="$B" timeout 90 $NEBULA up > /tmp/inst-b-up.txt 2>&1
UP_RC=$?
check "up fails"                       "[ $UP_RC -ne 0 ]"
check "up names the port"              "grep -q '7541 (api_port) is already in use' /tmp/inst-b-up.txt"
check "up names the other instance"    "grep -q \"NEBULA_HOME=$A\" /tmp/inst-b-up.txt"
check "up suggests free ports"         "grep -qE '^ +api_port = [0-9]+' /tmp/inst-b-up.txt"
check "B did not boot a VM"            "! [ -S $B/run/nebulad.sock ]"
check "A is untouched"                 "running $A"

echo "--- #22: port_conflict = auto moves off the taken ports"
seed_home "$B" 7541 42153 6543 b.local 'port_conflict = "auto"'
NEBULA_HOME="$B" nohup $NEBULAD >/dev/null 2>&1 &
disown 2>/dev/null
check "B boots on free ports"          "wait_up $B 60"
NEBULA_HOME="$B" $NEBULA status > /tmp/inst-b-status.txt 2>&1
check "auto choice is logged"          "grep -q 'picked a free one' $B/logs/nebulad.log"
check "status shows the chosen port"   "! grep -q 'api 127.0.0.1:7541' /tmp/inst-b-status.txt"
NEBULA_HOME="$B" $NEBULA down >/dev/null 2>&1

echo "--- #22: a shared dns_zone is called out"
seed_home "$B" 7551 42163 6553 a.local
NEBULA_HOME="$B" nohup $NEBULAD >/dev/null 2>&1 &
disown 2>/dev/null
check "B boots"                        "wait_up $B 60"
check "shared dns_zone warned"         "grep -q 'same dns_zone' $B/logs/nebulad.log"
NEBULA_HOME="$B" $NEBULA down >/dev/null 2>&1

echo "--- #23: every exit says why"
check "down logged its reason"         "grep -q 'nebulad shutting down.*reason=\"down\"' $B/logs/nebulad.log"
check "down recorded uptime"           "grep -q 'reason=\"down\".*uptime_secs=' $B/logs/nebulad.log"
check "exit stamped in instance.json"  "grep -q '\"reason\": \"down\"' $B/run/instance.json"

APID=$(cat "$A/run/nebulad.pid")
kill -TERM "$APID"
for _ in $(seq 1 60); do kill -0 "$APID" 2>/dev/null || break; sleep 0.5; done
check "A exited on SIGTERM"            "! kill -0 $APID 2>/dev/null"
check "SIGTERM logged by name"         "grep -q 'reason=\"signal\" detail=\"SIGTERM\"' $A/logs/nebulad.log"
check "control socket cleaned up"      "! [ -e $A/run/nebulad.sock ]"

echo "--- #23: the next start reports how the last one ended"
NEBULA_HOME="$A" nohup $NEBULAD >/dev/null 2>&1 &
disown 2>/dev/null
check "A restarts"                     "wait_up $A 60"
check "clean previous exit reported"   "grep -q 'previous run exited cleanly.*reason=signal' $A/logs/nebulad.log"

APID=$(cat "$A/run/nebulad.pid")
kill -9 "$APID"; sleep 1
NEBULA_HOME="$A" nohup $NEBULAD >/dev/null 2>&1 &
disown 2>/dev/null
check "A restarts after kill -9"       "wait_up $A 60"
check "unclean previous exit reported" "grep -q 'previous run did not shut down cleanly' $A/logs/nebulad.log"

echo
echo "instances: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
