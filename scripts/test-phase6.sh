#!/bin/bash
# Phase 6 acceptance: reliability & scale. Crash recovery matrix + a bounded
# scale test (50 containers on CI-class machines; SCALE=200 for the full rig).
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA=target/debug/nebula
SCALE="${SCALE:-50}"
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

$NEBULA down --force >/dev/null 2>&1; sleep 1
$NEBULA up >/dev/null || { echo "FATAL: up failed"; exit 1; }
$NEBULA use docker >/dev/null
for _ in $(seq 1 30); do docker version >/dev/null 2>&1 && break; sleep 1; done

echo "--- crash recovery: kill -9 nebulad"
DPID=$(cat ~/.nebula/run/nebulad.pid)
kill -9 "$DPID"
sleep 2
$NEBULA status > /tmp/p6-stale.txt 2>&1 || true
check "stale state detected"            "! grep -q 'nebula: running' /tmp/p6-stale.txt"
check "up recovers after daemon kill"   "$NEBULA up"
for _ in $(seq 1 30); do docker version >/dev/null 2>&1 && break; sleep 1; done
check "docker works after recovery"     "docker run --rm alpine true"

echo "--- crash recovery: guest agent killed (init must restart it)"
AGENT_PID=$($NEBULA exec sh -c "pidof vessel-agent" | awk '{print $1}')
$NEBULA exec sh -c "kill -9 $AGENT_PID" || true
sleep 2
check "agent restarted by init"         "$NEBULA exec true"
$NEBULA status > /tmp/p6-status.txt 2>&1 || true
check "agent healthy after restart"     "grep -q 'agent:.*healthy' /tmp/p6-status.txt"

echo "--- crash recovery: dockerd killed (init must restart it)"
$NEBULA exec sh -c 'kill -9 $(pidof dockerd)' || true
DOCKER_BACK=0
for _ in $(seq 1 30); do
    docker version >/dev/null 2>&1 && { DOCKER_BACK=1; break; }
    sleep 1
done
check "dockerd restarted by init"       "[ $DOCKER_BACK = 1 ]"

echo "--- scale: $SCALE concurrent containers"
docker rm -f $(docker ps -aq --filter label=nebula-p6) >/dev/null 2>&1 || true
T0=$(date +%s)
STARTED=0
for i in $(seq 1 "$SCALE"); do
    docker run -d --label nebula-p6 --name nebula-p6-$i alpine sleep 600 >/dev/null 2>&1 && STARTED=$((STARTED+1)) &
    # bounded fan-out
    if [ $((i % 10)) = 0 ]; then wait; fi
done
wait
T1=$(date +%s)
RUNNING=$(docker ps -q --filter label=nebula-p6 | wc -l | tr -d ' ')
echo "    started $RUNNING/$SCALE in $((T1-T0))s"
check "all containers running"          "[ $RUNNING = $SCALE ]"
check "engine responsive under load"    "docker run --rm alpine true"
$NEBULA stats > /tmp/p6-stats.txt 2>&1 || true
check "stats respond under load"        "grep -q 'host footprint' /tmp/p6-stats.txt"

echo "--- teardown"
# Re-issue rm each round: parallel force-removes can race dockerd and
# leave stragglers that a single shot never reaps.
REMOVED=0
for _ in $(seq 1 10); do
    LEFT=$(docker ps -aq --filter label=nebula-p6)
    [ -z "$LEFT" ] && { REMOVED=1; break; }
    echo "$LEFT" | xargs docker rm -f >/dev/null 2>&1
    sleep 2
done
check "bulk removal works"              "[ $REMOVED = 1 ]"

$NEBULA revert docker >/dev/null
echo
echo "phase 6: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
