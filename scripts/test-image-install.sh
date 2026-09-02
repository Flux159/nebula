#!/bin/bash
# install-image acceptance (issue #24): sparse, unstaged, and byte-identical.
#
# The whole point of writing holes is that reads still return zeros, and a
# hole written wrong is silent until something tries to boot from it — so
# every check here compares full SHA-256 of the installed file against the
# source, and the last one boots the result.
#
# Runs in a throwaway NEBULA_HOME under /private/tmp (short path: unix socket
# paths over ~104 bytes fail). Never touches ~/.nebula.
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA=target/debug/nebula
# /private/tmp on macOS, /tmp on Linux — short either way, which unix socket
# paths need (SUN_LEN is ~104 bytes).
TMPBASE=/tmp; [ -d /private/tmp ] && TMPBASE=/private/tmp
H=$TMPBASE/nebula-img-test
SRC=$TMPBASE/nebula-img-src

cargo build -p nebula-cli -p nebulad >/dev/null 2>&1 || { echo "FATAL: build failed"; exit 1; }
# Builds invalidate the ad-hoc signature; only macOS/VZ cares.
[ "$(uname -s)" = Darwin ] && scripts/sign-dev.sh target/debug/nebula target/debug/nebulad >/dev/null

PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}
cleanup() {
    [ -S "$H/run/nebulad.sock" ] && NEBULA_HOME="$H" $NEBULA down --force >/dev/null 2>&1
    rm -rf "$H" "$SRC"
}
trap cleanup EXIT

# shasum on macOS, sha256sum on most Linux images.
if command -v sha256sum >/dev/null 2>&1; then
    sha() { sha256sum "$1" | awk '{print $1}'; }
else
    sha() { shasum -a 256 "$1" | awk '{print $1}'; }
fi
# Bytes actually allocated, not the logical length.
phys_mb() { echo $(( $(du -k "$1" | awk '{print $1}') / 1024 )); }
logical_mb() { echo $(( $(wc -c < "$1") / 1024 / 1024 )); }

# Prefer the repo's dev images; fall back to whatever is installed.
# KERNEL_SRC/ROOTFS_SRC override both (cross-arch boxes keep the pair that
# actually boots there in ~/.nebula, not in vessel/out).
if [ -n "${KERNEL_SRC:-}" ] && [ -n "${ROOTFS_SRC:-}" ]; then
    K_SRC=$KERNEL_SRC; R_SRC=$ROOTFS_SRC
elif [ -f vessel/out/Image ] && [ -f vessel/out/rootfs.img ]; then
    K_SRC=vessel/out/Image; R_SRC=vessel/out/rootfs.img
elif [ -f ~/.nebula/kernel/Image ] && [ -f ~/.nebula/images/rootfs-pristine.img ]; then
    K_SRC=~/.nebula/kernel/Image; R_SRC=~/.nebula/images/rootfs-pristine.img
else
    echo "FATAL: no guest images to test with (build vessel/out or run install-image once)"; exit 1
fi
echo "source kernel: $K_SRC"
echo "source rootfs: $R_SRC ($(logical_mb $R_SRC) MiB logical)"
K_SHA=$(sha "$K_SRC"); R_SHA=$(sha "$R_SRC")

rm -rf "$H" "$SRC"; mkdir -p "$H" "$SRC"
# Ports of its own: a developer's real engine (or an embedded one) is usually
# up, and this suite must not fight it for the default ports.
write_config() {
    cat > "$H/config.toml" <<EOF
api_port = 7561
dns_port = 42173
k8s_port = 6563
dns_zone = "imgtest.local"
max_ram_mib = 2048
cpus = 2
data_disk_gib = 8
EOF
}
write_config

echo
echo "--- raw (uncompressed) sources"
T0=$(date +%s)
NEBULA_HOME="$H" $NEBULA install-image --kernel "$K_SRC" --rootfs "$R_SRC" > /tmp/img-install.txt 2>&1
RC=$?
T1=$(date +%s)
cat /tmp/img-install.txt
check "install-image succeeds"          "[ $RC = 0 ]"
check "kernel is byte-identical"        "[ \"$(sha $H/kernel/Image)\" = \"$K_SHA\" ]"
check "pristine is byte-identical"      "[ \"$(sha $H/images/rootfs-pristine.img)\" = \"$R_SHA\" ]"
check "live disk is byte-identical"     "[ \"$(sha $H/disks/rootfs.img)\" = \"$R_SHA\" ]"
check "no staging copy left behind"     "! [ -e $H/cache/image-install ]"

LOG=$(logical_mb "$H/images/rootfs-pristine.img")
PHYS=$(phys_mb "$H/images/rootfs-pristine.img")
echo "    pristine: ${PHYS} MiB on disk of ${LOG} MiB logical (install took $((T1-T0))s)"
check "pristine is sparse (<25% of logical)" "[ $PHYS -lt $((LOG / 4)) ]"
LIVE_PHYS=$(phys_mb "$H/disks/rootfs.img")
echo "    live:     ${LIVE_PHYS} MiB on disk"
check "live disk is sparse or cloned"   "[ $LIVE_PHYS -lt $((LOG / 4)) ]"

echo
echo "--- gzip sources (what embedders and releases actually ship)"
gzip -9 -c "$K_SRC" > "$SRC/kernel-Image.gz"
gzip -9 -c "$R_SRC" > "$SRC/rootfs.img.gz"
echo "    shipped sizes: kernel $(du -h $SRC/kernel-Image.gz | awk '{print $1}'), rootfs $(du -h $SRC/rootfs.img.gz | awk '{print $1}')"
rm -rf "$H"; mkdir -p "$H"; write_config
T0=$(date +%s)
NEBULA_HOME="$H" $NEBULA install-image --kernel "$SRC/kernel-Image.gz" --rootfs "$SRC/rootfs.img.gz" >/dev/null 2>&1
RC=$?
T1=$(date +%s)
check "install from .gz succeeds"       "[ $RC = 0 ]"
check "kernel from .gz is identical"    "[ \"$(sha $H/kernel/Image)\" = \"$K_SHA\" ]"
check "pristine from .gz is identical"  "[ \"$(sha $H/images/rootfs-pristine.img)\" = \"$R_SHA\" ]"
check "live from .gz is identical"      "[ \"$(sha $H/disks/rootfs.img)\" = \"$R_SHA\" ]"
PHYS=$(phys_mb "$H/images/rootfs-pristine.img")
echo "    pristine: ${PHYS} MiB on disk of ${LOG} MiB logical (install took $((T1-T0))s)"
check "gz install is sparse"            "[ $PHYS -lt $((LOG / 4)) ]"

echo
echo "--- upgrade: installing over an existing install leaves nothing of the old one"
# The reported symptom was an upgrade, and an in-place write that only
# overwrites the non-zero blocks would silently keep the old image's data.
python3 - "$H/disks/rootfs.img" "$H/images/rootfs-pristine.img" <<'PY'
import sys
# Scribble a recognisable pattern into a region the real image leaves zeroed,
# so a stale byte after reinstall is detectable.
for p in sys.argv[1:]:
    with open(p, "r+b") as f:
        f.seek(400 * 1024 * 1024)
        f.write(b"STALE" * 1000)
PY
check "scribble took effect"            "[ \"$(sha $H/images/rootfs-pristine.img)\" != \"$R_SHA\" ]"
NEBULA_HOME="$H" $NEBULA install-image --kernel "$SRC/kernel-Image.gz" --rootfs "$SRC/rootfs.img.gz" >/dev/null 2>&1
check "reinstall restores pristine"     "[ \"$(sha $H/images/rootfs-pristine.img)\" = \"$R_SHA\" ]"
check "reinstall restores live disk"    "[ \"$(sha $H/disks/rootfs.img)\" = \"$R_SHA\" ]"

echo
echo "--- upgrade: a genuinely different image replaces the old one wholesale"
# 0.1.7 -> 0.1.8 changed both images; installing a shorter one must not leave
# the tail of the longer one behind.
python3 - "$SRC/other.img" <<'PY'
import sys
# Half the size, different content, still mostly zeros like a real rootfs.
with open(sys.argv[1], "wb") as f:
    f.write(b"OTHERIMAGE" * 100)
    f.truncate(300 * 1024 * 1024)
    f.seek(200 * 1024 * 1024)
    f.write(b"tail-marker")
PY
O_SHA=$(sha "$SRC/other.img")
NEBULA_HOME="$H" $NEBULA install-image --kernel "$K_SRC" --rootfs "$SRC/other.img" >/dev/null 2>&1
check "different image installs clean"  "[ \"$(sha $H/images/rootfs-pristine.img)\" = \"$O_SHA\" ]"
check "live disk matches the new image" "[ \"$(sha $H/disks/rootfs.img)\" = \"$O_SHA\" ]"
check "size shrank to the new image"    "[ $(wc -c < $H/images/rootfs-pristine.img) = $(wc -c < $SRC/other.img) ]"

echo
echo "--- vessels reset (the other clone_file path)"
NEBULA_HOME="$H" $NEBULA install-image --kernel "$K_SRC" --rootfs "$R_SRC" >/dev/null 2>&1
python3 - "$H/disks/rootfs.img" <<'PY'
import sys
with open(sys.argv[1], "r+b") as f:
    f.seek(400 * 1024 * 1024)
    f.write(b"BROKEN" * 1000)
PY
NEBULA_HOME="$H" $NEBULA vessels reset engine >/dev/null 2>&1
check "reset restores pristine bytes"   "[ \"$(sha $H/disks/rootfs.img)\" = \"$R_SHA\" ]"
check "reset output is sparse"          "[ $(phys_mb $H/disks/rootfs.img) -lt $((LOG / 4)) ]"

echo
echo "--- the installed image actually boots"
if [ "${SKIP_BOOT:-0}" = 1 ]; then
    echo "SKIP: boot check (SKIP_BOOT=1)"
else
NEBULA_HOME="$H" nohup target/debug/nebulad >/dev/null 2>&1 &
disown 2>/dev/null
BOOTED=1
for _ in $(seq 1 120); do
    if [ -S "$H/run/nebulad.sock" ]; then
        NEBULA_HOME="$H" $NEBULA status > /tmp/img-status.txt 2>&1
        grep -q "agent:.*healthy" /tmp/img-status.txt && { BOOTED=0; break; }
    fi
    sleep 0.5
done
check "engine boots from the sparse image" "[ $BOOTED = 0 ]"
check "guest filesystem is intact"         "NEBULA_HOME=$H $NEBULA exec sh -c 'ls /sbin/nebula-init && head -c 1000000 /usr/bin/vessel-agent | wc -c'"
NEBULA_HOME="$H" $NEBULA down >/dev/null 2>&1
fi

echo
echo "image-install: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
