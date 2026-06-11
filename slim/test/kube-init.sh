#!/bin/bash
# Phase 4: init containers. An init container runs to completion (populating a
# shared emptyDir) before the main container starts; the pod reports Init:0/1
# while initializing. Driven by real kubectl over the TLS apiserver.
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
KUBECTL="${KUBECTL:-/opt/homebrew/bin/kubectl}"; command -v "$KUBECTL" >/dev/null || KUBECTL=kubectl
STAGE="$HOME/.slim-init-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"

# The init container sleeps then writes a file into the shared volume; the main
# container reads it on start — so its presence proves init ran first.
cat > "$STAGE/initd.yaml" <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: initd, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: initd}}
  template:
    metadata: {labels: {app: initd}}
    spec:
      volumes:
        - name: work
          emptyDir: {}
      initContainers:
        - name: setup
          image: alpine:3.19
          command: ["sh","-c","echo INIT-START; sleep 5; echo init-wrote-this > /work/data; echo INIT-DONE"]
          volumeMounts: [{name: work, mountPath: /work}]
      containers:
        - name: app
          image: alpine:3.19
          command: ["sh","-c","cat /work/data; echo APP-STARTED; sleep 600"]
          volumeMounts: [{name: work, mountPath: /work}]
YAML

docker rm -f slim-init >/dev/null 2>&1
for i in $(seq 1 20); do docker inspect slim-init >/dev/null 2>&1 || break; sleep 0.5; done
docker run -d --privileged -p 16446:6443 -v "$STAGE:/slim" --name slim-init alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-init >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:16446/version >/dev/null 2>&1 && break; sleep 1; done

export KUBECONFIG="$STAGE/kubeconfig"
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:16446", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: t}}]\n' > "$KUBECONFIG"
KC="$KUBECTL --cache-dir=$STAGE/kc"

# Re-apply until the pod object appears (fresh engine pulls alpine).
for i in $(seq 1 90); do
  $KC apply -f "$STAGE/initd.yaml" >/dev/null 2>&1
  $KC get pod initd-0 >/dev/null 2>&1 && break
  sleep 1
done

# Observe Init:0/1 while the init container runs, then wait for Running.
SAW_INIT=0; RUNNING=0
for i in $(seq 1 60); do
  S=$($KC get pod initd-0 --no-headers 2>/dev/null)
  echo "$S" | grep -q "Init:0/1" && SAW_INIT=1
  echo "$S" | grep -qE "1/1 +Running" && { RUNNING=1; break; }
  sleep 0.5
done

[ "$SAW_INIT" = 1 ] && ok "pod shows Init:0/1 while initializing" || bad saw-init "never observed Init:0/1"
[ "$RUNNING" = 1 ] && ok "pod becomes 1/1 Running after init" || bad running "$($KC get pod initd-0 --no-headers 2>&1)"

EC=$($KC get pod initd-0 -o jsonpath='{.status.initContainerStatuses[0].state.terminated.exitCode}' 2>&1)
[ "$EC" = "0" ] && ok "init container terminated exitCode 0" || bad init-ec "$EC"

O=$($KC logs initd-0 -c setup 2>&1); echo "$O" | grep -q INIT-DONE && ok "logs -c setup (init container)" || bad init-logs "$O"
# Main read the file the init wrote → init ran first AND shared the volume.
O=$($KC logs initd-0 2>&1); echo "$O" | grep -q "init-wrote-this" && ok "main started after init (read init's file)" || bad ordering "$O"
echo "$O" | grep -q APP-STARTED && ok "main container running" || bad app-started "$O"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
