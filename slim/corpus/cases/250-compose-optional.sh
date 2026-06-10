# 250-compose-optional: minimal compose up/down — but only if the client
# has a compose plugin at all. nebula-slim is not expected to ship compose
# initially, so SKIP is the expected slim result for now.
if ! dk compose version >"$OUT" 2>"$ERR"; then
    skip "docker compose not available (expected for slim v0)"
fi

ensure_image alpine:3.19
cleanup_add dk compose -p slimtest-compose -f "$CASE_TMP/compose.yml" down --remove-orphans

cat >"$CASE_TMP/compose.yml" <<'EOF'
services:
  probe:
    image: alpine:3.19
    command: ["echo", "compose-ok"]
EOF

# --exit-code-from waits for probe and propagates its exit code; asserting
# on the code (not attach output) avoids the fast-container attach race.
assert_ok "compose up" \
    dk compose -p slimtest-compose -f "$CASE_TMP/compose.yml" \
    up --exit-code-from probe probe

assert_ok "compose down" \
    dk compose -p slimtest-compose -f "$CASE_TMP/compose.yml" down --remove-orphans
