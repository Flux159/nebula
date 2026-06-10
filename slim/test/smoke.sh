#!/bin/sh
# Runs INSIDE a privileged alpine container in the nebula engine microVM.
# Boots slimd, drives it with docker-slim. Exercises the slim stack on real
# Linux (namespaces/overlayfs/cgroup2) inside nebula's VM on macOS.
set -u
# slim's layer store must NOT live on the container's own overlayfs (overlay
# upperdir on overlay = EINVAL). Mount a tmpfs for it. In a real slim vessel
# this is the ext4 data disk, so the constraint doesn't apply there.
mkdir -p /var/lib/nebula
mount -t tmpfs tmpfs /var/lib/nebula 2>/dev/null
export SLIM_DATA=/var/lib/nebula/slim
export SLIM_RUN_DIR=/var/lib/nebula/run
export SLIM_SOCKET=/var/run/docker.sock
export DOCKER_HOST=unix:///var/run/docker.sock
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
DS=/slim/docker-slim

echo "== booting slimd =="
/slim/slimd > /tmp/slimd.log 2>&1 &
SLIMD_PID=$!
# wait for socket
for i in $(seq 1 50); do [ -S "$SLIM_SOCKET" ] && break; sleep 0.1; done
[ -S "$SLIM_SOCKET" ] && ok "slimd socket up" || { bad "slimd socket"; cat /tmp/slimd.log; exit 1; }

echo "== version/info =="
$DS version >/tmp/o 2>&1 && grep -q "nebula-slim" /tmp/o && ok "version" || { bad "version"; cat /tmp/o; }
$DS info >/tmp/o 2>&1 && grep -q "overlay2" /tmp/o && ok "info" || { bad "info"; cat /tmp/o; }

echo "== pull alpine =="
$DS pull alpine:3.19 >/tmp/o 2>&1 && ok "pull alpine:3.19" || { bad "pull"; cat /tmp/o; tail /tmp/slimd.log; }
$DS images >/tmp/o 2>&1 && grep -q "alpine" /tmp/o && ok "images lists alpine" || { bad "images"; cat /tmp/o; }

echo "== run: echo =="
$DS run --rm alpine:3.19 echo hello-from-slim >/tmp/o 2>&1
grep -q "hello-from-slim" /tmp/o && ok "run echo" || { bad "run echo"; cat /tmp/o; tail -20 /tmp/slimd.log; }

echo "== run: exit code 7 =="
$DS run --rm alpine:3.19 sh -c 'exit 7' >/tmp/o 2>&1; [ $? -eq 7 ] && ok "exit code 7" || bad "exit code (got $?)"

echo "== run -d + logs + exec + stop + rm =="
$DS run -d --name s1 alpine:3.19 sh -c 'echo started; sleep 30' >/tmp/o 2>&1 && ok "run -d" || { bad "run -d"; cat /tmp/o; tail -20 /tmp/slimd.log; }
sleep 1
$DS ps >/tmp/o 2>&1 && grep -q "s1" /tmp/o && ok "ps shows s1" || { bad "ps"; cat /tmp/o; }
$DS logs s1 >/tmp/o 2>&1 && grep -q "started" /tmp/o && ok "logs" || { bad "logs"; cat /tmp/o; }
$DS exec s1 echo from-exec >/tmp/o 2>&1 && grep -q "from-exec" /tmp/o && ok "exec" || { bad "exec"; cat /tmp/o; tail -20 /tmp/slimd.log; }
$DS stop s1 >/tmp/o 2>&1 && ok "stop" || { bad "stop"; cat /tmp/o; }
$DS rm s1 >/tmp/o 2>&1 && ok "rm" || { bad "rm"; cat /tmp/o; }

echo "== inspect -f =="
$DS run -d --name s2 alpine:3.19 sleep 30 >/dev/null 2>&1
RUNNING=$($DS inspect -f '{{.State.Running}}' s2 2>/dev/null)
[ "$RUNNING" = "true" ] && ok "inspect -f State.Running" || bad "inspect -f (got '$RUNNING')"
$DS rm -f s2 >/dev/null 2>&1

echo "== volume + bind =="
$DS volume create v1 >/tmp/o 2>&1 && ok "volume create" || bad "volume create"
$DS run --rm -v v1:/data alpine:3.19 sh -c 'echo persisted > /data/f' >/tmp/o 2>&1
$DS run --rm -v v1:/data alpine:3.19 cat /data/f >/tmp/o 2>&1
grep -q "persisted" /tmp/o && ok "volume persistence" || { bad "volume persistence"; cat /tmp/o; }

echo "== docker build =="
mkdir -p /tmp/bctx
printf 'FROM alpine:3.19\nRUN echo built-in-layer > /built.txt\nCMD cat /built.txt\n' > /tmp/bctx/Dockerfile
$DS build -t slimbuilt:1 /tmp/bctx >/tmp/o 2>&1 && ok "build" || { bad "build"; cat /tmp/o; tail -20 /tmp/slimd.log; }
$DS run --rm slimbuilt:1 >/tmp/o 2>&1
grep -q "built-in-layer" /tmp/o && ok "run built image" || { bad "run built image"; cat /tmp/o; }

echo ""
echo "RESULT: $PASS passed, $FAIL failed"
kill $SLIMD_PID 2>/dev/null
[ $FAIL -eq 0 ]
