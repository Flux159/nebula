#!/bin/bash
# Acceptance for the nebula-slim engine (slimd + docker/kubectl/helm-slim).
#
# Strategy: build the slim binaries from the nebula-slim workspace, then run
# slimd inside a privileged container on the CURRENT nebula engine vessel —
# i.e. real Linux namespaces/overlayfs/cgroup2 in nebula's microVM on macOS —
# and drive it with the slim clients. This is the same boundary a slim-flavor
# vessel runs in (the VM is the security boundary), without needing the
# engine-selection boot plumbing.
#
# House style: capture-then-grep (no `cmd | grep -q`); SIGPIPE-safe.
set -euo pipefail

SLIM_REPO="${SLIM_REPO:-$HOME/Projects/nebula-slim}"
MUSL_TARGET=aarch64-unknown-linux-musl
case "$(uname -m)" in x86_64) MUSL_TARGET=x86_64-unknown-linux-musl ;; esac

test -d "$SLIM_REPO" || { echo "ERROR: nebula-slim repo not at $SLIM_REPO (set SLIM_REPO)" >&2; exit 1; }

echo "== building slim binaries ($MUSL_TARGET) =="
( cd "$SLIM_REPO" && chmod +x scripts/zigcc-aarch64-musl scripts/zigar 2>/dev/null || true
  cargo build --release --target "$MUSL_TARGET" \
    -p slimd -p docker-slim -p kubectl-slim -p helm-slim )

STAGE="$HOME/.nebula-slim-test"   # under HOME so the engine's virtiofs sees it
rm -rf "$STAGE"; mkdir -p "$STAGE"
for b in slimd docker-slim kubectl-slim helm-slim; do
    cp "$SLIM_REPO/target/$MUSL_TARGET/release/$b" "$STAGE/"
done
cp "$SLIM_REPO/test/smoke.sh" "$STAGE/smoke.sh" 2>/dev/null || true

echo "== size ledger =="
( cd "$SLIM_REPO" && ./scripts/size-report.sh 2>/dev/null ) || true

# Pick a working docker (the nebula engine).
docker version >/dev/null 2>&1 || { echo "ERROR: nebula engine not reachable via docker" >&2; exit 1; }

RES="$STAGE/result.txt"
echo "== running slim acceptance inside the engine microVM =="
docker run --rm --privileged -v "$STAGE:/slim" alpine:3.19 sh -c \
    'apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; sh /slim/smoke.sh' \
    > "$RES" 2>&1 || true

cat "$RES"
# Capture-then-grep the score line.
SCORE_LINE="$(grep "RESULT:" "$RES" || true)"
echo ""
echo "slim acceptance: $SCORE_LINE"
case "$SCORE_LINE" in
    *" 0 failed"*) echo "test-slim: PASS"; exit 0 ;;
    *) echo "test-slim: FAIL"; exit 1 ;;
esac
