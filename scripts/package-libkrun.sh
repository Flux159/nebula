#!/bin/bash
# Make the fork libkrun shippable: collect the dylib plus its brew-linked
# dependencies (libepoxy, virglrenderer, MoltenVK — the whole GPU=1 closure),
# rewrite every install name to @loader_path so the set is relocatable, and
# ad-hoc re-sign (install_name_tool invalidates signatures; arm64 refuses to
# load unsigned code).
#
#   scripts/package-libkrun.sh [OUT_DIR]     (default: dist/libkrun)
#
# Consumers: bundle-app.sh ships the set in Nebula.app/Contents/Frameworks,
# embed-kit.sh in <kit>/lib. The CLI finds it via ancestors of the running
# binary (see krun.rs dylib_path), so no env var is needed.
set -euo pipefail
cd "$(dirname "$0")/.."

SRC=third_party/libkrun/target/release/libkrun.1.18.0.dylib
OUT="${1:-dist/libkrun}"

if [ ! -f "$SRC" ]; then
    echo "==> fork dylib missing; building (scripts/build-libkrun.sh)"
    scripts/build-libkrun.sh
fi

rm -rf "$OUT"
mkdir -p "$OUT"
cp -L "$SRC" "$OUT/libkrun.dylib"
cp -L /opt/homebrew/opt/virglrenderer/lib/libvirglrenderer.1.dylib "$OUT/"
cp -L /opt/homebrew/opt/libepoxy/lib/libepoxy.0.dylib "$OUT/"
cp -L /opt/homebrew/opt/molten-vk/lib/libMoltenVK.dylib "$OUT/"
chmod 755 "$OUT"/*.dylib

for lib in "$OUT"/*.dylib; do
    install_name_tool -id "@loader_path/$(basename "$lib")" "$lib" 2>/dev/null
    # Repoint any brew dependency at the bundled copy beside it.
    # (`|| true`: a dylib with no brew deps must not trip pipefail+errexit.)
    deps="$(otool -L "$lib" | awk 'NR>1 {print $1}' | grep '^/opt/homebrew' || true)"
    echo "$deps" | while read -r dep; do
        [ -n "$dep" ] || continue
        base="$(basename "$dep")"
        if [ -f "$OUT/$base" ]; then
            install_name_tool -change "$dep" "@loader_path/$base" "$lib" 2>/dev/null
        else
            echo "ERROR: $(basename "$lib") needs $dep but it is not bundled" >&2
            exit 1
        fi
    done
    codesign --force -s - "$lib"
done

# Nothing may still point into /opt/homebrew.
if otool -L "$OUT"/*.dylib | grep -q '/opt/homebrew'; then
    echo "ERROR: unresolved homebrew references remain:" >&2
    otool -L "$OUT"/*.dylib | grep '/opt/homebrew' >&2
    exit 1
fi

ls -lh "$OUT" | grep -v '^total'
echo "==> relocatable libkrun set ready: $OUT/"
