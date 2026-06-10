# 180-build-cache: rebuilding an unchanged context hits the layer cache.
# Accept EITHER a cache marker in the build output ("Using cache" from the
# classic builder, "CACHED" from buildkit) OR a second build that finishes
# in under 2 seconds — some builders are quiet about cache hits.
ensure_image alpine:3.19
cleanup_add dk rmi -f slimtest-cache:1

mkdir -p "$CASE_TMP/ctx"
cat >"$CASE_TMP/ctx/Dockerfile" <<'EOF'
FROM alpine:3.19
RUN echo cache-probe > /c.txt
EOF

assert_ok "first build (warms cache)" dk build -t slimtest-cache:1 "$CASE_TMP/ctx"

t0=$(date +%s)
assert_ok "second build" dk build -t slimtest-cache:1 "$CASE_TMP/ctx"
t1=$(date +%s)
elapsed=$((t1 - t0))

# Capture-then-grep: build progress may land on stdout or stderr.
cat "$OUT" "$ERR" >"$CASE_TMP/buildlog" 2>/dev/null
cached=no
if grep -e 'Using cache' -e 'CACHED' "$CASE_TMP/buildlog" >/dev/null 2>&1; then
    cached=yes
fi
if [ "$elapsed" -lt 2 ]; then
    cached=yes
fi
assert_ok "second build cached (marker in log, or <2s — took ${elapsed}s)" \
    [ "$cached" = yes ]
