#!/bin/bash
# Ad-hoc sign a binary with the Nebula dev entitlements (VZ + Hypervisor).
# Usage: scripts/sign-dev.sh <binary> [...]
set -euo pipefail
# Linux needs no signing (KVM gates on /dev/kvm perms, not entitlements).
[ "$(uname)" = "Darwin" ] || { echo "sign-dev: no-op on $(uname)"; exit 0; }
ENTITLEMENTS="$(cd "$(dirname "$0")" && pwd)/entitlements/dev.entitlements"
for bin in "$@"; do
    codesign --force --sign - --entitlements "$ENTITLEMENTS" "$bin"
    echo "signed: $bin"
done
