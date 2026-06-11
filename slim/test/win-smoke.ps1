# Native-Windows smoke for the slim CLIs (no WSL2). Runs docker-slim.exe and
# kubectl-slim.exe against a slim engine reached over loopback/LAN TCP — the
# transport nebula's WHP host proxy provides on Windows (guest vsock → TCP).
#
# Setup: point these at a reachable slim engine. In CI/dev we bridge a slimd's
# unix sockets to TCP with socat:
#   socat TCP-LISTEN:2375,fork,reuseaddr UNIX-CONNECT:/var/run/docker.sock
#   socat TCP-LISTEN:6444,fork,reuseaddr UNIX-CONNECT:/var/run/slim-kube.sock
# then: powershell -File win-smoke.ps1 -EngineHost <ip>
param(
  [string]$EngineHost = $(if ($env:SLIM_ENGINE_HOST) { $env:SLIM_ENGINE_HOST } else { "127.0.0.1" }),
  [int]$DockerPort = 23751,
  [int]$KubePort   = 64441,
  [string]$Bin     = "C:\slimwin"
)
$ErrorActionPreference = "Continue"
$env:DOCKER_HOST    = "tcp://${EngineHost}:${DockerPort}"
$env:SLIM_KUBE_HOST = "tcp://${EngineHost}:${KubePort}"
$D = Join-Path $Bin "docker-slim.exe"
$K = Join-Path $Bin "kubectl-slim.exe"
$fail = 0
function Check($name, $output, $needle) {
  if ($output -match [regex]::Escape($needle)) { Write-Output "PASS: $name" }
  else { Write-Output "FAIL: $name -- $output"; $script:fail++ }
}

Check "docker version"  (& $D version 2>&1 | Out-String) "nebula-slim"
Check "docker run"      (& $D run --rm alpine:3.19 echo WIN-DOCKER-OK 2>&1 | Out-String) "WIN-DOCKER-OK"
& $D ps -a | Out-Null
Check "kubectl get nodes" (& $K get nodes 2>&1 | Out-String) "slim"

$yaml = @"
apiVersion: apps/v1
kind: Deployment
metadata: {name: winapp, namespace: default}
spec:
  replicas: 1
  selector: {matchLabels: {app: winapp}}
  template:
    metadata: {labels: {app: winapp}}
    spec: {containers: [{name: c, image: alpine:3.19, command: ["sh","-c","echo WIN-K8S-OK; sleep 600"]}]}
"@
Set-Content -Path (Join-Path $Bin "win.yaml") -Value $yaml -Encoding ascii
Check "kubectl apply" (& $K apply -f (Join-Path $Bin "win.yaml") 2>&1 | Out-String) "winapp created"
Start-Sleep -Seconds 8
Check "kubectl get pods" (& $K get pods 2>&1 | Out-String) "winapp-0"
Check "kubectl logs"     (& $K logs winapp-0 2>&1 | Out-String) "WIN-K8S-OK"
Check "kubectl exec"     (& $K exec winapp-0 -- echo WIN-EXEC-OK 2>&1 | Out-String) "WIN-EXEC-OK"
& $K delete deployment winapp | Out-Null

if ($fail -eq 0) { Write-Output "RESULT: all passed" } else { Write-Output "RESULT: $fail failed" }
