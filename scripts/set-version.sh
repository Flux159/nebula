#!/usr/bin/env bash
#
# Set the release version everywhere it is written down.
#
#   scripts/set-version.sh 0.1.7
#   scripts/set-version.sh 0.1.7 --no-lock   # skip `cargo update -w`
#
# The version lives in three files, and they are not one file for a reason:
# `ui/` is excluded from the cargo workspace (see `exclude` in the root
# Cargo.toml), so `[workspace.package] version` does not reach the Tauri app.
#
# That asymmetry shipped a real bug. Every artifact except the DMG takes its
# name from the root Cargo.toml, so bumping only that produced correctly named
# tarballs and a DMG still called Nebula_0.1.3_aarch64.dmg -- for three
# releases. The installer predicted the name from the tag, so `curl | bash` on
# macOS failed against releases that were otherwise fine.
#
# So: one command writes all of them. Run it instead of editing by hand.

set -euo pipefail

VERSION="${1:-}"
NO_LOCK="${2:-}"

if [ -z "$VERSION" ]; then
    echo "usage: scripts/set-version.sh <version> [--no-lock]" >&2
    echo "example: scripts/set-version.sh 0.1.7" >&2
    exit 1
fi

# Accept 0.1.7 or v0.1.7; write it without the v.
VERSION="${VERSION#v}"

if ! echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
    echo "error: '$VERSION' is not a semver version (expected 0.1.7)" >&2
    exit 1
fi

cd "$(dirname "$0")/.."

# 1. The cargo workspace -- names the Linux/Windows tarballs, the slim CLI
#    archives, and everything built from `cargo build`.
sed -i.bak -E "s/^version = \"[^\"]+\"/version = \"$VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

# 2. The Tauri crate -- outside the workspace, so it needs saying twice.
sed -i.bak -E "s/^version = \"[^\"]+\"/version = \"$VERSION\"/" ui/src-tauri/Cargo.toml
rm -f ui/src-tauri/Cargo.toml.bak

# 3. The Tauri config -- this is the one the DMG is named from.
sed -i.bak -E "s/(\"version\"[[:space:]]*:[[:space:]]*)\"[^\"]+\"/\1\"$VERSION\"/" ui/src-tauri/tauri.conf.json
rm -f ui/src-tauri/tauri.conf.json.bak

# 4. The lockfile records each workspace member's version; without this it
#    silently drifts, as it did from 0.1.3 through 0.1.5.
if [ "$NO_LOCK" != "--no-lock" ] && command -v cargo >/dev/null 2>&1; then
    cargo update -w --quiet
fi

echo "version -> $VERSION"
grep -m1 '^version' Cargo.toml               | sed 's/^/  Cargo.toml                     /'
grep -m1 '^version' ui/src-tauri/Cargo.toml  | sed 's/^/  ui\/src-tauri\/Cargo.toml       /'
grep -m1 '"version"' ui/src-tauri/tauri.conf.json | tr -d ' ' | sed 's/^/  ui\/src-tauri\/tauri.conf.json  /'
