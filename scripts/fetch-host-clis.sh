#!/bin/bash
# Fetch darwin/arm64 docker CLI, kubectl, and helm for bundling into
# Nebula.app (all Apache-2.0 — same set Rancher Desktop redistributes).
# Output: ui/src-tauri/resources/bin/{docker,kubectl,helm} + VERSIONS file.
# kubectl/helm are checksum-verified against their published digests;
# download.docker.com publishes no static-build checksums (TLS only).
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=ui/src-tauri/resources/bin
CACHE="${NEBULA_CLI_CACHE:-$HOME/.nebula/cache/host-clis}"
mkdir -p "$OUT" "$CACHE"

KUBECTL_VERSION="${KUBECTL_VERSION:-$(curl -fsSL https://dl.k8s.io/release/stable.txt)}"
HELM_VERSION="${HELM_VERSION:-$(curl -fsSL https://get.helm.sh/helm-latest-version)}"
DOCKER_VERSION="${DOCKER_VERSION:-$(curl -fsSL https://download.docker.com/mac/static/stable/aarch64/ \
    | grep -oE 'docker-[0-9]+\.[0-9]+\.[0-9]+\.tgz' | sort -uV | tail -1 | sed -E 's/docker-(.*)\.tgz/\1/')}"

echo "docker $DOCKER_VERSION | kubectl $KUBECTL_VERSION | helm $HELM_VERSION"

# kubectl (+ published sha256)
if [ ! -f "$CACHE/kubectl-$KUBECTL_VERSION" ]; then
    curl -fsSL -o "$CACHE/kubectl-$KUBECTL_VERSION" \
        "https://dl.k8s.io/release/$KUBECTL_VERSION/bin/darwin/arm64/kubectl"
    expected=$(curl -fsSL "https://dl.k8s.io/release/$KUBECTL_VERSION/bin/darwin/arm64/kubectl.sha256")
    actual=$(shasum -a 256 "$CACHE/kubectl-$KUBECTL_VERSION" | awk '{print $1}')
    [ "$expected" = "$actual" ] || { echo "kubectl checksum MISMATCH"; rm -f "$CACHE/kubectl-$KUBECTL_VERSION"; exit 1; }
fi

# helm (+ published sha256sum)
if [ ! -f "$CACHE/helm-$HELM_VERSION" ]; then
    curl -fsSL -o "$CACHE/helm.tgz" "https://get.helm.sh/helm-$HELM_VERSION-darwin-arm64.tar.gz"
    expected=$(curl -fsSL "https://get.helm.sh/helm-$HELM_VERSION-darwin-arm64.tar.gz.sha256sum" | awk '{print $1}')
    actual=$(shasum -a 256 "$CACHE/helm.tgz" | awk '{print $1}')
    [ "$expected" = "$actual" ] || { echo "helm checksum MISMATCH"; rm -f "$CACHE/helm.tgz"; exit 1; }
    tar -xzf "$CACHE/helm.tgz" -C "$CACHE" darwin-arm64/helm
    mv "$CACHE/darwin-arm64/helm" "$CACHE/helm-$HELM_VERSION"
    rm -rf "$CACHE/helm.tgz" "$CACHE/darwin-arm64"
fi

# docker static CLI (no published checksum; TLS-pinned host)
if [ ! -f "$CACHE/docker-$DOCKER_VERSION" ]; then
    curl -fsSL -o "$CACHE/docker.tgz" \
        "https://download.docker.com/mac/static/stable/aarch64/docker-$DOCKER_VERSION.tgz"
    tar -xzf "$CACHE/docker.tgz" -C "$CACHE" docker/docker
    mv "$CACHE/docker/docker" "$CACHE/docker-$DOCKER_VERSION"
    rm -rf "$CACHE/docker.tgz" "$CACHE/docker"
fi

install -m 755 "$CACHE/kubectl-$KUBECTL_VERSION" "$OUT/kubectl"
install -m 755 "$CACHE/helm-$HELM_VERSION" "$OUT/helm"
install -m 755 "$CACHE/docker-$DOCKER_VERSION" "$OUT/docker"
printf "docker=%s\nkubectl=%s\nhelm=%s\n" "$DOCKER_VERSION" "$KUBECTL_VERSION" "$HELM_VERSION" > "$OUT/VERSIONS"
ls -lh "$OUT"
