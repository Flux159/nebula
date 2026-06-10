#!/bin/sh
# kubectl-slim + helm-slim end-to-end against slimd, inside the nebula microVM.
set -u
mkdir -p /var/lib/nebula && mount -t tmpfs tmpfs /var/lib/nebula 2>/dev/null
export SLIM_DATA=/var/lib/nebula/slim SLIM_RUN_DIR=/var/lib/nebula/run
export DOCKER_HOST=unix:///var/run/docker.sock
export NEBULA_HOME=/var/lib/nebula
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
K=/slim/kubectl-slim
H=/slim/helm-slim

/slim/slimd >/tmp/slimd.log 2>&1 &
for i in $(seq 1 50); do [ -S /var/run/docker.sock ] && break; sleep 0.1; done
[ -S /var/run/docker.sock ] && ok "slimd up" || { bad "slimd up"; cat /tmp/slimd.log; exit 1; }

# Pre-pull so apply is fast/offline-ish.
/slim/docker-slim pull alpine:3.19 >/dev/null 2>&1

echo "== kubectl apply (ConfigMap + Deployment + Service) =="
cat > /tmp/app.yaml <<'YAML'
apiVersion: v1
kind: ConfigMap
metadata:
  name: appcfg
data:
  GREETING: hello-kube
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 2
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
        - name: web
          image: alpine:3.19
          command: ["sh","-c","echo $GREETING; sleep 300"]
          envFrom:
            - configMapRef:
                name: appcfg
YAML
$K apply -f /tmp/app.yaml >/tmp/o 2>&1 && grep -q "deployment/web created" /tmp/o && ok "apply" || { bad "apply"; cat /tmp/o; tail -20 /tmp/slimd.log; }
sleep 2

echo "== kubectl get =="
$K get deployment >/tmp/o 2>&1 && grep -q "web" /tmp/o && ok "get deployment" || { bad "get deployment"; cat /tmp/o; }
$K get pods >/tmp/o 2>&1; PODS=$(grep -c "web-" /tmp/o); [ "$PODS" -ge 2 ] && ok "get pods (>=2)" || { bad "get pods (got $PODS)"; cat /tmp/o; }

echo "== configmap env reached the pod =="
$K logs web >/tmp/o 2>&1 && grep -q "hello-kube" /tmp/o && ok "envFrom ConfigMap" || { bad "envFrom"; cat /tmp/o; }

echo "== kubectl exec =="
$K exec web -- echo from-kube-exec >/tmp/o 2>&1 && grep -q "from-kube-exec" /tmp/o && ok "kube exec" || { bad "kube exec"; cat /tmp/o; }

echo "== kubectl scale =="
$K scale --replicas=3 deployment/web >/tmp/o 2>&1 && ok "scale cmd" || { bad "scale"; cat /tmp/o; }
sleep 1
$K get pods >/tmp/o 2>&1; PODS=$(grep -c "web-" /tmp/o); [ "$PODS" -ge 3 ] && ok "scaled to 3" || { bad "scaled (got $PODS)"; cat /tmp/o; }

echo "== kubectl delete =="
$K delete -f /tmp/app.yaml >/tmp/o 2>&1 && ok "delete" || { bad "delete"; cat /tmp/o; }

echo "== helm template =="
mkdir -p /tmp/chart/templates
cat > /tmp/chart/Chart.yaml <<'YAML'
name: hi
version: 0.1.0
appVersion: "1"
YAML
cat > /tmp/chart/values.yaml <<'YAML'
replicas: 1
message: from-helm
YAML
cat > /tmp/chart/templates/_helpers.tpl <<'YAML'
{{- define "hi.name" -}}{{ .Release.Name }}-hi{{- end -}}
YAML
cat > /tmp/chart/templates/deploy.yaml <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "hi.name" . }}
spec:
  replicas: {{ .Values.replicas }}
  selector:
    matchLabels:
      app: {{ include "hi.name" . }}
  template:
    metadata:
      labels:
        app: {{ include "hi.name" . }}
    spec:
      containers:
        - name: hi
          image: alpine:3.19
          command: ["sh","-c","echo {{ .Values.message }}; sleep 300"]
YAML
$H template r1 /tmp/chart >/tmp/o 2>&1 && grep -q "name: r1-hi" /tmp/o && ok "helm template" || { bad "helm template"; cat /tmp/o; }

echo "== helm install + list =="
$H install r1 /tmp/chart --set message=installed-msg >/tmp/o 2>&1 && grep -q "STATUS: deployed" /tmp/o && ok "helm install" || { bad "helm install"; cat /tmp/o; tail -20 /tmp/slimd.log; }
sleep 2
$H list >/tmp/o 2>&1 && grep -q "r1" /tmp/o && ok "helm list" || { bad "helm list"; cat /tmp/o; }
$K logs r1-hi >/tmp/o 2>&1 && grep -q "installed-msg" /tmp/o && ok "helm release running w/ values" || { bad "helm release logs"; cat /tmp/o; }
$H uninstall r1 >/tmp/o 2>&1 && grep -q "uninstalled" /tmp/o && ok "helm uninstall" || { bad "helm uninstall"; cat /tmp/o; }

echo ""
echo "RESULT: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
