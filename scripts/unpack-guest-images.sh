#!/bin/bash
# Unpack a guest-images.yml artifact into vessel/out/, where bundle-app.sh and
# embed-kit.sh look for guest images.
#
#   scripts/unpack-guest-images.sh <artifact-dir> [arch]
#
# Naming contract with guest-images.yml / package-images.sh:
#   kernel-Image-<arch>.gz          -> vessel/out/Image
#   rootfs-<arch>.img.gz            -> vessel/out/rootfs.img
#   rootfs-<flavor>-<arch>.img.gz   -> vessel/out/rootfs-<flavor>.img
#
# It unpacks whatever the artifact actually contains rather than naming files,
# which is the point: release.yml used to extract the kernel and the full
# rootfs by hand and silently omitted the slim one, so `embed-kit.sh --flavor
# slim` fell through to a source build and died on a runner with no Docker.
# Adding a flavor to guest-images.yml must not require editing every consumer.
set -euo pipefail

DIR="${1:-}"
if [ -z "$DIR" ]; then
    echo "usage: $0 <artifact-dir> [arch]" >&2
    exit 2
fi
case "${2:-$(uname -m)}" in
    arm64 | aarch64) ARCH=arm64 ;;
    x86_64 | amd64) ARCH=x86_64 ;;
    *) echo "ERROR: unsupported arch ${2:-$(uname -m)}" >&2; exit 1 ;;
esac
test -d "$DIR" || { echo "ERROR: no such directory: $DIR" >&2; exit 1; }

cd "$(dirname "$0")/.."
mkdir -p vessel/out

# Verify what we are about to unpack. A truncated artifact download otherwise
# surfaces much later as a corrupt guest that fails to boot.
SUMS="$DIR/SHA256SUMS-$ARCH"
if [ -f "$SUMS" ]; then
    # shasum has no --ignore-missing, so check only the lines whose file is
    # actually present: the artifact for one arch never carries the other's.
    FILTERED="$(mktemp)"
    trap 'rm -f "$FILTERED"' EXIT
    while read -r sum name; do
        [ -n "${name:-}" ] || continue
        [ -f "$DIR/$name" ] && printf '%s  %s\n' "$sum" "$name" >> "$FILTERED"
    done < "$SUMS"
    if [ -s "$FILTERED" ]; then
        echo "==> verifying checksums"
        ( cd "$DIR" && shasum -a 256 -c "$FILTERED" )
    fi
else
    echo "warn: no SHA256SUMS-$ARCH in $DIR — unpacking unverified" >&2
fi

echo "==> unpacking guest images ($ARCH)"
FOUND=0
for f in "$DIR"/*"-$ARCH".gz "$DIR"/*"-$ARCH".img.gz; do
    [ -f "$f" ] || continue # unmatched glob stays literal
    base="$(basename "$f" .gz)"     # kernel-Image-arm64 | rootfs-arm64.img | rootfs-slim-arm64.img
    name="${base/-$ARCH/}"          # kernel-Image       | rootfs.img       | rootfs-slim.img
    [ "$name" = "kernel-Image" ] && name=Image
    echo "    $(basename "$f") -> vessel/out/$name"
    gzip -dc "$f" > "vessel/out/$name"
    FOUND=$((FOUND + 1))
done

if [ "$FOUND" -eq 0 ]; then
    echo "ERROR: no guest images for arch $ARCH in $DIR" >&2
    echo "  expected files named like kernel-Image-$ARCH.gz / rootfs-$ARCH.img.gz" >&2
    ls -la "$DIR" >&2
    exit 1
fi
echo "==> unpacked $FOUND guest image(s) into vessel/out/"
