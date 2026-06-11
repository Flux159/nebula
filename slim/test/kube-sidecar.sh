#!/bin/bash
# Phase 3: multi-container pods / sidecars. A pod with two containers sharing one
# network namespace (localhost) and an emptyDir volume. Driven by real kubectl
# over the TLS apiserver.
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
PAUSE="${PAUSE:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/pause}"
KUBECTL="${KUBECTL:-/opt/homebrew/bin/kubectl}"; command -v "$KUBECTL" >/dev/null || KUBECTL=kubectl
STAGE="$HOME/.slim-sidecar-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"; cp "$PAUSE" "$STAGE/pause"

# app writes a file into the shared emptyDir and serves it on localhost:8080;
# the sidecar shares the netns (reaches localhost) and the volume (reads the file).
cat > "$STAGE/multi.yaml" <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: multi, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: multi}}
  template:
    metadata: {labels: {app: multi}}
    spec:
      volumes:
        - name: shared
          emptyDir: {}
      containers:
        - name: app
          image: alpine:3.19
          command: ["sh","-c","echo APP-UP; echo from-app > /data/msg; while true; do nc -l -p 8080 < /data/msg; done"]
          volumeMounts: [{name: shared, mountPath: /data}]
        - name: side
          image: alpine:3.19
          command: ["sh","-c","echo SIDE-UP; sleep 600"]
          volumeMounts: [{name: shared, mountPath: /data}]
YAML

docker rm -f slim-sidecar >/dev/null 2>&1
for i in $(seq 1 20); do docker inspect slim-sidecar >/dev/null 2>&1 || break; sleep 0.5; done
docker run -d --privileged -e SLIM_REGISTRY_MIRROR -p 16445:6443 -v "$STAGE:/slim" --name slim-sidecar alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-sidecar >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:16445/version >/dev/null 2>&1 && break; sleep 1; done

export KUBECONFIG="$STAGE/kubeconfig"
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:16445", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: t}}]\n' > "$KUBECONFIG"
KC="$KUBECTL --cache-dir=$STAGE/kc"

# Re-apply until both containers are up (fresh engine pulls alpine).
for i in $(seq 1 90); do
  $KC apply -f "$STAGE/multi.yaml" >/dev/null 2>&1
  [ "$($KC get pod multi-0 --no-headers 2>/dev/null | grep -c '2/2')" = 1 ] && break
  sleep 1
done
sleep 2

O=$($KC get pod multi-0 --no-headers 2>&1); echo "$O" | grep -qE "multi-0 +2/2 +Running" && ok "both containers ready (2/2)" || bad ready "$O"
N=$($KC get pod multi-0 -o jsonpath='{.status.containerStatuses[*].name}' 2>&1); echo "$N" | grep -q app && echo "$N" | grep -q side && ok "two containerStatuses (app+side)" || bad cstatuses "$N"

# logs -c selects the container
O=$($KC logs multi-0 -c app 2>&1); echo "$O" | grep -q APP-UP && ok "logs -c app" || bad logs-app "$O"
O=$($KC logs multi-0 -c side 2>&1); echo "$O" | grep -q SIDE-UP && ok "logs -c side" || bad logs-side "$O"

# shared emptyDir: the sidecar reads the file the app wrote
O=$($KC exec multi-0 -c side -- cat /data/msg 2>&1); echo "$O" | grep -q from-app && ok "shared emptyDir volume (side reads app's file)" || bad emptydir "$O"

# shared netns: the sidecar reaches the app's listener over localhost
O=$($KC exec multi-0 -c side -- nc -w2 localhost 8080 2>&1); echo "$O" | grep -q from-app && ok "shared netns (side reaches app on localhost)" || bad netns "$O"

# default exec (no -c) targets the holder (app)
O=$($KC exec multi-0 -- cat /data/msg 2>&1); echo "$O" | grep -q from-app && ok "default exec targets holder" || bad exec-holder "$O"

# the pod sandbox uses the built-in pause image (not the app image)
O=$(docker exec slim-sidecar /slim/docker-slim ps -a --format '{{.Image}}' 2>/dev/null)
echo "$O" | grep -q "nebula/pause" && ok "pod sandbox uses built-in pause image" || bad pause-image "$O"

# delete tears down both containers
$KC delete deployment multi >/dev/null 2>&1; sleep 4
N=$(docker exec slim-sidecar /slim/docker-slim ps -a --format '{{.Names}}' 2>/dev/null | grep -c "default_multi-0")
[ "$N" = 0 ] && ok "delete removed all pod containers" || bad delete "got $N"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
