#!/bin/bash
# Acceptance for the nebula-slim engine (slimd + docker/kubectl/helm-slim).
#
# Strategy: build the slim binaries from the in-repo slim/ workspace, then run
# slimd inside a privileged container on the CURRENT nebula engine vessel —
# i.e. real Linux namespaces/overlayfs/cgroup2 in nebula's microVM on macOS —
# and drive it with the slim clients. This is the same boundary a slim-flavor
# vessel runs in (the VM is the security boundary), without needing the
# engine-selection boot plumbing.
#
# Suites: smoke.sh (core engine), appstack.sh (what an embedded app stack
# asks for: bind mounts, volumes, DNS, scoped ports, load), kube.sh.
#
# House style: capture-then-grep (no `cmd | grep -q`); SIGPIPE-safe.
set -euo pipefail
cd "$(dirname "$0")/.."

SLIM_REPO="${SLIM_REPO:-$PWD/slim}"
MUSL_TARGET=aarch64-unknown-linux-musl
case "$(uname -m)" in x86_64) MUSL_TARGET=x86_64-unknown-linux-musl ;; esac

test -d "$SLIM_REPO" || { echo "ERROR: slim workspace not at $SLIM_REPO (set SLIM_REPO)" >&2; exit 1; }

echo "== building slim binaries ($MUSL_TARGET) =="
( cd "$SLIM_REPO" && chmod +x scripts/zigcc-aarch64-musl scripts/zigcc-x86_64-musl scripts/zigar 2>/dev/null || true
  cargo build --release --target "$MUSL_TARGET" \
    -p slimd -p docker-slim -p kubectl-slim -p helm-slim )

STAGE="$HOME/.nebula-slim-test"   # under HOME so the engine's virtiofs sees it
rm -rf "$STAGE"; mkdir -p "$STAGE"
for b in slimd docker-slim kubectl-slim helm-slim; do
    cp "$SLIM_REPO/target/$MUSL_TARGET/release/$b" "$STAGE/"
done

# `docker load` needs a real archive to load; produce one with the engine that
# is already running (any image will do — the format is what's under test).
if docker image inspect alpine:3.19 >/dev/null 2>&1 || docker pull alpine:3.19 >/dev/null 2>&1; then
    docker save alpine:3.19 -o "$STAGE/load-image.tar" 2>/dev/null || true
    [ -f "$STAGE/load-image.tar" ] && gzip -9 -c "$STAGE/load-image.tar" > "$STAGE/load-image.tar.gz"

    # A probe image whose files are owned by a non-root user, baked by REAL
    # docker. This is the RagnarokMac shape: images built on a developer
    # machine, shipped in the .app, `docker load`ed on first launch — the flow
    # that hid tasks/fixuidgid.md's ownership bug from build-only tests.
    PROBE="$STAGE/uidgid-ctx"
    rm -rf "$PROBE"; mkdir -p "$PROBE"
    printf 'payload\n' > "$PROBE/payload.txt"
    cat > "$PROBE/Dockerfile" <<'EOF'
FROM alpine:3.19
RUN adduser -D -u 4242 appuser \
    && mkdir -p /chowned-dir && chown appuser:appuser /chowned-dir \
    && touch /chowned-file && chown 4242:4242 /chowned-file \
    && mkdir -p /setuid && cp /bin/busybox /setuid/bb && chmod 4755 /setuid/bb
COPY --chown=appuser:appuser payload.txt /copied-file
EOF
    if docker build -q -t nebula-uidgid-probe:1 "$PROBE" >/dev/null 2>&1; then
        docker save nebula-uidgid-probe:1 -o "$STAGE/uidgid-image.tar" 2>/dev/null || true
    else
        echo "WARN: could not build the uid/gid probe image — that check will skip" >&2
    fi
    rm -rf "$PROBE"
fi

echo "== size ledger =="
( cd "$SLIM_REPO" && ./scripts/size-report.sh 2>/dev/null ) || true

# Pick a working docker (the nebula engine).
docker version >/dev/null 2>&1 || { echo "ERROR: nebula engine not reachable via docker" >&2; exit 1; }

# run_suite <name> <script> -> prints output, leaves RESULT line in $LINE
run_suite() {
    local name="$1" script="$2" res="$STAGE/$1-result.txt"
    cp "$SLIM_REPO/test/$script" "$STAGE/$script"
    echo "== running $name acceptance inside the engine microVM =="
    docker run --rm --privileged -v "$STAGE:/slim" alpine:3.19 sh -c \
        "apk add --no-cache iptables ip6tables iproute2 >/dev/null 2>&1; sh /slim/$script" \
        > "$res" 2>&1 || true
    cat "$res"
    LINE="$(grep "RESULT:" "$res" || echo "RESULT: suite produced no result line, 1 failed")"
}

run_suite smoke smoke.sh;       SMOKE_LINE="$LINE"
run_suite appstack appstack.sh; APP_LINE="$LINE"
run_suite kube kube.sh;         KUBE_LINE="$LINE"

echo ""
echo "docker-slim:        $SMOKE_LINE"
echo "app stack:          $APP_LINE"
echo "kubectl/helm-slim:  $KUBE_LINE"
FAILED=0
for line in "$SMOKE_LINE" "$APP_LINE" "$KUBE_LINE"; do
    case "$line" in *" 0 failed"*) ;; *) FAILED=1 ;; esac
done
[ "$FAILED" -eq 0 ] && { echo "test-slim: PASS"; exit 0; }
echo "test-slim: FAIL"; exit 1
