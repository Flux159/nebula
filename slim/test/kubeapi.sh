#!/bin/bash
# Validate the apiserver-lite against the REAL kubectl: discovery, core CRUD,
# CRD registration + custom resource round-trip, and WATCH (the operator crux).
#
# Note: `kubectl apply` does a client-side OpenAPI-v2-protobuf preflight that
# slim doesn't serve, so we pass --validate=false (CLI-only; client-go
# operators don't do this preflight). Everything else is stock kubectl.
set -u
SERVE="${SERVE:-$(dirname "$0")/../target/release/examples/serve}"
ADDR=127.0.0.1:8453
URL=http://$ADDR
KC="kubectl --cache-dir=/tmp/kubeapi-cache"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "PASS: $1"; }
bad(){ FAIL=$((FAIL+1)); echo "FAIL: $1 -- $2"; }

"$SERVE" "$ADDR" >/tmp/kubeapi.log 2>&1 & SPID=$!
trap 'kill $SPID 2>/dev/null' EXIT
sleep 0.5

export KUBECONFIG=/tmp/kubeapi.kubeconfig
cat > "$KUBECONFIG" <<YAML
apiVersion: v1
kind: Config
clusters: [{name: slim, cluster: {server: $URL}}]
contexts: [{name: slim, context: {cluster: slim, user: slim}}]
current-context: slim
users: [{name: slim, user: {}}]
YAML
rm -rf /tmp/kubeapi-cache

O=$(curl -s $URL/version); echo "$O" | grep -q gitVersion && ok "GET /version" || bad version "$O"
O=$($KC api-resources 2>&1); echo "$O" | grep -q deployments && ok "kubectl api-resources" || bad api-resources "$O"

printf 'apiVersion: v1\nkind: ConfigMap\nmetadata: {name: cfg, namespace: demo}\ndata: {greeting: hi}\n' > /tmp/cm.yaml
O=$($KC apply -f /tmp/cm.yaml 2>&1); echo "$O" | grep -q "configmap/cfg created" && ok "apply configmap" || bad apply-cm "$O"
O=$($KC -n demo get cm cfg -o jsonpath='{.data.greeting}' 2>&1); [ "$O" = hi ] && ok "get -o jsonpath" || bad get-cm "$O"

printf 'apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata: {name: widgets.example.com}\nspec:\n  group: example.com\n  scope: Namespaced\n  names: {plural: widgets, singular: widget, kind: Widget, shortNames: [wg]}\n  versions: [{name: v1, served: true, storage: true}]\n' > /tmp/crd.yaml
O=$($KC apply -f /tmp/crd.yaml 2>&1); echo "$O" | grep -q created && ok "apply CRD" || bad apply-crd "$O"
rm -rf /tmp/kubeapi-cache  # refresh discovery so the new CRD is visible
O=$($KC api-resources 2>&1); echo "$O" | grep -q widgets && ok "CRD in discovery" || bad crd-disco "$O"

printf 'apiVersion: example.com/v1\nkind: Widget\nmetadata: {name: w1, namespace: demo}\nspec: {size: 7}\n' > /tmp/cr.yaml
O=$($KC apply -f /tmp/cr.yaml 2>&1); echo "$O" | grep -q "widget.example.com/w1 created" && ok "apply custom resource" || bad apply-cr "$O"
O=$($KC -n demo get widget w1 -o jsonpath='{.spec.size}' 2>&1); [ "$O" = 7 ] && ok "custom resource round-trips" || bad cr-spec "$O"

# WATCH: stream new + replay existing
( timeout 6 $KC -n demo get widgets -w -o name > /tmp/watch.out 2>&1 ) & WPID=$!
sleep 1
printf 'apiVersion: example.com/v1\nkind: Widget\nmetadata: {name: w2, namespace: demo}\nspec: {size: 9}\n' > /tmp/cr2.yaml
$KC apply -f /tmp/cr2.yaml >/dev/null 2>&1
wait $WPID 2>/dev/null
grep -q "widget.example.com/w2" /tmp/watch.out && ok "watch streamed new object" || bad watch "$(cat /tmp/watch.out)"
grep -q "widget.example.com/w1" /tmp/watch.out && ok "watch replayed existing" || bad watch-replay "$(cat /tmp/watch.out)"

O=$($KC -n demo delete widget w1 2>&1); echo "$O" | grep -q deleted && ok "delete" || bad delete "$O"
O=$($KC -n demo get widget w1 2>&1); echo "$O" | grep -qi "not found" && ok "404 after delete" || bad get-after-delete "$O"

echo ""; echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
