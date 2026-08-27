#!/bin/bash
# Build the nebula-slim host CLIs and stage them into an embed kit's bin/.
#
#   scripts/stage-slim-clis.sh <kit>/bin
#
# The slim engine speaks the Docker and Kubernetes APIs, but an embedder
# shipping it has no docker/kubectl/helm to talk to it with — so every slim
# kit carries these three (pure Rust, ~2.5 MB for all of them together).
#
# One definition, used by all three kit assemblers: scripts/embed-kit.sh
# (macOS), embed-kit-linux.yml and embed-kit-windows.yml. They used to
# disagree — the macOS kit shipped the CLIs and the other two did not, so
# embedders on Linux and Windows silently needed a second download from
# nebula-slim-clis-*. Keep this the only place that knows the list.
set -euo pipefail

BIN="${1:-}"
if [ -z "$BIN" ]; then
    echo "usage: $0 <kit-bin-dir>" >&2
    exit 2
fi
cd "$(dirname "$0")/.."
mkdir -p "$BIN"

# Git Bash on a Windows runner: cargo emits .exe.
EXE=""
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN* | Windows_NT) EXE=.exe ;;
esac

echo "==> slim host CLIs"
( cd slim && cargo build --release -p docker-slim -p kubectl-slim -p helm-slim )
for b in docker-slim kubectl-slim helm-slim; do
    src="slim/target/release/$b$EXE"
    test -f "$src" || { echo "ERROR: expected $src after the build" >&2; exit 1; }
    cp "$src" "$BIN/"
done
echo "==> staged docker-slim, kubectl-slim, helm-slim into $BIN/"
