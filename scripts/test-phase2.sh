#!/bin/bash
# Phase 2 acceptance: docker-out-of-the-box via `nebula use docker`, Rosetta
# amd64, compose, build, and lossless context revert.
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

$NEBULA up >/dev/null || { echo "FATAL: nebula up failed"; exit 1; }

# Wait for dockerd inside the guest (first boot pulls nothing; just service start).
for _ in $(seq 1 30); do
    $NEBULA exec docker version >/dev/null 2>&1 && break
    sleep 1
done

echo "--- context management"
BEFORE_CTX=$(docker context show 2>/dev/null || echo "")
cp ~/.docker/config.json /tmp/docker-config-before.json 2>/dev/null || echo "{}" > /tmp/docker-config-before.json
check "use docker"                  "$NEBULA use docker"
check "docker context is nebula"    "[ \"\$(docker context show)\" = nebula ]"

echo "--- docker basics through the socket proxy"
check "docker version (server)"     "docker version --format '{{.Server.Version}}' | grep -q ."
check "docker run hello-world"      "docker run --rm hello-world | grep -q 'Hello from Docker'"
check "arm64 native"                "docker run --rm alpine uname -m | grep -q aarch64"
check "amd64 via Rosetta"           "docker run --rm --platform linux/amd64 alpine uname -m | grep -q x86_64"
check "stdin/attach streaming"      "echo roundtrip | docker run --rm -i alpine cat | grep -q roundtrip"

echo "--- docker build"
TMPD=$(mktemp -d)
printf 'FROM alpine\nRUN echo built-in-nebula > /built\nCMD cat /built\n' > "$TMPD/Dockerfile"
check "docker build"                "docker build -q -t nebula-test-build $TMPD"
check "built image runs"            "docker run --rm nebula-test-build | grep -q built-in-nebula"
docker rmi -f nebula-test-build >/dev/null 2>&1

echo "--- docker compose"
cat > "$TMPD/compose.yml" <<'EOF'
services:
  redis:
    image: redis:7-alpine
  client:
    image: redis:7-alpine
    command: sh -c "for i in 1 2 3 4 5 6 7 8 9 10; do redis-cli -h redis ping && exit 0; sleep 1; done; exit 1"
    depends_on: [redis]
EOF
# client exits 0 only after a successful PONG; attach output is racy for
# fast containers, so assert on the propagated exit code instead.
check "compose up"                  "docker compose -f $TMPD/compose.yml up --exit-code-from client client"
docker compose -f "$TMPD/compose.yml" down >/dev/null 2>&1
rm -rf "$TMPD"

echo "--- revert restores prior state exactly"
check "revert docker"               "$NEBULA revert docker"
AFTER_CTX=$(docker context show 2>/dev/null || echo "")
check "context restored"            "[ \"$BEFORE_CTX\" = \"$AFTER_CTX\" ]"
check "double use/revert idempotent" "$NEBULA use docker && $NEBULA use docker && $NEBULA revert docker && [ \"\$(docker context show)\" = \"$BEFORE_CTX\" ]"

echo
echo "phase 2: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
