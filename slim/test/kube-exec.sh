#!/bin/bash
# Validate real `kubectl logs` and `kubectl exec` served by the apiserver from
# the in-process engine (log subresource + WebSocket exec). Needs kubectl ≥1.31
# for WebSocket exec (older kubectl uses SPDY, which slim doesn't serve).
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
PAUSE="${PAUSE:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/pause}"
KUBECTL="${KUBECTL:-/opt/homebrew/bin/kubectl}"; command -v "$KUBECTL" >/dev/null || KUBECTL=kubectl
STAGE="$HOME/.slim-exec-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"; cp "$PAUSE" "$STAGE/pause"
cat > "$STAGE/dep.yaml" <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: app, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: app}}
  template:
    metadata: {labels: {app: app}}
    spec: {containers: [{name: app, image: alpine:3.19, command: ["sh","-c","echo BOOT-LOG; sleep 600"]}]}
YAML
docker rm -f slim-exec >/dev/null 2>&1
docker run -d --privileged -e SLIM_REGISTRY_MIRROR -p 18443:6443 -v "$STAGE:/slim" --name slim-exec alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-exec >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:18443/version >/dev/null 2>&1 && break; sleep 1; done

export KUBECONFIG="$STAGE/kubeconfig"
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:18443", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: t}}]\n' > "$KUBECONFIG"
KC="$KUBECTL --cache-dir=$STAGE/kc"
$KC apply -f "$STAGE/dep.yaml" >/dev/null 2>&1
for i in $(seq 1 30); do $KC get pod app-0 >/dev/null 2>&1 && break; sleep 1; done
sleep 3

O=$($KC logs app-0 2>&1); echo "$O" | grep -q BOOT-LOG && ok "kubectl logs (log subresource)" || bad logs "$O"
O=$($KC exec app-0 -- echo EXEC-OK 2>&1); echo "$O" | grep -q EXEC-OK && ok "kubectl exec output (WebSocket)" || bad exec "$O"
O=$($KC exec app-0 -- sh -c 'echo a; echo b' 2>&1); [ "$(echo "$O" | grep -c '^[ab]$')" = 2 ] && ok "kubectl exec multi-line" || bad exec-multi "$O"
$KC exec app-0 -- sh -c 'exit 7' >/dev/null 2>&1; [ $? -eq 7 ] && ok "kubectl exec exit code propagates" || bad exec-exit "got $?"
$KC exec app-0 -- true >/dev/null 2>&1; [ $? -eq 0 ] && ok "kubectl exec exit 0" || bad exec-zero "got $?"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
