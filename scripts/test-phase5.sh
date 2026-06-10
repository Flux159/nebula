#!/bin/bash
# Phase 5 acceptance: k3s on demand, kubectl out of the box, prod-safe revert.
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

$NEBULA up >/dev/null || { echo "FATAL: up failed"; exit 1; }

BEFORE_CTX=$(kubectl config current-context 2>/dev/null || echo "default (unset)")
# Whatever happens, never leave the user's kubectl pointing at nebula.
restore_ctx() {
    CUR=$(kubectl config current-context 2>/dev/null || echo "")
    if [ "$CUR" = "nebula" ] && [ "$BEFORE_CTX" != "nebula" ]; then
        if [ "$BEFORE_CTX" = "default (unset)" ]; then
            kubectl config unset current-context >/dev/null 2>&1
        else
            kubectl config use-context "$BEFORE_CTX" >/dev/null 2>&1
        fi
    fi
}
trap restore_ctx EXIT

echo "--- nebula use kubectl"
check "use kubectl succeeds"           "$NEBULA use kubectl"
check "current context is nebula"      "[ \"\$(kubectl config current-context)\" = nebula ]"
check "get nodes Ready"                "kubectl get nodes --no-headers | grep -q ' Ready'"

echo "--- deploy a workload"
kubectl delete deploy nebula-p5 --ignore-not-found >/dev/null 2>&1
check "create deployment"              "kubectl create deployment nebula-p5 --image=nginx:alpine"
check "rollout completes"              "kubectl rollout status deploy/nebula-p5 --timeout=120s"
check "expose NodePort"                "kubectl expose deploy nebula-p5 --port 80 --type NodePort"
NODEPORT=$(kubectl get svc nebula-p5 -o jsonpath='{.spec.ports[0].nodePort}')
GUEST_IP=$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}' | sed -E 's|https://([0-9.]+):.*|\1|')
SVC_OK=0
for _ in $(seq 1 20); do
    curl -fsS -m 2 "http://$GUEST_IP:$NODEPORT/" 2>/dev/null | grep -q nginx && { SVC_OK=1; break; }
    sleep 1
done
check "service reachable from host"    "[ $SVC_OK = 1 ]"
check "kubectl logs works"             "kubectl logs deploy/nebula-p5 --tail 1"
check "kubectl exec works"             "kubectl exec deploy/nebula-p5 -- nginx -v"
kubectl delete svc,deploy nebula-p5 >/dev/null 2>&1

echo "--- k8s survives engine restart"
$NEBULA down >/dev/null && $NEBULA up >/dev/null
NODE_OK=0
for _ in $(seq 1 60); do
    kubectl get nodes --no-headers 2>/dev/null | grep -q ' Ready' && { NODE_OK=1; break; }
    sleep 2
done
check "k3s auto-starts after reboot"   "[ $NODE_OK = 1 ]"

echo "--- revert restores prior context"
check "revert kubectl"                 "$NEBULA revert kubectl"
AFTER_CTX=$(kubectl config current-context 2>/dev/null || echo "default (unset)")
check "context restored"               "[ \"$BEFORE_CTX\" = \"$AFTER_CTX\" ]"
check "nebula entries preserved"       "kubectl config get-contexts nebula"

echo
echo "phase 5: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
