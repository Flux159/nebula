#!/bin/bash
# Build the vendored libkrun fork (third_party/libkrun) on macOS.
# init.c (libkrun's embedded guest init) needs a Linux cross compiler; we use
# zig (brew install zig). Update the fork with:
#   git subtree pull --prefix third_party/libkrun https://github.com/containers/libkrun.git main --squash
set -euo pipefail
cd "$(dirname "$0")/../third_party/libkrun"
# bindgen needs libclang (brew install llvm). Some build scripts link it via
# @rpath; DYLD_* env can't help (stripped by SIP across /usr/bin/make), but
# ~/lib is on dyld's default fallback path, so a symlink there resolves it.
export LIBCLANG_PATH="${LIBCLANG_PATH:-$(ls -d /opt/homebrew/opt/llvm*/lib 2>/dev/null | head -1)}"
if [ -n "$LIBCLANG_PATH" ] && [ ! -e "$HOME/lib/libclang.dylib" ]; then
    mkdir -p "$HOME/lib"
    ln -sf "$LIBCLANG_PATH/libclang.dylib" "$HOME/lib/libclang.dylib"
fi
# Must be a make-variable override: the Makefile assigns (and exports) its own
# CC_LINUX, which clobbers the environment version.
make CC_LINUX="${CC_LINUX:-zig cc -target aarch64-linux-musl}" "$@"
ls -la target/release/libkrun*.dylib
