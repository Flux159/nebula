#!/bin/bash
# Print the size ledger row: stripped binary sizes (+gz) for every artifact.
set -euo pipefail
cd "$(dirname "$0")/.."

row() { # name path
    local name="$1" path="$2"
    if [ -f "$path" ]; then
        local raw gz
        raw=$(stat -f%z "$path" 2>/dev/null || stat -c%s "$path")
        gz=$(gzip -c "$path" | wc -c | tr -d ' ')
        printf "%-14s %10s B  %10s B gz\n" "$name" "$raw" "$gz"
    else
        printf "%-14s %s\n" "$name" "(not built)"
    fi
}

echo "== nebula-slim size ledger =="
echo "-- guest (aarch64-musl) --"
row slimd        target/aarch64-unknown-linux-musl/release/slimd
echo "-- host CLIs (this triple) --"
row docker-slim  target/release/docker-slim
row kubectl-slim target/release/kubectl-slim
row helm-slim    target/release/helm-slim
echo "-- host CLIs (aarch64-musl, for in-guest tests) --"
row docker-slim-musl  target/aarch64-unknown-linux-musl/release/docker-slim
row kubectl-slim-musl target/aarch64-unknown-linux-musl/release/kubectl-slim
row helm-slim-musl    target/aarch64-unknown-linux-musl/release/helm-slim
