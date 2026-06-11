#!/bin/bash
# Full slim suite on this Linux box. Pulls go through a registry mirror
# (SLIM_REGISTRY_MIRROR, default mirror.gcr.io) so back-to-back fresh-engine runs
# don't hit Docker Hub's anonymous pull rate limit.
cd ~/slimtest
BIN=$PWD/bin
export SLIMD=$BIN/slimd DSLIM=$BIN/docker-slim PAUSE=$BIN/pause KUBECTL=kubectl SERVE=$BIN/serve
export SLIM_REGISTRY_MIRROR="${SLIM_REGISTRY_MIRROR:-mirror.gcr.io}"
res() { echo "## $1: $2"; }

run_incontainer() {
  local name=$1 script=$2
  local stage=~/slimtest/stage-$name
  rm -rf "$stage"; mkdir -p "$stage"
  cp "$BIN"/slimd "$BIN"/docker-slim "$BIN"/kubectl-slim "$BIN"/helm-slim "$BIN"/pause "$stage/" 2>/dev/null
  cp "test/$script" "$stage/"
  docker rm -f "sl-$name" >/dev/null 2>&1
  local out
  out=$(docker run --rm --privileged -e SLIM_REGISTRY_MIRROR -v "$stage:/slim" --name "sl-$name" alpine:3.19 sh -c \
    "apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; sh /slim/$script" 2>&1)
  res "$name" "$(echo "$out" | grep RESULT)"
}

docker pull alpine:3.19 >/dev/null 2>&1
run_incontainer smoke smoke.sh
run_incontainer kube  kube.sh

for t in kubeapi kube-bridge kube-exec kube-incluster kube-probes kube-sidecar kube-init kube-volumes; do
  docker rm -f slim-bridge slim-exec slim-probes slim-sidecar slim-init slim-vol slim-incluster >/dev/null 2>&1
  out=$(bash "test/$t.sh" 2>&1)
  extra=""; [ "$t" = kube-sidecar ] && extra=$(echo "$out" | grep -i "pause image")
  res "$t" "$(echo "$out" | grep RESULT)${extra:+  [$extra]}"
done
echo LINUXDONE
