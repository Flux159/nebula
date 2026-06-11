#!/bin/bash
# Validate in-pod (in-cluster) access to the apiserver-lite: TLS + the projected
# ServiceAccount dir + KUBERNETES_SERVICE_* env. Applies a Deployment, then from
# INSIDE the pod container reaches the apiserver over HTTPS using only the
# in-cluster materials — exactly what a client-go operator does.
set -u
SLIMD="${SLIMD:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/slimd}"
DSLIM="${DSLIM:-$(dirname "$0")/../target/aarch64-unknown-linux-musl/release/docker-slim}"
STAGE="$HOME/.slim-incluster-test"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

rm -rf "$STAGE"; mkdir -p "$STAGE"; cp "$SLIMD" "$STAGE/slimd"; cp "$DSLIM" "$STAGE/docker-slim"
cat > "$STAGE/dep.yaml" <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: {name: op, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: op}}
  template:
    metadata: {labels: {app: op}}
    spec: {containers: [{name: op, image: curlimages/curl:8.11.1, command: ["sh","-c","sleep 600"]}]}
YAML

docker rm -f slim-incluster >/dev/null 2>&1
docker run -d --privileged -p 17443:6443 -v "$STAGE:/slim" --name slim-incluster alpine:3.19 sh -c \
  'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula; export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run SLIM_KUBE_API_ADDR=0.0.0.0:6443; exec /slim/slimd' >/dev/null 2>&1
trap 'docker rm -f slim-incluster >/dev/null 2>&1' EXIT
for i in $(seq 1 40); do curl -sk https://localhost:17443/version >/dev/null 2>&1 && break; sleep 1; done

dexec(){ docker exec slim-incluster sh -c "$1" 2>&1; }
dslim(){ docker exec slim-incluster sh -c "DOCKER_HOST=unix:///var/run/docker.sock /slim/docker-slim $1" 2>&1; }

curl -sk https://localhost:17443/version | grep -q gitVersion && ok "apiserver serves HTTPS" || bad tls-up "down"

export KUBECONFIG="$STAGE/kubeconfig"
# kubectl prompts for a username on an HTTPS cluster with no creds; a token
# (auth isn't enforced server-side) keeps it non-interactive — real operators
# carry the projected SA token.
printf 'apiVersion: v1\nkind: Config\nclusters: [{name: s, cluster: {server: "https://localhost:17443", insecure-skip-tls-verify: true}}]\ncontexts: [{name: s, context: {cluster: s, user: s}}]\ncurrent-context: s\nusers: [{name: s, user: {token: slim-admin}}]\n' > "$KUBECONFIG"
KC="kubectl --cache-dir=$STAGE/kc"
$KC apply -f "$STAGE/dep.yaml" >/dev/null 2>&1 && ok "kubectl apply over HTTPS" || bad apply "https apply failed"
# the bridge pulls the (curl-capable) image then starts the pod — allow time
POD=""
for i in $(seq 1 40); do
  POD=$(dslim 'ps --format "{{.Names}}"' | tr -d '"/[]' | grep default_op- | head -1)
  [ -n "$POD" ] && break; sleep 2
done
[ -n "$POD" ] && ok "deployment spawned pod container ($POD)" || { bad spawn "no pod"; echo "RESULT: $PASS passed, $((FAIL+1)) failed"; exit 1; }
sleep 2

SA=/var/run/secrets/kubernetes.io/serviceaccount
dslim "exec $POD cat $SA/namespace" | grep -q default && ok "SA namespace projected" || bad sa-ns "$(dslim "exec $POD ls $SA")"
dslim "exec $POD test -s $SA/ca.crt && echo yes" | grep -q yes && ok "SA ca.crt projected" || bad sa-ca "missing"
dslim "exec $POD test -s $SA/token && echo yes" | grep -q yes && ok "SA token projected" || bad sa-token "missing"
dslim "exec $POD sh -c 'echo \$KUBERNETES_SERVICE_HOST:\$KUBERNETES_SERVICE_PORT'" | grep -q ":6443" && ok "KUBERNETES_SERVICE_* env set" || bad env "$(dslim "exec $POD env")"

# From inside the pod, reach the apiserver EXACTLY like client-go in-cluster
# config: verify the server with the projected ca.crt, authenticate with the
# projected token, target the inherited KUBERNETES_SERVICE_* env. Real curl in
# a curl-capable pod image (pulled by the bridge).
echo "== in-pod: in-cluster HTTPS with projected CA + token (real curl) =="
OUT=$(dslim "exec $POD sh -c 'curl -s --cacert $SA/ca.crt -H \"Authorization: Bearer \$(cat $SA/token)\" https://\$KUBERNETES_SERVICE_HOST:\$KUBERNETES_SERVICE_PORT/api/v1/namespaces/default/pods'")
echo "$OUT" | grep -q '"kind":"PodList"' && ok "in-pod client listed pods (CA-verified TLS + token)" || bad incluster "$OUT"
echo "$OUT" | grep -q '"name":"op-0"' && ok "in-pod client sees its own pod" || bad incluster-self "$(echo "$OUT" | head -c 200)"

# Prove the PROJECTED ca.crt actually validates the serving cert (not just -k).
echo "== projected CA validates the serving cert =="
docker exec slim-incluster cat /var/lib/nebula/slim/kube-sa/default/ca.crt > "$STAGE/ca.crt" 2>/dev/null
curl -s --cacert "$STAGE/ca.crt" --resolve kubernetes.default.svc:17443:127.0.0.1 https://kubernetes.default.svc:17443/version 2>&1 | grep -q gitVersion \
  && ok "curl --cacert (projected CA) verifies the cert" || bad ca-verify "CA did not validate the serving cert"

$KC delete deployment op >/dev/null 2>&1
echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
