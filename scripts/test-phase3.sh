#!/bin/bash
# Phase 3 acceptance: host-resolver DNS for the guest, dynamic port
# forwarding to localhost, *.nebula.local, and the $HOME virtiofs share.
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

$NEBULA down --force >/dev/null 2>&1; sleep 1
$NEBULA up >/dev/null || { echo "FATAL: nebula up failed"; exit 1; }
$NEBULA use docker >/dev/null
for _ in $(seq 1 30); do docker version >/dev/null 2>&1 && break; sleep 1; done

echo "--- guest DNS via host resolver"
check "guest resolv.conf -> local relay"  "$NEBULA exec cat /etc/resolv.conf | grep -q 127.0.0.1"
check "guest resolves public names"       "$NEBULA exec nslookup registry-1.docker.io | grep -qi address"
PULL_OK=0
for _ in $(seq 1 10); do
    docker pull -q alpine:3.20 >/dev/null 2>&1 && { PULL_OK=1; break; }
    sleep 2
done
check "docker pull works over new DNS"    "[ $PULL_OK = 1 ]"

echo "--- dynamic port forwarding"
docker rm -f nebula-p3-web >/dev/null 2>&1
docker run -d --name nebula-p3-web -p 18080:80 nginx:alpine >/dev/null
FWD_OK=0
for _ in $(seq 1 15); do
    if curl -fsS -m 2 http://localhost:18080/ 2>/dev/null | grep -q nginx; then FWD_OK=1; break; fi
    sleep 1
done
check "localhost:18080 reaches nginx"     "[ $FWD_OK = 1 ]"
check "name.nebula.local resolves"        "dig +short -p 42053 @127.0.0.1 nebula-p3-web.nebula.local | grep -qE '^[0-9]+\.'"
check "unknown name is NXDOMAIN"          "dig -p 42053 @127.0.0.1 doesnotexist.nebula.local | grep -q NXDOMAIN"
check "host resolver path via 42053"      "dig +short -p 42053 @127.0.0.1 one.one.one.one | grep -qE '^(1\.1\.1\.1|1\.0\.0\.1)$'"

docker rm -f nebula-p3-web >/dev/null
RELEASED=0
for _ in $(seq 1 10); do
    curl -fsS -m 1 http://localhost:18080/ >/dev/null 2>&1 || { RELEASED=1; break; }
    sleep 1
done
check "port released after rm"            "[ $RELEASED = 1 ]"

echo "--- \$HOME virtiofs share"
MARK="nebula-p3-$$"
TESTDIR="$HOME/.nebula-p3-test"
mkdir -p "$TESTDIR" && echo "$MARK" > "$TESTDIR/host-file"
check "host file visible in guest"        "$NEBULA exec cat $TESTDIR/host-file | grep -q $MARK"
check "bind mount into container"         "docker run --rm -v $TESTDIR:/data alpine cat /data/host-file | grep -q $MARK"
check "guest write visible on host"       "docker run --rm -v $TESTDIR:/data alpine sh -c 'echo from-container > /data/guest-file' && grep -q from-container $TESTDIR/guest-file"
rm -rf "$TESTDIR"

$NEBULA revert docker >/dev/null
echo
echo "phase 3: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
