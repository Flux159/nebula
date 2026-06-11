#!/bin/bash
# Tier-B end-to-end: slimd hosts the apiserver-lite + controller bridge; a real
# `kubectl apply` of a Deployment spawns REAL containers on the slim engine.
#
# Runs slimd in a privileged container inside the nebula engine (kube API
# published on 16443 → reachable from the host), drives it with real kubectl.
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
PAUSE="${PAUSE:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/pause}"
STAGE="$HOME/.slim-bridge-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"; cp "$PAUSE" "$STAGE/pause"
cat > "$STAGE/dep.yaml" <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: default}
spec:
  replicas: 2
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec: {containers: [{name: web, image: alpine:3.19, command: ["sh","-c","echo up; sleep 600"]}]}
YAML

docker rm -f slim-bridge >/dev/null 2>&1
docker run -d --privileged -e SLIM_REGISTRY_MIRROR -p 16443:6443 -v "$STAGE:/slim" --name slim-bridge alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-bridge >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:16443/version >/dev/null 2>&1 && break; sleep 1; done

# The bridge apiserver serves TLS (in-cluster operators always speak HTTPS);
# skip verification and pass a dummy token (kubectl needs a user to not prompt).
export KUBECONFIG="$STAGE/kubeconfig"
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:16443", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: t}}]\n' > "$KUBECONFIG"
KC="kubectl --cache-dir=$STAGE/kc"
psnames(){ docker exec slim-bridge sh -c '/slim/docker-slim ps --format "{{.Names}}"' 2>/dev/null | tr -d '"/[]' | sort; }

curl -sk https://localhost:16443/version | grep -q gitVersion && ok "apiserver reachable" || bad apiserver "down"
O=$($KC apply -f "$STAGE/dep.yaml" 2>&1); echo "$O" | grep -q "deployment.apps/web created" && ok "kubectl apply deployment (no flags)" || bad apply "$O"
sleep 5
N=$(psnames | grep -cE "default_web-[0-9]+$"); [ "$N" -eq 2 ] && ok "deployment spawned 2 real containers" || bad spawn "got $N: $(psnames)"
O=$($KC get pods 2>&1); echo "$O" | grep -q "web-0" && ok "kubectl get pods shows synthesized pods" || bad getpods "$O"
# Phase 1: containerStatuses → real kubectl READY column + per-container fields.
O=$($KC get pod web-0 -o jsonpath='{.status.containerStatuses[0].ready}' 2>&1); [ "$O" = "true" ] && ok "containerStatus ready=true" || bad cs-ready "$O"
O=$($KC get pod web-0 -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>&1); [ "$O" = "0" ] && ok "containerStatus restartCount present" || bad cs-restarts "$O"
O=$($KC get pod web-0 --no-headers 2>&1); echo "$O" | grep -qE "web-0 +1/1 +Running" && ok "kubectl READY column 1/1" || bad ready-col "$O"
O=$($KC get pod web-0 -o jsonpath='{.status.containerStatuses[0].state.running.startedAt}' 2>&1); [ -n "$O" ] && ok "containerStatus state.running.startedAt" || bad cs-started "$O"
O=$($KC scale deployment/web --replicas=3 2>&1); echo "$O" | grep -q scaled && ok "kubectl scale (clean)" || bad scale "$O"
sleep 5
N=$(psnames | grep -cE "default_web-[0-9]+$"); [ "$N" -eq 3 ] && ok "scaled up to 3 containers" || bad scaleup "got $N"
$KC scale deployment/web --replicas=1 >/dev/null 2>&1; sleep 5
N=$(psnames | grep -cE "default_web-[0-9]+$"); [ "$N" -eq 1 ] && ok "scaled down to 1 container" || bad scaledown "got $N"
$KC delete deployment web >/dev/null 2>&1; sleep 5
N=$(psnames | grep -cE "default_web-[0-9]+$"); [ "$N" -eq 0 ] && ok "delete removed all containers" || bad delete "got $N"
O=$($KC get pods 2>&1); echo "$O" | grep -qi "no resources" && ok "pods cleaned up" || bad podsgone "$O"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
