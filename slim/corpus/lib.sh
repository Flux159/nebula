# corpus/lib.sh — assertion helpers for corpus cases. POSIX sh (runs under
# macOS bash 3.2, dash, and alpine ash). Sourced by run.sh into each case
# process; never executed directly.
#
# House rule (from nebula): NEVER `cmd | grep -q` — an early-closing reader
# turns into a SIGPIPE phantom failure. Every command's output is captured
# to the $OUT/$ERR files first, then grepped as files.
#
# Contract with run.sh (all provided via environment):
#   CASE_NAME   short case name (filename without .sh)
#   CASE_TMP    fresh per-case scratch dir; deleted by the runner
#   DOCKER_BIN  client binary under test
#   CORPUS_KEEP=1  keep containers/volumes on failure (skips cleanups)
#   CORPUS_QUICK=1 makes skip_if_quick cases SKIP
#
# A case PASSES if it reaches its end with zero failed assertions. `skip`
# exits the case early and marks it SKIP. Registered cleanups always run
# (reverse order), even on failure, unless CORPUS_KEEP=1 and something failed.

_FAILED=0
_PASSED=0
_CLEANUPS=""
LAST_RC=0
OUT="${CASE_TMP:?lib.sh must be run via run.sh}/stdout"
ERR="$CASE_TMP/stderr"

# dk — invoke the docker client under test. Use this instead of a bare
# `docker` or "$DOCKER_BIN" so cleanup_add lines survive eval even if
# DOCKER_BIN contains spaces.
dk() { "$DOCKER_BIN" "$@"; }

_ok() {
    _PASSED=$((_PASSED + 1))
    if [ "${CORPUS_VERBOSE:-0}" = "1" ]; then
        printf '    ok: %s\n' "$1"
    fi
}

_bad() {
    _FAILED=$((_FAILED + 1))
    printf '    FAIL-ASSERT: %s\n' "$1"
    if [ -s "$ERR" ]; then
        printf '    stderr (first 5 lines):\n'
        sed -n '1,5p' "$ERR" | sed 's/^/      | /'
    fi
}

# assert_ok <desc> <cmd...> — run cmd, capture stdout/stderr, assert exit 0.
assert_ok() {
    _desc=$1; shift
    "$@" >"$OUT" 2>"$ERR"
    LAST_RC=$?
    if [ "$LAST_RC" -eq 0 ]; then
        _ok "$_desc"
    else
        _bad "$_desc (exit $LAST_RC): $*"
    fi
}

# assert_fail <desc> <cmd...> — run cmd, capture, assert nonzero exit.
assert_fail() {
    _desc=$1; shift
    "$@" >"$OUT" 2>"$ERR"
    LAST_RC=$?
    if [ "$LAST_RC" -ne 0 ]; then
        _ok "$_desc"
    else
        _bad "$_desc (expected failure, got exit 0): $*"
    fi
}

# assert_exit <n> <desc> <cmd...> — run cmd, capture, assert exit code == n.
assert_exit() {
    _want=$1; _desc=$2; shift 2
    "$@" >"$OUT" 2>"$ERR"
    LAST_RC=$?
    if [ "$LAST_RC" -eq "$_want" ]; then
        _ok "$_desc"
    else
        _bad "$_desc (expected exit $_want, got $LAST_RC): $*"
    fi
}

# assert_out_contains <desc> <pattern> — grep (BRE) the last captured stdout.
assert_out_contains() {
    _desc=$1; _pat=$2
    if grep -e "$_pat" "$OUT" >/dev/null 2>&1; then
        _ok "$_desc"
    else
        _bad "$_desc (no /$_pat/ in captured stdout)"
        printf '    stdout was (first 5 lines):\n'
        sed -n '1,5p' "$OUT" | sed 's/^/      | /'
    fi
}

# assert_out_eq <desc> <expected> — last captured stdout (sans trailing
# newlines) must equal expected exactly.
assert_out_eq() {
    _desc=$1; _want=$2
    _got=$(cat "$OUT" 2>/dev/null)
    if [ "$_got" = "$_want" ]; then
        _ok "$_desc"
    else
        _bad "$_desc (expected '$_want', got '$_got')"
    fi
}

# assert_retry_contains <timeout_s> <desc> <pattern> <cmd...> — rerun cmd
# (capturing each time) about once per second until stdout matches pattern
# or timeout_s elapses. For servers that need warm-up (port publish, apps).
assert_retry_contains() {
    _t=$1; _desc=$2; _pat=$3; shift 3
    _i=0
    while [ "$_i" -le "$_t" ]; do
        "$@" >"$OUT" 2>"$ERR"
        LAST_RC=$?
        if grep -e "$_pat" "$OUT" >/dev/null 2>&1; then
            _ok "$_desc"
            return 0
        fi
        sleep 1
        _i=$((_i + 1))
    done
    _bad "$_desc (timed out after ${_t}s waiting for /$_pat/): $*"
    return 0
}

# skip <reason> — mark the whole case SKIP and stop it. Cleanups still run.
skip() {
    printf '%s\n' "${1:-skipped}" >"$CASE_TMP/skip_reason"
    exit 0
}

# skip_if_quick — heavy app cases call this first; CORPUS_QUICK=1 skips them.
skip_if_quick() {
    if [ "${CORPUS_QUICK:-0}" = "1" ]; then
        skip "CORPUS_QUICK=1 (heavy app case)"
    fi
    return 0
}

# cleanup_add <cmd...> — register teardown. Args are flattened to one line
# and eval'd at case exit, most-recently-added first, errors ignored.
# Use the `dk` function (not "$DOCKER_BIN") inside cleanup commands.
cleanup_add() {
    _CLEANUPS="$*
$_CLEANUPS"
}

# ensure_image <image> — best-effort: pull image if not already present.
# Lets cases run standalone under CORPUS_FILTER. Not an assertion.
ensure_image() {
    if dk image inspect "$1" >/dev/null 2>&1; then
        return 0
    fi
    dk pull "$1" >/dev/null 2>&1 || true
    return 0
}

_run_cleanups() {
    if [ "${CORPUS_KEEP:-0}" = "1" ] && [ "$_FAILED" -gt 0 ]; then
        printf '    CORPUS_KEEP=1: leaving resources for debugging\n'
        return 0
    fi
    # _CLEANUPS is newline-separated, newest first (= reverse registration).
    printf '%s\n' "$_CLEANUPS" | while IFS= read -r _c; do
        [ -n "$_c" ] || continue
        eval "$_c" >/dev/null 2>&1 || true
    done
    return 0
}

_lib_finish() {
    _run_cleanups
    if [ -f "$CASE_TMP/skip_reason" ]; then
        printf 'SKIP\n' >"$CASE_TMP/status"
    elif [ "$_FAILED" -gt 0 ]; then
        printf 'FAIL\n' >"$CASE_TMP/status"
    else
        printf 'PASS\n' >"$CASE_TMP/status"
    fi
}

# Make INT/TERM go through the EXIT trap so cleanups and status still happen.
trap 'exit 1' INT TERM
trap _lib_finish EXIT
