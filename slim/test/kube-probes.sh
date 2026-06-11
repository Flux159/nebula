#!/bin/bash
# Phase 2: readiness/liveness probes. slimd's bridge runs exec probes; readiness
# gates the pod Ready condition / READY column, liveness failure restarts the
# container. Driven by real kubectl over the TLS apiserver.
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
KUBECTL="${KUBECTL:-/opt/homebrew/bin/kubectl}"; command -v "$KUBECTL" >/dev/null || KUBECTL=kubectl
STAGE="$HOME/.slim-probes-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"

# Readiness: ready only once /tmp/ready exists. Liveness: alive while /tmp/alive
# exists (the container recreates it on (re)start) — removing it forces a restart.
cat > "$STAGE/rdy.yaml" <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: rdy, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: rdy}}
  template:
    metadata: {labels: {app: rdy}}
    spec:
      containers:
        - name: rdy
          image: alpine:3.19
          command: ["sh","-c","sleep 600"]
          readinessProbe:
            exec: {command: ["sh","-c","test -f /tmp/ready"]}
            initialDelaySeconds: 0
            periodSeconds: 1
            failureThreshold: 1
            successThreshold: 1
YAML
cat > "$STAGE/liv.yaml" <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: liv, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: liv}}
  template:
    metadata: {labels: {app: liv}}
    spec:
      containers:
        - name: liv
          image: alpine:3.19
          command: ["sh","-c","touch /tmp/alive; sleep 600"]
          livenessProbe:
            exec: {command: ["sh","-c","test -f /tmp/alive"]}
            initialDelaySeconds: 2
            periodSeconds: 1
            failureThreshold: 1
YAML

docker rm -f slim-probes >/dev/null 2>&1
for i in $(seq 1 20); do docker inspect slim-probes >/dev/null 2>&1 || break; sleep 0.5; done
docker run -d --privileged -p 16444:6443 -v "$STAGE:/slim" --name slim-probes alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-probes >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:16444/version >/dev/null 2>&1 && break; sleep 1; done

export KUBECONFIG="$STAGE/kubeconfig"
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:16444", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: t}}]\n' > "$KUBECONFIG"
KC="$KUBECTL --cache-dir=$STAGE/kc"

# Re-apply each iteration (idempotent upsert) until both pods are synthesized —
# robust against a fresh engine's alpine pull and any transient first-request
# hiccup against the just-booted apiserver.
for i in $(seq 1 90); do
  $KC apply -f "$STAGE/rdy.yaml" >/dev/null 2>&1
  $KC apply -f "$STAGE/liv.yaml" >/dev/null 2>&1
  $KC get pod rdy-0 >/dev/null 2>&1 && $KC get pod liv-0 >/dev/null 2>&1 && break
  sleep 1
done
sleep 4

# --- readiness ---
R=$($KC get pod rdy-0 -o jsonpath='{.status.containerStatuses[0].ready}' 2>&1)
[ "$R" = "false" ] && ok "readiness gates ready=false (probe failing)" || bad rdy-false "$R"
R=$($KC get pod rdy-0 --no-headers 2>&1); echo "$R" | grep -qE "rdy-0 +0/1 +Running" && ok "READY 0/1 while not ready" || bad rdy-col "$R"
RC=$($KC get pod rdy-0 -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>&1)
echo "$RC" | grep -q False && ok "Ready condition False" || bad rdy-cond "$RC"

$KC exec rdy-0 -- touch /tmp/ready >/dev/null 2>&1
GOOD=0
for i in $(seq 1 15); do
  R=$($KC get pod rdy-0 -o jsonpath='{.status.containerStatuses[0].ready}' 2>&1)
  [ "$R" = "true" ] && { GOOD=1; break; }; sleep 1
done
[ "$GOOD" = 1 ] && ok "readiness flips to ready after probe passes" || bad rdy-true "still not ready"
R=$($KC get pod rdy-0 --no-headers 2>&1); echo "$R" | grep -qE "rdy-0 +1/1 +Running" && ok "READY 1/1 once ready" || bad rdy-col2 "$R"

# --- liveness ---
RC0=$($KC get pod liv-0 -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>&1)
[ "$RC0" = "0" ] && ok "liveness healthy initially (restarts=0)" || bad liv-init "$RC0"
$KC exec liv-0 -- rm -f /tmp/alive >/dev/null 2>&1
BUMPED=0
for i in $(seq 1 20); do
  RC=$($KC get pod liv-0 -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>&1)
  [ -n "$RC" ] && [ "$RC" -ge 1 ] 2>/dev/null && { BUMPED=1; break; }; sleep 1
done
[ "$BUMPED" = 1 ] && ok "liveness failure restarted the container (restarts>=1)" || bad liv-restart "no restart"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
