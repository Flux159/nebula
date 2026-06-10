#!/bin/sh
# corpus/run.sh — docker compatibility corpus runner for nebula-slim.
# POSIX sh (macOS bash 3.2 / dash / alpine ash compatible). See README.md.
#
# Env contract:
#   DOCKER_BIN     client binary under test (default: docker)
#   DOCKER_HOST    daemon socket under test (passed through untouched)
#   CORPUS_FILTER  optional glob over case names, e.g. '0*' or '*build*'
#   CORPUS_QUICK=1 skip heavy app cases (190/200)
#   CORPUS_KEEP=1  keep containers/volumes/scratch dirs on failure
#
# Each case runs in its own `sh -u` process (set -u, NOT -e: cases assert
# explicitly via lib.sh helpers). Exit nonzero if any case FAILs.
set -u

CORPUS_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DOCKER_BIN="${DOCKER_BIN:-docker}"
CORPUS_FILTER="${CORPUS_FILTER:-*}"
export DOCKER_BIN

RESULTS_DIR="$CORPUS_DIR/results"
mkdir -p "$RESULTS_DIR"
TSV="$RESULTS_DIR/last.tsv"
: >"$TSV"

WORK_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/slimcorpus.XXXXXX") || exit 1

TOTAL=0; PASSC=0; FAILC=0; SKIPC=0

_final() {
    if [ "${CORPUS_KEEP:-0}" = "1" ] && [ "$FAILC" -gt 0 ]; then
        printf 'CORPUS_KEEP=1: scratch dirs kept at %s\n' "$WORK_ROOT"
    else
        rm -rf "$WORK_ROOT"
    fi
}
trap _final EXIT
trap 'exit 130' INT TERM

# Best-effort sweep of slimtest-* debris (containers, volumes, networks,
# images). All output discarded; the daemon under test may not even be up.
# Note: $ids word-splitting below is intentional (ids never contain spaces).
sweep() {
    ids=$("$DOCKER_BIN" ps -aq --filter "name=slimtest-" 2>/dev/null) || ids=""
    if [ -n "$ids" ]; then
        # shellcheck disable=SC2086
        "$DOCKER_BIN" rm -f $ids >/dev/null 2>&1 || true
    fi
    vols=$("$DOCKER_BIN" volume ls -q 2>/dev/null) || vols=""
    for v in $vols; do
        case "$v" in
            slimtest-*) "$DOCKER_BIN" volume rm -f "$v" >/dev/null 2>&1 || true ;;
        esac
    done
    nets=$("$DOCKER_BIN" network ls --format '{{.Name}}' 2>/dev/null) || nets=""
    for n in $nets; do
        case "$n" in
            slimtest-*) "$DOCKER_BIN" network rm "$n" >/dev/null 2>&1 || true ;;
        esac
    done
    imgs=$("$DOCKER_BIN" images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null) || imgs=""
    for img in $imgs; do
        case "$img" in
            slimtest-*|slimtest/*) "$DOCKER_BIN" rmi -f "$img" >/dev/null 2>&1 || true ;;
        esac
    done
}

# Reachability probe: warn but keep going — an unreachable daemon simply
# scores 0, which is itself a valid corpus result.
probe="$WORK_ROOT/probe"
if ! "$DOCKER_BIN" version >"$probe" 2>&1; then
    printf 'WARN: "%s version" failed against DOCKER_HOST=%s — expect failures\n' \
        "$DOCKER_BIN" "${DOCKER_HOST:-<default>}"
fi

sweep

for case_file in "$CORPUS_DIR"/cases/*.sh; do
    [ -e "$case_file" ] || continue
    name=$(basename "$case_file" .sh)
    # Unquoted $CORPUS_FILTER: glob match is the point.
    case "$name" in
        $CORPUS_FILTER) ;;
        *) continue ;;
    esac

    TOTAL=$((TOTAL + 1))
    case_tmp="$WORK_ROOT/$name"
    mkdir -p "$case_tmp"
    start=$(date +%s)
    CASE_NAME="$name" CASE_TMP="$case_tmp" \
        CORPUS_LIB="$CORPUS_DIR/lib.sh" CORPUS_CASE="$case_file" \
        sh -u -c '. "$CORPUS_LIB" && . "$CORPUS_CASE"' >"$case_tmp/case.log" 2>&1
    end=$(date +%s)
    secs=$((end - start))

    if [ -f "$case_tmp/status" ]; then
        status=$(cat "$case_tmp/status")
    else
        status=FAIL    # case process died before lib.sh could record anything
    fi

    printf '%s %s (%ss)\n' "$status" "$name" "$secs"
    case "$status" in
        PASS) PASSC=$((PASSC + 1)) ;;
        SKIP)
            SKIPC=$((SKIPC + 1))
            if [ -f "$case_tmp/skip_reason" ]; then
                sed 's/^/    reason: /' "$case_tmp/skip_reason"
            fi
            ;;
        *)
            FAILC=$((FAILC + 1))
            sed 's/^/    /' "$case_tmp/case.log"
            cp "$case_tmp/case.log" "$RESULTS_DIR/$name.log" 2>/dev/null || true
            ;;
    esac
    printf '%s\t%s\t%s\n' "$name" "$status" "$secs" >>"$TSV"
done

if [ "${CORPUS_KEEP:-0}" = "1" ] && [ "$FAILC" -gt 0 ]; then
    printf 'CORPUS_KEEP=1: skipping final slimtest-* sweep\n'
else
    sweep
fi

DEN=$((PASSC + FAILC))
if [ "$DEN" -gt 0 ]; then
    SCORE="$((100 * PASSC / DEN))%"
else
    SCORE="n/a"
fi

printf '\n==== corpus summary ====\n'
printf 'total=%s pass=%s fail=%s skip=%s\n' "$TOTAL" "$PASSC" "$FAILC" "$SKIPC"
printf 'SCORE=%s\n' "$SCORE"
printf 'results: %s\n' "$TSV"

[ "$FAILC" -eq 0 ]
