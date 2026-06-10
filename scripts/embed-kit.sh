#!/bin/bash
# Assemble everything a consuming app needs to embed Nebula:
#
#   scripts/embed-kit.sh [--flavor full|docker|minimal] [--out DIR]
#
# Output (default dist-embed/):
#   bin/nebula bin/nebulad          signed sidecar binaries
#   lib/libkrun.dylib (+deps)       relocatable fork dylib (sandboxes/GPU/
#                                   krun vessels; bin/nebula finds it at
#                                   ../lib automatically)
#   images/kernel-Image.gz          guest kernel
#   images/rootfs.img.gz            guest rootfs (chosen flavor)
#   config.toml.example             per-instance settings, ready to brand
#   entitlements.plist              required for signing the sidecars
#   EMBED.md                        the 5-step integration walkthrough
set -euo pipefail
cd "$(dirname "$0")/.."

FLAVOR=full
OUT=dist-embed
OVERLAY=""
SETUP=""
while [ $# -gt 0 ]; do
    case "$1" in
        --flavor) FLAVOR="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --overlay) OVERLAY="$2"; shift 2 ;;
        --setup) SETUP="$2"; shift 2 ;;
        *) echo "usage: $0 [--flavor full|docker|minimal] [--out DIR] [--overlay DIR] [--setup SCRIPT]" >&2; exit 2 ;;
    esac
done

echo "==> sidecar binaries"
cargo build --release -p nebula-cli -p nebulad
scripts/sign-dev.sh target/release/nebula target/release/nebulad

echo "==> guest images (flavor: $FLAVOR)"
if [ "$FLAVOR" = "full" ]; then IMG=vessel/out/rootfs.img; else IMG="vessel/out/rootfs-$FLAVOR.img"; fi
if [ -n "$OVERLAY$SETUP" ] || [ ! -f "$IMG" ]; then
    # Custom content always forces a fresh build.
    FLAVOR="$FLAVOR" OVERLAY="$OVERLAY" SETUP="$SETUP" vessel/build-rootfs.sh
fi
test -f vessel/out/Image || vessel/build-kernel.sh

echo "==> relocatable libkrun"
scripts/package-libkrun.sh dist/libkrun

echo "==> assembling $OUT/"
rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/images" "$OUT/lib"
cp target/release/nebula target/release/nebulad "$OUT/bin/"
cp dist/libkrun/*.dylib "$OUT/lib/"
gzip -9 -c vessel/out/Image > "$OUT/images/kernel-Image.gz"
gzip -9 -c "$IMG" > "$OUT/images/rootfs.img.gz"
cp scripts/entitlements/dev.entitlements "$OUT/entitlements.plist"

cat > "$OUT/config.toml.example" <<'EOF'
# Per-instance Nebula settings — copy to $NEBULA_HOME/config.toml before the
# first `nebula up`. Every value is optional.

# Resource ceilings (ballooning returns idle RAM to macOS).
max_ram_mib = 8192
cpus = 4
data_disk_gib = 32

# Isolation from a standalone Nebula install (and other embedders):
api_port = 7461          # REST API on 127.0.0.1 (0 disables)
dns_port = 42061         # host UDP port for the guest DNS relay
k8s_port = 6461          # host forward to this instance's k3s API

# Brand the container DNS zone: <name>.<zone> resolves on this instance.
dns_zone = "galaxy.local"
EOF

cat > "$OUT/EMBED.md" <<'EOF'
# Embedding Nebula — quick integration

1. Ship `bin/`, `lib/`, `images/` inside your app (Tauri: sidecars +
   resources). Keep `lib/` next to `bin/` — nebula finds `../lib/libkrun.dylib`
   by itself (needed for sandboxes, GPU, and krun vessels; the engine and vz
   vessels work without it). Sign the sidecars with `entitlements.plist`
   (virtualization + hypervisor).

2. First run, from your app:
       export NEBULA_HOME="$HOME/Library/Application Support/YourApp/nebula"
       mkdir -p "$NEBULA_HOME" && cp config.toml "$NEBULA_HOME/"   # your branded copy
       bin/nebula install-image --kernel images/kernel-Image.gz --rootfs images/rootfs.img.gz
       bin/nebula up          # ~0.5s to a healthy engine

3. Talk to it (always with NEBULA_HOME set):
       REST     http://127.0.0.1:<api_port>/v1alpha1/…  (SDKs: sdk/typescript, sdk/python)
       docker   unix://$NEBULA_HOME/run/docker.sock      (any docker client library)
       k8s      bin/nebula kubectl … / KUBECONFIG=$NEBULA_HOME/kubeconfig
                (first call starts k3s in the engine; ~20s once)

4. Repair surface for your UI:
       bin/nebula down && bin/nebula up        # restart engine
       bin/nebula vessels reset vessel          # restore engine OS, keep data

5. Autostart at login (per-instance launchd label, derived from NEBULA_HOME):
       bin/nebula autostart enable

Full guide: docs/embedding.md in the Nebula repo.
EOF

ls -lhR "$OUT" | grep -vE "^$|^total"
echo "==> embed kit ready: $OUT/"
