#!/bin/bash
# Split debug info out of shipped binaries: smaller DMG/kit, but bug-report
# traces stay symbolicatable from dist-debug/.
#
#   scripts/strip-debug.sh FILE...
#
# Mach-O: DWARF (if present) -> <name>.dSYM via dsymutil, plus the exact
#         unstripped binary (symbol tables alone are enough for `atos`),
#         then `strip -S -x`. Binaries strip(1) refuses (some Go layouts)
#         are left untouched.
# ELF:    llvm-objcopy --only-keep-debug -> <name>.debug + gnu-debuglink,
#         then --strip-all (gdb/addr2line find the .debug by link).
#
# OPT-IN for now (callers gate on NEBULA_STRIP=1): trace recovery from the
# split artifacts is not yet verified end-to-end — see tasks/features.md.
set -euo pipefail
cd "$(dirname "$0")/.."
# NOT under dist/ — package-images.sh recreates dist/ from scratch and would
# eat recovery artifacts produced earlier in a build.
OUT=dist-debug
mkdir -p "$OUT"

LLVM_OBJCOPY="${LLVM_OBJCOPY:-$(ls /opt/homebrew/opt/llvm*/bin/llvm-objcopy 2>/dev/null | head -1)}"

for f in "$@"; do
    base="$(basename "$f")"
    before=$(stat -f%z "$f")
    case "$(file -b "$f")" in
        Mach-O*)
            dsymutil "$f" -o "$OUT/$base.dSYM" >/dev/null 2>&1 || true
            cp "$f" "$OUT/$base.unstripped"
            if ! strip -S -x "$f" 2>/dev/null; then
                strip -S "$f" 2>/dev/null || {
                    rm -f "$OUT/$base.unstripped"
                    echo "skip (strip refused): $f"
                    continue
                }
            fi
            ;;
        ELF*)
            [ -n "$LLVM_OBJCOPY" ] || { echo "ERROR: llvm-objcopy not found (brew install llvm)" >&2; exit 1; }
            "$LLVM_OBJCOPY" --only-keep-debug "$f" "$OUT/$base.debug"
            "$LLVM_OBJCOPY" --strip-all "--add-gnu-debuglink=$OUT/$base.debug" "$f"
            ;;
        *)
            echo "skip (not a binary): $f"
            continue
            ;;
    esac
    after=$(stat -f%z "$f")
    echo "stripped: $f ($((before / 1024)) -> $((after / 1024)) KB)"
done
