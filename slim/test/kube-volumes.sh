#!/bin/bash
# Volume types: configMap + secret mounted as files (items, subPath). Driven by
# real kubectl over the TLS apiserver.
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
PAUSE="${PAUSE:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/pause}"
KUBECTL="${KUBECTL:-/opt/homebrew/bin/kubectl}"; command -v "$KUBECTL" >/dev/null || KUBECTL=kubectl
STAGE="$HOME/.slim-vol-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"; cp "$PAUSE" "$STAGE/pause"

cat > "$STAGE/vol.yaml" <<'YAML'
apiVersion: v1
kind: ConfigMap
metadata: {name: cfg, namespace: default}
data:
  greeting: hello-vol
  app.conf: "key=value"
---
apiVersion: v1
kind: Secret
metadata: {name: sec, namespace: default}
stringData:
  token: s3cr3t
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: vol, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: vol}}
  template:
    metadata: {labels: {app: vol}}
    spec:
      volumes:
        - name: cfgvol
          configMap: {name: cfg}
        - name: secvol
          secret: {secretName: sec}
        - name: conf-sub
          configMap: {name: cfg}
      containers:
        - name: app
          image: alpine:3.19
          command: ["sh","-c","cat /etc/cfg/greeting; cat /etc/sec/token; sleep 600"]
          volumeMounts:
            - {name: cfgvol, mountPath: /etc/cfg}
            - {name: secvol, mountPath: /etc/sec}
            - {name: conf-sub, mountPath: /etc/app.conf, subPath: app.conf}
YAML

docker rm -f slim-vol >/dev/null 2>&1
for i in $(seq 1 20); do docker inspect slim-vol >/dev/null 2>&1 || break; sleep 0.5; done
docker run -d --privileged -e SLIM_REGISTRY_MIRROR -p 16447:6443 -v "$STAGE:/slim" --name slim-vol alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-vol >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:16447/version >/dev/null 2>&1 && break; sleep 1; done

export KUBECONFIG="$STAGE/kubeconfig"
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:16447", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: t}}]\n' > "$KUBECONFIG"
KC="$KUBECTL --cache-dir=$STAGE/kc"

for i in $(seq 1 90); do
  $KC apply -f "$STAGE/vol.yaml" >/dev/null 2>&1
  [ "$($KC get pod vol-0 --no-headers 2>/dev/null | grep -c '1/1')" = 1 ] && break
  sleep 1
done
sleep 2

O=$($KC logs vol-0 2>&1); echo "$O" | grep -q hello-vol && ok "configMap key mounted as file" || bad cm "$O"
echo "$O" | grep -q s3cr3t && ok "secret (stringData) mounted as file" || bad sec "$O"
O=$($KC exec vol-0 -- cat /etc/cfg/app.conf 2>&1); echo "$O" | grep -q "key=value" && ok "configMap multi-key dir" || bad cm-multi "$O"
O=$($KC exec vol-0 -- cat /etc/app.conf 2>&1); echo "$O" | grep -q "key=value" && ok "configMap subPath (single file)" || bad subpath "$O"
# configMap/secret mounts are read-only
O=$($KC exec vol-0 -- sh -c 'echo x > /etc/cfg/greeting 2>&1 || echo RO' 2>&1); echo "$O" | grep -q RO && ok "configMap mount is read-only" || bad ro "$O"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
