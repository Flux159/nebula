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
#   EMBED.md                        the 6-step integration walkthrough
set -euo pipefail
cd "$(dirname "$0")/.."

FLAVOR=full
OUT=dist-embed
OVERLAY=""
SETUP=""
VESSEL_IMAGE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --flavor) FLAVOR="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --overlay) OVERLAY="$2"; shift 2 ;;
        --setup) SETUP="$2"; shift 2 ;;
        --vessel-image) VESSEL_IMAGE="$2"; shift 2 ;;
        *) echo "usage: $0 [--flavor full|docker|minimal] [--out DIR] [--overlay DIR] [--setup SCRIPT] [--vessel-image DOCKER_REF]" >&2; exit 2 ;;
    esac
done

echo "==> sidecar binaries"
cargo build --release -p nebula-cli -p nebulad
scripts/sign-dev.sh target/release/nebula target/release/nebulad

echo "==> guest images (flavor: $FLAVOR)"
if [ "$FLAVOR" = "full" ]; then IMG=vessel/out/rootfs.img; else IMG="vessel/out/rootfs-$FLAVOR.img"; fi
if [ -n "$OVERLAY$SETUP" ] || [ ! -f "$IMG" ]; then
    # Custom content always forces a fresh build.
    #
    # Building needs Docker. That is fine locally and never true on a macOS CI
    # runner, where the images are meant to arrive as the release workflow's
    # artifacts instead — so say that here rather than letting build-rootfs.sh
    # report a bare "docker: command not found" three levels down.
    if [ -z "$OVERLAY$SETUP" ] && ! command -v docker >/dev/null 2>&1; then
        cat >&2 <<EOF
ERROR: $IMG is missing and there is no docker to build it.

  In CI: fetch it from the guest-images job's artifact, which carries every
  flavor — extract the one matching --flavor $FLAVOR. See the "Fetch latest
  guest images" step in .github/workflows/release.yml.

  Locally: FLAVOR=$FLAVOR vessel/build-rootfs.sh
EOF
        exit 1
    fi
    FLAVOR="$FLAVOR" OVERLAY="$OVERLAY" SETUP="$SETUP" vessel/build-rootfs.sh
fi
test -f vessel/out/Image || vessel/build-kernel.sh

echo "==> relocatable libkrun"
scripts/package-libkrun.sh dist/libkrun

echo "==> assembling $OUT/"
rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/images" "$OUT/lib"
cp target/release/nebula target/release/nebulad "$OUT/bin/"
# The slim engine speaks the same APIs, but an embedder has no
# docker/kubectl/helm to talk to it with — every slim kit ships the clients.
# Shared with the kit-linux / kit-windows jobs so the three kits
# cannot disagree about what bin/ contains.
if [ "$FLAVOR" = "slim" ]; then
    scripts/stage-slim-clis.sh "$OUT/bin"
fi
cp dist/libkrun/*.dylib "$OUT/lib/"
gzip -9 -c vessel/out/Image > "$OUT/images/kernel-Image.gz"
gzip -9 -c "$IMG" > "$OUT/images/rootfs.img.gz"
cp scripts/entitlements/dev.entitlements "$OUT/entitlements.plist"

# Optional: pre-convert YOUR docker image (local or remote ref) into a vessel
# rootfs the shipped app can boot offline (vessels new --rootfs-img …).
if [ -n "$VESSEL_IMAGE" ]; then
    echo "==> converting vessel image: $VESSEL_IMAGE (needs a running engine)"
    TMPIMG="$(mktemp -d)/vessel-rootfs.img"
    target/release/nebula vessels convert-image "$VESSEL_IMAGE" --out "$TMPIMG"
    gzip -9 -c "$TMPIMG" > "$OUT/images/vessel-rootfs.img.gz"
    rm -rf "$(dirname "$TMPIMG")"
fi

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

# Published ports bind 127.0.0.1 unless this is on. Turn it on to honour the
# container's own publish address — `-p 0.0.0.0:6900:6900` then reaches the
# LAN, which is what a multiplayer game server or a shared dev service needs.
# It exposes those ports to the network, so it is off by default.
allow_public_publish = false
EOF

# Licences travel with the binaries.
#
# The kit ships lib/libkrun.* -- a modified Apache-2.0 fork -- and an embedder
# redistributes it inside their own app. Apache-2.0 requires the licence text,
# the copyright notice and the statement of changes to go with it, so put them
# in the kit rather than leaving each embedder to discover the obligation.
mkdir -p "$OUT/licenses"
# The script cd's to the repo root at the top, so these are root-relative.
cp THIRD-PARTY-LICENSES.md "$OUT/licenses/"
cp LICENSE "$OUT/licenses/LICENSE.nebula"

cat > "$OUT/EMBED.md" <<'EOF'
# Embedding Nebula — quick integration

0. Ship `licenses/` with whatever you distribute. lib/ carries four
   third-party libraries -- libkrun (a modified Apache-2.0 fork), MoltenVK
   (Apache-2.0), virglrenderer and libepoxy (MIT). Their licences require the
   notices to travel with the binaries; licenses/ is all of them in one file.

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

4. Your agents as microVMs (if the kit was built with --vessel-image):
       bin/nebula vessels new agent --rootfs-img images/vessel-rootfs.img.gz
   boots YOUR docker image as a snapshot-capable microVM, fully offline.
   (Gunzip once at install time and point --rootfs-img at the raw .img for
   instant ~10ms clone-based creates.) Without a prebuilt image:
       bin/nebula vessels new agent --from-image your/image     # local or pulled

5. Repair surface for your UI:
       bin/nebula down && bin/nebula up        # restart engine
       bin/nebula vessels reset vessel          # restore engine OS, keep data

6. Autostart at login (per-instance launchd label, derived from NEBULA_HOME):
       bin/nebula autostart enable

Full guide: docs/embedding.md in the Nebula repo.
EOF

if [ "$FLAVOR" = "slim" ]; then
    cat >> "$OUT/EMBED.md" <<'EOF'

## This is the slim flavor

The guest runs `slimd` (Rust) instead of dockerd/containerd/k3s, so `bin/`
also carries `docker-slim`, `kubectl-slim` and `helm-slim`. They speak the
same sockets as the real CLIs — point any docker client library at
`unix://$NEBULA_HOME/run/docker.sock` as usual, or use these when you'd
rather not make your users install anything. What slim does and doesn't do:
slim/README.md in the Nebula repo.
EOF
fi

ls -lhR "$OUT" | grep -vE "^$|^total"
echo "==> embed kit ready: $OUT/"
